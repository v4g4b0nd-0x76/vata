use crate::{VataErr, XdpConf, arena_alloc::Core};

#[cfg(all(feature = "xdp", target_os = "linux"))]
mod linux {
    use super::*;
    use aya::{
        Ebpf,
        maps::{Array, XskMap},
        programs::{Xdp, XdpMode},
    };
    use std::{ffi::CString, os::fd::AsRawFd};
    use xdp::{
        Umem,
        nic::NicIndex,
        slab::{Slab, StackSlab},
        socket::{PollTimeout, XdpSocket},
        {RingConfigBuilder, Rings},
    };

    const EBPF: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/vata-ebpf"));
    const BATCH_SIZE: usize = 64;

    pub struct XdpIngress {
        _bpf: Ebpf,
        socket: XdpSocket,
        umem: Umem,
        rings: Rings,
    }

    fn failed(error: impl std::fmt::Display) -> VataErr {
        VataErr::XdpUnavailable(error.to_string())
    }

    pub fn start(config: &XdpConf) -> Result<XdpIngress, VataErr> {
        let interface = CString::new(config.interface.as_str()).map_err(failed)?;
        let nic = NicIndex::lookup_by_name(&interface)
            .map_err(failed)?
            .ok_or_else(|| failed(format!("interface {} does not exist", config.interface)))?;
        let mut umem = Umem::map(
            xdp::umem::UmemCfgBuilder::default()
                .build()
                .map_err(failed)?,
        )
        .map_err(failed)?;
        let mut builder = xdp::socket::XdpSocketBuilder::new().map_err(failed)?;
        let (mut rings, bind_flags) = builder
            .build_rings(&umem, RingConfigBuilder::default().build().map_err(failed)?)
            .map_err(failed)?;
        if unsafe { rings.fill_ring.enqueue(&mut umem, 2048) } == 0 {
            return Err(failed("AF_XDP fill ring accepted no frames"));
        }
        let socket = builder.bind(nic, 0, bind_flags).map_err(failed)?;

        let mut bpf = Ebpf::load(EBPF).map_err(failed)?;
        let mut ports: Array<_, u16> = bpf
            .map_mut("UDP_PORT")
            .ok_or_else(|| failed("UDP_PORT map is missing"))?
            .try_into()
            .map_err(failed)?;
        ports.set(0, config.udp_port, 0).map_err(failed)?;
        let mut sockets: XskMap<_> = bpf
            .map_mut("XSKS")
            .ok_or_else(|| failed("XSKS map is missing"))?
            .try_into()
            .map_err(failed)?;
        sockets.set(0, socket.as_raw_fd(), 0).map_err(failed)?;
        let program: &mut Xdp = bpf
            .program_mut("vata_xdp")
            .ok_or_else(|| failed("vata_xdp program is missing"))?
            .try_into()
            .map_err(failed)?;
        program.load().map_err(failed)?;
        program
            .attach(&config.interface, XdpMode::Default)
            .map_err(failed)?;

        Ok(XdpIngress {
            _bpf: bpf,
            socket,
            umem,
            rings,
        })
    }

    impl XdpIngress {
        pub fn run(&mut self, core: &Core) -> Result<(), VataErr> {
            let rx = self
                .rings
                .rx_ring
                .as_mut()
                .ok_or_else(|| failed("AF_XDP receive ring is disabled"))?;
            let mut packets = StackSlab::<BATCH_SIZE>::new();

            loop {
                if !self
                    .socket
                    .poll_read(PollTimeout::new(None))
                    .map_err(failed)?
                {
                    continue;
                }

                let received = unsafe { rx.recv(&self.umem, &mut packets) };
                unsafe {
                    core.append_batch(|writer| {
                        while let Some(packet) = packets.pop_back() {
                            writer.append(&packet);
                            self.umem.free_packet(packet);
                        }
                    });
                }

                if received != 0
                    && unsafe { self.rings.fill_ring.enqueue(&mut self.umem, received) } != received
                {
                    return Err(failed(
                        "AF_XDP fill ring could not recycle every received frame",
                    ));
                }
            }
        }
    }
}

#[cfg(all(feature = "xdp", target_os = "linux"))]
pub use linux::{XdpIngress, start};

#[cfg(not(all(feature = "xdp", target_os = "linux")))]
pub struct XdpIngress;

#[cfg(not(all(feature = "xdp", target_os = "linux")))]
pub fn start(_: &XdpConf) -> Result<XdpIngress, VataErr> {
    Err(VataErr::XdpUnavailable(
        "rebuild on Linux with --features xdp".to_owned(),
    ))
}

#[cfg(not(all(feature = "xdp", target_os = "linux")))]
impl XdpIngress {
    pub fn run(&mut self, _: &Core) -> Result<(), VataErr> {
        Err(VataErr::XdpUnavailable(
            "rebuild on Linux with --features xdp".to_owned(),
        ))
    }
}

#[cfg(all(test, not(all(feature = "xdp", target_os = "linux"))))]
mod tests {
    use super::*;

    #[test]
    fn configured_xdp_rejects_a_binary_without_linux_xdp_support() {
        let result = start(&XdpConf {
            interface: "eth0".to_owned(),
            udp_port: 5353,
        });
        let Err(error) = result else {
            panic!("a binary without the xdp feature must reject xdp configuration");
        };

        assert_eq!(
            error.to_string(),
            "XDP is unavailable: rebuild on Linux with --features xdp\n"
        );
    }
}
