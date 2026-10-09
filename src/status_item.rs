use crate::i18n::t;
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
    NSCompositingOperation, NSEventModifierFlags, NSGraphicsContext, NSImage, NSLineCapStyle,
    NSLineJoinStyle, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem, NSWorkspace,
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
            Self::Idle => "Endu",
            Self::Recording => t("Endu — recording"),
            Self::Processing => t("Endu — transcribing"),
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
            | DictationIndicatorEvent::JobNoAudio { job_id }
            | DictationIndicatorEvent::JobReadyToPaste { job_id, .. } => {
                self.jobs.remove(&job_id);
            }
            // A late Transcribing event must not resurrect a cancelled job.
            DictationIndicatorEvent::Transcribing { .. }
            | DictationIndicatorEvent::JobQuiet { .. }
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
        // A still, half-written e distinguishes processing from the idle glyph.
        return if phase == IconPhase::Processing {
            PROCESSING_FRAMES * 3 / 8
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

/// The menu bar glyph is Endu's single-stroke "e": a zigzag crossbar, the
/// voice, that turns into the letter's arc. Coordinates use a 24-point design
/// square with the origin at the top left, matching the app icon.
const GLYPH_SIZE: f64 = 18.0;
const GLYPH_SCALE: f64 = GLYPH_SIZE / 24.0;
const GLYPH_STROKE: f64 = 2.1;
const WAVE_X: [f64; 8] = [4.5, 6.0, 8.0, 10.5, 13.0, 15.0, 16.5, 20.0];
const WAVE_Y: [f64; 8] = [0.0, 0.0, -3.5, 3.5, -3.0, 1.5, 0.0, 0.0];
const ARC_CENTER: f64 = 12.0;
const ARC_RADIUS: f64 = 8.0;
/// The arc leaves the crossbar's right end, passes over the top, and stops
/// 45 degrees below where it began, leaving the e open.
const ARC_END_DEGREES: f64 = 45.0;
const ARC_SWEEP_DEGREES: f64 = 360.0 - ARC_END_DEGREES;
const BADGE_CENTER: (f64, f64) = (20.5, 3.5);
const BADGE_RADIUS: f64 = 2.6;
/// The clear ring around the update badge, so it never touches the arc.
const BADGE_GAP: f64 = 1.6;
/// Writing finishes three quarters into the loop, then holds the whole e.
const WRITING_SHARE: f64 = 0.75;
const WRITING_GHOST_ALPHA: f64 = 0.28;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Glyph {
    Still,
    /// Per-vertex zigzag amplitude while listening.
    Listening([f64; 8]),
    /// The share of the stroke written so far, over a faint whole glyph.
    Writing(f64),
}

/// Integer frequencies loop seamlessly across the recording frames.
fn listening_amplitudes(cycle: f64) -> [f64; 8] {
    const FREQUENCY: [f64; 8] = [0.0, 0.0, 1.0, 2.0, 3.0, 2.0, 0.0, 0.0];
    const PHASE: [f64; 8] = [0.0, 0.0, 0.3, 1.7, 0.9, 2.4, 0.0, 0.0];
    std::array::from_fn(|index| 0.35 + 0.95 * (cycle * FREQUENCY[index] + PHASE[index]).sin().abs())
}

fn writing_progress(frame: usize) -> f64 {
    (frame as f64 / (PROCESSING_FRAMES as f64 * WRITING_SHARE)).min(1.0)
}

fn wave_points(amplitudes: [f64; 8]) -> [(f64, f64); 8] {
    std::array::from_fn(|index| {
        (
            WAVE_X[index],
            ARC_CENTER + WAVE_Y[index] * amplitudes[index],
        )
    })
}

/// The stroke's length in design points, for revealing it progressively.
fn glyph_length(amplitudes: [f64; 8]) -> f64 {
    let wave = wave_points(amplitudes)
        .windows(2)
        .map(|pair| (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1))
        .sum::<f64>();
    wave + ARC_RADIUS * ARC_SWEEP_DEGREES.to_radians()
}

fn glyph_path(amplitudes: [f64; 8]) -> Retained<NSBezierPath> {
    let path = NSBezierPath::bezierPath();
    for (index, (x, y)) in wave_points(amplitudes).into_iter().enumerate() {
        let point = NSPoint::new(x, y);
        if index == 0 {
            path.moveToPoint(point);
        } else {
            path.lineToPoint(point);
        }
    }
    // Decreasing angles in this flipped space travel up and over the top.
    path.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
        NSPoint::new(ARC_CENTER, ARC_CENTER),
        ARC_RADIUS,
        0.0,
        ARC_END_DEGREES,
        true,
    );
    path.setLineWidth(GLYPH_STROKE);
    path.setLineCapStyle(NSLineCapStyle::Round);
    path.setLineJoinStyle(NSLineJoinStyle::Round);
    path
}

fn circle(center: (f64, f64), radius: f64) -> Retained<NSBezierPath> {
    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(center.0 - radius, center.1 - radius),
        NSSize::new(radius * 2.0, radius * 2.0),
    ))
}

struct IconImages {
    idle: Retained<NSImage>,
    update: Retained<NSImage>,
    recording: Vec<Retained<NSImage>>,
    processing: Vec<Retained<NSImage>>,
}

impl IconImages {
    fn new() -> Result<Self> {
        let recording = (0..RECORDING_FRAMES)
            .map(|index| {
                let cycle = index as f64 / RECORDING_FRAMES as f64 * std::f64::consts::TAU;
                Self::frame(
                    Glyph::Listening(listening_amplitudes(cycle)),
                    IconPhase::Recording,
                    false,
                )
            })
            .collect();
        let processing = (0..PROCESSING_FRAMES)
            .map(|index| {
                Self::frame(
                    Glyph::Writing(writing_progress(index)),
                    IconPhase::Processing,
                    false,
                )
            })
            .collect();
        Ok(Self {
            idle: Self::frame(Glyph::Still, IconPhase::Idle, false),
            update: Self::frame(Glyph::Still, IconPhase::Idle, true),
            recording,
            processing,
        })
    }

    fn frame(glyph: Glyph, phase: IconPhase, update_badge: bool) -> Retained<NSImage> {
        let size = NSSize::new(GLYPH_SIZE, GLYPH_SIZE);
        // A drawing-backed template stays sharp at either backing scale. All
        // frames have equal bounds, so neighboring menu bar items never move.
        let draw = RcBlock::new(move |_: NSRect| {
            autoreleasepool(|_| {
                NSGraphicsContext::saveGraphicsState_class();
                let transform = NSAffineTransform::transform();
                transform.scaleBy(GLYPH_SCALE);
                transform.concat();
                let amplitudes = match glyph {
                    Glyph::Listening(amplitudes) => amplitudes,
                    Glyph::Still | Glyph::Writing(_) => [1.0; 8],
                };
                let path = glyph_path(amplitudes);
                let ink = NSColor::colorWithCalibratedWhite_alpha(0.0, 1.0);
                if let Glyph::Writing(progress) = glyph {
                    NSColor::colorWithCalibratedWhite_alpha(0.0, WRITING_GHOST_ALPHA).setStroke();
                    path.stroke();
                    if progress > 0.0 {
                        let length = glyph_length(amplitudes);
                        let pattern = [length * progress, length];
                        // SAFETY: the pattern outlives the call, which copies it.
                        unsafe { path.setLineDash_count_phase(pattern.as_ptr(), 2, 0.0) };
                        ink.setStroke();
                        path.stroke();
                    }
                } else {
                    ink.setStroke();
                    path.stroke();
                }
                if update_badge {
                    // A filled dot at the top-right corner mirrors macOS app
                    // icon badges without leaving the template appearance.
                    if let Some(context) = NSGraphicsContext::currentContext() {
                        context.setCompositingOperation(NSCompositingOperation::Clear);
                        circle(BADGE_CENTER, BADGE_RADIUS + BADGE_GAP).fill();
                        context.setCompositingOperation(NSCompositingOperation::SourceOver);
                    }
                    ink.setFill();
                    circle(BADGE_CENTER, BADGE_RADIUS).fill();
                }
                NSGraphicsContext::restoreGraphicsState_class();
                Bool::YES
            })
        });
        let image = NSImage::imageWithSize_flipped_drawingHandler(size, true, &draw);
        image.setTemplate(true);
        image.setAccessibilityDescription(Some(&NSString::from_str(phase.label())));
        image
    }

    fn image(&self, phase: IconPhase, frame: usize, update_available: bool) -> &NSImage {
        if phase == IconPhase::Idle && update_available {
            return &self.update;
        }
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
    OpenMicrophone,
    OpenProviders,
    OpenPostProcessing,
    OpenModels,
    OpenHud,
    OpenHistory,
    OpenStatistics,
    PasteLast,
    RestartToUpdate,
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
            send_action(&self.ivars().actions, StatusItemAction::OpenSettings);
        }

        #[unsafe(method(openProviders:))]
        fn open_providers(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::OpenProviders);
        }

        #[unsafe(method(openModels:))]
        fn open_models(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::OpenModels);
        }

        #[unsafe(method(openMicrophone:))]
        fn open_microphone(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::OpenMicrophone);
        }

        #[unsafe(method(openPostProcessing:))]
        fn open_post_processing(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::OpenPostProcessing);
        }

        #[unsafe(method(openHud:))]
        fn open_hud(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::OpenHud);
        }

        #[unsafe(method(openHistory:))]
        fn open_history(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::OpenHistory);
        }

        #[unsafe(method(openStatistics:))]
        fn open_statistics(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::OpenStatistics);
        }

        #[unsafe(method(pasteLast:))]
        fn paste_last(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::PasteLast);
        }

        #[unsafe(method(restartToUpdate:))]
        fn restart_to_update(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::RestartToUpdate);
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: &AnyObject) {
            send_action(&self.ivars().actions, StatusItemAction::Quit);
        }
    }

    unsafe impl NSMenuDelegate for StatusItemTarget {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, _menu: &NSMenu) {
            refresh_menu();
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
    status_line: Retained<NSMenuItem>,
    permission_item: Retained<NSMenuItem>,
    paste_item: Retained<NSMenuItem>,
    update_item: Retained<NSMenuItem>,
    /// Fixed rows and their English keys, retitled before the menu opens.
    localized: Vec<(Retained<NSMenuItem>, &'static str)>,
    images: IconImages,
    activity: IconActivity,
    phase: IconPhase,
    phase_started: Instant,
    last_frame: Option<usize>,
    reduce_motion: bool,
    motion_checked_at: Instant,
    ready_to_paste: bool,
    update_version: Option<String>,
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
                button.setImage(Some(self.images.image(
                    phase,
                    frame,
                    self.update_version.is_some(),
                )));
            }
            self.last_frame = Some(frame);
        }
    }

    fn update_label(&self, mtm: MainThreadMarker) {
        if let Some(button) = self.item.button(mtm) {
            let phase = if self.phase == IconPhase::Idle && self.ready_to_paste {
                t("Dictation ready — choose an app, then Paste Last Dictation")
            } else {
                self.phase.label()
            };
            let phase = if self.update_version.is_some() {
                tf!("{phase} · update available", phase = phase)
            } else {
                phase.to_string()
            };
            let label = NSString::from_str(&format!("{phase} · v{}", env!("CARGO_PKG_VERSION")));
            button.setToolTip(Some(&label));
            button.setAccessibilityLabel(Some(&label));
        }
    }
}

/// Queues a menu action and wakes the UI loop, which sleeps while idle.
fn send_action(actions: &SyncSender<StatusItemAction>, action: StatusItemAction) {
    if actions.try_send(action).is_ok() {
        crate::desktop::wake_ui();
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
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Endu"));

    // The first rows say what Endu is doing and what, if anything, blocks it.
    // Fixed rows keep their English key so they follow a language change the
    // next time the menu opens.
    let mut localized = Vec::new();
    let status_line = add_item(&menu, &target, "Endu", sel!(openSettings:), mtm);
    unsafe { status_line.setAction(None) };
    status_line.setEnabled(false);
    let permission_item = add_item(
        &menu,
        &target,
        "Grant Permissions…",
        sel!(openSettings:),
        mtm,
    );
    permission_item.setHidden(true);
    localized.push((permission_item.clone(), "Grant Permissions…"));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let paste_item = add_item(
        &menu,
        &target,
        "Paste Last Dictation",
        sel!(pasteLast:),
        mtm,
    );
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    for (title, action) in [
        ("History", sel!(openHistory:)),
        ("Statistics", sel!(openStatistics:)),
    ] {
        localized.push((add_item(&menu, &target, title, action, mtm), title));
    }
    let settings = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(t("Settings")));
    let general = add_item(&settings, &target, "General", sel!(openSettings:), mtm);
    general.setKeyEquivalent(&NSString::from_str(","));
    localized.push((general, "General"));
    for (title, action) in [
        ("Microphone", sel!(openMicrophone:)),
        ("Providers", sel!(openProviders:)),
        ("Models", sel!(openModels:)),
        ("Post-processing", sel!(openPostProcessing:)),
    ] {
        localized.push((add_item(&settings, &target, title, action, mtm), title));
    }
    add_item(&settings, &target, "HUD", sel!(openHud:), mtm);
    let settings_item = add_item(&menu, &target, "Settings", sel!(openSettings:), mtm);
    settings_item.setSubmenu(Some(&settings));
    localized.push((settings_item, "Settings"));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let update_item = add_item(
        &menu,
        &target,
        "Restart to Update",
        sel!(restartToUpdate:),
        mtm,
    );
    update_item.setHidden(true);
    let quit = add_item(&menu, &target, "Quit Endu", sel!(quit:), mtm);
    quit.setKeyEquivalent(&NSString::from_str("q"));
    localized.push((quit, "Quit Endu"));

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(-2.0);
    let button = item
        .button(mtm)
        .ok_or_else(|| eyre!("status item button is unavailable"))?;
    button.setImage(Some(&images.idle));
    button.setToolTip(Some(&NSString::from_str("Endu")));
    button.setAccessibilityLabel(Some(&NSString::from_str("Endu")));
    menu.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*target)));
    item.setMenu(Some(&menu));

    let now = Instant::now();
    STATUS_ITEM.with(|status_item| {
        *status_item.borrow_mut() = Some(StatusItemController {
            item,
            status_line,
            permission_item,
            paste_item,
            update_item,
            localized,
            images,
            activity: IconActivity::default(),
            phase: IconPhase::Idle,
            phase_started: now,
            last_frame: Some(0),
            reduce_motion: NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion(),
            motion_checked_at: now,
            ready_to_paste: false,
            update_version: None,
            _menu: menu,
            _target: target,
        });
    });
    Ok(receiver)
}

fn add_item(
    menu: &NSMenu,
    target: &StatusItemTarget,
    title: &'static str,
    action: Sel,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(t(title)),
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

/// Whether the glyph still animates and needs the UI loop's frame ticks.
pub fn is_animating() -> bool {
    STATUS_ITEM.with(|controller| {
        controller
            .borrow()
            .as_ref()
            .is_some_and(|controller| controller.phase != IconPhase::Idle)
    })
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
                    t("Paste Last Dictation (ready)")
                } else {
                    t("Paste Last Dictation")
                }));
            controller.update_label(mtm);
        }
    });
}

/// Rebuilds the dynamic rows just before the menu opens: the status line,
/// missing permissions, the saved Paste Last shortcut and a pending update.
/// Status menus do not handle key equivalents globally, so the shortcut only
/// labels its item; the listener still owns the shortcut itself. Permission
/// checks are preflight queries and never resolve provider keys.
fn refresh_menu() {
    let permissions_missing =
        !crate::onboarding::permission_warnings(crate::onboarding::status_with_api_key(true))
            .is_empty();
    let dictation = crate::app_settings::dictation_binding()
        .filter(|binding| !binding.is_empty())
        .map(|binding| binding.keycaps().concat());
    STATUS_ITEM.with(|controller| {
        if let Some(controller) = controller.borrow().as_ref() {
            controller
                .status_line
                .setTitle(&NSString::from_str(&status_headline(
                    controller.phase,
                    controller.ready_to_paste,
                    permissions_missing,
                    crate::app_settings::dictation_mode(),
                    dictation.as_deref(),
                )));
            for (item, title) in &controller.localized {
                item.setTitle(&NSString::from_str(t(title)));
            }
            controller
                .paste_item
                .setTitle(&NSString::from_str(if controller.ready_to_paste {
                    t("Paste Last Dictation (ready)")
                } else {
                    t("Paste Last Dictation")
                }));
            controller.permission_item.setHidden(!permissions_missing);
            let (key, modifiers) =
                paste_last_key_equivalent(crate::app_settings::paste_last_binding().as_ref())
                    .unwrap_or_else(|| (String::new(), NSEventModifierFlags::empty()));
            controller
                .paste_item
                .setKeyEquivalent(&NSString::from_str(&key));
            controller
                .paste_item
                .setKeyEquivalentModifierMask(modifiers);
            controller
                .update_item
                .setHidden(controller.update_version.is_none());
            if let Some(version) = &controller.update_version {
                controller.update_item.setTitle(&NSString::from_str(&tf!(
                    "Restart to Update to Endu {version}",
                    version = version
                )));
            }
        }
    });
}

fn status_headline(
    phase: IconPhase,
    ready_to_paste: bool,
    permissions_missing: bool,
    mode: crate::app_settings::DictationMode,
    dictation: Option<&str>,
) -> String {
    use crate::app_settings::DictationMode;
    match phase {
        IconPhase::Recording => return t("Recording…").into(),
        IconPhase::Processing => return t("Transcribing…").into(),
        IconPhase::Idle => {}
    }
    if permissions_missing {
        return t("Dictation needs permissions").into();
    }
    if ready_to_paste {
        return t("Dictation ready to paste").into();
    }
    match dictation {
        Some(shortcut) => tf!(
            "{gesture} {shortcut} to dictate",
            gesture = match mode {
                DictationMode::TapOrHold => t("Tap or hold"),
                DictationMode::Hold => t("Hold"),
                DictationMode::DoubleTap => t("Double-tap"),
            },
            shortcut = shortcut
        ),
        None => t("Ready to dictate").into(),
    }
}

fn paste_last_key_equivalent(
    binding: Option<&crate::app_settings::HotkeyBinding>,
) -> Option<(String, NSEventModifierFlags)> {
    let binding = binding?;
    let label = &binding.key.as_ref()?.label;
    let mut characters = label.chars();
    let character = characters.next()?;
    if characters.next().is_some() || character.is_whitespace() {
        return None;
    }
    let modifiers = binding.modifiers;
    let mut flags = NSEventModifierFlags::empty();
    for (enabled, flag) in [
        (modifiers.control.is_some(), NSEventModifierFlags::Control),
        (modifiers.option.is_some(), NSEventModifierFlags::Option),
        (modifiers.shift.is_some(), NSEventModifierFlags::Shift),
        (modifiers.command.is_some(), NSEventModifierFlags::Command),
        (modifiers.function, NSEventModifierFlags::Function),
    ] {
        if enabled {
            flags |= flag;
        }
    }
    Some((character.to_lowercase().collect(), flags))
}

/// Records a newer installed bundle so the menu bar icon badges, the tooltip
/// mentions it and the menu offers a restart until the process exits into the
/// new version.
pub fn set_pending_update(version: Option<String>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    STATUS_ITEM.with(|controller| {
        if let Some(controller) = controller.borrow_mut().as_mut()
            && controller.update_version != version
        {
            controller.update_version = version;
            controller.last_frame = None;
            controller.update_icon(mtm, Instant::now());
            controller.update_label(mtm);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_last_menu_item_shows_only_printable_shortcuts() {
        let default = crate::app_settings::HotkeyBinding::paste_last_default();
        let (key, flags) = paste_last_key_equivalent(Some(&default)).unwrap();
        assert_eq!(key, "v");
        assert_eq!(
            flags,
            NSEventModifierFlags::Option | NSEventModifierFlags::Shift
        );
        let modifier_only = crate::app_settings::HotkeyBinding::default();
        assert!(paste_last_key_equivalent(Some(&modifier_only)).is_none());
        let mut named = default.clone();
        named.key.as_mut().unwrap().label = "Space".into();
        assert!(paste_last_key_equivalent(Some(&named)).is_none());
        assert!(paste_last_key_equivalent(None).is_none());
    }

    #[test]
    fn status_line_reports_activity_before_setup_and_names_the_gesture() {
        use crate::app_settings::DictationMode;
        let headline = |phase, ready, missing, mode, shortcut| {
            status_headline(phase, ready, missing, mode, shortcut)
        };
        assert_eq!(
            headline(IconPhase::Recording, true, true, DictationMode::Hold, None),
            "Recording…"
        );
        assert_eq!(
            headline(
                IconPhase::Processing,
                false,
                true,
                DictationMode::Hold,
                None
            ),
            "Transcribing…"
        );
        assert_eq!(
            headline(IconPhase::Idle, true, true, DictationMode::Hold, Some("⌥")),
            "Dictation needs permissions"
        );
        assert_eq!(
            headline(IconPhase::Idle, true, false, DictationMode::Hold, Some("⌥")),
            "Dictation ready to paste"
        );
        assert_eq!(
            headline(
                IconPhase::Idle,
                false,
                false,
                DictationMode::TapOrHold,
                Some("⌥")
            ),
            "Tap or hold ⌥ to dictate"
        );
        assert_eq!(
            headline(
                IconPhase::Idle,
                false,
                false,
                DictationMode::DoubleTap,
                None
            ),
            "Ready to dictate"
        );
    }

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
                PROCESSING_FRAMES * 3 / 8
            );
            assert_eq!(
                animation_frame(IconPhase::Recording, Duration::from_secs(seconds), true),
                0
            );
        }
    }

    #[test]
    fn listening_loops_and_writing_reaches_the_whole_glyph() {
        let first = listening_amplitudes(0.0);
        let last = listening_amplitudes(std::f64::consts::TAU);
        for (start, end) in first.iter().zip(last) {
            assert!((start - end).abs() < 1e-9);
        }
        assert_ne!(first, listening_amplitudes(1.0));
        assert_eq!(writing_progress(0), 0.0);
        assert_eq!(writing_progress(PROCESSING_FRAMES - 1), 1.0);
        assert!(writing_progress(PROCESSING_FRAMES * 3 / 8) > 0.4);
        assert!(writing_progress(PROCESSING_FRAMES * 3 / 8) < 0.6);
        let length = glyph_length([1.0; 8]);
        assert!(length > ARC_RADIUS * 5.0 && length < 80.0);
    }

    #[test]
    fn native_glyphs_keep_fixed_bounds_and_each_phase_draws_distinctly() {
        use objc2_app_kit::NSBitmapImageRep;

        autoreleasepool(|_| {
            let images = IconImages::new().unwrap();
            let size = images.idle.size();
            assert_eq!((size.width, size.height), (GLYPH_SIZE, GLYPH_SIZE));
            for image in [&images.idle, &images.update]
                .into_iter()
                .chain(images.recording.iter())
                .chain(images.processing.iter())
            {
                assert_eq!(image.size(), size);
                assert!(image.isTemplate());
            }
            let rasterize = |image: &NSImage| {
                NSBitmapImageRep::imageRepWithData(&image.TIFFRepresentation().unwrap()).unwrap()
            };
            let alpha = |bitmap: &NSBitmapImageRep, x: f64, y: f64| {
                let column = (x / 24.0 * bitmap.pixelsWide() as f64) as isize;
                let row = (y / 24.0 * bitmap.pixelsHigh() as f64) as isize;
                bitmap.colorAtX_y(column, row).unwrap().alphaComponent()
            };
            let coverage = |bitmap: &NSBitmapImageRep| {
                (0..bitmap.pixelsHigh())
                    .flat_map(|y| (0..bitmap.pixelsWide()).map(move |x| (x, y)))
                    .map(|(x, y)| bitmap.colorAtX_y(x, y).unwrap().alphaComponent())
                    .sum::<f64>()
            };
            let idle = rasterize(&images.idle);
            // The arc's top and the zigzag's first peak are inked; the open
            // counter and the badge corner are not.
            assert!(alpha(&idle, 12.0, 4.0) > 0.5);
            assert!(alpha(&idle, 8.0, 8.6) > 0.3);
            assert!(alpha(&idle, 12.0, 17.0) < 0.1);
            assert!(alpha(&idle, BADGE_CENTER.0, BADGE_CENTER.1) < 0.1);

            let update = rasterize(&images.update);
            assert!(alpha(&update, BADGE_CENTER.0, BADGE_CENTER.1) > 0.9);

            let still = coverage(&idle);
            assert!(still > 0.0);
            let outline = |bitmap: &NSBitmapImageRep| {
                (0..bitmap.pixelsHigh())
                    .flat_map(|y| (0..bitmap.pixelsWide()).map(move |x| (x, y)))
                    .map(|(x, y)| bitmap.colorAtX_y(x, y).unwrap().alphaComponent() > 0.2)
                    .collect::<Vec<_>>()
            };
            assert_ne!(outline(&idle), outline(&rasterize(&images.recording[0])));
            let started = rasterize(&images.processing[1]);
            let written = rasterize(&images.processing[PROCESSING_FRAMES - 1]);
            assert!(coverage(&started) < coverage(&written) * 0.6);
            // Once written, the whole e is inked where the still one is; only
            // antialiased edges differ, where the faint ghost adds a little.
            let differing = outline(&written)
                .iter()
                .zip(outline(&idle))
                .filter(|(left, right)| **left != *right)
                .count();
            assert!(
                differing * 20 < outline(&idle).len(),
                "{differing} pixels differ"
            );
        });
    }
}
