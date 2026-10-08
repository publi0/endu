//! Preference import and export.

use super::*;

impl AppWindow {
    pub(super) fn export_preferences(&mut self, cx: &mut Context<Self>) {
        if self.preference_transfer_busy {
            return;
        }
        self.finish_editing(cx);
        self.cancel_hotkey_capture(cx);
        self.preference_transfer_busy = true;
        self.preference_transfer_error = None;
        let directory = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .join("Documents");
        let chosen = cx.prompt_for_new_path(&directory, Some("Hex-preferences.json"));
        let settings = self.settings.clone();
        let fixture = self
            .preview
            .then(|| self.openrouter_settings.read(cx).config_snapshot());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = match chosen.await {
                Ok(Ok(Some(path))) => {
                    cx.background_executor()
                        .spawn(async move {
                            let config = match fixture {
                                Some(config) => config,
                                None => crate::openrouter::load_config()
                                    .map_err(|_| "Could not read Models preferences.".to_owned())?,
                            };
                            let bytes =
                                crate::preferences_transfer::export_bytes(&settings, &config)
                                    .map_err(|error| error.to_string())?;
                            write_preferences_export(&path, &bytes)
                        })
                        .await
                }
                Ok(Ok(None)) => Ok(()),
                _ => Err("Could not open the export dialog.".to_owned()),
            };
            let _ = this.update(cx, |this, cx| {
                this.preference_transfer_busy = false;
                this.preference_transfer_error = result.err();
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn key_operation_pending(&self, cx: &App) -> bool {
        self.openrouter_settings.read(cx).has_key_operation()
            || self.openrouter_setup.read(cx).has_key_operation()
            || self.providers.read(cx).has_key_operation(cx)
    }

    pub(super) fn import_preferences(&mut self, cx: &mut Context<Self>) {
        if self.preference_transfer_busy {
            return;
        }
        if self.key_operation_pending(cx) {
            self.preference_transfer_error =
                Some("Wait for the key operation to finish before importing preferences.".into());
            cx.notify();
            return;
        }
        self.cancel_hotkey_capture(cx);
        self.preference_transfer_busy = true;
        self.preference_transfer_error = None;
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import Hex preferences".into()),
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = match chosen.await {
                Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                    Some(path) => {
                        cx.background_executor()
                            .spawn(async move { read_preferences_export(&path).map(Some) })
                            .await
                    }
                    None => Ok(None),
                },
                Ok(Ok(None)) => Ok(None),
                _ => Err("Could not open the import dialog.".to_owned()),
            };
            let _ = this.update(cx, |this, cx| {
                this.preference_transfer_busy = false;
                match result {
                    Ok(Some(bundle)) => {
                        let imported = if this.key_operation_pending(cx) {
                            Err(color_eyre::eyre::eyre!(
                                "Wait for the key operation to finish before importing preferences."
                            ))
                        } else if this.preview {
                            crate::preferences_transfer::preview_bundle(
                                bundle,
                                &this.settings,
                                &this.openrouter_settings.read(cx).config_snapshot(),
                            )
                        } else {
                            crate::preferences_transfer::import_bundle(bundle, &this.settings)
                        };
                        match imported {
                            Ok(imported) => this.accept_imported_preferences(imported, cx),
                            Err(error) => this.preference_transfer_error = Some(error.to_string()),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => this.preference_transfer_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn accept_imported_preferences(
        &mut self,
        imported: crate::preferences_transfer::ImportOutcome,
        cx: &mut Context<Self>,
    ) {
        self.cancel_hotkey_capture(cx);
        self.settings = imported.settings;
        self.settings_error = None;
        self.settings_feedback = None;
        self.preference_transfer_error = None;
        self.microphone_picker_open = false;
        self.microphone_channel_picker_open = false;
        self.microphone_picker_error = None;
        self.history_retention_open = false;
        self.release_microphone_toggle
            .set_enabled(self.settings.release_microphone_while_idle);
        self.double_tap_only_visibility.set_enabled(
            self.settings.dictation_mode == DictationMode::DoubleTap
                && self.settings.dictation_hotkey.key.is_some(),
        );
        self.dock_icon_toggle
            .set_enabled(self.settings.show_dock_icon);
        self.recording_audio_spring
            .set_target(recording_audio_index(self.settings.recording_audio_behavior) as f32);
        self.lower_volume_input.update(cx, |input, cx| {
            input.set_text(self.settings.lower_volume_percent.to_string(), cx);
        });
        for kind in [HotkeyKind::Dictation, HotkeyKind::PasteLast] {
            let index = hotkey_kind_index(kind);
            let side = hotkey_binding(&self.settings, kind).and_then(standalone_modifier_side);
            self.hotkey_side_animations[index].set_enabled(side.is_some());
            self.hotkey_side_selection_springs[index]
                .set_target(hotkey_side_index(side.unwrap_or(ModifierSide::Either)) as f32);
        }
        let hud = self.settings.hud;
        let sounds = self.settings.effective_sound_volumes();
        let priority = self.settings.microphone_priority.clone();
        self.hud_settings
            .update(cx, |view, cx| view.set_preferences(hud, None, cx));
        self.sound_settings
            .update(cx, |view, cx| view.set_preferences(sounds, None, cx));
        self.microphone_priority.update(cx, |view, cx| {
            view.close_picker(cx);
            view.set_preferences(priority, None, cx);
        });
        self.shared_keywords
            .update(cx, |view, cx| view.set_models(&imported.config, cx));
        self.providers.update(cx, |view, cx| {
            view.apply_imported_config(imported.config.clone(), cx)
        });
        self.model_options.update(cx, |view, cx| {
            view.apply_imported_config(imported.config.clone(), cx)
        });
        self.openrouter_setup.update(cx, |view, cx| {
            view.apply_imported_config(imported.config.clone(), cx)
        });
        self.openrouter_settings.update(cx, |view, cx| {
            view.apply_imported_config(imported.config, cx)
        });
        let preferences = self.settings.post_processing;
        self.post_processing_view
            .update(cx, |view, cx| view.set_preferences(preferences, None, cx));
        let vocabulary = self.settings.vocabulary.clone();
        self.shared_keywords.update(cx, |view, cx| {
            view.set_preferences(vocabulary.clone(), None, cx)
        });
        self.post_processing_view
            .update(cx, |view, cx| view.set_vocabulary(vocabulary, None, cx));
        self.refresh_microphone_description();
        // While the preferences window is open its Dock icon stays available;
        // the existing close/drop path applies the imported background setting.
        cx.notify();
    }

    pub(super) fn render_preference_transfer(&self, cx: &mut Context<Self>) -> AnyElement {
        let key_busy = self.key_operation_pending(cx);
        let actions =
            div().flex_none().flex().gap_2().children(
                ["Export", "Import"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        compact_button(label)
                            .id(("preferences-transfer", index))
                            .track_focus(&self.preference_transfer_focus[index].clone().tab_stop(
                                !(self.preference_transfer_busy || index == 1 && key_busy),
                            ))
                            .border_1()
                            .border_color(rgb(LINE))
                            .focus(|style| style.border_color(rgb(ACCENT)))
                            .when(
                                self.preference_transfer_busy || (index == 1 && key_busy),
                                |button| button.opacity(0.4),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index == 0 {
                                    this.export_preferences(cx);
                                } else {
                                    this.import_preferences(cx);
                                }
                            }))
                    }),
            );
        settings_panel().child(settings_row(
            "Import / export",
            "App and model preferences. Keys, API address, history and permissions stay on this Mac.",
            actions,
        ).border_b_0())
        .when_some(self.preference_transfer_error.clone(), |panel, error| panel.child(
            div().px_4().pb_3().text_size(px(11.0)).text_color(rgb(NEGATIVE)).child(error)
        )).into_any_element()
    }
}

pub(super) fn preferences_path_allowed(path: &std::path::Path) -> bool {
    let resolved = path
        .canonicalize()
        .ok()
        .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)));
    let Some(resolved) = resolved else {
        return false;
    };
    let mut protected = Vec::new();
    if let Ok(path) = crate::app_paths::support_dir() {
        protected.push(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        protected.push(
            std::path::PathBuf::from(home).join("Library/Application Support/hex-openrouter"),
        );
    }
    !protected
        .into_iter()
        .any(|root| resolved.starts_with(root.canonicalize().unwrap_or(root)))
}

pub(super) fn read_preferences_export(
    path: &std::path::Path,
) -> Result<crate::preferences_transfer::PreferenceBundle, String> {
    use std::io::Read;
    if !preferences_path_allowed(path) {
        return Err("Choose a preferences export outside Hex's application data.".into());
    }
    let file =
        std::fs::File::open(path).map_err(|_| "Could not open the preferences file.".to_owned())?;
    let mut bytes = Vec::new();
    file.take(crate::preferences_transfer::MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the preferences file.".to_owned())?;
    crate::preferences_transfer::decode(&bytes).map_err(|error| error.to_string())
}

pub(super) fn write_preferences_export(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    if !preferences_path_allowed(path) {
        return Err("Choose an export location outside Hex's application data.".into());
    }
    let temporary = path.with_extension(format!(
        "hex-export-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    write_preferences_export_to(path, &temporary, bytes)
}

pub(super) fn write_preferences_export_to(
    path: &std::path::Path,
    temporary: &std::path::Path,
    bytes: &[u8],
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temporary)
        .map_err(|_| "Could not create the preferences export.".to_owned())?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.map_err(|_| "Could not write the preferences file.".to_owned())
}
