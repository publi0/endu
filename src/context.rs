//! The foreground application, recorded with each History entry.

use std::ptr::NonNull;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::thread;
#[cfg(test)]
use std::time::Duration;

use block2::RcBlock;
use color_eyre::eyre::{Result, eyre};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};

use crate::doorbell::Doorbell;

/// A retained application instance, not just a display name or reusable PID.
/// NSRunningApplication's equality is the SDK's supported identity comparison.
#[derive(Clone, Debug, PartialEq)]
pub enum ForegroundApplication {
    Native(Retained<NSRunningApplication>),
    #[cfg(test)]
    Test(u64),
}

impl ForegroundApplication {
    fn capture() -> Result<Self> {
        autoreleasepool(|_| {
            NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .map(Self::Native)
                .ok_or_else(|| eyre!("macOS did not report a foreground application"))
        })
    }

    pub fn current_process_id(&self) -> Option<i32> {
        autoreleasepool(|_| {
            if Self::capture().ok().as_ref() != Some(self) {
                return None;
            }
            match self {
                Self::Native(application) if !application.isTerminated() => {
                    let pid = application.processIdentifier();
                    (pid > 0).then_some(pid)
                }
                _ => None,
            }
        })
    }

    /// The application's bundle directory, for inspecting its embedded
    /// frameworks. Reads only metadata macOS already holds.
    pub fn bundle_path(&self) -> Option<std::path::PathBuf> {
        match self {
            Self::Native(application) => autoreleasepool(|_| {
                application
                    .bundleURL()?
                    .path()
                    .map(|path| path.to_string().into())
            }),
            #[cfg(test)]
            Self::Test(_) => None,
        }
    }

    fn name(&self) -> Option<String> {
        match self {
            Self::Native(application) => application.localizedName().map(|name| name.to_string()),
            #[cfg(test)]
            Self::Test(_) => Some("Test application".into()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextSnapshot {
    pub application: Option<String>,
    pub target: Option<ForegroundApplication>,
}

type ObserverToken = Retained<ProtocolObject<dyn NSObjectProtocol>>;

pub struct ContextMonitor {
    pub updates: Receiver<ContextSnapshot>,
    observer: Option<(Retained<NSNotificationCenter>, ObserverToken)>,
    #[cfg(test)]
    stop: Option<Arc<AtomicBool>>,
}

impl ContextMonitor {
    pub fn start(wake: Arc<Doorbell>) -> Self {
        let (sender, updates) = mpsc::sync_channel(8);
        let state = Arc::new(Mutex::new((None::<ContextSnapshot>, None::<String>)));
        let capture_and_publish = {
            let sender = sender.clone();
            let state = state.clone();
            let wake = wake.clone();
            move || {
                let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                let (previous, previous_error) = &mut *guard;
                let context = match ContextSnapshot::capture() {
                    Ok(context) => {
                        *previous_error = None;
                        context
                    }
                    Err(error) => {
                        let error = error.to_string();
                        if previous_error.as_ref() != Some(&error) {
                            tracing::warn!(%error, "could not capture foreground context");
                            *previous_error = Some(error);
                        }
                        ContextSnapshot::default()
                    }
                };
                if publish_context_and_wake(&sender, previous, context, Some(&wake)) {
                    // Published or unchanged.
                }
            }
        };
        capture_and_publish();
        let observer = autoreleasepool(|_| {
            let center = NSWorkspace::sharedWorkspace().notificationCenter();
            // SAFETY: AppKit's notification name is an immutable static.
            let name = unsafe { NSWorkspaceDidActivateApplicationNotification };
            let callback = RcBlock::new(move |_: NonNull<NSNotification>| capture_and_publish());
            // SAFETY: The observer is removed on Drop before the monitor ends.
            let token = unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &callback)
            };
            (center, token)
        });
        Self {
            updates,
            observer: Some(observer),
            #[cfg(test)]
            stop: None,
        }
    }

    #[cfg(test)]
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
        Self {
            updates,
            observer: None,
            stop: Some(stop),
        }
    }
}

#[cfg(test)]
fn publish_context(
    sender: &mpsc::SyncSender<ContextSnapshot>,
    previous: &mut Option<ContextSnapshot>,
    context: ContextSnapshot,
) -> bool {
    publish_context_and_wake(sender, previous, context, None)
}

fn publish_context_and_wake(
    sender: &mpsc::SyncSender<ContextSnapshot>,
    previous: &mut Option<ContextSnapshot>,
    context: ContextSnapshot,
    wake: Option<&Doorbell>,
) -> bool {
    if previous.as_ref() == Some(&context) {
        return true;
    }
    match sender.try_send(context.clone()) {
        Ok(()) => {
            *previous = Some(context);
            if let Some(wake) = wake {
                wake.ring();
            }
        }
        Err(TrySendError::Full(_)) => {}
        Err(TrySendError::Disconnected(_)) => return false,
    }
    true
}

impl Drop for ContextMonitor {
    fn drop(&mut self) {
        if let Some((center, token)) = self.observer.take() {
            // SAFETY: Token came from this center's addObserver call.
            unsafe { center.removeObserver(token.as_ref()) };
        }
        #[cfg(test)]
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
    }
}

impl ContextSnapshot {
    pub fn capture() -> Result<Self> {
        autoreleasepool(|_| {
            let target = ForegroundApplication::capture()?;
            Ok(Self {
                application: target.name(),
                target: Some(target),
            })
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
            target: Some(ForegroundApplication::Test(1)),
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
