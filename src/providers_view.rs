//! Provider credentials and global request limits.

use crate::desktop_ui::{LINE, MUTED, NEGATIVE, settings_panel, settings_section_label};
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
    availability: Vec<Provider>,
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
    pub(crate) fn stage_timeout_draft(&mut self, timeout: &str, cx: &mut Context<Self>) {
        self.global
            .update(cx, |view, cx| view.stage_attempt_timeout_draft(timeout, cx));
    }

    pub fn finish_editing(&mut self, cx: &mut Context<Self>) {
        for (_, key) in &self.keys {
            key.update(cx, |view, cx| view.finish_editing(cx));
        }
        self.global.update(cx, |view, cx| view.finish_editing(cx));
        self.refresh(self.global.read(cx).config_snapshot(), cx);
    }
    pub fn close_pickers(&mut self, cx: &mut Context<Self>) {
        self.global.update(cx, |view, cx| view.close_pickers(cx));
    }
}

impl Render for ProvidersView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let advanced = self.global.update(cx, |view, cx| view.render_advanced(cx));
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
                div()
                    .px_1()
                    .mb_3()
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child("Keys added here are stored in the macOS Keychain."),
            )
            .child(settings_panel().children(self.keys.iter().enumerate().map(
                |(index, (_, key))| {
                    div()
                        .debug_selector(move || format!("provider-key-{index}"))
                        .child(key.clone())
                        .when(index + 1 < self.keys.len(), |row| {
                            row.border_b_1().border_color(rgb(LINE))
                        })
                },
            )))
            .child(
                div()
                    .debug_selector(|| "providers-advanced".into())
                    .child(advanced),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct KeyboardFixture(Entity<ProvidersView>);
    impl Render for KeyboardFixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // AppWindow provides this same Tab traversal around Providers.
            div()
                .on_key_down(|event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "tab" {
                        if event.keystroke.modifiers.shift {
                            window.focus_prev();
                        } else {
                            window.focus_next();
                        }
                        cx.stop_propagation();
                    }
                })
                .child(self.0.clone())
        }
    }

    #[gpui::test]
    fn advanced_opens_and_closes_from_the_keyboard(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (_fixture, cx) =
            cx.add_window_view(|_, cx| KeyboardFixture(cx.new(|cx| ProvidersView::new(true, cx))));
        cx.simulate_resize(gpui::size(px(760.0), px(2400.0)));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let collapsed = cx.debug_bounds("providers-advanced").unwrap();
        // All fixture keys are saved, so Advanced is the first Tab stop.
        cx.update(|window, _| {
            window.blur();
            window.focus_next();
        });
        for key in ["enter", "space"] {
            cx.simulate_keystrokes(key);
            cx.simulate_event(gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse(key).unwrap(),
            });
            cx.run_until_parked();
            let height = cx.debug_bounds("providers-advanced").unwrap().size.height;
            if key == "enter" {
                assert!(height > collapsed.size.height);
            } else {
                assert_eq!(height, collapsed.size.height);
            }
        }
    }

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
    fn advanced_is_collapsed_below_every_provider_key(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| ProvidersView::new(true, cx));
        cx.simulate_resize(gpui::size(px(760.0), px(2400.0)));
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(view.read(cx).keys.len(), Provider::ALL.len()));
        let collapsed = cx.debug_bounds("providers-advanced").unwrap();
        let header = cx.debug_bounds("openrouter-advanced").unwrap();
        assert_eq!(Provider::ALL.len(), 7);
        let last_key = cx.debug_bounds("provider-key-6").unwrap();
        assert!(collapsed.top() >= last_key.bottom());
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("providers-advanced").unwrap().size.height > collapsed.size.height);
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("openrouter-advanced").unwrap().size.height,
            header.size.height,
            "Focusing the disclosure must not replace its typography",
        );
        assert_eq!(
            cx.debug_bounds("advanced-collapsed-feedback")
                .unwrap()
                .size
                .height,
            px(0.0),
        );
        assert_eq!(
            cx.debug_bounds("providers-advanced").unwrap().size.height,
            collapsed.size.height
        );
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
