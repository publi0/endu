use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use color_eyre::eyre::{Result, eyre};
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::prelude::*;

pub fn support_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HEX_APPLICATION_SUPPORT_DIR") {
        return Ok(path.into());
    }
    Ok(dirs::data_dir()
        .ok_or_else(|| eyre!("application data directory is unavailable"))?
        .join("hex-openrouter"))
}

pub fn logs_dir() -> Result<PathBuf> {
    Ok(support_dir()?.join("logs"))
}

/// Diagnostics logs are append-only across launches, so cap each file at
/// startup: past the limit the previous content moves to a `.1` sibling and
/// the fresh file starts empty. One retained generation is plenty for
/// post-mortem work and bounds total disk use.
fn open_capped_log(path: PathBuf) -> io::Result<File> {
    use std::os::unix::fs::PermissionsExt;
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    if let Ok(metadata) = fs::metadata(&path)
        && metadata.len() >= MAX_BYTES
    {
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&path, &rotated);
    }
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    // Logs carry full transcripts and provider diagnostics; keep them
    // owner-only regardless of the ambient umask.
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(&path, permissions)?;
    Ok(file)
}

/// Creates the logs directory, appends process diagnostics to `process.log`
/// alongside stderr, installs the global tracing subscriber, and routes Ctrl-C
/// to `shutdown`. Returns the logs directory.
pub fn init_process_logging(shutdown: &'static AtomicBool) -> Result<PathBuf> {
    let log_dir = logs_dir()?;
    fs::create_dir_all(&log_dir)?;
    // The directory holds full transcripts; keep it owner-only too.
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&log_dir)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&log_dir, permissions)?;
    }
    let process_log = open_capped_log(log_dir.join("process.log"))?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("voice_control=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr.and(Mutex::new(process_log)))
                // These libraries may trace URLs (which contain keyterms), headers
                // or PCM frames. Apply this independently of RUST_LOG overrides.
                .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                    safe_log_target(metadata.target())
                })),
        )
        .init();
    ctrlc::set_handler(|| shutdown.store(true, Ordering::Relaxed))?;
    Ok(log_dir)
}

fn safe_log_target(target: &str) -> bool {
    !["tungstenite", "ureq", "ureq_proto", "rustls"]
        .iter()
        .any(|name| {
            target == *name
                || target
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with("::"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[derive(Clone)]
    struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn transport_traces_are_excluded_even_under_trace_logging() {
        let buffer = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(buffer.clone());
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "trace,tungstenite::protocol=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(move || sink.clone())
                    .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                        safe_log_target(metadata.target())
                    })),
            );
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "tungstenite::protocol", "PRIVATE_PCM_MARKER");
            tracing::debug!(target: "ureq::request", "PRIVATE_TERMS_MARKER");
            tracing::debug!(target: "rustls", "PRIVATE_TLS_MARKER");
            tracing::info!(target: "voice_control::providers", "safe category");
        });
        let log = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(!log.contains("PRIVATE_"));
        assert!(log.contains("safe category"));
    }
}
