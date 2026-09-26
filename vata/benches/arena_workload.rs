use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tikv_jemallocator::Jemalloc;
use vata::arena_alloc::{Core, ReaderHandle};

const RECORDS: usize = 2_000_000;
const READERS: usize = 4;
const RECORD_SIZE: usize = 1024;
const BATCH_RECORDS: usize = 100_000;
const BATCH_BYTES: usize = BATCH_RECORDS * RECORD_SIZE;

struct CountingAllocator;

static JEMALLOC: Jemalloc = Jemalloc;

static COUNT_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATION_CALLS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { JEMALLOC.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { JEMALLOC.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { JEMALLOC.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation(new_size);
        unsafe { JEMALLOC.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record_allocation(bytes: usize) {
    if COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
        ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
}

fn start_allocation_count() {
    ALLOCATION_CALLS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    COUNT_ALLOCATIONS.store(true, Ordering::Relaxed);
}

fn finish_allocation_count() -> (usize, usize) {
    COUNT_ALLOCATIONS.store(false, Ordering::Relaxed);
    (
        ALLOCATION_CALLS.load(Ordering::Relaxed),
        ALLOCATED_BYTES.load(Ordering::Relaxed),
    )
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    let middle = samples.len() / 2;
    if samples.len() % 2 == 1 {
        samples[middle]
    } else {
        Duration::from_nanos(
            ((samples[middle - 1].as_nanos() + samples[middle].as_nanos()) / 2) as u64,
        )
    }
}

fn main() {
    let batches = RECORDS / BATCH_RECORDS;
    let ready = Arc::new(Barrier::new(READERS + 1));
    let start = Arc::new(Barrier::new(READERS + 1));
    let completed_readers = Arc::new(AtomicUsize::new(0));
    let release_reader_results = Arc::new(AtomicBool::new(false));
    start_allocation_count();
    let core = Arc::new(Core::new(16, READERS));
    let readers: Vec<_> = (0..READERS)
        .map(|id| unsafe { ReaderHandle::new(Arc::clone(&core), id) })
        .collect();
    let worker_threads: Vec<_> = readers
        .into_iter()
        .map(|reader| {
            let ready = Arc::clone(&ready);
            let start = Arc::clone(&start);
            let completed_readers = Arc::clone(&completed_readers);
            let release_reader_results = Arc::clone(&release_reader_results);
            thread::spawn(move || {
                let mut out_buf = [0u8; RECORD_SIZE];
                let mut batch_times = Vec::with_capacity(batches);
                let mut total_read = 0;
                let mut batch_read = 0;

                ready.wait();
                start.wait();
                let mut batch_start = Instant::now();
                while total_read < RECORDS * RECORD_SIZE {
                    let read = reader.read_next_blocking(&mut out_buf);
                    total_read += read;
                    batch_read += read;
                    if batch_read == BATCH_BYTES {
                        batch_times.push(batch_start.elapsed());
                        batch_start = Instant::now();
                        batch_read = 0;
                    }
                }
                assert_eq!(batch_times.len(), batches);
                completed_readers.fetch_add(1, Ordering::Release);
                while !release_reader_results.load(Ordering::Acquire) {
                    thread::yield_now();
                }
                batch_times
            })
        })
        .collect();
    ready.wait();
    let setup_allocations = finish_allocation_count();

    let payload = [b'B'; RECORD_SIZE];
    let mut write_batches = Vec::with_capacity(batches);
    let mut read_batches = Vec::with_capacity(READERS * batches);

    start_allocation_count();
    start.wait();
    let mut batch_start = Instant::now();
    for record in 1..=RECORDS {
        unsafe { core.append_bytes(&payload) };
        if record % BATCH_RECORDS == 0 {
            write_batches.push(batch_start.elapsed());
            batch_start = Instant::now();
        }
    }
    while completed_readers.load(Ordering::Acquire) != READERS {
        thread::yield_now();
    }
    let steady_allocations = finish_allocation_count();
    release_reader_results.store(true, Ordering::Release);
    for worker in worker_threads {
        read_batches.extend(worker.join().unwrap());
    }

    println!("records: {RECORDS}, readers: {READERS}, batch: {BATCH_RECORDS}");
    println!(
        "setup allocations: {} calls, {} bytes",
        setup_allocations.0, setup_allocations.1
    );
    println!(
        "steady-state allocations: {} calls, {} bytes",
        steady_allocations.0, steady_allocations.1
    );
    println!("write 100k-batch median: {:?}", median(&mut write_batches));
    println!(
        "read 100k-batch median across all consumers: {:?}",
        median(&mut read_batches)
    );
}
