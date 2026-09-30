use std::sync::Arc;
use tikv_jemallocator::Jemalloc;
#[cfg(feature = "telemetry")]
use vata::arena_telemetry::ReportTelemetryOpts;
use vata::{
    Conf, VataErr,
    arena_alloc::{Core, SLAB_SIZE},
};

const GIB: usize = 1024 * 1024 * 1024;

fn slabs_for_gib(gib: usize) -> Result<usize, VataErr> {
    gib.checked_mul(GIB / SLAB_SIZE).ok_or_else(|| {
        VataErr::ConfLoadFailed("core_conf.cap is too large to convert from GiB to slabs".into())
    })
}

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), VataErr> {
    let conf = Arc::new(Conf::load()?);
    #[cfg(feature = "udp_listener")]
    let udp_listener_conf = conf
        .udp_listener
        .as_ref()
        .ok_or_else(|| VataErr::ConfLoadFailed(String::from("udp listener conf not provided")))?;
    #[cfg(feature = "udp_listener")]
    if udp_listener_conf.receiver == 0 {
        return Err(VataErr::ConfLoadFailed(String::from(
            "udp listener requires at least one receiver",
        )));
    }
    #[cfg(feature = "udp_listener")]
    if udp_listener_conf.processor != 0 && udp_listener_conf.processor != udp_listener_conf.receiver
    {
        return Err(VataErr::ConfLoadFailed(String::from(
            "udp listener processors must match receivers; receivers now own writer lanes",
        )));
    }
    #[cfg(feature = "udp_listener")]
    let lanes = udp_listener_conf.receiver;
    #[cfg(not(feature = "udp_listener"))]
    let lanes = 1;
    let slab_count = slabs_for_gib(conf.core_conf.cap)?;
    if slab_count <= lanes {
        return Err(VataErr::ConfLoadFailed(String::from(
            "core capacity needs one free slab beyond every writer lane",
        )));
    }
    let arena = Arc::new(Core::new_with_lanes(
        slab_count,
        conf.core_conf.max_readers,
        lanes,
    ));
    #[cfg(not(any(feature = "telemetry", feature = "udp_listener", feature = "xdp")))]
    let _ = &arena;

    #[cfg(feature = "telemetry")]
    let telemetry_opts = ReportTelemetryOpts {
        report_interval_ms: conf.telemetry_conf.report_interval_ms,
        writer_telemetry: arena.writer_metrics.clone(),
        reader_telemetry: arena.reader_metrics.clone(),
    };

    #[cfg(feature = "telemetry")]
    tokio::spawn(async move {
        use vata::arena_telemetry::report_telemetry;
        report_telemetry(telemetry_opts).await;
    });

    #[cfg(feature = "udp_listener")]
    {
        use vata::{UDP_RECV_BATCH, udp_listener::spawn_receivers};

        let blocking_handles =
            spawn_receivers(udp_listener_conf.port, UDP_RECV_BATCH, arena.writer_lanes())
                .map_err(|err| VataErr::SpwanUdpReceiver(err.to_string()))?;
        eprintln!(
            "UDP listener started on port {}; graceful Ctrl-C/SIGINT handling is not installed yet",
            udp_listener_conf.port
        );
        for handle in blocking_handles {
            handle.join().expect("worker thread panicked");
        }
    }
    #[cfg(feature = "xdp")]
    {
        let mut xdp = match conf.xdp_conf.as_ref() {
            Some(config) => Some(vata::xdp::start(config)?),
            None => None,
        };

        if let Some(xdp) = xdp.as_mut() {
            eprintln!(
                "XDP listener started on {}:{}; graceful Ctrl-C/SIGINT handling is not installed yet",
                conf.xdp_conf
                    .as_ref()
                    .expect("started XDP has configuration")
                    .interface,
                conf.xdp_conf
                    .as_ref()
                    .expect("started XDP has configuration")
                    .udp_port
            );
            xdp.run(&arena)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::slabs_for_gib;

    #[test]
    fn converts_gib_to_two_mebibyte_slabs() {
        assert_eq!(slabs_for_gib(8).unwrap(), 4096);
    }
}
