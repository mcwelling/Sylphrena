/// Tracing initialisation.  Writes to `.syl/syl.log` via a non-blocking file appender.
///
/// Returns the `WorkerGuard` that must be held alive for the log file to be flushed.
/// Drop it at the end of `main`.
use std::path::Path;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub fn init_logging(syl_dir: &Path) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(syl_dir)?;

    let file_appender = tracing_appender::rolling::never(syl_dir, "syl.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("syl=info,syl_rs=info,syl_core=info,syl_container=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();

    Ok(guard)
}
