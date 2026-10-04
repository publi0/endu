//! The Statistics pane: what dictation has cost and how often OpenRouter
//! needed a fallback, over a chosen period.

use gpui::{
    AnyElement, Context, Div, FontWeight, IntoElement, Render, SharedString, Window, div,
    prelude::*, px, rgb,
};

use super::stats::{self, ErrorKind, Period, Totals};
use crate::desktop_ui::{
    ACCENT, FAINT, LINE, MUTED, NEGATIVE, PANEL_RADIUS, SURFACE, TEXT, TEXT_SOFT, compact_panel,
    compact_panel_header, empty_message, error_message, header_button, pane_body, pane_content,
    pane_header_with_action, sliding_segmented_control, sliding_segmented_item,
};

const PERIOD_WIDTH: f32 = 64.0;
const CHART_HEIGHT: f32 = 120.0;

pub struct StatisticsView {
    preview: bool,
    period: Period,
    totals: Totals,
    daily: Vec<(String, u64)>,
    error: Option<String>,
    reset_armed: bool,
}

impl StatisticsView {
    pub fn new(preview: bool) -> Self {
        let mut view = Self {
            preview,
            period: Period::Week,
            totals: Totals::default(),
            daily: Vec::new(),
            error: None,
            reset_armed: false,
        };
        if preview {
            view.load_preview();
        } else {
            view.refresh();
        }
        view
    }

    /// Re-reads `stats.json`; it is small, so this stays on the UI thread.
    pub fn refresh(&mut self) {
        if self.preview {
            return;
        }
        match stats::summary(self.period)
            .and_then(|totals| stats::daily_words(self.period).map(|daily| (totals, daily)))
        {
            Ok((totals, daily)) => {
                self.totals = totals;
                self.daily = daily;
                self.error = None;
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
    }

    fn select_period(&mut self, period: Period, cx: &mut Context<Self>) {
        self.period = period;
        self.reset_armed = false;
        self.refresh();
        cx.notify();
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        if !self.reset_armed {
            self.reset_armed = true;
            cx.notify();
            return;
        }
        self.reset_armed = false;
        if self.preview {
            cx.notify();
            return;
        }
        if let Err(error) = stats::clear() {
            self.error = Some(format!("{error:#}"));
        }
        self.refresh();
        cx.notify();
    }

    fn load_preview(&mut self) {
        let mut totals = Totals {
            dictations: 184,
            failed_dictations: 2,
            skipped_silent: 6,
            words: 9_412,
            recorded_ms: 3_960_000,
            sent_ms: 2_870_000,
            latency_ms: 186 * 940,
            tokens: 412_300,
            cost_usd: 0.4123,
            fallbacks: 9,
            ..Totals::default()
        };
        totals
            .models
            .insert("openai/whisper-large-v3-turbo".into(), 175);
        totals
            .models
            .insert("openai/gpt-4o-mini-transcribe".into(), 9);
        totals.errors.insert(
            "rate_limited".into(),
            [("openai/whisper-large-v3-turbo".into(), 7)].into(),
        );
        totals.errors.insert(
            "timeout".into(),
            [
                ("openai/whisper-large-v3-turbo".into(), 2),
                ("openai/gpt-4o-mini-transcribe".into(), 2),
            ]
            .into(),
        );
        totals.error_examples.insert(
            "rate_limited".into(),
            "HTTP 429: Rate limit exceeded: free-models-per-min".into(),
        );
        totals.error_examples.insert(
            "timeout".into(),
            "network error (exit status: 28): Operation timed out".into(),
        );
        self.totals = totals;
        self.daily = [820, 1_430, 990, 0, 1_870, 2_210, 2_092]
            .into_iter()
            .enumerate()
            .map(|(index, words)| (format!("2026-09-{:02}", 28 + index), words))
            .collect();
    }

    fn render_header_action(&self, cx: &mut Context<Self>) -> AnyElement {
        let index = Period::ALL
            .iter()
            .position(|period| *period == self.period)
            .unwrap_or(0);
        let periods = sliding_segmented_control(index as f32, &[PERIOD_WIDTH; 4]).children(
            Period::ALL.into_iter().enumerate().map(|(index, period)| {
                sliding_segmented_item(PERIOD_WIDTH, period == self.period)
                    .id(("statistics-period", index))
                    .child(period.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.select_period(period, cx)))
            }),
        );
        let reset = header_button(if self.reset_armed {
            "Really reset?"
        } else {
            "Reset"
        })
        .id("statistics-reset")
        .when(self.reset_armed, |button| button.text_color(rgb(NEGATIVE)))
        .on_click(cx.listener(|this, _, _, cx| this.reset(cx)));
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(periods)
            .child(reset)
            .into_any_element()
    }

    fn render_cards(&self) -> AnyElement {
        let totals = &self.totals;
        let attempts = totals.dictations + totals.failed_dictations;
        let saved_ms = totals.recorded_ms.saturating_sub(totals.sent_ms);
        let first = div()
            .flex()
            .gap_3()
            .child(card(
                "WORDS",
                format_count(totals.words),
                totals.words.checked_div(totals.dictations).map_or_else(
                    || "Nothing dictated yet".into(),
                    |words| format!("{words} per dictation"),
                ),
            ))
            .child(card(
                "DICTATIONS",
                format_count(totals.dictations),
                format!("{} audio recorded", format_duration(totals.recorded_ms)),
            ))
            .child(card(
                "AUDIO SENT",
                format_duration(totals.sent_ms),
                if saved_ms > 0 && totals.recorded_ms > 0 {
                    format!(
                        "{} saved by trimming ({}%)",
                        format_duration(saved_ms),
                        saved_ms * 100 / totals.recorded_ms
                    )
                } else {
                    "No silence trimmed".into()
                },
            ))
            .child(card(
                "COST",
                format_cost(totals.cost_usd),
                format!("{} tokens", format_count(totals.tokens)),
            ));
        let second = div()
            .flex()
            .gap_3()
            .child(card(
                "AVERAGE LATENCY",
                totals
                    .average_latency_ms()
                    .map_or_else(|| "—".into(), format_latency),
                "From release to transcript".into(),
            ))
            .child(card(
                "FALLBACKS",
                format_count(totals.fallbacks),
                if totals.dictations > 0 {
                    format!(
                        "{}% of dictations",
                        percent(totals.fallbacks, totals.dictations)
                    )
                } else {
                    "Dictations that needed another model".into()
                },
            ))
            .child(card(
                "FAILED",
                format_count(totals.failed_dictations),
                if attempts > 0 {
                    format!(
                        "{}% — every model failed",
                        percent(totals.failed_dictations, attempts)
                    )
                } else {
                    "Every model failed".into()
                },
            ))
            .child(card(
                "SILENT",
                format_count(totals.skipped_silent),
                "Recordings with no speech, not sent".into(),
            ));
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(first)
            .child(second)
            .into_any_element()
    }

    fn render_chart(&self) -> Option<AnyElement> {
        if self.daily.len() < 2 {
            return None;
        }
        let max = self
            .daily
            .iter()
            .map(|(_, words)| *words)
            .max()
            .unwrap_or(0);
        let first = self.daily.first().map(|(day, _)| short_day(day));
        let last = self.daily.last().map(|(day, _)| short_day(day));
        let bars = self.daily.iter().map(|(_, words)| {
            let height = if max == 0 {
                0.0
            } else {
                (*words as f32 / max as f32 * CHART_HEIGHT).max(if *words > 0 { 3.0 } else { 0.0 })
            };
            div()
                .flex_1()
                .min_w(px(2.0))
                .h_full()
                .flex()
                .items_end()
                .child(
                    div()
                        .w_full()
                        .h(px(height.max(1.0)))
                        .rounded_t(px(3.0))
                        .bg(if *words > 0 { rgb(ACCENT) } else { rgb(LINE) }),
                )
        });
        Some(
            compact_panel()
                .child(compact_panel_header(
                    "Words per day",
                    Some(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .child(format!("Peak {}", format_count(max)))
                            .into_any_element(),
                    ),
                ))
                .child(
                    div()
                        .px_4()
                        .pt_4()
                        .pb_3()
                        .child(
                            div()
                                .h(px(CHART_HEIGHT))
                                .flex()
                                .items_end()
                                .gap(px(if self.daily.len() > 14 { 3.0 } else { 8.0 }))
                                .children(bars),
                        )
                        .child(
                            div()
                                .pt_2()
                                .flex()
                                .justify_between()
                                .text_size(px(10.0))
                                .text_color(rgb(FAINT))
                                .children(first)
                                .children(last),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_models(&self) -> AnyElement {
        let total: u64 = self.totals.models.values().sum();
        let mut models: Vec<_> = self.totals.models.iter().collect();
        models.sort_by(|left, right| right.1.cmp(left.1).then(left.0.cmp(right.0)));
        let body: Vec<AnyElement> = if models.is_empty() {
            vec![empty_message("No transcripts in this period.")]
        } else {
            models
                .into_iter()
                .map(|(model, count)| {
                    let share = percent(*count, total);
                    div()
                        .px_4()
                        .py_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .border_b_1()
                        .border_color(rgb(LINE))
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12.0))
                                        .text_color(rgb(TEXT_SOFT))
                                        .child(model.clone()),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.0))
                                        .text_color(rgb(MUTED))
                                        .child(format!("{} · {share}%", format_count(*count))),
                                ),
                        )
                        .child(meter(share))
                        .into_any_element()
                })
                .collect()
        };
        compact_panel()
            .flex_1()
            .min_w_0()
            .child(compact_panel_header("Models that answered", None))
            .children(body)
            .into_any_element()
    }

    fn render_errors(&self) -> AnyElement {
        let errors = self.totals.errors_by_count();
        let body: Vec<AnyElement> = if errors.is_empty() {
            vec![empty_message("No model errors in this period.")]
        } else {
            errors
                .into_iter()
                .map(|(kind, count)| {
                    let mut models: Vec<_> = self
                        .totals
                        .errors
                        .get(kind)
                        .map(|models| models.iter().collect())
                        .unwrap_or_default();
                    models.sort_by(|left: &(&String, &u64), right| {
                        right.1.cmp(left.1).then(left.0.cmp(right.0))
                    });
                    let example = self.totals.error_examples.get(kind).cloned();
                    div()
                        .px_4()
                        .py_3()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .border_b_1()
                        .border_color(rgb(LINE))
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(TEXT))
                                        .child(ErrorKind::label_for_key(kind)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(12.0))
                                        .text_color(rgb(NEGATIVE))
                                        .child(format_count(count)),
                                ),
                        )
                        .children(models.into_iter().map(|(model, count)| {
                            div()
                                .flex()
                                .justify_between()
                                .gap_3()
                                .text_size(px(11.0))
                                .text_color(rgb(MUTED))
                                .child(div().min_w_0().truncate().child(model.clone()))
                                .child(div().flex_none().child(format_count(*count)))
                        }))
                        .when_some(example, |row, example| {
                            row.child(
                                div()
                                    .pt_1()
                                    .text_size(px(10.0))
                                    .line_height(px(15.0))
                                    .text_color(rgb(FAINT))
                                    .child(SharedString::from(format!("Last: {example}"))),
                            )
                        })
                        .into_any_element()
                })
                .collect()
        };
        compact_panel()
            .flex_1()
            .min_w_0()
            .child(compact_panel_header("Why models failed", None))
            .children(body)
            .into_any_element()
    }
}

impl Render for StatisticsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = pane_header_with_action("Statistics", Some(self.render_header_action(cx)));
        let content = match &self.error {
            Some(error) => pane_content().child(error_message(
                "Statistics could not be loaded.",
                error.clone(),
            )),
            None => pane_content()
                .gap_4()
                .child(self.render_cards())
                .children(self.render_chart())
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap_4()
                        .child(self.render_models())
                        .child(self.render_errors()),
                )
                .child(
                    div()
                        .px_1()
                        .text_size(px(10.0))
                        .line_height(px(15.0))
                        .text_color(rgb(FAINT))
                        .child("Daily totals only — no text or audio. Cost and tokens are what OpenRouter reports for each request."),
                ),
        };
        div().size_full().flex().flex_col().child(header).child(
            pane_body().child(
                div()
                    .id("statistics-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .px_8()
                    .pt_5()
                    .pb_7()
                    .flex()
                    .justify_center()
                    .child(content),
            ),
        )
    }
}

fn card(label: &'static str, value: String, detail: String) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .px_4()
        .py_3()
        .flex()
        .flex_col()
        .gap_1()
        .rounded(px(PANEL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(SURFACE))
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(FAINT))
                .child(label),
        )
        .child(
            div()
                .text_size(px(22.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(value),
        )
        .child(
            div()
                .text_size(px(11.0))
                .line_height(px(15.0))
                .text_color(rgb(MUTED))
                .child(detail),
        )
}

fn meter(percent: u64) -> Div {
    div()
        .w_full()
        .h(px(4.0))
        .rounded_full()
        .bg(rgb(LINE))
        .child(
            div()
                .h_full()
                .w(gpui::relative(percent.min(100) as f32 / 100.0))
                .rounded_full()
                .bg(rgb(ACCENT)),
        )
}

fn percent(part: u64, whole: u64) -> u64 {
    (part * 100 + whole / 2).checked_div(whole).unwrap_or(0)
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        let remaining = digits.len() - index;
        if index > 0 && remaining.is_multiple_of(3) {
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
    if usd <= 0.0 {
        "$0".into()
    } else if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// `2026-10-04` → `Oct 4`.
fn short_day(day: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = day.split('-').skip(1);
    match (
        parts.next().and_then(|month| month.parse::<usize>().ok()),
        parts.next().and_then(|day| day.parse::<u32>().ok()),
    ) {
        (Some(month @ 1..=12), Some(day)) => format!("{} {day}", MONTHS[month - 1]),
        _ => day.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_formatted_for_reading() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_duration(42_000), "42 s");
        assert_eq!(format_duration(125_000), "2 min");
        assert_eq!(format_duration(3_960_000), "1 h 06 min");
        assert_eq!(format_latency(940), "940 ms");
        assert_eq!(format_latency(1_340), "1.3 s");
        assert_eq!(format_cost(0.0), "$0");
        assert_eq!(format_cost(0.00412), "$0.0041");
        assert_eq!(format_cost(1.5), "$1.50");
        assert_eq!(percent(1, 3), 33);
        assert_eq!(percent(2, 3), 67);
        assert_eq!(percent(5, 0), 0);
        assert_eq!(short_day("2026-10-04"), "Oct 4");
        assert_eq!(short_day("d00012"), "d00012");
    }
}
