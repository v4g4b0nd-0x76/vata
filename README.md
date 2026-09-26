# Vata benchmark notes

The Rust crate lives in [`vata/`](vata). The canonical recorded measurements are in
[`assets/bench_reports/2026-09-26-arena.md`](assets/bench_reports/2026-09-26-arena.md).

## Run the benchmarks

```sh
cd vata
make workload       # 2M 1 KiB records, 4 local readers; reports 100k-batch medians
make profile-perf   # Linux perf counters for Criterion's arena_bench
make profile-c2c    # Linux cache-to-cache / false-sharing report
```

`profile-perf` and `profile-c2c` require Linux `perf` access (normally `sudo`).
They profile `arena_bench`, which contains three Criterion workloads; `workload`
is the separate fixed 100k-batch throughput measurement. Do not compare absolute
perf counts between runs: Criterion performs as much work as it can during its
fixed profiling period.

## Recorded result

After the packed-cursor change, the fixed workload's unpinned median was
**9.965 ms per 100k records** with zero steady-state allocation calls. In the
Linux C2C profile, local-HITM loads normalized by sampled loads fell from **9.21%**
to **4.44%**. The full before/after counters, raw workload samples, and rejected
CPU-affinity experiment are in the [benchmark report](assets/bench_reports/2026-09-26-arena.md).

## Allocator behavior

`vata/src/main.rs` declares `tikv_jemallocator::Jemalloc` as the global allocator.
That makes allocations made by the **`vata` executable** use jemalloc. It does not
magically apply to other final binaries: each benchmark is its own executable and
therefore declares its own allocator.

- `arena_bench` declares jemalloc directly.
- `arena_workload` declares a small counting allocator that forwards every
  allocation, reallocation, and deallocation to jemalloc. It counts only the
  selected setup and steady-state windows; it does not replace the arena.
- The arena preallocates its slabs during setup. The recorded workload showed
  zero steady-state allocator calls, so appending a record does not allocate.

On Linux, the make targets launch the benchmark with:

```text
MALLOC_CONF=abort_conf:true,thp:always,metadata_thp:always
```

This is jemalloc configuration, not a request to the NIC. The makefile clears an
inherited `MALLOC_CONF` before building/profiling so a malformed shell-wide value
cannot affect Cargo or perf. If jemalloc still prints `Malformed conf string`, run
`unset MALLOC_CONF` and remove the invalid export from the shell startup file.

THP and Linux perf do not apply to macOS. On an M4 Pro, use the workload benchmark
for the arena measurement; memory-page policy is controlled by macOS rather than
Linux THP settings.
