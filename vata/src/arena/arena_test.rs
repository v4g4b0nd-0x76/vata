use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::{mem::align_of, mem::size_of};

use crate::arena_alloc::{CACHE_LINE, Core, ReaderHandle, SLAB_SIZE, SlabState, WriteCursor};

/// Helper function mirroring the exact FNV-1a checksum routine for data verification
fn calculate_checksum(bytes: &[u8]) -> usize {
    let mut hash: usize = 0x811C9DC5;
    for &byte in bytes {
        hash ^= byte as usize;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

#[test]
fn test_multislab_spanning_and_integrity_verification() {
    // 1. Initialize a core instance with a capacity of 4 slabs, supporting up to 2 readers
    let core = Arc::new(Core::new(4, 2));

    // 2. Build a payload larger than 2MB to force a cross-slab fragmentation event
    let test_payload = vec![b'A'; 3 * 1024 * 1024];
    unsafe { core.append_bytes(&test_payload) };

    // Validate that the writer advanced past index 0 due to the size of the payload
    let final_write_idx = core.write_cursor.slab_idx.load(Ordering::Acquire);
    assert!(
        final_write_idx > 0,
        "Writer layout failed to split data across slab boundaries!"
    );

    // 3. Confirm testing-only internal validation checksums match perfectly
    unsafe {
        let first_slab = core.slabs_base.add(0);
        let written = (*first_slab).written_bytes.load(Ordering::Acquire);
        assert!(
            written > 0,
            "Written bytes counter was not committed to the slab!"
        );

        let expected_hash = (*first_slab).checksum.load(Ordering::Acquire);

        // Use a clean raw slice to calculate actual data fingerprint
        let data_ptr = std::ptr::addr_of!((*first_slab).data) as *const u8;
        let actual_slice = std::slice::from_raw_parts(data_ptr, written);
        let actual_hash = calculate_checksum(actual_slice);

        assert_eq!(
            expected_hash, actual_hash,
            "Slab data corruption detected via validation checksums!"
        );
    }
}

#[test]
fn test_control_words_are_cache_line_isolated() {
    assert_eq!(align_of::<WriteCursor>(), CACHE_LINE);
    assert_eq!(size_of::<WriteCursor>(), CACHE_LINE);
    assert_eq!(align_of::<SlabState>(), CACHE_LINE);
    assert_eq!(size_of::<SlabState>(), CACHE_LINE);
    #[cfg(feature = "telemetry")]
    {
        assert_eq!(align_of::<crate::arena_alloc::TelemetryShard>(), CACHE_LINE);
        assert_eq!(size_of::<crate::arena_alloc::TelemetryShard>(), CACHE_LINE);
    }
}

#[cfg(feature = "telemetry")]
#[test]
fn test_cache_isolated_telemetry_tracking() {
    let core = Core::new(2, 1);
    let sample_string = b"Hello Mechanical Sympathy Core Storage Architecture";

    // Execute explicit appends
    unsafe { core.append_bytes(sample_string) };
    unsafe { core.append_bytes(sample_string) };

    // Validate that telemetry counters registered updates successfully
    let processed_bytes = core.writer_metrics.bytes_processed.load(Ordering::Relaxed);
    let ops = core.writer_metrics.ops_count.load(Ordering::Relaxed);

    assert_eq!(ops, 2);
    assert_eq!(processed_bytes, sample_string.len() * 2);
    assert!(core.writer_metrics.total_duration.load(Ordering::Relaxed) > 0);
}

#[test]
fn test_concurrent_multi_reader_spinning_no_cache_race() {
    const READER_THREADS: usize = 4;
    let core = Arc::new(Core::new(8, READER_THREADS));
    let mut reader_handles = Vec::new();

    // 1. Launch reader threads before writing data to intentionally trigger busy-spinning
    for id in 0..READER_THREADS {
        let reader = unsafe { ReaderHandle::new(Arc::clone(&core), id) };
        let handle = thread::spawn(move || {
            // Expected fixed size match chunk buffer
            let mut receive_buffer = vec![0u8; 40];
            let mut total_bytes_read = 0;

            // Run iterations; loops will spin-wait until the writer thread appends bytes
            for _ in 0..5 {
                let read = reader.read_next_blocking(&mut receive_buffer);
                if read > 0 {
                    total_bytes_read += read;
                    assert_eq!(
                        &receive_buffer[..read],
                        b"StorageEnginePayloadSignature_ValidMatch"
                    );
                }
            }
            total_bytes_read
        });
        reader_handles.push(handle);
    }

    // Give readers a brief window to enter their spin-lock execution layers
    thread::sleep(std::time::Duration::from_millis(10));

    // 2. Fire single writer thread to pump string data down the line
    let writer_core = Arc::clone(&core);
    let writer_handle = thread::spawn(move || {
        let mock_string = b"StorageEnginePayloadSignature_ValidMatch";
        for _ in 0..5 {
            unsafe { writer_core.append_bytes(mock_string) };
        }
    });

    writer_handle.join().unwrap();

    for handle in reader_handles {
        let bytes_consumed = handle.join().unwrap();
        assert!(
            bytes_consumed > 0,
            "Stalled reader failed to resume from spin-lock loop!"
        );
    }

    // 3. Confirm that thread telemetry shards maintained structural isolation
    #[cfg(feature = "telemetry")]
    for shard in &core.reader_metrics {
        let ops = shard.ops_count.load(Ordering::Relaxed);
        assert!(
            ops > 0,
            "Telemetry line missed update records due to cache collisions!"
        );
    }
}

#[test]
fn test_reader_follows_the_published_rollover_successor() {
    let core = Arc::new(Core::new(4, 1));
    let payload = [b'B'; 1024];
    let reader = unsafe { ReaderHandle::new(Arc::clone(&core), 0) };

    // Advance the writer into a third slab before the reader reaches the first rollover.
    for _ in 0..4096 {
        unsafe { core.append_bytes(&payload) };
    }

    let mut out_buf = [0u8; 1024];

    for _ in 0..2039 {
        assert_eq!(reader.read_next_blocking(&mut out_buf), 1024);
    }
    assert_eq!(reader.read_next_blocking(&mut out_buf), 992);
    assert_eq!(reader.read_next_blocking(&mut out_buf), 32);
}

#[test]
fn test_writer_pins_the_successor_for_registered_readers() {
    let core = Arc::new(Core::new(3, 1));
    let _reader = unsafe { ReaderHandle::new(Arc::clone(&core), 0) };

    unsafe { core.append_bytes(&vec![b'B'; SLAB_SIZE - CACHE_LINE]) };

    assert_ne!(
        core.states[1].reader_mask.load(Ordering::Acquire) & 1,
        0,
        "a reader behind the writer must pin the slab it will enter next"
    );
}

#[test]
#[should_panic(expected = "at least two slabs")]
fn test_core_rejects_a_single_slab() {
    Core::new(1, 0);
}

#[test]
#[should_panic(expected = "reader ID exceeds configured reader capacity")]
fn test_reader_rejects_an_id_outside_configured_capacity() {
    let core = Arc::new(Core::new(2, 1));
    unsafe { ReaderHandle::new(core, 1) };
}

#[test]
fn test_reader_skips_a_four_byte_slab_tail() {
    let core = Arc::new(Core::new(3, 1));
    let reader = unsafe { ReaderHandle::new(Arc::clone(&core), 0) };
    let first_payload = vec![b'B'; SLAB_SIZE - CACHE_LINE - 8];

    unsafe { core.append_bytes(&first_payload) };
    unsafe { core.append_bytes(b"C") };

    let mut out_buf = [0u8; 1024];
    assert_eq!(reader.read_next_blocking(&mut out_buf), 1024);
    assert_eq!(reader.read_next_blocking(&mut out_buf), 1);
}

#[test]
fn test_out_of_order_non_sequential_slab_reclamation() {
    let core = Arc::new(Core::new(3, 2));

    // Manually pin client bit flags down onto slab state indices to simulate real execution load
    // Bit 0 set on Slab 0 = Reader ID 0 is pinned here
    core.states[0].reader_mask.store(1 << 0, Ordering::Release);
    core.states[1].reader_mask.store(0, Ordering::Release);
    core.states[2].reader_mask.store(0, Ordering::Release);

    // Append string payload that consumes almost an entire slab
    let small_string = vec![b'B'; SLAB_SIZE - 100];
    unsafe { core.append_bytes(&small_string) };

    // Verify out-of-band reader protection flags are maintained correctly
    let slab_0_mask = core.states[0].reader_mask.load(Ordering::Acquire);
    assert_ne!(
        slab_0_mask, 0,
        "Dynamic control registration registry suffered an unexpected flag drop!"
    );
}
