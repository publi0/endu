//! Keep nonactivating overlays on the current macOS Space without activating Hex.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSPanel, NSWindowCollectionBehavior, NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter};

const RETRY_INTERVAL: Duration = Duration::from_millis(250);

pub fn collection_behavior() -> NSWindowCollectionBehavior {
    NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::CanJoinAllApplications
        | NSWindowCollectionBehavior::FullScreenAuxiliary
        | NSWindowCollectionBehavior::Stationary
        | NSWindowCollectionBehavior::IgnoresCycle
}

struct SpaceObserver {
    center: Retained<NSNotificationCenter>,
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    changed: Arc<AtomicBool>,
}

impl SpaceObserver {
    fn new(center: Retained<NSNotificationCenter>) -> Self {
        let changed = Arc::new(AtomicBool::new(false));
        let pending = changed.clone();
        // Notifications can arrive off the main thread. Only mark work here;
        // the existing UI loop performs every AppKit window operation.
        let callback = RcBlock::new(move |_: NonNull<NSNotification>| {
            pending.store(true, Ordering::Release);
        });
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceActiveSpaceDidChangeNotification),
                None,
                None,
                &callback,
            )
        };
        Self {
            center,
            token,
            changed,
        }
    }

    fn take_change(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }
}

impl Drop for SpaceObserver {
    fn drop(&mut self) {
        unsafe { self.center.removeObserver((*self.token).as_ref()) };
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Presentation {
    Unchanged,
    Rejoin,
    Hide,
}

#[derive(Default)]
struct VisibilityState {
    ordered: bool,
    last_attempt: Option<Instant>,
}

impl VisibilityState {
    fn update(
        &mut self,
        wanted: bool,
        space_changed: bool,
        on_active_space: bool,
        visible: bool,
        now: Instant,
    ) -> Presentation {
        if !wanted {
            let hide = self.ordered || visible;
            self.ordered = false;
            self.last_attempt = None;
            return if hide {
                Presentation::Hide
            } else {
                Presentation::Unchanged
            };
        }
        let first_show = !self.ordered;
        let missing = !on_active_space || !visible;
        let retry_due = self
            .last_attempt
            .is_none_or(|last| now.duration_since(last) >= RETRY_INTERVAL);
        if first_show || space_changed || (missing && retry_due) {
            self.ordered = true;
            self.last_attempt = Some(now);
            // A Space may have changed while the overlay was hidden. Rejoin
            // on every new presentation, even if AppKit reports stale flags.
            Presentation::Rejoin
        } else {
            Presentation::Unchanged
        }
    }
}

pub struct OverlayVisibility {
    observer: SpaceObserver,
    state: VisibilityState,
}

impl OverlayVisibility {
    pub fn new() -> Self {
        Self {
            // NSWorkspace posts Space changes on its own center, not the
            // process-wide default NSNotificationCenter.
            observer: SpaceObserver::new(NSWorkspace::sharedWorkspace().notificationCenter()),
            state: VisibilityState::default(),
        }
    }

    pub fn update(&mut self, panel: &NSPanel, wanted: bool) {
        let space_changed = self.observer.take_change();
        // Idle overlays need no WindowServer queries on the 16 ms UI tick.
        if !wanted && !self.state.ordered {
            return;
        }
        let on_active_space = panel.isOnActiveSpace();
        let action = self.state.update(
            wanted,
            space_changed,
            on_active_space,
            panel.isVisible(),
            Instant::now(),
        );
        match action {
            Presentation::Unchanged => {}
            Presentation::Hide => panel.orderOut(None),
            Presentation::Rejoin => {
                // A prewarmed/reused panel can retain an old Space association.
                // Re-register all-Spaces membership while it is offscreen,
                // then order it without becoming key or activating the app.
                // Do not combine CanJoinAllSpaces with MoveToActiveSpace.
                panel.orderOut(None);
                panel.setCollectionBehavior(
                    collection_behavior() & !NSWindowCollectionBehavior::CanJoinAllSpaces,
                );
                panel.setCollectionBehavior(collection_behavior());
                panel.orderFrontRegardless();
                tracing::debug!(
                    space_changed,
                    previously_on_active_space = on_active_space,
                    on_active_space = panel.isOnActiveSpace(),
                    "overlay refreshed for active Space"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_change_refreshes_an_already_ordered_overlay() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        assert_eq!(
            state.update(true, false, true, false, now),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(true, false, true, true, now),
            Presentation::Unchanged
        );
        // isVisible/isOnActiveSpace can still reflect the previous frame at
        // the notification. The change itself must trigger re-presentation.
        assert_eq!(
            state.update(true, true, true, true, now),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(true, false, true, true, now),
            Presentation::Unchanged
        );
    }

    #[test]
    fn reused_and_displaced_panels_rejoin_without_reordering_every_frame() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        assert_eq!(
            state.update(true, false, false, false, now),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(true, false, false, true, now),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(true, false, false, true, now + RETRY_INTERVAL),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(true, false, true, true, now + RETRY_INTERVAL * 2),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(true, false, true, false, now + RETRY_INTERVAL * 2),
            Presentation::Rejoin
        );
    }

    #[test]
    fn space_changes_never_resurrect_finished_or_cancelled_overlays() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        assert_eq!(
            state.update(true, false, true, false, now),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(false, true, false, true, now),
            Presentation::Hide
        );
        assert_eq!(
            state.update(false, true, false, false, now),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(true, false, true, false, now),
            Presentation::Rejoin
        );
    }

    #[test]
    fn observer_uses_its_workspace_center_coalesces_changes_and_unregisters() {
        let center = NSNotificationCenter::new();
        let unrelated_center = NSNotificationCenter::new();
        let observer = SpaceObserver::new(center.clone());
        let pending = observer.changed.clone();
        let post = |center: &NSNotificationCenter| unsafe {
            center.postNotificationName_object(NSWorkspaceActiveSpaceDidChangeNotification, None);
        };
        post(&unrelated_center);
        assert!(!observer.take_change());
        post(&center);
        post(&center);
        assert!(observer.take_change());
        assert!(!observer.take_change());
        drop(observer);
        post(&center);
        assert!(!pending.load(Ordering::Acquire));
    }

    #[test]
    fn production_overlay_receives_workspace_notifications_not_default_center_notifications() {
        let visibility = OverlayVisibility::new();
        let post = |center: &NSNotificationCenter| unsafe {
            center.postNotificationName_object(NSWorkspaceActiveSpaceDidChangeNotification, None);
        };
        post(&NSNotificationCenter::defaultCenter());
        assert!(!visibility.observer.take_change());
        post(&NSWorkspace::sharedWorkspace().notificationCenter());
        assert!(visibility.observer.take_change());
        assert!(!visibility.observer.take_change());
    }
}
