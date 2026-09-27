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
const CURSOR_OFFSET_BITS: u32 = SLAB_SIZE.trailing_zeros();
const CURSOR_OFFSET_MASK: u64 = (SLAB_SIZE - 1) as u64;
const MAX_CURSOR_SLAB_INDEX: usize = (u64::MAX >> CURSOR_OFFSET_BITS) as usize;

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

/// Per-slab control state.
#[repr(C, align(64))]
pub struct SlabState {
    pub reader_mask: AtomicU64,
    next_slab_idx: AtomicUsize,
    next_free: AtomicUsize,
}

impl Default for SlabState {
    fn default() -> Self {
        Self {
            reader_mask: AtomicU64::new(0),
            next_slab_idx: AtomicUsize::new(NO_SUCCESSOR),
            next_free: AtomicUsize::new(NO_SUCCESSOR),
        }
    }
}

#[repr(C, align(64))]
pub(crate) struct WriteCursor {
    value: AtomicU64,
}

#[inline(always)]
pub(crate) fn pack_cursor(slab_idx: usize, offset: usize) -> u64 {
    debug_assert!(offset < SLAB_SIZE);
    ((slab_idx as u64) << CURSOR_OFFSET_BITS) | offset as u64
}

#[inline(always)]
pub(crate) fn unpack_cursor(value: u64) -> (usize, usize) {
    (
        (value >> CURSOR_OFFSET_BITS) as usize,
        (value & CURSOR_OFFSET_MASK) as usize,
    )
}

impl WriteCursor {
    fn new(slab_idx: usize, offset: usize) -> Self {
        Self {
            value: AtomicU64::new(pack_cursor(slab_idx, offset)),
        }
    }

    #[inline(always)]
    pub(crate) fn load(&self, order: Ordering) -> (usize, usize) {
        unpack_cursor(self.value.load(order))
    }

    #[inline(always)]
    fn store(&self, slab_idx: usize, offset: usize, order: Ordering) {
        self.value.store(pack_cursor(slab_idx, offset), order);
    }
}

pub struct Core {
    pub(crate) slabs_base: *mut Slab,
    pub(crate) states: Vec<SlabState>,
    pub(crate) pool_cap: usize,
    pub(crate) reader_capacity: usize,
    pub(crate) write_cursor: WriteCursor,
    /// Head of the intrusive free-slab stack. `NO_SUCCESSOR` means empty.
    free_head: AtomicUsize,
    #[cfg(feature = "telemetry")]
    pub writer_metrics: Arc<TelemetryShard>,
    #[cfg(feature = "telemetry")]
    pub reader_metrics: Arc<Vec<TelemetryShard>>,
}

unsafe impl Sync for Core {}
unsafe impl Send for Core {}

/// A single-producer write session. One release publish covers every record appended to it.
pub struct CoreWriter<'a> {
    core: &'a Core,
    idx: usize,
    offset: usize,
    current_slab: *mut Slab,
    #[cfg(feature = "telemetry")]
    start_mark: Instant,
    #[cfg(feature = "telemetry")]
    ops_count: usize,
    #[cfg(feature = "telemetry")]
    bytes_processed: usize,
}

pub struct ReaderHandle {
    core: Arc<Core>,
    client_id: usize,
    current_slab_idx: Cell<usize>,
    current_offset: Cell<usize>,
}

/// Streams `len` bytes from `src` to `dst`, using non-temporal (write-combining) stores
/// when the `non_temporal_writes` Cargo feature is enabled on x86_64, and a plain
/// `copy_nonoverlapping` everywhere else (this feature does not exist yet — add
/// `non_temporal_writes = []` under `[features]` in Cargo.toml to opt in).
#[inline(always)]
unsafe fn copy_streaming(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(all(feature = "non_temporal_writes", target_arch = "x86_64"))]
    unsafe {
        use core::arch::x86_64::{__m128i, _mm_stream_si128};

        let mut i = 0usize;
        // Streaming stores require a 16-byte aligned destination. Only take the fast
        // path when that holds; the remainder (and any unaligned case) falls through to
        // the plain copy below.
        if (dst as usize) % 16 == 0 {
            while i + 16 <= len {
                let chunk = ptr::read_unaligned(src.add(i) as *const __m128i);
                _mm_stream_si128(dst.add(i) as *mut __m128i, chunk);
                i += 16;
            }
        }
        if i < len {
            ptr::copy_nonoverlapping(src.add(i), dst.add(i), len - i);
        }
        return;
    }

    #[cfg(not(all(feature = "non_temporal_writes", target_arch = "x86_64")))]
    unsafe {
        ptr::copy_nonoverlapping(src, dst, len);
    }
}

/// Issues a store fence when non-temporal writes are active, guaranteeing any streamed
/// data is globally visible before the cursor position that "publishes" it to readers is
/// stored. A no-op on the default (non-streaming) path, where ordinary stores are already
/// ordered correctly by the existing `Ordering::Release` on the cursor stores.
#[inline(always)]
fn sfence() {
    #[cfg(all(feature = "non_temporal_writes", target_arch = "x86_64"))]
    unsafe {
        core::arch::x86_64::_mm_sfence();
    }
}

impl Core {
    pub fn new(cap: usize, max_readers: usize) -> Self {
        assert!(cap >= 2, "arena requires at least two slabs");
        assert!(
            cap <= MAX_CURSOR_SLAB_INDEX,
            "arena capacity exceeds packed cursor range"
        );
        let layout = std::alloc::Layout::array::<Slab>(cap).unwrap();
        let slabs_base = unsafe { std::alloc::alloc_zeroed(layout) as *mut Slab };
        if slabs_base.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        let mut states: Vec<SlabState> = Vec::with_capacity(cap);
        for _ in 0..cap {
            states.push(SlabState::default());
        }

        if cap > 1 {
            for i in 1..cap {
                let next = if i + 1 < cap { i + 1 } else { NO_SUCCESSOR };
                states[i].next_free.store(next, Ordering::Relaxed);
            }
        }
        let free_head = AtomicUsize::new(if cap > 1 { 1 } else { NO_SUCCESSOR });

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
            write_cursor: WriteCursor::new(0, 0),
            free_head,
            #[cfg(feature = "telemetry")]
            writer_metrics: Arc::new(TelemetryShard::default()),
            #[cfg(feature = "telemetry")]
            reader_metrics: Arc::new(reader_metrics),
        }
    }

    fn push_free(&self, idx: usize) {
        loop {
            let head = self.free_head.load(Ordering::Acquire);
            self.states[idx].next_free.store(head, Ordering::Relaxed);
            if self
                .free_head
                .compare_exchange_weak(head, idx, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Pops a slab index off the free stack, or `None` if it's currently empty. Only the
    /// writer calls this (single consumer), but it still needs a CAS because pushers
    /// (readers, or the writer's own self-reclaim) can race with it concurrently.
    fn pop_free(&self) -> Option<usize> {
        loop {
            let head = self.free_head.load(Ordering::Acquire);
            if head == NO_SUCCESSOR {
                return None;
            }
            let next = self.states[head].next_free.load(Ordering::Acquire);
            if self
                .free_head
                .compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(head);
            }
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

        let next_idx = loop {
            if let Some(idx) = self.pop_free() {
                break idx;
            }
            std::thread::yield_now();
        };

        let reader_mask = self.states[current_idx].reader_mask.load(Ordering::Acquire);
        self.states[next_idx]
            .next_slab_idx
            .store(NO_SUCCESSOR, Ordering::Relaxed);
        self.states[next_idx]
            .reader_mask
            .fetch_or(reader_mask, Ordering::Release);

        sfence();

        self.states[current_idx]
            .next_slab_idx
            .store(next_idx, Ordering::Release);
        self.write_cursor.store(next_idx, 0, Ordering::Release);

        if reader_mask == 0 {
            self.push_free(current_idx);
        }

        (next_idx, unsafe { self.slabs_base.add(next_idx) })
    }

    /// Starts a single-producer write batch. Records remain independently readable, but the
    /// cursor is published once after the closure returns.
    #[inline(always)]
    pub unsafe fn append_batch(&self, write: impl FnOnce(&mut CoreWriter<'_>)) {
        let (idx, offset) = self.write_cursor.load(Ordering::Relaxed);
        let mut writer = CoreWriter {
            core: self,
            idx,
            offset,
            current_slab: unsafe { self.slabs_base.add(idx) },
            #[cfg(feature = "telemetry")]
            start_mark: Instant::now(),
            #[cfg(feature = "telemetry")]
            ops_count: 0,
            #[cfg(feature = "telemetry")]
            bytes_processed: 0,
        };
        write(&mut writer);
        unsafe { writer.finish() };
    }

    /// Single-record convenience path for callers that do not already have a batch.
    #[inline(always)]
    pub unsafe fn append_bytes(&self, bytes: &[u8]) {
        unsafe { self.append_batch(|writer| writer.append(bytes)) };
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

impl CoreWriter<'_> {
    /// Appends one independently readable record without allocating. The enclosing
    /// `Core::append_batch` call is responsible for the single-writer invariant.
    #[inline(always)]
    pub fn append(&mut self, mut bytes: &[u8]) {
        #[cfg(feature = "telemetry")]
        {
            self.ops_count += 1;
            self.bytes_processed += bytes.len();
        }

        unsafe {
            while !bytes.is_empty() {
                let available = (*self.current_slab).data.len() - self.offset;
                if available <= 4 {
                    let (idx, slab) =
                        self.core
                            .handle_write_rollover(self.idx, self.offset, self.current_slab);
                    self.idx = idx;
                    self.current_slab = slab;
                    self.offset = 0;
                    continue;
                }

                let chunk_len = bytes.len().min(available - 4);
                let packed_header = chunk_len as u32 | ((chunk_len < bytes.len()) as u32) << 31;
                let dst = (*self.current_slab).data.as_mut_ptr().add(self.offset);
                ptr::copy_nonoverlapping(&packed_header as *const u32 as *const u8, dst, 4);
                copy_streaming(bytes.as_ptr(), dst.add(4), chunk_len);

                self.offset += 4 + chunk_len;
                bytes = &bytes[chunk_len..];

                if (packed_header & MASK_CONTINUATION) != 0 {
                    let (idx, slab) =
                        self.core
                            .handle_write_rollover(self.idx, self.offset, self.current_slab);
                    self.idx = idx;
                    self.current_slab = slab;
                    self.offset = 0;
                }
            }
        }
    }

    #[inline(always)]
    unsafe fn finish(self) {
        sfence();
        self.core
            .write_cursor
            .store(self.idx, self.offset, Ordering::Release);

        #[cfg(feature = "telemetry")]
        {
            let duration = self.start_mark.elapsed().as_nanos() as u64;
            self.core
                .writer_metrics
                .ops_count
                .fetch_add(self.ops_count, Ordering::Relaxed);
            self.core
                .writer_metrics
                .bytes_processed
                .fetch_add(self.bytes_processed, Ordering::Relaxed);
            self.core
                .writer_metrics
                .total_duration
                .fetch_add(duration, Ordering::Relaxed);
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
        let prev_mask = core.states[idx]
            .reader_mask
            .fetch_and(!mask_bit, Ordering::Release);

        // If clearing our bit dropped the mask to zero, we were the last reader still
        // referencing this slab — hand it back to the writer's free stack. Any other
        // pinned reader will already have caught up and left by the time this happens,
        // since the writer only ever forwards pins ahead of itself, never backward.
        if prev_mask & !mask_bit == 0 {
            core.push_free(idx);
        }

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
                let (global_write_idx, global_write_offset) =
                    core.write_cursor.load(Ordering::Acquire);

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
