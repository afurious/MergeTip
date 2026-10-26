//! Prometheus metrics (implementation plan "Metrics and alerts"). Exposed on `METRICS_ADDR`, an
//! internal port that is never routed publicly; unset, metrics are recorded nowhere.

use std::net::SocketAddr;

use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder};

/// Latency buckets in seconds: the alerts read p50/p99 from these histograms.
const SECONDS_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];
const ATTEMPT_BUCKETS: &[f64] = &[1.0, 2.0, 3.0, 4.0, 5.0];

/// The exporter configuration shared by both binaries and the tests.
pub fn builder() -> Result<PrometheusBuilder, BuildError> {
    PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), SECONDS_BUCKETS)?
        .set_buckets_for_metric(
            Matcher::Full("payment_attempts_per_tip".into()),
            ATTEMPT_BUCKETS,
        )
}

/// Serves `/metrics` on `addr` and makes it the process-wide recorder.
pub fn install(addr: SocketAddr) -> Result<(), BuildError> {
    builder()?.with_http_listener(addr).install()?;
    tracing::info!(%addr, "metrics listening");
    Ok(())
}
