use crate::{VataErr, XdpConf, arena_alloc::WriterLane};

#[derive(Debug)]
pub struct XdpProbeStats {
    pub packets: usize,
    pub payload_bytes: usize,
    pub elapsed: std::time::Duration,
    pub rx_dropped: u64,
    pub rx_invalid_descs: u64,
    pub rx_ring_full: u64,
    pub rx_fill_ring_empty_descs: u64,
}

impl XdpProbeStats {
    pub fn kernel_drops(&self) -> u64 {
        self.rx_dropped
            .saturating_add(self.rx_invalid_descs)
            .saturating_add(self.rx_ring_full)
            .saturating_add(self.rx_fill_ring_empty_descs)
    }
}

#[cfg(all(feature = "xdp", target_os = "linux"))]
mod linux {
    use super::*;
    use aya::{
        Ebpf,
        maps::{Array, XskMap},
        programs::{Xdp, XdpMode},
    };
    use std::{
        ffi::CString,
        os::fd::AsRawFd,
        time::{Duration, Instant},
    };
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
        start_inner(config, false)
    }

    /// Starts AF_XDP with a strict zero-copy bind. Unlike the normal listener,
    /// this never falls back to copy mode.
    pub fn start_zerocopy(config: &XdpConf) -> Result<XdpIngress, VataErr> {
        start_inner(config, true)
    }

    fn start_inner(config: &XdpConf, strict_zerocopy: bool) -> Result<XdpIngress, VataErr> {
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
        if strict_zerocopy {
            bind_flags.force_zerocopy();
        }
        if unsafe { rings.fill_ring.enqueue(&mut umem, 2048) } == 0 {
            return Err(failed("AF_XDP fill ring accepted no frames"));
        }
        let socket = builder.bind(nic, 0, bind_flags).map_err(|error| {
            if strict_zerocopy {
                failed(format!(
                    "AF_XDP strict zero-copy bind on {} queue 0 failed: {error}",
                    config.interface
                ))
            } else {
                failed(error)
            }
        })?;

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
        pub fn run(&mut self, writer: &mut WriterLane) -> Result<(), VataErr> {
            let mut packets = StackSlab::<BATCH_SIZE>::new();

            loop {
                if !self
                    .socket
                    .poll_read(PollTimeout::new(None))
                    .map_err(failed)?
                {
                    continue;
                }

                self.drain_ready(writer, &mut packets)?;
            }
        }

        /// Runs the same AF_XDP-to-arena path for a bounded hardware probe.
        pub fn run_for(
            &mut self,
            writer: &mut WriterLane,
            packet_limit: usize,
            timeout: Duration,
        ) -> Result<XdpProbeStats, VataErr> {
            let started = Instant::now();
            let deadline = started + timeout;
            let mut packets = StackSlab::<BATCH_SIZE>::new();
            let mut stats = XdpProbeStats {
                packets: 0,
                payload_bytes: 0,
                elapsed: Duration::ZERO,
                rx_dropped: 0,
                rx_invalid_descs: 0,
                rx_ring_full: 0,
                rx_fill_ring_empty_descs: 0,
            };

            while stats.packets < packet_limit {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let wait = deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(100));
                if !self
                    .socket
                    .poll_read(PollTimeout::new(Some(wait)))
                    .map_err(failed)?
                {
                    continue;
                }
                let (received, payload_bytes) = self.drain_ready(writer, &mut packets)?;
                stats.packets += received;
                stats.payload_bytes += payload_bytes;
            }

            stats.elapsed = started.elapsed();
            let kernel = self.socket.statistics().map_err(failed)?;
            stats.rx_dropped = kernel.rx_dropped;
            stats.rx_invalid_descs = kernel.rx_invalid_descs;
            stats.rx_ring_full = kernel.rx_ring_full;
            stats.rx_fill_ring_empty_descs = kernel.rx_fill_ring_empty_descs;
            Ok(stats)
        }

        #[inline(always)]
        fn drain_ready(
            &mut self,
            writer: &mut WriterLane,
            packets: &mut StackSlab<BATCH_SIZE>,
        ) -> Result<(usize, usize), VataErr> {
            let rx = self
                .rings
                .rx_ring
                .as_mut()
                .ok_or_else(|| failed("AF_XDP receive ring is disabled"))?;
            let received = unsafe { rx.recv(&self.umem, packets) };
            let mut payload_bytes = 0;
            writer.append_batch(|out| {
                while let Some(packet) = packets.pop_back() {
                    payload_bytes += packet.len();
                    out.append(&packet);
                    self.umem.free_packet(packet);
                }
            });

            if received != 0
                && unsafe { self.rings.fill_ring.enqueue(&mut self.umem, received) } != received
            {
                return Err(failed(
                    "AF_XDP fill ring could not recycle every received frame",
                ));
            }
            Ok((received, payload_bytes))
        }
    }
}

#[cfg(all(feature = "xdp", target_os = "linux"))]
pub use linux::{XdpIngress, start, start_zerocopy};

#[cfg(not(all(feature = "xdp", target_os = "linux")))]
pub struct XdpIngress;

#[cfg(not(all(feature = "xdp", target_os = "linux")))]
pub fn start(_: &XdpConf) -> Result<XdpIngress, VataErr> {
    Err(VataErr::XdpUnavailable(
        "rebuild on Linux with --features xdp".to_owned(),
    ))
}

#[cfg(not(all(feature = "xdp", target_os = "linux")))]
pub fn start_zerocopy(_: &XdpConf) -> Result<XdpIngress, VataErr> {
    Err(VataErr::XdpUnavailable(
        "rebuild on Linux with --features xdp".to_owned(),
    ))
}

#[cfg(not(all(feature = "xdp", target_os = "linux")))]
impl XdpIngress {
    pub fn run(&mut self, _: &mut WriterLane) -> Result<(), VataErr> {
        Err(VataErr::XdpUnavailable(
            "rebuild on Linux with --features xdp".to_owned(),
        ))
    }

    pub fn run_for(
        &mut self,
        _: &mut WriterLane,
        _: usize,
        _: std::time::Duration,
    ) -> Result<XdpProbeStats, VataErr> {
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
            cpu: None,
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

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn probe_stats_sums_kernel_drop_reasons() {
        let stats = XdpProbeStats {
            packets: 0,
            payload_bytes: 0,
            elapsed: std::time::Duration::ZERO,
            rx_dropped: 1,
            rx_invalid_descs: 2,
            rx_ring_full: 3,
            rx_fill_ring_empty_descs: 4,
        };

        assert_eq!(stats.kernel_drops(), 10);
    }
}
