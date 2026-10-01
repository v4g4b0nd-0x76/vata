use crate::arena_alloc::WriterLane;
use crate::{MAX_UDP_DATAGRAM, UDP_SOCKET_BUF_SIZE};
#[cfg(target_os = "linux")]
use libc::mmsghdr;
use libc::{EAGAIN, EINTR, iovec};
use socket2::{Domain, Protocol, Socket, Type};
use std::io::Result;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::AsRawFd;
use std::thread::{self, JoinHandle};

pub type PacketBuf = Box<[u8; MAX_UDP_DATAGRAM]>;

#[cfg(target_os = "macos")]
const SYS_RECVMSG_X: libc::c_int = 480;

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
#[repr(C)]
struct MsghdrX {
    msg_name: *mut libc::c_void,
    msg_namelen: libc::socklen_t,
    msg_iov: *mut iovec,
    msg_iovlen: libc::c_int,
    msg_control: *mut libc::c_void,
    msg_controllen: libc::socklen_t,
    msg_flags: libc::c_int,
    msg_datalen: usize,
}

fn new_socket(port: u16) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_recv_buffer_size(UDP_SOCKET_BUF_SIZE)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    Ok(socket.into())
}

pub fn spawn_receivers(
    port: u16,
    recv_batch: usize,
    writers: Vec<WriterLane>,
    cpu_cores: Vec<usize>,
) -> Result<Vec<JoinHandle<()>>> {
    let mut handles = Vec::with_capacity(writers.len());
    for (thread_idx, writer) in writers.into_iter().enumerate() {
        let socket = new_socket(port)?;
        let cpu = cpu_cores.get(thread_idx).copied();
        handles.push(
            thread::Builder::new()
                .name(format!("udp-recv-{thread_idx}"))
                .spawn(move || {
                    if let Some(cpu) = cpu {
                        crate::cpu_tuning::pin_current_thread(cpu)
                            .expect("validated UDP CPU must remain available");
                    }
                    receiver_loop(socket, recv_batch, writer)
                })?,
        );
    }
    Ok(handles)
}

pub struct RecvMmsg {
    bufs: Vec<PacketBuf>,
    _iovecs: Vec<iovec>,
    #[cfg(target_os = "linux")]
    msgs: Vec<mmsghdr>,
    #[cfg(target_os = "macos")]
    msgs: Vec<MsghdrX>,
}

impl RecvMmsg {
    pub fn new(batch: usize) -> Self {
        let mut bufs = Vec::with_capacity(batch);
        let mut iovecs = vec![unsafe { std::mem::zeroed::<iovec>() }; batch];
        #[cfg(target_os = "linux")]
        let mut msgs = vec![unsafe { std::mem::zeroed::<mmsghdr>() }; batch];
        #[cfg(target_os = "macos")]
        let mut msgs = vec![unsafe { std::mem::zeroed::<MsghdrX>() }; batch];

        for _ in 0..batch {
            bufs.push(Box::new([0; MAX_UDP_DATAGRAM]));
        }

        for i in 0..batch {
            iovecs[i].iov_base = bufs[i].as_mut_ptr().cast();
            iovecs[i].iov_len = MAX_UDP_DATAGRAM;
            #[cfg(target_os = "linux")]
            {
                msgs[i].msg_hdr.msg_iov = &mut iovecs[i];
                msgs[i].msg_hdr.msg_iovlen = 1;
            }
            #[cfg(target_os = "macos")]
            {
                msgs[i].msg_iov = &mut iovecs[i];
                msgs[i].msg_iovlen = 1;
                msgs[i].msg_datalen = MAX_UDP_DATAGRAM;
            }
        }

        Self {
            bufs,
            _iovecs: iovecs,
            msgs,
        }
    }
}

fn receiver_loop(socket: UdpSocket, batch: usize, mut writer: WriterLane) {
    let fd = socket.as_raw_fd();
    let mut recv = RecvMmsg::new(batch);

    loop {
        append_received(fd, batch, false, &mut recv, &mut writer);
        while append_received(fd, batch, true, &mut recv, &mut writer) {}
    }
}

fn append_received(
    fd: i32,
    batch: usize,
    nonblocking: bool,
    recv: &mut RecvMmsg,
    writer: &mut WriterLane,
) -> bool {
    #[cfg(target_os = "linux")]
    {
        append_received_linux(fd, batch, nonblocking, recv, writer)
    }
    #[cfg(target_os = "macos")]
    {
        append_received_macos(fd, batch, nonblocking, recv, writer)
    }
}

#[cfg(target_os = "linux")]
fn append_received_linux(
    fd: i32,
    batch: usize,
    nonblocking: bool,
    recv: &mut RecvMmsg,
    writer: &mut WriterLane,
) -> bool {
    let received = unsafe { recvmmsg_batch(fd, recv.msgs.as_mut_ptr(), batch as u32, nonblocking) };
    if received <= 0 {
        let error = std::io::Error::last_os_error();
        return error.raw_os_error() == Some(EINTR)
            || (!nonblocking && error.raw_os_error() != Some(EAGAIN));
    }

    writer.append_batch(|out| {
        for idx in 0..received as usize {
            let len = recv.msgs[idx].msg_len as usize;
            if len > 0 && len <= MAX_UDP_DATAGRAM {
                out.append(&recv.bufs[idx][..len]);
            }
        }
    });
    true
}

#[cfg(target_os = "macos")]
fn append_received_macos(
    fd: i32,
    batch: usize,
    nonblocking: bool,
    recv: &mut RecvMmsg,
    writer: &mut WriterLane,
) -> bool {
    let limit = if nonblocking { batch } else { 1 };
    let received =
        unsafe { recvmsg_x_batch(fd, recv.msgs.as_mut_ptr(), limit as u32, nonblocking) };
    if received <= 0 {
        let error = std::io::Error::last_os_error();
        return error.raw_os_error() == Some(EINTR)
            || (!nonblocking && error.raw_os_error() != Some(EAGAIN));
    }

    writer.append_batch(|out| {
        for idx in 0..received as usize {
            let len = recv.msgs[idx].msg_datalen;
            if len > 0 && len <= MAX_UDP_DATAGRAM {
                out.append(&recv.bufs[idx][..len]);
            }
        }
    });
    true
}

#[cfg(target_os = "macos")]
unsafe fn recvmsg_x_batch(fd: i32, msgs: *mut MsghdrX, batch: u32, nonblocking: bool) -> i32 {
    let flags = if nonblocking { libc::MSG_DONTWAIT } else { 0 };
    unsafe { libc::syscall(SYS_RECVMSG_X, fd, msgs, batch, flags) as i32 }
}

#[cfg(target_os = "linux")]
unsafe fn recvmmsg_batch(fd: i32, msgs: *mut mmsghdr, batch: u32, nonblocking: bool) -> i32 {
    #[cfg(not(all(target_env = "musl", target_os = "linux")))]
    {
        let flags = if nonblocking {
            libc::MSG_DONTWAIT
        } else {
            libc::MSG_WAITFORONE
        };
        unsafe { libc::recvmmsg(fd, msgs, batch, flags, std::ptr::null_mut()) }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::arena_alloc::{Core, ReaderSet};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn macos_nonblocking_drain_publishes_partial_batch_before_eagain() {
        let socket = new_socket(0).unwrap();
        let port = socket.local_addr().unwrap().port();
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let core = Arc::new(Core::new_with_lanes(3, 1, 1));
        let reader = unsafe { ReaderSet::new(Arc::clone(&core), 0) };
        let mut writers = core.writer_lanes();
        let mut writer = writers.pop().unwrap();
        let mut recv = RecvMmsg::new(64);

        for payload in [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()] {
            sender.send_to(payload, addr).unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        while !append_received(socket.as_raw_fd(), 64, true, &mut recv, &mut writer) {
            assert!(Instant::now() < deadline, "UDP datagrams did not arrive");
            thread::yield_now();
        }

        let mut out = [0; 8];
        for payload in [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()] {
            let len = reader.try_read_next(&mut out).unwrap();
            assert_eq!(&out[..len], payload);
        }
    }
}
