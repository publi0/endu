//! Shortcut controls, capture and binding rules.

use super::*;
use crate::desktop_ui::CONTROL_TEXT_SIZE;
use crate::i18n::t;

impl AppWindow {
    pub(super) fn render_hotkey_control(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let binding_keycaps = hotkey_binding(&self.settings, kind)
            .map_or_else(|| vec![t("Off").into()], HotkeyBinding::keycaps);
        // Translated labels and the Off keycap vary in width; measure them.
        let idle_width = hotkey_idle_width(binding_keycaps.len()).max(
            20.0 + measured_keycaps_width(window, &binding_keycaps)
                + control_text_width(window, t("Change shortcut")),
        );
        if let HotkeyCaptureState::Listening {
            kind: active,
            modifiers,
            message,
            ..
        } = &self.hotkey_capture
            && *active == kind
        {
            let label = message.unwrap_or(if modifiers.is_empty() {
                t("Press shortcut")
            } else {
                t("Release to save or add key")
            });
            let keycaps = if modifiers.is_empty() || message.is_some() {
                0.0
            } else {
                8.0 + measured_keycaps_width(
                    window,
                    &HotkeyBinding {
                        modifiers: *modifiers,
                        key: None,
                    }
                    .keycaps(),
                )
            };
            let needed = 64.0
                + keycaps
                + control_text_width(window, label)
                + control_text_width(window, t("Cancel"));
            if self.hotkey_width_spring.target < needed {
                self.hotkey_width_spring.set_target(needed);
            }
        }
        if matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Saved { saved_at, .. }
                if saved_at.elapsed() >= Duration::from_millis(700)
        ) {
            self.hotkey_capture = HotkeyCaptureState::Idle;
            self.hotkey_capture_animation.set_enabled(false);
        }
        if matches!(self.hotkey_capture, HotkeyCaptureState::Idle) {
            self.hotkey_width_origin = None;
        }
        let intensity = self.hotkey_capture_animation.render_position(window);
        let this_capture = matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Listening { kind: active, .. }
                | HotkeyCaptureState::Saved { kind: active, .. }
                if active == kind
        );
        // One width spring serves whichever control is capturing; it starts
        // from that control's own width. Idle controls never drive it, or the
        // two would pull it back and forth and redraw the window forever.
        if this_capture && self.hotkey_width_origin != Some(kind) {
            let target = self.hotkey_width_spring.target;
            self.hotkey_width_spring = ToggleSpring::at(idle_width);
            self.hotkey_width_spring.set_target(target);
            self.hotkey_width_origin = Some(kind);
        }
        let animated_control_width = if this_capture {
            self.hotkey_width_spring.render_position(window)
        } else {
            idle_width
        };
        let control_width = if this_capture {
            animated_control_width
        } else {
            idle_width
        };
        let capture_active = !matches!(self.hotkey_capture, HotkeyCaptureState::Idle);
        let control_intensity = if this_capture { intensity } else { 0.0 };
        let capture_color = if matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Saved { kind: active, .. } if active == kind
        ) {
            rgb(crate::desktop_ui::ThemeColor::SavedCapture)
        } else {
            rgb(crate::desktop_ui::ThemeColor::ListeningCapture)
        };
        let pulse = match &self.hotkey_capture {
            HotkeyCaptureState::Listening {
                kind: active,
                started_at,
                ..
            } if *active == kind => {
                window.request_animation_frame();
                (started_at.elapsed().as_secs_f32() * 4.5).sin() * 0.5 + 0.5
            }
            HotkeyCaptureState::Saved { kind: active, .. } if *active == kind => {
                window.request_animation_frame();
                1.0
            }
            HotkeyCaptureState::Idle
            | HotkeyCaptureState::Listening { .. }
            | HotkeyCaptureState::Saved { .. } => 0.0,
        };
        let content = match &self.hotkey_capture {
            HotkeyCaptureState::Listening {
                kind: active,
                modifiers,
                message,
                ..
            } if *active == kind => {
                let label = message.unwrap_or(if modifiers.is_empty() {
                    t("Press shortcut")
                } else {
                    t("Release to save or add key")
                });
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(5.0 + pulse * 3.0))
                            .ml_2()
                            .rounded_full()
                            .bg(rgb(NEGATIVE))
                            .opacity(0.6 + pulse * 0.4),
                    )
                    .when(!modifiers.is_empty() && message.is_none(), |content| {
                        content.child(hotkey_keycaps(
                            HotkeyBinding {
                                modifiers: *modifiers,
                                key: None,
                            }
                            .keycaps(),
                            0.85,
                        ))
                    })
                    .child(
                        div()
                            .text_size(px(CONTROL_TEXT_SIZE))
                            .text_color(rgb(TEXT_SOFT))
                            .child(label),
                    )
                    .child(
                        div()
                            .id(match kind {
                                HotkeyKind::Dictation => "cancel-dictation-hotkey-capture",
                                HotkeyKind::PasteLast => "cancel-paste-last-hotkey-capture",
                            })
                            .h(px(26.0))
                            .ml_1()
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded(px(4.0))
                            .text_size(px(CONTROL_TEXT_SIZE))
                            .text_color(rgb(MUTED))
                            .hover(|button| {
                                button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT_SOFT))
                            })
                            .child(t("Cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.cancel_hotkey_capture(cx);
                            })),
                    )
                    .into_any_element()
            }
            HotkeyCaptureState::Saved { kind: active, .. } if *active == kind => div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(10.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_SOFT))
                        .child("✓"),
                )
                .child(hotkey_keycaps(binding_keycaps.clone(), 1.0))
                .into_any_element(),
            HotkeyCaptureState::Idle
            | HotkeyCaptureState::Listening { .. }
            | HotkeyCaptureState::Saved { .. } => div()
                .flex()
                .items_center()
                .gap_2()
                .child(hotkey_keycaps(binding_keycaps, 1.0))
                .child(
                    div()
                        .text_size(px(CONTROL_TEXT_SIZE))
                        .text_color(rgb(TEXT_SOFT))
                        .child(t("Change shortcut")),
                )
                .into_any_element(),
        };
        div()
            .id(match kind {
                HotkeyKind::Dictation => "dictation-hotkey-control",
                HotkeyKind::PasteLast => "paste-last-hotkey-control",
            })
            .track_focus(&self.hotkey_focus)
            .w(px(control_width))
            .min_w(px(HOTKEY_MIN_WIDTH))
            .h(px(32.0))
            .px(px(4.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(px(6.0))
            .border_1()
            // At rest it shares the segmented track; capture tints it.
            .border_color(mix_color(
                rgb(crate::desktop_ui::ThemeColor::Track),
                rgb(TEXT_SOFT),
                control_intensity * 0.55,
            ))
            .bg(mix_color(
                rgb(crate::desktop_ui::ThemeColor::Track),
                capture_color,
                control_intensity * (0.55 + pulse * 0.15),
            ))
            .when(!capture_active, |control| {
                control.hover(|control| control.bg(rgb(SURFACE_HOVER)))
            })
            .child(content)
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.hotkey_capture.is_listening() && this_capture {
                    this.cancel_hotkey_capture(cx);
                } else {
                    this.begin_hotkey_capture(kind, window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.capture_hotkey_key(event, cx);
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                this.capture_hotkey_modifiers(event, cx);
            }))
            .into_any_element()
    }

    pub(super) fn render_hotkey_setting_control(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hotkey = self.render_hotkey_control(kind, window, cx);
        let can_reset = hotkey_binding(&self.settings, kind) != Some(&default_hotkey_binding(kind));
        let side = hotkey_binding(&self.settings, kind).and_then(standalone_modifier_side);
        let side_animation = &mut self.hotkey_side_animations[hotkey_kind_index(kind)];
        side_animation.set_enabled(side.is_some());
        let side_position = side_animation.render_position(window).clamp(0.0, 1.0);
        let selected = side.unwrap_or(ModifierSide::Either);
        let side_widths = [44.0, 56.0, 46.0];
        let side_selection_spring =
            &mut self.hotkey_side_selection_springs[hotkey_kind_index(kind)];
        side_selection_spring.set_target(hotkey_side_index(selected) as f32);
        let selection_position = side_selection_spring.render_position(window);
        let side_selector = div()
            .w(px(HOTKEY_SIDE_SELECTOR_WIDTH * side_position))
            .mr(px(8.0 * side_position))
            .flex_none()
            .overflow_hidden()
            .opacity(side_position)
            .child(
                sliding_segmented_control(selection_position, &side_widths)
                    .w(px(HOTKEY_SIDE_SELECTOR_WIDTH))
                    .children(
                        [
                            (t("Left"), ModifierSide::Left),
                            (t("Either"), ModifierSide::Either),
                            (t("Right"), ModifierSide::Right),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, side))| {
                            let candidate = hotkey_side_binding(&self.settings, kind, side);
                            sliding_segmented_item(side_widths[index], selected == side)
                                .id(("hotkey-side", hotkey_kind_index(kind) * 3 + index))
                                .when(candidate.is_none(), |item| item.opacity(0.35))
                                .child(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let Some(candidate) =
                                        hotkey_side_binding(&this.settings, kind, side)
                                    else {
                                        return;
                                    };
                                    if this.update_settings(
                                        hotkey_feedback_scope(kind),
                                        cx,
                                        |settings| {
                                            set_hotkey_binding(settings, kind, candidate);
                                        },
                                    ) {
                                        this.hotkey_side_selection_springs[hotkey_kind_index(kind)]
                                            .set_target(index as f32);
                                    }
                                }))
                        }),
                    ),
            );
        div()
            .flex()
            .items_center()
            .child(side_selector)
            .child(hotkey)
            .child(
                compact_button(t("Reset"))
                    .id(("reset-hotkey", hotkey_kind_index(kind)))
                    .debug_selector(move || format!("reset-hotkey-{}", hotkey_kind_index(kind)))
                    .track_focus(
                        &self.hotkey_reset_focus[hotkey_kind_index(kind)]
                            .clone()
                            .tab_stop(can_reset),
                    )
                    .flex_none()
                    .ml_2()
                    .h(px(32.0))
                    .border_1()
                    .border_color(rgb(LINE))
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .when(!can_reset, |button| button.opacity(0.35))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if can_reset {
                            this.reset_hotkey_binding(kind, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    pub(super) fn reset_hotkey_binding(
        &mut self,
        kind: HotkeyKind,
        cx: &mut Context<Self>,
    ) -> bool {
        self.cancel_hotkey_capture(cx);
        let binding = default_hotkey_binding(kind);
        if hotkey_binding(&self.settings, kind) == Some(&binding) {
            return true;
        }
        if hotkey_binding_conflicts(&self.settings, kind, &binding) {
            let other = match kind {
                HotkeyKind::Dictation => t("Paste last dictation"),
                HotkeyKind::PasteLast => t("Dictation"),
            };
            self.settings_feedback = Some(SettingsFeedback {
                control: hotkey_feedback_scope(kind),
                success: false,
                message: tf!(
                    "The default shortcut is used by {other}. Change that shortcut first.",
                    other = other
                ),
            });
            cx.notify();
            return false;
        }
        self.update_settings(hotkey_feedback_scope(kind), cx, |settings| {
            set_hotkey_binding(settings, kind, binding);
        })
    }

    pub(super) fn begin_hotkey_capture(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preview {
            crate::app_settings::set_hotkey_capture_active(true);
        }
        self.hotkey_width_spring.set_target(hotkey_capture_width(0));
        self.hotkey_capture = HotkeyCaptureState::Listening {
            kind,
            modifiers: HotkeyModifiers::default(),
            message: None,
            started_at: Instant::now(),
        };
        self.hotkey_capture_animation.set_enabled(true);
        self.hotkey_focus.focus(window);
        cx.notify();
    }

    pub(super) fn cancel_hotkey_capture(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.hotkey_capture, HotkeyCaptureState::Idle) {
            if !self.preview {
                crate::app_settings::set_hotkey_capture_active(false);
            }
            self.hotkey_capture = HotkeyCaptureState::Idle;
            self.hotkey_capture_animation.set_enabled(false);
            self.hotkey_width_spring.set_target(HOTKEY_MIN_WIDTH);
            cx.notify();
        }
    }

    pub(super) fn capture_hotkey_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.hotkey_capture.is_listening() {
            return;
        }
        cx.stop_propagation();
        if event.is_held {
            return;
        }
        if event.keystroke.key == "escape" {
            self.cancel_hotkey_capture(cx);
            return;
        }
        let modifiers = hotkey_modifiers(event.keystroke.modifiers).without_side_constraints();
        let key = match hotkey_key(&event.keystroke.key) {
            Ok(key) => key,
            Err(message) => {
                self.set_hotkey_capture_message(message, cx);
                return;
            }
        };
        if modifiers.is_empty() && !is_function_key(&key.label) {
            self.set_hotkey_capture_message(t("Add a modifier"), cx);
            return;
        }
        let binding = HotkeyBinding {
            modifiers,
            key: Some(key),
        };
        self.hotkey_width_spring
            .set_target(hotkey_capture_width(binding.keycaps().len()));
        self.save_hotkey_binding(binding, cx);
    }

    pub(super) fn capture_hotkey_modifiers(
        &mut self,
        event: &ModifiersChangedEvent,
        cx: &mut Context<Self>,
    ) {
        let current = hotkey_modifiers(event.modifiers);
        if !current.is_empty() {
            self.hotkey_width_spring.set_target(hotkey_capture_width(
                HotkeyBinding {
                    modifiers: current,
                    key: None,
                }
                .keycaps()
                .len(),
            ));
        }
        let released = {
            let HotkeyCaptureState::Listening {
                modifiers, message, ..
            } = &mut self.hotkey_capture
            else {
                return;
            };
            if !current.is_empty() {
                if current.count() >= modifiers.count() {
                    *modifiers = current;
                    *message = None;
                }
                cx.notify();
                None
            } else if message.is_some() {
                *modifiers = HotkeyModifiers::default();
                *message = None;
                cx.notify();
                None
            } else {
                (!modifiers.is_empty()).then_some(*modifiers)
            }
        };
        if let Some(modifiers) = released {
            let modifiers = if modifiers.count() == 1 {
                modifiers
            } else {
                modifiers.without_side_constraints()
            };
            self.save_hotkey_binding(
                HotkeyBinding {
                    modifiers,
                    key: None,
                },
                cx,
            );
        }
    }

    pub(super) fn set_hotkey_capture_message(
        &mut self,
        next_message: &'static str,
        cx: &mut Context<Self>,
    ) {
        if let HotkeyCaptureState::Listening { message, .. } = &mut self.hotkey_capture {
            *message = Some(next_message);
            cx.notify();
        }
    }

    pub(super) fn save_hotkey_binding(&mut self, binding: HotkeyBinding, cx: &mut Context<Self>) {
        if binding.is_empty() {
            self.set_hotkey_capture_message(t("Press a shortcut"), cx);
            return;
        }
        let kind = match self.hotkey_capture {
            HotkeyCaptureState::Listening { kind, .. } => kind,
            HotkeyCaptureState::Idle | HotkeyCaptureState::Saved { .. } => return,
        };
        if hotkey_binding_conflicts(&self.settings, kind, &binding) {
            self.set_hotkey_capture_message(t("Already in use"), cx);
            return;
        }
        let keycap_count = binding.keycaps().len();
        if !self.update_settings(hotkey_feedback_scope(kind), cx, |settings| {
            set_hotkey_binding(settings, kind, binding)
        }) {
            self.set_hotkey_capture_message(t("Could not save shortcut. Try again."), cx);
            return;
        }
        self.hotkey_width_spring
            .set_target(hotkey_saved_width(keycap_count));
        if !self.preview {
            crate::app_settings::set_hotkey_capture_active(false);
        }
        self.hotkey_capture = HotkeyCaptureState::Saved {
            kind,
            saved_at: Instant::now(),
        };
        cx.notify();
    }
}

pub(super) fn set_hotkey_binding(
    settings: &mut AppSettings,
    kind: HotkeyKind,
    binding: HotkeyBinding,
) {
    match kind {
        HotkeyKind::Dictation => {
            if binding.key.is_none() {
                settings.double_tap_only = false;
            }
            settings.dictation_hotkey = binding;
        }
        HotkeyKind::PasteLast => settings.paste_last_hotkey = Some(binding),
    }
}

/// Rendered width of control text, so translated labels never clip.
fn control_text_width(window: &Window, text: &str) -> f32 {
    let run = window.text_style().to_run(text.len());
    f32::from(
        window
            .text_system()
            .shape_line(text.to_owned().into(), px(CONTROL_TEXT_SIZE), &[run], None)
            .width,
    )
}

/// Keycaps are at least 34 points wide, with 8-point padding and a border.
fn measured_keycaps_width(window: &Window, parts: &[String]) -> f32 {
    parts
        .iter()
        .map(|part| (control_text_width(window, part) + 18.0).max(34.0))
        .sum::<f32>()
        + 3.0 * parts.len().saturating_sub(1) as f32
}

pub(super) fn hotkey_idle_width(keycap_count: usize) -> f32 {
    (112.0 + hotkey_keycaps_width(keycap_count)).max(HOTKEY_MIN_WIDTH)
}

pub(super) const fn hotkey_kind_index(kind: HotkeyKind) -> usize {
    match kind {
        HotkeyKind::Dictation => 0,
        HotkeyKind::PasteLast => 1,
    }
}

pub(super) const fn hotkey_side_index(side: ModifierSide) -> usize {
    match side {
        ModifierSide::Left => 0,
        ModifierSide::Either => 1,
        ModifierSide::Right => 2,
    }
}

pub(super) fn standalone_modifier_side(binding: &HotkeyBinding) -> Option<ModifierSide> {
    if binding.key.is_some() || binding.modifiers.count() != 1 {
        return None;
    }
    binding
        .modifiers
        .control
        .or(binding.modifiers.option)
        .or(binding.modifiers.shift)
        .or(binding.modifiers.command)
}

pub(super) fn set_standalone_modifier_side(binding: &mut HotkeyBinding, side: ModifierSide) {
    if standalone_modifier_side(binding).is_none() {
        return;
    }
    if binding.modifiers.control.is_some() {
        binding.modifiers.control = Some(side);
    } else if binding.modifiers.option.is_some() {
        binding.modifiers.option = Some(side);
    } else if binding.modifiers.shift.is_some() {
        binding.modifiers.shift = Some(side);
    } else if binding.modifiers.command.is_some() {
        binding.modifiers.command = Some(side);
    }
}

pub(super) fn hotkey_binding_conflicts(
    settings: &AppSettings,
    kind: HotkeyKind,
    binding: &HotkeyBinding,
) -> bool {
    let others = match kind {
        HotkeyKind::Dictation => settings.paste_last_hotkey.clone(),
        HotkeyKind::PasteLast => Some(settings.dictation_hotkey.clone()),
    };
    crate::app_settings::hotkey_conflicts(binding, others)
}

pub(super) fn default_hotkey_binding(kind: HotkeyKind) -> HotkeyBinding {
    match kind {
        HotkeyKind::Dictation => HotkeyBinding::default(),
        HotkeyKind::PasteLast => HotkeyBinding::paste_last_default(),
    }
}

pub(super) fn hotkey_binding(settings: &AppSettings, kind: HotkeyKind) -> Option<&HotkeyBinding> {
    match kind {
        HotkeyKind::Dictation => Some(&settings.dictation_hotkey),
        HotkeyKind::PasteLast => settings.paste_last_hotkey.as_ref(),
    }
}

pub(super) fn hotkey_side_binding(
    settings: &AppSettings,
    kind: HotkeyKind,
    side: ModifierSide,
) -> Option<HotkeyBinding> {
    let mut binding = hotkey_binding(settings, kind)?.clone();
    standalone_modifier_side(&binding)?;
    set_standalone_modifier_side(&mut binding, side);
    (!hotkey_binding_conflicts(settings, kind, &binding)).then_some(binding)
}

pub(super) fn hotkey_capture_width(keycap_count: usize) -> f32 {
    if keycap_count == 0 {
        180.0
    } else {
        236.0 + hotkey_keycaps_width(keycap_count)
    }
}

pub(super) fn hotkey_saved_width(keycap_count: usize) -> f32 {
    (53.0 + hotkey_keycaps_width(keycap_count)).max(HOTKEY_MIN_WIDTH)
}

pub(super) fn hotkey_keycaps_width(keycap_count: usize) -> f32 {
    if keycap_count == 0 {
        0.0
    } else {
        34.0 * keycap_count as f32 + 3.0 * keycap_count.saturating_sub(1) as f32
    }
}

pub(super) fn hotkey_modifiers(modifiers: GpuiModifiers) -> HotkeyModifiers {
    hotkey_modifiers_with_flags(modifiers, crate::suppression::physical_modifier_flags())
}

pub(super) fn hotkey_modifiers_with_flags(modifiers: GpuiModifiers, flags: u64) -> HotkeyModifiers {
    let physical = crate::app_settings::modifiers_from_flags(flags);
    if !physical.is_empty() {
        return physical;
    }
    HotkeyModifiers {
        control: modifiers.control.then_some(Default::default()),
        option: modifiers.alt.then_some(Default::default()),
        shift: modifiers.shift.then_some(Default::default()),
        command: modifiers.platform.then_some(Default::default()),
        function: modifiers.function,
    }
}

pub(super) fn hotkey_key(key: &str) -> Result<HotkeyKey, &'static str> {
    let special = match key {
        "space" => Some((49, "Space")),
        "tab" => Some((48, "Tab")),
        "enter" => Some((36, "Return")),
        "backspace" => Some((51, "Delete")),
        "up" => Some((126, "Up")),
        "down" => Some((125, "Down")),
        "left" => Some((123, "Left")),
        "right" => Some((124, "Right")),
        "pageup" => Some((116, "Page Up")),
        "pagedown" => Some((121, "Page Down")),
        "home" => Some((115, "Home")),
        "end" => Some((119, "End")),
        "delete" => Some((117, "Forward Delete")),
        "insert" => Some((114, "Help")),
        "f1" => Some((122, "F1")),
        "f2" => Some((120, "F2")),
        "f3" => Some((99, "F3")),
        "f4" => Some((118, "F4")),
        "f5" => Some((96, "F5")),
        "f6" => Some((97, "F6")),
        "f7" => Some((98, "F7")),
        "f8" => Some((100, "F8")),
        "f9" => Some((101, "F9")),
        "f10" => Some((109, "F10")),
        "f11" => Some((103, "F11")),
        "f12" => Some((111, "F12")),
        "f13" => Some((105, "F13")),
        "f14" => Some((107, "F14")),
        "f15" => Some((113, "F15")),
        "f16" => Some((106, "F16")),
        "f17" => Some((64, "F17")),
        "f18" => Some((79, "F18")),
        "f19" => Some((80, "F19")),
        "f20" => Some((90, "F20")),
        _ => None,
    };
    if let Some((code, label)) = special {
        return Ok(HotkeyKey {
            code,
            label: label.into(),
        });
    }
    let mut characters = key.chars();
    let Some(character) = characters.next() else {
        return Err(t("Unsupported key"));
    };
    if characters.next().is_some() {
        return Err(t("Unsupported key"));
    }
    let code = crate::keyboard::key_code_for(character).map_err(|_| t("Unsupported key"))?;
    Ok(HotkeyKey {
        code,
        label: character.to_uppercase().collect(),
    })
}

pub(super) fn is_function_key(label: &str) -> bool {
    label
        .strip_prefix('F')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=20).contains(&number))
}
