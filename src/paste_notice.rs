//! A brief, click-through notice for a result kept after the destination changed.

use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFont, NSPanel, NSStatusWindowLevel, NSTextAlignment,
    NSTextField, NSView, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

const WIDTH: f64 = 340.0;
const HEIGHT: f64 = 76.0;

fn notice_copy(copied_to_clipboard: bool) -> (&'static str, &'static str) {
    if copied_to_clipboard {
        (
            "Dictation copied",
            "Auto-paste did not complete. Press ⌘V\nto paste the text in the app you choose.",
        )
    } else {
        (
            "Dictation ready",
            "Auto-paste paused. Use Paste Last Dictation\nto insert the text in the app you choose.",
        )
    }
}

pub struct PasteNotice {
    window: Retained<NSPanel>,
    until: Option<Instant>,
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    visibility: crate::overlay_visibility::OverlayVisibility,
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
        window.setCollectionBehavior(crate::overlay_visibility::collection_behavior());
        unsafe { window.setReleasedWhenClosed(false) };
        Ok(Self {
            window,
            until: None,
            title,
            detail,
            visibility: crate::overlay_visibility::OverlayVisibility::new(),
        })
    }

    pub fn show(&mut self, copied_to_clipboard: bool) {
        let (title, detail) = notice_copy(copied_to_clipboard);
        self.title.setStringValue(&NSString::from_str(title));
        self.detail.setStringValue(&NSString::from_str(detail));
        self.position_on_selected_screen();
        self.until = Some(Instant::now() + Duration::from_secs(5));
        self.visibility.update(&mut self.window, true);
    }

    fn position_on_selected_screen(&self) {
        let preferences = crate::hud_settings::current();
        let Some(screen) = crate::hud_screen::resolve(preferences) else {
            return;
        };
        let frame = screen.visible_frame;
        // Leave 16 points between the visible HUD and the notice at either edge.
        // At Normal size and the default edge distance this is the original 44 pt.
        let gap = f64::from(preferences.edge_distance)
            + f64::from(16.0 * preferences.size.scale())
            + 16.0;
        let top_left = NSPoint::new(
            frame.origin.x + (frame.size.width - WIDTH) / 2.0,
            preferences
                .position
                .window_top(frame.origin.y, frame.size.height, HEIGHT, HEIGHT, gap),
        );
        let current = self.window.frame();
        if (current.origin.x - top_left.x).abs() > 0.5
            || (current.origin.y + current.size.height - top_left.y).abs() > 0.5
        {
            self.window.setFrameTopLeftPoint(top_left);
        }
    }

    pub fn hide(&mut self) {
        self.until = None;
        self.visibility.update(&mut self.window, false);
    }

    pub fn maintain(&mut self) {
        if self.until.is_some_and(|until| Instant::now() >= until) {
            self.hide();
        } else if self.until.is_some() {
            self.position_on_selected_screen();
            self.visibility.update(&mut self.window, true);
        }
    }
}

impl Drop for PasteNotice {
    fn drop(&mut self) {
        self.window.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_never_claims_the_clipboard_was_written_for_deferred_output() {
        let copied = notice_copy(true);
        let deferred = notice_copy(false);
        assert_eq!(copied.0, "Dictation copied");
        assert!(copied.1.contains("⌘V"));
        assert_eq!(deferred.0, "Dictation ready");
        assert!(deferred.1.contains("Paste Last Dictation"));
        assert!(!deferred.1.contains("⌘V"));
    }
}
