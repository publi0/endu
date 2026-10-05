//! A brief, click-through notice for a result kept after the destination changed.

use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSFont, NSPanel, NSScreen, NSStatusWindowLevel,
    NSTextAlignment, NSTextField, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

const WIDTH: f64 = 340.0;
const HEIGHT: f64 = 76.0;

pub struct PasteNotice {
    window: Retained<NSPanel>,
    until: Option<Instant>,
}

impl PasteNotice {
    pub fn new() -> Result<Self, &'static str> {
        let mtm = MainThreadMarker::new().ok_or("paste notice needs the main thread")?;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT));
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            frame,
            NSWindowStyleMask::Borderless
                | NSWindowStyleMask::NonactivatingPanel
                | NSWindowStyleMask::UtilityWindow,
            NSBackingStoreType::Buffered,
            false,
        );
        let view = NSView::initWithFrame(mtm.alloc(), frame);
        view.setWantsLayer(true);
        if let Some(layer) = view.layer() {
            layer.setCornerRadius(10.0);
            layer.setBackgroundColor(Some(
                &NSColor::colorWithSRGBRed_green_blue_alpha(0.12, 0.13, 0.15, 0.97).CGColor(),
            ));
        }
        let title = NSTextField::labelWithString(&NSString::from_str("Dictation ready"), mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        title.setTextColor(Some(&NSColor::whiteColor()));
        title.setAlignment(NSTextAlignment::Center);
        title.setFrame(NSRect::new(
            NSPoint::new(12.0, 46.0),
            NSSize::new(WIDTH - 24.0, 20.0),
        ));
        view.addSubview(&title);
        let detail = NSTextField::wrappingLabelWithString(
            &NSString::from_str(
                "Auto-paste paused. Use Paste Last Dictation\nto insert the text in the app you choose.",
            ),
            mtm,
        );
        detail.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        detail.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.78, 0.80, 0.83, 1.0,
        )));
        detail.setAlignment(NSTextAlignment::Center);
        detail.setSelectable(false);
        detail.setMaximumNumberOfLines(2);
        detail.setFrame(NSRect::new(
            NSPoint::new(12.0, 10.0),
            NSSize::new(WIDTH - 24.0, 32.0),
        ));
        view.addSubview(&detail);
        window.setContentView(Some(&view));
        window.setTitle(&NSString::from_str("Hex — dictation ready"));
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setOpaque(false);
        window.setIgnoresMouseEvents(true);
        window.setHasShadow(true);
        window.setHidesOnDeactivate(false);
        window.setCanHide(false);
        window.setLevel(NSStatusWindowLevel);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::CanJoinAllApplications
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        unsafe { window.setReleasedWhenClosed(false) };
        Ok(Self {
            window,
            until: None,
        })
    }

    pub fn show(&mut self) {
        if let Some(mtm) = MainThreadMarker::new() {
            let pointer = NSEvent::mouseLocation();
            if let Some(screen) = NSScreen::screens(mtm).iter().find(|screen| {
                let frame = screen.frame();
                pointer.x >= frame.origin.x
                    && pointer.x < frame.origin.x + frame.size.width
                    && pointer.y >= frame.origin.y
                    && pointer.y < frame.origin.y + frame.size.height
            }) {
                let frame = screen.visibleFrame();
                self.window.setFrameTopLeftPoint(NSPoint::new(
                    frame.origin.x + (frame.size.width - WIDTH) / 2.0,
                    frame.origin.y + frame.size.height - 44.0,
                ));
            }
        }
        self.until = Some(Instant::now() + Duration::from_secs(5));
        self.window.orderFrontRegardless();
    }

    pub fn hide(&mut self) {
        self.until = None;
        self.window.orderOut(None);
    }

    pub fn maintain(&mut self) {
        if self.until.is_some_and(|until| Instant::now() >= until) {
            self.hide();
        }
    }
}

impl Drop for PasteNotice {
    fn drop(&mut self) {
        self.window.close();
    }
}
