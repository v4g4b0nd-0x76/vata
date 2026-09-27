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
    let arena = Arc::new(Core::new(
        slabs_for_gib(conf.core_conf.cap)?,
        conf.core_conf.max_readers,
    ));
    let mut xdp = match conf.xdp_conf.as_ref() {
        Some(config) => Some(vata::xdp::start(config)?),
        None => None,
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

    if let Some(xdp) = xdp.as_mut() {
        xdp.run(&arena)?;
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
