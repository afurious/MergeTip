use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::AppEnv;

/// Human-readable logs in dev; JSON everywhere else so log pipelines can index the structured
/// fields (`tip_id`, `job_id`, …). `RUST_LOG` overrides the default `info` filter.
///
/// With a Sentry DSN, errors (and `error!` events) also go to Sentry. Default PII is off: no
/// IPs, headers or user data are attached, and the logs themselves never carry secrets. Keep the
/// returned guard alive for the life of the process; dropping it flushes Sentry.
#[must_use = "dropping the guard shuts Sentry down"]
pub fn init(app_env: AppEnv, sentry_dsn: Option<&str>) -> Option<sentry::ClientInitGuard> {
    let guard = sentry_dsn.map(|dsn| {
        let mut options = sentry::ClientOptions::default();
        options.release = sentry::release_name!();
        options.environment = Some(app_env.as_str().into());
        options.send_default_pii = false;
        sentry::init((dsn, options))
    });
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let output = if app_env.is_dev() {
        fmt::layer().boxed()
    } else {
        fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .boxed()
    };
    let sentry_layer = guard
        .as_ref()
        .map(|_| sentry::integrations::tracing::layer());
    // A second init (e.g. in tests) keeps the first subscriber, which is what we want.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(output)
        .with(sentry_layer)
        .try_init();
    guard
}
