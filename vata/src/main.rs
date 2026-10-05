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
    #[cfg(feature = "ingress")]
    let ingress_conf = conf
        .ingress
        .as_ref()
        .ok_or_else(|| VataErr::ConfLoadFailed(String::from("ingress conf not provided")))?;
    #[cfg(feature = "ingress")]
    if ingress_conf.receiver == 0 {
        return Err(VataErr::ConfLoadFailed(String::from(
            "ingress requires at least one receiver",
        )));
    }
    #[cfg(feature = "ingress")]
    if ingress_conf.processor != 0 && ingress_conf.processor != ingress_conf.receiver {
        return Err(VataErr::ConfLoadFailed(String::from(
            "ingress processors must match receivers; receivers now own writer lanes",
        )));
    }
    #[cfg(feature = "ingress")]
    if !ingress_conf.cpu_cores.is_empty() && ingress_conf.cpu_cores.len() != ingress_conf.receiver {
        return Err(VataErr::ConfLoadFailed(String::from(
            "ingress cpu_cores must contain one CPU per receiver",
        )));
    }
    if let Some(cpu) = conf.core_conf.numa_cpu {
        vata::cpu_tuning::validate_cpu(cpu)
            .map_err(|err| VataErr::CpuTuningFailed(err.to_string()))?;
    }
    if conf.client.is_some() && conf.core_conf.max_readers == 0 {
        return Err(VataErr::ConfLoadFailed(String::from(
            "client endpoint requires core_conf.max_readers >= 1",
        )));
    }
    #[cfg(feature = "ingress")]
    for &cpu in &ingress_conf.cpu_cores {
        vata::cpu_tuning::validate_cpu(cpu)
            .map_err(|err| VataErr::CpuTuningFailed(err.to_string()))?;
    }
    #[cfg(feature = "xdp")]
    if let Some(cpu) = conf.xdp_conf.as_ref().and_then(|config| config.cpu) {
        vata::cpu_tuning::validate_cpu(cpu)
            .map_err(|err| VataErr::CpuTuningFailed(err.to_string()))?;
    }
    let tcp_writer_lanes = usize::from(conf.client.is_some());
    #[cfg(feature = "xdp")]
    let xdp_writer_lanes = usize::from(conf.xdp_conf.is_some());
    #[cfg(not(feature = "xdp"))]
    let xdp_writer_lanes = 0usize;
    #[cfg(feature = "ingress")]
    let lanes = ingress_conf.receiver + tcp_writer_lanes + xdp_writer_lanes;
    #[cfg(not(feature = "ingress"))]
    let lanes = (tcp_writer_lanes + xdp_writer_lanes).max(1);
    let slab_count = slabs_for_gib(conf.core_conf.cap)?;
    if slab_count <= lanes {
        return Err(VataErr::ConfLoadFailed(String::from(
            "core capacity needs one free slab beyond every writer lane",
        )));
    }
    if let Some(cpu) = conf.core_conf.numa_cpu {
        vata::cpu_tuning::pin_current_thread(cpu)
            .map_err(|err| VataErr::CpuTuningFailed(err.to_string()))?;
    }
    let arena = Arc::new(Core::new_with_lanes(
        slab_count,
        conf.core_conf.max_readers,
        lanes,
    ));
    #[cfg(not(any(feature = "telemetry", feature = "ingress", feature = "xdp")))]
    let _ = &arena;

    #[cfg(feature = "ingress")]
    let mut writer_lanes = arena.writer_lanes();
    #[cfg(not(feature = "ingress"))]
    let mut writer_lanes: Vec<vata::arena_alloc::WriterLane> = if conf.client.is_some() {
        arena.writer_lanes()
    } else {
        Vec::new()
    };
    let client_writer = if conf.client.is_some() {
        writer_lanes.pop()
    } else {
        None
    };
    #[cfg(feature = "xdp")]
    let mut xdp_writer = if conf.xdp_conf.is_some() {
        writer_lanes.pop()
    } else {
        None
    };

    let _client = if let Some(config) = conf.client.as_ref() {
        let addr = config
            .addr
            .parse::<std::net::SocketAddr>()
            .map_err(|err| VataErr::ConfLoadFailed(format!("client.addr is invalid: {err}")))?;
        Some(
            vata::client::spawn(
                addr,
                Arc::clone(&arena),
                client_writer,
                0,
                config.queue_capacity,
            )
            .map_err(|err| VataErr::Client(err.to_string()))?,
        )
    } else {
        None
    };

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

    #[cfg(feature = "ingress")]
    {
        use vata::{UDP_RECV_BATCH, ingress::spawn_receivers};

        let blocking_handles = spawn_receivers(
            ingress_conf.port,
            UDP_RECV_BATCH,
            writer_lanes,
            ingress_conf.cpu_cores.clone(),
        )
        .map_err(|err| VataErr::Ingress(err.to_string()))?;
        eprintln!(
            "Ingress started on UDP port {} CPUs {:?}; graceful Ctrl-C/SIGINT handling is not installed yet",
            ingress_conf.port, ingress_conf.cpu_cores
        );
        for handle in blocking_handles {
            handle.join().expect("worker thread panicked");
        }
    }
    #[cfg(feature = "xdp")]
    {
        if let Some(cpu) = conf.xdp_conf.as_ref().and_then(|config| config.cpu) {
            vata::cpu_tuning::pin_current_thread(cpu)
                .map_err(|err| VataErr::CpuTuningFailed(err.to_string()))?;
        }
        let mut xdp = match conf.xdp_conf.as_ref() {
            Some(config) => Some(vata::xdp::start(config)?),
            None => None,
        };

        if let Some(xdp) = xdp.as_mut() {
            eprintln!(
                "XDP listener started on {}:{} CPU {:?}; graceful Ctrl-C/SIGINT handling is not installed yet",
                conf.xdp_conf
                    .as_ref()
                    .expect("started XDP has configuration")
                    .interface,
                conf.xdp_conf
                    .as_ref()
                    .expect("started XDP has configuration")
                    .udp_port,
                conf.xdp_conf
                    .as_ref()
                    .expect("started XDP has configuration")
                    .cpu
            );
            let writer = xdp_writer
                .as_mut()
                .expect("configured XDP must reserve a writer lane");
            xdp.run(writer)?;
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
