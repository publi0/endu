//! History and recording-recovery lists, details and their actions.

use super::*;
use crate::i18n::t;
use crate::text_input::{Changed, Dismissed, Navigate, Submitted, TextInput};

/// Searchable model picker state for "Retry with" in a recovery detail view.
pub(super) struct RecoveryModelPicker {
    pub search: Entity<TextInput>,
    pub highlight: usize,
    pub scroll: gpui::ScrollHandle,
    _subscriptions: Vec<gpui::Subscription>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RecoveryModelChoice {
    Current,
    Model {
        id: String,
        name: String,
        provider: String,
    },
}

impl RecoveryModelChoice {
    pub(super) fn model_id(&self) -> Option<&str> {
        match self {
            Self::Current => None,
            Self::Model { id, .. } => Some(id.as_str()),
        }
    }
}

/// One History row: a recording to recover or a pasted dictation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum HistoryItem {
    Recovery(String),
    Dictation(u64),
}

/// How long a deleted row takes to fold away.
const HISTORY_FOLD_MS: u64 = 220;
/// How long the copy check stays before returning to the copy symbol.
const HISTORY_COPIED_FOR: Duration = Duration::from_millis(1_500);
/// How long a just-recovered row may still ease in its text.
const HISTORY_RECOVERED_FOR: Duration = Duration::from_secs(3);

impl AppWindow {
    /// One timeline, newest first: a failed recording stays where it
    /// happened, below dictations that came after it.
    pub(super) fn history_items(&self) -> Vec<HistoryItem> {
        let mut items: Vec<_> = self
            .recovery_entries
            .iter()
            .map(|entry| (entry.timestamp_ms, HistoryItem::Recovery(entry.id.clone())))
            .chain(
                self.history_entries
                    .iter()
                    .map(|entry| (entry.timestamp_ms, HistoryItem::Dictation(entry.id))),
            )
            .collect();
        // Stable, so entries recorded in the same millisecond keep their order.
        items.sort_by(|left, right| right.0.cmp(&left.0));
        items.into_iter().map(|(_, item)| item).collect()
    }

    /// The entry shown across the pane, if one is open.
    pub(super) fn open_history_item(&self) -> Option<HistoryItem> {
        self.selected_recovery
            .clone()
            .map(HistoryItem::Recovery)
            .or(self.selected_history.map(HistoryItem::Dictation))
    }

    pub(super) fn show_history_item(&mut self, item: HistoryItem, cx: &mut Context<Self>) {
        self.stop_recovery_playback();
        self.recovery_model_picker = None;
        (self.selected_recovery, self.selected_history) = match item {
            HistoryItem::Recovery(id) => (Some(id), None),
            HistoryItem::Dictation(id) => (None, Some(id)),
        };
        self.recovery_delete_armed = false;
        self.recovery_error = None;
        self.history_copied = None;
        self.history_clear_armed = false;
        self.history_row_delete_armed = None;
        cx.notify();
    }

    pub(super) fn stop_recovery_playback(&mut self) {
        if let Some(playback) = self.recovery_playback.take() {
            playback.stop();
        }
        crate::recording_recovery::stop_active_playback();
    }

    pub(super) fn toggle_recovery_playback(&mut self, id: &str, cx: &mut Context<Self>) {
        if self
            .recovery_playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing(id))
        {
            self.stop_recovery_playback();
            cx.notify();
            return;
        }
        self.stop_recovery_playback();
        let Some(store) = &self.recovery else {
            return;
        };
        match store.play(id) {
            Ok(playback) => {
                let wait = playback.duration() + Duration::from_millis(120);
                let playing_id = id.to_owned();
                self.recovery_playback = Some(playback);
                self.recovery_error = None;
                cx.spawn(async move |this, cx| {
                    gpui::Timer::after(wait).await;
                    let _ = this.update(cx, |this, cx| {
                        if this
                            .recovery_playback
                            .as_ref()
                            .is_some_and(|playback| !playback.is_playing(&playing_id))
                        {
                            this.recovery_playback = None;
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            Err(error) => {
                self.recovery_error = Some((id.to_owned(), error.to_string()));
            }
        }
        cx.notify();
    }

    /// Batch-capable models from currently connected providers, prefixed by
    /// `Current` ("Current models") and filtered by `query`. Uses cached key
    /// availability only.
    pub(super) fn recovery_retry_model_choices(
        &self,
        query: &str,
        cx: &App,
    ) -> Vec<RecoveryModelChoice> {
        let settings = self.openrouter_settings.read(cx);
        let available = settings.available_providers_snapshot();
        let catalog = crate::openrouter::catalog::available_catalog(settings.catalog_models());
        let needle = query.trim().to_lowercase();
        let mut choices = Vec::new();
        if needle.is_empty()
            || t("Current models").to_lowercase().contains(&needle)
            || "current models".contains(&needle)
            || "default".contains(&needle)
        {
            choices.push(RecoveryModelChoice::Current);
        }
        for model in catalog {
            let parsed = crate::providers::ModelRef::parse(&model.id);
            if !available.contains(&parsed.provider)
                || crate::providers::is_realtime_only(&model.id)
            {
                continue;
            }
            let provider = if model.provider.is_empty() {
                parsed.provider.label().to_owned()
            } else {
                model.provider.clone()
            };
            if needle.is_empty()
                || model.name.to_lowercase().contains(&needle)
                || model.id.to_lowercase().contains(&needle)
                || provider.to_lowercase().contains(&needle)
                || parsed.provider.id().contains(&needle)
            {
                choices.push(RecoveryModelChoice::Model {
                    id: model.id,
                    name: model.name,
                    provider,
                });
            }
        }
        choices
    }

    fn open_recovery_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search = cx.new(|cx| {
            TextInput::picker(cx, "", "")
                .localized_placeholder(|| t("Search name, provider or feature"))
        });
        let subscriptions = vec![
            cx.subscribe(&search, |this, _, _: &Changed, cx| {
                if let Some(picker) = &mut this.recovery_model_picker {
                    picker.highlight = 0;
                    picker.scroll.scroll_to_item(0);
                }
                cx.notify();
            }),
            cx.subscribe(&search, |this, _, Navigate(direction): &Navigate, cx| {
                let query = this
                    .recovery_model_picker
                    .as_ref()
                    .map(|picker| picker.search.read(cx).text().to_owned())
                    .unwrap_or_default();
                let count = this.recovery_retry_model_choices(&query, cx).len();
                if let Some(picker) = &mut this.recovery_model_picker
                    && count > 0
                {
                    picker.highlight = if *direction < 0 {
                        picker.highlight.saturating_sub(1)
                    } else {
                        (picker.highlight + 1).min(count - 1)
                    };
                    picker.scroll.scroll_to_item(picker.highlight);
                }
                cx.notify();
            }),
            cx.subscribe_in(&search, window, |this, _, _: &Submitted, window, cx| {
                let query = this
                    .recovery_model_picker
                    .as_ref()
                    .map(|picker| picker.search.read(cx).text().to_owned())
                    .unwrap_or_default();
                let choices = this.recovery_retry_model_choices(&query, cx);
                if let Some(picker) = &this.recovery_model_picker
                    && let Some(choice) = choices.get(picker.highlight)
                {
                    let selected = choice.model_id().map(str::to_owned);
                    this.select_recovery_model(selected, window, cx);
                }
            }),
            cx.subscribe_in(&search, window, |this, _, _: &Dismissed, window, cx| {
                this.close_recovery_model_picker(window, cx);
            }),
        ];
        let choices = self.recovery_retry_model_choices("", cx);
        let highlight = choices
            .iter()
            .position(|choice| choice.model_id() == self.recovery_retry_model.as_deref())
            .unwrap_or(0);
        let scroll = gpui::ScrollHandle::new();
        scroll.scroll_to_item(highlight);
        gpui::Focusable::focus_handle(search.read(cx), cx).focus(window);
        self.recovery_model_picker = Some(RecoveryModelPicker {
            search,
            highlight,
            scroll,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    pub(super) fn close_recovery_model_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recovery_model_picker.take().is_some() {
            self.recovery_model_trigger.focus(window);
            cx.notify();
        }
    }

    pub(super) fn toggle_recovery_model_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recovery_model_picker.is_some() {
            self.close_recovery_model_picker(window, cx);
        } else {
            self.open_recovery_model_picker(window, cx);
        }
    }

    pub(super) fn select_recovery_model(
        &mut self,
        choice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recovery_retry_model = choice;
        self.recovery_model_picker = None;
        self.recovery_model_trigger.focus(window);
        cx.notify();
    }

    fn render_recovery_model_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.recovery_model_picker.as_ref()?;
        let query = picker.search.read(cx).text().to_owned();
        let choices = self.recovery_retry_model_choices(&query, cx);
        let highlight = picker.highlight.min(choices.len().saturating_sub(1));
        let rows = choices.into_iter().enumerate().map(|(index, choice)| {
            let selected = choice.model_id() == self.recovery_retry_model.as_deref();
            let highlighted = index == highlight;
            let (title, subtitle) = match &choice {
                RecoveryModelChoice::Current => (
                    t("Current models").to_owned(),
                    t("Use your current Models settings").to_owned(),
                ),
                RecoveryModelChoice::Model { id, name, provider } => {
                    (name.clone(), format!("{provider} · {id}"))
                }
            };
            let selected_id = choice.model_id().map(str::to_owned);
            div()
                .id(("recovery-model-choice", index))
                .w_full()
                .flex_none()
                .px_3()
                .py(px(7.0))
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .rounded_sm()
                .when(highlighted, |row| row.bg(rgb(SURFACE_HOVER)))
                .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(if selected { TEXT } else { TEXT_SOFT }))
                                .truncate()
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(FAINT))
                                .truncate()
                                .child(subtitle),
                        ),
                )
                .when(selected, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(rgb(ACCENT))
                            .child("✓"),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.select_recovery_model(selected_id.clone(), window, cx);
                }))
        });
        let rows: Vec<_> = rows.collect();
        let empty = rows.is_empty();
        Some(
            div()
                .id("recovery-model-picker")
                .w(px(320.0))
                .max_h(px(300.0))
                .whitespace_normal()
                .p_2()
                .flex()
                .flex_col()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(LINE))
                .bg(rgb(SURFACE))
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    this.recovery_model_picker = None;
                    cx.notify();
                }))
                .child(div().flex_none().child(picker.search.clone()))
                .child(
                    div()
                        .id("recovery-model-picker-list")
                        .flex_1()
                        .min_h_0()
                        .max_h(px(236.0))
                        .overflow_y_scroll()
                        .track_scroll(&picker.scroll)
                        .flex()
                        .flex_col()
                        .when(empty, |list| {
                            list.child(
                                div()
                                    .px_3()
                                    .py_3()
                                    .text_size(px(11.0))
                                    .text_color(rgb(MUTED))
                                    .child(t("No models match.")),
                            )
                        })
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    fn recovery_error_for(&self, id: &str) -> Option<String> {
        self.recovery_error
            .as_ref()
            .filter(|(owner, _)| owner == id)
            .map(|(_, error)| error.clone())
    }

    pub(super) fn can_retry_recovery(&self, entry: &RecoveryEntry) -> bool {
        !entry.busy
            && entry.status != RecoveryStatus::Recovered
            && !self
                .recovery
                .as_ref()
                .is_some_and(RecordingRecovery::retry_in_progress)
    }

    pub(super) fn copy_recovery_text(&mut self, id: &str, cx: &mut Context<Self>) {
        self.history_row_delete_armed = None;
        let Some(text) = self
            .recovery_entries
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.text.clone())
        else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.mark_copied(HistoryItem::Recovery(id.to_owned()), cx);
        self.recovery_error = None;
        cx.notify();
    }

    /// Retry and Copy for a failed recording, usable without opening it.
    fn recovery_row_actions(
        &self,
        index: usize,
        entry: &RecoveryEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let can_retry = self.can_retry_recovery(entry);
        let copied = self.history_copied.as_ref() == Some(&HistoryItem::Recovery(entry.id.clone()));
        let retry_id = entry.id.clone();
        let copy_id = entry.id.clone();
        let delete_id = entry.id.clone();
        let delete_armed = self.history_row_delete_armed.as_ref()
            == Some(&HistoryItem::Recovery(entry.id.clone()));
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0))
            .when(entry.status != RecoveryStatus::Recovered, |actions| {
                actions.child(
                    div()
                        .debug_selector(move || format!("recovery-row-retry-{index}"))
                        .child(
                            if entry.busy {
                                crate::desktop_ui::icon_button_with(
                                    ("recovery-row-retry", index),
                                    t("Transcribing"),
                                    crate::desktop_ui::spinner(
                                        ("recovery-row-spinner", index),
                                        crate::desktop_ui::ThemeColor::Accent,
                                        13.0,
                                    ),
                                )
                            } else {
                                crate::desktop_ui::icon_button(
                                    ("recovery-row-retry", index),
                                    "arrow.clockwise",
                                    t("Retry & copy: transcribe the saved audio again with your current models, then copy the text"),
                                    crate::desktop_ui::ThemeColor::Accent,
                                )
                            }
                            .when(!can_retry && !entry.busy, |button| button.opacity(0.45))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if can_retry {
                                        this.retry_recovery(&retry_id, cx);
                                    }
                                },
                            )),
                        ),
                )
            })
            .when(entry.text.is_some(), |actions| {
                actions.child(
                    div()
                        .debug_selector(move || format!("recovery-row-copy-{index}"))
                        .child(copy_action(("recovery-row-copy", index), copied, self.history_copied_count).on_click(
                            cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.copy_recovery_text(&copy_id, cx);
                            }),
                        )),
                )
            })
            .child(
                delete_action(
                    ("recovery-row-delete", index),
                    delete_armed,
                    self.history_delete_armed_count,
                )
                    .when(entry.busy, |button| button.opacity(0.45))
                    .when(!entry.busy, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.delete_history_row(HistoryItem::Recovery(delete_id.clone()), cx);
                        }))
                    }),
            )
            .into_any_element()
    }

    /// Returns to the list, keeping its search and scroll position.
    pub(super) fn close_history_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_recovery_playback();
        self.recovery_model_picker = None;
        if let Some(item) = self.open_history_item() {
            self.history_last_opened = Some(item);
        }
        self.selected_recovery = None;
        self.selected_history = None;
        self.recovery_delete_armed = false;
        self.recovery_error = None;
        self.window_focus.focus(window);
        cx.notify();
    }

    /// Escape returns to the list; Up and Down open the neighbouring entry.
    /// Keys typed into the search field or a menu are left to them.
    pub(super) fn history_detail_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let modifiers = event.keystroke.modifiers;
        let Some(current) = self.open_history_item() else {
            return false;
        };
        if self.history_retention_open
            || self.recovery_model_picker.is_some()
            || modifiers.control
            || modifiers.alt
            || modifiers.platform
            || modifiers.shift
            || gpui::Focusable::focus_handle(self.history_search.read(cx), cx).is_focused(window)
        {
            return false;
        }
        let step: isize = match event.keystroke.key.as_str() {
            "escape" => {
                self.close_history_item(window, cx);
                return true;
            }
            "up" => -1,
            "down" => 1,
            _ => return false,
        };
        let items = self.history_items();
        if let Some(next) = items
            .iter()
            .position(|item| *item == current)
            .and_then(|index| index.checked_add_signed(step))
            .and_then(|index| items.get(index))
        {
            self.show_history_item(next.clone(), cx);
        }
        true
    }

    fn render_history_back(&self, cx: &mut Context<Self>) -> AnyElement {
        let items = self.history_items();
        let position = self
            .open_history_item()
            .and_then(|current| items.iter().position(|item| *item == current));
        div()
            .px_6()
            .pt_5()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                header_button(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            gpui_symbols::Icon::new("chevron.left")
                                .size(px(10.0))
                                .color(rgb(MUTED))
                                .weight(gpui_symbols::SymbolWeight::Semibold)
                                .rendering_mode(gpui_symbols::RenderingMode::Monochrome),
                        )
                        .child(t("History")),
                )
                .id("history-back")
                .on_click(cx.listener(|this, _, window, cx| this.close_history_item(window, cx))),
            )
            .children(position.map(|index| {
                div().text_size(px(11.0)).text_color(rgb(FAINT)).child(tf!(
                    "{position} of {count} · ↑ ↓",
                    position = index + 1,
                    count = items.len()
                ))
            }))
            .into_any_element()
    }
    pub(super) fn history_revisions(&self) -> ((u64, usize), (u64, usize)) {
        (
            self.history.as_ref().map_or((0, 0), History::view_key),
            self.recovery
                .as_ref()
                .map_or((0, 0), RecordingRecovery::view_key),
        )
    }

    pub(super) fn reload_history(&mut self, cx: &mut Context<Self>) {
        // Read revisions first: a change landing during the reload is picked up next poll.
        self.history_loaded = Some(self.history_revisions());
        let query = self.history_search.read(cx).text().to_string();
        self.recovery_entries = self
            .recovery
            .as_ref()
            .map(|store| store.entries(&query))
            .unwrap_or_default();
        if self
            .selected_recovery
            .as_ref()
            .is_some_and(|id| !self.recovery_entries.iter().any(|entry| &entry.id == id))
        {
            self.selected_recovery = None;
            self.recovery_delete_armed = false;
            self.recovery_error = None;
        }
        self.resolve_recovery_copy(cx);
        let Some(history) = &self.history else {
            self.history_entries.clear();
            self.selected_history = None;
            return;
        };
        self.history_entries = history.search(&query);
        if self
            .selected_history
            .is_some_and(|id| !self.history_entries.iter().any(|entry| entry.id == id))
        {
            self.selected_history = None;
        }
    }

    fn mark_copied(&mut self, item: HistoryItem, cx: &mut Context<Self>) {
        self.history_copied = Some(item);
        let copied_at = Instant::now();
        self.history_copied_at = Some(copied_at);
        self.history_copied_count += 1;
        cx.spawn(async move |window, cx| {
            gpui::Timer::after(HISTORY_COPIED_FOR).await;
            let _ = window.update(cx, |this, cx| {
                if this.history_copied_at == Some(copied_at) {
                    this.history_copied_at = None;
                    this.history_copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Whether `id` was just recovered, so its row eases in its text.
    fn just_recovered(&self, id: &str) -> bool {
        self.history_recovered
            .as_ref()
            .is_some_and(|(recovered, at)| recovered == id && at.elapsed() < HISTORY_RECOVERED_FOR)
    }

    /// Finishes "Retry & copy": copies the recovered text once, or gives up
    /// when the retry fails. Never pastes.
    fn resolve_recovery_copy(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(store)) = (&self.recovery_copy_pending, &self.recovery) else {
            return;
        };
        let entry = store.entries("").into_iter().find(|entry| &entry.id == id);
        match entry {
            Some(entry) if entry.status == RecoveryStatus::Recovered => {
                self.history_recovered = Some((entry.id.clone(), Instant::now()));
                if let Some(text) = entry.text {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    self.mark_copied(HistoryItem::Recovery(entry.id), cx);
                }
                self.recovery_copy_pending = None;
            }
            Some(entry) if entry.busy || store.retry_in_progress() => {}
            _ => self.recovery_copy_pending = None,
        }
    }

    /// Keeps the visible list current while the pane is open.
    pub(super) fn poll_history(&mut self, cx: &mut Context<Self>) -> bool {
        if self.preview && self.recovery.is_none()
            || self.pane != Pane::History
            || self.history.is_none() && self.recovery.is_none()
        {
            return false;
        }
        if self.history_loaded == Some(self.history_revisions()) {
            return false;
        }
        let previous = std::mem::take(&mut self.history_entries);
        let previous_recovery = std::mem::take(&mut self.recovery_entries);
        self.reload_history(cx);
        self.history_entries != previous || self.recovery_entries != previous_recovery
    }

    pub(super) fn set_history_retention(
        &mut self,
        retention: HistoryRetention,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.settings.history_retention == retention {
            return true;
        }
        if !self.update_settings(SettingControl::Retention, cx, |settings| {
            settings.history_retention = retention
        }) {
            return false;
        }
        if let Some(history) = &self.history
            && let Err(error) = history.set_retention(retention)
        {
            self.history_error = Some(error.to_string());
        }
        if let Some(recovery) = &self.recovery {
            recovery.set_retention(retention);
        }
        self.reload_history(cx);
        cx.notify();
        true
    }

    pub(super) fn toggle_retention_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.history_retention_open = !self.history_retention_open;
        if self.history_retention_open {
            let selected = HistoryRetention::ALL
                .iter()
                .position(|value| *value == self.settings.history_retention)
                .unwrap_or(0);
            self.history_retention_picker_state
                .open(selected, HistoryRetention::ALL.len(), window);
        } else {
            self.history_retention_picker_state.trigger.focus(window);
        }
        cx.notify();
    }

    pub(super) fn choose_retention(
        &mut self,
        choice: HistoryRetention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.set_history_retention(choice, cx) {
            self.history_retention_open = false;
            self.history_retention_picker_state.trigger.focus(window);
        }
        cx.notify();
    }

    pub(super) fn retention_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if self
            .history_retention_picker_state
            .navigate(key, HistoryRetention::ALL.len())
        {
        } else if matches!(key, "enter" | "space") {
            if let Some(choice) =
                HistoryRetention::ALL.get(self.history_retention_picker_state.highlight)
            {
                self.choose_retention(*choice, window, cx);
            }
        } else if matches!(key, "escape" | "tab") {
            self.history_retention_open = false;
            self.history_retention_picker_state.close(event, window);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn copy_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        self.history_row_delete_armed = None;
        let Some(entry) = self.history_entries.iter().find(|entry| entry.id == id) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(entry.text.clone()));
        self.mark_copied(HistoryItem::Dictation(id), cx);
        self.history_error = None;
        cx.notify();
    }

    pub(super) fn delete_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(history) = &self.history {
            if let Err(error) = history.delete(id) {
                self.history_error = Some(error.to_string());
            }
            self.reload_history(cx);
        }
        cx.notify();
    }

    /// The row trash asks for a second click: the first arms it, the second
    /// deletes the dictation or the saved recording.
    pub(super) fn delete_history_row(&mut self, item: HistoryItem, cx: &mut Context<Self>) {
        if self.history_removing.is_some() {
            return;
        }
        if self.history_row_delete_armed.as_ref() != Some(&item) {
            self.history_row_delete_armed = Some(item);
            self.history_delete_armed_count += 1;
            cx.notify();
            return;
        }
        self.history_row_delete_armed = None;
        if crate::desktop_ui::reduce_motion() {
            self.remove_history_row(item, cx);
            return;
        }
        // Fold the row away first, then delete it.
        self.history_removing = Some(item.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            gpui::Timer::after(Duration::from_millis(HISTORY_FOLD_MS)).await;
            let _ = this.update(cx, |this, cx| {
                this.history_removing = None;
                this.remove_history_row(item, cx);
            });
        })
        .detach();
    }

    fn remove_history_row(&mut self, item: HistoryItem, cx: &mut Context<Self>) {
        match item {
            HistoryItem::Dictation(id) => self.delete_history_entry(id, cx),
            HistoryItem::Recovery(id) => {
                if let Some(store) = &self.recovery {
                    self.recovery_error = store
                        .delete(&id)
                        .err()
                        .map(|error| (id.clone(), error.to_string()));
                    self.reload_history(cx);
                }
                cx.notify();
            }
        }
    }

    pub(super) fn clear_history(&mut self, cx: &mut Context<Self>) {
        self.history_row_delete_armed = None;
        if !self.history_clear_armed {
            self.history_clear_armed = true;
            cx.notify();
            return;
        }
        self.history_clear_armed = false;
        if let Some(history) = &self.history {
            if let Err(error) = history.clear() {
                self.history_error = Some(error.to_string());
            }
            self.reload_history(cx);
        }
        cx.notify();
    }

    pub(super) fn retry_recovery(&mut self, id: &str, cx: &mut Context<Self>) {
        self.stop_recovery_playback();
        self.recovery_model_picker = None;
        self.history_row_delete_armed = None;
        self.history_copied = None;
        self.recovery_copy_pending = Some(id.to_owned());
        let model_override = self.recovery_retry_model.clone();
        if let Some(store) = &self.recovery {
            self.recovery_error = (if self.preview {
                let preview_model = model_override
                    .clone()
                    .unwrap_or_else(|| "openai/whisper-large-v3-turbo".into());
                let parsed = crate::providers::ModelRef::parse(&preview_model);
                let report = crate::openrouter::StepReport {
                    executions: vec![crate::openrouter::report::ExecutionReport {
                        provider: parsed.provider.id().into(),
                        model: parsed.model.into(),
                        streaming: false,
                        keyword_count: 0,
                        outcome: "success".into(),
                        error: None,
                        cost_usd: None,
                        estimated_cost_usd: None,
                    }],
                    model: Some(preview_model),
                    ..Default::default()
                };
                store.retry_with_processing(
                    id,
                    self.settings.post_processing,
                    crate::vocabulary::Snapshot::new(self.settings.vocabulary.clone()),
                    move |_| {
                        Ok(crate::openrouter::transcribe::Transcription {
                            text: "Recovered preview dictation.".into(),
                            report: Some(report),
                        })
                    },
                )
            } else {
                store.retry_with_model(id, model_override)
            })
            .err()
            .map(|error| (id.to_owned(), error.to_string()));
            if self.recovery_error.is_some() {
                self.recovery_copy_pending = None;
            }
            self.recovery_delete_armed = false;
            self.reload_history(cx);
            cx.notify();
        }
    }

    pub(super) fn render_recovery_rows(
        &self,
        cx: &mut Context<Self>,
    ) -> Vec<(HistoryItem, AnyElement)> {
        self.recovery_entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let id = entry.id.clone();
                let item = HistoryItem::Recovery(id.clone());
                let selected = self.history_last_opened.as_ref() == Some(&item);
                let error = self.recovery_error_for(&entry.id);
                let just_recovered = self.just_recovered(&entry.id);
                let recovered_text = entry
                    .text
                    .as_ref()
                    .filter(|_| entry.status == RecoveryStatus::Recovered)
                    .map(|text| text.replace('\n', " "));
                let mut meta = vec![entry.application_label().to_owned()];
                if let Some(model) = entry.row_model_label() {
                    meta.push(model);
                }
                meta.push(tf!(
                    "{duration} audio",
                    duration = crate::openrouter::report::seconds(entry.audio_ms)
                ));
                let meta = meta.join(" · ");
                let primary_line = if let Some(text) = recovered_text {
                    fade_in_if(
                        just_recovered,
                        ("recovered-text", index),
                        0.0,
                        div()
                            .w_full()
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .truncate()
                            .text_color(rgb(WARNING))
                            // A recovered row reads like any dictation:
                            // its text in warm amber, with full details inside.
                            .child(text),
                    )
                } else {
                    div()
                        .w_full()
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .truncate()
                        .text_color(rgb(if entry.busy { TEXT_SOFT } else { NEGATIVE }))
                        .child(entry.title())
                        .into_any_element()
                };
                let row = div()
                    .id(("recovery-entry", index))
                    .w_full()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .border_b_1()
                    .border_color(rgb(crate::desktop_ui::DIVIDER))
                    .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    // Same columns as a dictation row: text and details on the
                    // left, age on the right.
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(primary_line)
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_size(px(10.0))
                                    .text_color(rgb(FAINT))
                                    .child(meta),
                            )
                            .children(error.map(|error| {
                                div()
                                    .text_size(px(11.0))
                                    .line_height(px(15.0))
                                    .text_color(rgb(NEGATIVE))
                                    .child(error)
                            })),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(rgb(FAINT))
                            .child(event_age(entry.timestamp_ms)),
                    )
                    .child(self.recovery_row_actions(index, entry, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show_history_item(HistoryItem::Recovery(id.clone()), cx);
                    }))
                    .into_any_element();
                (item, row)
            })
            .collect()
    }

    pub(super) fn render_recovery_detail(
        &self,
        entry: &RecoveryEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = entry.id.clone();
        let play_id = id.clone();
        let delete_id = id.clone();
        let copy_id = id.clone();
        let recovered = entry.status == RecoveryStatus::Recovered;
        let can_retry = self.can_retry_recovery(entry);
        let playing = self
            .recovery_playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing(&id));
        let copied = self.history_copied.as_ref() == Some(&HistoryItem::Recovery(id.clone()));
        let report = entry.transcription.as_ref();
        let subtitle = format!(
            "{} · {} · {}",
            entry.application_label(),
            tf!(
                "{duration} audio",
                duration = crate::openrouter::report::seconds(entry.audio_ms)
            ),
            event_age(entry.timestamp_ms)
        );
        let title_color = if entry.busy {
            TEXT
        } else if recovered {
            WARNING
        } else {
            NEGATIVE
        };
        let mut header_actions = div().flex().flex_wrap().items_center().gap_2();
        if entry.text.is_some() {
            header_actions = header_actions.child(
                header_button(if copied { t("Copied") } else { t("Copy text") })
                    .id("recovery-copy")
                    .track_focus(&self.recovery_action_focus[0])
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_recovery_text(&copy_id, cx);
                    })),
            );
        }
        if !recovered {
            header_actions = header_actions.child(
                header_button(if playing { t("Stop") } else { t("Play") })
                    .id("recovery-play")
                    .track_focus(&self.recovery_action_focus[3])
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_recovery_playback(&play_id, cx);
                    })),
            );
        }
        header_actions = header_actions.child(
            header_button(if self.recovery_delete_armed {
                t("Delete permanently?")
            } else {
                t("Delete")
            })
            .id("recovery-delete")
            .track_focus(&self.recovery_action_focus[2].clone().tab_stop(!entry.busy))
            .focus(|style| style.border_color(rgb(ACCENT)))
            .when(self.recovery_delete_armed, |button| {
                button.text_color(rgb(NEGATIVE)).border_color(rgb(NEGATIVE))
            })
            .when(entry.busy, |button| button.opacity(0.45))
            .when(!entry.busy, |button| {
                button.on_click(cx.listener(move |this, _, _, cx| {
                    if !this.recovery_delete_armed {
                        this.recovery_delete_armed = true;
                        cx.notify();
                        return;
                    }
                    this.stop_recovery_playback();
                    if let Some(store) = &this.recovery {
                        this.recovery_error = store
                            .delete(&delete_id)
                            .err()
                            .map(|error| (delete_id.clone(), error.to_string()));
                        this.recovery_delete_armed = false;
                        this.reload_history(cx);
                        cx.notify();
                    }
                }))
            }),
        );
        let retry_bar = (!recovered).then(|| {
            let catalog = self.openrouter_settings.read(cx).catalog_models().to_vec();
            let active_label = self.recovery_retry_model.as_deref().map_or_else(
                || t("Current models").to_owned(),
                |model| crate::openrouter::catalog::selector_label(model, &catalog),
            );
            let picker_menu = self.render_recovery_model_picker(cx);
            let retry_button = header_button(if entry.busy {
                t("Transcribing")
            } else {
                t("Retry & copy")
            })
            .id("recovery-retry")
            .track_focus(&self.recovery_action_focus[1].clone().tab_stop(can_retry))
            .focus(|style| style.border_color(rgb(ACCENT)))
            .when(can_retry, |button| {
                button.border_color(rgb(ACCENT)).text_color(rgb(TEXT))
            })
            .when(!can_retry, |button| button.opacity(0.45))
            .when(can_retry, |button| {
                button.on_click(cx.listener(move |this, _, _, cx| {
                    this.retry_recovery(&id, cx);
                }))
            });
            div()
                .mt_5()
                .pt_5()
                .border_t_1()
                .border_color(rgb(crate::desktop_ui::DIVIDER))
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(rgb(TEXT_SOFT))
                                        .child(t("Retry with")),
                                )
                                .child(
                                    div()
                                        .relative()
                                        .flex_none()
                                        .child(
                                            disclosure_button(active_label)
                                                .id("recovery-model-trigger")
                                                .track_focus(&self.recovery_model_trigger)
                                                .focus(|style| style.border_color(rgb(ACCENT)))
                                                .on_click(cx.listener(|this, event, window, cx| {
                                                    if matches!(event, gpui::ClickEvent::Mouse(_)) {
                                                        this.toggle_recovery_model_picker(
                                                            window, cx,
                                                        );
                                                    }
                                                }))
                                                .on_key_down(cx.listener(
                                                    |this, event, window, cx| {
                                                        if picker_open_key(event) {
                                                            this.toggle_recovery_model_picker(
                                                                window, cx,
                                                            );
                                                            cx.stop_propagation();
                                                        }
                                                    },
                                                )),
                                        )
                                        .children(picker_menu.map(picker_popup)),
                                ),
                        )
                        .child(retry_button),
                )
                .children(self.recovery_error_for(&entry.id).map(|error| {
                    div()
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .text_color(rgb(NEGATIVE))
                        .child(error)
                }))
        });
        let recovered_tiles = recovered.then(|| {
            let mut tiles = div()
                .mt_5()
                .pt_5()
                .border_t_1()
                .border_color(rgb(crate::desktop_ui::DIVIDER))
                .flex()
                .flex_wrap()
                .gap_3()
                .child(history_tile(
                    t("Audio"),
                    report
                        .and_then(|report| report.audio_summary())
                        .unwrap_or_else(|| {
                            (crate::openrouter::report::seconds(entry.audio_ms), None)
                        }),
                ));
            if let Some(model) = entry.detail_model_label() {
                tiles = tiles.child(history_tile(t("Model"), (model, None)));
            }
            if let Some(report) = report {
                tiles = tiles.child(history_tile(t("Cost (USD)"), report.cost_tile()));
            }
            tiles
        });
        // Show the standalone failure block only when the cause isn't already
        // attached to a failed request card below (e.g. post-processing removed
        // all text, Endu quit before transcribing, or legacy recovery files).
        let unlinked_failure = entry.message.clone().filter(|_| {
            !report.is_some_and(crate::openrouter::StepReport::has_failed_attempt_reasons)
        });
        div()
            .id("recovery-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_6()
            .pt_4()
            .pb_6()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_baseline()
                                    .gap_x_3()
                                    .child(
                                        div()
                                            .text_size(px(18.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(rgb(title_color))
                                            .child(entry.title()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(rgb(FAINT))
                                            .child(subtitle),
                                    ),
                            )
                            .child(header_actions),
                    )
                    .children(entry.text.clone().map(|text| {
                        div()
                            .mt_5()
                            .pt_5()
                            .border_t_1()
                            .border_color(rgb(crate::desktop_ui::DIVIDER))
                            .child(section_label(t("Text")))
                            .child(
                                div()
                                    .pt_3()
                                    .text_size(px(13.0))
                                    .line_height(px(20.0))
                                    .text_color(rgb(TEXT))
                                    .child(text),
                            )
                    }))
                    .children(recovered_tiles)
                    .children(retry_bar)
                    .children(unlinked_failure.map(|message| {
                        div()
                            .mt_5()
                            .pt_5()
                            .border_t_1()
                            .border_color(rgb(crate::desktop_ui::DIVIDER))
                            .child(section_label(t("Failure reason")))
                            .child(
                                div()
                                    .pt_2()
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(rgb(NEGATIVE))
                                    .child(message),
                            )
                    }))
                    .children(report.and_then(render_requests_section))
                    .child(
                        div()
                            .mt_5()
                            .pt_4()
                            .border_t_1()
                            .border_color(rgb(crate::desktop_ui::DIVIDER))
                            .text_size(px(11.0))
                            .line_height(px(16.0))
                            .text_color(rgb(if entry.volatile { NEGATIVE } else { MUTED }))
                            .child(if entry.volatile {
                                t("This recording is only in memory. Keep Endu open until it is recovered.")
                            } else if recovered {
                                t("Recovered text is saved locally until you delete it. The temporary audio has been removed.")
                            } else {
                                t("Audio is saved on this Mac until recovery or deletion. Retry & copy uses your chosen model or current Models settings, saves the text here and copies it, without automatic paste.")
                            }),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_history(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let retention = self.settings.history_retention;
        let search = div().w(px(220.0)).child(self.history_search.clone());
        let retention_control = header_button(tf!("Keep: {period}", period = retention.label()))
            .id("history-retention")
            .track_focus(&self.history_retention_picker_state.trigger)
            .focus(|style| style.border_color(rgb(ACCENT)))
            .on_click(cx.listener(|this, event, window, cx| {
                if matches!(event, gpui::ClickEvent::Mouse(_)) {
                    this.toggle_retention_picker(window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event, window, cx| {
                if picker_open_key(event) {
                    this.toggle_retention_picker(window, cx);
                    cx.stop_propagation();
                }
            }));
        let retention_control = div().relative().child(retention_control).when(
            self.history_retention_open,
            |control| {
                control.child(picker_popup(selection_picker_menu(
                    "history-retention-menu",
                    &self.history_retention_picker_state,
                    HistoryRetention::ALL.to_vec(),
                    retention,
                    |choice| choice.label().to_owned(),
                    self.feedback_error(SettingControl::Retention),
                    (
                        cx.listener(|this, choice: &HistoryRetention, window, cx| {
                            this.choose_retention(*choice, window, cx);
                        }),
                        cx.listener(|this, _, _, cx| {
                            this.history_retention_open = false;
                            cx.notify();
                        }),
                        cx.listener(Self::retention_picker_key),
                    ),
                )))
            },
        );
        let retention_control = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(retention_control)
            .when(!self.history_retention_open, |control| {
                control.children(self.setting_feedback(SettingControl::Retention))
            });
        let clear = header_button(if self.history_clear_armed {
            t("Clear text history?")
        } else {
            t("Clear dictations")
        })
        .id("history-clear")
        .when(self.history_clear_armed, |button| {
            button.text_color(rgb(NEGATIVE))
        })
        .on_click(cx.listener(|this, _, _, cx| this.clear_history(cx)));
        let header_action = div()
            .flex()
            .items_center()
            .gap_3()
            .child(search)
            .child(retention_control)
            .child(clear)
            .into_any_element();
        let mut rows: std::collections::HashMap<_, _> =
            self.render_recovery_rows(cx).into_iter().collect();
        rows.extend(
            self.history_entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let id = entry.id;
                    let item = HistoryItem::Dictation(id);
                    let selected = self.history_last_opened.as_ref() == Some(&item);
                    let report = entry.transcription.as_ref();
                    let mut meta: Vec<String> = entry.application.iter().cloned().collect();
                    if let Some(model) =
                        report.and_then(crate::openrouter::StepReport::row_model_label)
                    {
                        meta.push(model);
                    }
                    if entry.total_ms > 0 {
                        meta.push(crate::openrouter::report::duration_label(entry.total_ms));
                    }
                    let had_fallback_or_retry = report
                        .and_then(crate::openrouter::StepReport::recovery_badge)
                        .is_some();
                    let meta = meta.join(" · ");
                    let copied = self.history_copied.as_ref() == Some(&item);
                    let delete_armed = self.history_row_delete_armed.as_ref() == Some(&item);
                    let row = div()
                        .id(("history-entry", index))
                        .w_full()
                        .px_4()
                        .py_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .border_b_1()
                        .border_color(rgb(crate::desktop_ui::DIVIDER))
                        .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                        .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .w_full()
                                        .text_size(px(12.0))
                                        .text_color(rgb(if had_fallback_or_retry {
                                            WARNING
                                        } else {
                                            TEXT_SOFT
                                        }))
                                        .line_height(px(18.0))
                                        .truncate()
                                        .child(entry.text.replace('\n', " ")),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(FAINT))
                                        .truncate()
                                        .child(meta),
                                ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(rgb(FAINT))
                                .child(event_age(entry.timestamp_ms)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .debug_selector(move || format!("history-row-copy-{index}"))
                                        .child(
                                            copy_action(
                                                ("history-row-copy", index),
                                                copied,
                                                self.history_copied_count,
                                            )
                                            .on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.copy_history_entry(id, cx);
                                                }),
                                            ),
                                        ),
                                )
                                .child(
                                    delete_action(
                                        ("history-row-delete", index),
                                        delete_armed,
                                        self.history_delete_armed_count,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.delete_history_row(HistoryItem::Dictation(id), cx);
                                        },
                                    )),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.show_history_item(HistoryItem::Dictation(id), cx);
                        }))
                        .into_any_element();
                    (item, row)
                }),
        );
        let rows: Vec<_> = self
            .history_items()
            .iter()
            .filter_map(|item| {
                let row = rows.remove(item)?;
                Some(if self.history_removing.as_ref() == Some(item) {
                    crate::desktop_ui::animate_once(
                        div().overflow_hidden().child(row),
                        "history-row-fold",
                        HISTORY_FOLD_MS,
                        |row, progress| {
                            let rest = 1.0 - crate::desktop_ui::ease_out(progress);
                            row.opacity(rest).max_h(px(140.0 * rest))
                        },
                    )
                } else {
                    row
                })
            })
            .collect();
        let retention_off = retention.is_off();
        let content = if self.open_history_item().is_some() {
            compact_panel()
                .h_full()
                .flex()
                .flex_col()
                .child(self.render_history_back(cx))
                .child(self.render_history_detail(cx))
                .into_any_element()
        } else {
            pane_list(
                "history-list",
                if retention_off && self.recovery_entries.is_empty() {
                    Some(t(
                        "History is off. Failed recordings are still kept for recovery.",
                    ))
                } else if self.history_entries.is_empty()
                    && self.recovery_entries.is_empty()
                    && self.history_error.is_none()
                {
                    Some(if self.history_search.read(cx).text().trim().is_empty() {
                        t("No dictations retained yet.")
                    } else {
                        t("No matching dictations.")
                    })
                } else {
                    None
                },
                t("History could not be loaded."),
                self.history_error.clone(),
            )
            .track_scroll(&self.history_scroll)
            .rounded(px(PANEL_RADIUS))
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(SURFACE))
            .overflow_x_hidden()
            .child(div().w_full().flex().flex_col().children(rows))
            .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header_with_action(t("History"), Some(header_action)))
            .children(
                self.recovery
                    .as_ref()
                    .and_then(RecordingRecovery::load_warning)
                    .map(|warning| {
                        div()
                            .px_5()
                            .py_2()
                            .text_size(px(12.0))
                            .text_color(rgb(NEGATIVE))
                            .child(warning.to_owned())
                    }),
            )
            .child(
                pane_body()
                    .px_8()
                    .py_5()
                    .child(pane_content().h_full().child(content)),
            )
            .into_any_element()
    }

    pub(super) fn render_history_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(entry) = self
            .selected_recovery
            .as_ref()
            .and_then(|id| self.recovery_entries.iter().find(|entry| &entry.id == id))
        {
            return self.render_recovery_detail(entry, cx);
        }
        let Some(entry) = self
            .selected_history
            .and_then(|id| self.history_entries.iter().find(|entry| entry.id == id))
        else {
            return detail_placeholder(t("Select a dictation."));
        };
        let id = entry.id;
        let copied = self.history_copied == Some(HistoryItem::Dictation(id));
        let action_button = |label: &'static str, id_suffix: &'static str| {
            header_button(label).id(SharedString::from(format!("history-action-{id_suffix}")))
        };
        let report = entry.transcription.as_ref();
        let mut subtitle: Vec<String> = entry.application.iter().cloned().collect();
        subtitle.push(event_age(entry.timestamp_ms));
        div()
            .id(("history-detail", id as usize))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_6()
            .pt_4()
            .pb_6()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    // The detail column is narrow: actions and the subtitle
                    // move to their own lines rather than leaving the panel.
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_baseline()
                                    .gap_x_3()
                                    .child(
                                        div()
                                            .text_size(px(18.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(t("Dictation")),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(rgb(FAINT))
                                            .child(subtitle.join(" · ")),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        action_button(
                                            if copied { t("Copied") } else { t("Copy") },
                                            "copy",
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.copy_history_entry(id, cx)
                                            }),
                                        ),
                                    )
                                    .child({
                                        let delete_armed = self.history_row_delete_armed
                                            == Some(HistoryItem::Dictation(id));
                                        action_button(
                                            if delete_armed {
                                                t("Confirm delete")
                                            } else {
                                                t("Delete")
                                            },
                                            "delete",
                                        )
                                        .when(delete_armed, |button| {
                                            button
                                                .text_color(rgb(NEGATIVE))
                                                .border_color(rgb(NEGATIVE))
                                        })
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                if this.history_row_delete_armed
                                                    == Some(HistoryItem::Dictation(id))
                                                {
                                                    this.history_row_delete_armed = None;
                                                    this.delete_history_entry(id, cx);
                                                } else {
                                                    this.history_row_delete_armed =
                                                        Some(HistoryItem::Dictation(id));
                                                    cx.notify();
                                                }
                                            }),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .mt_5()
                            .pt_5()
                            .border_t_1()
                            .border_color(rgb(crate::desktop_ui::DIVIDER))
                            .child(section_label(t("Text")))
                            .child(
                                div()
                                    .pt_3()
                                    .text_size(px(13.0))
                                    .line_height(px(20.0))
                                    .text_color(rgb(TEXT))
                                    .child(entry.text.clone()),
                            ),
                    )
                    .child(
                        div()
                            .mt_5()
                            .pt_5()
                            .border_t_1()
                            .border_color(rgb(crate::desktop_ui::DIVIDER))
                            .flex()
                            .flex_wrap()
                            .gap_3()
                            .child(history_timing_tile(entry))
                            .child(history_tile(
                                t("Audio sent"),
                                report
                                    .and_then(|report| report.audio_summary())
                                    .unwrap_or_else(|| {
                                        (crate::openrouter::report::seconds(entry.audio_ms), None)
                                    }),
                            ))
                            .child(history_tile(
                                t("Cost (USD)"),
                                report.map_or_else(
                                    || ("—".into(), Some(t("Not recorded").into())),
                                    crate::openrouter::StepReport::cost_tile,
                                ),
                            )),
                    )
                    .children(report.and_then(render_requests_section)),
            )
            .into_any_element()
    }

    pub(super) fn reconcile_recovery_focus(&self, window: &mut Window) {
        let selected = self
            .selected_recovery
            .as_ref()
            .and_then(|id| self.recovery_entries.iter().find(|entry| &entry.id == id));
        if self.recovery_action_focus[1].is_focused(window)
            && selected.is_none_or(|entry| entry.status == RecoveryStatus::Recovered)
        {
            if selected.is_some_and(|entry| entry.text.is_some()) {
                self.recovery_action_focus[0].focus(window);
            } else {
                self.window_focus.focus(window);
            }
        } else if selected.is_none()
            && self
                .recovery_action_focus
                .iter()
                .any(|focus| focus.is_focused(window))
        {
            self.window_focus.focus(window);
        }
    }
}

/// A real saved WAV and failure entry, entirely synthetic and isolated from
/// app services. The preview Retry handler also uses a local fixture.
pub(super) fn preview_recording_recovery() -> Option<RecordingRecovery> {
    let directory =
        std::env::temp_dir().join(format!("hex-recovery-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let store = RecordingRecovery::open(directory).ok()?;
    let samples: Vec<f32> = (0..294_400)
        .map(|index| (index as f32 * std::f32::consts::TAU * 220.0 / 16_000.0).sin() * 0.08)
        .collect();
    let _ = store.transcribe_original(&samples, Some("Codex"), crate::post_processing::Preferences::default(), |_| {
        Err(crate::openrouter::transcribe::ChainFailure {
            failures: vec![
                crate::openrouter::stats::Failure {
                    model: "google::gemini-3.5-transcribe-live".into(),
                    kind: crate::openrouter::stats::ErrorKind::Timeout,
                    detail: "google::gemini-3.5-transcribe-live: network error (exit status: 28): curl: (28) Operation timed out after 30000 milliseconds with 0 bytes received".into(),
                },
                crate::openrouter::stats::Failure {
                    model: "openai/whisper-large-v3-turbo".into(),
                    kind: crate::openrouter::stats::ErrorKind::Server,
                    detail: "openai/whisper-large-v3-turbo: HTTP 503: service unavailable".into(),
                },
            ],
            executions: vec![
                crate::openrouter::report::ExecutionReport {
                    provider: "google".into(),
                    model: "gemini-3.5-transcribe-live".into(),
                    streaming: true,
                    keyword_count: 2,
                    outcome: "failed".into(),
                    error: Some("The API request timed out after 30 seconds. No transcription was returned.".into()),
                    cost_usd: None,
                    estimated_cost_usd: None,
                },
                crate::openrouter::report::ExecutionReport {
                    provider: "openrouter".into(),
                    model: "openai/whisper-large-v3-turbo".into(),
                    streaming: false,
                    keyword_count: 0,
                    outcome: "failed".into(),
                    error: Some("The API returned a server error (HTTP 503). Retry when the service is available.".into()),
                    cost_usd: None,
                    estimated_cost_usd: None,
                },
            ],
        }.into())
    });
    Some(store)
}

/// Deterministic History fixtures for the preview.
pub(super) fn preview_history() -> Option<History> {
    preview_history_in(
        std::env::temp_dir().join(format!("hex-history-preview-{}", std::process::id())),
    )
}

/// The preview History fixtures, stored in `directory`, which is replaced.
pub(super) fn preview_history_in(directory: std::path::PathBuf) -> Option<History> {
    use crate::history::{HistoryDraft, HistoryStore, now_ms};
    use crate::openrouter::{AudioTrim, StepReport};
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).ok()?;
    let mut store = HistoryStore::open(
        directory.join("history.json"),
        HistoryRetention::Week,
        now_ms(),
    );
    let now = now_ms();
    let report = |model: &str, latency_ms, failed: &[&str], recorded_ms, sent_ms| StepReport {
        executions: if model.contains("::") {
            let selected = crate::providers::ModelRef::parse(model);
            vec![crate::openrouter::report::ExecutionReport {
                provider: selected.provider.label().into(),
                model: selected.model.into(),
                streaming: true,
                keyword_count: 3,
                outcome: "success".into(),
                error: None,
                cost_usd: None,
                // A native preview request shows its published-price estimate.
                estimated_cost_usd: Some(0.000_32),
            }]
        } else {
            let selected = crate::providers::ModelRef::parse(model);
            failed
                .iter()
                .map(|id| crate::openrouter::report::ExecutionReport {
                    provider: "OpenRouter".into(),
                    model: (*id).into(),
                    streaming: false,
                    keyword_count: 0,
                    outcome: "failed".into(),
                    error: Some(
                        t("The API rate limit was reached (HTTP 429). Wait before retrying.")
                            .into(),
                    ),
                    cost_usd: None,
                    estimated_cost_usd: None,
                })
                .chain(std::iter::once(
                    crate::openrouter::report::ExecutionReport {
                        provider: selected.provider.label().into(),
                        model: selected.model.into(),
                        streaming: false,
                        keyword_count: 0,
                        outcome: "success".into(),
                        error: None,
                        cost_usd: Some(0.000_123),
                        estimated_cost_usd: None,
                    },
                ))
                .collect()
        },
        omitted_executions: 0,
        model: Some(model.into()),
        latency_ms,
        failed: failed.iter().map(|model| (*model).into()).collect(),
        audio: Some(AudioTrim {
            recorded_ms,
            sent_ms,
        }),
    };
    let fixtures = [
        (
            26 * 60 * 60 * 1_000,
            "The parser needs a retry when the socket drops.",
            Some("Zed"),
            report("openai/whisper-large-v3-turbo", 640, &[], 3_400, 2_600),
        ),
        (
            3 * 60 * 60 * 1_000,
            "Sounds good, shipping it after lunch.",
            Some("Slack"),
            report(
                "openai/gpt-4o-mini-transcribe",
                1_180,
                &["openai/whisper-large-v3-turbo"],
                2_900,
                2_100,
            ),
        ),
        (
            4 * 60 * 1_000,
            "Remember to validate the artifact before publishing the release, and double-check that the notes mention the new model pickers.",
            Some("Messages"),
            report("deepgram::nova-3", 310, &[], 9_400, 9_400),
        ),
    ];
    for (age_ms, text, application, transcription) in fixtures {
        let _ = store.record(
            HistoryDraft {
                text: text.into(),
                application: application.map(Into::into),
                audio_ms: transcription.audio.map_or(0, |audio| audio.recorded_ms),
                inference_ms: transcription.latency_ms,
                total_ms: transcription.latency_ms + 120,
                transcription: Some(transcription),
            },
            now.saturating_sub(age_ms),
        );
    }
    // The preview's failed recording is made as the window opens; one success
    // just after it shows a failure keeping its place in the timeline.
    let _ = store.record(
        HistoryDraft {
            text: "Retrying is fine, this one went through.".into(),
            application: Some("Notes".into()),
            audio_ms: 1_800,
            inference_ms: 420,
            total_ms: 540,
            transcription: Some(report("deepgram::nova-3", 420, &[], 1_800, 1_800)),
        },
        now + 1_000,
    );
    Some(History::new(store))
}

pub(super) fn detail_placeholder(message: &'static str) -> AnyElement {
    div()
        .flex_1()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(rgb(FAINT))
        .child(message)
        .into_any_element()
}

/// Narrower tiles break their labels mid-word and clip their values.
const HISTORY_TILE_MIN_WIDTH: f32 = 140.0;
/// Narrower request text breaks the model and provider mid-word.
const HISTORY_ATTEMPT_TEXT_MIN_WIDTH: f32 = 120.0;

/// A compact figure for the History summary row.
pub(super) fn history_tile(
    label: &'static str,
    (value, detail): (String, Option<String>),
) -> AnyElement {
    crate::desktop_ui::layout_item(div())
        .flex_1()
        .min_w(px(HISTORY_TILE_MIN_WIDTH))
        .px_3()
        .py_3()
        .rounded(px(CONTROL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(FAINT))
                .child(label),
        )
        .child(
            div()
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .truncate()
                .child(value),
        )
        .children(detail.map(|detail| {
            div()
                .text_size(px(11.0))
                .line_height(px(15.0))
                .text_color(rgb(MUTED))
                .child(detail)
        }))
        .into_any_element()
}

/// Release-to-paste time, with how much of it went to transcription.
pub(super) fn history_timing_tile(entry: &HistoryEntry) -> AnyElement {
    use crate::openrouter::report::duration_label;
    let summary = if entry.total_ms > 0 {
        (
            duration_label(entry.total_ms),
            (entry.inference_ms > 0).then(|| {
                tf!(
                    "{duration} transcribing",
                    duration = duration_label(entry.inference_ms)
                )
            }),
        )
    } else if entry.inference_ms > 0 {
        (
            duration_label(entry.inference_ms),
            Some(t("transcribing").to_owned()),
        )
    } else {
        (t("Not recorded").to_owned(), None)
    };
    history_tile(t("Release to paste"), summary)
}

/// Delete on a list row. Turns red after one click and deletes on the second.
fn delete_action(
    id: impl Into<gpui::ElementId>,
    armed: bool,
    armed_count: u64,
) -> gpui::Stateful<Div> {
    use crate::desktop_ui::{ThemeColor, animate_once, icon_button_with, symbol_icon};
    if armed {
        // A small shake says the next click deletes.
        let glyph = animate_once(
            div()
                .relative()
                .child(symbol_icon("trash.fill", ThemeColor::Negative, 13.0)),
            gpui::ElementId::NamedInteger("trash-armed".into(), armed_count),
            360,
            |glyph, progress| {
                let shake = (progress * std::f32::consts::PI * 6.0).sin() * (1.0 - progress);
                glyph.left(px(shake * 2.5))
            },
        );
        icon_button_with(id, t("Click again to delete permanently"), glyph).bg(rgb(SURFACE_HOVER))
    } else {
        icon_button_with(
            id,
            t("Delete"),
            symbol_icon("trash", ThemeColor::Muted, 13.0).into_any_element(),
        )
    }
}

/// Copy on a list row: a symbol, since every row repeats it. A check pops in
/// to confirm and later gives way to the copy symbol again.
fn copy_action(
    id: impl Into<gpui::ElementId>,
    copied: bool,
    copied_count: u64,
) -> gpui::Stateful<Div> {
    use crate::desktop_ui::{ThemeColor, animate_once, icon_button_with, symbol_icon};
    if copied {
        let check = |size: f32| symbol_icon("checkmark", ThemeColor::Positive, size);
        let glyph = animate_once(
            check(13.0),
            gpui::ElementId::NamedInteger("copied-check".into(), copied_count),
            280,
            move |_, progress| check(13.0 * (1.0 + 0.4 * (progress * std::f32::consts::PI).sin())),
        );
        icon_button_with(id, t("Copied"), glyph)
    } else {
        icon_button_with(
            id,
            t("Copy"),
            symbol_icon("doc.on.doc", ThemeColor::TextSoft, 13.0).into_any_element(),
        )
    }
}

/// Fades `element` in when `play`, starting after `delay` of the fade.
fn fade_in_if(play: bool, id: impl Into<gpui::ElementId>, delay: f32, element: Div) -> AnyElement {
    if !play {
        return element.into_any_element();
    }
    crate::desktop_ui::animate_once(element, id, 600, move |element, progress| {
        element.opacity(crate::desktop_ui::ease_out(
            (progress - delay) / (1.0 - delay),
        ))
    })
}

pub(super) fn history_badge(label: &'static str) -> AnyElement {
    div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(rgb(SURFACE_SELECTED))
        .text_size(px(10.0))
        .text_color(rgb(TEXT_SOFT))
        .child(label)
        .into_any_element()
}

fn render_requests_section(report: &crate::openrouter::StepReport) -> Option<AnyElement> {
    let attempts = report.attempts();
    if attempts.is_empty() {
        return None;
    }
    let count = attempts.len();
    Some(
        div()
            .mt_5()
            .pt_5()
            .border_t_1()
            .border_color(rgb(crate::desktop_ui::DIVIDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(section_label(if count == 1 {
                        t("Request")
                    } else {
                        t("Requests")
                    }))
                    .when(count > 1, |title| {
                        title.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(FAINT))
                                .child(count.to_string()),
                        )
                    }),
            )
            .child(
                div().mt_3().flex().flex_col().gap_2().children(
                    attempts
                        .into_iter()
                        .enumerate()
                        .map(|(index, attempt)| history_attempt_row(index + 1, attempt)),
                ),
            )
            .when_some(
                Some(report.omitted_executions).filter(|omitted| *omitted > 0),
                |section, omitted| {
                    section.child(
                        div()
                            .mt_2()
                            .text_size(px(11.0))
                            .text_color(rgb(FAINT))
                            .child(tf!("{omitted} more not kept in History", omitted = omitted)),
                    )
                },
            )
            .into_any_element(),
    )
}

/// One transcription request: what was asked, why, and how it ended.
pub(super) fn history_attempt_row(
    number: usize,
    attempt: crate::openrouter::report::AttemptView,
) -> AnyElement {
    let status_color = if attempt.succeeded {
        POSITIVE
    } else {
        NEGATIVE
    };
    crate::desktop_ui::layout_container(div())
        .px_3()
        .py(px(10.0))
        .rounded(px(CONTROL_RADIUS))
        .border_1()
        .border_color(rgb(LINE))
        .flex()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex_none()
                .mt(px(1.0))
                .size(px(18.0))
                .rounded_full()
                .bg(rgb(SURFACE_SELECTED))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.0))
                .text_color(rgb(TEXT_SOFT))
                .child(number.to_string()),
        )
        .child(
            crate::desktop_ui::layout_item(div())
                .flex_1()
                .min_w(px(HISTORY_ATTEMPT_TEXT_MIN_WIDTH))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(rgb(TEXT))
                                .child(attempt.model.clone()),
                        )
                        .children(attempt.step.label().map(history_badge))
                        .child(
                            div()
                                .ml_auto()
                                .flex_none()
                                .text_size(px(11.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(status_color))
                                .child(if attempt.succeeded {
                                    t("Succeeded")
                                } else {
                                    t("Failed")
                                }),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child(attempt.details()),
                )
                .children(attempt.error.map(|error| {
                    div()
                        .mt(px(2.0))
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .text_color(rgb(NEGATIVE))
                        .child(error)
                }))
                .children(
                    attempt
                        .cost
                        .map(|cost| div().text_size(px(11.0)).text_color(rgb(FAINT)).child(cost)),
                ),
        )
        .into_any_element()
}

pub(super) fn event_age(timestamp_ms: u64) -> String {
    let seconds = crate::events::now_ms().saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => t("now").into(),
        60..=3_599 => tf!("{minutes}m ago", minutes = seconds / 60),
        3_600..=86_399 => tf!("{hours}h ago", hours = seconds / 3_600),
        _ => tf!("{days}d ago", days = seconds / 86_400),
    }
}
