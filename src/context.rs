//! The foreground application, recorded with each History entry.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::thread;
use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSWorkspace;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextSnapshot {
    pub application: Option<String>,
}

pub struct ContextMonitor {
    pub updates: Receiver<ContextSnapshot>,
    stop: Arc<AtomicBool>,
}

impl ContextMonitor {
    pub fn start() -> Self {
        Self::start_with_capture(ContextSnapshot::capture, Duration::from_millis(500))
    }

    fn start_with_capture(
        mut capture: impl FnMut() -> Result<ContextSnapshot> + Send + 'static,
        poll_interval: Duration,
    ) -> Self {
        let (sender, updates) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        thread::spawn(move || {
            let mut previous = None;
            let mut previous_error = None;
            while !worker_stop.load(Ordering::Acquire) {
                let context = match capture() {
                    Ok(context) => {
                        previous_error = None;
                        context
                    }
                    Err(error) => {
                        let error = error.to_string();
                        if previous_error.as_ref() != Some(&error) {
                            tracing::warn!(%error, "could not capture foreground context");
                            previous_error = Some(error);
                        }
                        ContextSnapshot::default()
                    }
                };
                if !publish_context(&sender, &mut previous, context) {
                    break;
                }
                thread::sleep(poll_interval);
            }
        });
        Self { updates, stop }
    }
}

fn publish_context(
    sender: &mpsc::SyncSender<ContextSnapshot>,
    previous: &mut Option<ContextSnapshot>,
    context: ContextSnapshot,
) -> bool {
    if previous.as_ref() == Some(&context) {
        return true;
    }
    match sender.try_send(context.clone()) {
        Ok(()) => *previous = Some(context),
        Err(TrySendError::Full(_)) => {}
        Err(TrySendError::Disconnected(_)) => return false,
    }
    true
}

impl Drop for ContextMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl ContextSnapshot {
    pub fn capture() -> Result<Self> {
        let application = autoreleasepool(|_| {
            let application = NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .ok_or_else(|| eyre!("macOS did not report a foreground application"))?;
            application
                .localizedName()
                .map(|name| name.to_string())
                .ok_or_else(|| eyre!("the foreground application has no display name"))
        })?;
        Ok(Self {
            application: Some(application),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn monitor_invalidates_stale_context_after_capture_failure() {
        let captured = ContextSnapshot {
            application: Some("Zed".into()),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_calls = calls.clone();
        let expected = captured.clone();
        let monitor = ContextMonitor::start_with_capture(
            move || {
                if capture_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    Ok(captured.clone())
                } else {
                    Err(eyre!("capture failed"))
                }
            },
            Duration::from_millis(1),
        );

        assert_eq!(
            monitor
                .updates
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            expected
        );
        assert_eq!(
            monitor
                .updates
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ContextSnapshot::default()
        );
    }
}
