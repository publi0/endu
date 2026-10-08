//! Microphone pane: input device, channel, levels and capture options.

use super::*;

impl AppWindow {
    pub(super) fn microphone_choices(&self) -> Vec<Option<String>> {
        std::iter::once(None)
            .chain(self.microphone_devices.iter().cloned().map(Some))
            .collect()
    }

    pub(super) fn choose_microphone(
        &mut self,
        device: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.update_settings(SettingControl::Microphone, cx, |settings| {
            settings.microphone = device
        }) {
            self.microphone_picker_error = self
                .feedback_error(SettingControl::Microphone)
                .or_else(|| self.settings_error.take());
            return false;
        }
        self.microphone_picker_open = false;
        self.microphone_picker_error = None;
        self.refresh_microphone_description();
        true
    }

    pub(super) fn render_microphone_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.microphone_choices();
        selection_picker_menu(
            "microphone-picker",
            &self.microphone_picker_state,
            choices,
            self.settings.microphone.clone(),
            |device| device.clone().unwrap_or_else(|| "Automatic".into()),
            self.microphone_picker_error
                .clone()
                .or_else(|| self.feedback_error(SettingControl::Microphone)),
            (
                cx.listener(|this, device: &Option<String>, window, cx| {
                    if this.choose_microphone(device.clone(), cx) {
                        this.microphone_picker_state.trigger.focus(window);
                    }
                }),
                cx.listener(|this, _, _, cx| {
                    this.microphone_picker_open = false;
                    this.microphone_picker_error = None;
                    cx.notify();
                }),
                cx.listener(Self::microphone_picker_key),
            ),
        )
        .into_any_element()
    }

    pub(super) fn microphone_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choices = self.microphone_choices();
        let key = event.keystroke.key.as_str();
        if self.microphone_picker_state.navigate(key, choices.len()) {
        } else if matches!(key, "enter" | "space") {
            if let Some(device) = choices.get(self.microphone_picker_state.highlight)
                && self.choose_microphone(device.clone(), cx)
            {
                self.microphone_picker_state.trigger.focus(window);
            }
        } else if matches!(key, "escape" | "tab") {
            self.microphone_picker_open = false;
            self.microphone_picker_state.close(event, window);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn toggle_microphone_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.microphone_channel_picker_open = false;
        self.microphone_picker_open = !self.microphone_picker_open;
        if self.microphone_picker_open && !self.preview {
            match crate::audio::input_device_names() {
                Ok(devices) => {
                    self.microphone_devices = devices;
                    self.microphone_picker_error = None;
                }
                Err(error) => self.microphone_picker_error = Some(error.to_string()),
            }
        }
        if self.microphone_picker_open {
            let choices = self.microphone_choices();
            let index = choices
                .iter()
                .position(|device| *device == self.settings.microphone)
                .unwrap_or(0);
            self.microphone_picker_state
                .open(index, choices.len(), window);
        } else {
            self.microphone_picker_state.trigger.focus(window);
        }
        cx.notify();
    }

    /// The Input device row description names the microphone that dictation
    /// would use right now, so the effective choice is visible at the top of
    /// the pane instead of only in the channel and level rows below.
    pub(super) fn input_device_description(&self) -> String {
        match (
            &self.microphone_description,
            &self.microphone_description_error,
        ) {
            (Some(description), _) => {
                let mode = if self.settings.microphone.is_some() {
                    "Fixed"
                } else {
                    "Automatic"
                };
                format!(
                    "Current: {} — {} ({mode})",
                    description.name,
                    description.channel_label()
                )
            }
            (None, Some(error)) => format!("Microphone unavailable: {error}"),
            (None, None) => "Checking the available microphones…".to_string(),
        }
    }

    pub(super) fn refresh_microphone_description(&mut self) -> bool {
        let previous = (
            self.microphone_description.clone(),
            self.microphone_description_error.clone(),
        );
        let result = if self.preview {
            Ok(crate::microphone::InputDescription::for_preview(
                self.settings.microphone_channel.as_ref(),
            ))
        } else {
            crate::audio::input_description(
                self.settings.microphone.as_deref(),
                &self.settings.microphone_priority,
            )
        };
        match result {
            Ok(description) => {
                self.microphone_description = Some(description);
                self.microphone_description_error = None;
            }
            Err(error) => {
                self.microphone_description = None;
                self.microphone_description_error = Some(error.to_string());
            }
        }
        self.microphone_refresh_at = Instant::now() + Duration::from_secs(5);
        let changed = previous
            != (
                self.microphone_description.clone(),
                self.microphone_description_error.clone(),
            );
        if changed {
            self.microphone_channel_picker_open = false;
        }
        changed
    }

    pub(super) fn poll_microphone(&mut self) -> bool {
        if self.preview || self.pane != Pane::Microphone {
            return false;
        }
        let latest = crate::microphone::latest();
        let mut changed = self.microphone_diagnostic != latest;
        self.microphone_diagnostic = latest;
        if Instant::now() >= self.microphone_refresh_at {
            changed |= self.refresh_microphone_description();
        }
        changed
    }

    pub(super) fn select_microphone_channel(
        &mut self,
        device: &crate::microphone::InputDescription,
        channel: Option<u16>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.refresh_microphone_description();
        let Some(current) = &self.microphone_description else {
            cx.notify();
            return false;
        };
        if current.device_id != device.device_id
            || channel.is_some_and(|channel| {
                crate::microphone::resolve_channel(Some(channel), current.channels).is_none()
            })
        {
            self.settings_feedback = Some(SettingsFeedback {
                control: SettingControl::Channel,
                success: false,
                message: "Microphone changed. The channel choice was not applied.".into(),
            });
            cx.notify();
            return false;
        }
        let Some(device_id) = &current.device_id else {
            return false;
        };
        let selection = channel.map(|channel| crate::microphone::ChannelSelection {
            device_id: device_id.clone(),
            device_name: current.name.clone(),
            channel,
        });
        let device_id = device_id.clone();
        let saved = self.update_settings(SettingControl::Channel, cx, |settings| {
            if selection.is_some()
                || settings
                    .microphone_channel
                    .as_ref()
                    .is_some_and(|previous| previous.device_id == device_id)
            {
                settings.microphone_channel = selection;
            }
        });
        if saved {
            self.microphone_channel_picker_open = false;
            self.refresh_microphone_description();
        }
        cx.notify();
        saved
    }

    pub(super) fn toggle_microphone_channel_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.microphone_picker_open = false;
        self.refresh_microphone_description();
        self.microphone_channel_picker_open = !self.microphone_channel_picker_open;
        if self.microphone_channel_picker_open {
            if let Some(source) = &self.microphone_description {
                self.microphone_channel_picker_state.open(
                    usize::from(source.channel.unwrap_or(0)),
                    usize::from(source.channels) + 1,
                    window,
                );
            }
        } else {
            self.microphone_channel_picker_state.trigger.focus(window);
        }
        cx.notify();
    }

    pub(super) fn microphone_channel_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.microphone_description.clone() else {
            return;
        };
        let key = event.keystroke.key.as_str();
        if self
            .microphone_channel_picker_state
            .navigate(key, usize::from(source.channels) + 1)
        {
        } else if matches!(key, "enter" | "space") {
            let index = self.microphone_channel_picker_state.highlight;
            let selected = (index > 0).then_some(index as u16);
            if self.select_microphone_channel(&source, selected, cx)
                || !self.microphone_channel_picker_open
            {
                self.microphone_channel_picker_state.trigger.focus(window);
            }
        } else if matches!(key, "escape" | "tab") {
            self.microphone_channel_picker_open = false;
            self.microphone_channel_picker_state.close(event, window);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn render_microphone_channel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(description) = self.microphone_description.clone() else {
            return self
                .setting_row(
                    SettingControl::Channel,
                    "Input channel",
                    self.microphone_description_error
                        .clone()
                        .unwrap_or_else(|| "Input metadata is unavailable".into()),
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child("Unavailable"),
                )
                .into_any_element();
        };
        let note = if description.channel_unavailable() {
            format!(
                "{}: saved channel unavailable; using the existing mix",
                description.name
            )
        } else if let Some(previous) = &description.fallback_from {
            format!(
                "Using {} because {previous} is unavailable",
                description.name
            )
        } else if description.channels == 1 {
            format!("{} · one input channel", description.name)
        } else {
            format!(
                "{} · all channels are mixed until you select one",
                description.name
            )
        };
        let menu = self.microphone_channel_picker_open.then(|| {
            let source = description.clone();
            selection_picker_menu(
                "microphone-channel-picker",
                &self.microphone_channel_picker_state,
                std::iter::once(None)
                    .chain((1..=source.channels).map(Some))
                    .collect(),
                source.channel,
                |channel| {
                    channel.map_or_else(
                        || "Mix channels".into(),
                        |channel| format!("Channel {channel}"),
                    )
                },
                self.feedback_error(SettingControl::Channel),
                (
                    cx.listener(move |this, channel: &Option<u16>, window, cx| {
                        if this.select_microphone_channel(&source, *channel, cx)
                            || !this.microphone_channel_picker_open
                        {
                            this.microphone_channel_picker_state.trigger.focus(window);
                        }
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.microphone_channel_picker_open = false;
                        cx.notify();
                    }),
                    cx.listener(Self::microphone_channel_picker_key),
                ),
            )
        });
        let selectable = description.device_id.is_some()
            && (description.channels > 1 || description.requested_channel.is_some());
        if !selectable {
            return self
                .setting_row(
                    SettingControl::Channel,
                    "Input channel",
                    note,
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child(description.channel_label()),
                )
                .into_any_element();
        }
        self.setting_row(
            SettingControl::Channel,
            "Input channel",
            note,
            div()
                .relative()
                .flex_none()
                .child(
                    disclosure_button(description.channel_label())
                        .id("microphone-channel")
                        .track_focus(&self.microphone_channel_picker_state.trigger)
                        .focus(|style| style.border_color(rgb(ACCENT)))
                        .when(selectable, |button| {
                            button
                                .on_click(cx.listener(|this, event, window, cx| {
                                    if matches!(event, gpui::ClickEvent::Mouse(_)) {
                                        this.toggle_microphone_channel_picker(window, cx);
                                    }
                                }))
                                .on_key_down(cx.listener(|this, event, window, cx| {
                                    if picker_open_key(event) {
                                        this.toggle_microphone_channel_picker(window, cx);
                                        cx.stop_propagation();
                                    }
                                }))
                        }),
                )
                .children(menu.map(picker_popup)),
        )
        .into_any_element()
    }

    pub(super) fn render_microphone_diagnostic(&self) -> AnyElement {
        let Some(report) = &self.microphone_diagnostic else {
            return settings_row(
                "Input levels",
                "Measured from the last analyzed recording, before silence trimming",
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child("Record a short dictation first"),
            )
            .into_any_element();
        };
        let db = |value: Option<f64>| {
            value.map_or_else(|| "−∞ dBFS".into(), |value| format!("{value:.1} dBFS"))
        };
        let source = report.input.as_ref().map_or_else(
            || "Input not identified".into(),
            |input| format!("{} · {}", input.name, input.channel_label()),
        );
        div()
            .border_b_1()
            .border_color(rgb(LINE))
            .child(
                settings_row(
                    "Input levels",
                    format!(
                        "Last analyzed recording: {source} · {:.1} s",
                        report.duration_ms as f64 / 1_000.0
                    ),
                    level_meter(
                        report.levels.rms_dbfs(),
                        report.levels.peak_dbfs(),
                        report.levels.warning().is_none(),
                        format!(
                            "RMS {} · Peak {}",
                            db(report.levels.rms_dbfs()),
                            db(report.levels.peak_dbfs())
                        ),
                    ),
                )
                .border_b_0(),
            )
            .when_some(report.levels.warning(), |panel, warning| {
                panel.child(
                    div()
                        .px_4()
                        .pb_3()
                        .text_size(px(11.0))
                        .text_color(rgb(NEGATIVE))
                        .child(warning),
                )
            })
            .into_any_element()
    }

    pub(super) fn toggle_trim_silence(&mut self, cx: &mut Context<Self>) {
        let result = self
            .openrouter_settings
            .update(cx, |settings, cx| settings.toggle_trim(cx));
        self.settings_feedback = Some(SettingsFeedback {
            control: SettingControl::Trim,
            success: result.is_ok(),
            message: result.err().unwrap_or_else(|| "Saved.".into()),
        });
        cx.notify();
    }

    pub(super) fn render_microphone(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let permission_warnings = self.render_permission_warnings(cx);
        let microphone_channel = self.render_microphone_channel(cx);
        let microphone_diagnostic = self.render_microphone_diagnostic();
        let trim_enabled = self.openrouter_settings.read(cx).trim_silence();
        let trim_control = div()
            .id("microphone-trim-silence")
            .flex_none()
            .cursor_pointer()
            .child(toggle(if trim_enabled { 1.0 } else { 0.0 }))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_trim_silence(cx)));
        let release_microphone_position = self.release_microphone_toggle.render_position(window);
        let microphone_label = self
            .settings
            .microphone
            .clone()
            .unwrap_or_else(|| "Automatic".into());
        let microphone_picker = self
            .microphone_picker_open
            .then(|| self.render_microphone_picker(cx));
        let recording_audio_position = self.recording_audio_spring.render_position(window);
        let audio_widths = [crate::desktop_ui::settings_segment_width(4); 4];
        let audio_behavior = sliding_segmented_control(recording_audio_position, &audio_widths)
            .children(
                [
                    RecordingAudioBehavior::Mute,
                    RecordingAudioBehavior::LowerVolume,
                    RecordingAudioBehavior::PauseMedia,
                    RecordingAudioBehavior::DoNothing,
                ]
                .into_iter()
                .enumerate()
                .map(|(index, behavior)| {
                    let selected = self.settings.recording_audio_behavior == behavior;
                    sliding_segmented_item(audio_widths[index], selected)
                        .id(("recording-audio-behavior", index))
                        .child(behavior.label())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.settings.recording_audio_behavior != behavior
                                && this.update_settings(
                                    SettingControl::AudioBehavior,
                                    cx,
                                    |settings| settings.recording_audio_behavior = behavior,
                                )
                            {
                                this.recording_audio_spring.set_target(index as f32);
                            }
                        }))
                }),
            );
        let microphone_mode = sliding_segmented_control(
            release_microphone_position,
            &[crate::desktop_ui::settings_segment_width(2); 2],
        )
        .children(
            [("Keep ready (fast)", false), ("Release when idle", true)]
                .into_iter()
                .enumerate()
                .map(|(index, (label, release))| {
                    sliding_segmented_item(
                        crate::desktop_ui::settings_segment_width(2),
                        self.settings.release_microphone_while_idle == release,
                    )
                    .id(("microphone-mode", index))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.settings.release_microphone_while_idle != release
                            && this.update_settings(
                                SettingControl::MicrophoneMode,
                                cx,
                                |settings| settings.release_microphone_while_idle = release,
                            )
                        {
                            this.release_microphone_toggle.set_enabled(release);
                        }
                    }))
                }),
        );
        configuration_pane("Microphone", "microphone-scroll", div().children(permission_warnings)
                .child(settings_section_label("INPUT AND RECORDING"))
                .child(
                    settings_panel()
                        .child(self.setting_row(SettingControl::Microphone,
                            "Input device",
                            self.input_device_description(),
                            div()
                                .relative()
                                .flex_none()
                                .child(
                                    disclosure_button(microphone_label)
                                        .id("microphone-setting").debug_selector(|| "microphone-setting".into())
                                        .track_focus(&self.microphone_picker_state.trigger)
                                        .focus(|style| style.border_color(rgb(ACCENT)))
                                        .on_click(cx.listener(|this, event, window, cx| {
                if matches!(event, gpui::ClickEvent::Mouse(_)) {
                    this.toggle_microphone_picker(window, cx);
                }
            }))
                                        .on_key_down(cx.listener(|this, event, window, cx| {
                                            if picker_open_key(event) {
                                                this.toggle_microphone_picker(window, cx);
                                                cx.stop_propagation();
                                            }
                                        })),
                                )
                                .children(microphone_picker.map(picker_popup)),
                        ))
                        .child(self.microphone_priority.clone())
                        .child(microphone_channel)
                        .child(microphone_diagnostic)
                        .child(self.setting_row(SettingControl::MicrophoneMode,
                            "Microphone mode",
                            if self.settings.release_microphone_while_idle {
                                "Opens on the shortcut: the orange indicator only shows while dictating, but the first syllable can be lost"
                            } else {
                                "Keeps the microphone open so a short pre-roll catches the start of speech. Failed transcriptions keep their audio for recovery"
                            },
                            microphone_mode,
                        ))
                        .child(self.setting_row(SettingControl::Trim,
                            "Trim silence",
                            "Trims completed clips before upload. Live streaming sends continuous audio, including pauses",
                            trim_control,
                        ))
                        .child(
                            self.setting_row(SettingControl::AudioBehavior,
                                "While dictating",
                                match self.settings.recording_audio_behavior {
                                    RecordingAudioBehavior::Mute => "Fades system audio out and back in quickly; preserves detected manual volume changes",
                                    RecordingAudioBehavior::LowerVolume => "Lowers system audio with a quick fade; preserves detected manual volume changes",
                                    RecordingAudioBehavior::PauseMedia => "Pauses playing media and resumes it after dictation",
                                    RecordingAudioBehavior::DoNothing => "Leaves other audio unchanged while dictating",
                                },
                                audio_behavior,
                            )
                            .when(self.settings.recording_audio_behavior != RecordingAudioBehavior::LowerVolume, |row| row.border_b_0()),
                        )
                        .children(self.render_lower_volume_row()),
                )
        )
    }
}

/// Average and peak level on a −60…0 dBFS scale, with a plain verdict first.
pub(super) fn level_meter(
    rms: Option<f64>,
    peak: Option<f64>,
    good: bool,
    values: String,
) -> AnyElement {
    const WIDTH: f32 = 160.0;
    let position = |value: Option<f64>| {
        value.map_or(0.0, |value| {
            ((value + 60.0) / 60.0).clamp(0.0, 1.0) as f32 * WIDTH
        })
    };
    let tone = if good { POSITIVE } else { NEGATIVE };
    div()
        .flex()
        .flex_col()
        .items_end()
        .gap_1()
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(tone))
                .child(if good { "Good level" } else { "Check level" }),
        )
        .child(
            div()
                .relative()
                .w(px(WIDTH))
                .h(px(6.0))
                .rounded_full()
                .bg(rgb(SURFACE_SELECTED))
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(px(position(rms)))
                        .rounded_full()
                        .bg(rgb(tone)),
                )
                .when(peak.is_some(), |meter| {
                    meter.child(
                        div()
                            .absolute()
                            .top(px(-2.0))
                            .left(px((position(peak) - 1.0).max(0.0)))
                            .w(px(2.0))
                            .h(px(10.0))
                            .bg(rgb(TEXT_SOFT)),
                    )
                }),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(FAINT))
                .child(values),
        )
        .into_any_element()
}
