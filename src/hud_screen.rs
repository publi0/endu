//! Shared HUD/notice screen selection. Queries geometry only, never screen pixels
//! or window titles, and never requests additional macOS permissions.

use std::cell::RefCell;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use core_graphics_types::geometry::CGRect;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSEvent, NSScreen, NSWorkspace};
use objc2_foundation::{NSNumber, NSPoint, NSRect, NSString};

use crate::hud_settings::{HudPreferences, HudScreen, MonitorId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorChoice {
    pub id: MonitorId,
    pub name: String,
}

pub struct ResolvedScreen {
    /// The screen's visible frame, its top kept below the notch.
    pub visible_frame: NSRect,
    pub backing_scale: f32,
}

#[derive(Clone, Copy)]
struct ScreenGeometry {
    id: Option<MonitorId>,
    frame: NSRect,
}

pub fn monitors() -> Vec<MonitorChoice> {
    let Some(marker) = MainThreadMarker::new() else {
        return Vec::new();
    };
    NSScreen::screens(marker)
        .iter()
        .filter_map(|screen| {
            Some(MonitorChoice {
                id: screen_id(&screen)?,
                name: screen.localizedName().to_string(),
            })
        })
        .collect()
}

pub fn resolve(preferences: HudPreferences) -> Option<ResolvedScreen> {
    let marker = MainThreadMarker::new()?;
    let screens = NSScreen::screens(marker);
    let geometry: Vec<_> = screens
        .iter()
        .map(|screen| ScreenGeometry {
            id: if preferences.screen == HudScreen::FixedMonitor {
                screen_id(&screen)
            } else {
                None
            },
            frame: screen.frame(),
        })
        .collect();
    // The first NSScreen is the primary display. Quartz's origin is its top
    // left; Cocoa's is its bottom left, including in negative-origin layouts.
    let primary = geometry.first()?.frame;
    let active = if preferences.screen == HudScreen::ActiveWindow {
        active_window()
            .map(|bounds| quartz_to_cocoa(bounds, primary.origin.y + primary.size.height))
    } else {
        None
    };
    let index = select_screen(&geometry, NSEvent::mouseLocation(), active, preferences)?;
    let screen = screens.objectAtIndex(index);
    Some(ResolvedScreen {
        visible_frame: below_safe_area(
            screen.visibleFrame(),
            screen.frame(),
            screen.safeAreaInsets().top,
        ),
        backing_scale: screen.backingScaleFactor() as f32,
    })
}

/// A fullscreen Space hides the menu bar, so `visibleFrame` reaches the top
/// of a notched screen and a top HUD or notice would sit inside the notch.
/// Clamp the visible top to the safe area; the bottom edge never moves.
fn below_safe_area(visible: NSRect, frame: NSRect, safe_area_top: f64) -> NSRect {
    let safe_area_top = if safe_area_top.is_finite() {
        safe_area_top.max(0.0)
    } else {
        0.0
    };
    let visible_top = visible.origin.y + visible.size.height;
    let top = visible_top.min(frame.origin.y + frame.size.height - safe_area_top);
    NSRect::new(
        visible.origin,
        objc2_foundation::NSSize::new(visible.size.width, (top - visible.origin.y).max(0.0)),
    )
}

fn select_screen(
    screens: &[ScreenGeometry],
    pointer: NSPoint,
    active_window: Option<NSRect>,
    preferences: HudPreferences,
) -> Option<usize> {
    let preferred = match preferences.screen {
        HudScreen::Pointer => None,
        HudScreen::FixedMonitor => preferences
            .fixed_monitor
            .and_then(|id| screens.iter().position(|screen| screen.id == Some(id))),
        HudScreen::ActiveWindow => active_window.and_then(|window| {
            screens
                .iter()
                .enumerate()
                .map(|(index, screen)| (index, overlap(window, screen.frame)))
                .filter(|(_, area)| *area > 0.0)
                .max_by(|left, right| {
                    left.1
                        .total_cmp(&right.1)
                        .then_with(|| right.0.cmp(&left.0))
                })
                .map(|(index, _)| index)
        }),
    };
    preferred
        .or_else(|| {
            screens
                .iter()
                .position(|screen| contains(screen.frame, pointer))
        })
        .or_else(|| (!screens.is_empty()).then_some(0))
}

fn contains(frame: NSRect, point: NSPoint) -> bool {
    point.x >= frame.origin.x
        && point.x < frame.origin.x + frame.size.width
        && point.y >= frame.origin.y
        && point.y < frame.origin.y + frame.size.height
}

fn overlap(left: NSRect, right: NSRect) -> f64 {
    let width = (left.origin.x + left.size.width).min(right.origin.x + right.size.width)
        - left.origin.x.max(right.origin.x);
    let height = (left.origin.y + left.size.height).min(right.origin.y + right.size.height)
        - left.origin.y.max(right.origin.y);
    width.max(0.0) * height.max(0.0)
}

fn quartz_to_cocoa(bounds: CGRect, primary_top: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(
            bounds.origin.x,
            primary_top - bounds.origin.y - bounds.size.height,
        ),
        objc2_foundation::NSSize::new(bounds.size.width, bounds.size.height),
    )
}

fn screen_id(screen: &NSScreen) -> Option<MonitorId> {
    let number = screen
        .deviceDescription()
        .objectForKey(&NSString::from_str("NSScreenNumber"))?
        .downcast::<NSNumber>()
        .ok()?;
    // The display UUID persists across connection order and display-number changes.
    let uuid =
        OwnedCf::new(unsafe { CGDisplayCreateUUIDFromDisplayID(number.unsignedIntValue()) })?;
    Some(MonitorId::from_bytes(
        unsafe { CFUUIDGetUUIDBytes(uuid.0) }.bytes,
    ))
}

struct WindowCache {
    pid: i32,
    sampled_at: Instant,
    bounds: Option<CGRect>,
}

thread_local! {
    static WINDOW_CACHE: RefCell<Option<WindowCache>> = const { RefCell::new(None) };
}

fn active_window() -> Option<CGRect> {
    let pid = NSWorkspace::sharedWorkspace()
        .frontmostApplication()?
        .processIdentifier();
    WINDOW_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(cached) = &*cache
            && cached.pid == pid
            && cached.sampled_at.elapsed() < Duration::from_millis(150)
        {
            return cached.bounds;
        }
        let bounds = front_window_bounds(pid);
        *cache = Some(WindowCache {
            pid,
            sampled_at: Instant::now(),
            bounds,
        });
        bounds
    })
}

fn front_window_bounds(pid: i32) -> Option<CGRect> {
    // On-screen windows, front to back, excluding desktop elements. This is a
    // metadata query; unlike image capture it never asks for Screen Recording.
    let windows = OwnedCf::new(unsafe { CGWindowListCopyWindowInfo(1 | 16, 0) })?;
    let count = unsafe { CFArrayGetCount(windows.0) }.clamp(0, 4096);
    for index in 0..count {
        let window = unsafe { CFArrayGetValueAtIndex(windows.0, index) };
        if window.is_null() || unsafe { CFGetTypeID(window) != CFDictionaryGetTypeID() } {
            continue;
        }
        if dictionary_integer(window, unsafe { kCGWindowOwnerPID }) != Some(i64::from(pid))
            || dictionary_integer(window, unsafe { kCGWindowLayer }) != Some(0)
        {
            continue;
        }
        let value = unsafe { CFDictionaryGetValue(window, kCGWindowBounds) };
        if value.is_null() || unsafe { CFGetTypeID(value) != CFDictionaryGetTypeID() } {
            continue;
        }
        let mut bounds = CGRect::default();
        if unsafe { CGRectMakeWithDictionaryRepresentation(value, &mut bounds) }
            && [
                bounds.origin.x,
                bounds.origin.y,
                bounds.size.width,
                bounds.size.height,
            ]
            .iter()
            .all(|value| value.is_finite())
            && bounds.size.width > 0.0
            && bounds.size.height > 0.0
        {
            return Some(bounds);
        }
    }
    None
}

fn dictionary_integer(dictionary: *const c_void, key: *const c_void) -> Option<i64> {
    let value = unsafe { CFDictionaryGetValue(dictionary, key) };
    if value.is_null() || unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut integer = 0_i64;
    unsafe { CFNumberGetValue(value, 4, (&mut integer as *mut i64).cast()) }.then_some(integer)
}

struct OwnedCf(*const c_void);

impl OwnedCf {
    fn new(value: *const c_void) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

#[repr(C)]
struct UuidBytes {
    bytes: [u8; 16],
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *const c_void;
    fn CGWindowListCopyWindowInfo(options: u32, relative_to: u32) -> *const c_void;
    fn CGRectMakeWithDictionaryRepresentation(
        dictionary: *const c_void,
        rectangle: *mut CGRect,
    ) -> bool;
    static kCGWindowOwnerPID: *const c_void;
    static kCGWindowLayer: *const c_void;
    static kCGWindowBounds: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFUUIDGetUUIDBytes(uuid: *const c_void) -> UuidBytes;
    fn CFRelease(value: *const c_void);
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dictionary: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: *const c_void, kind: isize, value: *mut c_void) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics_types::geometry::{CGPoint, CGSize};
    use objc2_foundation::NSSize;

    fn screen(id: u8, x: f64, y: f64, width: f64, height: f64) -> ScreenGeometry {
        ScreenGeometry {
            id: Some(MonitorId::from_bytes([id; 16])),
            frame: NSRect::new(NSPoint::new(x, y), NSSize::new(width, height)),
        }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    // Geometry measured on a 14" MacBook Air: frame height 956, menu bar
    // bottom 923, 32 pt notch.
    #[test]
    fn top_edge_clears_the_notch_in_a_fullscreen_space() {
        let frame = rect(0.0, 0.0, 1512.0, 956.0);
        let clamped = below_safe_area(frame, frame, 32.0);
        assert_eq!(clamped, rect(0.0, 0.0, 1512.0, 924.0));
    }

    #[test]
    fn top_edge_stays_under_the_menu_bar_on_a_notched_screen() {
        let frame = rect(0.0, 0.0, 1512.0, 956.0);
        let visible = rect(0.0, 70.0, 1512.0, 853.0);
        assert_eq!(below_safe_area(visible, frame, 32.0), visible);
    }

    #[test]
    fn screens_without_a_notch_are_unchanged() {
        let frame = rect(-1920.0, 200.0, 1920.0, 1080.0);
        let visible = rect(-1920.0, 200.0, 1920.0, 1055.0);
        assert_eq!(below_safe_area(visible, frame, 0.0), visible);
        assert_eq!(below_safe_area(frame, frame, 0.0), frame);
        assert_eq!(below_safe_area(frame, frame, f64::NAN), frame);
        assert_eq!(below_safe_area(frame, frame, -10.0), frame);
    }

    /// Both the HUD and the paste notice place their top edge from the
    /// clamped frame, so a fullscreen Space keeps them under the notch.
    #[test]
    fn top_position_uses_the_clamped_frame() {
        use crate::hud_settings::HudPosition;
        let frame = rect(0.0, 0.0, 1512.0, 956.0);
        let clamped = below_safe_area(frame, frame, 32.0);
        let top =
            HudPosition::Top.window_top(clamped.origin.y, clamped.size.height, 60.0, 40.0, 8.0);
        assert_eq!(top, 924.0 + 10.0 - 8.0);
        let bottom =
            HudPosition::Bottom.window_top(clamped.origin.y, clamped.size.height, 60.0, 40.0, 8.0);
        assert_eq!(
            bottom,
            HudPosition::Bottom.window_top(frame.origin.y, frame.size.height, 60.0, 40.0, 8.0)
        );
    }

    #[test]
    fn fixed_uuid_survives_reordering_and_disconnect_falls_back_to_pointer() {
        let left = screen(1, -1000.0, -200.0, 1000.0, 800.0);
        let right = screen(2, 0.0, 0.0, 1440.0, 900.0);
        let preferences = HudPreferences {
            screen: HudScreen::FixedMonitor,
            fixed_monitor: left.id,
            ..Default::default()
        };
        let pointer = NSPoint::new(20.0, 20.0);
        assert_eq!(
            select_screen(&[right, left], pointer, None, preferences),
            Some(1)
        );
        assert_eq!(
            select_screen(&[left, right], pointer, None, preferences),
            Some(0)
        );
        assert_eq!(select_screen(&[right], pointer, None, preferences), Some(0));
        assert_eq!(select_screen(&[], pointer, None, preferences), None);
    }

    #[test]
    fn active_window_uses_largest_overlap_and_handles_negative_origins() {
        let screens = [
            screen(1, -1000.0, 0.0, 1000.0, 900.0),
            screen(2, 0.0, 0.0, 1440.0, 900.0),
        ];
        let preferences = HudPreferences {
            screen: HudScreen::ActiveWindow,
            ..Default::default()
        };
        let pointer = NSPoint::new(20.0, 20.0);
        let window = NSRect::new(NSPoint::new(-600.0, 100.0), NSSize::new(800.0, 400.0));
        assert_eq!(
            select_screen(&screens, pointer, Some(window), preferences),
            Some(0)
        );
        assert_eq!(select_screen(&screens, pointer, None, preferences), Some(1));
        let quartz = CGRect::new(&CGPoint::new(-900.0, -400.0), &CGSize::new(800.0, 300.0));
        let cocoa = quartz_to_cocoa(quartz, 900.0);
        assert_eq!(cocoa.origin, NSPoint::new(-900.0, 1000.0));
        assert_eq!(cocoa.size, NSSize::new(800.0, 300.0));
    }
}
