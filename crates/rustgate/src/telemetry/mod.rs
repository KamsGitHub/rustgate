//! Logging setup. Metrics and OpenTelemetry tracing arrive in Phase 7.
//!
//! JSON output includes the current span's fields, so every log line inside a request carries
//! its `request_id`, `route` and `upstream` without passing them around explicitly.

use tracing_subscriber::EnvFilter;

pub fn init(json: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    // `try_init` so tests (or an embedding application) that already installed a subscriber
    // don't panic.
    let _ = if json {
        builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .try_init()
    } else {
        builder.try_init()
    };
}
