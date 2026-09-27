#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action::XDP_PASS,
    macros::{map, xdp},
    maps::{Array, XskMap},
    programs::XdpContext,
};

const ETH_P_IP: u16 = 0x0800;
const ETH_P_IPV6: u16 = 0x86dd;
const IPPROTO_UDP: u8 = 17;

#[map]
static UDP_PORT: Array<u16> = Array::with_max_entries(1, 0);

#[map]
static XSKS: XskMap = XskMap::with_max_entries(1, 0);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EthernetHeader {
    _destination: [u8; 6],
    _source: [u8; 6],
    ethertype: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct UdpHeader {
    _source: u16,
    destination: u16,
    _length: u16,
    _checksum: u16,
}

unsafe fn read<T: Copy>(ctx: &XdpContext, offset: usize) -> Result<T, ()> {
    let start = ctx.data();
    let end = ctx.data_end();
    let size = core::mem::size_of::<T>();
    if offset
        .checked_add(size)
        .is_none_or(|end_offset| start + end_offset > end)
    {
        return Err(());
    }
    Ok(unsafe { core::ptr::read_unaligned((start + offset) as *const T) })
}

fn udp_destination_port(ctx: &XdpContext) -> Result<u16, ()> {
    let ethernet: EthernetHeader = unsafe { read(ctx, 0)? };
    let ethertype = u16::from_be(ethernet.ethertype);
    let udp_offset = match ethertype {
        ETH_P_IP => {
            let version_and_ihl: u8 = unsafe { read(ctx, core::mem::size_of::<EthernetHeader>())? };
            if version_and_ihl >> 4 != 4 {
                return Err(());
            }
            let header_len = (version_and_ihl as usize & 0x0f) * 4;
            let protocol: u8 = unsafe { read(ctx, core::mem::size_of::<EthernetHeader>() + 9)? };
            if header_len < 20 || protocol != IPPROTO_UDP {
                return Err(());
            }
            core::mem::size_of::<EthernetHeader>() + header_len
        }
        ETH_P_IPV6 => {
            let next_header: u8 = unsafe { read(ctx, core::mem::size_of::<EthernetHeader>() + 6)? };
            if next_header != IPPROTO_UDP {
                return Err(());
            }
            core::mem::size_of::<EthernetHeader>() + 40
        }
        _ => return Err(()),
    };
    let udp: UdpHeader = unsafe { read(ctx, udp_offset)? };
    Ok(u16::from_be(udp.destination))
}

#[xdp]
pub fn vata_xdp(ctx: XdpContext) -> u32 {
    match udp_destination_port(&ctx) {
        Ok(port)
            if UDP_PORT
                .get(0)
                .is_some_and(|configured| *configured == port) =>
        {
            XSKS.redirect(ctx.rx_queue_index(), XDP_PASS as u64)
                .unwrap_or(XDP_PASS)
        }
        _ => XDP_PASS,
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
