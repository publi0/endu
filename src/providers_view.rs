//! Provider credentials, per-model options and global request limits.

use crate::desktop_ui::{LINE, NEGATIVE, settings_panel, settings_section_label};
use crate::model_options_view::ModelOptionsView;
use crate::openrouter::{
    Config, KeyStatus,
    settings_view::{self, ConfigChanged, KeyChanged, OpenRouterSettings},
};
use crate::providers::Provider;
use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, Subscription, Window, div, prelude::*, px,
    rgb,
};

/// No credential data is sent to the parent. It can recheck readiness and read
/// the configuration snapshot when it receives this event.
pub struct ProvidersChanged;

pub struct ProvidersView {
    preview: bool,
    config: Config,
    keys: Vec<(Provider, Entity<OpenRouterSettings>)>,
    options: Entity<ModelOptionsView>,
    global: Entity<OpenRouterSettings>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<ProvidersChanged> for ProvidersView {}

impl ProvidersView {
    pub fn new(preview: bool, cx: &mut Context<Self>) -> Self {
        let loaded = if preview {
            Ok(Config::default())
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
                (
                    provider,
                    settings_view::new_provider_key(provider, preview, cx),
                )
            })
            .collect();
        let options = cx.new(|cx| ModelOptionsView::new(config.clone(), preview, cx));
        let global = settings_view::new_global_options(preview, cx);
        let mut subscriptions = Vec::new();
        for (_, key) in &keys {
            subscriptions.push(cx.observe(key, |_, _, cx| cx.notify()));
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
            cx.subscribe(&options, |this, _, event: &ConfigChanged, cx| {
                this.refresh(event.0.clone(), cx);
                cx.emit(ProvidersChanged);
            }),
        );
        subscriptions.push(cx.subscribe(&global, |this, _, event: &ConfigChanged, cx| {
            this.refresh(event.0.clone(), cx);
            cx.emit(ProvidersChanged);
        }));
        subscriptions.push(cx.observe(&options, |_, _, cx| cx.notify()));
        subscriptions.push(cx.observe(&global, |_, _, cx| cx.notify()));
        Self {
            preview,
            config,
            keys,
            options,
            global,
            error,
            _subscriptions: subscriptions,
        }
    }
    pub fn config_snapshot(&self) -> Config {
        self.config.clone()
    }
    pub fn has_key_operation(&self, cx: &gpui::App) -> bool {
        self.keys
            .iter()
            .any(|(_, key)| key.read(cx).has_key_operation())
    }
    pub fn refresh(&mut self, config: Config, cx: &mut Context<Self>) {
        self.config = config.clone();
        self.error = None;
        self.options
            .update(cx, |view, cx| view.refresh(config.clone(), cx));
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
        self.options.update(cx, |view, cx| {
            view.apply_imported_config(config.clone(), cx)
        });
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
    pub fn finish_editing(&mut self, cx: &mut Context<Self>) {
        for (_, key) in &self.keys {
            key.update(cx, |view, cx| view.finish_editing(cx));
        }
        self.options.update(cx, |view, cx| {
            view.finish_editing(cx);
        });
        self.refresh(self.options.read(cx).config_snapshot(), cx);
        self.global.update(cx, |view, cx| view.finish_editing(cx));
        self.refresh(self.global.read(cx).config_snapshot(), cx);
    }
    pub fn close_pickers(&mut self, cx: &mut Context<Self>) {
        self.options.update(cx, |view, cx| view.close_pickers(cx));
        self.global.update(cx, |view, cx| view.close_pickers(cx));
    }
}

impl Render for ProvidersView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .children(self.error.clone().map(|error| {
                div()
                    .px_1()
                    .py_2()
                    .text_size(px(12.0))
                    .text_color(rgb(NEGATIVE))
                    .child(error)
            }))
            .child(settings_section_label("PROVIDER KEYS"))
            .child(settings_panel().children(self.keys.iter().enumerate().map(
                |(index, (_, key))| {
                    div()
                        .child(key.clone())
                        .when(index + 1 < self.keys.len(), |row| {
                            row.border_b_1().border_color(rgb(LINE))
                        })
                },
            )))
            .child(self.options.clone())
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
                assert_eq!(view.keys.len(), 4);
                assert!(!view.has_key_operation(cx));
                view.sync_key_status(Provider::Deepgram, KeyStatus::Keychain("demo".into()), cx);
                for (provider, key) in &view.keys {
                    if *provider == Provider::Deepgram {
                        assert_eq!(
                            key.read(cx).key_status(),
                            Some(&KeyStatus::Keychain("demo".into()))
                        );
                    } else {
                        assert_eq!(key.read(cx).key_status(), Some(&KeyStatus::Missing));
                    }
                }
            })
        });
    }

    #[gpui::test]
    fn profile_changes_and_imports_synchronize_global_and_model_editors(
        cx: &mut gpui::TestAppContext,
    ) {
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
                view.options
                    .update(cx, |_, cx| cx.emit(ConfigChanged(config.clone())));
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.config_snapshot(), config);
                assert_eq!(view.global.read(cx).config_snapshot(), config);
                assert_eq!(view.options.read(cx).config_snapshot(), config);
                let imported = Config::default();
                view.apply_imported_config(imported.clone(), cx);
                assert_eq!(view.global.read(cx).config_snapshot(), imported);
                assert_eq!(view.options.read(cx).config_snapshot(), imported);
            })
        });
    }
}
