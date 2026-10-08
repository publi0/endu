//! An editor for Automatic routing. The parent owns persistence and sends the
//! saved preferences back after each PriorityChange, including any save error.

use gpui::{
    AnyElement, ClickEvent, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent,
    MouseDownEvent, Render, Window, div, prelude::*, px, rgb,
};

use crate::desktop_ui::{
    ACCENT, LINE, MUTED, NEGATIVE, PickerState, SURFACE, SURFACE_HOVER, SURFACE_SELECTED, TEXT,
    TEXT_SOFT, compact_button, picker_open_key, picker_popup, settings_row,
};
use crate::microphone::DevicePreference;

pub struct PriorityChange(pub Vec<DevicePreference>);

#[derive(Clone, Copy)]
enum FocusTarget {
    Add,
    Row(usize, usize),
}

pub struct MicrophonePriorityView {
    preferences: Vec<DevicePreference>,
    preview: bool,
    catalog: Option<Vec<DevicePreference>>,
    loading: bool,
    catalog_error: Option<String>,
    save_error: Option<String>,
    pending: Option<FocusTarget>,
    restore_focus: Option<FocusTarget>,
    row_focus: Vec<[FocusHandle; 3]>,
    picker_open: bool,
    picker: PickerState,
}

impl EventEmitter<PriorityChange> for MicrophonePriorityView {}

impl MicrophonePriorityView {
    pub fn new(preferences: Vec<DevicePreference>, preview: bool, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            row_focus: row_focus(preferences.len(), cx),
            preferences,
            preview,
            catalog: None,
            loading: false,
            catalog_error: None,
            save_error: None,
            pending: None,
            restore_focus: None,
            picker_open: false,
            picker: PickerState::new(cx),
        };
        view.refresh_catalog(cx);
        view
    }

    /// Acknowledge a proposed edit with the actually persisted list. A failed
    /// save keeps both the previous ordering and the open menu available.
    pub fn set_preferences(
        &mut self,
        preferences: Vec<DevicePreference>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let pending = self.pending.take();
        if self.preferences != preferences {
            self.row_focus = row_focus(preferences.len(), cx);
        }
        self.preferences = preferences;
        self.save_error = error;
        if self.save_error.is_none() {
            self.restore_focus = pending;
            if matches!(pending, Some(FocusTarget::Add)) {
                self.picker_open = false;
            }
        }
        self.picker.highlight = self
            .picker
            .highlight
            .min(self.choices().len().saturating_sub(1));
        cx.notify();
    }

    pub fn close_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = false;
        self.restore_focus = None;
        cx.notify();
    }

    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        if self.preview {
            self.catalog = Some(preview_catalog());
            return;
        }
        self.loading = true;
        self.catalog_error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async {
                    crate::audio::input_device_catalog().map_err(|error| format!("{error:#}"))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(catalog) => {
                        this.catalog = Some(catalog);
                        this.catalog_error = None;
                    }
                    Err(error) => this.catalog_error = Some(error),
                }
                this.picker.highlight = this
                    .picker
                    .highlight
                    .min(this.choices().len().saturating_sub(1));
                this.picker.scroll.scroll_to_item(this.picker.highlight);
                cx.notify();
            });
        })
        .detach();
    }

    fn choices(&self) -> Vec<DevicePreference> {
        self.catalog
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|device| {
                !self
                    .preferences
                    .iter()
                    .any(|preference| preference.matches(device))
            })
            .cloned()
            .collect()
    }

    fn submit(
        &mut self,
        preferences: Vec<DevicePreference>,
        focus: FocusTarget,
        cx: &mut Context<Self>,
    ) {
        if self.pending.is_some() || preferences == self.preferences {
            return;
        }
        self.pending = Some(focus);
        self.save_error = None;
        cx.emit(PriorityChange(preferences));
        cx.notify();
    }

    fn add(&mut self, device: DevicePreference, cx: &mut Context<Self>) {
        if self.loading
            || self
                .preferences
                .iter()
                .any(|preference| preference.matches(&device))
        {
            return;
        }
        let mut preferences = self.preferences.clone();
        preferences.push(device);
        self.submit(preferences, FocusTarget::Add, cx);
    }

    fn change_row(&mut self, index: usize, action: usize, cx: &mut Context<Self>) {
        if index >= self.preferences.len() {
            return;
        }
        let mut preferences = self.preferences.clone();
        let focus = match action {
            0 if index > 0 => {
                preferences.swap(index, index - 1);
                FocusTarget::Row(index - 1, action)
            }
            1 if index + 1 < preferences.len() => {
                preferences.swap(index, index + 1);
                FocusTarget::Row(index + 1, action)
            }
            2 => {
                preferences.remove(index);
                if preferences.is_empty() {
                    FocusTarget::Add
                } else {
                    FocusTarget::Row(index.min(preferences.len() - 1), action)
                }
            }
            _ => return,
        };
        self.submit(preferences, focus, cx);
    }

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        self.picker_open = !self.picker_open;
        if self.picker_open {
            self.refresh_catalog(cx);
            self.picker.open(0, self.choices().len(), window);
        } else {
            self.picker.trigger.focus(window);
        }
        cx.notify();
    }

    fn picker_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let choices = self.choices();
        let key = event.keystroke.key.as_str();
        if self.picker.navigate(key, choices.len()) {
        } else if matches!(key, "enter" | "space") {
            if let Some(device) = choices.get(self.picker.highlight) {
                self.add(device.clone(), cx);
            }
        } else if matches!(key, "escape" | "tab") {
            self.picker_open = false;
            self.picker.close(event, window);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn render_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.choices();
        let message = if self.loading {
            Some("Looking for microphones…")
        } else if choices.is_empty() && self.catalog_error.is_none() {
            Some("No more connected microphones to add.")
        } else {
            None
        };
        div()
            .id("microphone-priority-picker")
            .track_focus(&self.picker.menu)
            .w(px(280.0))
            .max_h(px(320.0))
            .p_2()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(SURFACE))
            .shadow_lg()
            .occlude()
            .on_key_down(cx.listener(Self::picker_key))
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_picker(cx)))
            .children(message.map(note))
            .child(
                div()
                    .id("microphone-priority-choices")
                    .min_h_0()
                    .max_h(px(240.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.picker.scroll)
                    .flex()
                    .flex_col()
                    .children(choices.into_iter().enumerate().map(|(index, device)| {
                        div()
                            .id(("microphone-priority-choice", index))
                            .min_h(px(34.0))
                            .flex_none()
                            .px_3()
                            .py_2()
                            .rounded_sm()
                            .text_size(px(12.0))
                            .text_color(rgb(TEXT_SOFT))
                            .when(index == self.picker.highlight, |row| {
                                row.bg(rgb(SURFACE_SELECTED))
                            })
                            .when(self.loading || self.pending.is_some(), |row| {
                                row.opacity(0.45)
                            })
                            .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                            .child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .whitespace_normal()
                                    .child(device.name.clone()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.add(device.clone(), cx);
                            }))
                    })),
            )
            .children(
                self.save_error
                    .as_ref()
                    .or(self.catalog_error.as_ref())
                    .map(|error| error_note(error.clone())),
            )
            .into_any_element()
    }

    fn row_button(
        &self,
        index: usize,
        action: usize,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        const IDS: [&str; 3] = [
            "microphone-priority-up",
            "microphone-priority-down",
            "microphone-priority-remove",
        ];
        let enabled = self.pending.is_none()
            && match action {
                0 => index > 0,
                1 => index + 1 < self.preferences.len(),
                _ => true,
            };
        compact_button(label)
            .id((IDS[action], index))
            .track_focus(&self.row_focus[index][action].clone().tab_stop(enabled))
            .border_1()
            .border_color(rgb(LINE))
            .focus(|style| style.border_color(rgb(ACCENT)))
            .when(!enabled, |button| button.opacity(0.4))
            .on_click(cx.listener(move |this, _, _, cx| {
                if enabled {
                    this.change_row(index, action, cx);
                }
            }))
    }
}

impl Render for MicrophonePriorityView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(target) = self.restore_focus.take() {
            match target {
                FocusTarget::Row(index, action) if index < self.row_focus.len() => {
                    // Keep the move button focused at a boundary. Moving to
                    // Remove here would turn a repeated Enter into deletion.
                    self.row_focus[index][action].focus(window);
                }
                _ => self.picker.trigger.focus(window),
            }
        }
        let add = div()
            .relative()
            .flex_none()
            .child(
                compact_button("Add microphone")
                    .id("microphone-priority-add")
                    .track_focus(&self.picker.trigger.clone().tab_stop(self.pending.is_none()))
                    .border_1()
                    .border_color(rgb(LINE))
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .when(self.pending.is_some(), |button| button.opacity(0.45))
                    .on_click(cx.listener(|this, event, window, cx| {
                        if matches!(event, ClickEvent::Mouse(_)) {
                            this.toggle_picker(window, cx);
                        }
                    }))
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        if picker_open_key(event) {
                            this.toggle_picker(window, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
            .children(
                self.picker_open
                    .then(|| picker_popup(self.render_picker(cx))),
            );
        div()
            .w_full()
            .min_w_0()
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
            .child(settings_row(
                "Automatic priority",
                "Used when Input device is Automatic",
                add,
            ))
            .children(
                self.preferences
                    .iter()
                    .enumerate()
                    .map(|(index, preference)| {
                        let unavailable = self.catalog.as_ref().is_some_and(|catalog| {
                            !catalog.iter().any(|device| preference.matches(device))
                        });
                        div()
                            .px_4()
                            .py_2()
                            .flex()
                            .items_center()
                            .gap_3()
                            .border_b_1()
                            .border_color(rgb(LINE))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.0))
                                    .text_color(rgb(MUTED))
                                    .child(format!("{}.", index + 1)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.0))
                                    .text_color(rgb(TEXT))
                                    .child(div().truncate().child(preference.name.clone()))
                                    .when(unavailable, |row| {
                                        row.child(
                                            div()
                                                .text_size(px(10.0))
                                                .text_color(rgb(MUTED))
                                                .child("Disconnected — kept in this order"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .gap_1()
                                    .child(self.row_button(index, 0, "↑", cx))
                                    .child(self.row_button(index, 1, "↓", cx))
                                    .child(self.row_button(index, 2, "Remove", cx)),
                            )
                    }),
            )
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child(self.priority_note()),
            )
            .when(!self.picker_open, |panel| {
                panel.children(
                    self.save_error
                        .as_ref()
                        .map(|error| error_note(error.clone())),
                )
            })
    }
}

impl MicrophonePriorityView {
    fn priority_note(&self) -> String {
        if !self.preferences.is_empty() {
            return "Unavailable microphones are skipped. The system default is tried after this list.".into();
        }
        let connected = self
            .catalog
            .as_deref()
            .map(crate::audio::legacy_automatic_preferences)
            .unwrap_or_default();
        if connected.is_empty() {
            "Using the system default microphone. Add microphones to choose your own order.".into()
        } else {
            let names = connected
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("Using {names} when connected, then the system default.")
        }
    }
}

fn row_focus(count: usize, cx: &gpui::App) -> Vec<[FocusHandle; 3]> {
    (0..count)
        .map(|_| std::array::from_fn(|_| cx.focus_handle().tab_stop(true)))
        .collect()
}

fn note(message: &'static str) -> AnyElement {
    div()
        .px_3()
        .py_2()
        .text_size(px(11.0))
        .text_color(rgb(MUTED))
        .child(message)
        .into_any_element()
}

fn error_note(message: String) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .flex_none()
        .px_3()
        .py_2()
        .whitespace_normal()
        .text_size(px(11.0))
        .text_color(rgb(NEGATIVE))
        .child(message)
        .into_any_element()
}

fn preview_catalog() -> Vec<DevicePreference> {
    [
        ("preview-usb", "USB microphone"),
        ("preview-display", "Studio Display Microphone"),
        ("preview-input", "Built-in Microphone"),
    ]
    .into_iter()
    .map(|(id, name)| DevicePreference {
        id: Some(id.into()),
        name: name.into(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn adding_and_removing_by_keyboard_waits_for_save_and_does_not_reopen_on_key_up(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) =
            cx.add_window_view(|_, cx| MicrophonePriorityView::new(Vec::new(), true, cx));
        cx.update(|window, cx| {
            cx.subscribe(&view, |view, change: &PriorityChange, cx| {
                view.update(cx, |view, cx| {
                    view.set_preferences(change.0.clone(), None, cx)
                });
            })
            .detach();
            view.read(cx).picker.trigger.focus(window);
        });
        cx.simulate_keystrokes("enter down enter");
        cx.run_until_parked();
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(view.preferences, [preview_catalog()[1].clone()]);
            assert!(!view.picker_open);
            assert!(view.pending.is_none());
            assert!(view.picker.trigger.is_focused(window));
        });
        cx.simulate_keystrokes("tab");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("space").unwrap(),
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(view.preferences.is_empty());
            assert!(view.picker.trigger.is_focused(window));
        });
    }

    #[gpui::test]
    fn a_failed_add_keeps_the_menu_focus_and_previous_preferences(cx: &mut gpui::TestAppContext) {
        let (view, cx) =
            cx.add_window_view(|_, cx| MicrophonePriorityView::new(Vec::new(), true, cx));
        cx.update(|window, cx| {
            cx.subscribe(&view, |view, _: &PriorityChange, cx| {
                view.update(cx, |view, cx| {
                    view.set_preferences(
                        view.preferences.clone(),
                        Some("Could not save preferences.".into()),
                        cx,
                    )
                });
            })
            .detach();
            view.read(cx).picker.trigger.focus(window);
        });
        cx.simulate_keystrokes("enter enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(view.preferences.is_empty());
            assert!(view.picker_open);
            assert!(view.picker.menu.is_focused(window));
            assert!(view.save_error.is_some());
            assert!(view.pending.is_none());
        });
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(!view.picker_open);
            assert!(view.picker.trigger.is_focused(window));
            assert!(view.save_error.is_some());
        });
    }

    #[gpui::test]
    fn reordering_preserves_disconnected_device_ids_and_duplicates_are_filtered(
        cx: &mut gpui::TestAppContext,
    ) {
        let missing = DevicePreference {
            id: Some("disconnected-uid".into()),
            name: "Saved microphone".into(),
        };
        let initial = vec![missing.clone(), preview_catalog()[0].clone()];
        let (view, cx) =
            cx.add_window_view(move |_, cx| MicrophonePriorityView::new(initial, true, cx));
        cx.update(|window, cx| {
            cx.subscribe(&view, |view, change: &PriorityChange, cx| {
                view.update(cx, |view, cx| {
                    view.set_preferences(change.0.clone(), None, cx)
                });
            })
            .detach();
            view.read(cx).row_focus[1][0].focus(window);
        });
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.preferences, [preview_catalog()[0].clone(), missing]);
            assert!(!view.choices().contains(&preview_catalog()[0]));
            assert!(!view.loading);
        });
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(view.preferences.len(), 2);
            assert!(view.row_focus[0][0].is_focused(window));
        });
    }
}
