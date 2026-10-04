//! Fork: the OpenRouter section of Settings. A self-contained GPUI view that
//! `app_window` embeds with one line, so upstream UI changes rarely conflict.

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px, rgb,
};

use super::form::Form;
use super::{Config, KeyStatus};
use crate::desktop_ui::{
    ACCENT, FAINT, LINE, MUTED, NEGATIVE, SURFACE_SELECTED, TEXT, TEXT_SOFT, compact_button,
    settings_panel, settings_section_label, toggle,
};
use crate::text_input::{Changed, Submitted, TextInput};

const WIDE_INPUT: f32 = 420.0;
const NARROW_INPUT: f32 = 120.0;

/// The view, or `None` outside the OpenRouter build.
pub fn new<V: 'static>(cx: &mut Context<V>) -> Option<Entity<OpenRouterSettings>> {
    super::ENABLED.then(|| cx.new(OpenRouterSettings::new))
}

#[derive(Clone, Copy)]
enum InputKind {
    Single,
    Multiline,
}

pub struct OpenRouterSettings {
    base: Config,
    key_input: Entity<TextInput>,
    key_status: Option<KeyStatus>,
    base_url: Entity<TextInput>,
    transcription_models: Entity<TextInput>,
    attempt_timeout: Entity<TextInput>,
    total_timeout: Entity<TextInput>,
    chunk_seconds: Entity<TextInput>,
    rate_limit_wait: Entity<TextInput>,
    temperature: Entity<TextInput>,
    cleanup_enabled: bool,
    cleanup_models: Entity<TextInput>,
    cleanup_timeout: Entity<TextInput>,
    cleanup_prompt: Entity<TextInput>,
    dirty: bool,
    busy: bool,
    /// `(ok, text)` for the last action.
    message: Option<(bool, String)>,
    _subscriptions: Vec<Subscription>,
}

impl OpenRouterSettings {
    fn new(cx: &mut Context<Self>) -> Self {
        let (base, message) = match super::load_config() {
            Ok(config) => (config, None),
            Err(error) => (
                Config::default(),
                Some((
                    false,
                    format!("{error:#}. Saving will replace the file with these values."),
                )),
            ),
        };
        let form = Form::from_config(&base);
        let mut subscriptions = Vec::new();
        let mut field =
            |kind: InputKind, placeholder: &'static str, value: &str, cx: &mut Context<Self>| {
                let value = value.to_owned();
                let entity = cx.new(|cx| match kind {
                    InputKind::Single => TextInput::new(cx, placeholder, &value),
                    InputKind::Multiline => TextInput::multiline(cx, placeholder, &value),
                });
                subscriptions.push(cx.subscribe(&entity, |this, _, _: &Changed, cx| {
                    this.dirty = true;
                    this.message = None;
                    cx.notify();
                }));
                entity
            };
        use InputKind::{Multiline, Single};
        let base_url = field(Single, "https://openrouter.ai/api/v1", &form.base_url, cx);
        let transcription_models = field(
            Multiline,
            "openai/whisper-large-v3-turbo",
            &form.transcription_models,
            cx,
        );
        let attempt_timeout = field(Single, "30", &form.attempt_timeout_seconds, cx);
        let total_timeout = field(Single, "90", &form.total_timeout_seconds, cx);
        let chunk_seconds = field(Single, "120", &form.chunk_seconds, cx);
        let rate_limit_wait = field(Single, "2000", &form.rate_limit_retry_max_wait_ms, cx);
        let temperature = field(Single, "provider default", &form.temperature, cx);
        let cleanup_models = field(Multiline, "openai/gpt-4o-mini", &form.cleanup_models, cx);
        let cleanup_timeout = field(Single, "15", &form.cleanup_timeout_seconds, cx);
        let cleanup_prompt = field(
            Multiline,
            "Empty uses the built-in prompt: fix punctuation and recognition errors, drop filler words, keep the wording.",
            &form.cleanup_prompt,
            cx,
        );
        let key_input = cx.new(|cx| TextInput::new(cx, "Paste a key (sk-or-v1-…)", ""));
        subscriptions.push(cx.subscribe(&key_input, |this, _, _: &Submitted, cx| {
            this.save_key(cx);
        }));
        let mut view = Self {
            base,
            key_input,
            key_status: None,
            base_url,
            transcription_models,
            attempt_timeout,
            total_timeout,
            chunk_seconds,
            rate_limit_wait,
            temperature,
            cleanup_enabled: form.cleanup_enabled,
            cleanup_models,
            cleanup_timeout,
            cleanup_prompt,
            dirty: false,
            busy: false,
            message,
            _subscriptions: subscriptions,
        };
        view.refresh_key_status(cx);
        view
    }

    fn form(&self, cx: &Context<Self>) -> Form {
        let text = |input: &Entity<TextInput>| input.read(cx).text().to_owned();
        Form {
            base_url: text(&self.base_url),
            transcription_models: text(&self.transcription_models),
            attempt_timeout_seconds: text(&self.attempt_timeout),
            total_timeout_seconds: text(&self.total_timeout),
            chunk_seconds: text(&self.chunk_seconds),
            rate_limit_retry_max_wait_ms: text(&self.rate_limit_wait),
            temperature: text(&self.temperature),
            cleanup_enabled: self.cleanup_enabled,
            cleanup_models: text(&self.cleanup_models),
            cleanup_timeout_seconds: text(&self.cleanup_timeout),
            cleanup_prompt: text(&self.cleanup_prompt),
        }
    }

    fn load_form(&mut self, form: &Form, cx: &mut Context<Self>) {
        let set = |input: &Entity<TextInput>, value: &str, cx: &mut Context<Self>| {
            input.update(cx, |input, cx| input.set_text(value, cx));
        };
        set(&self.base_url, &form.base_url, cx);
        set(&self.transcription_models, &form.transcription_models, cx);
        set(&self.attempt_timeout, &form.attempt_timeout_seconds, cx);
        set(&self.total_timeout, &form.total_timeout_seconds, cx);
        set(&self.chunk_seconds, &form.chunk_seconds, cx);
        set(
            &self.rate_limit_wait,
            &form.rate_limit_retry_max_wait_ms,
            cx,
        );
        set(&self.temperature, &form.temperature, cx);
        set(&self.cleanup_models, &form.cleanup_models, cx);
        set(&self.cleanup_timeout, &form.cleanup_timeout_seconds, cx);
        set(&self.cleanup_prompt, &form.cleanup_prompt, cx);
        self.cleanup_enabled = form.cleanup_enabled;
        self.dirty = false;
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let config = match self.form(cx).apply(&self.base) {
            Ok(config) => config,
            Err(error) => {
                self.message = Some((false, error));
                cx.notify();
                return;
            }
        };
        match super::save_config(&config) {
            Ok(()) => {
                self.base = config;
                self.dirty = false;
                self.message = Some((true, "Saved. The next dictation uses it.".into()));
            }
            Err(error) => self.message = Some((false, format!("{error:#}"))),
        }
        cx.notify();
    }

    fn revert(&mut self, cx: &mut Context<Self>) {
        match super::load_config() {
            Ok(config) => {
                self.base = config;
                let form = Form::from_config(&self.base);
                self.load_form(&form, cx);
                self.message = None;
            }
            Err(error) => self.message = Some((false, format!("{error:#}"))),
        }
        cx.notify();
    }

    fn reset_defaults(&mut self, cx: &mut Context<Self>) {
        let defaults = Config {
            api_key: self.base.api_key.clone(),
            ..Config::default()
        };
        self.load_form(&Form::from_config(&defaults), cx);
        self.dirty = true;
        self.message = Some((true, "Defaults loaded. Save to apply them.".into()));
        cx.notify();
    }

    /// Run blocking work (Keychain, network) off the UI thread.
    fn run<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { work() }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_key_status(&mut self, cx: &mut Context<Self>) {
        let config = self.base.clone();
        self.run(
            cx,
            move || super::key_status(&config),
            |this, status, _| this.key_status = Some(status),
        );
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let key = self.key_input.read(cx).text().to_owned();
        if let Err(error) = super::validate_key(&key) {
            self.message = Some((false, error.to_string()));
            cx.notify();
            return;
        }
        let config = self.base.clone();
        self.run(
            cx,
            move || super::store_keychain_key(&key).map(|()| super::key_status(&config)),
            |this, result, cx| match result {
                Ok(status) => {
                    this.key_input
                        .update(cx, |input, cx| input.set_text("", cx));
                    this.key_status = Some(status);
                    this.message = Some((true, "Key saved in the Keychain.".into()));
                }
                Err(error) => this.message = Some((false, format!("{error:#}"))),
            },
        );
    }

    fn test_key(&mut self, cx: &mut Context<Self>) {
        let config = self
            .form(cx)
            .apply(&self.base)
            .unwrap_or_else(|_| self.base.clone());
        self.run(
            cx,
            move || super::check_key(&config),
            |this, result, _| {
                this.message = Some(match result {
                    Ok(summary) => (true, summary),
                    Err(error) => (false, format!("{error:#}")),
                });
            },
        );
    }

    fn remove_key(&mut self, cx: &mut Context<Self>) {
        let config = self.base.clone();
        self.run(
            cx,
            move || super::delete_keychain_key().map(|()| super::key_status(&config)),
            |this, result, _| match result {
                Ok(status) => {
                    this.key_status = Some(status);
                    this.message = Some((true, "Key removed from the Keychain.".into()));
                }
                Err(error) => this.message = Some((false, format!("{error:#}"))),
            },
        );
    }

    fn reveal_config(&mut self, cx: &mut Context<Self>) {
        let result = super::config_path().and_then(|path| {
            std::process::Command::new("/usr/bin/open")
                .arg("-R")
                .arg(path)
                .spawn()
                .map(drop)
                .map_err(Into::into)
        });
        if let Err(error) = result {
            self.message = Some((false, format!("{error:#}")));
            cx.notify();
        }
    }
}

fn row(
    title: &'static str,
    description: impl Into<SharedString>,
    control: impl IntoElement,
) -> gpui::Div {
    div()
        .w_full()
        .min_h(px(64.0))
        .px_4()
        .py_3()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .border_b_1()
        .border_color(rgb(LINE))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child(description.into()),
                ),
        )
        .child(control)
}

fn sized(input: &Entity<TextInput>, width: f32) -> AnyElement {
    div()
        .w(px(width))
        .flex_none()
        .child(input.clone())
        .into_any_element()
}

fn button(label: &'static str, primary: bool) -> gpui::Div {
    let button = compact_button(label).border_1().border_color(rgb(LINE));
    if primary {
        button.bg(rgb(SURFACE_SELECTED)).text_color(rgb(TEXT))
    } else {
        button
    }
}

impl Render for OpenRouterSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let key_description: SharedString = match &self.key_status {
            None => "Checking the Keychain…".into(),
            Some(status) => status.label().into(),
        };
        let key_control = div()
            .w(px(WIDE_INPUT))
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.key_input.clone())
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("Remove", false)
                            .id("openrouter-remove-key")
                            .on_click(cx.listener(|this, _, _, cx| this.remove_key(cx))),
                    )
                    .child(
                        button("Test key", false)
                            .id("openrouter-test-key")
                            .on_click(cx.listener(|this, _, _, cx| this.test_key(cx))),
                    )
                    .child(
                        button("Save key", true)
                            .id("openrouter-save-key")
                            .on_click(cx.listener(|this, _, _, cx| this.save_key(cx))),
                    ),
            );
        let cleanup_toggle = div()
            .id("openrouter-cleanup-toggle")
            .flex_none()
            .child(toggle(if self.cleanup_enabled { 1.0 } else { 0.0 }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.cleanup_enabled = !this.cleanup_enabled;
                this.dirty = true;
                this.message = None;
                cx.notify();
            }));
        let status = if self.busy {
            Some((true, "Working…".to_owned()))
        } else {
            self.message.clone()
        };
        let footer = div()
            .w_full()
            .pt_3()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.0))
                    .text_color(rgb(match &status {
                        Some((false, _)) => NEGATIVE,
                        Some((true, _)) => TEXT_SOFT,
                        None => FAINT,
                    }))
                    .child(match status {
                        Some((_, text)) => SharedString::from(text),
                        None if self.dirty => "Unsaved changes.".into(),
                        None => "Saved in openrouter.json; read on every dictation.".into(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("Show file", false)
                            .id("openrouter-reveal")
                            .on_click(cx.listener(|this, _, _, cx| this.reveal_config(cx))),
                    )
                    .child(
                        button("Defaults", false)
                            .id("openrouter-defaults")
                            .on_click(cx.listener(|this, _, _, cx| this.reset_defaults(cx))),
                    )
                    .child(
                        button("Revert", false)
                            .id("openrouter-revert")
                            .on_click(cx.listener(|this, _, _, cx| this.revert(cx))),
                    )
                    .child(
                        button("Save", true)
                            .id("openrouter-save")
                            .when(self.dirty, |save| save.border_color(rgb(ACCENT)))
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            );
        div()
            .child(settings_section_label("OPENROUTER"))
            .child(
                settings_panel()
                    .child(row("API key", key_description, key_control))
                    .child(row(
                        "Transcription models",
                        "One per line, tried in order. Any error moves on to the next model. Choose OpenRouter as the dictation model above to use them.",
                        sized(&self.transcription_models, WIDE_INPUT),
                    ))
                    .child(row(
                        "Attempt timeout (s)",
                        "Deadline for one request to one model",
                        sized(&self.attempt_timeout, NARROW_INPUT),
                    ))
                    .child(row(
                        "Total timeout (s)",
                        "Deadline for the whole fallback chain",
                        sized(&self.total_timeout, NARROW_INPUT),
                    ))
                    .child(row(
                        "Chunk length (s)",
                        "Long recordings are split at a quiet point (10 to 200)",
                        sized(&self.chunk_seconds, NARROW_INPUT),
                    ))
                    .child(row(
                        "Rate-limit retry (ms)",
                        "A 429 asking to wait at most this long is retried once on the same model; 0 falls back at once",
                        sized(&self.rate_limit_wait, NARROW_INPUT),
                    ))
                    .child(row(
                        "Temperature",
                        "0 to 1; empty leaves it to the provider",
                        sized(&self.temperature, NARROW_INPUT),
                    ))
                    .child(
                        row(
                            "API URL",
                            "OpenRouter or a compatible endpoint",
                            sized(&self.base_url, WIDE_INPUT),
                        )
                        .border_b_0(),
                    ),
            )
            .child(settings_section_label("OPENROUTER CLEANUP"))
            .child(
                settings_panel()
                    .child(row(
                        "Clean up transcripts",
                        "A text model fixes punctuation and drops filler words before Modes. On failure the raw transcript is pasted.",
                        cleanup_toggle,
                    ))
                    .child(row(
                        "Cleanup models",
                        "One per line, tried in order",
                        sized(&self.cleanup_models, WIDE_INPUT),
                    ))
                    .child(row(
                        "Cleanup timeout (s)",
                        "Deadline for the whole cleanup chain",
                        sized(&self.cleanup_timeout, NARROW_INPUT),
                    ))
                    .child(
                        row(
                            "Cleanup prompt",
                            "System prompt for the cleanup model",
                            sized(&self.cleanup_prompt, WIDE_INPUT),
                        )
                        .border_b_0(),
                    ),
            )
            .child(footer)
    }
}
