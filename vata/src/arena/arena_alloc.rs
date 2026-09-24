use std::cell::Cell;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "telemetry")]
use std::time::Instant;

pub const CACHE_LINE: usize = 64;
pub const SLAB_SIZE: usize = 2 * 1024 * 1024;

pub const MASK_CONTINUATION: u32 = 1 << 31; // Highest bit = Spanning Continuation indicator
pub const MASK_LENGTH: u32 = !MASK_CONTINUATION; // Lower 31 bits = Data Chunk Length
const NO_SUCCESSOR: usize = usize::MAX;

#[repr(C, align(64))]
pub struct Slab {
    #[cfg(test)]
    pub checksum: AtomicUsize,
    #[cfg(test)]
    pub written_bytes: AtomicUsize,
    pub data: [u8; SLAB_SIZE - CACHE_LINE],
}

#[cfg(feature = "telemetry")]
#[repr(C, align(64))]
#[derive(Default)]
pub struct TelemetryShard {
    pub ops_count: AtomicUsize,
    pub bytes_processed: AtomicUsize,
    pub total_duration: AtomicU64,
}

#[repr(C, align(64))]
pub struct SlabState {
    pub reader_mask: AtomicU64,
    next_slab_idx: AtomicUsize,
}

impl Default for SlabState {
    fn default() -> Self {
        Self {
            reader_mask: AtomicU64::new(0),
            next_slab_idx: AtomicUsize::new(NO_SUCCESSOR),
        }
    }
}

#[repr(C, align(64))]
pub(crate) struct WriteCursor {
    pub(crate) slab_idx: AtomicUsize,
    pub(crate) offset: AtomicUsize,
}

pub struct Core {
    pub(crate) slabs_base: *mut Slab,
    pub(crate) states: Vec<SlabState>,
    pub(crate) pool_cap: usize,
    pub(crate) reader_capacity: usize,
    pub(crate) write_cursor: WriteCursor,
    #[cfg(feature = "telemetry")]
    pub writer_metrics: Arc<TelemetryShard>,
    #[cfg(feature = "telemetry")]
    pub reader_metrics: Arc<Vec<TelemetryShard>>,
}

unsafe impl Sync for Core {}
unsafe impl Send for Core {}

pub struct ReaderHandle {
    core: Arc<Core>,
    client_id: usize,
    current_slab_idx: Cell<usize>,
    current_offset: Cell<usize>,
}

impl Core {
    pub fn new(cap: usize, max_readers: usize) -> Self {
        assert!(cap >= 2, "arena requires at least two slabs");
        let layout = std::alloc::Layout::array::<Slab>(cap).unwrap();
        let slabs_base = unsafe { std::alloc::alloc_zeroed(layout) as *mut Slab };
        if slabs_base.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        let mut states: Vec<SlabState> = Vec::with_capacity(cap);
        for _ in 0..cap {
            states.push(SlabState::default());
        }
        #[cfg(feature = "telemetry")]
        let mut reader_metrics = Vec::with_capacity(max_readers);
        #[cfg(feature = "telemetry")]
        for _ in 0..max_readers {
            reader_metrics.push(TelemetryShard::default());
        }
        Self {
            slabs_base,
            states,
            pool_cap: cap,
            reader_capacity: max_readers,
            write_cursor: WriteCursor {
                slab_idx: AtomicUsize::new(0),
                offset: AtomicUsize::new(0),
            },
            #[cfg(feature = "telemetry")]
            writer_metrics: Arc::new(TelemetryShard::default()),
            #[cfg(feature = "telemetry")]
            reader_metrics: Arc::new(reader_metrics),
        }
    }

    /// Out-of-line cold handler to ensure `append_bytes` hot loop remains perfectly linear.
    #[cold]
    #[inline(never)]
    unsafe fn handle_write_rollover(
        &self,
        current_idx: usize,
        offset: usize,
        current_slab: *mut Slab,
    ) -> (usize, *mut Slab) {
        #[cfg(test)]
        self.finalize_test_checksum(current_slab, offset);
        #[cfg(not(test))]
        let _ = (offset, current_slab);

        let next_idx = self.route_next_free_index(current_idx);
        let reader_mask = self.states[current_idx].reader_mask.load(Ordering::Acquire);
        self.states[next_idx]
            .next_slab_idx
            .store(NO_SUCCESSOR, Ordering::Relaxed);
        self.states[next_idx]
            .reader_mask
            .fetch_or(reader_mask, Ordering::Release);
        self.states[current_idx]
            .next_slab_idx
            .store(next_idx, Ordering::Release);
        self.write_cursor.offset.store(0, Ordering::Release);
        self.write_cursor
            .slab_idx
            .store(next_idx, Ordering::Release);
        (next_idx, unsafe { self.slabs_base.add(next_idx) })
    }

    /// Single-Writer Fast Loop: Linearly slices and fragments raw byte payloads across slabs.
    ///
    /// # Safety
    /// The caller must serialize all calls for this `Core` to one producer.
    #[inline(always)]
    pub unsafe fn append_bytes(&self, mut bytes: &[u8]) {
        let mut idx = self.write_cursor.slab_idx.load(Ordering::Acquire);
        let mut offset = self.write_cursor.offset.load(Ordering::Relaxed);
        #[cfg(feature = "telemetry")]
        let start_mark = Instant::now();
        #[cfg(feature = "telemetry")]
        let initial_len = bytes.len();

        unsafe {
            let mut current_slab = self.slabs_base.add(idx);

            while !bytes.is_empty() {
                let available = (*current_slab).data.len() - offset;

                // Checked via cold function call to manipulate compiler basic blocks
                if available <= 4 {
                    let (new_idx, new_slab) = self.handle_write_rollover(idx, offset, current_slab);
                    idx = new_idx;
                    current_slab = new_slab;
                    offset = 0;
                    continue;
                }

                let max_payload = available - 4;
                let chunk_len = bytes.len().min(max_payload);

                // Branchless mathematical flag determination
                let is_cont = (chunk_len < bytes.len()) as u32;
                let packed_header = chunk_len as u32 | (is_cont << 31);

                let dst = (*current_slab).data.as_mut_ptr().add(offset);
                ptr::copy_nonoverlapping(&packed_header as *const u32 as *const u8, dst, 4);
                ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(4), chunk_len);

                offset += 4 + chunk_len;
                bytes = &bytes[chunk_len..];

                if (packed_header & MASK_CONTINUATION) != 0 {
                    let (new_idx, new_slab) = self.handle_write_rollover(idx, offset, current_slab);
                    idx = new_idx;
                    current_slab = new_slab;
                    offset = 0;
                }
            }
            self.write_cursor.offset.store(offset, Ordering::Release);
        }

        #[cfg(feature = "telemetry")]
        {
            // Cache-Isolated Writer Telemetry update
            let duration = start_mark.elapsed().as_nanos() as u64;
            self.writer_metrics
                .ops_count
                .fetch_add(1, Ordering::Relaxed);
            self.writer_metrics
                .bytes_processed
                .fetch_add(initial_len, Ordering::Relaxed);
            self.writer_metrics
                .total_duration
                .fetch_add(duration, Ordering::Relaxed);
        }
    }

    #[inline(always)]
    fn route_next_free_index(&self, current: usize) -> usize {
        let total = self.pool_cap;
        let mut scan = (current + 1) % total;

        loop {
            if self.states[scan].reader_mask.load(Ordering::Acquire) == 0 {
                return scan;
            }
            scan = (scan + 1) % total;
            if scan == current {
                std::thread::yield_now(); // Core protection fallback loop execution
            }
        }
    }

    #[cfg(test)]
    fn finalize_test_checksum(&self, slab: *mut Slab, offset: usize) {
        unsafe {
            // Use safe out-of-band macro projections instead of doing direct pointer dereferencing
            let data_array_ptr = ptr::addr_of!((*slab).data) as *const u8;
            let final_data = core::slice::from_raw_parts(data_array_ptr, offset);

            let mut hash: usize = 0x811C9DC5;
            for &byte in final_data {
                hash ^= byte as usize;
                hash = hash.wrapping_mul(0x01000193);
            }

            let checksum_field = ptr::addr_of!((*slab).checksum) as *const AtomicUsize;
            let written_bytes_field = ptr::addr_of!((*slab).written_bytes) as *const AtomicUsize;

            (*checksum_field).store(hash, Ordering::Release);
            (*written_bytes_field).store(offset, Ordering::Release);
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::array::<Slab>(self.pool_cap).unwrap();
        unsafe { std::alloc::dealloc(self.slabs_base.cast(), layout) };
    }
}

impl ReaderHandle {
    /// # Safety
    /// `client_id` must be unique and within `0..max_readers`. Create every reader before the
    /// producer starts and keep each handle alive until that producer has stopped.
    pub unsafe fn new(core: Arc<Core>, client_id: usize) -> Self {
        assert!(client_id < 64, "reader IDs must fit in the reader mask");
        assert!(
            client_id < core.reader_capacity,
            "reader ID exceeds configured reader capacity"
        );
        let reader = Self {
            core,
            client_id,
            current_slab_idx: Cell::new(0),
            current_offset: Cell::new(0),
        };
        reader.core.states[0]
            .reader_mask
            .fetch_or(1u64 << client_id, Ordering::Relaxed);
        reader
    }

    /// Out-of-line cold handler to ensure standard packet consumption execution runs without stalls.
    #[cold]
    #[inline(never)]
    fn handle_read_rollover(&self, core: &Core, idx: usize) -> usize {
        let next_idx = core.states[idx].next_slab_idx.load(Ordering::Acquire);
        assert_ne!(
            next_idx, NO_SUCCESSOR,
            "rollover successor was not published"
        );
        let mask_bit = 1u64 << self.client_id;

        core.states[next_idx]
            .reader_mask
            .fetch_or(mask_bit, Ordering::Relaxed);
        core.states[idx]
            .reader_mask
            .fetch_and(!mask_bit, Ordering::Release);

        self.current_slab_idx.set(next_idx);
        self.current_offset.set(0);
        next_idx
    }

    /// Multi-Reader Fast Path: Consumes streams with hardware-friendly busy spinning.
    #[inline(always)]
    pub fn read_next_blocking(&self, out_buf: &mut [u8]) -> usize {
        let core = &self.core;
        let mut idx = self.current_slab_idx.get();
        let mut offset = self.current_offset.get();
        #[cfg(feature = "telemetry")]
        let start_mark = Instant::now();
        let mut spin_count = 0;

        unsafe {
            let slab_ptr = loop {
                let global_write_idx = core.write_cursor.slab_idx.load(Ordering::Acquire);
                let global_write_offset = core.write_cursor.offset.load(Ordering::Acquire);

                if idx != global_write_idx || offset < global_write_offset {
                    let slab_ptr = core.slabs_base.add(idx);
                    if offset + 4 < (*slab_ptr).data.len() {
                        break slab_ptr;
                    }
                    idx = self.handle_read_rollover(core, idx);
                    offset = 0;
                    continue;
                }

                // Exponential compiler backoff mechanism to protect memory bus lanes
                if spin_count < 10 {
                    std::hint::spin_loop();
                } else if spin_count < 20 {
                    for _ in 0..10 {
                        std::hint::spin_loop();
                    }
                } else {
                    std::thread::yield_now();
                }
                spin_count += 1;
            };

            let src = (*slab_ptr).data.as_ptr().add(offset);
            let mut packed_header: u32 = 0;
            ptr::copy_nonoverlapping(src, &mut packed_header as *mut u32 as *mut u8, 4);

            let chunk_len = (packed_header & MASK_LENGTH) as usize;
            let is_continued = (packed_header & MASK_CONTINUATION) != 0;

            let to_read = chunk_len.min(out_buf.len());
            ptr::copy_nonoverlapping(src.add(4), out_buf.as_mut_ptr(), to_read);

            let next_offset = offset + 4 + chunk_len;

            // Roll to next slab atomically out-of-band
            if is_continued && next_offset >= ((*slab_ptr).data.len() - 4) {
                self.handle_read_rollover(core, idx);
            } else {
                self.current_offset.set(next_offset);
            }

            #[cfg(feature = "telemetry")]
            {
                // Record thread-isolated telemetry
                let duration = start_mark.elapsed().as_nanos() as u64;
                let shard = &core.reader_metrics[self.client_id];
                shard.ops_count.fetch_add(1, Ordering::Relaxed);
                shard.bytes_processed.fetch_add(to_read, Ordering::Relaxed);
                shard.total_duration.fetch_add(duration, Ordering::Relaxed);
            }

            to_read
        }
    }
}
