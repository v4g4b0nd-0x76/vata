use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use tikv_jemallocator::Jemalloc;
use vata::arena_alloc::{Core, ReaderHandle};

#[global_allocator]
static ALLOCATOR: Jemalloc = Jemalloc;

fn bench_core_storage_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("Core_Storage_Engine");

    // 1. Single-Writer Throughput: Benchmark appending a continuous 4MB payload
    // This benchmarks multi-slab fragmentation and out-of-line rollover performance.
    let payload = vec![b'A'; 4 * 1024 * 1024];

    group.bench_function("append_large_4mb_payload", |b| {
        // Instantiate a clean core with 16 slabs capacity for each measurement iteration
        let core = Core::new(16, 1);
        b.iter(|| {
            unsafe { core.append_bytes(black_box(&payload)) };
        });
    });

    // 2. High-Contention Asynchronous Coordination: Benchmark concurrent read/write execution.
    // This measures the efficiency of the cache-friendly exponential busy-spin backoff loop.
    group.bench_function("concurrent_busy_spin_read_write", |b| {
        b.iter_custom(|iters| {
            // Allocate a ring of 16 slabs supporting 1 dedicated reader thread
            let core = Arc::new(Core::new(16, 1));
            let reader = unsafe { ReaderHandle::new(Arc::clone(&core), 0) };

            // Spawn the concurrent consumer thread to track the writer's pace
            let reader_thread = thread::spawn(move || {
                // Track exactly how many bytes the writer will output over the fixed iteration loop
                let mut out_buf = vec![0u8; 1024];
                let mut total_read = 0;
                let target_bytes = iters as usize * 1024;

                while total_read < target_bytes {
                    let n = reader.read_next_blocking(&mut out_buf);
                    total_read += n;
                }
                total_read
            });

            // Start hardware timer capture
            let start = std::time::Instant::now();
            let write_payload = vec![b'B'; 1024];

            for _ in 0..iters {
                unsafe { core.append_bytes(&write_payload) };
            }

            // Block until reader thread finishes pulling all data out of line
            reader_thread.join().unwrap();
            start.elapsed()
        });
    });

    // 3. Free-Stack Reclamation Under Contention: many slabs, several lagging readers,
    // forcing frequent rollovers and frequent push/pop traffic on the free-slab stack.
    // This targets the O(1) reclamation path directly (previously an O(pool_cap) linear
    // scan), so a regression in that path shows up here even when the two benchmarks
    // above look unaffected — they exercise mostly-uncontended rollover, this one
    // exercises the case multiple readers are racing to push freed slabs back
    // concurrently while the writer pops from the same stack.
    group.bench_function("free_stack_reclaim_many_slabs_multi_reader", |b| {
        b.iter_custom(|iters| {
            const READERS: usize = 4;
            const RECORD_SIZE: usize = 256;

            // Small slab pool relative to payload volume maximizes rollover frequency,
            // i.e. maximizes free-stack push/pop traffic per byte written.
            let core = Arc::new(Core::new(32, READERS));
            let readers: Vec<_> = (0..READERS)
                .map(|id| unsafe { ReaderHandle::new(Arc::clone(&core), id) })
                .collect();

            let reader_threads: Vec<_> = readers
                .into_iter()
                .map(|reader| {
                    thread::spawn(move || {
                        let mut out_buf = vec![0u8; RECORD_SIZE];
                        let mut total_read = 0usize;
                        let target_bytes = iters as usize * RECORD_SIZE;
                        while total_read < target_bytes {
                            total_read += reader.read_next_blocking(&mut out_buf);
                        }
                    })
                })
                .collect();

            let start = std::time::Instant::now();
            let write_payload = vec![b'E'; RECORD_SIZE];
            for _ in 0..iters {
                unsafe { core.append_bytes(&write_payload) };
            }

            for handle in reader_threads {
                handle.join().unwrap();
            }
            start.elapsed()
        });
    });

    group.finish();
}

criterion_group!(benches, bench_core_storage_throughput);
criterion_main!(benches);
