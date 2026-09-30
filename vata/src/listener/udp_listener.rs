use crate::arena_alloc::WriterLane;
use crate::{MAX_UDP_DATAGRAM, UDP_SOCKET_BUF_SIZE};
use libc::{EAGAIN, EINTR, iovec, mmsghdr};
use socket2::{Domain, Protocol, Socket, Type};
use std::io::Result;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::AsRawFd;
use std::thread::{self, JoinHandle};

pub type PacketBuf = Box<[u8; MAX_UDP_DATAGRAM]>;

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
    msgs: Vec<mmsghdr>,
}

impl RecvMmsg {
    pub fn new(batch: usize) -> Self {
        let mut bufs = Vec::with_capacity(batch);
        let mut iovecs = vec![unsafe { std::mem::zeroed::<iovec>() }; batch];
        let mut msgs = vec![unsafe { std::mem::zeroed::<mmsghdr>() }; batch];

        for _ in 0..batch {
            bufs.push(Box::new([0; MAX_UDP_DATAGRAM]));
        }

        for i in 0..batch {
            iovecs[i].iov_base = bufs[i].as_mut_ptr().cast();
            iovecs[i].iov_len = MAX_UDP_DATAGRAM;
            msgs[i].msg_hdr.msg_iov = &mut iovecs[i];
            msgs[i].msg_hdr.msg_iovlen = 1;
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
