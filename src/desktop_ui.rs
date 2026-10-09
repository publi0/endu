use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gpui::{
    AnimationExt, AnyElement, App, BoxShadow, Corner, Div, ElementId, FocusHandle, FontWeight,
    IntoElement, KeyDownEvent, PathBuilder, Rgba, ScrollHandle, SharedString, Stateful, Window,
    WindowAppearance, anchored, canvas, deferred, div, hsla, point, prelude::*, px, rgba,
};

/// Marks a control so the layout test can check that it stays horizontally
/// inside every [`layout_container`] it overlaps. A no-op outside tests.
pub(crate) fn layout_item<E: InteractiveElement>(element: E) -> E {
    layout_probe(element, LayoutProbe::Item)
}

/// Marks a clipping surface (a panel or content column). Containers are
/// themselves checked against the containers that enclose them.
pub(crate) fn layout_container<E: InteractiveElement>(element: E) -> E {
    layout_probe(element, LayoutProbe::Container)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayoutProbe {
    Item,
    Container,
}

#[cfg(test)]
fn layout_probe<E: InteractiveElement>(element: E, kind: LayoutProbe) -> E {
    match layout_probes::register(kind) {
        Some(name) => element.debug_selector(|| name),
        None => element,
    }
}

#[cfg(not(test))]
fn layout_probe<E: InteractiveElement>(element: E, _: LayoutProbe) -> E {
    element
}

/// Names the probes built on this thread while recording, so a test can look
/// their bounds up after a frame. GPUI never clears its bounds map, so every
/// name is new: a probe from an earlier frame cannot report stale bounds under
/// a reused name. Other tests never record, so their frames do not grow it.
#[cfg(test)]
pub(crate) mod layout_probes {
    use super::LayoutProbe;
    use std::cell::{Cell, RefCell};

    thread_local! {
        static RECORDING: Cell<bool> = const { Cell::new(false) };
        static NEXT: Cell<usize> = const { Cell::new(0) };
        static PROBES: RefCell<Vec<(String, LayoutProbe)>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn register(kind: LayoutProbe) -> Option<String> {
        RECORDING.get().then(|| {
            let name = format!("layout-probe-{}", NEXT.replace(NEXT.get() + 1));
            PROBES.with_borrow_mut(|probes| probes.push((name.clone(), kind)));
            name
        })
    }

    pub(crate) fn record() {
        RECORDING.set(true);
        PROBES.take();
    }

    pub(crate) fn take() -> Vec<(String, LayoutProbe)> {
        PROBES.take()
    }
}

/// Keyboard state shared by the small choice menus. Domain edits remain with
/// their owner, so a failed save can leave the menu and its focus intact.
pub(crate) struct PickerState {
    pub trigger: FocusHandle,
    pub menu: FocusHandle,
    pub scroll: ScrollHandle,
    pub highlight: usize,
}

impl PickerState {
    pub fn new(cx: &App) -> Self {
        Self {
            trigger: cx.focus_handle().tab_stop(true),
            menu: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            highlight: 0,
        }
    }

    pub fn open(&mut self, selected: usize, count: usize, window: &mut Window) {
        self.highlight = selected.min(count.saturating_sub(1));
        self.scroll.scroll_to_item(self.highlight);
        self.menu.focus(window);
    }

    pub fn navigate(&mut self, key: &str, count: usize) -> bool {
        let last = count.saturating_sub(1);
        self.highlight = match key {
            "up" => self.highlight.saturating_sub(1).min(last),
            "down" => self.highlight.saturating_add(1).min(last),
            "home" => 0,
            "end" => last,
            _ => return false,
        };
        self.scroll.scroll_to_item(self.highlight);
        true
    }

    pub fn close(&self, event: &KeyDownEvent, window: &mut Window) {
        self.trigger.focus(window);
        if event.keystroke.key == "tab" {
            if event.keystroke.modifiers.shift {
                window.focus_prev();
            } else {
                window.focus_next();
            }
        }
    }
}

// Picker triggers handle activation on key-down. Their on_click handlers must
// ignore ClickEvent::Keyboard: GPUI emits it on key-up, which could otherwise
// reopen a menu after selection has restored focus to its trigger.
pub(crate) fn picker_open_key(event: &KeyDownEvent) -> bool {
    let modifiers = event.keystroke.modifiers;
    !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && matches!(
            event.keystroke.key.as_str(),
            "enter" | "space" | "down" | "up"
        )
}

/// Anchor at the trigger's right edge; GPUI flips above or fits the window
/// when there is not enough room below. The zero-sized wrapper supplies the
/// actual window-space anchor even for buttons with intrinsic widths.
pub(crate) fn picker_popup(menu: impl IntoElement) -> AnyElement {
    deferred(
        div()
            .absolute()
            .top_0()
            .right_0()
            .w(px(0.0))
            .h(px(0.0))
            .child(
                anchored()
                    .anchor(Corner::TopRight)
                    .offset(point(px(0.0), px(CONTROL_HEIGHT + 4.0)))
                    .child(menu),
            ),
    )
    .into_any_element()
}

#[derive(Clone, Copy)]
pub(crate) enum NavigationIcon {
    Settings,
    Microphone,
    Models,
    Providers,
    PostProcessing,
    Hud,
    History,
    Statistics,
}

/// Counts pane changes, so the newly selected icon moves once each time.
static NAVIGATIONS: AtomicU64 = AtomicU64::new(0);

/// Records a pane change; the selected icon then plays its motion.
pub(crate) fn note_navigation() {
    NAVIGATIONS.fetch_add(1, Ordering::Relaxed);
}

fn navigation_icon(icon: NavigationIcon, selected: bool) -> AnyElement {
    let color = if selected { ACCENT } else { MUTED };
    let glyph = move |progress: f32| {
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                crate::nav_icons::paint(icon, progress, bounds, rgb(color), window);
            },
        )
        .size(px(15.0))
        .flex_none()
    };
    let navigations = NAVIGATIONS.load(Ordering::Relaxed);
    if !selected || navigations == 0 {
        return glyph(0.0).into_any_element();
    }
    animate_once(
        glyph(0.0),
        ElementId::NamedInteger("navigation-icon".into(), navigations),
        crate::nav_icons::NAV_ICON_MOTION_MS,
        move |_, progress| glyph(progress),
    )
}

fn disclosure_chevron() -> AnyElement {
    gpui_symbols::Icon::new("chevron.down")
        .size(px(10.0))
        .color(rgb(MUTED))
        .weight(gpui_symbols::SymbolWeight::Medium)
        .rendering_mode(gpui_symbols::RenderingMode::Monochrome)
        .into_any_element()
}

pub(crate) const SIDEBAR_WIDTH: f32 = 220.0;

/// A semantic interface color, resolved against the window's appearance when
/// it is painted. Dark is Graphite and light is Tabatinga; both share the
/// urucum accent, so components never choose colors per appearance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThemeColor {
    Canvas,
    Sidebar,
    Surface,
    SurfaceHover,
    SurfaceSelected,
    SegmentedItemHover,
    Line,
    /// Separators inside a panel, quieter than its edge.
    Divider,
    /// The recessed track behind segmented controls.
    Track,
    Accent,
    /// The "endu" wordmark: near white on Graphite, jenipapo on Tabatinga.
    Wordmark,
    /// The accent washed over the control field, for selected segments.
    AccentSoft,
    OnAccent,
    Text,
    TextSoft,
    Muted,
    Faint,
    Negative,
    Positive,
    PositiveBadge,
    PositiveBadgeHover,
    PositiveBadgeText,
    PositiveDot,
    SavedCapture,
    ListeningCapture,
    NavigationHover,
    Keycap,
    KeycapBorder,
    ToggleTrack,
    ToggleKnob,
    ToggleKnobOn,
}

impl ThemeColor {
    /// `(graphite, tabatinga)`.
    const fn values(self) -> (u32, u32) {
        match self {
            Self::Canvas => (0x111111, 0xf6f1e6),
            Self::Sidebar => (0x202020, 0xece4d3),
            Self::Surface => (0x171717, 0xfbf8f1),
            Self::SurfaceHover => (0x1d1d1d, 0xf1eadc),
            Self::SurfaceSelected => (0x3a3a3a, 0xe3d9c4),
            Self::SegmentedItemHover => (0x262626, 0xebe3d2),
            Self::Line => (0x262626, 0xe4dccb),
            Self::Divider => (0x232323, 0xefe9dc),
            Self::Track => (0x0f0f0f, 0xf1ebdf),
            Self::Accent => (0xe0582f, 0xc8401e),
            Self::Wordmark => (0xeeeeee, 0x1b2a3a),
            Self::AccentSoft => (0x321c16, 0xf1ddd0),
            Self::OnAccent => (0xffffff, 0xffffff),
            Self::Text => (0xeeeeee, 0x1b2a3a),
            Self::TextSoft => (0xb8b8b8, 0x3e4a57),
            Self::Muted => (0x858585, 0x766e5e),
            Self::Faint => (0x626262, 0x9a917c),
            Self::Negative => (0xc98f89, 0xa8433a),
            Self::Positive => (0x8fbf98, 0x3f7d4e),
            Self::PositiveBadge => (0x17231a, 0xe3eedc),
            Self::PositiveBadgeHover => (0x1d2c21, 0xd8e7cf),
            Self::PositiveBadgeText => (0x91bd99, 0x2f6a3d),
            Self::PositiveDot => (0x69d89f, 0x3f9a5c),
            Self::SavedCapture => (0x1b2420, 0xe3eedc),
            Self::ListeningCapture => (0x251c1b, 0xf6e1d8),
            Self::NavigationHover => (0x2a2a2a, 0xe4dbc8),
            Self::Keycap => (0x2b2b2b, 0xffffff),
            Self::KeycapBorder => (0x444444, 0xcfc4ad),
            Self::ToggleTrack => (0x4a4a4a, 0xcfc5b0),
            Self::ToggleKnob => (0xc8c8c8, 0xffffff),
            Self::ToggleKnobOn => (0xfafafa, 0xffffff),
        }
    }
}

static DARK_APPEARANCE: AtomicBool = AtomicBool::new(true);

/// Follows the window's effective appearance, which already reflects the
/// Appearance setting through the application override. Returns whether the
/// palette changed, so the caller can repaint.
pub(crate) fn sync_appearance(appearance: WindowAppearance) -> bool {
    set_dark_appearance(matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    ))
}

/// Returns whether the palette changed.
pub(crate) fn set_dark_appearance(dark: bool) -> bool {
    DARK_APPEARANCE.swap(dark, Ordering::Relaxed) != dark
}

pub(crate) fn dark_appearance() -> bool {
    DARK_APPEARANCE.load(Ordering::Relaxed)
}

/// A color [`rgb`] accepts: a [`ThemeColor`] or a literal `0xRRGGBB`.
pub(crate) trait ColorValue {
    fn resolve(self) -> u32;
}

impl ColorValue for u32 {
    fn resolve(self) -> u32 {
        self
    }
}

impl ColorValue for ThemeColor {
    fn resolve(self) -> u32 {
        let (dark, light) = self.values();
        if dark_appearance() { dark } else { light }
    }
}

/// [`gpui::rgb`] for theme colors. Import this rather than GPUI's.
pub(crate) fn rgb(color: impl ColorValue) -> Rgba {
    gpui::rgb(color.resolve())
}

pub(crate) const CANVAS: ThemeColor = ThemeColor::Canvas;
pub(crate) const SIDEBAR: ThemeColor = ThemeColor::Sidebar;
pub(crate) const SURFACE: ThemeColor = ThemeColor::Surface;
pub(crate) const SURFACE_HOVER: ThemeColor = ThemeColor::SurfaceHover;
pub(crate) const SURFACE_SELECTED: ThemeColor = ThemeColor::SurfaceSelected;
const SEGMENTED_ITEM_HOVER: ThemeColor = ThemeColor::SegmentedItemHover;
pub(crate) const LINE: ThemeColor = ThemeColor::Line;
pub(crate) const DIVIDER: ThemeColor = ThemeColor::Divider;
pub(crate) const ACCENT: ThemeColor = ThemeColor::Accent;
pub(crate) const ACCENT_SOFT: ThemeColor = ThemeColor::AccentSoft;
pub(crate) const ON_ACCENT: ThemeColor = ThemeColor::OnAccent;
pub(crate) const TEXT: ThemeColor = ThemeColor::Text;
pub(crate) const TEXT_SOFT: ThemeColor = ThemeColor::TextSoft;
pub(crate) const MUTED: ThemeColor = ThemeColor::Muted;
pub(crate) const FAINT: ThemeColor = ThemeColor::Faint;
pub(crate) const NEGATIVE: ThemeColor = ThemeColor::Negative;
pub(crate) const POSITIVE: ThemeColor = ThemeColor::Positive;

pub(crate) const CONTROL_HEIGHT: f32 = 32.0;
pub(crate) const CONTROL_TEXT_SIZE: f32 = 12.0;
pub(crate) const CONTROL_RADIUS: f32 = 6.0;
pub(crate) const SETTINGS_CONTROL_WIDTH: f32 = 300.0;
pub(crate) const NUMBER_INPUT_WIDTH: f32 = 96.0;
pub(crate) const TEXT_INPUT_HEIGHT: f32 = CONTROL_HEIGHT;
pub(crate) const MULTILINE_INPUT_HEIGHT: f32 = 76.0;
pub(crate) const PANEL_RADIUS: f32 = 12.0;
pub(crate) const COMPACT_PANEL_HEADER_HEIGHT: f32 = 38.0;

pub(crate) fn window_frame() -> Div {
    div()
        .size_full()
        .relative()
        .overflow_hidden()
        .flex()
        .bg(rgb(CANVAS))
        .text_color(rgb(TEXT))
}

const WORDMARK_WAVE_MS: u64 = 1_400;

struct WordmarkWave {
    plays: u64,
    started: Option<std::time::Instant>,
    queued: bool,
}

/// The wordmark's waves so far. A wave always finishes; requests while one
/// plays queue a single replay, however many clicks arrive.
static WORDMARK: std::sync::Mutex<WordmarkWave> = std::sync::Mutex::new(WordmarkWave {
    plays: 0,
    started: None,
    queued: false,
});

/// Asks for a wave. Returns how long a queued one waits for the current wave.
fn request_wordmark_wave() -> Option<std::time::Duration> {
    let wave = std::time::Duration::from_millis(WORDMARK_WAVE_MS);
    let mut state = WORDMARK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = std::time::Instant::now();
    match state.started.map(|started| now.duration_since(started)) {
        Some(elapsed) if elapsed < wave => {
            state.queued = true;
            Some(wave - elapsed)
        }
        _ => {
            state.plays += 1;
            state.started = Some(now);
            state.queued = false;
            None
        }
    }
}

/// The wave to show now, starting a queued one once the current has ended.
fn current_wordmark_wave() -> u64 {
    let wave = std::time::Duration::from_millis(WORDMARK_WAVE_MS);
    let mut state = WORDMARK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.queued
        && state
            .started
            .is_none_or(|started| started.elapsed() >= wave)
    {
        state.plays += 1;
        state.started = Some(std::time::Instant::now());
        state.queued = false;
    }
    state.plays
}
const WORDMARK_HEIGHT: f32 = 22.0;
/// The wordmark's design box, matching `resources/brand/wordmark-*.svg`.
const WORDMARK_BOX: (f32, f32) = (200.0, 72.0);
const WORDMARK_STROKE: f32 = 8.0;

/// The monoline "endu" wordmark at the top of the sidebar. The app icon
/// already lives in the Dock and menu bar, so the window shows only the name,
/// aligned with the navigation icons below it. A click is a small thank-you:
/// the e's crossbar becomes the urucum voice wave of the icon and the letters
/// hop in turn before everything settles.
pub(crate) fn sidebar_brand() -> AnyElement {
    let plays = current_wordmark_wave();
    let wordmark = |progress: f32| {
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                paint_wordmark(bounds, progress, window);
            },
        )
        .h(px(WORDMARK_HEIGHT))
        .w(px(WORDMARK_HEIGHT * WORDMARK_BOX.0 / WORDMARK_BOX.1))
        .flex_none()
    };
    let mark = if plays == 0 {
        wordmark(1.0).into_any_element()
    } else {
        animate_once(
            wordmark(0.0),
            ElementId::NamedInteger("wordmark-wave".into(), plays),
            WORDMARK_WAVE_MS,
            move |_, progress| wordmark(progress),
        )
    };
    div()
        .id("sidebar-wordmark")
        .h(px(WORDMARK_HEIGHT + 10.0))
        .px_3()
        .mb_3()
        .flex()
        .items_center()
        .child(mark)
        .on_click(|_, window, cx| {
            if let Some(wait) = request_wordmark_wave() {
                // Redraw when the current wave ends, so the queued one starts.
                cx.spawn(async move |cx| {
                    gpui::Timer::after(wait).await;
                    let _ = cx.update(|cx| cx.refresh_windows());
                })
                .detach();
            }
            window.refresh();
        })
        .into_any_element()
}

/// Plays the wordmark's wave, as a click on it does.
pub(crate) fn play_wordmark() {
    request_wordmark_wave();
}

/// A check that draws itself, like the HUD's when dictation lands. `id`
/// `None` shows it already drawn.
pub(crate) fn drawn_check(id: Option<ElementId>, color: ThemeColor, size: f32) -> AnyElement {
    const POINTS: [(f32, f32); 3] = [(0.22, 0.54), (0.42, 0.74), (0.8, 0.3)];
    let mark = move |progress: f32| {
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                let at = |(x, y): (f32, f32)| {
                    point(
                        bounds.origin.x + px(x * size),
                        bounds.origin.y + px(y * size),
                    )
                };
                let lengths: Vec<f32> = POINTS
                    .windows(2)
                    .map(|pair| (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1))
                    .collect();
                let mut remaining = lengths.iter().sum::<f32>() * progress.clamp(0.0, 1.0);
                if remaining <= 0.0 {
                    return;
                }
                let mut path = PathBuilder::stroke(px(size * 0.12));
                path.move_to(at(POINTS[0]));
                for (index, length) in lengths.iter().enumerate() {
                    let (from, to) = (POINTS[index], POINTS[index + 1]);
                    let share = (remaining / length).min(1.0);
                    path.line_to(at((
                        from.0 + (to.0 - from.0) * share,
                        from.1 + (to.1 - from.1) * share,
                    )));
                    remaining -= length;
                    if remaining <= 0.0 {
                        break;
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, rgb(color));
                }
            },
        )
        .size(px(size))
        .flex_none()
    };
    match id {
        Some(id) => animate_once(mark(0.0), id, 420, move |_, progress| {
            mark(ease_out(progress))
        }),
        None => mark(1.0).into_any_element(),
    }
}

/// The crossbar's voice wave, from the menu bar glyph: x in the e's 40-unit
/// crossbar and the zigzag's resting offsets.
const WORDMARK_WAVE: [(f32, f32); 8] = [
    (0.0, 0.0),
    (3.9, 0.0),
    (9.0, -1.0),
    (15.5, 1.0),
    (21.9, -0.86),
    (27.1, 0.43),
    (31.0, 0.0),
    (40.0, 0.0),
];

fn paint_wordmark(bounds: gpui::Bounds<gpui::Pixels>, progress: f32, window: &mut Window) {
    use std::f32::consts::{PI, TAU};
    let scale = f32::from(bounds.size.height) / WORDMARK_BOX.1;
    let origin = bounds.origin;
    // Design units, with the SVG's -6 viewBox offset folded in.
    let at = |x: f32, y: f32, lift: f32| {
        point(
            origin.x + px(x * scale),
            origin.y + px((y + 6.0 - lift) * scale),
        )
    };
    let ink = rgb(ThemeColor::Wordmark);
    let swell = (progress * PI).sin().max(0.0);
    // Each letter hops once, a little after the one before it.
    let lift = |letter: usize| {
        let local = (progress - letter as f32 * 0.1) / 0.45;
        if (0.0..1.0).contains(&local) {
            (local * PI).sin() * 7.0
        } else {
            0.0
        }
    };
    let stroke = |build: &dyn Fn(&mut PathBuilder), color: Rgba, window: &mut Window| {
        let mut path = PathBuilder::stroke(px(WORDMARK_STROKE * scale));
        build(&mut path);
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    };
    // Lyon's default caps and joins are square; round every end and corner.
    let dot = |center: gpui::Point<gpui::Pixels>, color: Rgba, window: &mut Window| {
        let radius = px(WORDMARK_STROKE * scale / 2.0);
        let mut path = PathBuilder::fill();
        path.move_to(point(center.x + radius, center.y));
        path.arc_to(
            point(radius, radius),
            px(0.0),
            false,
            true,
            point(center.x - radius, center.y),
        );
        path.arc_to(
            point(radius, radius),
            px(0.0),
            false,
            true,
            point(center.x + radius, center.y),
        );
        path.close();
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    };
    let arc = |radius: f32| point(px(radius * scale), px(radius * scale));

    // e: the crossbar becomes the wave, then the arc around it.
    let e = lift(0);
    let wave: Vec<_> = WORDMARK_WAVE
        .iter()
        .enumerate()
        .map(|(index, (x, rest))| {
            let shimmer = (progress * TAU * 2.0 * (1.0 + index as f32 * 0.5) + index as f32).sin();
            at(
                5.0 + x,
                40.0 + rest * 8.75 * swell * (0.6 + 0.4 * shimmer),
                e,
            )
        })
        .collect();
    let crossbar = mix_color(ink, rgb(ACCENT), swell);
    stroke(
        &|path| {
            path.move_to(at(45.0, 40.0, e));
            path.arc_to(arc(20.0), px(0.0), true, false, at(39.1, 54.1, e));
        },
        ink,
        window,
    );
    stroke(
        &|path| {
            path.move_to(wave[0]);
            for point in &wave[1..] {
                path.line_to(*point);
            }
        },
        crossbar,
        window,
    );
    for point in &wave {
        dot(*point, crossbar, window);
    }
    dot(at(39.1, 54.1, e), ink, window);

    // n
    let n = lift(1);
    stroke(
        &|path| {
            path.move_to(at(57.0, 20.0, n));
            path.line_to(at(57.0, 60.0, n));
            path.move_to(at(57.0, 38.0, n));
            path.arc_to(arc(18.0), px(0.0), false, true, at(93.0, 38.0, n));
            path.line_to(at(93.0, 60.0, n));
        },
        ink,
        window,
    );
    for (x, y) in [(57.0, 20.0), (57.0, 60.0), (93.0, 60.0)] {
        dot(at(x, y, n), ink, window);
    }

    // d
    let d = lift(2);
    stroke(
        &|path| {
            path.move_to(at(145.0, 40.0, d));
            path.arc_to(arc(20.0), px(0.0), false, true, at(105.0, 40.0, d));
            path.arc_to(arc(20.0), px(0.0), false, true, at(145.0, 40.0, d));
            path.move_to(at(145.0, 0.0, d));
            path.line_to(at(145.0, 60.0, d));
        },
        ink,
        window,
    );
    for (x, y) in [(145.0, 0.0), (145.0, 60.0)] {
        dot(at(x, y, d), ink, window);
    }

    // u
    let u = lift(3);
    stroke(
        &|path| {
            path.move_to(at(157.0, 20.0, u));
            path.line_to(at(157.0, 42.0, u));
            path.arc_to(arc(18.0), px(0.0), false, false, at(193.0, 42.0, u));
            path.move_to(at(193.0, 20.0, u));
            path.line_to(at(193.0, 60.0, u));
        },
        ink,
        window,
    );
    for (x, y) in [(157.0, 20.0), (193.0, 20.0), (193.0, 60.0)] {
        dot(at(x, y, u), ink, window);
    }
}

pub(crate) fn sidebar_frame() -> Div {
    div()
        .h_full()
        .flex_none()
        .bg(rgb(SIDEBAR))
        .border_r_1()
        .border_color(rgb(LINE))
}

pub(crate) fn navigation_item(icon: NavigationIcon, selected: bool) -> Div {
    div()
        .h(px(38.0))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .rounded(px(6.0))
        .text_size(px(13.0))
        .text_color(if selected { rgb(TEXT) } else { rgb(MUTED) })
        .when(selected, |item| item.bg(rgb(SURFACE_SELECTED)))
        .hover(|item| {
            item.bg(rgb(ThemeColor::NavigationHover))
                .text_color(rgb(TEXT_SOFT))
        })
        .child(
            div()
                .size(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(navigation_icon(icon, selected)),
        )
}

/// The one content width every pane is bounded to. Headers and bodies share
/// it; panes must not introduce their own content widths.
pub(crate) const PANE_CONTENT_WIDTH: f32 = 940.0;

/// The one list of a list pane: the full content width, scrolling, with the
/// pane's empty notice and load error ahead of its rows. An entry opens on
/// its own across the pane rather than in a column beside the list.
/// Callers decide when `empty` applies and append their rows.
pub(crate) fn pane_list(
    id: impl Into<ElementId>,
    empty: Option<&'static str>,
    error_title: &'static str,
    error: Option<String>,
) -> Stateful<Div> {
    layout_container(div().id(id))
        .w_full()
        .h_full()
        .overflow_y_scroll()
        .when_some(empty, |list, message| list.child(empty_message(message)))
        .when_some(error, |list, error| {
            list.child(error_message(error_title, error))
        })
}

pub(crate) fn pane_header(title: &'static str) -> AnyElement {
    pane_header_with_action(title, None)
}

/// The one pane header: title left, optional action right, bounded to
/// [`PANE_CONTENT_WIDTH`] like the body below it.
pub(crate) fn pane_header_with_action(
    title: &'static str,
    action: Option<AnyElement>,
) -> AnyElement {
    div()
        .h(px(60.0))
        .px_8()
        .flex_none()
        .flex()
        .justify_center()
        .child(
            div()
                .h_full()
                .w_full()
                .max_w(px(PANE_CONTENT_WIDTH))
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(20.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .when_some(action, |header, action| header.child(action)),
        )
        .into_any_element()
}

/// The one pane body: fills the space under the header and centers its
/// children. Put the pane's content column inside [`pane_content`].
pub(crate) fn pane_body() -> Div {
    div()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .flex()
        .justify_center()
}

/// The one pane-header action button, using the shared control height. Every
/// clickable header action renders this.
/// A borderless symbol button for an action repeated on every list row; its
/// label appears on hover.
pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    symbol: &'static str,
    label: &'static str,
    color: ThemeColor,
) -> Stateful<Div> {
    icon_button_with(
        id,
        label,
        symbol_icon(symbol, color, 13.0).into_any_element(),
    )
}

/// An SF Symbol in one theme color.
pub(crate) fn symbol_icon(
    symbol: &'static str,
    color: ThemeColor,
    size: f32,
) -> gpui_symbols::Icon {
    gpui_symbols::Icon::new(symbol)
        .size(px(size))
        .color(rgb(color))
        .weight(gpui_symbols::SymbolWeight::Medium)
        .rendering_mode(gpui_symbols::RenderingMode::Monochrome)
}

/// A turning three-quarter ring for work in progress. With reduced motion it
/// rests in place.
pub(crate) fn spinner(id: impl Into<ElementId>, color: ThemeColor, size: f32) -> AnyElement {
    let ring = move |turn: f32| {
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                use std::f32::consts::TAU;
                let stroke = size * 0.13;
                let radius = size / 2.0 - stroke;
                let center = point(
                    bounds.origin.x + px(size / 2.0),
                    bounds.origin.y + px(size / 2.0),
                );
                let at = |angle: f32| {
                    point(
                        center.x + px(radius * angle.cos()),
                        center.y + px(radius * angle.sin()),
                    )
                };
                let start = turn * TAU;
                let mut path = PathBuilder::stroke(px(stroke));
                path.move_to(at(start));
                path.arc_to(
                    point(px(radius), px(radius)),
                    px(0.0),
                    true,
                    true,
                    at(start + TAU * 0.75),
                );
                if let Ok(path) = path.build() {
                    window.paint_path(path, rgb(color));
                }
            },
        )
        .size(px(size))
        .flex_none()
    };
    animate_loop(ring(0.0), id, 900, 0.0, move |_, turn| ring(turn))
}

/// An [`icon_button`] around any glyph, such as an animated one.
pub(crate) fn icon_button_with(
    id: impl Into<ElementId>,
    label: &'static str,
    glyph: AnyElement,
) -> Stateful<Div> {
    layout_item(div())
        .id(id)
        .flex_none()
        .size(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(CONTROL_RADIUS))
        .hover(|button| button.bg(rgb(SURFACE_HOVER)))
        .child(glyph)
        .tooltip(move |_, cx| cx.new(|_| Tooltip(label)).into())
}

struct Tooltip(&'static str);

impl gpui::Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(5.0))
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(SURFACE))
            .text_size(px(11.0))
            .text_color(rgb(TEXT_SOFT))
            .child(self.0)
    }
}

pub(crate) fn header_button(label: impl IntoElement) -> Div {
    layout_item(div())
        .h(px(CONTROL_HEIGHT))
        .px_3()
        .flex()
        .items_center()
        .rounded(px(CONTROL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(SURFACE))
        .text_size(px(12.0))
        .text_color(rgb(TEXT_SOFT))
        .hover(|button| button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT)))
        .child(label)
}

/// The one pane content column, bounded to [`PANE_CONTENT_WIDTH`].
pub(crate) fn pane_content() -> Div {
    layout_container(div())
        .w_full()
        .max_w(px(PANE_CONTENT_WIDTH))
        .min_h(px(0.0))
        .flex()
        .flex_col()
}

pub(crate) fn section_label(label: &'static str) -> AnyElement {
    div()
        .text_size(px(12.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .child(label)
        .into_any_element()
}

pub(crate) fn hotkey_keycaps(parts: Vec<String>, opacity: f32) -> AnyElement {
    layout_item(div())
        .flex()
        .items_center()
        .gap(px(3.0))
        .opacity(opacity)
        .children(parts.into_iter().map(|part| {
            let (side, label) = split_sided_keycap(&part);
            div()
                .min_w(px(34.0))
                .h(px(24.0))
                .px_2()
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .border_1()
                .border_color(rgb(ThemeColor::KeycapBorder))
                .bg(rgb(ThemeColor::Keycap))
                .text_size(px(12.0))
                .font_weight(FontWeight::NORMAL)
                .text_color(rgb(TEXT))
                .when_some(side, |keycap, side| {
                    keycap.child(
                        div()
                            .absolute()
                            .top(px(2.0))
                            .left(px(4.0))
                            .text_size(px(7.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(FAINT))
                            .child(side),
                    )
                })
                .child(label)
        }))
        .into_any_element()
}

fn split_sided_keycap(part: &str) -> (Option<&'static str>, String) {
    match part {
        "L⌃" => (Some("L"), "⌃".into()),
        "R⌃" => (Some("R"), "⌃".into()),
        "L⌥" => (Some("L"), "⌥".into()),
        "R⌥" => (Some("R"), "⌥".into()),
        "L⇧" => (Some("L"), "⇧".into()),
        "R⇧" => (Some("R"), "⇧".into()),
        "L⌘" => (Some("L"), "⌘".into()),
        "R⌘" => (Some("R"), "⌘".into()),
        _ => (None, part.to_string()),
    }
}

pub(crate) fn toggle(position: f32) -> AnyElement {
    let color_position = position.clamp(0.0, 1.0);
    // The off track stays visibly lighter than the panel so a switch never
    // reads as a lone knob or a checkbox.
    layout_item(div())
        .w(px(28.0))
        .h(px(16.0))
        .p(px(2.0))
        .flex_none()
        .flex()
        .items_center()
        .rounded_full()
        .bg(mix_color(
            rgb(ThemeColor::ToggleTrack),
            rgb(ACCENT),
            color_position,
        ))
        .child(
            div()
                .ml(px(12.0 * position.clamp(-0.04, 1.04)))
                .size(px(12.0))
                .rounded_full()
                .bg(mix_color(
                    rgb(ThemeColor::ToggleKnob),
                    rgb(ThemeColor::ToggleKnobOn),
                    color_position,
                )),
        )
        .into_any_element()
}

pub(crate) fn settings_section_label(label: &'static str) -> AnyElement {
    div()
        .pt_6()
        .pb_2()
        .px_1()
        .text_size(px(12.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .child(label)
        .into_any_element()
}

/// The chevron and label of a collapsible section. The caller owns focus,
/// clicks and keys; the header only highlights its own label, never the row.
pub(crate) fn disclosure_header(label: &'static str, detail: &'static str, open: bool) -> Div {
    div()
        .mt_5()
        .mb_2()
        .px_1()
        .py_1()
        .flex()
        .items_center()
        .gap_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgba(0x00000000))
        .cursor_pointer()
        .text_size(px(12.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .hover(|header| header.text_color(rgb(TEXT_SOFT)))
        .focus(|header| header.border_color(rgb(ACCENT)))
        .child(
            gpui_symbols::Icon::new(if open {
                "chevron.down"
            } else {
                "chevron.right"
            })
            .size(px(9.0))
            .color(rgb(MUTED))
            .weight(gpui_symbols::SymbolWeight::Semibold)
            .rendering_mode(gpui_symbols::RenderingMode::Monochrome),
        )
        .child(label)
        .child(
            div()
                .font_weight(FontWeight::NORMAL)
                .text_color(rgb(FAINT))
                .child(detail),
        )
}

pub(crate) fn settings_copy(
    title: &'static str,
    description: impl Into<SharedString>,
) -> AnyElement {
    let description = description.into();
    div()
        .debug_selector(|| "settings-copy".into())
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .when(!description.is_empty(), |copy| {
            copy.child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child(description),
            )
        })
        .into_any_element()
}

pub(crate) fn compact_button(label: impl IntoElement) -> Div {
    layout_item(div())
        .h(px(CONTROL_HEIGHT))
        .px_3()
        .flex()
        .items_center()
        .rounded(px(CONTROL_RADIUS))
        .text_size(px(CONTROL_TEXT_SIZE))
        .text_color(rgb(TEXT_SOFT))
        .hover(|button| button.bg(rgb(SURFACE_HOVER)))
        .child(label)
}

pub(crate) fn compact_panel() -> Div {
    layout_container(div())
        .w_full()
        .rounded(px(PANEL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(SURFACE))
        // A soft lift separates the panel from Tabatinga without a hard edge;
        // on Graphite the surface step already does.
        .when(!dark_appearance(), |panel| {
            panel.shadow(vec![BoxShadow {
                color: hsla(0.11, 0.3, 0.25, 0.06),
                offset: point(px(0.0), px(1.0)),
                blur_radius: px(3.0),
                spread_radius: px(0.0),
            }])
        })
        .overflow_hidden()
}

pub(crate) fn compact_panel_header(title: impl IntoElement, action: Option<AnyElement>) -> Div {
    div()
        .h(px(COMPACT_PANEL_HEADER_HEIGHT))
        .px_3()
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(rgb(DIVIDER))
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(title),
        )
        .when_some(action, |header, action| header.child(action))
}

pub(crate) fn disclosure_button(label: impl IntoElement) -> Div {
    layout_item(div())
        .w(px(SETTINGS_CONTROL_WIDTH))
        .h(px(CONTROL_HEIGHT))
        .px_3()
        .flex_none()
        .flex()
        .items_center()
        .gap_2()
        .rounded(px(CONTROL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(CANVAS))
        .text_size(px(CONTROL_TEXT_SIZE))
        .text_color(rgb(TEXT_SOFT))
        .hover(|button| button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT)))
        // GPUI's truncation measures slightly narrower than the shaped text
        // paints, so a clipped label could lose its last glyph without an
        // ellipsis. Truncate a little early and let any overhang use the gap.
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .pr(px(6.0))
                .overflow_hidden()
                .child(
                    div()
                        .w(px(SETTINGS_CONTROL_WIDTH - 24.0 - 2.0 - 8.0 - 10.0 - 6.0))
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label),
                ),
        )
        .child(
            div()
                .size(px(10.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(disclosure_chevron()),
        )
}

pub(crate) fn settings_row(
    title: &'static str,
    description: impl Into<SharedString>,
    control: impl IntoElement,
) -> Div {
    div()
        .w_full()
        .min_h(px(72.0))
        .px_4()
        .py_3()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .border_b_1()
        .border_color(rgb(DIVIDER))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(settings_copy(title, description)),
        )
        .child(control)
}

pub(crate) fn settings_panel() -> Div {
    compact_panel().relative()
}

pub(crate) fn segmented_control() -> Div {
    layout_item(div())
        .h(px(CONTROL_HEIGHT))
        .p(px(2.0))
        .flex_none()
        .flex()
        .items_center()
        .rounded(px(CONTROL_RADIUS))
        // The border matches the track so segment widths keep their metrics
        // while the control reads as one recessed surface, not a box.
        .border_1()
        .border_color(rgb(ThemeColor::Track))
        .bg(rgb(ThemeColor::Track))
}

pub(crate) fn segmented_item(selected: bool) -> Div {
    layout_item(div())
        .h(px(26.0))
        .px_3()
        .flex()
        .items_center()
        .rounded(px(4.0))
        .text_size(px(CONTROL_TEXT_SIZE))
        .text_color(if selected { rgb(ACCENT) } else { rgb(MUTED) })
        .when(selected, |item| item.bg(rgb(ACCENT_SOFT)))
        .when(!selected, |item| {
            item.hover(|item| {
                item.bg(rgb(SEGMENTED_ITEM_HOVER))
                    .text_color(rgb(TEXT_SOFT))
            })
        })
}

pub(crate) fn settings_segment_width(count: usize) -> f32 {
    (SETTINGS_CONTROL_WIDTH - 6.0) / count.max(1) as f32
}

pub(crate) fn settings_segmented_control() -> Div {
    segmented_control().w(px(SETTINGS_CONTROL_WIDTH))
}

/// An item of [`settings_choice`]: transparent, so the sliding indicator
/// beneath the selected one shows through.
pub(crate) fn settings_segmented_item(selected: bool, count: usize) -> Div {
    segmented_item(selected)
        .w(px(settings_segment_width(count)))
        .px_0()
        .justify_center()
        .bg(rgba(0x00000000))
}

/// A [`segmented_control`] whose selection indicator slides between items.
/// `position` is a fractional item index and `widths` lists every item's
/// width in order; append the items with [`sliding_segmented_item`].
pub(crate) fn sliding_segmented_control(position: f32, widths: &[f32]) -> Div {
    let (left, width) = segmented_geometry(position, widths);
    segmented_control().relative().child(
        div()
            .absolute()
            .left(px(left))
            .top(px(2.0))
            .w(px(width))
            .h(px(26.0))
            .rounded(px(4.0))
            .bg(rgb(ACCENT_SOFT)),
    )
}

/// A fixed-width, centered item that stays transparent so the sliding
/// indicator beneath it shows through.
pub(crate) fn sliding_segmented_item(width: f32, selected: bool) -> Div {
    segmented_item(selected)
        .w(px(width))
        .px(px(0.0))
        .justify_center()
        .bg(rgba(0x00000000))
}

fn segmented_geometry(position: f32, widths: &[f32]) -> (f32, f32) {
    let last = widths.len().saturating_sub(1);
    let position = position.clamp(0.0, last as f32);
    let lower = (position.floor() as usize).min(last);
    let upper = (lower + 1).min(last);
    let progress = position - lower as f32;
    let left = |index: usize| 2.0 + widths[..index].iter().sum::<f32>();
    (
        left(lower) + (left(upper) - left(lower)) * progress,
        widths[lower] + (widths[upper] - widths[lower]) * progress,
    )
}

/// Whether macOS asks apps to reduce motion. Every animation then shows its
/// end state at once; tests always do, so their renders stay deterministic.
pub(crate) fn reduce_motion() -> bool {
    use std::sync::atomic::AtomicU64;
    static CHECKED_AT_MS: AtomicU64 = AtomicU64::new(0);
    static REDUCE: AtomicBool = AtomicBool::new(false);
    if cfg!(test) {
        return true;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    // The preference rarely changes; read it at most once a second.
    if now.saturating_sub(CHECKED_AT_MS.load(Ordering::Relaxed)) >= 1_000 {
        CHECKED_AT_MS.store(now, Ordering::Relaxed);
        REDUCE.store(
            objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion(),
            Ordering::Relaxed,
        );
    }
    REDUCE.load(Ordering::Relaxed)
}

/// Eases out: quick to start, gentle to settle.
pub(crate) fn ease_out(progress: f32) -> f32 {
    1.0 - (1.0 - progress.clamp(0.0, 1.0)).powi(3)
}

/// Plays `animator` once over `duration_ms` the first time an element with
/// `id` appears; a new `id` plays it again. With reduced motion the element
/// shows its end state.
pub(crate) fn animate_once<E: IntoElement + 'static>(
    element: E,
    id: impl Into<ElementId>,
    duration_ms: u64,
    animator: impl Fn(E, f32) -> E + 'static,
) -> AnyElement {
    if reduce_motion() {
        return animator(element, 1.0).into_any_element();
    }
    element
        .with_animation(
            id,
            gpui::Animation::new(std::time::Duration::from_millis(duration_ms)),
            animator,
        )
        .into_any_element()
}

/// Plays `animator` in a loop, for work in progress. With reduced motion the
/// element rests at `still`.
pub(crate) fn animate_loop<E: IntoElement + 'static>(
    element: E,
    id: impl Into<ElementId>,
    duration_ms: u64,
    still: f32,
    animator: impl Fn(E, f32) -> E + 'static,
) -> AnyElement {
    if reduce_motion() {
        return animator(element, still).into_any_element();
    }
    element
        .with_animation(
            id,
            gpui::Animation::new(std::time::Duration::from_millis(duration_ms)).repeat(),
            animator,
        )
        .into_any_element()
}

struct SegmentPill {
    selected: usize,
    from: usize,
    generation: u64,
}

thread_local! {
    /// The last selection of every segmented control, so a new selection can
    /// glide from where the indicator was.
    static SEGMENT_PILLS: std::cell::RefCell<std::collections::HashMap<SharedString, SegmentPill>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The sliding selection indicator of a segmented control named `id`. Put it
/// first inside a `relative()` control whose items have no background.
pub(crate) fn segment_pill(
    id: impl Into<SharedString>,
    selected: usize,
    widths: &[f32],
) -> AnyElement {
    let id = id.into();
    let (from, generation) = SEGMENT_PILLS.with(|pills| {
        let mut pills = pills.borrow_mut();
        let pill = pills.entry(id.clone()).or_insert(SegmentPill {
            selected,
            from: selected,
            generation: 0,
        });
        if pill.selected != selected {
            pill.from = pill.selected;
            pill.selected = selected;
            pill.generation += 1;
        }
        (pill.from, pill.generation)
    });
    let widths = widths.to_vec();
    let indicator = move |position: f32| {
        let (left, width) = segmented_geometry(position, &widths);
        div()
            .absolute()
            .left(px(left))
            .top(px(2.0))
            .w(px(width))
            .h(px(26.0))
            .rounded(px(4.0))
            .bg(rgb(ACCENT_SOFT))
    };
    let (from, to) = (from as f32, selected as f32);
    if generation == 0 {
        return indicator(to).into_any_element();
    }
    animate_once(
        indicator(from),
        ElementId::NamedInteger(format!("segment-pill-{id}").into(), generation),
        220,
        move |_, progress| indicator(from + (to - from) * ease_out(progress)),
    )
}

/// A settings segmented control named `id` whose selection slides; items come
/// from [`settings_segmented_item`]. `None` leaves no item selected.
pub(crate) fn settings_choice(
    id: impl Into<SharedString>,
    selected: Option<usize>,
    count: usize,
) -> Div {
    let widths = vec![settings_segment_width(count); count];
    settings_segmented_control()
        .relative()
        .when_some(selected, |control, selected| {
            control.child(segment_pill(id, selected, &widths))
        })
}

pub(crate) fn mix_color(from: Rgba, to: Rgba, position: f32) -> Rgba {
    let position = position.clamp(0.0, 1.0);
    Rgba {
        r: from.r + (to.r - from.r) * position,
        g: from.g + (to.g - from.g) * position,
        b: from.b + (to.b - from.b) * position,
        a: from.a + (to.a - from.a) * position,
    }
}

pub(crate) fn empty_message(message: &'static str) -> AnyElement {
    div()
        .p_6()
        .text_size(px(12.0))
        .text_color(rgb(FAINT))
        .child(message)
        .into_any_element()
}

pub(crate) fn error_message(message: &'static str, error: String) -> AnyElement {
    div()
        .p_6()
        .flex()
        .flex_col()
        .gap_2()
        .text_size(px(12.0))
        .text_color(rgb(NEGATIVE))
        .child(message)
        .child(
            div()
                .text_size(px(11.0))
                .line_height(px(17.0))
                .text_color(rgb(MUTED))
                .child(error),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{segmented_geometry, split_sided_keycap};

    #[test]
    fn side_badges_only_apply_to_sided_modifiers() {
        assert_eq!(split_sided_keycap("L⌥"), (Some("L"), "⌥".into()));
        assert_eq!(split_sided_keycap("R⌘"), (Some("R"), "⌘".into()));
        assert_eq!(split_sided_keycap("Return"), (None, "Return".into()));
    }

    #[test]
    fn segmented_indicator_interpolates_between_uneven_items() {
        let widths = [50.0, 90.0, 80.0];
        assert_eq!(segmented_geometry(0.0, &widths), (2.0, 50.0));
        assert_eq!(segmented_geometry(1.0, &widths), (52.0, 90.0));
        assert_eq!(segmented_geometry(2.0, &widths), (142.0, 80.0));
        assert_eq!(segmented_geometry(0.5, &widths), (27.0, 70.0));
        assert_eq!(segmented_geometry(-1.0, &widths), (2.0, 50.0));
        assert_eq!(segmented_geometry(5.0, &widths), (142.0, 80.0));
        assert_eq!(segmented_geometry(2.5, &[34.0; 5]), (87.0, 34.0));
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext, Window, size};

    struct StandardControls(gpui::Entity<crate::text_input::TextInput>);

    impl Render for StandardControls {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap_2()
                .child(
                    disclosure_button("Active window").debug_selector(|| "standard-picker".into()),
                )
                .child(
                    settings_segmented_control()
                        .debug_selector(|| "standard-segments".into())
                        .children((0..3).map(|index| {
                            settings_segmented_item(index == 0, 3)
                                .debug_selector(move || format!("standard-segment-{index}"))
                                .child(["Small", "Normal", "Large"][index])
                        })),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(NUMBER_INPUT_WIDTH))
                        .debug_selector(|| "standard-number".into())
                        .child(self.0.clone()),
                )
        }
    }

    #[gpui::test]
    fn settings_controls_share_height_width_and_equal_segments(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, cx| {
            StandardControls(cx.new(|cx| crate::text_input::TextInput::new(cx, "", "24")))
        });
        cx.run_until_parked();
        let picker = cx.debug_bounds("standard-picker").unwrap();
        let segments = cx.debug_bounds("standard-segments").unwrap();
        let number = cx.debug_bounds("standard-number").unwrap();
        assert_eq!(picker.size.width, px(SETTINGS_CONTROL_WIDTH));
        assert_eq!(picker.size.width, segments.size.width);
        assert_eq!(picker.size.height, px(CONTROL_HEIGHT));
        assert_eq!(picker.size.height, segments.size.height);
        assert_eq!(picker.size.height, number.size.height);
        assert_eq!(number.size.width, px(NUMBER_INPUT_WIDTH));
        let widths: Vec<_> = [
            "standard-segment-0",
            "standard-segment-1",
            "standard-segment-2",
        ]
        .into_iter()
        .map(|selector| cx.debug_bounds(selector).unwrap().size.width)
        .collect();
        assert!(widths.iter().all(|width| *width == widths[0]));
    }

    struct SettingsRow;

    impl Render for SettingsRow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w_full().child(
                settings_row(
                    "Microphone mode",
                    "Default: keeps the microphone open for the fastest start. A short pre-roll helps catch the beginning of speech. Failed recordings are kept locally for recovery.",
                    div()
                        .debug_selector(|| "settings-control".into())
                        .w(px(SETTINGS_CONTROL_WIDTH))
                        .h(px(CONTROL_HEIGHT))
                        .flex_none(),
                )
                .debug_selector(|| "settings-row".into()),
            )
        }
    }

    struct EdgePicker;

    impl Render for EdgePicker {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().relative().child(
                div()
                    .absolute()
                    .right_0()
                    .bottom_0()
                    .w(px(100.0))
                    .h(px(CONTROL_HEIGHT))
                    .relative()
                    .child(picker_popup(
                        div()
                            .w(px(220.0))
                            .h(px(300.0))
                            .debug_selector(|| "edge-popup".into()),
                    )),
            )
        }
    }

    #[gpui::test]
    fn picker_near_the_window_edge_fits_inside_the_viewport(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| EdgePicker);
        for (width, height) in [(1040.0, 720.0), (800.0, 600.0)] {
            cx.simulate_resize(size(px(width), px(height)));
            cx.run_until_parked();
            let popup = cx.debug_bounds("edge-popup").unwrap();
            assert!(
                popup.left() >= px(0.0) && popup.right() <= px(width),
                "{popup:?}"
            );
            assert!(
                popup.top() >= px(0.0) && popup.bottom() <= px(height),
                "{popup:?}"
            );
        }
    }

    #[gpui::test]
    fn settings_row_wraps_copy_without_displacing_fixed_control(cx: &mut TestAppContext) {
        // Real GPUI/Taffy layout with deterministic test-platform text metrics.
        let (_, cx) = cx.add_window_view(|_, _| SettingsRow);
        let mut heights = Vec::new();
        for width in [940.0, 756.0, 576.0] {
            cx.simulate_resize(size(px(width), px(400.0)));
            cx.run_until_parked();
            let row = cx.debug_bounds("settings-row").unwrap();
            let copy = cx.debug_bounds("settings-copy").unwrap();
            let control = cx.debug_bounds("settings-control").unwrap();
            assert_eq!(row.size.width, px(width));
            assert_eq!(control.size.width, px(SETTINGS_CONTROL_WIDTH));
            assert_eq!(control.size.height, px(CONTROL_HEIGHT));
            assert!(
                control.right() <= row.right(),
                "{width}: {control:?} outside {row:?}"
            );
            assert!(copy.left() >= row.left());
            assert!(
                copy.right() + px(16.0) <= control.left(),
                "{width}: gap between {copy:?} and {control:?} is below 16px"
            );
            assert!(copy.top() >= row.top() && copy.bottom() <= row.bottom());
            heights.push((row.size.height, copy.size.height));
        }
        assert!(
            heights[2].0 > heights[0].0,
            "narrow row must grow: {heights:?}"
        );
        assert!(
            heights[2].1 > heights[0].1,
            "narrow copy must wrap: {heights:?}"
        );
    }
}
