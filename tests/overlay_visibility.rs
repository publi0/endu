// AppKit window operations must run on the OS main thread. This native harness
// exercises the production recovery code without microphone, settings or keys.
#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports)]
#[path = "../src/overlay_visibility.rs"]
mod overlay_visibility;

const NATIVE_TEST: &str =
    "overlay_visibility::recovery_preserves_layer_and_never_activates_or_resurrects";
const ARGUMENT_TEST: &str = "overlay_visibility::harness_arguments";

#[derive(Debug, Default)]
struct HarnessOptions {
    filter: Option<String>,
    skip: Vec<String>,
    exact: bool,
    ignored: bool,
    list: bool,
    quiet: bool,
    observe_spaces: bool,
}
impl HarnessOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--list" => options.list = true,
                "--ignored" => options.ignored = true,
                "--include-ignored" | "--nocapture" | "--show-output" => {}
                "--exact" => options.exact = true,
                "--quiet" | "-q" => options.quiet = true,
                "--observe-spaces" => options.observe_spaces = true,
                "--skip" => {
                    let filter = args
                        .next()
                        .filter(|value| !value.starts_with('-'))
                        .ok_or("--skip needs a filter")?;
                    options.skip.push(filter.clone());
                }
                "--test-threads" => validate_threads(
                    args.next()
                        .ok_or("--test-threads needs a positive number")?,
                )?,
                _ if arg.starts_with("--test-threads=") => validate_threads(&arg[15..])?,
                _ if arg.starts_with("--skip=") => options.skip.push(arg[7..].into()),
                _ if arg.starts_with('-') => return Err(format!("unsupported test option {arg}")),
                _ => {
                    if options.filter.replace(arg.clone()).is_some() {
                        return Err("expected at most one test filter".into());
                    }
                }
            }
        }
        Ok(options)
    }
    fn selected(&self, name: &str) -> bool {
        let matches = |filter: &str| {
            if self.exact {
                name == filter
            } else {
                name.contains(filter)
            }
        };
        !self.ignored
            && self.filter.as_ref().is_none_or(|filter| matches(filter))
            && !self.skip.iter().any(|filter| matches(filter))
    }
}
fn validate_threads(value: &str) -> Result<(), String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .map(|_| ())
        .ok_or_else(|| "--test-threads needs a positive number".into())
}
fn argument_regressions() {
    let parse = |args: &[&str]| {
        HarnessOptions::parse(&args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>())
    };
    for args in [vec!["--test-threads", "1"], vec!["--test-threads=1"]] {
        let options = parse(&args).unwrap();
        assert!(options.selected(NATIVE_TEST));
        assert!(options.filter.is_none());
    }
    assert!(
        !parse(&["--exact", "overlay_visibility"])
            .unwrap()
            .selected(NATIVE_TEST)
    );
    assert!(
        parse(&["--exact", NATIVE_TEST])
            .unwrap()
            .selected(NATIVE_TEST)
    );
    assert!(
        !parse(&["--skip", "overlay_visibility"])
            .unwrap()
            .selected(NATIVE_TEST)
    );
    assert!(
        !parse(&["--skip=overlay_visibility"])
            .unwrap()
            .selected(NATIVE_TEST)
    );
    assert!(
        !parse(&["--exact", "--skip", NATIVE_TEST])
            .unwrap()
            .selected(NATIVE_TEST)
    );
    assert!(
        parse(&["--skip", "unrelated", "--skip", "also_unrelated"])
            .unwrap()
            .selected(NATIVE_TEST)
    );
    assert!(!parse(&["--ignored"]).unwrap().selected(NATIVE_TEST));
    for args in [
        vec!["--test-threads"],
        vec!["--test-threads", "0"],
        vec!["--test-threads=x"],
        vec!["--skip"],
        vec!["--skip", "--exact"],
        vec!["--unsupported"],
        vec!["one", "two"],
    ] {
        assert!(
            parse(&args).is_err(),
            "invalid arguments must fail rather than silently skip"
        );
    }
}
fn report(passed: usize, quiet: bool) {
    if !quiet {
        println!(
            "test result: ok. {passed} passed; 0 failed; {} filtered out",
            2 - passed
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("overlay visibility regression: skipped (macOS only)");
}

#[cfg(target_os = "macos")]
fn main() {
    use core_graphics_types::geometry::CGSize;
    use metal::{
        Device, MTLLoadAction, MTLPixelFormat, MTLStoreAction, MetalLayer, RenderPassDescriptor,
    };
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSPanel,
        NSScreen, NSStatusWindowLevel, NSView, NSWindowAnimationBehavior, NSWindowStyleMask,
        NSWorkspace,
    };
    use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize};
    use objc2_quartz_core::CALayer;
    use std::time::{Duration, Instant};

    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let options = HarnessOptions::parse(&args).unwrap_or_else(|error| {
        eprintln!("overlay visibility test: {error}");
        std::process::exit(2);
    });
    if options.list {
        for name in [ARGUMENT_TEST, NATIVE_TEST] {
            if options.selected(name) {
                println!("{name}: test");
            }
        }
        return;
    }
    let mut passed = 0;
    if options.selected(ARGUMENT_TEST) {
        argument_regressions();
        passed += 1;
        if !options.quiet {
            println!("test {ARGUMENT_TEST} ... ok");
        }
    }
    if !options.selected(NATIVE_TEST) {
        report(passed, options.quiet);
        return;
    }

    let marker =
        MainThreadMarker::new().expect("native overlay regression must run on the main thread");
    let application = NSApplication::sharedApplication(marker);
    assert!(application.setActivationPolicy(NSApplicationActivationPolicy::Accessory));
    let foreground = || {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier())
    };
    let own_pid = i32::try_from(std::process::id()).unwrap();
    assert_ne!(foreground(), Some(own_pid));
    let screen = NSScreen::screens(marker)
        .firstObject()
        .expect("native regression needs a display");
    let visible = screen.visibleFrame();
    let frame = NSRect::new(
        NSPoint::new(
            visible.origin.x + visible.size.width / 2.0,
            visible.origin.y + 24.0,
        ),
        NSSize::new(112.0, 64.0),
    );
    let mut panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        marker.alloc(),
        frame,
        NSWindowStyleMask::Borderless
            | NSWindowStyleMask::UtilityWindow
            | NSWindowStyleMask::NonactivatingPanel
            | NSWindowStyleMask::FullSizeContentView,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setOpaque(false);
    panel.setIgnoresMouseEvents(true);
    panel.setHidesOnDeactivate(false);
    panel.setCanHide(false);
    panel.setHasShadow(false);
    panel.setLevel(NSStatusWindowLevel);
    panel.setCollectionBehavior(overlay_visibility::collection_behavior());
    panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    unsafe { panel.setReleasedWhenClosed(false) };
    let view = NSView::initWithFrame(marker.alloc(), frame);
    view.setWantsLayer(true);
    let device = Device::system_default().expect("native HUD regression needs Metal");
    let metal_layer = MetalLayer::new();
    metal_layer.set_device(&device);
    metal_layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
    metal_layer.set_opaque(false);
    metal_layer.set_drawable_size(CGSize::new(frame.size.width, frame.size.height));
    let cocoa_layer =
        unsafe { &*((&*metal_layer as *const metal::MetalLayerRef).cast::<CALayer>()) };
    view.setLayer(Some(cocoa_layer));
    let layer = view.layer().expect("layer-backed view");
    panel.setContentView(Some(&view));
    let mut visibility = overlay_visibility::OverlayVisibility::new();
    visibility.update(&mut panel, true);
    let original_number = panel.windowNumber();

    let pump =
        || NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.02));
    let queue = device.new_command_queue();
    let draw = || {
        objc2::rc::autoreleasepool(|_| {
            let drawable = metal_layer
                .next_drawable()
                .expect("the retained Metal layer must supply a drawable");
            let pass = RenderPassDescriptor::new();
            let attachment = pass.color_attachments().object_at(0).unwrap();
            attachment.set_texture(Some(drawable.texture()));
            attachment.set_load_action(MTLLoadAction::Clear);
            attachment.set_store_action(MTLStoreAction::Store);
            attachment.set_clear_color(metal::MTLClearColor::new(0.1, 0.8, 0.2, 1.0));
            let buffer = queue.new_command_buffer();
            buffer.new_render_command_encoder(pass).end_encoding();
            buffer.present_drawable(drawable);
            buffer.commit();
        })
    };
    pump();
    draw();
    let deadline = Instant::now() + Duration::from_secs(4);
    // Fault injection: simulate a reused panel that WindowServer never shows.
    // The recovery must abandon its ordinary rejoin loop and replace it.
    while panel.windowNumber() == original_number {
        assert!(
            Instant::now() < deadline,
            "persistent failed presentation was never recovered"
        );
        panel.orderOut(None);
        visibility.update(&mut panel, true);
        pump();
    }
    assert!(std::ptr::eq(&*panel.contentView().unwrap(), &*view));
    assert!(std::ptr::eq(&*view.layer().unwrap(), &*layer));
    assert_eq!(panel.frame(), frame);
    assert!(panel.ignoresMouseEvents());
    assert_eq!(panel.level(), NSStatusWindowLevel);
    assert_eq!(panel.animationBehavior(), NSWindowAnimationBehavior::None);
    // The user can switch their foreground app while this native check runs;
    // recovery must never activate the test application itself.
    assert_ne!(
        foreground(),
        Some(own_pid),
        "recovery activated its application"
    );
    pump();
    draw();
    let deadline = Instant::now() + Duration::from_secs(2);
    while overlay_visibility::window_is_on_screen(&panel) != Some(true) {
        assert!(
            Instant::now() < deadline,
            "replacement is not actually onscreen"
        );
        pump();
    }
    // Optional manual Space check: a green, click-through Metal panel uses the
    // production visibility controller. No capture, credentials or app data.
    if options.observe_spaces {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let changes = Arc::new(AtomicUsize::new(0));
        let pending = changes.clone();
        let callback = block2::RcBlock::new(
            move |_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
                pending.fetch_add(1, Ordering::Relaxed);
            },
        );
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(objc2_app_kit::NSWorkspaceActiveSpaceDidChangeNotification),
                None,
                None,
                &callback,
            )
        };
        let end = Instant::now() + Duration::from_secs(60);
        let mut last = None;
        while Instant::now() < end {
            visibility.update(&mut panel, true);
            draw();
            pump();
            let observation = (
                changes.load(Ordering::Relaxed),
                panel.windowNumber(),
                overlay_visibility::window_is_on_screen(&panel),
            );
            if last != Some(observation) {
                println!(
                    "Space observations: changes={} window={} onscreen={:?}",
                    observation.0, observation.1, observation.2
                );
                last = Some(observation);
            }
        }
        unsafe { center.removeObserver((*token).as_ref()) };
        println!(
            "Actual Space changes observed: {}",
            changes.load(Ordering::Relaxed)
        );
    }
    let recovered_number = panel.windowNumber();
    unsafe {
        NSWorkspace::sharedWorkspace()
            .notificationCenter()
            .postNotificationName_object(
                objc2_app_kit::NSWorkspaceActiveSpaceDidChangeNotification,
                None,
            );
    }
    // A terminal animation can still be drawing when a Space changes. It
    // must hide rather than revive the panel on the new desktop.
    visibility.maintain_fading(&mut panel);
    visibility.maintain_fading(&mut panel);
    pump();
    assert_eq!(panel.windowNumber(), recovered_number);
    assert_eq!(overlay_visibility::window_is_on_screen(&panel), Some(false));
    panel.close();
    passed += 1;
    if !options.quiet {
        println!("test {NATIVE_TEST} ... ok");
    }
    report(passed, options.quiet);
}
