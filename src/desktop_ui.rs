use gpui::{
    AnyElement, App, Corner, Div, ElementId, FocusHandle, FontWeight, IntoElement, KeyDownEvent,
    Rgba, ScrollHandle, SharedString, Stateful, Window, anchored, deferred, div, point, prelude::*,
    px, rgb, rgba,
};

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

impl NavigationIcon {
    fn sf_symbol(self) -> &'static str {
        match self {
            Self::Settings => "slider.horizontal.3",
            Self::Microphone => "mic",
            Self::Models => "sparkles",
            Self::Providers => "network",
            Self::PostProcessing => "textformat",
            Self::Hud => "capsule",
            Self::History => "clock.fill",
            Self::Statistics => "chart.bar.fill",
        }
    }
}

fn navigation_icon(icon: NavigationIcon, selected: bool) -> AnyElement {
    gpui_symbols::Icon::new(icon.sf_symbol())
        .size(px(12.0))
        .color(rgb(if selected { TEXT } else { TEXT_SOFT }))
        .weight(gpui_symbols::SymbolWeight::Semibold)
        .rendering_mode(gpui_symbols::RenderingMode::Monochrome)
        .into_any_element()
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

pub(crate) const CANVAS: u32 = 0x111111;
pub(crate) const SIDEBAR: u32 = 0x202020;
pub(crate) const SURFACE: u32 = 0x171717;
pub(crate) const SURFACE_HOVER: u32 = 0x1d1d1d;
pub(crate) const SURFACE_SELECTED: u32 = 0x3a3a3a;
const SEGMENTED_ITEM_HOVER: u32 = 0x262626;
pub(crate) const LINE: u32 = 0x292929;
pub(crate) const ACCENT: u32 = 0x3b5cf6;
pub(crate) const TEXT: u32 = 0xeeeeee;
pub(crate) const TEXT_SOFT: u32 = 0xb8b8b8;
pub(crate) const MUTED: u32 = 0x858585;
pub(crate) const FAINT: u32 = 0x626262;
pub(crate) const NEGATIVE: u32 = 0xc98f89;

pub(crate) const CONTROL_HEIGHT: f32 = 32.0;
pub(crate) const CONTROL_TEXT_SIZE: f32 = 12.0;
pub(crate) const CONTROL_RADIUS: f32 = 6.0;
pub(crate) const SETTINGS_CONTROL_WIDTH: f32 = 300.0;
pub(crate) const NUMBER_INPUT_WIDTH: f32 = 96.0;
pub(crate) const TEXT_INPUT_HEIGHT: f32 = CONTROL_HEIGHT;
pub(crate) const MULTILINE_INPUT_HEIGHT: f32 = 76.0;
pub(crate) const PANEL_RADIUS: f32 = 10.0;
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
        .hover(|item| item.bg(rgb(0x2a2a2a)).text_color(rgb(TEXT_SOFT)))
        .child(
            div()
                .size(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .border_1()
                .border_color(rgb(if selected { 0x606060 } else { 0x3a3a3a }))
                .bg(rgb(if selected { 0x494949 } else { 0x2b2b2b }))
                .child(navigation_icon(icon, selected)),
        )
}

/// The one content width every pane is bounded to. Headers and bodies share
/// it; panes must not introduce their own content widths.
pub(crate) const PANE_CONTENT_WIDTH: f32 = 940.0;

/// The one fixed list-column width every list+detail pane uses.
pub(crate) const PANE_LIST_WIDTH: f32 = 320.0;

/// The one list column of a list+detail pane: fixed to [`PANE_LIST_WIDTH`],
/// scrolling, with the pane's empty notice and load error ahead of its rows.
/// Callers decide when `empty` applies and append their rows.
pub(crate) fn pane_list(
    id: impl Into<ElementId>,
    empty: Option<&'static str>,
    error_title: &'static str,
    error: Option<String>,
) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(PANE_LIST_WIDTH))
        .h_full()
        .flex_none()
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
        .h(px(70.0))
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
                .border_b_1()
                .border_color(rgb(LINE))
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
pub(crate) fn header_button(label: impl IntoElement) -> Div {
    div()
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
    div()
        .w_full()
        .max_w(px(PANE_CONTENT_WIDTH))
        .min_h(px(0.0))
        .flex()
        .flex_col()
}

pub(crate) fn section_label(label: &'static str) -> AnyElement {
    div()
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(FAINT))
        .child(label)
        .into_any_element()
}

pub(crate) fn hotkey_keycaps(parts: Vec<String>, opacity: f32) -> AnyElement {
    div()
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
                .border_color(rgb(0x444444))
                .bg(rgb(0x2b2b2b))
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
    div()
        .w(px(24.0))
        .h(px(16.0))
        .p(px(2.0))
        .flex_none()
        .flex()
        .items_center()
        .rounded(px(4.0))
        .bg(mix_color(rgb(0x3a3a3a), rgb(ACCENT), color_position))
        .child(
            div()
                .ml(px(8.0 * position.clamp(-0.04, 1.04)))
                .size(px(12.0))
                .rounded(px(2.0))
                .bg(mix_color(rgb(0xc8c8c8), rgb(0xfafafa), color_position)),
        )
        .into_any_element()
}

pub(crate) fn settings_section_label(label: &'static str) -> AnyElement {
    div()
        .pt_5()
        .pb_2()
        .px_1()
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(FAINT))
        .child(label)
        .into_any_element()
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
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(description),
        )
        .into_any_element()
}

pub(crate) fn compact_button(label: impl IntoElement) -> Div {
    div()
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
    div()
        .w_full()
        .rounded(px(PANEL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(SURFACE))
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
        .border_color(rgb(LINE))
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
    div()
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
        .child(div().min_w(px(0.0)).flex_1().truncate().child(label))
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
        .border_color(rgb(LINE))
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
    div()
        .h(px(CONTROL_HEIGHT))
        .p(px(2.0))
        .flex_none()
        .flex()
        .items_center()
        .rounded(px(CONTROL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(CANVAS))
}

pub(crate) fn segmented_item(selected: bool) -> Div {
    div()
        .h(px(26.0))
        .px_3()
        .flex()
        .items_center()
        .rounded(px(4.0))
        .text_size(px(CONTROL_TEXT_SIZE))
        .text_color(if selected { rgb(TEXT) } else { rgb(MUTED) })
        .when(selected, |item| item.bg(rgb(SURFACE_SELECTED)))
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

pub(crate) fn settings_segmented_item(selected: bool, count: usize) -> Div {
    segmented_item(selected)
        .w(px(settings_segment_width(count)))
        .px_0()
        .justify_center()
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
            .bg(rgb(SURFACE_SELECTED)),
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
