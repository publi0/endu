//! History and recording-recovery lists, details and their actions.

use super::*;

impl AppWindow {
    pub(super) fn history_revisions(&self) -> ((u64, usize), u64) {
        (
            self.history.as_ref().map_or((0, 0), History::view_key),
            self.recovery
                .as_ref()
                .map_or(0, RecordingRecovery::revision),
        )
    }

    pub(super) fn reload_history(&mut self, cx: &App) {
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
            self.recovery_copied = false;
            self.recovery_error = None;
        }
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

    /// Keeps the visible list current while the pane is open.
    pub(super) fn poll_history(&mut self, cx: &App) -> bool {
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
        let Some(entry) = self.history_entries.iter().find(|entry| entry.id == id) else {
            return;
        };
        match arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(entry.text.clone()))
        {
            Ok(()) => {
                self.history_copied = Some(id);
                self.history_error = None;
            }
            Err(error) => self.history_error = Some(error.to_string()),
        }
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

    pub(super) fn clear_history(&mut self, cx: &mut Context<Self>) {
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
        self.recovery_copied = false;
        if let Some(store) = &self.recovery {
            self.recovery_error = (if self.preview {
                store.retry_with_processing(
                    id,
                    self.settings.post_processing,
                    crate::vocabulary::Snapshot::new(self.settings.vocabulary.clone()),
                    |_| {
                        Ok(crate::openrouter::transcribe::Transcription {
                            text: "Recovered preview dictation.".into(),
                            report: None,
                        })
                    },
                )
            } else {
                store.retry(id)
            })
            .err()
            .map(|error| error.to_string());
            self.recovery_delete_armed = false;
            self.reload_history(cx);
            cx.notify();
        }
    }

    pub(super) fn render_recovery_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.recovery_entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let id = entry.id.clone();
                let selected = self.selected_recovery.as_ref() == Some(&id);
                div()
                    .id(("recovery-entry", index))
                    .w_full()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(if entry.status == RecoveryStatus::Recovered {
                                TEXT_SOFT
                            } else {
                                NEGATIVE
                            }))
                            .child(entry.title()),
                    )
                    .child(
                        div()
                            .mt_1()
                            .w_full()
                            .truncate()
                            .text_size(px(10.0))
                            .text_color(rgb(FAINT))
                            .child(format!(
                                "{} · {} audio · {}",
                                entry.application_label(),
                                crate::openrouter::report::seconds(entry.audio_ms),
                                event_age(entry.timestamp_ms)
                            )),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_recovery = Some(id.clone());
                        this.selected_history = None;
                        this.recovery_delete_armed = false;
                        this.recovery_copied = false;
                        this.recovery_error = None;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect()
    }

    pub(super) fn render_recovery_detail(
        &self,
        entry: &RecoveryEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = entry.id.clone();
        let delete_id = id.clone();
        let recovered = entry.status == RecoveryStatus::Recovered;
        let retry_busy = self
            .recovery
            .as_ref()
            .is_some_and(RecordingRecovery::retry_in_progress);
        let can_retry = !entry.busy && !retry_busy && !recovered;
        let mut actions = div().flex().flex_wrap().gap_2();
        if let Some(text) = entry.text.clone() {
            actions = actions.child(
                header_button(if self.recovery_copied {
                    "Copied"
                } else {
                    "Copy text"
                })
                .id("recovery-copy")
                .track_focus(&self.recovery_action_focus[0])
                .focus(|style| style.border_color(rgb(ACCENT)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                    this.recovery_copied = true;
                    this.recovery_error = None;
                    cx.notify();
                })),
            );
        }
        if !recovered {
            actions = actions.child(
                header_button(if entry.busy { "Transcribing" } else { "Retry" })
                    .id("recovery-retry")
                    .track_focus(&self.recovery_action_focus[1].clone().tab_stop(can_retry))
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .when(!can_retry, |button| button.opacity(0.45))
                    .when(can_retry, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.retry_recovery(&id, cx);
                        }))
                    }),
            );
        }
        actions = actions.child(
            header_button(if self.recovery_delete_armed {
                "Delete permanently?"
            } else {
                "Delete"
            })
            .id("recovery-delete")
            .track_focus(&self.recovery_action_focus[2].clone().tab_stop(!entry.busy))
            .focus(|style| style.border_color(rgb(ACCENT)))
            .when(entry.busy, |button| button.opacity(0.45))
            .when(!entry.busy, |button| {
                button.on_click(cx.listener(move |this, _, _, cx| {
                    if !this.recovery_delete_armed {
                        this.recovery_delete_armed = true;
                        cx.notify();
                        return;
                    }
                    if let Some(store) = &this.recovery {
                        this.recovery_error = store
                            .delete(&delete_id)
                            .err()
                            .map(|error| error.to_string());
                        this.recovery_delete_armed = false;
                        this.reload_history(cx);
                        cx.notify();
                    }
                }))
            }),
        );
        div().id("recovery-detail").flex_1().min_w_0().h_full().overflow_y_scroll().px_6().py_6()
            .child(div().text_size(px(18.0)).font_weight(FontWeight::SEMIBOLD).child(entry.title()))
            .child(div().mt_2().text_size(px(11.0)).text_color(rgb(MUTED))
                .child(format!("{} audio · {}", crate::openrouter::report::seconds(entry.audio_ms), event_age(entry.timestamp_ms))))
            .child(div().mt_3().child(detail_row("Application", entry.application_label())))
            .child(div().mt_4().child(actions))
            .children(self.recovery_error.clone().map(|error| div().mt_3()
                .text_size(px(12.0)).text_color(rgb(NEGATIVE)).child(error)))
            .children(entry.message.clone().map(|message| div().mt_5()
                .child(section_label("Failure reason"))
                .child(div().pt_2().text_size(px(12.0)).line_height(px(18.0)).text_color(rgb(NEGATIVE)).child(message))))
            .child(div().mt_5().pt_4().border_t_1().border_color(rgb(LINE)).text_size(px(12.0)).text_color(rgb(TEXT_SOFT))
                .child(if entry.volatile { "This recording is only in memory. Keep Hex open until it is recovered." }
                    else if recovered { "Recovered text is saved locally until you delete it. The temporary audio has been removed." }
                    else { "Audio is saved on this Mac until recovery or deletion. Retry uses your current Models settings and saves the text here, without automatic paste." }))
            .children(entry.text.clone().map(|text| div().mt_5().text_size(px(13.0)).line_height(px(20.0)).child(text)))
            .into_any_element()
    }

    pub(super) fn render_history(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let retention = self.settings.history_retention;
        let search = div().w(px(220.0)).child(self.history_search.clone());
        let retention_control = header_button(format!("Keep: {}", retention.label()))
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
            "Clear text history?"
        } else {
            "Clear dictations"
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
        let mut rows = self.render_recovery_rows(cx);
        rows.extend(
            self.history_entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let id = entry.id;
                    let selected = self.selected_history == Some(id);
                    let report = entry.transcription.as_ref();
                    let mut meta: Vec<String> = entry.application.iter().cloned().collect();
                    if let Some(model) = report.and_then(|report| report.model.as_deref()) {
                        let model = crate::providers::ModelRef::parse(model).model;
                        meta.push(model.rsplit('/').next().unwrap_or(model).to_owned());
                    }
                    if entry.total_ms > 0 {
                        meta.push(crate::openrouter::report::duration_label(entry.total_ms));
                    }
                    let badge = report.and_then(crate::openrouter::StepReport::recovery_badge);
                    let meta = meta.join(" · ");
                    div()
                        .id(("history-entry", index))
                        .w_full()
                        .px_4()
                        .py_3()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_4()
                        .border_b_1()
                        .border_color(rgb(LINE))
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
                                        .text_color(rgb(TEXT_SOFT))
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
                                .flex()
                                .flex_col()
                                .items_end()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(FAINT))
                                        .child(event_age(entry.timestamp_ms)),
                                )
                                .children(badge.map(history_badge)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected_history = Some(id);
                            this.selected_recovery = None;
                            this.recovery_delete_armed = false;
                            this.recovery_error = None;
                            this.history_clear_armed = false;
                            cx.notify();
                        }))
                        .into_any_element()
                }),
        );
        let retention_off = retention.is_off();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header_with_action("History", Some(header_action)))
            .children(self.recovery.as_ref().and_then(RecordingRecovery::load_warning).map(|warning|
                div().px_5().py_2().text_size(px(12.0)).text_color(rgb(NEGATIVE)).child(warning.to_owned())))
            .child(
                pane_body().p_5().child(
                    pane_content()
                        .flex_row()
                        .gap_5()
                        .child(
                            pane_list(
                                "history-list",
                                if retention_off && self.recovery_entries.is_empty() {
                                    Some("History is off. Failed recordings are still kept for recovery.")
                                } else if self.history_entries.is_empty() && self.recovery_entries.is_empty()
                                    && self.history_error.is_none()
                                {
                                    Some("No dictations retained yet.")
                                } else {
                                    None
                                },
                                "History could not be loaded.",
                                self.history_error.clone(),
                            )
                            .rounded(px(PANEL_RADIUS))
                            .border_1()
                            .border_color(rgb(LINE))
                            .bg(rgb(SURFACE))
                            .overflow_x_hidden()
                            .child(
                                div()
                                    .w(px(PANE_LIST_WIDTH - 2.0))
                                    .flex()
                                    .flex_col()
                                    .children(rows),
                            ),
                        )
                        .child(
                            compact_panel()
                                .flex_1()
                                .min_w(px(0.0))
                                .h_full()
                                .flex()
                                .flex_col()
                                .child(self.render_history_detail(cx)),
                        ),
                ),
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
            return detail_placeholder("Select a dictation.");
        };
        let id = entry.id;
        let copied = self.history_copied == Some(id);
        let action_button = |label: &'static str, id_suffix: &'static str| {
            div()
                .id(SharedString::from(format!("history-action-{id_suffix}")))
                .h(px(30.0))
                .px_3()
                .flex()
                .items_center()
                .rounded_sm()
                .bg(rgb(SURFACE))
                .text_size(px(12.0))
                .text_color(rgb(TEXT_SOFT))
                .hover(|button| button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT)))
                .child(label)
        };
        let report = entry.transcription.as_ref();
        let mut subtitle: Vec<String> = entry.application.iter().cloned().collect();
        subtitle.push(event_age(entry.timestamp_ms));
        div()
            .id("history-detail")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_y_scroll()
            .px_6()
            .py_6()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_size(px(18.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Dictation"),
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
                                            if copied { "Copied" } else { "Copy" },
                                            "copy",
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.copy_history_entry(id, cx)
                                            }),
                                        ),
                                    )
                                    .child(action_button("Delete", "delete").on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.delete_history_entry(id, cx)
                                        }),
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .mt_5()
                            .pt_5()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .child(section_label("Text"))
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
                            .border_color(rgb(LINE))
                            .flex()
                            .gap_3()
                            .child(history_timing_tile(entry))
                            .child(history_tile(
                                "Audio sent",
                                report
                                    .and_then(|report| report.audio_summary())
                                    .unwrap_or_else(|| {
                                        (crate::openrouter::report::seconds(entry.audio_ms), None)
                                    }),
                            ))
                            .child(history_tile("Cost (USD)", {
                                let summary = report.map_or_else(
                                    || "Not recorded".to_owned(),
                                    |report| report.cost_summary(),
                                );
                                match summary.split_once(" · ") {
                                    // The tile label names the currency.
                                    Some((value, detail)) => (
                                        value.trim_end_matches(" USD").to_owned(),
                                        Some(detail.to_owned()),
                                    ),
                                    None => (summary.trim_end_matches(" USD").to_owned(), None),
                                }
                            })),
                    )
                    .children(
                        report
                            .map(|report| (report, report.attempts()))
                            .filter(|(_, attempts)| !attempts.is_empty())
                            .map(|(report, attempts)| {
                                let count = attempts.len();
                                div()
                                    .mt_5()
                                    .pt_5()
                                    .border_t_1()
                                    .border_color(rgb(LINE))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(section_label(if count == 1 {
                                                "Request"
                                            } else {
                                                "Requests"
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
                                    .child(div().mt_3().flex().flex_col().gap_2().children(
                                        attempts.into_iter().enumerate().map(|(index, attempt)| {
                                            history_attempt_row(index + 1, attempt)
                                        }),
                                    ))
                                    .when_some(
                                        Some(report.omitted_executions)
                                            .filter(|omitted| *omitted > 0),
                                        |section, omitted| {
                                            section.child(
                                                div()
                                                    .mt_2()
                                                    .text_size(px(11.0))
                                                    .text_color(rgb(FAINT))
                                                    .child(format!(
                                                        "{omitted} more not kept in History"
                                                    )),
                                            )
                                        },
                                    )
                            }),
                    ),
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
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_PREVIEW: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "hex-recovery-preview-{}-{}",
        std::process::id(),
        NEXT_PREVIEW.fetch_add(1, Ordering::Relaxed),
    ));
    let store = RecordingRecovery::open(directory).ok()?;
    let samples: Vec<f32> = (0..294_400)
        .map(|index| (index as f32 * std::f32::consts::TAU * 220.0 / 16_000.0).sin() * 0.08)
        .collect();
    let _ = store.transcribe_original(&samples, Some("Codex"), crate::post_processing::Preferences::default(), |_| {
        Err(crate::openrouter::transcribe::ChainFailure { failures: vec![crate::openrouter::stats::Failure {
            model: "openai/whisper-large-v3-turbo".into(),
            kind: crate::openrouter::stats::ErrorKind::Timeout,
            detail: "openai/whisper-large-v3-turbo: network error (exit status: 28): curl: (28) Operation timed out after 30000 milliseconds with 0 bytes received".into(),
        }] }.into())
    });
    Some(store)
}

/// Deterministic History fixtures for the preview.
pub(super) fn preview_history() -> Option<History> {
    use crate::history::{HistoryDraft, HistoryStore, now_ms};
    use crate::openrouter::{AudioTrim, StepReport};
    let directory =
        std::env::temp_dir().join(format!("hex-history-preview-{}", std::process::id()));
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
                cost_usd: None,
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
                    cost_usd: None,
                })
                .chain(std::iter::once(
                    crate::openrouter::report::ExecutionReport {
                        provider: selected.provider.label().into(),
                        model: selected.model.into(),
                        streaming: false,
                        keyword_count: 0,
                        outcome: "success".into(),
                        cost_usd: Some(0.000_123),
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

pub(super) fn detail_row(label: &'static str, value: impl Into<String>) -> AnyElement {
    div()
        .py_3()
        .flex()
        .items_start()
        .gap_4()
        .border_b_1()
        .border_color(rgb(LINE))
        .child(
            div()
                .w(px(104.0))
                .flex_none()
                .text_size(px(11.0))
                .text_color(rgb(FAINT))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(rgb(TEXT_SOFT))
                .child(value.into()),
        )
        .into_any_element()
}

/// A compact figure for the History summary row.
pub(super) fn history_tile(
    label: &'static str,
    (value, detail): (String, Option<String>),
) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
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
            (entry.inference_ms > 0)
                .then(|| format!("{} transcribing", duration_label(entry.inference_ms))),
        )
    } else if entry.inference_ms > 0 {
        (
            duration_label(entry.inference_ms),
            Some("transcribing".to_owned()),
        )
    } else {
        ("Not recorded".to_owned(), None)
    };
    history_tile("Release to paste", summary)
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
    div()
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
            div()
                .flex_1()
                .min_w_0()
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
                        .children(attempt.step.label().map(history_badge)),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child(attempt.details()),
                ),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .items_end()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(status_color))
                        .child(if attempt.succeeded {
                            "Succeeded"
                        } else {
                            "Failed"
                        }),
                )
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
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}
