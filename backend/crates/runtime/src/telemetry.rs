use tracing_subscriber::{EnvFilter, fmt};

use crate::AppEnv;

/// Human-readable logs in dev; JSON everywhere else so log pipelines can index the structured
/// fields (`tip_id`, `job_id`, …). `RUST_LOG` overrides the default `info` filter.
pub fn init(app_env: AppEnv) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = fmt().with_env_filter(filter);
    let result = if app_env.is_dev() {
        builder.try_init()
    } else {
        builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .try_init()
    };
    // A second init (e.g. in tests) keeps the first subscriber, which is what we want.
    let _ = result;
}
