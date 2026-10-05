use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::arena_alloc::{Core, ReaderSet};
use crate::listener::ingress::spawn_receivers;

#[test]
fn loopback_datagram_reaches_an_arena_writer_lane() {
    let port = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let core = Arc::new(Core::new_with_lanes(3, 1, 1));
    let reader = unsafe { ReaderSet::new(Arc::clone(&core), 0) };
    let _receivers = spawn_receivers(port, 64, core.writer_lanes(), Vec::new()).unwrap();
    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let payload = b"udp-loopback";

    sender
        .send_to(payload, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .unwrap();

    let mut out = [0; 64];
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(len) = reader.try_read_next(&mut out) {
            assert_eq!(&out[..len], payload);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "UDP ingress did not publish the datagram"
        );
        std::thread::yield_now();
    }
}
