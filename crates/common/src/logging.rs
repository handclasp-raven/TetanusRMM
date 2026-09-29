use tracing_subscriber::EnvFilter;

/// Install the global `tracing` subscriber.
///
/// The filter comes from `RUST_LOG` and defaults to `info`. Safe to call more
/// than once; later calls are ignored.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
