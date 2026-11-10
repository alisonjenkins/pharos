//! Logs and Prometheus metrics for the service.

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Songs by how they ended: `ok`, `ffmpeg` (422) or `internal` (500).
pub const SONGS_TOTAL: &str = "pharos_aligner_songs_total";
/// Wall time of one song, the wait for the lock included.
pub const SONG_SECONDS: &str = "pharos_aligner_song_seconds";
/// A song takes seconds to minutes on the CPU.
const SONG_BUCKETS: &[f64] = &[1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0];

/// Plain `key=value` lines, as the Python service wrote, so the existing
/// LogQL line filters (`|= "path="`) keep matching. `RUST_LOG` overrides `info`.
pub fn init_logs() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false),
        )
        .try_init();
}

/// The recorder behind `/metrics`, plus the /proc collector it refreshes per scrape.
pub struct Metrics {
    handle: PrometheusHandle,
    process: metrics_process::Collector,
}

impl Metrics {
    pub fn install() -> Result<Self, String> {
        let handle = PrometheusBuilder::new()
            .set_buckets_for_metric(Matcher::Full(SONG_SECONDS.into()), SONG_BUCKETS)
            .and_then(PrometheusBuilder::install_recorder)
            .map_err(|e| format!("prometheus recorder: {e}"))?;
        let process = metrics_process::Collector::default();
        process.describe();
        metrics::describe_counter!(SONGS_TOTAL, "Songs aligned, by outcome");
        metrics::describe_histogram!(
            SONG_SECONDS,
            metrics::Unit::Seconds,
            "Wall time of one song"
        );
        Ok(Self { handle, process })
    }

    pub fn render(&self) -> String {
        self.process.collect();
        self.handle.render()
    }
}
