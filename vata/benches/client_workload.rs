use std::env;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vata::arena_alloc::Core;
use vata::client::{VataClient, spawn};

const DEFAULT_RECORDS: usize = 100_000;
const PAYLOAD_BYTES: usize = 1024;
const BATCH: usize = 64;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn records() -> usize {
    env::var("VATA_CLIENT_RECORDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value| value > 0)
        .unwrap_or(DEFAULT_RECORDS)
}

fn free_tcp_addr() -> SocketAddrV4 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port())
}

fn main() {
    let records = records().div_ceil(BATCH) * BATCH;
    let core = Arc::new(Core::new_with_lanes(8, 1, 1));
    let writer = core.writer_lanes().pop().unwrap();
    let server = spawn(free_tcp_addr(), Arc::clone(&core), Some(writer), 0, 1024).unwrap();
    let mut client = VataClient::connect_read_writer(server.local_addr(), BATCH as u16).unwrap();
    let payload = [b'C'; PAYLOAD_BYTES];
    let started = Instant::now();
    let mut sent = 0usize;
    let mut received = 0usize;

    while sent < records {
        let batch = (records - sent).min(BATCH);
        client
            .write_batch(std::iter::repeat(payload.as_slice()).take(batch))
            .unwrap();
        sent += batch;

        while received < sent {
            for record in client.read_batch_timeout(READ_TIMEOUT).unwrap() {
                assert_eq!(record.len(), PAYLOAD_BYTES);
                received += 1;
            }
        }
    }

    let elapsed = started.elapsed();
    println!("sent: {sent}");
    println!("received: {received}");
    println!("elapsed: {elapsed:?}");
    println!(
        "client records/s: {:.0}",
        received as f64 / elapsed.as_secs_f64()
    );
}
