pub mod udp_listener;

pub const UDP_SOCKET_BUF_SIZE: usize = 64 * 1024 * 1024;
pub const UDP_IO_BATCH_TARGET_BYTES: usize = 256 * 1024;
pub const UDP_IO_BATCH: usize = UDP_IO_BATCH_TARGET_BYTES / MAX_UDP_DATAGRAM;
pub const UDP_FRAME_HEADER_SIZE: usize = 5;
pub const MAX_UDP_DATAGRAM: usize = 1472;
pub const MAX_UDP_FRAME_PAYLOAD: usize = MAX_UDP_DATAGRAM - UDP_FRAME_HEADER_SIZE;
pub const fn items_per_frame(wire_bytes: usize) -> usize {
    if wire_bytes == 0 {
        return 0;
    }
    MAX_UDP_FRAME_PAYLOAD / wire_bytes
}
pub const UDP_RECV_BATCH_TARGET_BYTES: usize = 512 * 1024;

pub const UDP_RECV_BATCH: usize = UDP_RECV_BATCH_TARGET_BYTES / MAX_UDP_DATAGRAM;
#[cfg(test)]
mod udp_test;
