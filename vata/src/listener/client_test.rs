use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::MAX_UDP_DATAGRAM;
use crate::arena_alloc::{Core, WriterLane};
use crate::listener::client::{
    ClientMode, Frame, Op, VataClient, records_from_body, records_to_body, spawn, subscribe_body,
    write_frame,
};
use crate::listener::ingress::spawn_receivers;

fn free_tcp_addr() -> SocketAddrV4 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port())
}

fn one_tcp_writer(core: &Arc<Core>) -> WriterLane {
    core.writer_lanes().pop().unwrap()
}

#[test]
fn protocol_open_frame_carries_read_mode_and_batch_size() {
    let mut wire = Vec::new();
    write_frame(
        &mut wire,
        &Frame {
            op: Op::Open,
            flags: 0,
            stream_id: 7,
            body: subscribe_body(ClientMode::Read, 2),
        },
    )
    .unwrap();

    let frame = Frame::read_from(&mut Cursor::new(wire)).unwrap();

    assert_eq!(frame.op, Op::Open);
    assert_eq!(frame.stream_id, 7);
    assert_eq!(frame.mode_and_batch().unwrap(), (ClientMode::Read, 2));
}

#[test]
fn protocol_open_frame_carries_read_write_mode() {
    let mut wire = Vec::new();
    write_frame(
        &mut wire,
        &Frame {
            op: Op::Open,
            flags: 0,
            stream_id: 7,
            body: subscribe_body(ClientMode::ReadWrite, 2),
        },
    )
    .unwrap();

    let frame = Frame::read_from(&mut Cursor::new(wire)).unwrap();

    assert_eq!(frame.mode_and_batch().unwrap(), (ClientMode::ReadWrite, 2));
}

#[test]
fn reader_connection_receives_configured_batch_size() {
    let core = Arc::new(Core::new(3, 1));
    let listener = spawn(free_tcp_addr(), Arc::clone(&core), None, 0, 8).expect("listener starts");
    let mut client = VataClient::connect_reader(listener.local_addr(), 2).unwrap();

    unsafe {
        core.append_batch(|writer| {
            writer.append(b"one");
            writer.append(b"two");
            writer.append(b"three");
            writer.append(b"four");
        });
    }

    let first = client.read_batch_timeout(Duration::from_secs(2)).unwrap();
    let second = client.read_batch_timeout(Duration::from_secs(2)).unwrap();

    assert_eq!(first, vec![b"one".to_vec(), b"two".to_vec()]);
    assert_eq!(second, vec![b"three".to_vec(), b"four".to_vec()]);
}

#[test]
fn reader_client_can_warm_connection_with_ping() {
    let core = Arc::new(Core::new(3, 1));
    let listener = spawn(free_tcp_addr(), Arc::clone(&core), None, 0, 8).expect("listener starts");

    VataClient::warm(listener.local_addr()).unwrap();
}

#[test]
fn reader_client_retries_until_listener_starts() {
    let addr = SocketAddr::from(free_tcp_addr());
    let core = Arc::new(Core::new(3, 1));
    let server_core = Arc::clone(&core);
    let server = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        spawn(addr, server_core, None, 0, 8).expect("listener starts")
    });

    let mut client =
        VataClient::connect_reader_with_retry(addr, 1, 20, Duration::from_millis(10)).unwrap();
    let _listener = server.join().unwrap();

    unsafe { core.append_bytes(b"ready") };

    assert_eq!(
        client.read_batch_timeout(Duration::from_secs(2)).unwrap(),
        vec![b"ready".to_vec()]
    );
}

#[test]
fn delivery_body_round_trips_records() {
    let payloads = [b"alpha".as_slice(), b"beta".as_slice()];
    let body = records_to_body(payloads);

    assert_eq!(
        records_from_body(&body, MAX_UDP_DATAGRAM).unwrap(),
        vec![b"alpha".to_vec(), b"beta".to_vec()]
    );
}

#[test]
fn read_write_connection_pushes_and_receives_on_one_socket() {
    let core = Arc::new(Core::new(3, 1));
    let listener = spawn(
        free_tcp_addr(),
        Arc::clone(&core),
        Some(one_tcp_writer(&core)),
        0,
        8,
    )
    .expect("listener starts");
    let mut client = VataClient::connect_read_writer(listener.local_addr(), 2).unwrap();

    client
        .write_batch([b"one".as_slice(), b"two".as_slice()])
        .unwrap();

    assert_eq!(
        client.read_batch_timeout(Duration::from_secs(2)).unwrap(),
        vec![b"one".to_vec(), b"two".to_vec()]
    );
}

#[test]
fn udp_and_tcp_writers_can_run_side_by_side() {
    let udp_port = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let core = Arc::new(Core::new_with_lanes(4, 1, 2));
    let mut lanes = core.writer_lanes();
    let tcp_writer = lanes.pop().unwrap();
    let _udp = spawn_receivers(udp_port, 64, lanes, Vec::new()).unwrap();
    let listener =
        spawn(free_tcp_addr(), Arc::clone(&core), Some(tcp_writer), 0, 8).expect("listener starts");
    let mut client = VataClient::connect_read_writer(listener.local_addr(), 2).unwrap();
    let udp = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();

    client.write_batch([b"tcp".as_slice()]).unwrap();
    udp.send_to(b"udp", SocketAddrV4::new(Ipv4Addr::LOCALHOST, udp_port))
        .unwrap();

    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.extend(client.read_batch_timeout(Duration::from_secs(2)).unwrap());
    }
    seen.sort();

    assert_eq!(seen, vec![b"tcp".to_vec(), b"udp".to_vec()]);
}
