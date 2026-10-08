use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Render,
    ScrollHandle, Subscription, Window, div, prelude::*, px, rgb,
};

use crate::desktop_ui::{
    ACCENT, LINE, MUTED, NEGATIVE, SURFACE_HOVER, TEXT, compact_button, settings_panel,
    settings_row, settings_section_label, toggle,
};
use crate::text_input::{self, Changed, Dismissed, EditFinished, Submitted, TextInput};
use crate::vocabulary::{Snapshot, Vocabulary};

/// An in-flight change is displayed only after the parent confirms persistence.
struct PendingChange {
    candidate: Vocabulary,
    draft: String,
    consume_draft: bool,
}

#[derive(Clone)]
pub struct VocabularyChange(pub Vocabulary);

pub struct VocabularyView {
    remote: bool,
    models: Vec<String>,
    preferences: Vocabulary,
    snapshot: Snapshot,
    names: Entity<TextInput>,
    sample: Entity<TextInput>,
    chips: Vec<(String, FocusHandle)>,
    chip_scroll: ScrollHandle,
    focused_chip: Option<String>,
    pending_change: Option<PendingChange>,
    error: Option<String>,
    preview: bool,
    support: Vec<(String, String)>,
    focus: [FocusHandle; 4],
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<VocabularyChange> for VocabularyView {}

impl VocabularyView {
    pub fn new(preferences: Vocabulary, preview: bool, cx: &mut Context<Self>) -> Self {
        let names = cx.new(|cx| {
            TextInput::new(cx, "Add a name or phrase, then press Enter", "").commit_on_blur()
        });
        let sample = cx.new(|cx| TextInput::new(cx, "Try a name or a sentence", ""));
        let subscriptions = vec![
            cx.subscribe(&names, |_, _, _: &Changed, cx| cx.notify()),
            cx.subscribe(&names, |this, _, _: &Submitted, cx| this.finish(cx)),
            cx.subscribe(&names, |this, _, _: &EditFinished, cx| this.finish(cx)),
            cx.subscribe(&names, |this, _, _: &Dismissed, cx| this.cancel_draft(cx)),
            cx.subscribe(&sample, |_, _, _: &Changed, cx| cx.notify()),
        ];
        let chips = preferences
            .terms
            .iter()
            .map(|term| (term.clone(), cx.focus_handle().tab_stop(true)))
            .collect();
        if !preview {
            cx.spawn(async move |weak, cx| {
                loop {
                    gpui::Timer::after(Duration::from_secs(1)).await;
                    let rows = crate::openrouter::vocabulary_support::status();
                    if weak
                        .update(cx, |this, cx| {
                            if this.support != rows {
                                this.support = rows;
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        Self {
            remote: false,
            models: Vec::new(),
            snapshot: Snapshot::new(preferences.clone()),
            preferences,
            names,
            sample,
            chips,
            chip_scroll: ScrollHandle::new(),
            focused_chip: None,
            pending_change: None,
            error: None,
            preview,
            support: Vec::new(),
            focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            _subscriptions: subscriptions,
        }
    }

    pub fn for_models(preferences: Vocabulary, preview: bool, cx: &mut Context<Self>) -> Self {
        let mut view = Self::new(preferences, preview, cx);
        view.remote = true;
        view
    }

    pub fn set_models(&mut self, config: &crate::openrouter::Config, cx: &mut Context<Self>) {
        self.models.clone_from(&config.transcription.models);
        cx.notify();
    }

    fn candidate(&self, cx: &Context<Self>) -> Result<Vocabulary, String> {
        appended_terms(&self.preferences, self.names.read(cx).text())
    }

    pub fn pending(&self, cx: &Context<Self>) -> Option<Result<Vocabulary, String>> {
        (self.pending_change.is_none() && !self.names.read(cx).text().trim().is_empty())
            .then(|| self.candidate(cx))
    }

    fn request(&mut self, candidate: Vocabulary, consume_draft: bool, cx: &mut Context<Self>) {
        if self.pending_change.is_some() {
            return;
        }
        if candidate == self.preferences {
            if consume_draft {
                self.names.update(cx, |input, cx| input.set_text("", cx));
            }
            self.error = None;
            cx.notify();
            return;
        }
        self.pending_change = Some(PendingChange {
            candidate: candidate.clone(),
            draft: self.names.read(cx).text().to_owned(),
            consume_draft,
        });
        self.error = None;
        cx.emit(VocabularyChange(candidate));
        cx.notify();
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        if self.pending_change.is_some() {
            return;
        }
        // An explicit submission must not submit again when focus next leaves,
        // even if validation or persistence failed. The visible draft remains.
        let draft = self.names.read(cx).text().to_owned();
        self.names.update(cx, |input, cx| input.set_text(draft, cx));
        match self.candidate(cx) {
            Ok(candidate) => self.request(candidate, true, cx),
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    fn cancel_draft(&mut self, cx: &mut Context<Self>) {
        self.names.update(cx, |input, cx| input.set_text("", cx));
        self.error = None;
        cx.notify();
    }

    /// Paste preserves the current selection and splits only explicit separators.
    /// The single-line TextInput normally flattens newlines to spaces, so capture
    /// this action before it reaches that input.
    fn paste_names(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let separated = text.contains([',', '\n', '\r', '\u{2028}', '\u{2029}']);
        let text = text.replace(['\n', '\r', '\u{2028}', '\u{2029}'], ",");
        self.names.update(cx, |input, cx| {
            input.replace_text_in_range(None, &text, window, cx)
        });
        if separated {
            self.finish(cx);
        }
    }

    fn remove_term(&mut self, term: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_change.is_some() {
            return;
        }
        let mut candidate = self.preferences.clone();
        candidate.terms.retain(|value| value != term);
        // Keep the draft intact when removing an existing chip.
        self.request(candidate, false, cx);
        self.names.focus_handle(cx).focus(window);
    }

    fn remove_last(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.names.read(cx).text().is_empty() {
            return false;
        }
        if let Some(term) = self.preferences.terms.last().cloned() {
            self.remove_term(&term, window, cx);
        }
        true
    }

    pub fn set_preferences(
        &mut self,
        preferences: Vocabulary,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let submitted = self.pending_change.take();
        let draft = self.names.read(cx).text();
        let accepted_draft = if let Some(submitted) = &submitted {
            submitted.consume_draft
                && submitted.candidate == preferences
                && submitted.draft == draft
        } else {
            // The parent also flushes pending() directly before leaving a pane.
            !draft.trim().is_empty()
                && self
                    .candidate(cx)
                    .is_ok_and(|candidate| candidate == preferences)
        };
        if error.is_none() && (accepted_draft || submitted.is_none()) {
            self.names.update(cx, |input, cx| input.set_text("", cx));
        }
        self.chips = preferences
            .terms
            .iter()
            .map(|term| {
                let focus = self
                    .chips
                    .iter()
                    .find(|(old, _)| old == term)
                    .map(|(_, focus)| focus.clone())
                    .unwrap_or_else(|| cx.focus_handle().tab_stop(true));
                (term.clone(), focus)
            })
            .collect();
        if error.is_none()
            && accepted_draft
            && preferences.terms.len() > self.preferences.terms.len()
        {
            self.chip_scroll.scroll_to_item(preferences.terms.len() - 1);
        }
        self.snapshot = Snapshot::new(preferences.clone());
        self.preferences = preferences;
        self.error = error;
        cx.notify();
    }

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.pending_change.is_some() || (index == 2 && !self.preferences.restore_names) {
            return;
        }
        match self.candidate(cx) {
            Ok(mut candidate) => {
                match index {
                    0 => candidate.remote_hints = !candidate.remote_hints,
                    1 => candidate.restore_names = !candidate.restore_names,
                    _ => candidate.approximate = !candidate.approximate,
                }
                self.request(candidate, true, cx);
            }
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    fn render_names(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self
            .chips
            .iter()
            .position(|(_, focus)| focus.is_focused(window));
        let focused_term = focused.map(|index| self.chips[index].0.clone());
        if focused_term != self.focused_chip {
            if let Some(index) = focused {
                self.chip_scroll.scroll_to_item(index);
            }
            self.focused_chip = focused_term;
        }
        let chips = self
            .chips
            .iter()
            .enumerate()
            .map(|(index, (term, focus))| {
                let remove_term = term.clone();
                let key_term = term.clone();
                let focus = focus.clone();
                let click_focus = focus.clone();
                div()
                    .id(("vocabulary-chip", index))
                    .debug_selector(move || format!("vocabulary-chip-{index}"))
                    .track_focus(&focus)
                    .h(px(28.0))
                    .max_w_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl_2()
                    .pr_1()
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(rgb(LINE))
                    .bg(rgb(SURFACE_HOVER))
                    .focus(|chip| chip.border_color(rgb(ACCENT)))
                    .text_size(px(12.0))
                    .on_click(move |_, window, _| click_focus.focus(window))
                    .on_key_down(cx.listener(
                        move |this, event: &gpui::KeyDownEvent, window, cx| {
                            if event.keystroke.modifiers.platform
                                || event.keystroke.modifiers.control
                                || event.keystroke.modifiers.alt
                            {
                                return;
                            }
                            match event.keystroke.key.as_str() {
                                "backspace" | "delete" => {
                                    if !event.is_held {
                                        this.remove_term(&key_term, window, cx);
                                    }
                                    cx.stop_propagation();
                                }
                                "tab" => {
                                    if event.keystroke.modifiers.shift {
                                        window.focus_prev();
                                    } else {
                                        window.focus_next();
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                                _ => {}
                            }
                        },
                    ))
                    .child(div().min_w_0().truncate().child(term.clone()))
                    .child(
                        div()
                            .id(("vocabulary-remove", index))
                            .debug_selector(move || format!("vocabulary-remove-{index}"))
                            .size(px(24.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .rounded(px(4.0))
                            .text_color(rgb(MUTED))
                            .hover(|button| button.text_color(rgb(TEXT)))
                            .child("×")
                            .on_click(cx.listener(move |this, event, window, cx| {
                                if matches!(event, gpui::ClickEvent::Mouse(_)) {
                                    this.remove_term(&remove_term, window, cx);
                                }
                                cx.stop_propagation();
                            })),
                    )
            })
            .collect::<Vec<_>>();
        div()
            .when(!self.chips.is_empty(), |view| {
                view.child(
                    div()
                        .id("vocabulary-chips")
                        .debug_selector(|| "vocabulary-chips".into())
                        .max_h(px(200.0))
                        .overflow_y_scroll()
                        .track_scroll(&self.chip_scroll)
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .mb_3()
                        .children(chips),
                )
            })
            .child(
                div()
                    .debug_selector(|| "vocabulary-draft".into())
                    .capture_action(cx.listener(|this, _: &text_input::Paste, window, cx| {
                        this.paste_names(window, cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &text_input::Backspace, window, cx| {
                        if this.remove_last(window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &text_input::Enter, _, cx| {
                        this.finish(cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &text_input::Escape, _, cx| {
                        this.cancel_draft(cx);
                        cx.stop_propagation();
                    }))
                    .capture_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "tab" {
                            if event.keystroke.modifiers.shift {
                                window.focus_prev();
                            } else {
                                window.focus_next();
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(self.names.clone()),
            )
            .into_any_element()
    }

    fn rule(
        &self,
        index: usize,
        title: &'static str,
        description: &'static str,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = index != 2 || self.preferences.restore_names;
        settings_row(title, description, toggle(if selected { 1.0 } else { 0.0 }))
            .id(("vocabulary-rule", index))
            .debug_selector(move || format!("vocabulary-rule-{index}"))
            .track_focus(&self.focus[index].clone().tab_stop(enabled))
            .when(!enabled, |row| row.opacity(0.45))
            .when(enabled, |row| {
                row.cursor_pointer().hover(|row| row.bg(rgb(SURFACE_HOVER)))
            })
            .focus(|row| row.bg(rgb(SURFACE_HOVER)))
            .on_click(cx.listener(move |this, event, window, cx| {
                if matches!(event, gpui::ClickEvent::Mouse(_)) && enabled {
                    this.focus[index].focus(window);
                    this.choose(index, cx);
                }
            }))
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.platform
                        || event.keystroke.modifiers.control
                        || event.keystroke.modifiers.alt
                    {
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "enter" | "space" => {
                            if !event.is_held {
                                this.choose(index, cx);
                            }
                            cx.stop_propagation();
                        }
                        "tab" => {
                            if event.keystroke.modifiers.shift {
                                window.focus_prev();
                            } else {
                                window.focus_next();
                            }
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                }),
            )
            .into_any_element()
    }

    pub fn restore_text(&self, text: &str) -> String {
        self.snapshot.restore(text).text
    }

    fn recheck(&mut self, cx: &mut Context<Self>) {
        if self.preview {
            self.support = vec![("Preview".into(), "Model checks are simulated here".into())];
        } else {
            crate::openrouter::vocabulary_support::schedule(true);
            self.support = crate::openrouter::vocabulary_support::status();
        }
        cx.notify();
    }
}

impl Render for VocabularyView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let result = self.snapshot.restore(self.sample.read(cx).text()).text;
        let has_provider = |provider| {
            self.models
                .iter()
                .any(|id| crate::providers::ModelRef::parse(id).provider == provider)
        };
        let routed = has_provider(crate::providers::Provider::OpenRouter);
        let editor = self.render_names(window, cx);
        let names = div().p_4().border_b_1().border_color(rgb(LINE))
            .child(div().text_size(px(13.0)).font_weight(gpui::FontWeight::SEMIBOLD)
                .child(if self.remote { "Shared keywords" } else { "Names and terms" }))
            .child(div().mt_1().mb_3().text_size(px(11.0)).text_color(rgb(MUTED))
                .child(if self.remote { "Press Enter to add a phrase. Paste a list separated by commas or new lines. Shared by compatible models, in this order." }
                    else { "Press Enter to add a name or phrase. Paste a list separated by commas or new lines. Shared with Keywords in Models." }))
            .child(editor)
            .when_some(self.error.clone(), |row, error| row.child(div().mt_2().text_size(px(11.0)).text_color(rgb(NEGATIVE)).child(error)));
        let mut panel = settings_panel().child(names);
        if self.remote {
            panel = panel.child(self.rule(
                0,
                "Send keywords",
                "Only terms that fit each model’s supported limits are sent",
                self.preferences.remote_hints,
                cx,
            ));
        } else {
            panel = panel
                .child(self.rule(1, "Restore names locally", "Restores case, spacing and punctuation after other formatting", self.preferences.restore_names, cx))
                .child(self.rule(2, "Correct small spelling errors", "Only long names with one changed letter and a single clear match; URLs and code stay unchanged", self.preferences.approximate, cx))
                .child(div().p_4().child(div().mb_2().text_size(px(11.0)).text_color(rgb(MUTED)).child("Try the local correction"))
                    .child(self.sample.clone())
                    .when(!result.is_empty(), |row| row.child(div().mt_2().text_size(px(12.0)).text_color(rgb(TEXT)).child(result))));
        }
        div()
            .child(settings_section_label(if self.remote {
                "KEYWORDS"
            } else {
                "LOCAL VOCABULARY"
            }))
            .child(panel)
            .when(self.remote, |view| {
                view.when(routed, |view| {
                    view.child(
                        div()
                            .mt_3()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child(
                                if self.preview {
                                    "OpenRouter support · isolated preview"
                                } else {
                                    "OpenRouter support · cached verification"
                                },
                            ))
                            .child(
                                compact_button("Recheck OpenRouter")
                                    .id("vocabulary-recheck")
                                    .track_focus(&self.focus[3])
                                    .on_click(cx.listener(|this, event, _, cx| {
                                        if matches!(event, gpui::ClickEvent::Mouse(_)) {
                                            this.recheck(cx);
                                        }
                                    }))
                                    .on_key_down(cx.listener(
                                        |this, event: &gpui::KeyDownEvent, _, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) && !event.is_held
                                            {
                                                this.recheck(cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    )),
                            ),
                    )
                })
                .children(
                    self.support
                        .iter()
                        .filter(|(model, _)| self.models.contains(model))
                        .map(|(model, status)| {
                            div()
                                .mt_2()
                                .text_size(px(11.0))
                                .text_color(rgb(MUTED))
                                .child(format!("{model} — {status}"))
                        }),
                )
            })
    }
}

fn appended_terms(preferences: &Vocabulary, draft: &str) -> Result<Vocabulary, String> {
    let mut candidate = preferences.clone();
    candidate.terms.extend(
        draft
            .split([',', '\n', '\r', '\u{2028}', '\u{2029}'])
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .map(str::to_owned),
    );
    candidate.validate()?;
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{cell::RefCell, rc::Rc};

    fn vocabulary(terms: &[&str]) -> Vocabulary {
        Vocabulary {
            terms: terms.iter().map(|term| (*term).into()).collect(),
            ..Vocabulary::default()
        }
    }

    #[test]
    fn phrases_stay_whole_and_paste_validation_is_atomic() {
        let accepted = vocabulary(&["OpenRouter"]);
        let candidate =
            appended_terms(&accepted, "Claude Code, Nimbus-Files\nRust\r\nGPUI").unwrap();
        assert_eq!(
            candidate.terms,
            ["OpenRouter", "Claude Code", "Nimbus-Files", "Rust", "GPUI"]
        );
        assert_eq!(
            appended_terms(&accepted, "Claude Code").unwrap().terms[1],
            "Claude Code"
        );
        assert!(appended_terms(&accepted, "Good Name, https://invalid").is_err());
        assert!(appended_terms(&accepted, "New Name, open router").is_err());
        assert_eq!(accepted.terms, ["OpenRouter"]);
    }

    #[gpui::test]
    fn enter_and_blur_add_once_escape_only_cancels_the_draft(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(text_input::key_bindings()));
        let (view, cx) =
            cx.add_window_view(|_, cx| VocabularyView::new(vocabulary(&["OpenRouter"]), true, cx));
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorded = events.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, move |view, event: &VocabularyChange, cx| {
                recorded.borrow_mut().push(event.0.clone());
                view.update(cx, |this, cx| {
                    this.set_preferences(event.0.clone(), None, cx)
                });
            })
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| view.read(cx).names.focus_handle(cx).focus(window));
        cx.simulate_input("Claude Code");
        cx.update(|_, cx| assert_eq!(view.read(cx).preferences.terms, ["OpenRouter"]));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(
                view.read(cx).preferences.terms,
                ["OpenRouter", "Claude Code"]
            );
            assert!(view.read(cx).names.read(cx).text().is_empty());
            assert!(view.read(cx).names.focus_handle(cx).is_focused(window));
            window.blur();
        });
        cx.run_until_parked();
        assert_eq!(events.borrow().len(), 1);
        cx.update(|window, cx| view.read(cx).names.focus_handle(cx).focus(window));
        cx.simulate_input("Never saved");
        cx.simulate_keystrokes("escape");
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(view.read(cx).names.read(cx).text().is_empty());
            assert_eq!(
                view.read(cx).preferences.terms,
                ["OpenRouter", "Claude Code"]
            );
        });
        assert_eq!(events.borrow().len(), 1);
        cx.update(|window, cx| view.read(cx).names.focus_handle(cx).focus(window));
        cx.simulate_input("On blur");
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        assert_eq!(events.borrow().len(), 2);
        cx.update(|_, cx| assert_eq!(view.read(cx).preferences.terms.last().unwrap(), "On blur"));
    }

    #[gpui::test]
    fn paste_creates_whole_chips_and_invalid_batches_remain_editable(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.bind_keys(text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| {
            VocabularyView::for_models(vocabulary(&["OpenRouter"]), true, cx)
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorded = events.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, move |view, event: &VocabularyChange, cx| {
                recorded.borrow_mut().push(event.0.clone());
                view.update(cx, |this, cx| {
                    this.set_preferences(event.0.clone(), None, cx)
                });
            })
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.read(cx).names.focus_handle(cx).focus(window);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                "Claude Code\nNimbus-Files, GPUI".into(),
            ));
        });
        cx.simulate_keystrokes("cmd-v");
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).preferences.terms,
                ["OpenRouter", "Claude Code", "Nimbus-Files", "GPUI"]
            )
        });
        assert_eq!(events.borrow().len(), 1);
        cx.update(|_, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                "Good Name, https://invalid".into(),
            ))
        });
        cx.simulate_keystrokes("cmd-v");
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).preferences.terms.len(), 4);
            assert!(view.read(cx).names.read(cx).text().contains("Good Name"));
            assert!(view.read(cx).error.is_some());
        });
        assert_eq!(events.borrow().len(), 1);
    }

    #[gpui::test]
    fn chip_keyboard_mouse_and_empty_backspace_remove_entire_phrases(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.bind_keys(text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| {
            VocabularyView::new(
                vocabulary(&["Claude Code", "Nimbus-Files", "OpenRouter"]),
                true,
                cx,
            )
        });
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, |view, event: &VocabularyChange, cx| {
                view.update(cx, |this, cx| {
                    this.set_preferences(event.0.clone(), None, cx)
                });
            })
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| view.read(cx).chips[0].1.focus(window));
        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).preferences.terms,
                ["Nimbus-Files", "OpenRouter"]
            )
        });
        // Nonempty input still deletes one draft character.
        cx.simulate_input("abc");
        cx.simulate_keystrokes("backspace");
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).names.read(cx).text(), "ab");
            assert_eq!(view.read(cx).preferences.terms.len(), 2);
        });
        cx.simulate_keystrokes("escape backspace");
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(view.read(cx).preferences.terms, ["Nimbus-Files"]));
        // Shift-Tab enters the remaining chip; Backspace removes it as a unit.
        cx.simulate_keystrokes("shift-tab");
        cx.update(|window, cx| assert!(view.read(cx).chips[0].1.is_focused(window)));
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        cx.update(|_, cx| assert!(view.read(cx).preferences.terms.is_empty()));
        cx.simulate_keystrokes("backspace");
        cx.update(|_, cx| {
            view.update(cx, |this, cx| {
                this.set_preferences(vocabulary(&["Claude Code"]), None, cx)
            })
        });
        cx.run_until_parked();
        let remove = cx.debug_bounds("vocabulary-remove-0").unwrap();
        cx.simulate_click(remove.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| assert!(view.read(cx).preferences.terms.is_empty()));
    }

    #[gpui::test]
    fn failed_removal_keeps_accepted_chips_and_unsubmitted_draft(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            VocabularyView::new(vocabulary(&["Claude Code", "OpenRouter"]), true, cx)
        });
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, |view, event: &VocabularyChange, cx| {
                assert_eq!(event.0.terms, ["OpenRouter"]);
                view.update(cx, |this, cx| {
                    this.set_preferences(
                        this.preferences.clone(),
                        Some("Could not save.".into()),
                        cx,
                    )
                });
            })
        });
        cx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.names
                    .update(cx, |input, cx| input.set_text("Pending phrase", cx));
                this.remove_term("Claude Code", window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).preferences.terms,
                ["Claude Code", "OpenRouter"]
            );
            assert_eq!(view.read(cx).names.read(cx).text(), "Pending phrase");
            assert!(view.read(cx).error.is_some());
        });
    }

    #[gpui::test]
    fn imported_or_externally_synced_terms_discard_old_drafts(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(text_input::key_bindings()));
        let (view, cx) =
            cx.add_window_view(|_, cx| VocabularyView::new(vocabulary(&["Original"]), true, cx));
        let events = Rc::new(RefCell::new(0));
        let recorded = events.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, move |_, _: &VocabularyChange, _| {
                *recorded.borrow_mut() += 1;
            })
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| view.read(cx).names.focus_handle(cx).focus(window));
        cx.simulate_input("Old draft");
        cx.update(|_, cx| {
            view.update(cx, |this, cx| {
                assert!(this.pending(cx).is_some());
                this.set_preferences(vocabulary(&["Imported phrase"]), None, cx);
                assert!(this.names.read(cx).text().is_empty());
                assert!(this.pending(cx).is_none());
            })
        });
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        assert_eq!(*events.borrow(), 0);
        cx.update(|_, cx| assert_eq!(view.read(cx).preferences.terms, ["Imported phrase"]));
    }

    #[gpui::test]
    fn many_chips_scroll_with_the_draft_outside_the_bounded_area(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            VocabularyView::new(
                Vocabulary {
                    terms: (0..crate::vocabulary::MAX_TERMS)
                        .map(|i| format!("Project {i}"))
                        .collect(),
                    ..Vocabulary::default()
                },
                true,
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(600.0), px(720.0)));
        cx.run_until_parked();
        let chips = cx.debug_bounds("vocabulary-chips").unwrap();
        let input = cx.debug_bounds("vocabulary-draft").unwrap();
        assert!(chips.size.height <= px(200.0));
        assert!(chips.size.height >= px(190.0));
        assert!(input.top() >= chips.bottom());
        assert!(input.bottom() < px(500.0));
        cx.update(|window, cx| view.read(cx).chips.last().unwrap().1.focus(window));
        cx.run_until_parked();
        let last_selector: &'static str = Box::leak(
            format!("vocabulary-chip-{}", crate::vocabulary::MAX_TERMS - 1).into_boxed_str(),
        );
        let last = cx.debug_bounds(last_selector).unwrap();
        assert!(last.top() >= chips.top() - px(1.0));
        assert!(last.bottom() <= chips.bottom() + px(1.0));
    }

    #[gpui::test]
    fn vocabulary_edits_wait_for_save_and_failed_saves_preserve_the_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) =
            cx.add_window_view(|_, cx| VocabularyView::new(Vocabulary::default(), true, cx));
        let subscription = cx.update(|_, cx| {
            cx.subscribe(&view, |view, event: &VocabularyChange, cx| {
                view.update(cx, |this, cx| {
                    this.set_preferences(
                        this.preferences.clone(),
                        Some("Could not save.".into()),
                        cx,
                    )
                });
                assert_eq!(event.0.terms, vec!["Nimbus-Files"]);
            })
        });
        cx.update(|_, cx| {
            view.update(cx, |this, cx| {
                this.names
                    .update(cx, |input, cx| input.set_text("Nimbus-Files", cx));
                this.finish(cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert!(view.preferences.terms.is_empty());
            assert!(view.error.is_some());
            assert_eq!(view.names.read(cx).text(), "Nimbus-Files");
        });
        drop(subscription);
    }
}
