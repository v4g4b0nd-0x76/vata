#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action::XDP_PASS,
    macros::xdp,
    programs::XdpContext,
};

#[xdp]
pub fn vata_xdp(_ctx: XdpContext) -> u32 {
    XDP_PASS
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
