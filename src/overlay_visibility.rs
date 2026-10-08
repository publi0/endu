//! Keep nonactivating overlays on the current macOS Space without activating Hex.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSBackingStoreType, NSPanel, NSWindowAnimationBehavior, NSWindowCollectionBehavior,
    NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification,
};
use objc2_core_foundation::{CFArray, CFRetained};
use objc2_foundation::{
    NSArray, NSDictionary, NSNotification, NSNotificationCenter, NSNumber, NSString,
};

const RETRY_INTERVAL: Duration = Duration::from_millis(250);

// WindowServer can report stale isOnActiveSpace/isVisible flags while a Space
// transition settles. Keep re-registering all-Spaces membership for a short
// window after every change, even when the flags already look correct.
const SETTLE_WINDOW: Duration = Duration::from_secs(1);
const RECREATE_INTERVAL: Duration = Duration::from_secs(1);

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
    Recreate,
    Hide,
}

#[derive(Default)]
struct VisibilityState {
    ordered: bool,
    last_attempt: Option<Instant>,
    settle_until: Option<Instant>,
    missing_since: Option<Instant>,
    recreate_on_show: bool,
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
            // A short capture can finish before the recovery deadline. Keep
            // that failure for the next show, never recreate an idle overlay.
            self.recreate_on_show |= self.ordered
                && self
                    .missing_since
                    .is_some_and(|since| now.duration_since(since) >= RETRY_INTERVAL);
            let hide = self.ordered || visible;
            self.ordered = false;
            self.last_attempt = None;
            self.settle_until = None;
            self.missing_since = None;
            return if hide {
                Presentation::Hide
            } else {
                Presentation::Unchanged
            };
        }
        if space_changed {
            self.settle_until = Some(now + SETTLE_WINDOW);
        }
        let first_show = !self.ordered;
        if first_show && self.recreate_on_show {
            self.recreate_on_show = false;
            self.ordered = true;
            self.last_attempt = Some(now);
            self.missing_since = Some(now);
            return Presentation::Recreate;
        }
        let missing = !on_active_space || !visible;
        let retry_due = self
            .last_attempt
            .is_none_or(|last| now.duration_since(last) >= RETRY_INTERVAL);
        if missing {
            let since = self.missing_since.get_or_insert(now);
            if !first_show && retry_due && now.duration_since(*since) >= RECREATE_INTERVAL {
                self.last_attempt = Some(now);
                self.missing_since = Some(now);
                self.settle_until = None;
                return Presentation::Recreate;
            }
        } else {
            self.missing_since = None;
        }
        let settling = self.settle_until.is_some_and(|until| now < until);
        if first_show || space_changed || (missing && retry_due) || (settling && retry_due) {
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
    last_observation: Option<Instant>,
    on_screen: Option<bool>,
}

impl OverlayVisibility {
    pub fn new() -> Self {
        Self {
            // NSWorkspace posts Space changes on its own center, not the
            // process-wide default NSNotificationCenter.
            observer: SpaceObserver::new(NSWorkspace::sharedWorkspace().notificationCenter()),
            state: VisibilityState::default(),
            last_observation: None,
            on_screen: None,
        }
    }

    /// Finish an existing fade without restoring the panel in a new Space.
    pub fn maintain_fading(&mut self, panel: &mut Retained<NSPanel>) {
        if self.observer.take_change() {
            self.update(panel, false);
        }
    }

    pub fn update(&mut self, panel: &mut Retained<NSPanel>, wanted: bool) {
        let space_changed = self.observer.take_change();
        // Idle overlays need no WindowServer queries on the 16 ms UI tick.
        if !wanted && !self.state.ordered {
            return;
        }
        let on_active_space = panel.isOnActiveSpace();
        let now = Instant::now();
        if !wanted && self.state.missing_since.is_some() && window_is_on_screen(panel) == Some(true)
        {
            // A successful short show can end before the next observation.
            self.state.missing_since = None;
        }
        if wanted
            && (space_changed
                || !self.state.ordered
                || self
                    .last_observation
                    .is_none_or(|last| now.duration_since(last) >= RETRY_INTERVAL))
        {
            let on_screen = window_is_on_screen(panel);
            if on_screen == Some(true) && self.on_screen != Some(true) {
                tracing::info!(window = panel.windowNumber(), "overlay confirmed onscreen");
            }
            self.on_screen = on_screen;
            self.last_observation = Some(now);
        }
        let action = self.state.update(
            wanted,
            space_changed,
            on_active_space,
            panel.isVisible() && self.on_screen != Some(false),
            now,
        );
        match action {
            Presentation::Unchanged => {}
            Presentation::Hide => {
                panel.orderOut(None);
                self.last_observation = None;
                self.on_screen = None;
            }
            Presentation::Recreate => {
                let old_number = panel.windowNumber();
                if let Some(replacement) = recreate_panel(panel) {
                    tracing::warn!(
                        old_window = old_number,
                        new_window = replacement.windowNumber(),
                        "recreated overlay after failed Space recovery"
                    );
                    *panel = replacement;
                    self.last_observation = None;
                    self.on_screen = None;
                } else {
                    tracing::warn!("could not recreate overlay after failed Space recovery");
                }
            }
            Presentation::Rejoin => {
                // A prewarmed/reused panel can retain an old Space association.
                // Re-register all-Spaces membership while it is offscreen,
                // then order it without becoming key or activating the app.
                // Do not combine CanJoinAllSpaces with MoveToActiveSpace.
                // Hex owns the visual entrance/exit. AppKit's inferred utility
                // animations must not delay an orderOut/orderFront repair.
                panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
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

/// Query only this window's metadata. AppKit's per-window flags can claim a
/// successful show while WindowServer still has the reused panel off-Space.
pub(crate) fn window_is_on_screen(panel: &NSPanel) -> Option<bool> {
    if panel.windowNumber() <= 0 {
        return Some(false);
    }
    let number = u32::try_from(panel.windowNumber()).ok()?;
    let ids = window_id_array(number)?;
    // CGWindow.h specifies one dictionary per supplied raw CGWindowID. The
    // Create result is +1 and its CFArray/CFDictionary contents are toll-free
    // Foundation objects; Retained below consumes that ownership exactly once.
    let raw = unsafe { CGWindowListCreateDescriptionFromArray(&ids) };
    let windows = unsafe {
        Retained::from_raw(
            raw.cast_mut()
                .cast::<NSArray<NSDictionary<NSString, AnyObject>>>(),
        )
    }?;
    let Some(window) = windows.firstObject() else {
        return Some(false);
    };
    let actual_number = window.objectForKey(&NSString::from_str("kCGWindowNumber"))?;
    if actual_number.downcast_ref::<NSNumber>()?.integerValue() != panel.windowNumber() {
        return None;
    }
    match window.objectForKey(&NSString::from_str("kCGWindowIsOnscreen")) {
        Some(value) => value
            .downcast_ref::<NSNumber>()
            .map(|value| value.boolValue()),
        // WindowServer omits this key for windows that are not onscreen.
        None => Some(false),
    }
}

fn window_id_array(number: u32) -> Option<CFRetained<CFArray>> {
    // CGWindowID arrays store scalar IDs in pointer-sized slots, not CFNumber
    // objects. CFArray.h explicitly permits NULL callbacks: no retain/release
    // may be attempted on these integer values. CFRetained owns only the array.
    let mut values = [number as usize as *const c_void];
    unsafe { CFArray::new(None, values.as_mut_ptr(), 1, std::ptr::null()) }
}

fn recreate_panel(panel: &NSPanel) -> Option<Retained<NSPanel>> {
    let marker = MainThreadMarker::new()?;
    let content = panel.contentView()?;
    let replacement = NSPanel::initWithContentRect_styleMask_backing_defer(
        marker.alloc(),
        panel.frame(),
        panel.styleMask(),
        NSBackingStoreType::Buffered,
        false,
    );
    replacement.setTitle(&panel.title());
    replacement.setBackgroundColor(Some(&panel.backgroundColor()));
    replacement.setOpaque(panel.isOpaque());
    replacement.setIgnoresMouseEvents(panel.ignoresMouseEvents());
    replacement.setHasShadow(panel.hasShadow());
    replacement.setHidesOnDeactivate(panel.hidesOnDeactivate());
    replacement.setCanHide(panel.canHide());
    replacement.setLevel(panel.level());
    replacement.setAnimationBehavior(NSWindowAnimationBehavior::None);
    replacement.setCollectionBehavior(collection_behavior());
    replacement.setFrame_display(panel.frame(), false);
    unsafe { replacement.setReleasedWhenClosed(false) };
    // Reparent the existing view and Metal layer, not the renderer. Its phase,
    // queued jobs and animation springs survive without acquiring its lock.
    panel.orderOut(None);
    panel.setContentView(None);
    replacement.setContentView(Some(&content));
    panel.close();
    replacement.orderFrontRegardless();
    Some(replacement)
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCreateDescriptionFromArray(window_ids: &CFArray) -> *const c_void;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specific_window_query_uses_owned_raw_id_slots_without_object_callbacks() {
        let id = 0x1234_5678;
        let ids = window_id_array(id).expect("CFArray allocation");
        assert_eq!(ids.count(), 1);
        assert_eq!(unsafe { ids.value_at_index(0) } as usize, id as usize);
        let retained = ids.clone();
        drop(ids);
        assert_eq!(unsafe { retained.value_at_index(0) } as usize, id as usize);
        // Dropping the last owner must release the array, never the scalar ID.
        drop(retained);
    }

    #[test]
    fn a_new_capture_during_a_previous_fade_can_recover_without_reviving_terminal_work() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        assert_eq!(
            state.update(true, false, true, false, now),
            Presentation::Rejoin
        );
        // Without a Space notification, maintain_fading intentionally leaves
        // presentation alone. A rapid new capture can arrive before that fade
        // has settled, so `ordered` remains true and this is not first_show.
        assert!(state.ordered);
        assert_eq!(
            state.update(true, false, true, false, now + RETRY_INTERVAL),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(true, false, true, false, now + RECREATE_INTERVAL),
            Presentation::Recreate
        );
        // When a Space changes during terminal fading, the caller hides once.
        assert_eq!(
            state.update(false, true, false, false, now + RECREATE_INTERVAL * 2),
            Presentation::Hide
        );
        assert_eq!(
            state.update(false, true, false, false, now + RECREATE_INTERVAL * 3),
            Presentation::Unchanged
        );
        assert!(!state.ordered);
        // Only a new live capture authorizes recovery again.
        assert_eq!(
            state.update(true, false, false, false, now + RECREATE_INTERVAL * 4),
            Presentation::Recreate
        );
        assert!(state.ordered);
    }

    #[test]
    fn persistent_missing_output_escalates_to_recreation_at_a_bounded_rate() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        assert_eq!(
            state.update(true, false, true, false, now),
            Presentation::Rejoin
        );
        // The window still exists and reports active-Space membership, but the
        // verified onscreen result stays false through all ordinary repairs.
        for attempt in 1..4 {
            assert_eq!(
                state.update(true, false, true, false, now + RETRY_INTERVAL * attempt),
                Presentation::Rejoin
            );
        }
        assert_eq!(
            state.update(true, false, true, false, now + RECREATE_INTERVAL),
            Presentation::Recreate
        );
        assert_eq!(
            state.update(
                true,
                false,
                true,
                false,
                now + RECREATE_INTERVAL + Duration::from_millis(16)
            ),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(true, false, true, false, now + RECREATE_INTERVAL * 2),
            Presentation::Recreate
        );
    }

    #[test]
    fn confirmed_presentation_clears_the_failed_recovery_deadline() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        state.update(true, false, true, false, now);
        state.update(true, false, true, true, now + RETRY_INTERVAL);
        assert_eq!(
            state.update(true, false, true, true, now + RECREATE_INTERVAL),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(true, true, true, false, now + RECREATE_INTERVAL * 2),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(
                true,
                false,
                true,
                false,
                now + RECREATE_INTERVAL * 2 + RETRY_INTERVAL
            ),
            Presentation::Rejoin
        );
    }

    #[test]
    fn cancellation_prevents_recreating_a_still_missing_panel() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        state.update(true, false, false, false, now);
        assert_eq!(
            state.update(false, true, false, false, now + RECREATE_INTERVAL),
            Presentation::Hide
        );
        assert_eq!(
            state.update(false, true, false, false, now + RECREATE_INTERVAL * 2),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(true, false, false, false, now + RECREATE_INTERVAL * 3),
            Presentation::Recreate
        );
    }

    #[test]
    fn failed_short_capture_is_recovered_only_when_the_next_capture_starts() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        state.update(true, false, true, false, now);
        assert_eq!(
            state.update(true, false, true, false, now + RETRY_INTERVAL),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(false, false, true, false, now + RETRY_INTERVAL * 2),
            Presentation::Hide
        );
        assert_eq!(
            state.update(false, true, true, false, now + RECREATE_INTERVAL * 2),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(true, false, true, false, now + RECREATE_INTERVAL * 3),
            Presentation::Recreate
        );
        assert_eq!(
            state.update(
                true,
                false,
                true,
                true,
                now + RECREATE_INTERVAL * 3 + RETRY_INTERVAL
            ),
            Presentation::Unchanged
        );
    }

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
    fn stale_flags_during_space_settle_still_trigger_bounded_rejoins() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        // Overlay shown and steady on the current Space.
        assert_eq!(
            state.update(true, false, true, false, now),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(true, false, true, true, now),
            Presentation::Unchanged
        );
        // Space changes; AppKit keeps reporting stale (correct-looking) flags
        // while WindowServer settles the move.
        assert_eq!(
            state.update(true, true, true, true, now),
            Presentation::Rejoin
        );
        // Immediately after the rejoin the flags still look fine: the settle
        // window keeps repairing at the retry interval even without "missing".
        assert_eq!(
            state.update(true, false, true, true, now + RETRY_INTERVAL / 2),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(true, false, true, true, now + RETRY_INTERVAL),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(true, false, true, true, now + RETRY_INTERVAL * 2),
            Presentation::Rejoin
        );
        // Once the settle window expires, healthy flags stop the repairs.
        assert_eq!(
            state.update(true, false, true, true, now + SETTLE_WINDOW),
            Presentation::Unchanged
        );
        assert_eq!(
            state.update(
                true,
                false,
                true,
                true,
                now + SETTLE_WINDOW + RETRY_INTERVAL
            ),
            Presentation::Unchanged
        );
    }

    #[test]
    fn hiding_the_overlay_cancels_any_pending_settle_window() {
        let now = Instant::now();
        let mut state = VisibilityState::default();
        assert_eq!(
            state.update(true, true, true, true, now),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(false, false, false, true, now),
            Presentation::Hide
        );
        // Re-shown later: first_show repairs it, but no leftover settle window
        // keeps reordering beyond that.
        assert_eq!(
            state.update(true, false, true, false, now + SETTLE_WINDOW * 2),
            Presentation::Rejoin
        );
        assert_eq!(
            state.update(
                true,
                false,
                true,
                true,
                now + SETTLE_WINDOW * 2 + RETRY_INTERVAL
            ),
            Presentation::Unchanged
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
