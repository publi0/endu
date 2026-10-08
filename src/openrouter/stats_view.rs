//! Aggregate Statistics: global dictation totals and scoped, measured attempts.
use super::stats::{
    self, AttemptSample, Dashboard, DictationTelemetry, ErrorKind, Failure, Period, RequestMode,
    RequestTotals, Sample, Totals,
};
use super::stats_dashboard::{self, Group, Mode, Sort};
use crate::desktop_ui::{
    ACCENT, FAINT, LINE, MUTED, NEGATIVE, PANEL_RADIUS, POSITIVE, PickerState,
    SETTINGS_CONTROL_WIDTH, SURFACE, SURFACE_HOVER, SURFACE_SELECTED, TEXT, TEXT_SOFT,
    compact_panel, compact_panel_header, disclosure_button, empty_message, error_message,
    header_button, pane_body, pane_content, pane_header_with_action, picker_open_key, picker_popup,
    segmented_control, segmented_item,
};
use crate::providers::{ModelRef, Provider};
use gpui::{
    AnyElement, Context, Div, FocusHandle, FontWeight, IntoElement, KeyDownEvent, MouseDownEvent,
    Render, Window, div, prelude::*, px, rgb,
};

const CHART_HEIGHT: f32 = 120.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Chart {
    Words,
    Dictations,
    Wait,
    Failures,
}
impl Chart {
    const ALL: [Self; 4] = [Self::Words, Self::Dictations, Self::Wait, Self::Failures];
    fn title(self) -> &'static str {
        match self {
            Self::Words => "Words per day",
            Self::Dictations => "Successful dictations per day",
            Self::Wait => "Average wait per day",
            Self::Failures => "Failed dictations per day",
        }
    }
    fn value(self, totals: &Totals) -> Option<u64> {
        match self {
            Self::Words => Some(totals.words),
            Self::Dictations => Some(totals.dictations),
            Self::Wait => totals.average_latency_ms(),
            Self::Failures => Some(totals.failed_dictations),
        }
    }
    fn format(self, value: u64) -> String {
        if self == Self::Wait {
            format_latency(value)
        } else {
            format_count(value)
        }
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Segment {
    Period,
    Chart,
    Group,
    Mode,
}
impl Segment {
    fn index(self) -> usize {
        match self {
            Self::Period => 0,
            Self::Chart => 1,
            Self::Group => 2,
            Self::Mode => 3,
        }
    }
    fn labels(self) -> Vec<&'static str> {
        match self {
            Self::Period => Period::ALL.iter().map(|period| period.label()).collect(),
            Self::Chart => vec!["Words", "Dictations", "Wait", "Failures"],
            Self::Group => vec!["Providers", "Models"],
            Self::Mode => vec!["All", "Live", "Recorded"],
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Menu {
    Provider,
    Sort,
}
struct Controls {
    segments: [Vec<FocusHandle>; 4],
    provider: PickerState,
    sort: PickerState,
    reset: FocusHandle,
}
impl Controls {
    fn new(cx: &gpui::App) -> Self {
        Self {
            segments: [4, 4, 2, 3].map(|count| (0..count).map(|_| cx.focus_handle()).collect()),
            provider: PickerState::new(cx),
            sort: PickerState::new(cx),
            reset: cx.focus_handle().tab_stop(true),
        }
    }
    fn picker(&self, menu: Menu) -> &PickerState {
        match menu {
            Menu::Provider => &self.provider,
            Menu::Sort => &self.sort,
        }
    }
    fn picker_mut(&mut self, menu: Menu) -> &mut PickerState {
        match menu {
            Menu::Provider => &mut self.provider,
            Menu::Sort => &mut self.sort,
        }
    }
}
pub struct StatisticsView {
    preview: bool,
    preview_cleared: bool,
    period: Period,
    data: Dashboard,
    chart: Chart,
    group: Group,
    mode: Mode,
    provider: Option<Provider>,
    sort: Sort,
    controls: Option<Controls>,
    menu: Option<Menu>,
    needs_reload: bool,
    loading: bool,
    resetting: bool,
    generation: u64,
    error: Option<String>,
    reset_armed: bool,
}
impl StatisticsView {
    pub fn new(preview: bool) -> Self {
        let mut view = Self {
            preview,
            preview_cleared: false,
            period: Period::Week,
            data: Dashboard::default(),
            chart: Chart::Words,
            group: Group::Provider,
            mode: Mode::All,
            provider: None,
            sort: Sort::Requests,
            controls: None,
            menu: None,
            needs_reload: false,
            loading: false,
            resetting: false,
            generation: 0,
            error: None,
            reset_armed: false,
        };
        view.refresh();
        view
    }
    /// Callers mark a refresh and notify; rendering starts an off-thread read.
    pub fn refresh(&mut self) {
        if self.preview {
            self.data = preview_dashboard(self.period);
            if self.preview_cleared {
                self.data.totals = Totals::default();
                self.data.previous = self.data.previous.as_ref().map(|_| Totals::default());
                for (_, totals) in &mut self.data.daily {
                    *totals = Totals::default();
                }
            }
            self.error = None;
        } else if !self.resetting {
            self.generation = self.generation.wrapping_add(1);
            self.needs_reload = true;
        }
    }
    fn accept_result(
        &mut self,
        generation: u64,
        result: Result<Dashboard, String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        self.resetting = false;
        match result {
            Ok(data) => {
                self.data = data;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.preview || self.resetting {
            return;
        }
        self.needs_reload = false;
        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let period = self.period;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { stats::dashboard(period).map_err(|error| format!("{error:#}")) },
                )
                .await;
            let _ = this.update(cx, |this, cx| this.accept_result(generation, result, cx));
        })
        .detach();
    }
    fn select_period(&mut self, period: Period, cx: &mut Context<Self>) {
        if self.resetting {
            return;
        }
        self.period = period;
        self.menu = None;
        self.reset_armed = false;
        self.refresh();
        cx.notify();
    }
    fn reset(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.resetting {
            return;
        }
        if !self.reset_armed {
            self.reset_armed = true;
            cx.notify();
            return;
        }
        self.reset_armed = false;
        if self.preview {
            self.preview_cleared = true;
            self.refresh();
            cx.notify();
            return;
        }
        self.resetting = true;
        self.loading = true;
        self.needs_reload = false;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let period = self.period;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    stats::clear()
                        .and_then(|_| stats::dashboard(period))
                        .map_err(|error| format!("{error:#}"))
                })
                .await;
            let _ = this.update(cx, |this, cx| this.accept_result(generation, result, cx));
        })
        .detach();
        cx.notify();
    }
    fn segment_index(&self, segment: Segment) -> usize {
        match segment {
            Segment::Period => Period::ALL
                .iter()
                .position(|period| *period == self.period)
                .unwrap_or(0),
            Segment::Chart => Chart::ALL
                .iter()
                .position(|chart| *chart == self.chart)
                .unwrap_or(0),
            Segment::Group => usize::from(self.group == Group::Model),
            Segment::Mode => match self.mode {
                Mode::All => 0,
                Mode::Live => 1,
                Mode::Recorded => 2,
            },
        }
    }
    fn choose_segment(&mut self, segment: Segment, index: usize, cx: &mut Context<Self>) {
        if self.resetting {
            return;
        }
        match segment {
            Segment::Period => {
                self.select_period(Period::ALL[index], cx);
                return;
            }
            Segment::Chart => self.chart = Chart::ALL[index],
            Segment::Group => self.group = [Group::Provider, Group::Model][index],
            Segment::Mode => self.mode = [Mode::All, Mode::Live, Mode::Recorded][index],
        }
        cx.notify();
    }
    fn segments(&self, segment: Segment, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let labels = segment.labels();
        let count = labels.len();
        let selected = self.segment_index(segment);
        let item_width = (width - 6.0) / count as f32;
        segmented_control()
            .debug_selector(move || format!("statistics-control-{}", segment.index()))
            .w(px(width))
            .flex_none()
            .children(labels.into_iter().enumerate().map(|(index, label)| {
                let focus =
                    self.controls.as_ref().unwrap().segments[segment.index()][index].clone();
                segmented_item(index == selected)
                    .w(px(item_width))
                    .px_0()
                    .justify_center()
                    .when(index == selected, |item| {
                        item.debug_selector(move || {
                            format!("statistics-active-{}", segment.index())
                        })
                    })
                    .id(("statistics-segment", segment.index() * 4 + index))
                    .track_focus(&focus.clone().tab_stop(index == selected && !self.resetting))
                    .focus(|style| style.border_1().border_color(rgb(ACCENT)))
                    .when(self.resetting, |item| item.opacity(0.45))
                    .child(
                        div()
                            .debug_selector(move || {
                                format!("statistics-label-{}-{index}", segment.index())
                            })
                            .child(label),
                    )
                    .on_click(cx.listener(move |this, event, window, cx| {
                        if matches!(event, gpui::ClickEvent::Mouse(_)) {
                            focus.focus(window);
                            this.choose_segment(segment, index, cx);
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.modifiers.platform
                            || event.keystroke.modifiers.control
                            || event.keystroke.modifiers.alt
                        {
                            return;
                        }
                        if event.keystroke.key == "tab" {
                            if event.keystroke.modifiers.shift {
                                window.focus_prev();
                            } else {
                                window.focus_next();
                            }
                            cx.stop_propagation();
                            return;
                        }
                        if let Some(next) =
                            segment_key(&event.keystroke.key, this.segment_index(segment), count)
                        {
                            this.choose_segment(segment, next, cx);
                            this.controls.as_ref().unwrap().segments[segment.index()][next]
                                .focus(window);
                            cx.stop_propagation();
                        }
                    }))
            }))
            .into_any_element()
    }
    fn provider_choices(&self) -> Vec<Option<Provider>> {
        let mut providers = stats_dashboard::observed_providers(&self.data.totals);
        if let Some(selected) = self.provider
            && !providers.contains(&selected)
        {
            providers.push(selected);
            providers.sort();
        }
        std::iter::once(None)
            .chain(providers.into_iter().map(Some))
            .collect()
    }
    fn menu_choices(&self, menu: Menu) -> Vec<String> {
        match menu {
            Menu::Provider => self
                .provider_choices()
                .into_iter()
                .map(|provider| provider.map_or("All providers", Provider::label).to_owned())
                .collect(),
            Menu::Sort => ["Attempts", "Average latency", "Success rate"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }
    }
    fn toggle_menu(&mut self, menu: Menu, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu == Some(menu) {
            self.menu = None;
            self.controls
                .as_ref()
                .unwrap()
                .picker(menu)
                .trigger
                .focus(window);
        } else {
            let count = self.menu_choices(menu).len();
            let selected = match menu {
                Menu::Provider => self
                    .provider_choices()
                    .iter()
                    .position(|provider| *provider == self.provider)
                    .unwrap_or(0),
                Menu::Sort => match self.sort {
                    Sort::Requests => 0,
                    Sort::Latency => 1,
                    Sort::Reliability => 2,
                },
            };
            self.menu = Some(menu);
            self.controls
                .as_mut()
                .unwrap()
                .picker_mut(menu)
                .open(selected, count, window);
        }
        cx.notify();
    }
    fn choose_menu(
        &mut self,
        menu: Menu,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match menu {
            Menu::Provider => self.provider = self.provider_choices()[index],
            Menu::Sort => self.sort = [Sort::Requests, Sort::Latency, Sort::Reliability][index],
        }
        self.menu = None;
        self.controls
            .as_ref()
            .unwrap()
            .picker(menu)
            .trigger
            .focus(window);
        cx.notify();
    }
    fn menu_keys(
        &mut self,
        menu: Menu,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.platform
            || event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
        {
            return;
        }
        let count = self.menu_choices(menu).len();
        if self
            .controls
            .as_mut()
            .unwrap()
            .picker_mut(menu)
            .navigate(&event.keystroke.key, count)
        {
            cx.stop_propagation();
            cx.notify();
        } else if event.keystroke.key == "enter" {
            if !event.is_held {
                let index = self.controls.as_ref().unwrap().picker(menu).highlight;
                self.choose_menu(menu, index, window, cx);
            }
            cx.stop_propagation();
        } else if matches!(event.keystroke.key.as_str(), "escape" | "tab") {
            self.menu = None;
            self.controls
                .as_ref()
                .unwrap()
                .picker(menu)
                .close(event, window);
            cx.stop_propagation();
            cx.notify();
        }
    }
    fn dropdown(&self, menu: Menu, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.menu_choices(menu);
        let selected = match menu {
            Menu::Provider => self
                .provider_choices()
                .iter()
                .position(|provider| *provider == self.provider)
                .unwrap_or(0),
            Menu::Sort => match self.sort {
                Sort::Requests => 0,
                Sort::Latency => 1,
                Sort::Reliability => 2,
            },
        };
        let picker = self.controls.as_ref().unwrap().picker(menu);
        let popup = (self.menu == Some(menu)).then(|| {
            div()
                .id(if menu == Menu::Provider {
                    "statistics-provider-menu"
                } else {
                    "statistics-sort-menu"
                })
                .w(px(SETTINGS_CONTROL_WIDTH))
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(LINE))
                .bg(rgb(SURFACE))
                .shadow_lg()
                .occlude()
                .track_focus(&picker.menu)
                .on_key_down(cx.listener(move |this, event, window, cx| {
                    this.menu_keys(menu, event, window, cx)
                }))
                .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.menu = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id(if menu == Menu::Provider {
                            "statistics-provider-options"
                        } else {
                            "statistics-sort-options"
                        })
                        .max_h(px(240.0))
                        .overflow_y_scroll()
                        .track_scroll(&picker.scroll)
                        .children(choices.iter().cloned().enumerate().map(|(index, label)| {
                            div()
                                .id(("statistics-choice", index))
                                .h(px(32.0))
                                .px_3()
                                .flex()
                                .items_center()
                                .rounded_sm()
                                .text_size(px(12.0))
                                .when(index == picker.highlight, |row| {
                                    row.bg(rgb(SURFACE_SELECTED))
                                })
                                .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                                .child(label)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.choose_menu(menu, index, window, cx)
                                }))
                        })),
                )
                .into_any_element()
        });
        div()
            .relative()
            .flex_none()
            .w(px(SETTINGS_CONTROL_WIDTH))
            .child(
                disclosure_button(choices[selected].clone())
                    .id(if menu == Menu::Provider {
                        "statistics-provider"
                    } else {
                        "statistics-sort"
                    })
                    .track_focus(&picker.trigger)
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .on_click(cx.listener(move |this, event, window, cx| {
                        if matches!(event, gpui::ClickEvent::Mouse(_)) {
                            this.toggle_menu(menu, window, cx);
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event, window, cx| {
                        if picker_open_key(event) {
                            this.toggle_menu(menu, window, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
            .children(popup.map(picker_popup))
            .into_any_element()
    }
    fn render_header_action(&self, cx: &mut Context<Self>) -> AnyElement {
        let reset = header_button(if self.resetting {
            "Resetting…"
        } else if self.reset_armed {
            "Really reset?"
        } else {
            "Reset"
        })
        .id("statistics-reset")
        .track_focus(&self.controls.as_ref().unwrap().reset)
        .when(self.reset_armed, |button| button.text_color(rgb(NEGATIVE)))
        .when(self.loading, |button| button.opacity(0.45))
        .on_click(cx.listener(|this, event, _, cx| {
            if matches!(event, gpui::ClickEvent::Mouse(_)) {
                this.reset(cx);
            }
        }))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") && !event.is_held {
                this.reset(cx);
                cx.stop_propagation();
            }
        }));
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(self.segments(Segment::Period, 262.0, cx))
            .child(reset)
            .into_any_element()
    }

    fn render_overview(&self) -> AnyElement {
        let totals = &self.data.totals;
        let completed = totals.dictations.saturating_add(totals.failed_dictations);
        let previous = self.data.previous.as_ref();
        let (cost, cost_detail) = overview_cost(totals);
        let cards = div()
            .flex()
            .gap_3()
            .child(card(
                "Words",
                format_count(totals.words),
                trend(Some(totals.words), previous.map(|old| old.words), true),
            ))
            .child(card(
                "Successful",
                format_count(totals.dictations),
                trend(
                    Some(totals.dictations),
                    previous.map(|old| old.dictations),
                    true,
                ),
            ))
            .child(card(
                "Success rate",
                rate(totals.dictations, completed),
                (
                    format!(
                        "{} failed · {} silent",
                        format_count(totals.failed_dictations),
                        format_count(totals.skipped_silent)
                    ),
                    MUTED,
                ),
            ))
            .child(card(
                "Avg wait",
                measured_latency(totals.average_latency_ms()),
                trend(
                    totals.average_latency_ms(),
                    previous.and_then(Totals::average_latency_ms),
                    false,
                ),
            ));
        let trimmed = totals.recorded_ms.saturating_sub(totals.sent_ms);
        let secondary = div()
            .flex()
            .gap_3()
            .child(small_card(
                "Audio recorded",
                format_duration(totals.recorded_ms),
                if trimmed > 0 {
                    format!(
                        "{} sent · {} silence trimmed",
                        format_duration(totals.sent_ms),
                        rate(trimmed, totals.recorded_ms)
                    )
                } else if totals.recorded_ms > 0 {
                    "Nothing trimmed".into()
                } else {
                    "No recorded audio".into()
                },
            ))
            .child(small_card(
                "Reported cost (USD)",
                cost.map_or_else(|| "—".into(), format_cost),
                cost_detail,
            ));
        let details = &totals.details;
        let recoveries = div()
            .flex()
            .gap_3()
            .child(recovery_column(
                "Fallback",
                details.fallback_dictations,
                "Answered by the next model",
            ))
            .child(recovery_column(
                "Retry",
                details.retried_dictations,
                "Same model asked again",
            ))
            .child(recovery_column(
                "Recorded retry",
                details.live_recoveries,
                "Live failed, the recorded clip answered",
            ));
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title(
                "Overview",
                match self.period {
                    Period::Today => "compared with yesterday",
                    Period::Week => "compared with the previous 7 days",
                    Period::Month => "compared with the previous 30 days",
                    Period::AllTime => "all retained days (up to 400)",
                },
            ))
            .child(cards)
            .child(secondary)
            .child(note(
                "Words are raw transcription output. Avg wait is transcription time, \
                 including failures; queue and paste are excluded.",
            ))
            .child(
                compact_panel()
                    .flex_none()
                    .child(compact_panel_header("Recoveries", None))
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(recoveries)
                            .child(note(coverage(totals))),
                    ),
            )
            .into_any_element()
    }
    fn render_chart(&self, cx: &mut Context<Self>) -> AnyElement {
        let values: Vec<_> = self
            .data
            .daily
            .iter()
            .map(|(_, totals)| self.chart.value(totals))
            .collect();
        let peak = values.iter().flatten().copied().max();
        let maximum = peak.unwrap_or(0);
        let bars = values.iter().enumerate().map(|(index, value)| {
            let height = value.map(|value| {
                if maximum == 0 {
                    1.0
                } else {
                    (value as f32 / maximum as f32 * CHART_HEIGHT).max(if value > 0 {
                        3.0
                    } else {
                        1.0
                    })
                }
            });
            div()
                .flex_1()
                .min_w(px(2.0))
                .h_full()
                .flex()
                .items_end()
                .justify_center()
                .child(
                    div()
                        .debug_selector(move || format!("statistics-bar-{index}"))
                        .w_full()
                        .max_w(px(if height.is_some() { 44.0 } else { 4.0 }))
                        .h(px(height.unwrap_or(4.0)))
                        .rounded_t(px(3.0))
                        .bg(if value.is_none() {
                            rgb(FAINT)
                        } else if value.unwrap_or(0) > 0 {
                            rgb(ACCENT)
                        } else {
                            rgb(LINE)
                        }),
                )
        });
        compact_panel().debug_selector(|| "statistics-chart".into()).flex_none()
            .child(div().px_4().py_3().flex().flex_wrap().items_center().justify_between().gap_3()
                .child(div().flex().flex_col().gap_1()
                    .child(div().text_size(px(12.0)).font_weight(FontWeight::SEMIBOLD).child(self.chart.title()))
                    .child(note(peak.map_or_else(|| "No measurements".into(), |value| format!("Peak {}", self.chart.format(value))))))
                .child(self.segments(Segment::Chart, SETTINGS_CONTROL_WIDTH, cx)))
            .child(div().px_4().pt_2().pb_3()
                .child(div().debug_selector(|| "statistics-chart-plot".into()).h(px(CHART_HEIGHT)).flex_none().flex().items_end()
                    .gap(px(if values.len() > 14 { 3.0 } else { 8.0 })).children(bars))
                .child(div().pt_2().flex().justify_between().text_size(px(10.0)).text_color(rgb(FAINT))
                    .child(self.data.daily.first().map_or_else(String::new, |(day, _)| short_day(day)))
                    .child(self.data.daily.last().map_or_else(String::new, |(day, _)| short_day(day))))
                .when(self.chart == Chart::Wait, |chart| chart.child(div().mt_2().child(note("Grey dots mean no wait measurement, rather than zero-latency dictations."))))
                .when(self.period == Period::AllTime, |chart| chart.child(div().mt_2().child(note("Chart: last 30 days. Overview: all retained daily totals.")))))
            .into_any_element()
    }
    fn render_comparison(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = stats_dashboard::comparison_rows(
            &self.data.totals,
            self.group,
            self.mode,
            self.provider,
            self.sort,
        );
        let headers = table_row()
            .text_color(rgb(MUTED))
            .text_size(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(if self.group == Group::Provider {
                        "PROVIDER"
                    } else {
                        "MODEL"
                    }),
            )
            .child(cell("Attempts", 66.0))
            .child(cell("Success", 66.0))
            .child(cell("Avg", 64.0))
            .child(cell("~P95", 64.0));
        let body = rows.iter().enumerate().map(|(index, row)| {
            table_row()
                .debug_selector(move || format!("statistics-comparison-row-{index}"))
                .text_size(px(12.0))
                .text_color(rgb(TEXT_SOFT))
                .child(div().flex_1().min_w_0().truncate().child(row.label.clone()))
                .child(cell(format_count(row.metrics.attempts), 66.0))
                .child(cell(
                    rate(row.metrics.successes, row.metrics.attempts),
                    66.0,
                ))
                .child(cell(
                    measured_latency(row.metrics.latency.average_ms()),
                    64.0,
                ))
                .child(cell(
                    measured_latency(row.metrics.latency.percentile_ms(95)),
                    64.0,
                ))
        });
        compact_panel().debug_selector(|| "statistics-comparison".into()).flex_none()
            .child(compact_panel_header("Request comparison", None))
            .child(div().px_4().py_3().flex().flex_col().gap_3()
                .child(note("Filters apply to this table and the panels below; Overview and the chart include everything."))
                .child(div().flex().flex_wrap().gap_3()
                    .child(labelled("Group", self.segments(Segment::Group, SETTINGS_CONTROL_WIDTH, cx)))
                    .child(labelled("Mode", self.segments(Segment::Mode, SETTINGS_CONTROL_WIDTH, cx))))
                .child(div().flex().flex_wrap().gap_3()
                    .child(labelled("Provider", self.dropdown(Menu::Provider, cx)))
                    .child(labelled("Sort by", self.dropdown(Menu::Sort, cx)))))
            .child(headers).children(body)
            .when(rows.is_empty(), |panel| panel.child(empty_message(if self.data.totals.details.dictations == 0 {
                "No detailed attempts yet. Earlier dictations remain in Overview."
            } else { "No attempts match these filters." })))
            .child(div().px_4().py_3().child(note("Live starts during recording and is timed from release to final text; recorded starts after it and is timed from request to response. Avg and ~P95 use successful attempts; ~P95 is approximate and needs 20 measurements.")))
            .into_any_element()
    }
    fn render_details(&self) -> Option<AnyElement> {
        let combined =
            stats_dashboard::combined_requests(&self.data.totals, self.mode, self.provider);
        if combined.attempts == 0 {
            return None;
        }
        let mut kinds: Vec<_> = combined.errors.iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let modes: Vec<_> = [(Mode::Live, "Live"), (Mode::Recorded, "Recorded")]
            .into_iter()
            .filter(|(mode, _)| self.mode == Mode::All || self.mode == *mode)
            .map(|(mode, label)| {
                let metrics =
                    stats_dashboard::combined_requests(&self.data.totals, mode, self.provider);
                mode_column(label, &metrics)
            })
            .collect();
        let transport = compact_panel()
            .flex_1()
            .min_w(px(240.0))
            .child(compact_panel_header("Live vs recorded", None))
            .child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .gap_4()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .children(modes),
            )
            .child(detail_row(
                "Keywords",
                format!(
                    "{} attempts with hints",
                    format_count(combined.keyword_requests)
                ),
                format!(
                    "{} terms sent · confirmed sends only",
                    format_count(combined.keywords_sent)
                ),
            ));
        let errors =
            compact_panel()
                .flex_1()
                .min_w(px(240.0))
                .child(compact_panel_header("Errors", None))
                .children(kinds.iter().map(|(kind, count)| {
                    div()
                        .px_4()
                        .py_3()
                        .flex()
                        .justify_between()
                        .gap_3()
                        .border_b_1()
                        .border_color(rgb(LINE))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.0))
                                .text_color(rgb(TEXT_SOFT))
                                .child(ErrorKind::label_for_key(kind)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_baseline()
                                .gap_2()
                                .child(div().text_size(px(10.0)).text_color(rgb(FAINT)).child(
                                    format!("{} of attempts", rate(**count, combined.attempts)),
                                ))
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(rgb(NEGATIVE))
                                        .child(format_count(**count)),
                                ),
                        )
                }))
                .when(kinds.is_empty(), |panel| {
                    panel.child(empty_message("No recorded attempt errors."))
                });
        Some(
            div()
                .flex_none()
                .flex()
                .flex_wrap()
                .items_start()
                .gap_3()
                .child(transport)
                .child(errors)
                .into_any_element(),
        )
    }
}
impl Render for StatisticsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.controls.is_none() {
            self.controls = Some(Controls::new(cx));
        }
        if self.needs_reload {
            self.reload(cx);
        }
        let header = pane_header_with_action("Statistics", Some(self.render_header_action(cx)));
        let content = if self.loading {
            pane_content().child(empty_message(if self.resetting {
                "Resetting statistics…"
            } else {
                "Loading statistics…"
            }))
        } else if let Some(error) = &self.error {
            pane_content().child(error_message(
                "Statistics could not be loaded.",
                error.clone(),
            ))
        } else {
            pane_content().gap_4().child(self.render_overview()).child(self.render_chart(cx))
                .child(self.render_comparison(cx)).children(self.render_details())
                .child(note("Daily totals only, never text or audio. Older records lack transport, retry and percentile details. Costs are what providers reported, not a complete bill."))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .on_key_down(|event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                }
            })
            .child(header)
            .child(
                pane_body().child(
                    div()
                        .id("statistics-scroll")
                        .size_full()
                        .overflow_y_scroll()
                        .px_8()
                        .pt_5()
                        .pb_7()
                        .flex()
                        .items_start()
                        .justify_center()
                        .child(content),
                ),
            )
    }
}
fn segment_key(key: &str, current: usize, count: usize) -> Option<usize> {
    match key {
        "left" => Some(current.saturating_sub(1)),
        "right" => Some((current + 1).min(count - 1)),
        "home" => Some(0),
        "end" => Some(count - 1),
        "enter" | "space" => Some(current),
        _ => None,
    }
}
fn note(text: impl Into<gpui::SharedString>) -> Div {
    div()
        .text_size(px(10.0))
        .line_height(px(15.0))
        .text_color(rgb(MUTED))
        .child(text.into())
}
fn labelled(title: &'static str, control: AnyElement) -> Div {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(FAINT))
                .child(title),
        )
        .child(control)
}
fn card(title: &'static str, value: String, (detail, tone): (String, u32)) -> Div {
    div()
        .debug_selector(move || format!("statistics-card-{title}"))
        .flex_1()
        .min_w_0()
        .h(px(116.0))
        .p_3()
        .rounded(px(PANEL_RADIUS))
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(LINE))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(MUTED))
                .child(title),
        )
        .child(
            div()
                .debug_selector(move || format!("statistics-card-value-{title}"))
                .w_full()
                .h(px(32.0))
                .flex_none()
                .line_height(px(30.0))
                .text_size(px(24.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(value),
        )
        .when(!detail.is_empty(), |card| {
            card.child(note(detail).text_color(rgb(tone)))
        })
}
fn small_card(title: &'static str, value: String, detail: String) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .p_3()
        .rounded(px(PANEL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .child(note(title))
        .child(
            div()
                .my_1()
                .text_size(px(17.0))
                .text_color(rgb(TEXT))
                .child(value),
        )
        .child(note(detail))
}
fn table_row() -> Div {
    div()
        .px_4()
        .py_3()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(rgb(LINE))
}
fn cell(text: impl Into<gpui::SharedString>, width: f32) -> Div {
    div()
        .w(px(width))
        .flex_none()
        .flex()
        .justify_end()
        .child(div().min_w_0().truncate().child(text.into()))
}
fn detail_row(title: &'static str, value: String, detail: String) -> Div {
    div()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(rgb(LINE))
        .child(note(title))
        .child(
            div()
                .my_1()
                .text_size(px(12.0))
                .text_color(rgb(TEXT_SOFT))
                .child(value),
        )
        .child(note(detail))
}
fn coverage(totals: &Totals) -> String {
    format!(
        "Counted from {} of {} non-silent dictations with request details.",
        format_count(totals.details.dictations),
        format_count(totals.dictations.saturating_add(totals.failed_dictations))
    )
}
/// Change against the comparison period, coloured by whether it is an improvement.
fn trend(current: Option<u64>, previous: Option<u64>, higher_is_better: bool) -> (String, u32) {
    match (current, previous) {
        (_, None) | (None, _) => (String::new(), MUTED),
        (Some(0), Some(0)) => ("No change".into(), MUTED),
        (Some(_), Some(0)) => ("No prior baseline".into(), MUTED),
        (Some(current), Some(previous)) => {
            let delta = (current as f64 / previous as f64 - 1.0) * 100.0;
            if delta.abs() < 0.5 {
                return ("No change".into(), MUTED);
            }
            let arrow = if delta > 0.0 { "▲" } else { "▼" };
            let text = if delta > 9_999.0 {
                format!("{arrow} >9,999%")
            } else {
                format!("{arrow} {:.0}%", delta.abs())
            };
            let improved = (delta > 0.0) == higher_is_better;
            (text, if improved { POSITIVE } else { NEGATIVE })
        }
    }
}
fn section_title(title: &'static str, detail: &'static str) -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(title),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(detail),
        )
}
fn recovery_column(title: &'static str, count: u64, detail: &'static str) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .text_size(px(17.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(if count > 0 { TEXT } else { MUTED }))
                        .child(format_count(count)),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(TEXT_SOFT))
                        .child(title),
                ),
        )
        .child(note(detail))
}
fn mode_column(label: &'static str, metrics: &RequestTotals) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(note(label))
        .child(
            div()
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(measured_latency(metrics.latency.average_ms())),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(TEXT_SOFT))
                .child(format!(
                    "{} success · {} attempts",
                    rate(metrics.successes, metrics.attempts),
                    format_count(metrics.attempts)
                )),
        )
        .child(note(format!(
            "average of {} measurements",
            format_count(metrics.latency.count)
        )))
}
fn rate(part: u64, whole: u64) -> String {
    if whole == 0 {
        "—".into()
    } else {
        format!("{}%", percent(part, whole))
    }
}
fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    let rounded = (u128::from(part) * 100 + u128::from(whole) / 2) / u128::from(whole);
    rounded.min(100) as u64
}

fn measured_latency(value: Option<u64>) -> String {
    value.map_or_else(|| "—".into(), format_latency)
}

fn overview_cost(totals: &Totals) -> (Option<f64>, String) {
    // Overview always covers the whole selected period. Comparison filters do
    // not enter this calculation, and the legacy total overlaps newer successes.
    let measured = stats_dashboard::combined_requests(totals, Mode::All, None);
    if measured.cost_reports > 0 {
        let mut coverage = format!(
            "{} of {} detailed attempts reported cost",
            format_count(measured.cost_reports),
            format_count(measured.attempts)
        );
        if totals.dictations.saturating_add(totals.failed_dictations) > totals.details.dictations {
            coverage.push_str(" · older records excluded");
        }
        return (Some(measured.reported_cost_usd), coverage);
    }
    if totals.cost_usd.is_finite() && totals.cost_usd > 0.0 {
        return (
            Some(totals.cost_usd),
            "Historical amounts only · no detailed cost reports".into(),
        );
    }
    let detail = if measured.attempts > 0 {
        format!(
            "0 of {} detailed attempts reported cost",
            format_count(measured.attempts)
        )
    } else {
        "No cost reports available".into()
    };
    (None, detail)
}
fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}
fn format_duration(ms: u64) -> String {
    let seconds = ms / 1_000;
    match seconds {
        0..=59 => format!("{seconds} s"),
        60..=3_599 => format!("{} min", seconds / 60),
        _ => format!("{} h {:02} min", seconds / 3_600, seconds % 3_600 / 60),
    }
}
fn format_latency(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1_000.0)
    }
}
fn format_cost(usd: f64) -> String {
    if !usd.is_finite() || usd < 0.0 {
        return "—".into();
    }
    if usd == 0.0 {
        return "$0.00".into();
    }
    if !(0.000_000_000_001..1_000_000_000.0).contains(&usd) {
        return format!("${usd:.6e}");
    }
    let precision = if usd < 0.000_001 { 12 } else { 6 };
    let mut amount = format!("{usd:.precision$}");
    while amount.ends_with('0') && amount.len() - amount.find('.').unwrap_or(0) > 3 {
        amount.pop();
    }
    format!("${amount}")
}
fn short_day(day: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = day.split('-').skip(1);
    match (
        parts.next().and_then(|v| v.parse::<usize>().ok()),
        parts.next().and_then(|v| v.parse::<u32>().ok()),
    ) {
        (Some(month @ 1..=12), Some(day)) => format!("{} {day}", MONTHS[month - 1]),
        _ => day.to_owned(),
    }
}

/// Consecutive synthetic days ending October 8. All aggregates share the same
/// samples, so switching periods does not invent a second set of totals.
fn preview_dashboard(period: Period) -> Dashboard {
    let mut days = Vec::new();
    for day in 0_u64..60 {
        let date = if day < 22 {
            format!("2026-08-{:02}", day + 10)
        } else if day < 52 {
            format!("2026-09-{:02}", day - 21)
        } else {
            format!("2026-10-{:02}", day - 51)
        };
        let mut totals = Totals::default();
        if day % 13 != 3 {
            for index in 0..(20 + day % 11) {
                let provider =
                    ((day + index) % crate::providers::Provider::ALL.len() as u64) as usize;
                let recorded_model = [
                    "microsoft/mai-transcribe-2",
                    "openai::gpt-transcribe",
                    "deepgram::nova-3",
                    "elevenlabs::scribe_v2",
                    "microsoft::MAI-Transcribe-2",
                    "grok::grok-voice-transcribe-2.0",
                    "google::gemini-3.5-transcribe",
                    "meta::muse-voice-transcribe-1.0",
                ][provider];
                let live_model = [
                    None,
                    Some("openai::gpt-live-transcribe"),
                    Some("deepgram::nova-3"),
                    Some("elevenlabs::scribe_v2_realtime"),
                    Some("microsoft::MAI-Transcribe-2-Streaming"),
                    Some("grok::grok-voice-transcribe-2.0"),
                    Some("google::gemini-3.5-transcribe-live"),
                    Some("meta::muse-voice-transcribe-1.0"),
                ][provider];
                let live = index % 2 == 0 && live_model.is_some();
                let first_model = if live {
                    live_model.unwrap()
                } else {
                    recorded_model
                };
                let first_mode = if live {
                    RequestMode::Live
                } else {
                    RequestMode::Recorded
                };
                let failed_attempt = (day + index) % 11 == 0;
                let failed_dictation = (day * 7 + index) % 37 == 0;
                let retry = failed_attempt && !live && index % 2 == 0;
                let fallback = failed_attempt && !retry;
                let winner = if fallback && first_model == "openai::gpt-transcribe" {
                    "deepgram::nova-3"
                } else if fallback {
                    "openai::gpt-transcribe"
                } else {
                    first_model
                };
                let winner_mode = if failed_attempt {
                    RequestMode::Recorded
                } else {
                    first_mode
                };
                let latency = if winner_mode == RequestMode::Live {
                    220 + index * 15
                } else {
                    620 + provider as u64 * 110 + index * 17
                };
                let keywords = usize::from(ModelRef::parse(winner).capabilities().keywords) * 4;
                let cost = (ModelRef::parse(winner).provider == Provider::OpenRouter)
                    .then_some(0.003 + index as f64 * 0.0001);
                let mut attempts = Vec::new();
                let mut failures = Vec::new();
                if failed_attempt || failed_dictation {
                    let kind = if index % 2 == 0 {
                        ErrorKind::RateLimited
                    } else {
                        ErrorKind::Timeout
                    };
                    attempts.push(AttemptSample {
                        model: first_model.into(),
                        mode: first_mode,
                        error: Some(kind),
                        ..Default::default()
                    });
                    failures.push(Failure {
                        model: first_model.into(),
                        kind,
                        detail: String::new(),
                    });
                }
                if !failed_dictation {
                    attempts.push(AttemptSample {
                        model: winner.into(),
                        mode: winner_mode,
                        success: true,
                        latency_ms: Some(latency),
                        keyword_count: keywords,
                        cost_usd: cost,
                        ..Default::default()
                    });
                }
                let recorded = 9_000 + index * 420;
                let mut sample = Sample {
                    telemetry: Some(DictationTelemetry {
                        attempts,
                        used_fallback: fallback && !failed_dictation,
                        retried: retry && !failed_dictation,
                        live_recovered: live && failed_attempt && !failed_dictation,
                    }),
                    words: (!failed_dictation).then_some(24 + (day + index * 3) % 70),
                    models: if failed_dictation {
                        Vec::new()
                    } else {
                        vec![winner.into()]
                    },
                    recorded_ms: recorded,
                    sent_ms: if live { recorded } else { recorded * 72 / 100 },
                    latency_ms: latency + if failed_attempt { 1_300 } else { 0 },
                    cost_usd: if failed_dictation {
                        0.0
                    } else {
                        cost.unwrap_or(0.0)
                    },
                    failures,
                    ..Default::default()
                };
                if !failed_dictation {
                    sample.model_latency.insert(
                        winner.into(),
                        stats::ModelLatency {
                            responses: 1,
                            total_ms: latency,
                        },
                    );
                }
                totals.add_sample(&sample);
            }
            if day % 5 == 0 {
                totals.add_sample(&Sample {
                    recorded_ms: 3_000,
                    skipped_silent: true,
                    ..Default::default()
                });
            }
        }
        days.push((date, totals));
    }
    let count = match period {
        Period::Today => 1,
        Period::Week => 7,
        Period::Month => 30,
        Period::AllTime => 60,
    };
    let mut totals = Totals::default();
    for (_, day) in &days[60 - count..] {
        totals.merge(day);
    }
    let previous = (period != Period::AllTime).then(|| {
        let mut totals = Totals::default();
        for (_, day) in &days[60 - count * 2..60 - count] {
            totals.merge(day);
        }
        totals
    });
    Dashboard {
        totals,
        previous,
        daily: days[60 - count.min(30)..].to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_reported_cost_matches_history_including_failed_attempts() {
        use crate::openrouter::report::{ExecutionReport, StepReport};
        let attempts = vec![
            AttemptSample {
                model: "fixture/primary".into(),
                error: Some(ErrorKind::RateLimited),
                cost_usd: Some(0.001),
                ..Default::default()
            },
            AttemptSample {
                model: "fixture/primary".into(),
                error: Some(ErrorKind::Rejected),
                cost_usd: Some(0.002),
                ..Default::default()
            },
            AttemptSample {
                model: "openai::gpt-transcribe".into(),
                success: true,
                cost_usd: Some(0.0),
                ..Default::default()
            },
        ];
        let report = StepReport {
            executions: attempts
                .iter()
                .map(|attempt| {
                    let model = ModelRef::parse(&attempt.model);
                    ExecutionReport {
                        provider: model.provider.id().into(),
                        model: model.model.into(),
                        cost_usd: attempt.cost_usd,
                        outcome: if attempt.success { "success" } else { "failed" }.into(),
                        ..Default::default()
                    }
                })
                .collect(),
            ..Default::default()
        };
        let mut totals = Totals::default();
        totals.add_sample(&Sample {
            words: Some(1),
            cost_usd: 0.0,
            telemetry: Some(DictationTelemetry {
                attempts,
                used_fallback: true,
                retried: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            totals.cost_usd, 0.0,
            "legacy cost only reflects successful responses"
        );
        let (cost, coverage) = overview_cost(&totals);
        assert!((cost.unwrap() - 0.003).abs() < f64::EPSILON);
        assert_eq!(coverage, "3 of 3 detailed attempts reported cost");
        assert_eq!(report.cost_summary(), "$0.003 USD");
        assert_eq!(
            format_cost(cost.unwrap()),
            report.cost_summary().trim_end_matches(" USD")
        );
        let comparison =
            stats_dashboard::combined_requests(&totals, Mode::Recorded, Some(Provider::OpenAi));
        assert_eq!(comparison.reported_cost_usd, 0.0);
        assert_eq!(comparison.cost_reports, 1);
        assert_eq!(
            stats_dashboard::combined_requests(&totals, Mode::Live, None).attempts,
            0
        );
        assert_eq!(
            overview_cost(&totals),
            (cost, coverage),
            "comparison scope must not change overview"
        );
    }

    #[test]
    fn overview_cost_keeps_legacy_separate_and_unknown_distinct_from_explicit_zero() {
        let legacy = Totals {
            dictations: 5,
            cost_usd: 1.25,
            ..Default::default()
        };
        let (amount, description) = overview_cost(&legacy);
        assert_eq!(amount, Some(1.25));
        assert!(description.contains("Historical amounts only"));
        let mut mixed = legacy;
        mixed.add_sample(&Sample {
            words: Some(1),
            telemetry: Some(DictationTelemetry {
                attempts: vec![AttemptSample {
                    model: "fixture/model".into(),
                    success: true,
                    cost_usd: Some(0.0),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let (amount, description) = overview_cost(&mixed);
        assert_eq!(mixed.cost_usd, 1.25, "historical accounting remains intact");
        assert_eq!(
            amount,
            Some(0.0),
            "do not add overlapping historical totals to measured attempts"
        );
        assert!(description.contains("1 of 1 detailed attempts"));
        assert!(description.contains("older records excluded"));

        let mut measured = Totals::default();
        measured.add_sample(&Sample {
            words: Some(1),
            telemetry: Some(DictationTelemetry {
                attempts: vec![AttemptSample {
                    model: "fixture/model".into(),
                    success: true,
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            overview_cost(&measured),
            (None, "0 of 1 detailed attempts reported cost".into())
        );
        measured.add_sample(&Sample {
            words: Some(1),
            telemetry: Some(DictationTelemetry {
                attempts: vec![AttemptSample {
                    model: "fixture/model".into(),
                    success: true,
                    cost_usd: Some(0.000_000_12),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let (amount, description) = overview_cost(&measured);
        assert_eq!(format_cost(amount.unwrap()), "$0.00000012");
        assert_eq!(description, "1 of 2 detailed attempts reported cost");
        assert_ne!(format_cost(f64::MIN_POSITIVE), "$0.00");
    }

    #[test]
    fn preview_periods_and_request_totals_are_coherent() {
        let week = preview_dashboard(Period::Week);
        let today = preview_dashboard(Period::Today);
        let month = preview_dashboard(Period::Month);
        let all = preview_dashboard(Period::AllTime);
        assert_eq!(week.daily.len(), 7);
        assert_eq!(today.daily.len(), 1);
        assert_eq!(month.daily.len(), 30);
        assert_eq!(all.daily.len(), 30);
        assert!(today.totals.words < week.totals.words && week.totals.words < month.totals.words);
        assert!(month.totals.words < all.totals.words);
        assert!(all.previous.is_none());
        let mut daily = Totals::default();
        for (_, totals) in &week.daily {
            daily.merge(totals);
        }
        assert_eq!(daily, week.totals);
        assert_eq!(
            stats_dashboard::observed_providers(&week.totals).len(),
            crate::providers::Provider::ALL.len()
        );
        assert_eq!(
            week.totals.dictations + week.totals.failed_dictations,
            week.totals.details.dictations
        );
        assert!(
            stats_dashboard::combined_requests(&week.totals, Mode::All, None).attempts
                >= week.totals.details.dictations
        );
    }

    #[test]
    fn missing_metrics_are_not_zero_or_legacy_request_estimates() {
        let legacy = Totals {
            words: 100,
            dictations: 4,
            ..Default::default()
        };
        assert_eq!(
            stats_dashboard::combined_requests(&legacy, Mode::All, None).attempts,
            0
        );
        assert_eq!(Chart::Wait.value(&Totals::default()), None);
        assert_eq!(measured_latency(None), "—");
        assert_eq!(rate(0, 0), "—");
        assert_eq!(trend(Some(10), Some(0), true).0, "No prior baseline");
        assert_eq!(trend(Some(86), Some(100), true), ("▼ 14%".into(), NEGATIVE));
        assert_eq!(
            trend(Some(86), Some(100), false),
            ("▼ 14%".into(), POSITIVE)
        );
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_duration(3_960_000), "1 h 06 min");
        assert_eq!(format_latency(1_340), "1.3 s");
        assert_eq!(short_day("2026-10-08"), "Oct 8");
        assert_eq!(rate(u64::MAX, u64::MAX), "100%");
        assert_eq!(percent(u64::MAX, 1), 100);
        assert_eq!(percent(u64::MAX, 0), 0);
        assert_eq!(format_cost(f64::INFINITY), "—");
        assert_eq!(trend(Some(u64::MAX), Some(1), true).0, "▲ >9,999%");
        assert!(
            coverage(&Totals {
                dictations: u64::MAX,
                failed_dictations: u64::MAX,
                ..Default::default()
            })
            .contains(&format_count(u64::MAX))
        );
    }

    #[gpui::test]
    fn keyboard_filters_keep_global_totals_and_picker_key_up_does_not_reopen(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| StatisticsView::new(true));
        cx.run_until_parked();
        let original = cx.update(|window, cx| {
            let view = view.read(cx);
            view.controls.as_ref().unwrap().segments[0][1].focus(window);
            view.data.totals.clone()
        });
        cx.simulate_keystrokes("right");
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).period, Period::Month);
            assert!(view.read(cx).data.totals.words > original.words);
        });
        let period_totals = cx.update(|window, cx| {
            view.read(cx)
                .controls
                .as_ref()
                .unwrap()
                .provider
                .trigger
                .focus(window);
            view.read(cx).data.totals.clone()
        });
        cx.simulate_keystrokes("enter end enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(view.provider, Some(Provider::Meta));
            assert_eq!(view.menu, None);
            assert!(
                view.controls
                    .as_ref()
                    .unwrap()
                    .provider
                    .trigger
                    .is_focused(window)
            );
            assert_eq!(view.data.totals, period_totals);
            view.controls.as_ref().unwrap().sort.trigger.focus(window);
        });
        cx.simulate_keystrokes("enter down escape");
        cx.update(|_, cx| assert_eq!(view.read(cx).sort, Sort::Requests));
        cx.update(|window, cx| {
            view.read(cx).controls.as_ref().unwrap().segments[3][0].focus(window)
        });
        cx.simulate_keystrokes("right");
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).mode, Mode::Live);
            assert_eq!(view.read(cx).data.totals, period_totals);
        });
    }

    #[gpui::test]
    fn selected_period_background_tracks_mouse_and_keyboard_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| StatisticsView::new(true));
        cx.run_until_parked();
        let initial = cx.debug_bounds("statistics-active-0").unwrap();
        let month = cx.debug_bounds("statistics-label-0-2").unwrap();
        let week = cx.debug_bounds("statistics-label-0-1").unwrap();
        assert!(initial.left() <= week.left() && initial.right() >= week.right());
        cx.simulate_click(month.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(view.read(cx).period, Period::Month));
        let active = cx.debug_bounds("statistics-active-0").unwrap();
        // This selector belongs to the segment with the actual selected
        // background, and is written again every frame (never inferred absent).
        assert!(active.left() > initial.left());
        assert!(active.left() <= month.left() && active.right() >= month.right());
        assert!(active.left() > week.center().x);
        assert_eq!(active.size.height, px(26.0));
        assert_eq!(
            cx.debug_bounds("statistics-control-0").unwrap().size.height,
            px(32.0)
        );
        cx.simulate_keystrokes("home");
        cx.update(|_, cx| assert_eq!(view.read(cx).period, Period::Today));
        let active = cx.debug_bounds("statistics-active-0").unwrap();
        let today = cx.debug_bounds("statistics-label-0-0").unwrap();
        assert!(active.left() <= today.left() && active.right() >= today.right());
        assert!(active.right() < week.center().x);
    }

    #[gpui::test]
    fn overview_values_use_the_card_width_instead_of_collapsing_to_ellipsis(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_, cx) = cx.add_window_view(|_, _| StatisticsView::new(true));
        for width in [1040.0, 860.0] {
            cx.simulate_resize(gpui::size(
                px(width - crate::desktop_ui::SIDEBAR_WIDTH),
                px(720.0),
            ));
            cx.run_until_parked();
            for (card_selector, value_selector) in [
                ("statistics-card-Words", "statistics-card-value-Words"),
                (
                    "statistics-card-Successful",
                    "statistics-card-value-Successful",
                ),
                (
                    "statistics-card-Success rate",
                    "statistics-card-value-Success rate",
                ),
                ("statistics-card-Avg wait", "statistics-card-value-Avg wait"),
            ] {
                let card = cx.debug_bounds(card_selector).unwrap();
                let value = cx.debug_bounds(value_selector).unwrap();
                // 12pt horizontal padding plus 1pt border on each side.
                assert!((value.size.width - (card.size.width - px(26.0))).abs() <= px(1.0));
                assert!(value.size.width >= px(80.0));
                assert_eq!(value.size.height, px(32.0));
                assert!(value.left() > card.left() && value.right() < card.right());
                assert!(value.top() > card.top() && value.bottom() < card.bottom());
            }
        }
    }

    #[gpui::test]
    fn chart_keeps_its_plot_and_bars_when_statistics_overflow_the_viewport(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| StatisticsView::new(true));
        for (width, height) in [(1040.0, 720.0), (860.0, 560.0)] {
            cx.simulate_resize(gpui::size(
                px(width - crate::desktop_ui::SIDEBAR_WIDTH),
                px(height),
            ));
            cx.run_until_parked();
            let chart = cx.debug_bounds("statistics-chart").unwrap();
            let plot = cx.debug_bounds("statistics-chart-plot").unwrap();
            assert_eq!(plot.size.height, px(CHART_HEIGHT));
            assert!(chart.size.height > px(CHART_HEIGHT));
            assert!(plot.top() >= chart.top() && plot.bottom() <= chart.bottom());
            let values = cx.update(|_, cx| {
                view.read(cx)
                    .data
                    .daily
                    .iter()
                    .map(|(_, total)| total.words)
                    .collect::<Vec<_>>()
            });
            let peak = *values.iter().max().unwrap();
            for (index, selector) in [
                "statistics-bar-0",
                "statistics-bar-1",
                "statistics-bar-2",
                "statistics-bar-3",
                "statistics-bar-4",
                "statistics-bar-5",
                "statistics-bar-6",
            ]
            .into_iter()
            .enumerate()
            {
                let bar = cx.debug_bounds(selector).unwrap();
                let tolerance = px(1.0);
                assert!(bar.size.width >= px(2.0));
                assert!(
                    bar.top() >= plot.top() - tolerance
                        && bar.bottom() <= plot.bottom() + tolerance
                );
                assert!(
                    bar.left() >= plot.left() - tolerance
                        && bar.right() <= plot.right() + tolerance
                );
                assert!((bar.bottom() - plot.bottom()).abs() <= tolerance);
                if values[index] == peak {
                    assert!((bar.size.height - px(CHART_HEIGHT)).abs() <= tolerance);
                }
            }
            let comparison = cx.debug_bounds("statistics-comparison").unwrap();
            assert!(comparison.size.width <= px(width - crate::desktop_ui::SIDEBAR_WIDTH));
        }
    }

    #[gpui::test]
    fn preview_reset_and_legacy_empty_states_remain_local(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| StatisticsView::new(true));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.reset(cx);
                view.reset(cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).data.totals, Totals::default());
            assert!(!view.read(cx).needs_reload);
        });
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.select_period(Period::Month, cx);
                assert_eq!(view.data.totals.words, 0);
                view.data.totals = Totals {
                    dictations: 14,
                    words: 220,
                    ..Default::default()
                };
                view.chart = Chart::Wait;
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).data.totals.details.requests.len(), 0);
            assert!(view.read(cx).provider_choices().contains(&None));
        });
        assert_eq!(
            cx.debug_bounds("statistics-chart-plot")
                .unwrap()
                .size
                .height,
            px(CHART_HEIGHT)
        );
    }

    #[gpui::test]
    fn stale_dashboard_results_cannot_replace_the_current_period(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| StatisticsView::new(true));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let old = view.data.totals.clone();
                view.generation = 8;
                view.period = Period::Month;
                view.loading = true;
                view.accept_result(7, Ok(preview_dashboard(Period::Today)), cx);
                assert_eq!(view.data.totals, old);
                assert!(view.loading);
                view.accept_result(8, Ok(preview_dashboard(Period::Month)), cx);
                assert_eq!(view.data.totals, preview_dashboard(Period::Month).totals);
                assert!(!view.loading);
                view.accept_result(7, Err("obsolete error".into()), cx);
                assert!(view.error.is_none());
            })
        });
    }
}
