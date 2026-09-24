use std::sync::{Arc, atomic::Ordering};

use tokio::time::{self, interval};

use crate::arena_alloc::TelemetryShard;

pub struct ReportTelemetryOpts {
    pub report_interval_ms: u64,
    pub writer_telemetry: Arc<TelemetryShard>,
    pub reader_telemetry: Arc<Vec<TelemetryShard>>,
}
pub async fn report_telemetry(opts: ReportTelemetryOpts) {
    let report_interval_ms = opts.report_interval_ms;
    let writer_telemetry = opts.writer_telemetry;
    let reader_telemetry = opts.reader_telemetry;

    let mut tk = interval(time::Duration::from_millis(report_interval_ms));
    loop {
        tk.tick().await;
        // TODO: add tracing later for persist log rotation
        let writer_log_str = format!(
            r"\tops_count: {}\n\tbytes_processed: {}\n\ttotal_duration: {}",
            writer_telemetry.ops_count.load(Ordering::Relaxed),
            writer_telemetry.bytes_processed.load(Ordering::Relaxed),
            writer_telemetry.total_duration.load(Ordering::Relaxed)
        );
        let reader_log_str = reader_telemetry
            .iter()
            .enumerate()
            .map(|(idx, shard)| {
                format!(
                    r"\nreader_{}:\n\tops_count: {}\n\tbytes_processed: {}\n\ttotal_duration: {}",
                    idx,
                    shard.ops_count.load(Ordering::Relaxed),
                    shard.bytes_processed.load(Ordering::Relaxed),
                    shard.total_duration.load(Ordering::Relaxed)
                )
            })
            .collect::<String>();
        println!("{}{}", writer_log_str, reader_log_str);
    }
}
