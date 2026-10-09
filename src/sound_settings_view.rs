//! Sound controls emit one proposed edit; the owner saves and supplies preferences.

use gpui::{
    AnyElement, App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Render, Window, div, prelude::*, px,
};

use crate::desktop_ui::{
    ACCENT, CONTROL_HEIGHT, CONTROL_TEXT_SIZE, LINE, NEGATIVE, PickerState, SETTINGS_CONTROL_WIDTH,
    SURFACE, SURFACE_HOVER, SURFACE_SELECTED, TEXT, TEXT_SOFT, compact_button, disclosure_button,
    picker_open_key, picker_popup, rgb, settings_panel, settings_row, settings_segmented_item,
};
use crate::i18n::t;
use crate::interaction_settings::SoundVolumes;
use crate::start_cue::StartCue;

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
            Self::Start => t("Start sound"),
            Self::Stop => t("Stop sound"),
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

/// A chosen start cue. Choosing the current cue again still emits, so the
/// owner can replay it as a preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartCueChange(pub StartCue);

/// A request to hear the selected start cue again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartCuePreview;

pub struct SoundSettingsView {
    volumes: SoundVolumes,
    error: Option<(SoundEvent, String)>,
    focus: [[FocusHandle; 5]; 3],
    focus_index: [usize; 3],
    start_cue: StartCue,
    cue_error: Option<String>,
    cue_picker: PickerState,
    cue_open: bool,
    play_focus: FocusHandle,
    /// Counts previews so the Play button's little wave moves for each one.
    played: u64,
}

impl EventEmitter<SoundVolumeChange> for SoundSettingsView {}
impl EventEmitter<StartCueChange> for SoundSettingsView {}
impl EventEmitter<StartCuePreview> for SoundSettingsView {}

impl SoundSettingsView {
    pub fn new(volumes: SoundVolumes, start_cue: StartCue, cx: &mut Context<Self>) -> Self {
        let volumes = volumes.normalized();
        Self {
            volumes,
            error: None,
            focus: std::array::from_fn(|_| std::array::from_fn(|_| cx.focus_handle())),
            focus_index: SoundEvent::ALL.map(|event| nearest_level(event.volume(volumes))),
            start_cue,
            cue_error: None,
            cue_picker: PickerState::new(cx),
            cue_open: false,
            play_focus: cx.focus_handle().tab_stop(true),
            played: 0,
        }
    }

    pub fn set_start_cue(&mut self, cue: StartCue, error: Option<String>, cx: &mut Context<Self>) {
        self.start_cue = cue;
        self.cue_error = error;
        cx.notify();
    }

    fn toggle_cues(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cue_open {
            self.cue_open = false;
            self.cue_picker.trigger.focus(window);
        } else {
            self.cue_open = true;
            self.cue_picker
                .open(self.start_cue.index(), StartCue::ALL.len(), window);
        }
        cx.notify();
    }

    fn choose_cue(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&cue) = StartCue::ALL.get(index) else {
            return;
        };
        self.cue_open = false;
        self.cue_picker.trigger.focus(window);
        // Choosing a cue previews it, like Play.
        self.played += 1;
        cx.emit(StartCueChange(cue));
        cx.notify();
    }

    fn cue_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if self.cue_picker.navigate(key, StartCue::ALL.len()) {
            cx.stop_propagation();
            cx.notify();
        } else if matches!(key, "escape" | "tab") {
            self.cue_open = false;
            self.cue_picker.close(event, window);
            cx.stop_propagation();
            cx.notify();
        } else if matches!(key, "enter" | "space") {
            self.choose_cue(self.cue_picker.highlight, window, cx);
            cx.stop_propagation();
        }
    }

    fn render_cue_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let popup = self.cue_open.then(|| {
            let rows = StartCue::ALL.into_iter().enumerate().map(|(index, cue)| {
                div()
                    .id(("start-cue-choice", index))
                    .debug_selector(move || format!("start-cue-choice-{index}"))
                    .h(px(CONTROL_HEIGHT))
                    .px_3()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .rounded_sm()
                    .text_size(px(CONTROL_TEXT_SIZE))
                    .text_color(rgb(TEXT_SOFT))
                    .when(index == self.cue_picker.highlight, |row| {
                        row.bg(rgb(SURFACE_SELECTED))
                    })
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    .child(div().min_w_0().truncate().child(cue.label()))
                    .when(cue == self.start_cue, |row| {
                        row.child(div().flex_none().text_color(rgb(TEXT)).child("✓"))
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.choose_cue(index, window, cx)),
                    )
            });
            div()
                .id("start-cue-menu")
                .debug_selector(|| "start-cue-menu".into())
                .track_focus(&self.cue_picker.menu)
                .w(px(SETTINGS_CONTROL_WIDTH))
                .p_2()
                .rounded_sm()
                .border_1()
                .border_color(rgb(LINE))
                .bg(rgb(SURFACE))
                .shadow_lg()
                .occlude()
                .on_key_down(cx.listener(Self::cue_keys))
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.cue_open = false;
                    this.cue_picker.trigger.focus(window);
                    cx.notify();
                }))
                .child(
                    div()
                        .id("start-cue-list")
                        .max_h(px(240.0))
                        .overflow_y_scroll()
                        .track_scroll(&self.cue_picker.scroll)
                        .flex()
                        .flex_col()
                        .children(rows),
                )
        });
        let play = compact_button(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(playing_wave(self.played))
                .child(t("Play")),
        )
        .id("start-cue-play")
        .debug_selector(|| "start-cue-play".into())
        .flex_none()
        .border_1()
        .border_color(rgb(LINE))
        .track_focus(&self.play_focus)
        .focus(|style| style.border_color(rgb(ACCENT)))
        .on_click(cx.listener(|this, _, _, cx| {
            this.played += 1;
            cx.emit(StartCuePreview);
            cx.notify();
        }));
        let picker = div()
            .flex_none()
            .relative()
            .child(
                disclosure_button(self.start_cue.label())
                    .id("start-cue-trigger")
                    .debug_selector(|| "start-cue-trigger".into())
                    .track_focus(&self.cue_picker.trigger)
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        if picker_open_key(event) {
                            this.toggle_cues(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                        if !matches!(event, ClickEvent::Keyboard(_)) {
                            this.toggle_cues(window, cx);
                        }
                    })),
            )
            .children(popup.map(picker_popup));
        let control = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .child(play)
            .child(picker);
        div()
            .border_b_1()
            .border_color(rgb(crate::desktop_ui::DIVIDER))
            .child(
                settings_row(t("Start cue"), t("Plays when recording begins"), control)
                    .border_b_0(),
            )
            .when_some(self.cue_error.clone(), |row, error| {
                row.child(
                    div()
                        .px_4()
                        .pb_3()
                        .text_size(px(11.0))
                        .text_color(rgb(NEGATIVE))
                        .child(error),
                )
            })
            .into_any_element()
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
        let control = crate::desktop_ui::settings_choice(
            format!("sound-level-{row}"),
            LEVELS
                .iter()
                .position(|level| (volume - level).abs() < f32::EPSILON),
            LEVELS.len(),
        )
        .children(LEVELS.into_iter().enumerate().map(|(index, level)| {
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
        }));
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
        settings_panel()
            .child(self.render_cue_row(cx))
            .children(SoundEvent::ALL.map(|event| self.render_row(event, cx)))
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
    if LEVELS
        .iter()
        .any(|level| (volume - level).abs() < f32::EPSILON)
    {
        return String::new();
    }
    let percent = format!("{:.2}", volume * 100.0);
    tf!(
        "{percent}% volume",
        percent = percent.trim_end_matches('0').trim_end_matches('.')
    )
}

/// Three small bars beside Play that ripple while a cue sounds and rest after.
fn playing_wave(played: u64) -> AnyElement {
    use crate::desktop_ui::ThemeColor;
    const REST: [f32; 3] = [5.0, 9.0, 6.0];
    let bars = move |progress: f32| {
        let live = if played == 0 { 0.0 } else { 1.0 - progress };
        div()
            .w(px(12.0))
            .h(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            .children(REST.iter().enumerate().map(move |(index, rest)| {
                let swing = (progress * std::f32::consts::TAU * 3.0 + index as f32 * 1.7).sin();
                div()
                    .w(px(2.0))
                    .h(px((rest + 3.0 * swing * live).clamp(2.0, 12.0)))
                    .rounded(px(1.0))
                    .bg(crate::desktop_ui::mix_color(
                        rgb(ThemeColor::Muted),
                        rgb(ThemeColor::Accent),
                        live,
                    ))
            }))
    };
    if played == 0 {
        return bars(1.0).into_any_element();
    }
    crate::desktop_ui::animate_once(
        bars(0.0),
        gpui::ElementId::NamedInteger("start-cue-wave".into(), played),
        900,
        move |_, progress| bars(progress),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn imported_levels_are_displayed_without_rounding_to_presets() {
        assert_eq!(volume_label(0.0), "");
        assert_eq!(volume_label(0.6), "60% volume");
        assert_eq!(volume_label(0.375), "37.5% volume");
        assert_eq!(volume_label(1.0), "");
        assert_eq!(nearest_level(0.6), 2);
    }

    #[gpui::test]
    fn start_cue_menu_previews_by_keyboard_and_restores_focus(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            SoundSettingsView::new(SoundVolumes::default(), StartCue::default(), cx)
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        cx.update(|window, cx| {
            let received = changes.clone();
            cx.subscribe(&view, move |_, event: &StartCueChange, _| {
                received.borrow_mut().push(*event)
            })
            .detach();
            view.read(cx).cue_picker.trigger.focus(window);
        });
        cx.simulate_keystrokes("down");
        cx.update(|window, cx| {
            assert!(view.read(cx).cue_open);
            assert!(view.read(cx).cue_picker.menu.is_focused(window));
            assert_eq!(view.read(cx).cue_picker.highlight, 0);
        });
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            assert!(!view.read(cx).cue_open);
            assert!(view.read(cx).cue_picker.trigger.is_focused(window));
        });
        assert!(changes.borrow().is_empty());

        cx.simulate_keystrokes("enter down down enter");
        cx.update(|window, cx| {
            assert!(!view.read(cx).cue_open);
            assert!(view.read(cx).cue_picker.trigger.is_focused(window));
            // The owner confirms the saved cue; the view does not assume it.
            assert_eq!(view.read(cx).start_cue, StartCue::Breath);
            view.update(cx, |view, cx| {
                view.set_start_cue(StartCue::TwoNotes, Some("Could not save".into()), cx)
            });
            assert_eq!(view.read(cx).cue_error.as_deref(), Some("Could not save"));
        });
        assert_eq!(*changes.borrow(), [StartCueChange(StartCue::TwoNotes)]);

        let previews = Rc::new(RefCell::new(0));
        cx.update(|window, cx| {
            let received = previews.clone();
            cx.subscribe(&view, move |_, _: &StartCuePreview, _| {
                *received.borrow_mut() += 1
            })
            .detach();
            view.read(cx).play_focus.focus(window);
        });
        cx.simulate_keystrokes("enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        assert_eq!(*previews.borrow(), 1);
        assert_eq!(changes.borrow().len(), 1);
    }

    #[gpui::test]
    fn keyboard_emits_one_delta_and_saved_preferences_remain_owner_controlled(
        cx: &mut gpui::TestAppContext,
    ) {
        let volumes = SoundVolumes::default();
        let (view, cx) =
            cx.add_window_view(|_, cx| SoundSettingsView::new(volumes, StartCue::default(), cx));
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
