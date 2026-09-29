use tracing_subscriber::EnvFilter;

/// Install the global `tracing` subscriber.
///
/// The filter comes from `RUST_LOG` and defaults to `info`. Logs go to stderr,
/// keeping stdout for command output. Safe to call more than once; later calls
/// are ignored.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Install the global subscriber writing to `path` (appending), for
/// processes with no console: the Windows service and the session helper.
/// Falls back to stderr if the file cannot be opened.
pub fn init_file(path: &std::path::Path) {
    let file = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        });
    match file {
        Ok(file) => {
            let filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .try_init();
        }
        Err(e) => {
            init();
            tracing::warn!("cannot open log file {}: {e}", path.display());
        }
    }
}
