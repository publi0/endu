//! Sound controls emit one proposed edit; the owner saves and supplies preferences.

use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent,
    Render, Window, div, prelude::*, px, rgb,
};

use crate::desktop_ui::{
    ACCENT, LINE, NEGATIVE, settings_panel, settings_row, settings_segmented_control,
    settings_segmented_item,
};
use crate::interaction_settings::SoundVolumes;

const LEVELS: [f32; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
const LABELS: [&str; 5] = ["Off", "25%", "50%", "75%", "100%"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SoundEvent {
    Start,
    Stop,
    ErrorCancel,
}

impl SoundEvent {
    const ALL: [Self; 3] = [Self::Start, Self::Stop, Self::ErrorCancel];

    fn index(self) -> usize {
        match self {
            Self::Start => 0,
            Self::Stop => 1,
            Self::ErrorCancel => 2,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Start => "Start sound",
            Self::Stop => "Stop sound",
            Self::ErrorCancel => "Error/cancel sound",
        }
    }

    fn volume(self, volumes: SoundVolumes) -> f32 {
        match self {
            Self::Start => volumes.start,
            Self::Stop => volumes.stop,
            Self::ErrorCancel => volumes.error_cancel,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SoundVolumeChange {
    pub event: SoundEvent,
    pub volume: f32,
}

pub struct SoundSettingsView {
    volumes: SoundVolumes,
    error: Option<(SoundEvent, String)>,
    focus: [[FocusHandle; 5]; 3],
    focus_index: [usize; 3],
}

impl EventEmitter<SoundVolumeChange> for SoundSettingsView {}

impl SoundSettingsView {
    pub fn new(volumes: SoundVolumes, cx: &mut Context<Self>) -> Self {
        let volumes = volumes.normalized();
        Self {
            volumes,
            error: None,
            focus: std::array::from_fn(|_| std::array::from_fn(|_| cx.focus_handle())),
            focus_index: SoundEvent::ALL.map(|event| nearest_level(event.volume(volumes))),
        }
    }

    pub fn set_preferences(
        &mut self,
        volumes: SoundVolumes,
        error: Option<(SoundEvent, String)>,
        cx: &mut Context<Self>,
    ) {
        self.volumes = volumes.normalized();
        if error.is_none() {
            self.focus_index =
                SoundEvent::ALL.map(|event| nearest_level(event.volume(self.volumes)));
        }
        self.error = error;
        cx.notify();
    }

    fn choose(&mut self, event: SoundEvent, index: usize, cx: &mut Context<Self>) {
        self.focus_index[event.index()] = index;
        cx.emit(SoundVolumeChange {
            event,
            volume: LEVELS[index],
        });
        cx.notify();
    }

    fn control_key(
        &mut self,
        sound: SoundEvent,
        index: usize,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.keystroke.modifiers;
        if modifiers.platform || modifiers.control || modifiers.alt {
            return;
        }
        let next = match event.keystroke.key.as_str() {
            "left" | "up" => index.saturating_sub(1),
            "right" | "down" => (index + 1).min(LEVELS.len() - 1),
            "home" => 0,
            "end" => LEVELS.len() - 1,
            "enter" | "space" => {
                self.choose(sound, index, cx);
                cx.stop_propagation();
                return;
            }
            "tab" => {
                if modifiers.shift {
                    window.focus_prev();
                } else {
                    window.focus_next();
                }
                cx.stop_propagation();
                return;
            }
            _ => return,
        };
        self.focus_index[sound.index()] = next;
        self.focus[sound.index()][next].focus(window);
        cx.notify();
        cx.stop_propagation();
    }

    fn render_row(&self, sound: SoundEvent, cx: &mut Context<Self>) -> AnyElement {
        let row = sound.index();
        let volume = sound.volume(self.volumes);
        let control = settings_segmented_control().children(LEVELS.into_iter().enumerate().map(
            |(index, level)| {
                // An imported intermediate level is shown in the copy, not rounded
                // into a preset selection that the user never chose.
                let selected = (volume - level).abs() < f32::EPSILON;
                settings_segmented_item(selected, LEVELS.len())
                    .id(("sound-level", row * LEVELS.len() + index))
                    .debug_selector(move || format!("sound-level-{row}-{index}"))
                    .track_focus(
                        &self.focus[row][index]
                            .clone()
                            .tab_stop(self.focus_index[row] == index),
                    )
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .cursor_pointer()
                    .child(LABELS[index])
                    .on_click(cx.listener(move |this, event, window, cx| {
                        if matches!(event, gpui::ClickEvent::Mouse(_)) {
                            this.focus[row][index].focus(window);
                            this.choose(sound, index, cx);
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event, window, cx| {
                        this.control_key(sound, index, event, window, cx);
                    }))
            },
        ));
        div()
            .border_color(rgb(LINE))
            .when(sound != SoundEvent::ErrorCancel, |row| row.border_b_1())
            .child(settings_row(sound.title(), volume_label(volume), control).border_b_0())
            .when_some(
                self.error.as_ref().filter(|(event, _)| *event == sound),
                |row, (_, error)| {
                    row.child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_size(px(11.0))
                            .text_color(rgb(NEGATIVE))
                            .child(error.clone()),
                    )
                },
            )
            .into_any_element()
    }
}

impl Focusable for SoundSettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus[0][self.focus_index[0]].clone()
    }
}

impl Render for SoundSettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        settings_panel().children(SoundEvent::ALL.map(|event| self.render_row(event, cx)))
    }
}

fn nearest_level(volume: f32) -> usize {
    LEVELS
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            (volume - **left).abs().total_cmp(&(volume - **right).abs())
        })
        .map_or(0, |(index, _)| index)
}

fn volume_label(volume: f32) -> String {
    if volume <= 0.0 {
        return "Off".into();
    }
    let percent = format!("{:.2}", volume * 100.0);
    format!(
        "{}% volume",
        percent.trim_end_matches('0').trim_end_matches('.')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn imported_levels_are_displayed_without_rounding_to_presets() {
        assert_eq!(volume_label(0.0), "Off");
        assert_eq!(volume_label(0.6), "60% volume");
        assert_eq!(volume_label(0.375), "37.5% volume");
        assert_eq!(volume_label(1.0), "100% volume");
        assert_eq!(nearest_level(0.6), 2);
    }

    #[gpui::test]
    fn keyboard_emits_one_delta_and_saved_preferences_remain_owner_controlled(
        cx: &mut gpui::TestAppContext,
    ) {
        let volumes = SoundVolumes::default();
        let (view, cx) = cx.add_window_view(|_, cx| SoundSettingsView::new(volumes, cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        cx.update(|window, cx| {
            let received = changes.clone();
            cx.subscribe(&view, move |_, event: &SoundVolumeChange, _| {
                received.borrow_mut().push(*event)
            })
            .detach();
            view.read(cx).focus_handle(cx).focus(window);
        });
        cx.simulate_keystrokes("right enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        assert_eq!(
            *changes.borrow(),
            [SoundVolumeChange {
                event: SoundEvent::Start,
                volume: 1.0
            }]
        );
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).volumes, volumes);
            view.update(cx, |view, cx| {
                view.set_preferences(
                    volumes,
                    Some((SoundEvent::Start, "Could not save".into())),
                    cx,
                )
            });
        });
        cx.simulate_keystrokes("tab home space");
        cx.update(|window, cx| {
            assert!(view.read(cx).focus[SoundEvent::Stop.index()][0].is_focused(window));
            assert_eq!(view.read(cx).error.as_ref().unwrap().1, "Could not save");
            assert_eq!(view.read(cx).volumes, volumes);
        });
        assert_eq!(
            changes.borrow().last(),
            Some(&SoundVolumeChange {
                event: SoundEvent::Stop,
                volume: 0.0
            })
        );
        cx.simulate_keystrokes("shift-tab");
        cx.update(|window, cx| {
            assert!(view.read(cx).focus[SoundEvent::Start.index()][4].is_focused(window))
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_preferences(
                    SoundVolumes {
                        start: 0.25,
                        stop: 0.75,
                        error_cancel: 0.5,
                    },
                    None,
                    cx,
                );
                assert_eq!(view.focus_index, [1, 3, 2]);
            });
            // Re-entering the control after an external import starts at its
            // imported selection, then Tab follows the other imported levels.
            view.read(cx).focus_handle(cx).focus(window);
        });
        cx.simulate_keystrokes("tab enter shift-tab enter");
        assert_eq!(
            &changes.borrow()[2..],
            &[
                SoundVolumeChange {
                    event: SoundEvent::Stop,
                    volume: 0.75,
                },
                SoundVolumeChange {
                    event: SoundEvent::Start,
                    volume: 0.25,
                },
            ]
        );
        cx.update(|window, cx| {
            assert!(view.read(cx).focus[SoundEvent::Start.index()][1].is_focused(window));
            assert!(view.read(cx).error.is_none());
        });
    }
}
