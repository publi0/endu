//! Provider credentials and global request limits.

use crate::desktop_ui::{
    LINE, NEGATIVE, SETTINGS_CONTROL_WIDTH, settings_panel, settings_row, settings_section_label,
};
use crate::openrouter::{
    Config, KeyStatus,
    settings_view::{self, ConfigChanged, KeyChanged, OpenRouterSettings},
};
use crate::providers::Provider;
use crate::text_input::{Changed, Dismissed, EditFinished, Submitted, TextInput};
use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, Subscription, Window, div, prelude::*, px,
    rgb,
};

/// No credential data is sent to the parent. It can recheck readiness and read
/// the configuration snapshot when it receives this event.
pub struct ProvidersChanged;

#[derive(Clone, Copy)]
enum MicrosoftField {
    Batch,
    Streaming,
    Deployment,
}

impl MicrosoftField {
    const ALL: [Self; 3] = [Self::Batch, Self::Streaming, Self::Deployment];

    fn index(self) -> usize {
        match self {
            Self::Batch => 0,
            Self::Streaming => 1,
            Self::Deployment => 2,
        }
    }

    fn value(self, config: &Config) -> &str {
        match self {
            Self::Batch => &config.microsoft.endpoint,
            Self::Streaming => &config.microsoft.streaming_endpoint,
            Self::Deployment => &config.microsoft.deployment,
        }
    }

    fn set(self, config: &mut Config, value: &str) {
        match self {
            Self::Batch => config.microsoft.endpoint = value.into(),
            Self::Streaming => config.microsoft.streaming_endpoint = value.into(),
            Self::Deployment => config.microsoft.deployment = value.into(),
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::Batch => "https://resource.cognitiveservices.azure.com",
            Self::Streaming => "https://resource.services.ai.azure.com",
            Self::Deployment => "Deployment name",
        }
    }
}

pub struct ProvidersView {
    preview: bool,
    config: Config,
    keys: Vec<(Provider, Entity<OpenRouterSettings>)>,
    availability: Vec<Provider>,
    microsoft: [Entity<TextInput>; 3],
    microsoft_dirty: [bool; 3],
    microsoft_errors: [Option<String>; 3],
    global: Entity<OpenRouterSettings>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<ProvidersChanged> for ProvidersView {}

impl ProvidersView {
    pub fn new(preview: bool, cx: &mut Context<Self>) -> Self {
        let loaded = if preview {
            let mut config = Config::default();
            config.microsoft.endpoint = "https://hex-preview.cognitiveservices.azure.com".into();
            config.microsoft.streaming_endpoint =
                "https://hex-preview.services.ai.azure.com".into();
            config.microsoft.deployment = "mai-transcribe-preview".into();
            Ok(config)
        } else {
            crate::openrouter::load_config()
        };
        let (config, error) = match loaded {
            Ok(config) => (config, None),
            Err(error) => (Config::default(), Some(format!("{error:#}"))),
        };
        let keys: Vec<_> = Provider::ALL
            .into_iter()
            .map(|provider| {
                let key = settings_view::new_provider_key(provider, preview, cx);
                if preview {
                    key.update(cx, |view, cx| {
                        view.sync_key_status(KeyStatus::Keychain("demo".into()), cx)
                    });
                }
                (provider, key)
            })
            .collect();
        let global = settings_view::new_global_options(preview, cx);
        global.update(cx, |view, cx| view.refresh_config(config.clone(), cx));
        for (_, key) in &keys {
            key.update(cx, |view, cx| view.refresh_config(config.clone(), cx));
        }
        let mut subscriptions = Vec::new();
        let microsoft = MicrosoftField::ALL.map(|field| {
            let input = cx.new(|cx| {
                TextInput::new(cx, field.placeholder(), field.value(&config)).commit_on_blur()
            });
            subscriptions.push(cx.subscribe(&input, move |this, _, _: &Changed, cx| {
                this.microsoft_dirty[field.index()] = true;
                this.microsoft_errors[field.index()] = None;
                cx.notify();
            }));
            subscriptions.push(cx.subscribe(&input, move |this, _, _: &Submitted, cx| {
                this.save_microsoft(field, cx);
            }));
            subscriptions.push(cx.subscribe(&input, move |this, _, _: &EditFinished, cx| {
                this.save_microsoft(field, cx);
            }));
            subscriptions.push(cx.subscribe(&input, move |this, _, _: &Dismissed, cx| {
                this.microsoft_dirty[field.index()] = false;
                this.microsoft_errors[field.index()] = None;
                this.load_microsoft(cx);
                cx.notify();
            }));
            input
        });
        for (_, key) in &keys {
            subscriptions.push(cx.observe(key, |this, _, cx| {
                // Initial asynchronous lookups notify without KeyChanged.
                // Availability events carry identities only, never keys.
                let available = this.available_providers(cx);
                if this.availability != available {
                    this.availability = available;
                    cx.emit(ProvidersChanged);
                }
                cx.notify();
            }));
            subscriptions.push(cx.subscribe(key, |this, _, _: &KeyChanged, cx| {
                // Keychain migration may remove a plaintext key. Re-read only
                // the small config file, after the operation has completed.
                if !this.preview {
                    match crate::openrouter::load_config() {
                        Ok(config) => this.refresh(config, cx),
                        Err(error) => this.error = Some(format!("{error:#}")),
                    }
                }
                cx.emit(ProvidersChanged);
                cx.notify();
            }));
        }
        subscriptions.push(
            cx.subscribe(&global, |this, editor, _: &ConfigChanged, cx| {
                // More than one editor can flush during close. Read the current
                // snapshot instead of replaying an earlier queued change.
                let config = editor.read(cx).config_snapshot();
                this.refresh(config, cx);
                cx.emit(ProvidersChanged);
            }),
        );
        subscriptions.push(cx.observe(&global, |_, _, cx| cx.notify()));
        Self {
            preview,
            config,
            keys,
            availability: if preview {
                Provider::ALL.to_vec()
            } else {
                Vec::new()
            },
            microsoft,
            microsoft_dirty: [false; 3],
            microsoft_errors: std::array::from_fn(|_| None),
            global,
            error,
            _subscriptions: subscriptions,
        }
    }
    pub fn config_snapshot(&self) -> Config {
        self.config.clone()
    }
    /// Read cached UI state only. Unknown or missing keys are unavailable.
    pub fn available_providers(&self, cx: &gpui::App) -> Vec<Provider> {
        self.keys
            .iter()
            .filter_map(|(provider, key)| {
                matches!(
                    key.read(cx).key_status(),
                    Some(KeyStatus::Keychain(_) | KeyStatus::ConfigFile | KeyStatus::Environment)
                )
                .then_some(*provider)
            })
            .collect()
    }

    pub fn has_key_operation(&self, cx: &gpui::App) -> bool {
        self.keys
            .iter()
            .any(|(_, key)| key.read(cx).has_key_operation())
    }
    pub fn refresh(&mut self, config: Config, cx: &mut Context<Self>) {
        self.config = config.clone();
        self.error = None;
        self.load_microsoft(cx);
        self.global
            .update(cx, |view, cx| view.refresh_config(config.clone(), cx));
        for (_, key) in &self.keys {
            key.update(cx, |view, cx| view.refresh_config(config.clone(), cx));
        }
        cx.notify();
    }
    pub fn apply_imported_config(&mut self, config: Config, cx: &mut Context<Self>) {
        self.config = config.clone();
        self.error = None;
        self.microsoft_dirty = [false; 3];
        self.microsoft_errors = std::array::from_fn(|_| None);
        self.load_microsoft(cx);
        self.global.update(cx, |view, cx| {
            view.apply_imported_config(config.clone(), cx)
        });
        for (_, key) in &self.keys {
            key.update(cx, |view, cx| {
                view.apply_imported_config(config.clone(), cx)
            });
        }
        cx.notify();
    }
    pub fn sync_key_status(
        &mut self,
        provider: Provider,
        status: KeyStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some((_, key)) = self.keys.iter().find(|(id, _)| *id == provider) {
            key.update(cx, |view, cx| view.sync_key_status(status, cx));
        }
        cx.emit(ProvidersChanged);
        cx.notify();
    }
    #[cfg(test)]
    pub(crate) fn stage_connection_and_timeout_drafts(
        &mut self,
        endpoint: &str,
        timeout: &str,
        cx: &mut Context<Self>,
    ) {
        self.microsoft[MicrosoftField::Batch.index()].update(cx, |input, cx| {
            input.set_text(endpoint, cx);
            cx.emit(Changed);
        });
        self.global
            .update(cx, |view, cx| view.stage_attempt_timeout_draft(timeout, cx));
    }

    pub fn finish_editing(&mut self, cx: &mut Context<Self>) {
        for (_, key) in &self.keys {
            key.update(cx, |view, cx| view.finish_editing(cx));
        }
        for field in MicrosoftField::ALL {
            self.save_microsoft(field, cx);
        }
        self.global.update(cx, |view, cx| view.finish_editing(cx));
        self.refresh(self.global.read(cx).config_snapshot(), cx);
    }
    fn load_microsoft(&mut self, cx: &mut Context<Self>) {
        for field in MicrosoftField::ALL {
            if !self.microsoft_dirty[field.index()] {
                let value = field.value(&self.config).to_owned();
                self.microsoft[field.index()].update(cx, |input, cx| input.set_text(value, cx));
            }
        }
    }

    fn save_microsoft(&mut self, field: MicrosoftField, cx: &mut Context<Self>) {
        let index = field.index();
        if !self.microsoft_dirty[index] {
            return;
        }
        let value = self.microsoft[index].read(cx).text().trim().to_owned();
        let edit = |base: &Config| {
            let mut config = base.clone();
            field.set(&mut config, &value);
            config.microsoft.validate()?;
            Ok::<_, String>(config)
        };
        let saved = if self.preview {
            edit(&self.config)
        } else {
            crate::openrouter::update_config(|base| {
                edit(base).map_err(|error| color_eyre::eyre::eyre!("{error}"))
            })
            .map_err(|error| format!("{error:#}"))
        };
        match saved {
            Ok(config) => {
                self.microsoft_dirty[index] = false;
                self.microsoft_errors[index] = None;
                self.refresh(config, cx);
                cx.emit(ProvidersChanged);
            }
            Err(error) => {
                self.microsoft_errors[index] = Some(error);
                cx.notify();
            }
        }
    }

    fn render_microsoft(&self) -> gpui::Div {
        let fields = [
            (
                MicrosoftField::Batch,
                "Batch endpoint",
                "HTTPS resource root for recorded audio",
            ),
            (
                MicrosoftField::Streaming,
                "Streaming endpoint",
                "HTTPS resource root for live transcription",
            ),
            (
                MicrosoftField::Deployment,
                "Deployment",
                "Deployment name for live transcription",
            ),
        ];
        div().children(fields.into_iter().map(|(field, title, description)| {
            let index = field.index();
            div()
                .child(settings_row(
                    title,
                    description,
                    div()
                        .debug_selector(move || format!("microsoft-connection-{index}"))
                        .w(px(SETTINGS_CONTROL_WIDTH))
                        .flex_none()
                        .child(self.microsoft[index].clone()),
                ))
                .children(self.microsoft_errors[index].clone().map(|error| {
                    div()
                        .px_4()
                        .pb_3()
                        .text_size(px(11.0))
                        .text_color(rgb(NEGATIVE))
                        .child(error)
                }))
        }))
    }

    pub fn close_pickers(&mut self, cx: &mut Context<Self>) {
        self.global.update(cx, |view, cx| view.close_pickers(cx));
    }
}

impl Render for ProvidersView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .debug_selector(|| "providers-credentials-and-limits".into())
            .children(self.error.clone().map(|error| {
                div()
                    .px_1()
                    .py_2()
                    .text_size(px(12.0))
                    .text_color(rgb(NEGATIVE))
                    .child(error)
            }))
            .child(settings_section_label("PROVIDER KEYS"))
            .child(
                settings_panel().children(self.keys.iter().enumerate().map(
                    |(index, (provider, key))| {
                        div()
                            .debug_selector(move || format!("provider-key-{index}"))
                            .child(key.clone())
                            .when(*provider == Provider::Microsoft, |row| {
                                row.child(self.render_microsoft())
                            })
                            .when(index + 1 < self.keys.len(), |row| {
                                row.border_b_1().border_color(rgb(LINE))
                            })
                    },
                )),
            )
            .child(self.global.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn preview_keys_are_isolated_and_status_sync_targets_one_provider(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| ProvidersView::new(true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.keys.len(), Provider::ALL.len());
                assert!(!view.has_key_operation(cx));
                assert_eq!(view.available_providers(cx), Provider::ALL.to_vec());
                view.sync_key_status(Provider::Deepgram, KeyStatus::Missing, cx);
                for (provider, key) in &view.keys {
                    if *provider == Provider::Deepgram {
                        assert_eq!(key.read(cx).key_status(), Some(&KeyStatus::Missing));
                    } else {
                        assert_eq!(
                            key.read(cx).key_status(),
                            Some(&KeyStatus::Keychain("demo".into()))
                        );
                    }
                }
            })
        });
    }

    #[gpui::test]
    fn lookup_notifications_update_availability_without_key_change_events(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::{cell::RefCell, rc::Rc};
        let (view, cx) = cx.add_window_view(|_, cx| ProvidersView::new(true, cx));
        let updates = Rc::new(RefCell::new(Vec::new()));
        let recorded = updates.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, move |view, _: &ProvidersChanged, cx| {
                recorded
                    .borrow_mut()
                    .push(view.read(cx).available_providers(cx));
            })
        });
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                for (_, key) in &view.keys {
                    key.update(cx, |key, cx| key.sync_key_status(KeyStatus::Missing, cx));
                }
            })
        });
        cx.run_until_parked();
        assert!(updates.borrow().last().unwrap().is_empty());
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                for (provider, key) in &view.keys {
                    if *provider == Provider::OpenRouter {
                        key.update(cx, |key, cx| key.sync_key_status(KeyStatus::ConfigFile, cx));
                    } else if *provider == Provider::OpenAi {
                        key.update(cx, |key, cx| {
                            key.sync_key_status(KeyStatus::Environment, cx)
                        });
                    }
                }
            })
        });
        cx.run_until_parked();
        assert_eq!(
            updates.borrow().last().unwrap(),
            &[Provider::OpenRouter, Provider::OpenAi]
        );
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.sync_key_status(Provider::OpenAi, KeyStatus::Missing, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(updates.borrow().last().unwrap(), &[Provider::OpenRouter]);
    }

    #[gpui::test]
    fn microsoft_connection_edits_validate_on_blur_and_enter_escape_cancels(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::Focusable;
        use std::{cell::RefCell, rc::Rc};
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| ProvidersView::new(true, cx));
        cx.simulate_resize(gpui::size(px(760.0), px(2400.0)));
        let changes = Rc::new(RefCell::new(0));
        let recorded = changes.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, move |_, _: &ProvidersChanged, _| {
                *recorded.borrow_mut() += 1;
            })
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| view.read(cx).microsoft[0].focus_handle(cx).focus(window));
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("https://edited.cognitiveservices.azure.com");
        cx.update(|_, cx| {
            assert!(
                view.read(cx)
                    .config
                    .microsoft
                    .endpoint
                    .contains("hex-preview")
            )
        });
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).config.microsoft.endpoint,
                "https://edited.cognitiveservices.azure.com"
            );
            assert_eq!(
                view.read(cx).global.read(cx).config_snapshot(),
                view.read(cx).config
            );
        });
        assert_eq!(*changes.borrow(), 1);
        cx.update(|window, cx| view.read(cx).microsoft[1].focus_handle(cx).focus(window));
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("http://unsafe.services.ai.azure.com");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(view.read(cx).microsoft_errors[1].is_some());
            assert!(
                view.read(cx)
                    .config
                    .microsoft
                    .streaming_endpoint
                    .starts_with("https://hex-preview")
            );
            assert_eq!(
                view.read(cx).microsoft[1].read(cx).text(),
                "http://unsafe.services.ai.azure.com"
            );
        });
        assert_eq!(*changes.borrow(), 1);
        cx.update(|window, cx| view.read(cx).microsoft[1].focus_handle(cx).focus(window));
        cx.simulate_keystrokes("escape");
        cx.update(|_, cx| {
            assert!(!view.read(cx).microsoft_dirty[1]);
            assert!(view.read(cx).microsoft_errors[1].is_none());
            assert_eq!(
                view.read(cx).microsoft[1].read(cx).text(),
                view.read(cx).config.microsoft.streaming_endpoint
            );
        });
        cx.update(|window, cx| view.read(cx).microsoft[2].focus_handle(cx).focus(window));
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("MAI-Custom-Deployment");
        cx.simulate_keystrokes("enter");
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        assert_eq!(*changes.borrow(), 2, "Enter plus blur commits one change");
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).config.microsoft.deployment,
                "MAI-Custom-Deployment"
            )
        });
    }

    #[gpui::test]
    fn microsoft_drafts_survive_refresh_and_flush_but_import_discards_them(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| ProvidersView::new(true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.microsoft[0].update(cx, |input, cx| {
                    input.set_text("https://draft.cognitiveservices.azure.com", cx)
                });
                view.microsoft_dirty[0] = true;
                view.microsoft[2].update(cx, |input, cx| input.set_text("Draft-Deployment", cx));
                view.microsoft_dirty[2] = true;
                let mut config = view.config_snapshot();
                config.transcription.attempt_timeout_seconds = 47;
                view.refresh(config, cx);
                assert_eq!(view.microsoft[2].read(cx).text(), "Draft-Deployment");
                view.finish_editing(cx);
                assert_eq!(
                    view.config.microsoft.endpoint,
                    "https://draft.cognitiveservices.azure.com"
                );
                assert_eq!(view.config.microsoft.deployment, "Draft-Deployment");
                assert_eq!(view.config.transcription.attempt_timeout_seconds, 47);
                assert_eq!(view.global.read(cx).config_snapshot(), view.config);
                view.microsoft[2].update(cx, |input, cx| input.set_text("Obsolete-Draft", cx));
                view.microsoft_dirty[2] = true;
                let mut imported = view.config_snapshot();
                imported.microsoft.deployment = "Imported-Local-Connection".into();
                view.apply_imported_config(imported.clone(), cx);
                view.finish_editing(cx);
                assert_eq!(view.config, imported);
                assert_eq!(
                    view.microsoft[2].read(cx).text(),
                    "Imported-Local-Connection"
                );
                assert!(!view.microsoft_dirty.iter().any(|dirty| *dirty));
            })
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn seven_provider_rows_and_microsoft_controls_render_with_standard_metrics(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| ProvidersView::new(true, cx));
        cx.simulate_resize(gpui::size(px(760.0), px(2400.0)));
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(view.read(cx).keys.len(), 7));
        for selector in [
            "provider-key-0",
            "provider-key-1",
            "provider-key-2",
            "provider-key-3",
            "provider-key-4",
            "provider-key-5",
            "provider-key-6",
        ] {
            assert!(cx.debug_bounds(selector).is_some());
        }
        for selector in [
            "microsoft-connection-0",
            "microsoft-connection-1",
            "microsoft-connection-2",
        ] {
            let bounds = cx.debug_bounds(selector).unwrap();
            assert_eq!(bounds.size.width, px(SETTINGS_CONTROL_WIDTH));
            assert_eq!(bounds.size.height, px(crate::desktop_ui::CONTROL_HEIGHT));
        }
    }

    #[gpui::test]
    fn global_changes_and_imports_preserve_model_profiles(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| ProvidersView::new(true, cx));
        let mut config = Config::default();
        config.transcription.models = vec!["deepgram::nova-3".into()];
        config.transcription.model_options.insert(
            "deepgram::nova-3".into(),
            crate::providers::ModelOptions {
                language: "pt".into(),
                streaming: true,
                ..Default::default()
            },
        );
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.global.update(cx, |editor, cx| {
                    editor.apply_imported_config(config.clone(), cx);
                    cx.emit(ConfigChanged);
                });
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.config_snapshot(), config);
                assert_eq!(view.global.read(cx).config_snapshot(), config);
                let imported = Config::default();
                view.apply_imported_config(imported.clone(), cx);
                assert_eq!(view.global.read(cx).config_snapshot(), imported);
            })
        });
    }
}
