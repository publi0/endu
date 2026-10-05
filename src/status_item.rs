use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use block2::RcBlock;
use color_eyre::eyre::{Result, eyre};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Bool, NSObjectProtocol, Sel};
use objc2::{DefinedClass, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAffineTransformNSAppKitAdditions, NSBezierPath, NSColor,
    NSCompositingOperation, NSGraphicsContext, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSAffineTransform, NSObject, NSPoint, NSRect, NSSize, NSString,
};

use crate::dictation_indicator::DictationIndicatorEvent;

const ICON_FPS: u128 = 30;
const RECORDING_FRAMES: usize = 30;
const PROCESSING_FRAMES: usize = 60;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum IconPhase {
    #[default]
    Idle,
    Recording,
    Processing,
}

impl IconPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "Hex",
            Self::Recording => "Hex — recording",
            Self::Processing => "Hex — transcribing",
        }
    }
}

#[derive(Default)]
struct IconActivity {
    recording: bool,
    jobs: BTreeSet<u64>,
}

impl IconActivity {
    fn apply(&mut self, event: DictationIndicatorEvent) {
        match event {
            DictationIndicatorEvent::Started => self.recording = true,
            DictationIndicatorEvent::Preparing
            | DictationIndicatorEvent::Discarded
            | DictationIndicatorEvent::Cancelled
            | DictationIndicatorEvent::Failed => self.recording = false,
            DictationIndicatorEvent::Submitted { job_id } => {
                self.recording = false;
                self.jobs.insert(job_id);
            }
            DictationIndicatorEvent::JobCompleted { job_id }
            | DictationIndicatorEvent::JobCancelled { job_id }
            | DictationIndicatorEvent::JobFailed { job_id }
            | DictationIndicatorEvent::JobReadyToPaste { job_id, .. } => {
                self.jobs.remove(&job_id);
            }
            // A late Transcribing event must not resurrect a cancelled job.
            DictationIndicatorEvent::Transcribing { .. }
            | DictationIndicatorEvent::Meter { .. }
            | DictationIndicatorEvent::ReadyToPaste { .. }
            | DictationIndicatorEvent::PasteCommitted => {}
        }
    }

    fn phase(&self) -> IconPhase {
        if self.recording {
            IconPhase::Recording
        } else if !self.jobs.is_empty() {
            IconPhase::Processing
        } else {
            IconPhase::Idle
        }
    }
}

fn animation_frame(phase: IconPhase, elapsed: Duration, reduce_motion: bool) -> usize {
    if reduce_motion {
        // A static 30-degree turn distinguishes processing from the idle hexagon.
        return if phase == IconPhase::Processing {
            PROCESSING_FRAMES / 12
        } else {
            0
        };
    }
    let count = match phase {
        IconPhase::Idle => return 0,
        IconPhase::Recording => RECORDING_FRAMES,
        IconPhase::Processing => PROCESSING_FRAMES,
    };
    ((elapsed.as_millis() * ICON_FPS / 1_000) % count as u128) as usize
}

struct IconImages {
    idle: Retained<NSImage>,
    recording: Vec<Retained<NSImage>>,
    processing: Vec<Retained<NSImage>>,
}

impl IconImages {
    fn new() -> Result<Self> {
        let symbol = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("hexagon"),
            Some(&NSString::from_str("Hex")),
        )
        .ok_or_else(|| eyre!("Hex status symbol is unavailable"))?;
        let source_size = symbol.size();
        let side = source_size.width.max(source_size.height);
        let size = NSSize::new(side, side);
        let idle = Self::frame(&symbol, size, 0.0, None, IconPhase::Idle);
        let recording = (0..RECORDING_FRAMES)
            .map(|index| {
                let cycle = index as f64 / RECORDING_FRAMES as f64 * std::f64::consts::TAU;
                let opacity = 0.775 + 0.225 * cycle.cos();
                Self::frame(&symbol, size, 0.0, Some(opacity), IconPhase::Recording)
            })
            .collect();
        let processing = (0..PROCESSING_FRAMES)
            .map(|index| {
                Self::frame(
                    &symbol,
                    size,
                    -(index as f64) * 360.0 / PROCESSING_FRAMES as f64,
                    None,
                    IconPhase::Processing,
                )
            })
            .collect();
        Ok(Self {
            idle,
            recording,
            processing,
        })
    }

    fn frame(
        symbol: &Retained<NSImage>,
        size: NSSize,
        angle: f64,
        dot: Option<f64>,
        phase: IconPhase,
    ) -> Retained<NSImage> {
        let source = symbol.clone();
        // A drawing-backed template stays sharp at either backing scale. All
        // frames have equal bounds, so neighboring menu bar items never move.
        let draw = RcBlock::new(move |_: NSRect| {
            autoreleasepool(|_| {
                NSGraphicsContext::saveGraphicsState_class();
                let transform = NSAffineTransform::transform();
                transform.translateXBy_yBy(size.width / 2.0, size.height / 2.0);
                transform.rotateByDegrees(angle);
                transform.concat();
                let source_size = source.size();
                source.drawInRect_fromRect_operation_fraction(
                    NSRect::new(
                        NSPoint::new(-source_size.width / 2.0, -source_size.height / 2.0),
                        source_size,
                    ),
                    NSRect::ZERO,
                    NSCompositingOperation::SourceOver,
                    1.0,
                );
                if let Some(opacity) = dot {
                    NSColor::colorWithCalibratedWhite_alpha(0.0, opacity).setFill();
                    let diameter = size.width * 0.24;
                    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                        NSPoint::new(-diameter / 2.0, -diameter / 2.0),
                        NSSize::new(diameter, diameter),
                    ))
                    .fill();
                }
                NSGraphicsContext::restoreGraphicsState_class();
                Bool::YES
            })
        });
        let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw);
        image.setTemplate(true);
        image.setAccessibilityDescription(Some(&NSString::from_str(phase.label())));
        image
    }

    fn image(&self, phase: IconPhase, frame: usize) -> &NSImage {
        match phase {
            IconPhase::Idle => &self.idle,
            IconPhase::Recording => &self.recording[frame],
            IconPhase::Processing => &self.processing[frame],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusItemAction {
    OpenSettings,
    OpenModels,
    OpenHud,
    OpenHistory,
    OpenStatistics,
    PasteLast,
    Quit,
}

#[derive(Debug)]
struct StatusItemTargetIvars {
    actions: SyncSender<StatusItemAction>,
}

objc2::define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = StatusItemTargetIvars]
    struct StatusItemTarget;

    unsafe impl NSObjectProtocol for StatusItemTarget {}

    impl StatusItemTarget {
        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: &AnyObject) {
            let _ = self.ivars().actions.try_send(StatusItemAction::OpenSettings);
        }

        #[unsafe(method(openModels:))]
        fn open_models(&self, _sender: &AnyObject) {
            let _ = self.ivars().actions.try_send(StatusItemAction::OpenModels);
        }

        #[unsafe(method(openHud:))]
        fn open_hud(&self, _sender: &AnyObject) {
            let _ = self.ivars().actions.try_send(StatusItemAction::OpenHud);
        }

        #[unsafe(method(openHistory:))]
        fn open_history(&self, _sender: &AnyObject) {
            let _ = self.ivars().actions.try_send(StatusItemAction::OpenHistory);
        }

        #[unsafe(method(openStatistics:))]
        fn open_statistics(&self, _sender: &AnyObject) {
            let _ = self.ivars().actions.try_send(StatusItemAction::OpenStatistics);
        }

        #[unsafe(method(pasteLast:))]
        fn paste_last(&self, _sender: &AnyObject) {
            let _ = self.ivars().actions.try_send(StatusItemAction::PasteLast);
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: &AnyObject) {
            let _ = self.ivars().actions.try_send(StatusItemAction::Quit);
        }
    }
);

impl StatusItemTarget {
    fn new(actions: SyncSender<StatusItemAction>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(StatusItemTargetIvars { actions });
        unsafe { msg_send![super(this), init] }
    }
}

struct StatusItemController {
    item: Retained<NSStatusItem>,
    paste_item: Retained<NSMenuItem>,
    images: IconImages,
    activity: IconActivity,
    phase: IconPhase,
    phase_started: Instant,
    last_frame: Option<usize>,
    reduce_motion: bool,
    motion_checked_at: Instant,
    ready_to_paste: bool,
    _menu: Retained<NSMenu>,
    _target: Retained<StatusItemTarget>,
}

impl StatusItemController {
    fn update_icon(&mut self, mtm: MainThreadMarker, now: Instant) {
        let phase = self.activity.phase();
        let changed = self.phase != phase;
        if changed {
            self.phase = phase;
            self.phase_started = now;
            self.last_frame = None;
            self.update_label(mtm);
        }
        if changed
            || (phase != IconPhase::Idle
                && now.duration_since(self.motion_checked_at) >= Duration::from_secs(1))
        {
            self.reduce_motion =
                NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
            self.motion_checked_at = now;
        }
        let frame = animation_frame(
            phase,
            now.duration_since(self.phase_started),
            self.reduce_motion,
        );
        if self.last_frame != Some(frame) {
            if let Some(button) = self.item.button(mtm) {
                button.setImage(Some(self.images.image(phase, frame)));
            }
            self.last_frame = Some(frame);
        }
    }

    fn update_label(&self, mtm: MainThreadMarker) {
        if let Some(button) = self.item.button(mtm) {
            let label = if self.phase == IconPhase::Idle && self.ready_to_paste {
                "Dictation ready — choose an app, then Paste Last Dictation"
            } else {
                self.phase.label()
            };
            let label = NSString::from_str(label);
            button.setToolTip(Some(&label));
            button.setAccessibilityLabel(Some(&label));
        }
    }
}

thread_local! {
    static STATUS_ITEM: RefCell<Option<StatusItemController>> = const { RefCell::new(None) };
}

pub fn install() -> Result<Receiver<StatusItemAction>> {
    let mtm =
        MainThreadMarker::new().ok_or_else(|| eyre!("status item requires the main thread"))?;
    let (actions, receiver) = sync_channel(8);
    let images = IconImages::new()?;
    let target = StatusItemTarget::new(actions, mtm);
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Hex"));

    let paste_item = add_item(
        &menu,
        &target,
        "Paste Last Dictation",
        sel!(pasteLast:),
        mtm,
    );
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&menu, &target, "Settings", sel!(openSettings:), mtm);
    add_item(&menu, &target, "Models", sel!(openModels:), mtm);
    add_item(&menu, &target, "HUD", sel!(openHud:), mtm);
    add_item(&menu, &target, "History", sel!(openHistory:), mtm);
    add_item(&menu, &target, "Statistics", sel!(openStatistics:), mtm);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item(&menu, &target, "Quit Hex", sel!(quit:), mtm);

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(-2.0);
    let button = item
        .button(mtm)
        .ok_or_else(|| eyre!("status item button is unavailable"))?;
    button.setImage(Some(&images.idle));
    button.setToolTip(Some(&NSString::from_str("Hex")));
    button.setAccessibilityLabel(Some(&NSString::from_str("Hex")));
    item.setMenu(Some(&menu));

    let now = Instant::now();
    STATUS_ITEM.with(|status_item| {
        *status_item.borrow_mut() = Some(StatusItemController {
            item,
            paste_item,
            images,
            activity: IconActivity::default(),
            phase: IconPhase::Idle,
            phase_started: now,
            last_frame: Some(0),
            reduce_motion: NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion(),
            motion_checked_at: now,
            ready_to_paste: false,
            _menu: menu,
            _target: target,
        });
    });
    Ok(receiver)
}

fn add_item(
    menu: &NSMenu,
    target: &StatusItemTarget,
    title: &str,
    action: Sel,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::new(),
        )
    };
    unsafe { item.setTarget(Some(target)) };
    menu.addItem(&item);
    item
}

pub fn installed() -> bool {
    STATUS_ITEM.with(|controller| controller.borrow().is_some())
}

/// Main-thread observation of the existing capture/pipeline events. Audio
/// capture never waits on the status item or its animation.
pub fn handle_indicator(event: DictationIndicatorEvent) {
    if matches!(event, DictationIndicatorEvent::Meter { .. }) {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    STATUS_ITEM.with(|controller| {
        if let Some(controller) = controller.borrow_mut().as_mut() {
            controller.activity.apply(event);
            controller.update_icon(mtm, Instant::now());
        }
    });
}

pub fn animate() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    STATUS_ITEM.with(|controller| {
        if let Some(controller) = controller.borrow_mut().as_mut()
            && controller.phase != IconPhase::Idle
        {
            controller.update_icon(mtm, Instant::now());
        }
    });
}

pub fn set_ready_to_paste(ready: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    STATUS_ITEM.with(|controller| {
        if let Some(controller) = controller.borrow_mut().as_mut() {
            controller.ready_to_paste = ready;
            controller
                .paste_item
                .setTitle(&NSString::from_str(if ready {
                    "Paste Last Dictation (ready)"
                } else {
                    "Paste Last Dictation"
                }));
            controller.update_label(mtm);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_has_priority_and_last_finished_job_restores_idle() {
        let mut activity = IconActivity::default();
        assert_eq!(activity.phase(), IconPhase::Idle);
        activity.apply(DictationIndicatorEvent::Preparing);
        assert_eq!(activity.phase(), IconPhase::Idle);
        activity.apply(DictationIndicatorEvent::Started);
        assert_eq!(activity.phase(), IconPhase::Recording);
        activity.apply(DictationIndicatorEvent::Submitted { job_id: 1 });
        assert_eq!(activity.phase(), IconPhase::Processing);
        activity.apply(DictationIndicatorEvent::Started);
        activity.apply(DictationIndicatorEvent::JobCompleted { job_id: 1 });
        assert_eq!(activity.phase(), IconPhase::Recording);
        activity.apply(DictationIndicatorEvent::Submitted { job_id: 2 });
        activity.apply(DictationIndicatorEvent::Submitted { job_id: 3 });
        activity.apply(DictationIndicatorEvent::JobFailed { job_id: 2 });
        assert_eq!(activity.phase(), IconPhase::Processing);
        activity.apply(DictationIndicatorEvent::JobReadyToPaste {
            job_id: 3,
            copied_to_clipboard: false,
        });
        assert_eq!(activity.phase(), IconPhase::Idle);
    }

    #[test]
    fn cancellation_and_stale_events_cannot_leave_the_icon_spinning() {
        let mut activity = IconActivity::default();
        activity.apply(DictationIndicatorEvent::Submitted { job_id: 1 });
        activity.apply(DictationIndicatorEvent::Started);
        activity.apply(DictationIndicatorEvent::Cancelled);
        assert_eq!(activity.phase(), IconPhase::Processing);
        activity.apply(DictationIndicatorEvent::JobCancelled { job_id: 1 });
        activity.apply(DictationIndicatorEvent::Transcribing { job_id: 1 });
        assert_eq!(activity.phase(), IconPhase::Idle);
        activity.apply(DictationIndicatorEvent::Started);
        activity.apply(DictationIndicatorEvent::JobFailed { job_id: 99 });
        assert_eq!(activity.phase(), IconPhase::Recording);
        activity.apply(DictationIndicatorEvent::Failed);
        assert_eq!(activity.phase(), IconPhase::Idle);
    }

    #[test]
    fn rotation_takes_two_seconds_and_reduced_motion_stays_still() {
        assert_eq!(
            animation_frame(IconPhase::Processing, Duration::ZERO, false),
            0
        );
        assert_eq!(
            animation_frame(IconPhase::Processing, Duration::from_secs(1), false),
            30
        );
        assert_eq!(
            animation_frame(IconPhase::Processing, Duration::from_secs(2), false),
            0
        );
        assert_eq!(
            animation_frame(IconPhase::Recording, Duration::from_secs(1), false),
            0
        );
        for seconds in [0, 1, 30] {
            assert_eq!(
                animation_frame(IconPhase::Processing, Duration::from_secs(seconds), true),
                5
            );
            assert_eq!(
                animation_frame(IconPhase::Recording, Duration::from_secs(seconds), true),
                0
            );
        }
    }

    #[test]
    fn native_images_keep_fixed_bounds_and_only_recording_has_a_center_dot() {
        use objc2_app_kit::NSBitmapImageRep;

        autoreleasepool(|_| {
            let images = IconImages::new().unwrap();
            let size = images.idle.size();
            assert!(size.width > 0.0 && size.height > 0.0);
            for image in std::iter::once(&images.idle)
                .chain(images.recording.iter())
                .chain(images.processing.iter())
            {
                assert_eq!(image.size(), size);
                assert!(image.isTemplate());
            }
            let rasterize = |image: &NSImage| {
                NSBitmapImageRep::imageRepWithData(&image.TIFFRepresentation().unwrap()).unwrap()
            };
            let idle = rasterize(&images.idle);
            let recording = rasterize(&images.recording[0]);
            let processing = rasterize(&images.processing[5]);
            let center_alpha = |bitmap: &NSBitmapImageRep| {
                bitmap
                    .colorAtX_y(bitmap.pixelsWide() / 2, bitmap.pixelsHigh() / 2)
                    .unwrap()
                    .alphaComponent()
            };
            assert!(center_alpha(&idle) < 0.1);
            assert!(center_alpha(&recording) > 0.9);
            assert!(center_alpha(&processing) < 0.1);
            let outline = |bitmap: &NSBitmapImageRep| {
                (0..bitmap.pixelsHigh())
                    .flat_map(|y| {
                        (0..bitmap.pixelsWide())
                            .map(move |x| bitmap.colorAtX_y(x, y).unwrap().alphaComponent() > 0.2)
                    })
                    .collect::<Vec<_>>()
            };
            let idle_outline = outline(&idle);
            let rotated_outline = outline(&processing);
            assert!(idle_outline.iter().any(|filled| *filled));
            assert!(rotated_outline.iter().any(|filled| *filled));
            assert_ne!(idle_outline, rotated_outline);
        });
    }
}
