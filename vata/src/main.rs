use std::sync::Arc;
use tikv_jemallocator::Jemalloc;
#[cfg(feature = "telemetry")]
use vata::arena_telemetry::ReportTelemetryOpts;
use vata::{Conf, VataErr, arena_alloc::Core};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), VataErr> {
    let conf = Arc::new(Conf::load()?);
    let arena = Arc::new(Core::new(conf.core_conf.cap, conf.core_conf.max_readers));

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
    // TODO: connection to nic on udp port provided in config
    // TODO: linker from nic mem to allocator with bump allocation
    Ok(())
}
