use crate::{VataErr, XdpConf};

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
        socket::XdpSocket,
        {RingConfigBuilder, Rings},
    };

    const EBPF: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/vata-ebpf"));

    pub struct XdpIngress {
        _bpf: Ebpf,
        _socket: XdpSocket,
        _umem: Umem,
        _rings: Rings,
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
        let (mut rings, mut bind_flags) = builder
            .build_rings(&umem, RingConfigBuilder::default().build().map_err(failed)?)
            .map_err(failed)?;
        // ponytail: RX frames are seeded but never reclaimed; add a receive worker before live traffic.
        if unsafe { rings.fill_ring.enqueue(&mut umem, 2048) } == 0 {
            return Err(failed("AF_XDP fill ring accepted no frames"));
        }
        bind_flags.force_zerocopy();
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
            .attach(&config.interface, XdpMode::Driver)
            .map_err(failed)?;

        Ok(XdpIngress {
            _bpf: bpf,
            _socket: socket,
            _umem: umem,
            _rings: rings,
        })
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
