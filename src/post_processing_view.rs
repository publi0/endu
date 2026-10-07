use gpui::{
    AnyElement, Context, EventEmitter, FocusHandle, Render, Window, div, prelude::*, px, rgb,
};

use crate::desktop_ui::{
    LINE, MUTED, NEGATIVE, PANE_CONTENT_WIDTH, SURFACE_HOVER, TEXT, compact_panel,
    compact_panel_header, pane_header, settings_panel, settings_row, settings_section_label,
    toggle,
};
use crate::post_processing::Preferences;

const EXAMPLE: &str = "“Olá, João...  Tudo bem?”\nEste é um exemplo.";

#[derive(Clone, Copy)]
pub struct PostProcessingChange(pub Preferences);

#[derive(Clone, Copy, Eq, PartialEq)]
enum Rule {
    Lowercase,
    Initial,
    Punctuation,
    Ellipses,
    Period,
    Spaces,
    SingleLine,
}

impl Rule {
    fn index(self) -> usize {
        self as usize
    }

    fn enabled(self, preferences: Preferences) -> bool {
        match self {
            Self::Initial => !preferences.lowercase,
            Self::Ellipses | Self::Period => !preferences.remove_punctuation,
            _ => true,
        }
    }

    fn selected(self, preferences: Preferences) -> bool {
        match self {
            Self::Lowercase => preferences.lowercase,
            Self::Initial => preferences.lowercase_initial,
            Self::Punctuation => preferences.remove_punctuation,
            Self::Ellipses => preferences.remove_ellipses,
            Self::Period => preferences.remove_final_period,
            Self::Spaces => preferences.collapse_spaces,
            Self::SingleLine => preferences.single_line,
        }
    }

    fn flip(self, preferences: &mut Preferences) {
        let field = match self {
            Self::Lowercase => &mut preferences.lowercase,
            Self::Initial => &mut preferences.lowercase_initial,
            Self::Punctuation => &mut preferences.remove_punctuation,
            Self::Ellipses => &mut preferences.remove_ellipses,
            Self::Period => &mut preferences.remove_final_period,
            Self::Spaces => &mut preferences.collapse_spaces,
            Self::SingleLine => &mut preferences.single_line,
        };
        *field = !*field;
    }

    fn title(self) -> &'static str {
        match self {
            Self::Lowercase => "Lowercase text",
            Self::Initial => "Lowercase first letter",
            Self::Punctuation => "Remove punctuation",
            Self::Ellipses => "Remove ellipses",
            Self::Period => "Remove final period",
            Self::Spaces => "Collapse spaces",
            Self::SingleLine => "Single line",
        }
    }

    fn description(self, preferences: Preferences) -> &'static str {
        if !self.enabled(preferences) {
            return if self == Self::Initial {
                "Included in Lowercase text"
            } else {
                "Included in Remove punctuation"
            };
        }
        match self {
            Self::Lowercase => "Converts all letters to lowercase",
            Self::Initial => "Changes only the first letter; keeps the rest of the text",
            Self::Punctuation => "Removes punctuation marks while keeping words separated",
            Self::Ellipses => "Removes … and runs of three or more dots",
            Self::Period => "Removes the period at the end; keeps other sentence marks",
            Self::Spaces => "Collapses repeated spaces and tabs; keeps paragraphs",
            Self::SingleLine => "Joins paragraphs and line breaks with spaces",
        }
    }
}

pub struct PostProcessingView {
    vocabulary: gpui::Entity<crate::vocabulary_view::VocabularyView>,
    _vocabulary_subscription: gpui::Subscription,
    preferences: Preferences,
    focus: [FocusHandle; 7],
    pending: Option<Rule>,
    error: Option<(Rule, String)>,
}

impl EventEmitter<PostProcessingChange> for PostProcessingView {}
impl EventEmitter<crate::vocabulary_view::VocabularyChange> for PostProcessingView {}

impl PostProcessingView {
    pub fn new(
        preferences: Preferences,
        vocabulary: crate::vocabulary::Vocabulary,
        preview: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let vocabulary =
            cx.new(|cx| crate::vocabulary_view::VocabularyView::new(vocabulary, preview, cx));
        let subscription = cx.subscribe(
            &vocabulary,
            |_, _, event: &crate::vocabulary_view::VocabularyChange, cx| cx.emit(event.clone()),
        );
        Self {
            vocabulary,
            _vocabulary_subscription: subscription,
            preferences,
            focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            pending: None,
            error: None,
        }
    }

    pub fn set_vocabulary(
        &mut self,
        preferences: crate::vocabulary::Vocabulary,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.vocabulary
            .update(cx, |view, cx| view.set_preferences(preferences, error, cx));
    }

    pub fn pending_vocabulary(
        &self,
        cx: &mut gpui::App,
    ) -> Option<Result<crate::vocabulary::Vocabulary, String>> {
        self.vocabulary.update(cx, |view, cx| view.pending(cx))
    }

    pub fn set_preferences(
        &mut self,
        preferences: Preferences,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.preferences = preferences;
        self.error = error.map(|message| (self.pending.unwrap_or(Rule::Lowercase), message));
        self.pending = None;
        cx.notify();
    }

    fn choose(&mut self, rule: Rule, cx: &mut Context<Self>) {
        if !rule.enabled(self.preferences) {
            return;
        }
        let mut candidate = self.preferences;
        rule.flip(&mut candidate);
        self.pending = Some(rule);
        self.error = None;
        cx.emit(PostProcessingChange(candidate));
    }

    fn row(&self, rule: Rule, last: bool, cx: &mut Context<Self>) -> AnyElement {
        let enabled = rule.enabled(self.preferences);
        div()
            .border_color(rgb(LINE))
            .when(!last, |row| row.border_b_1())
            .child(
                settings_row(
                    rule.title(),
                    rule.description(self.preferences),
                    toggle(if rule.selected(self.preferences) || !enabled {
                        1.0
                    } else {
                        0.0
                    }),
                )
                .border_b_0()
                .id(("post-processing-rule", rule.index()))
                .debug_selector(move || format!("post-processing-rule-{}", rule.index()))
                .track_focus(&self.focus[rule.index()].clone().tab_stop(enabled))
                .when(!enabled, |row| row.opacity(0.45))
                .when(enabled, |row| {
                    row.cursor_pointer().hover(|row| row.bg(rgb(SURFACE_HOVER)))
                })
                .focus(|style| style.bg(rgb(SURFACE_HOVER)))
                .on_click(cx.listener(move |this, event, window, cx| {
                    if matches!(event, gpui::ClickEvent::Mouse(_)) && rule.enabled(this.preferences)
                    {
                        this.focus[rule.index()].focus(window);
                        this.choose(rule, cx);
                    }
                }))
                .on_key_down(cx.listener(
                    move |this, event: &gpui::KeyDownEvent, window, cx| {
                        let modifiers = event.keystroke.modifiers;
                        if modifiers.platform || modifiers.control || modifiers.alt {
                            return;
                        }
                        match event.keystroke.key.as_str() {
                            "enter" | "space" => {
                                if !event.is_held {
                                    this.choose(rule, cx);
                                }
                                cx.stop_propagation();
                            }
                            "tab" => {
                                if modifiers.shift {
                                    window.focus_prev();
                                } else {
                                    window.focus_next();
                                }
                                cx.stop_propagation();
                            }
                            _ => {}
                        }
                    },
                )),
            )
            .when_some(
                self.error.as_ref().filter(|(scope, _)| *scope == rule),
                |row, (_, message)| {
                    row.child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_size(px(11.0))
                            .text_color(rgb(NEGATIVE))
                            .child(message.clone()),
                    )
                },
            )
            .into_any_element()
    }
}

impl Render for PostProcessingView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let formatted = self.preferences.process(EXAMPLE);
        let result = self.vocabulary.read(cx).restore_text(&formatted);
        let content = div()
            .child(self.vocabulary.clone())
            .child(settings_section_label("LETTER CASE"))
            .child(
                settings_panel()
                    .child(self.row(Rule::Lowercase, false, cx))
                    .child(self.row(Rule::Initial, true, cx)),
            )
            .child(settings_section_label("PUNCTUATION"))
            .child(
                settings_panel()
                    .child(self.row(Rule::Punctuation, false, cx))
                    .child(self.row(Rule::Ellipses, false, cx))
                    .child(self.row(Rule::Period, true, cx)),
            )
            .child(settings_section_label("SPACING"))
            .child(
                settings_panel()
                    .child(self.row(Rule::Spaces, false, cx))
                    .child(self.row(Rule::SingleLine, true, cx)),
            )
            .child(settings_section_label("EXAMPLE"))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(
                        compact_panel()
                            .flex_1()
                            .min_w_0()
                            .child(compact_panel_header("Original", None))
                            .child(
                                div()
                                    .p_4()
                                    .text_size(px(12.0))
                                    .text_color(rgb(MUTED))
                                    .child(EXAMPLE),
                            ),
                    )
                    .child(
                        compact_panel()
                            .flex_1()
                            .min_w_0()
                            .child(compact_panel_header("Result", None))
                            .child(
                                div()
                                    .p_4()
                                    .text_size(px(12.0))
                                    .text_color(rgb(TEXT))
                                    .child(result),
                            ),
                    ),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header("Post-processing"))
            .child(
                div()
                    .id("post-processing-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_8()
                    .pt_1()
                    .pb_7()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .child(content.w_full().min_w_0().max_w(px(PANE_CONTENT_WIDTH))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn switches_wait_for_save_and_broader_rules_cover_specific_ones(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            PostProcessingView::new(Preferences::default(), Default::default(), true, cx)
        });
        let accepted = std::rc::Rc::new(std::cell::Cell::new(false));
        let subscription = cx.update(|_, cx| {
            let accepted = accepted.clone();
            cx.subscribe(&view, move |view, event: &PostProcessingChange, cx| {
                view.update(cx, |view, cx| {
                    if accepted.get() {
                        view.set_preferences(event.0, None, cx);
                    } else {
                        view.set_preferences(view.preferences, Some("Could not save.".into()), cx);
                    }
                });
            })
        });
        cx.update(|_, cx| view.update(cx, |view, cx| view.choose(Rule::Lowercase, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(!view.read(cx).preferences.lowercase);
            assert!(view.read(cx).error.is_some());
        });
        accepted.set(true);
        cx.update(|window, cx| view.read(cx).focus[0].focus(window));
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert!(view.preferences.lowercase);
            assert!(!Rule::Initial.enabled(view.preferences));
            assert!(view.error.is_none());
        });
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.choose(Rule::Initial, cx);
                assert!(!view.preferences.lowercase_initial);
                view.choose(Rule::Punctuation, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(!Rule::Ellipses.enabled(view.read(cx).preferences));
            assert!(!Rule::Period.enabled(view.read(cx).preferences));
        });
        drop(subscription);
    }
}
