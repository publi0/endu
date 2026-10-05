//! HUD controls. The owner persists changes and sends the accepted snapshot back.

use std::rc::Rc;

use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    KeyDownEvent, Render, Subscription, Window, div, prelude::*, px, rgb,
};

use crate::desktop_ui::{
    ACCENT, LINE, MUTED, NEGATIVE, PANE_CONTENT_WIDTH, PickerState, SURFACE, SURFACE_HOVER,
    SURFACE_SELECTED, TEXT, TEXT_SOFT, compact_button, disclosure_button, pane_header,
    picker_open_key, picker_popup, settings_panel, settings_row, settings_section_label,
};
use crate::hud_screen::{self, MonitorChoice};
use crate::hud_settings::{
    HudBrightness, HudColor, HudPosition, HudPreferences, HudScreen, HudSize, MAX_EDGE_DISTANCE,
    MIN_EDGE_DISTANCE, MonitorId,
};
use crate::text_input::{Changed, Submitted, TextInput};

#[derive(Clone, Copy, Debug)]
pub struct HudChange {
    pub preferences: HudPreferences,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Section {
    Position,
    Size,
    Brightness,
    Screen,
    Distance,
    Recording,
    Transcription,
}

pub struct HudSettingsView {
    preferences: HudPreferences,
    preview: bool,
    pending_section: Option<Section>,
    error: Option<(Section, String)>,
    distance: Entity<TextInput>,
    monitors: Vec<MonitorChoice>,
    monitor_picker: PickerState,
    monitor_open: bool,
    position_focus: [FocusHandle; 2],
    size_focus: [FocusHandle; 3],
    brightness_focus: [FocusHandle; 3],
    screen_focus: [FocusHandle; 2],
    color_focus: [[FocusHandle; 6]; 2],
    apply_focus: FocusHandle,
    refresh_focus: FocusHandle,
    distance_submit: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<HudChange> for HudSettingsView {}

impl HudSettingsView {
    pub fn new(preferences: HudPreferences, preview: bool, cx: &mut Context<Self>) -> Self {
        let preferences = preferences.normalized();
        let distance = cx.new(|cx| TextInput::new(cx, "12", preferences.edge_distance.to_string()));
        let subscriptions = vec![cx.subscribe(&distance, |this, _, _: &Changed, cx| {
            if this
                .error
                .as_ref()
                .is_some_and(|(scope, _)| *scope == Section::Distance)
            {
                this.error = None;
            }
            cx.notify();
        })];
        Self {
            preferences,
            preview,
            pending_section: None,
            error: None,
            distance,
            monitors: monitor_choices(preview),
            monitor_picker: PickerState::new(cx),
            monitor_open: false,
            position_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            size_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            brightness_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            screen_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            color_focus: std::array::from_fn(|_| {
                std::array::from_fn(|_| cx.focus_handle().tab_stop(true))
            }),
            apply_focus: cx.focus_handle().tab_stop(true),
            refresh_focus: cx.focus_handle().tab_stop(true),
            distance_submit: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_preferences(
        &mut self,
        preferences: HudPreferences,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let preferences = preferences.normalized();
        let pending = self.pending_section.take();
        if error.is_none()
            && (pending.is_none()
                || pending == Some(Section::Distance)
                || preferences.edge_distance != self.preferences.edge_distance)
        {
            self.distance.update(cx, |input, cx| {
                input.set_text(preferences.edge_distance.to_string(), cx)
            });
        }
        self.preferences = preferences;
        self.error = error.map(|message| (pending.unwrap_or(Section::Position), message));
        cx.notify();
    }

    /// The pane owner moves focus to the newly selected pane after closing.
    pub fn close_picker(&mut self, cx: &mut Context<Self>) {
        if self.monitor_open {
            self.monitor_open = false;
            cx.notify();
        }
    }

    fn change(&mut self, section: Section, preferences: HudPreferences, cx: &mut Context<Self>) {
        self.pending_section = Some(section);
        self.error = None;
        cx.emit(HudChange {
            preferences: preferences.normalized(),
        });
        cx.notify();
    }

    fn apply_distance(&mut self, cx: &mut Context<Self>) {
        match parse_distance(self.distance.read(cx).text()) {
            Ok(edge_distance) => self.change(
                Section::Distance,
                HudPreferences {
                    edge_distance,
                    ..self.preferences
                },
                cx,
            ),
            Err(error) => {
                self.error = Some((Section::Distance, error.into()));
                cx.notify();
            }
        }
    }

    fn refresh_monitors(&mut self) {
        self.monitors = monitor_choices(self.preview);
    }

    fn toggle_monitors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.monitor_open {
            self.monitor_open = false;
            self.monitor_picker.trigger.focus(window);
        } else {
            self.refresh_monitors();
            if self.monitors.is_empty() {
                self.error = Some((
                    Section::Screen,
                    "No connected monitors are available.".into(),
                ));
                cx.notify();
                return;
            }
            let selected = self
                .monitors
                .iter()
                .position(|choice| Some(choice.id) == self.preferences.fixed_monitor)
                .unwrap_or(0);
            self.monitor_open = true;
            self.monitor_picker
                .open(selected, self.monitors.len(), window);
        }
        cx.notify();
    }

    fn choose_monitor(&mut self, id: MonitorId, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_monitors();
        if !self.monitors.iter().any(|choice| choice.id == id) {
            self.error = Some((
                Section::Screen,
                "That monitor disconnected. Choose another monitor.".into(),
            ));
            self.monitor_open = false;
            self.monitor_picker.trigger.focus(window);
            cx.notify();
            return;
        }
        self.monitor_open = false;
        self.monitor_picker.trigger.focus(window);
        self.change(
            Section::Screen,
            HudPreferences {
                screen: HudScreen::FixedMonitor,
                fixed_monitor: Some(id),
                ..self.preferences
            },
            cx,
        );
    }

    fn monitor_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if self.monitor_picker.navigate(key, self.monitors.len()) {
            cx.stop_propagation();
            cx.notify();
        } else if matches!(key, "escape" | "tab") {
            self.monitor_open = false;
            self.monitor_picker.close(event, window);
            cx.stop_propagation();
            cx.notify();
        } else if matches!(key, "enter" | "space") {
            if let Some(choice) = self.monitors.get(self.monitor_picker.highlight) {
                self.choose_monitor(choice.id, window, cx);
            }
            cx.stop_propagation();
        }
    }

    fn choices<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        choices: &[T],
        selection: (T, &[FocusHandle]),
        label: impl Fn(T) -> &'static str,
        change: impl Fn(&mut Self, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (selected, focus) = selection;
        let change = Rc::new(change);
        div()
            .flex_none()
            .flex()
            .gap_2()
            .children(choices.iter().enumerate().map(|(index, &value)| {
                let change = change.clone();
                compact_button(label(value))
                    .id((id, index))
                    .debug_selector(move || format!("{id}-{index}"))
                    .track_focus(&focus[index])
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .border_1()
                    .border_color(rgb(LINE))
                    .h(px(32.0))
                    .when(value == selected, |button| {
                        button.bg(rgb(SURFACE_SELECTED)).text_color(rgb(TEXT))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| change(this, value, cx)))
            }))
            .into_any_element()
    }

    fn palette(&self, recording: bool, cx: &mut Context<Self>) -> AnyElement {
        let group = usize::from(!recording);
        let id = if recording {
            "hud-recording-color"
        } else {
            "hud-transcription-color"
        };
        let selected = if recording {
            self.preferences.recording_color
        } else {
            self.preferences.transcription_color
        };
        div()
            .w(px(304.0))
            .flex_none()
            .flex()
            .flex_wrap()
            .gap_2()
            .children(HudColor::ALL.into_iter().enumerate().map(|(index, color)| {
                compact_button("")
                    .id((id, index))
                    .debug_selector(move || format!("{id}-{index}"))
                    .track_focus(&self.color_focus[group][index])
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .border_1()
                    .border_color(rgb(LINE))
                    .w(px(96.0))
                    .h(px(32.0))
                    .gap(px(6.0))
                    .when(color == selected, |button| {
                        button.bg(rgb(SURFACE_SELECTED)).text_color(rgb(TEXT))
                    })
                    .child(
                        div()
                            .size(px(10.0))
                            .flex_none()
                            .rounded_full()
                            .bg(rgb(color.swatch())),
                    )
                    .child(color.label())
                    .when(color == selected, |button| {
                        button.child(div().text_size(px(10.0)).child("✓"))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let mut preferences = this.preferences;
                        if recording {
                            preferences.recording_color = color;
                        } else {
                            preferences.transcription_color = color;
                        }
                        this.change(
                            if recording {
                                Section::Recording
                            } else {
                                Section::Transcription
                            },
                            preferences,
                            cx,
                        );
                    }))
            }))
            .into_any_element()
    }

    fn screen_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.choices(
            "hud-screen",
            &HudScreen::ALL[..2],
            (self.preferences.screen, &self.screen_focus),
            HudScreen::label,
            |this, screen, cx| {
                this.change(
                    Section::Screen,
                    HudPreferences {
                        screen,
                        ..this.preferences
                    },
                    cx,
                )
            },
            cx,
        );
        let selected = self.preferences.screen == HudScreen::FixedMonitor;
        let label = self
            .preferences
            .fixed_monitor
            .and_then(|id| self.monitors.iter().find(|choice| choice.id == id))
            .map(|choice| format!("Fixed: {}", choice.name))
            .unwrap_or_else(|| "Choose fixed monitor".into());
        let popup = self.monitor_open.then(|| {
            let rows = self.monitors.iter().enumerate().map(|(index, choice)| {
                let id = choice.id;
                div()
                    .id(("hud-monitor-choice", index))
                    .h(px(34.0))
                    .px_3()
                    .flex_none()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .text_size(px(12.0))
                    .text_color(rgb(TEXT_SOFT))
                    .when(index == self.monitor_picker.highlight, |row| {
                        row.bg(rgb(SURFACE_SELECTED))
                    })
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    .child(div().truncate().child(choice.name.clone()))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.choose_monitor(id, window, cx)),
                    )
            });
            div()
                .id("hud-monitor-menu")
                .debug_selector(|| "hud-monitor-menu".into())
                .track_focus(&self.monitor_picker.menu)
                .w(px(300.0))
                .p_2()
                .rounded_sm()
                .border_1()
                .border_color(rgb(LINE))
                .bg(rgb(SURFACE))
                .shadow_lg()
                .occlude()
                .on_key_down(cx.listener(Self::monitor_keys))
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.monitor_open = false;
                    this.monitor_picker.trigger.focus(window);
                    cx.notify();
                }))
                .child(
                    div()
                        .id("hud-monitor-list")
                        .max_h(px(240.0))
                        .overflow_y_scroll()
                        .track_scroll(&self.monitor_picker.scroll)
                        .flex()
                        .flex_col()
                        .children(rows),
                )
        });
        div()
            .flex_none()
            .flex()
            .flex_col()
            .items_end()
            .gap_2()
            .child(choices)
            .child(
                div()
                    .relative()
                    .child(
                        disclosure_button(label)
                            .id("hud-monitor-trigger")
                            .debug_selector(|| "hud-monitor-trigger".into())
                            .track_focus(&self.monitor_picker.trigger)
                            .focus(|style| style.border_color(rgb(ACCENT)))
                            .when(selected, |button| {
                                button.bg(rgb(SURFACE_SELECTED)).text_color(rgb(TEXT))
                            })
                            .on_key_down(cx.listener(|this, event, window, cx| {
                                if picker_open_key(event) {
                                    this.toggle_monitors(window, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                                if !matches!(event, ClickEvent::Keyboard(_)) {
                                    this.toggle_monitors(window, cx);
                                }
                            })),
                    )
                    .children(popup.map(picker_popup)),
            )
            .child(
                compact_button("Refresh monitors")
                    .id("hud-monitor-refresh")
                    .track_focus(&self.refresh_focus)
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_monitors();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn row(
        &self,
        section: Section,
        title: &'static str,
        description: impl Into<gpui::SharedString>,
        control: impl IntoElement,
    ) -> Div {
        let error = self
            .error
            .as_ref()
            .filter(|(scope, _)| *scope == section)
            .map(|(_, message)| message.clone());
        div()
            .border_b_1()
            .border_color(rgb(LINE))
            .child(settings_row(title, description, control).border_b_0())
            .when_some(error, |row, error| {
                row.child(
                    div()
                        .px_4()
                        .pb_3()
                        .text_size(px(11.0))
                        .text_color(rgb(NEGATIVE))
                        .child(error),
                )
            })
    }
}

impl Render for HudSettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.distance_submit.is_none() {
            self.distance_submit = Some(cx.subscribe_in(
                &self.distance,
                window,
                |this, _, _: &Submitted, window, cx| {
                    this.apply_distance(cx);
                    if this
                        .error
                        .as_ref()
                        .is_some_and(|(section, _)| *section == Section::Distance)
                    {
                        this.distance.focus_handle(cx).focus(window);
                    }
                },
            ));
        }
        let position = self.choices(
            "hud-position",
            &HudPosition::ALL,
            (self.preferences.position, &self.position_focus),
            HudPosition::label,
            |this, position, cx| {
                this.change(
                    Section::Position,
                    HudPreferences {
                        position,
                        ..this.preferences
                    },
                    cx,
                )
            },
            cx,
        );
        let size = self.choices(
            "hud-size",
            &HudSize::ALL,
            (self.preferences.size, &self.size_focus),
            HudSize::label,
            |this, size, cx| {
                this.change(
                    Section::Size,
                    HudPreferences {
                        size,
                        ..this.preferences
                    },
                    cx,
                )
            },
            cx,
        );
        let brightness = self.choices(
            "hud-brightness",
            &HudBrightness::ALL,
            (self.preferences.brightness, &self.brightness_focus),
            HudBrightness::label,
            |this, brightness, cx| {
                this.change(
                    Section::Brightness,
                    HudPreferences {
                        brightness,
                        ..this.preferences
                    },
                    cx,
                )
            },
            cx,
        );
        let screen = self.screen_control(cx);
        let recording = self.palette(true, cx);
        let transcription = self.palette(false, cx);
        let distance = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(66.0)).child(self.distance.clone()))
            .child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child("pt"))
            .child(
                compact_button("Apply")
                    .id("hud-distance-apply")
                    .debug_selector(|| "hud-distance-apply".into())
                    .track_focus(&self.apply_focus)
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .border_1()
                    .border_color(rgb(LINE))
                    .on_click(cx.listener(|this, _, _, cx| this.apply_distance(cx))),
            );
        let disconnected = self.preferences.screen == HudScreen::FixedMonitor
            && !self
                .monitors
                .iter()
                .any(|choice| Some(choice.id) == self.preferences.fixed_monitor);
        let display_note = if disconnected {
            "The saved monitor is unavailable. The HUD follows your pointer until it reconnects."
        } else {
            "The HUD and Paste Last notice use this display. If unavailable, they follow your pointer."
        };
        let content = div()
            .child(settings_section_label("PLACEMENT"))
            .child(settings_panel()
                .child(self.row(Section::Position, "Screen edge", "Top or bottom of the visible display area, clear of the Dock.", position))
                .child(self.row(Section::Screen, "Display", display_note, screen))
                .child(self.row(Section::Distance, "Edge distance", "0–160 points from the screen edge. Press Enter or Apply.", distance).border_b_0()))
            .child(div().mt_6().child(settings_section_label("APPEARANCE")))
            .child(settings_panel()
                .child(self.row(Section::Size, "Size", "Scales the capsule and transcription sphere together.", size))
                .child(self.row(Section::Brightness, "Brightness", "Adjusts the light while keeping the current animation.", brightness))
                .child(self.row(Section::Recording, "Recording", "Color of the capsule while you speak.", recording))
                .child(self.row(Section::Transcription, "Transcribing", "Color of the sphere while audio is transcribed.", transcription).border_b_0()))
            .child(div().pt_4().px_1().text_size(px(11.0)).text_color(rgb(MUTED))
                .child("Changes apply immediately. The preparing capsule stays neutral while the microphone opens."));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header("HUD"))
            .child(
                div()
                    .id("hud-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_8()
                    .pt_1()
                    .pb_7()
                    .child(
                        div().w_full().flex().justify_center().child(
                            content
                                .w_full()
                                .min_w_0()
                                .max_w(px(PANE_CONTENT_WIDTH))
                                .relative(),
                        ),
                    ),
            )
    }
}

fn parse_distance(value: &str) -> Result<u16, &'static str> {
    value
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|value| (MIN_EDGE_DISTANCE..=MAX_EDGE_DISTANCE).contains(value))
        .ok_or("Enter a whole number from 0 to 160.")
}

fn monitor_choices(preview: bool) -> Vec<MonitorChoice> {
    if preview {
        vec![
            MonitorChoice {
                id: MonitorId::from_bytes([1; 16]),
                name: "Built-in display".into(),
            },
            MonitorChoice {
                id: MonitorId::from_bytes([2; 16]),
                name: "External display".into(),
            },
        ]
    } else {
        hud_screen::monitors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Focusable;

    #[test]
    fn distance_requires_an_integer_within_the_visible_control_range() {
        for value in ["0", "12", "160", " 24 "] {
            assert!(parse_distance(value).is_ok());
        }
        for value in ["", "-1", "161", "1.5", "99999999999"] {
            assert!(parse_distance(value).is_err());
        }
    }

    #[gpui::test]
    fn external_preferences_reset_drafts_but_unrelated_saves_preserve_them(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) =
            cx.add_window_view(|_, cx| HudSettingsView::new(HudPreferences::default(), true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.distance
                    .update(cx, |input, cx| input.set_text("24", cx));
                let larger = HudPreferences {
                    size: HudSize::Large,
                    ..view.preferences
                };
                view.change(Section::Size, larger, cx);
                view.set_preferences(larger, None, cx);
                assert_eq!(view.distance.read(cx).text(), "24");
                // An external reset/import is authoritative even when its edge
                // distance equals the previous saved value.
                view.set_preferences(HudPreferences::default(), None, cx);
                assert_eq!(view.distance.read(cx).text(), "12");
            });
        });
    }

    #[gpui::test]
    fn monitor_picker_is_keyboard_accessible_and_emits_complete_preferences(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) =
            cx.add_window_view(|_, cx| HudSettingsView::new(HudPreferences::default(), true, cx));
        let changes = Rc::new(std::cell::RefCell::new(Vec::new()));
        let subscription = cx.update(|_, cx| {
            let changes = changes.clone();
            cx.subscribe(&view, move |view, event: &HudChange, cx| {
                changes.borrow_mut().push(event.preferences);
                view.update(cx, |view, cx| {
                    view.set_preferences(event.preferences, None, cx)
                });
            })
        });
        cx.update(|window, cx| view.read(cx).monitor_picker.trigger.focus(window));
        cx.simulate_keystrokes("enter down enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(!view.monitor_open);
            assert!(view.monitor_picker.trigger.is_focused(window));
            assert_eq!(view.preferences.screen, HudScreen::FixedMonitor);
            assert_eq!(
                view.preferences.fixed_monitor,
                Some(MonitorId::from_bytes([2; 16]))
            );
            assert_eq!(view.preferences.recording_color, HudColor::Red);
            assert_eq!(view.preferences.transcription_color, HudColor::Blue);
        });
        assert_eq!(changes.borrow().len(), 1);
        drop(subscription);
    }

    #[gpui::test]
    fn distance_enter_emits_only_valid_changes_and_failed_save_keeps_original(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) =
            cx.add_window_view(|_, cx| HudSettingsView::new(HudPreferences::default(), true, cx));
        let changes = Rc::new(std::cell::RefCell::new(Vec::new()));
        let subscription = cx.update(|_, cx| {
            let changes = changes.clone();
            cx.subscribe(&view, move |_, event: &HudChange, _| {
                changes.borrow_mut().push(event.preferences)
            })
        });
        cx.update(|window, cx| view.read(cx).distance.focus_handle(cx).focus(window));
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("161");
        cx.simulate_keystrokes("enter");
        assert!(changes.borrow().is_empty());
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(view.error.is_some());
            assert!(view.distance.focus_handle(cx).is_focused(window));
        });
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("24");
        cx.simulate_keystrokes("enter");
        assert_eq!(changes.borrow()[0].edge_distance, 24);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.set_preferences(HudPreferences::default(), Some("Could not save".into()), cx);
                assert_eq!(view.preferences.edge_distance, 12);
                assert_eq!(view.distance.read(cx).text(), "24");
                assert_eq!(view.error.as_ref().unwrap().0, Section::Distance);
            })
        });
        drop(subscription);
    }
}
