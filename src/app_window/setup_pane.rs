//! Setup gate, permission warnings and the provider key notice.

use super::*;

impl AppWindow {
    pub(super) fn render_permission_warnings(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let warnings = crate::onboarding::permission_warnings(self.setup_status);
        if self.setup_visible || warnings.is_empty() {
            return None;
        }
        Some(
            div()
                .child(settings_section_label("PERMISSIONS NEEDED"))
                .child(
                    settings_panel().children(warnings.into_iter().map(|warning| {
                        let (name, description) = permission_warning_copy(warning.kind);
                        let action = compact_button(permission_action_label(warning.action))
                            .id(permission_warning_id(warning.kind))
                            .border_1()
                            .border_color(rgb(LINE))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.perform_permission_action(warning);
                                cx.notify();
                            }));
                        settings_row(name, description, action)
                    })),
                )
                .into_any_element(),
        )
    }

    /// A banner over every pane while dictation cannot work for lack of a key.
    pub(super) fn render_key_notice(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.setup_visible || self.setup_status.api_key || self.pane == Pane::Providers {
            return None;
        }
        Some(
            div()
                .id("api-key-notice")
                .w_full()
                .flex()
                .justify_center()
                .flex_shrink_0()
                .px_8()
                .py_4()
                .bg(rgb(SURFACE))
                .border_b_1()
                .border_color(rgb(LINE))
                .child(
                    div()
                        .w_full()
                        .max_w(px(PANE_CONTENT_WIDTH))
                        .flex()
                        .items_center()
                        .gap_4()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .border_l_2()
                                .border_color(rgb(ACCENT))
                                .pl_3()
                                .child(settings_copy(
                                    "Connect a provider to start dictating",
                                    "Add a key in Providers, then choose its models in Models. Keys stay in your Keychain.",
                                )),
                        )
                        .child(
                            compact_button("Open Providers")
                                .id("open-key-models")
                                .flex_none()
                                .bg(rgb(ACCENT))
                                .text_color(rgb(TEXT))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.select_pane(Pane::Providers, cx);
                                    this.focus_pane(window);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn perform_permission_action(&mut self, warning: PermissionWarning) {
        if self.preview {
            return;
        }
        match warning.action {
            PermissionAction::RequestMicrophone => crate::onboarding::request_microphone(),
            PermissionAction::OpenMicrophoneSettings => {
                crate::onboarding::open_permission_settings("microphone");
            }
            PermissionAction::OpenInputMonitoringSettings => {
                crate::permission_guide::show_input_monitoring();
            }
            PermissionAction::OpenAccessibilitySettings => {
                crate::permission_guide::show_accessibility();
            }
        }
        self.permission_refresh_at = Instant::now() + PERMISSION_REFRESH_INTERVAL;
    }

    pub(super) fn render_setup(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.setup_status;
        let mut permission_rows = Vec::new();
        if status.microphone != PermissionState::Ready {
            let action = match status.microphone {
                PermissionState::NeedsSettings => compact_button("Open Settings")
                    .id("setup-microphone")
                    .bg(rgb(SURFACE_SELECTED))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.preview {
                            crate::onboarding::open_permission_settings("microphone");
                        }
                        cx.notify();
                    })),
                PermissionState::NeedsRequest | PermissionState::Ready => {
                    compact_button("Grant Access")
                        .id("setup-microphone")
                        .bg(rgb(SURFACE_SELECTED))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.preview {
                                crate::onboarding::request_microphone();
                            }
                            cx.notify();
                        }))
                }
            };
            permission_rows.push(setup_row(
                "Microphone",
                "Capture your voice while you dictate.",
                action.into_any_element(),
            ));
        }
        if status.input_monitoring != PermissionState::Ready {
            permission_rows.push(setup_row(
                "Input Monitoring",
                "Recognize the dictation shortcut in any app.",
                compact_button("Grant Access")
                    .id("setup-input-monitoring")
                    .bg(rgb(SURFACE_SELECTED))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.preview {
                            crate::permission_guide::show_input_monitoring();
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            ));
        }
        if status.accessibility != PermissionState::Ready {
            permission_rows.push(setup_row(
                "Accessibility",
                "Paste the transcript into the app you are using.",
                compact_button("Grant Access")
                    .id("setup-accessibility")
                    .bg(rgb(SURFACE_SELECTED))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.preview {
                            crate::permission_guide::show_accessibility();
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            ));
        }
        div()
            .id("setup-backdrop")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .py_4()
            .bg(rgba(0x000000dd))
            .child(
                // Scrolls instead of clipping when the window is shorter than
                // the sheet with every permission still missing.
                div()
                    .id("setup")
                    .w(px(640.0))
                    .max_h_full()
                    .overflow_y_scroll()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(LINE))
                    .bg(rgb(CANVAS))
                    .child(
                        div()
                            .px_7()
                            .pt_7()
                            .pb_5()
                            .child(
                                div()
                                    .text_size(px(22.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Set up Hex"),
                            )
                            .child(
                                div()
                                    .pt_2()
                                    .text_size(px(12.0))
                                    .line_height(px(19.0))
                                    .text_color(rgb(MUTED))
                                    .child("Tap the shortcut to keep recording and tap again to stop, or hold and release. Hex trims silence, transcribes with your selected provider, and pastes the text once these permissions and your key are in place."),
                            ),
                    )
                    .when(!permission_rows.is_empty(), |setup| {
                        setup.child(
                            div()
                                .mx_7()
                                .pb_5()
                                .child(setup_group_label("PERMISSIONS"))
                                .child(
                                    div()
                                        .border_t_1()
                                        .border_color(rgb(LINE))
                                        .children(permission_rows),
                                ),
                        )
                    })
                    .child(
                        div()
                            .mx_7()
                            .pb_6()
                            .child(setup_group_label("PROVIDER"))
                            .child(if status.api_key {
                                div()
                                    .border_t_1()
                                    .border_color(rgb(LINE))
                                    .child(setup_row(
                                        "Provider ready",
                                        "Choose models and fallbacks in Models.",
                                        setup_ready_badge(),
                                    ))
                                    .into_any_element()
                            } else {
                                div().child(self.openrouter_setup.clone())
                                    .child(div().mt_2().flex().child(
                                        compact_button("Use another provider…").id("setup-choose-provider")
                                            .border_1()
                                            .border_color(rgb(LINE))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.setup_visible = false;
                                                this.select_pane(Pane::Providers, cx);
                                                this.focus_pane(window);
                                            })),
                                    )).into_any_element()
                            }),
                    ),
            )
            .into_any_element()
    }
}

pub(super) fn setup_row(
    title: &'static str,
    description: &'static str,
    control: AnyElement,
) -> Div {
    div()
        .w_full()
        .min_h(px(70.0))
        .py_3()
        .flex()
        .items_center()
        .gap_4()
        .border_b_1()
        .border_color(rgb(LINE))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(settings_copy(title, description)),
        )
        .child(div().flex_shrink_0().child(control))
}

pub(super) fn setup_group_label(label: &'static str) -> Div {
    div()
        .pb_2()
        .text_size(px(10.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .child(label)
}

pub(super) fn setup_ready_badge() -> AnyElement {
    div()
        .h(px(28.0))
        .px_3()
        .flex()
        .items_center()
        .rounded_sm()
        .bg(rgb(0x17231a))
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(0x91bd99))
        .child("Ready")
        .into_any_element()
}

pub(super) const fn permission_warning_copy(kind: PermissionKind) -> (&'static str, &'static str) {
    match kind {
        PermissionKind::Microphone => (
            "Microphone access is off",
            "Hex cannot record dictation until microphone access is restored.",
        ),
        PermissionKind::InputMonitoring => (
            "Input Monitoring is off",
            "Hex cannot recognize the dictation shortcut in other apps.",
        ),
        PermissionKind::Accessibility => (
            "Accessibility is off",
            "Hex cannot paste the transcript into the foreground app.",
        ),
    }
}

pub(super) const fn permission_action_label(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::OpenMicrophoneSettings => "Open Settings",
        PermissionAction::RequestMicrophone
        | PermissionAction::OpenInputMonitoringSettings
        | PermissionAction::OpenAccessibilitySettings => "Grant Access",
    }
}

pub(super) const fn permission_warning_id(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Microphone => "permission-warning-microphone",
        PermissionKind::InputMonitoring => "permission-warning-input-monitoring",
        PermissionKind::Accessibility => "permission-warning-accessibility",
    }
}
