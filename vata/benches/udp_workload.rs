use std::env;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use vata::arena_alloc::{Core, ReaderSet};
use vata::udp_listener::spawn_receivers;

const DEFAULT_PACKETS: usize = 100_000;
const PAYLOAD_BYTES: usize = 1024;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);
const RECEIVE_SETTLE: Duration = Duration::from_millis(100);

fn packets() -> usize {
    env::var("VATA_UDP_PACKETS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value| value > 0)
        .unwrap_or(DEFAULT_PACKETS)
}

fn main() {
    let packets = packets();
    let port = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let core = Arc::new(Core::new_with_lanes(8, 1, 1));
    let reader = unsafe { ReaderSet::new(Arc::clone(&core), 0) };
    let _receivers = spawn_receivers(port, 64, core.writer_lanes(), Vec::new()).unwrap();
    let warmup_sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    warmup_sender
        .send_to(b"warmup", SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .unwrap();
    let warmup_deadline = Instant::now() + RECEIVE_TIMEOUT;
    let mut warmup_out = [0; 16];
    loop {
        if let Some(len) = reader.try_read_next(&mut warmup_out) {
            assert_eq!(&warmup_out[..len], b"warmup");
            break;
        }
        assert!(
            Instant::now() < warmup_deadline,
            "UDP listener did not receive the warmup datagram"
        );
        thread::yield_now();
    }
    let ready = Arc::new(Barrier::new(2));
    let sender_ready = Arc::clone(&ready);
    let (sent_tx, sent_rx) = mpsc::channel();
    let sender = thread::spawn(move || {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .unwrap();
        let payload = [b'B'; PAYLOAD_BYTES];
        sender_ready.wait();
        let sent = (0..packets)
            .filter(|_| socket.send(&payload).is_ok())
            .count();
        sent_tx.send(()).unwrap();
        sent
    });

    ready.wait();
    let started = Instant::now();
    let deadline = started + RECEIVE_TIMEOUT;
    let mut received = 0;
    let mut record_bytes = 0;
    let mut sender_done = false;
    let mut last_received = started;
    let mut out = [0; PAYLOAD_BYTES];
    while Instant::now() < deadline {
        if let Some(len) = reader.try_read_next(&mut out) {
            record_bytes += len;
            assert!(
                record_bytes <= PAYLOAD_BYTES,
                "unexpected UDP record length"
            );
            if record_bytes == PAYLOAD_BYTES {
                received += 1;
                record_bytes = 0;
            }
            last_received = Instant::now();
        } else {
            sender_done |= sent_rx.try_recv().is_ok();
            if sender_done && Instant::now().duration_since(last_received) >= RECEIVE_SETTLE {
                break;
            }
            thread::yield_now();
        }
    }
    let sent = sender.join().unwrap();
    let dropped = sent.saturating_sub(received);
    let elapsed = last_received.duration_since(started);

    println!("sent: {sent}");
    println!("received: {received}");
    println!("dropped: {dropped}");
    println!("elapsed: {elapsed:?}");
    println!(
        "receive packets/s: {:.0}",
        received as f64 / elapsed.as_secs_f64()
    );
    if dropped > 0 {
        eprintln!("UDP loss observed; reduce VATA_UDP_PACKETS for a loss-free comparison");
    }
}
