//! The Models pane: the API key, language, primary and fallback models,
//! and advanced request limits. Silence trimming shares this configuration
//! but is controlled from Settings under Microphone.
//! Every control saves `openrouter.json` immediately; the next dictation
//! reads it.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, KeyDownEvent, MouseDownEvent, Render, ScrollHandle, SharedString, Subscription,
    Window, div, prelude::*, px, rgb,
};

use super::catalog::{self, CatalogModel};
use super::form::{self, AdvancedForm, MAX_FALLBACKS};
use super::{Config, KeyStatus};
use crate::desktop_ui::{
    ACCENT, FAINT, LINE, MUTED, NEGATIVE, PickerState, SURFACE, SURFACE_HOVER, SURFACE_SELECTED,
    TEXT, TEXT_SOFT, compact_button, disclosure_button, picker_open_key, picker_popup,
    settings_panel, settings_row, settings_section_label,
};
use crate::text_input::{Changed, Dismissed, Navigate, Submitted, TextInput};

const MODEL_BUTTON_WIDTH: f32 = 300.0;
const KEY_INPUT_WIDTH: f32 = 300.0;
const NARROW_INPUT: f32 = 120.0;
const WIDE_INPUT: f32 = 300.0;
const PICKER_WIDTH: f32 = 380.0;
const KEYS_URL: &str = "https://openrouter.ai/keys";

pub fn new<V: 'static>(preview: bool, cx: &mut Context<V>) -> Entity<OpenRouterSettings> {
    cx.new(|cx| OpenRouterSettings::new(false, preview, cx))
}

/// Just the API key row, for the first-run setup sheet.
pub fn new_key_setup<V: 'static>(preview: bool, cx: &mut Context<V>) -> Entity<OpenRouterSettings> {
    cx.new(|cx| OpenRouterSettings::new(true, preview, cx))
}

pub struct KeyChanged(pub KeyStatus);

/// Where the last action's outcome is shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scope {
    Configuration,
    Key,
    Language,
    Model(usize),
    Microphone,
    Advanced,
}

#[derive(Clone, Copy, PartialEq)]
enum KeyOperation {
    Refresh,
    Save,
    Test,
    Remove,
    Move,
}

impl KeyOperation {
    fn label(self) -> &'static str {
        match self {
            Self::Refresh => "Checking key",
            Self::Save => "Saving key",
            Self::Test => "Testing key",
            Self::Remove => "Removing key",
            Self::Move => "Moving key",
        }
    }
}

enum CatalogState {
    Idle,
    Loading,
    Loaded(Vec<CatalogModel>),
    Failed(String),
}

impl CatalogState {
    fn models(&self) -> &[CatalogModel] {
        match self {
            Self::Loaded(models) => models,
            Self::Idle | Self::Loading | Self::Failed(_) => &[],
        }
    }
}

struct ModelPicker {
    slot: usize,
    search: Entity<TextInput>,
    highlight: usize,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

/// One row in the open model picker.
#[derive(Clone, Debug, PartialEq)]
enum PickerChoice {
    Catalog(CatalogModel),
    Custom(String),
}

impl PickerChoice {
    fn id(&self) -> &str {
        match self {
            Self::Catalog(model) => &model.id,
            Self::Custom(id) => id,
        }
    }
}

struct AdvancedInputs {
    base_url: Entity<TextInput>,
    attempt_timeout: Entity<TextInput>,
    total_timeout: Entity<TextInput>,
    chunk_seconds: Entity<TextInput>,
    rate_limit_wait: Entity<TextInput>,
    temperature: Entity<TextInput>,
}

pub struct OpenRouterSettings {
    key_only: bool,
    preview: bool,
    config: Config,
    key_status: Option<KeyStatus>,
    key_revision: u64,
    key_editing: bool,
    key_remove_armed: bool,
    key_input: Entity<TextInput>,
    catalog: CatalogState,
    picker: Option<ModelPicker>,
    model_focus: [FocusHandle; MAX_FALLBACKS + 1],
    language_picker_open: bool,
    language_picker_state: PickerState,
    advanced_open: bool,
    advanced: AdvancedInputs,
    advanced_saved: AdvancedForm,
    advanced_dirty: bool,
    key_operation: Option<KeyOperation>,
    message: Option<(Scope, bool, String)>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<KeyChanged> for OpenRouterSettings {}

impl OpenRouterSettings {
    fn new(key_only: bool, preview: bool, cx: &mut Context<Self>) -> Self {
        let loaded = if preview {
            Ok(Config::default())
        } else {
            super::load_config()
        };
        let (config, message) = match loaded {
            Ok(config) => (config, None),
            Err(error) => (
                Config::default(),
                Some((
                    Scope::Configuration,
                    false,
                    format!("{error:#}. Fix the file before saving settings."),
                )),
            ),
        };
        let mut subscriptions = Vec::new();
        let key_input = cx.new(|cx| TextInput::new(cx, "sk-or-v1-…", ""));
        subscriptions.push(cx.subscribe(&key_input, |this, _, _: &Submitted, cx| {
            this.save_key(cx);
        }));
        let form = AdvancedForm::from_config(&config);
        let mut field = |placeholder: &'static str, value: &str, cx: &mut Context<Self>| {
            let value = value.to_owned();
            let entity = cx.new(|cx| TextInput::new(cx, placeholder, &value));
            subscriptions.push(cx.subscribe(&entity, |this, _, _: &Changed, cx| {
                this.advanced_dirty = true;
                this.clear_message(Scope::Advanced);
                cx.notify();
            }));
            subscriptions.push(cx.subscribe(&entity, |this, _, _: &Submitted, cx| {
                this.save_advanced(cx);
            }));
            entity
        };
        let advanced = AdvancedInputs {
            base_url: field("https://openrouter.ai/api/v1", &form.base_url, cx),
            attempt_timeout: field("30", &form.attempt_timeout_seconds, cx),
            total_timeout: field("90", &form.total_timeout_seconds, cx),
            chunk_seconds: field("120", &form.chunk_seconds, cx),
            rate_limit_wait: field("2000", &form.rate_limit_retry_max_wait_ms, cx),
            temperature: field("provider default", &form.temperature, cx),
        };
        let mut view = Self {
            key_only,
            preview,
            config,
            key_status: None,
            key_revision: 0,
            key_editing: false,
            key_remove_armed: false,
            key_input,
            catalog: CatalogState::Idle,
            picker: None,
            model_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            language_picker_open: false,
            language_picker_state: PickerState::new(cx),
            advanced_open: false,
            advanced,
            advanced_saved: form,
            advanced_dirty: false,
            key_operation: None,
            message,
            _subscriptions: subscriptions,
        };
        if preview {
            view.sync_key_status(KeyStatus::Missing, cx);
        } else {
            view.refresh_key_status(cx);
        }
        view
    }

    fn clear_message(&mut self, scope: Scope) {
        if self
            .message
            .as_ref()
            .is_some_and(|(current, ..)| *current == scope)
        {
            self.message = None;
        }
    }

    fn report(&mut self, scope: Scope, result: Result<String, String>) {
        self.message = Some(match result {
            Ok(text) => (scope, true, text),
            Err(text) => (scope, false, text),
        });
    }

    /// Rebase one edit onto the latest file, leaving unrelated fields intact.
    fn commit(
        &mut self,
        scope: Scope,
        edit: impl FnOnce(&Config) -> Result<Config, String>,
        success: &str,
    ) -> bool {
        if self.busy() {
            self.report(
                scope,
                Err("Wait for the key operation to finish, then try again.".into()),
            );
            return false;
        }
        let saved = if self.preview {
            edit(&self.config)
        } else {
            super::update_config(|config| {
                edit(config).map_err(|error| color_eyre::eyre::eyre!("{error}"))
            })
            .map_err(|error| format!("{error:#}"))
        };
        match saved {
            Ok(config) => {
                self.config = config;
                self.report(scope, Ok(success.to_owned()));
                true
            }
            Err(error) => {
                self.report(scope, Err(error));
                false
            }
        }
    }

    fn busy(&self) -> bool {
        self.key_operation.is_some()
    }

    fn action_label(&self, operation: KeyOperation, idle: &'static str) -> &'static str {
        if self.key_operation == Some(operation) {
            operation.label()
        } else {
            idle
        }
    }

    /// Run blocking work (Keychain, network) off the UI thread.
    fn run<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        operation: KeyOperation,
        work: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        if self.busy() || self.preview {
            return;
        }
        self.key_operation = Some(operation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { work() }).await;
            let _ = this.update(cx, |this, cx| {
                this.key_operation = None;
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ---- API key -------------------------------------------------------

    pub(crate) fn has_key_operation(&self) -> bool {
        self.busy()
    }

    pub(crate) fn config_snapshot(&self) -> Config {
        self.config.clone()
    }

    pub(crate) fn apply_imported_config(&mut self, config: Config, cx: &mut Context<Self>) {
        self.config = config;
        self.close_pickers(cx);
        self.advanced_saved = AdvancedForm::from_config(&self.config);
        self.load_advanced(&self.advanced_saved.clone(), cx);
        self.advanced_dirty = false;
        self.catalog = CatalogState::Idle;
        self.message = None;
        cx.notify();
    }

    /// Reconcile another editor's key change without starting another lookup.
    pub fn sync_key_status(&mut self, status: KeyStatus, cx: &mut Context<Self>) {
        self.key_revision += 1;
        self.key_editing = matches!(status, KeyStatus::Missing);
        self.key_remove_armed = false;
        self.key_status = Some(status);
        self.key_input
            .update(cx, |input, cx| input.set_text("", cx));
        cx.notify();
    }

    fn key_changed(&mut self, status: KeyStatus, cx: &mut Context<Self>) {
        self.sync_key_status(status.clone(), cx);
        cx.emit(KeyChanged(status));
    }

    #[cfg(test)]
    pub fn key_status(&self) -> Option<&KeyStatus> {
        self.key_status.as_ref()
    }

    fn refresh_key_status(&mut self, cx: &mut Context<Self>) {
        let config = self.config.clone();
        let revision = self.key_revision;
        self.run(
            cx,
            KeyOperation::Refresh,
            move || super::key_status(&config),
            move |this, status, cx| {
                if this.key_revision == revision {
                    this.sync_key_status(status, cx);
                }
            },
        );
    }

    fn begin_key_replacement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.key_editing = true;
        self.key_remove_armed = false;
        self.clear_message(Scope::Key);
        self.key_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.key_input.focus_handle(cx).focus(window);
        cx.notify();
    }

    fn cancel_key_replacement(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.key_editing = matches!(self.key_status, Some(KeyStatus::Missing) | None);
        self.key_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.clear_message(Scope::Key);
        cx.notify();
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let key = self.key_input.read(cx).text().trim().to_owned();
        if let Err(error) = super::validate_key(&key) {
            self.report(Scope::Key, Err(error.to_string()));
            cx.notify();
            return;
        }
        if self.preview {
            self.key_changed(KeyStatus::Keychain("demo".into()), cx);
            self.clear_message(Scope::Key);
            return;
        }
        self.run(
            cx,
            KeyOperation::Save,
            move || {
                super::store_keychain_key(&key)?;
                let config = super::load_config()?;
                Ok::<_, color_eyre::Report>((super::key_status(&config), super::check_key(&config)))
            },
            |this, result, cx| match result {
                Ok((status, check)) => {
                    this.key_changed(status, cx);
                    match check {
                        Ok(_) => this.clear_message(Scope::Key),
                        Err(error) => this.report(
                            Scope::Key,
                            Err(format!(
                                "Key saved, but OpenRouter did not accept it: {error:#}"
                            )),
                        ),
                    }
                }
                Err(error) => this.report(Scope::Key, Err(format!("{error:#}"))),
            },
        );
    }

    fn test_key(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        if self.preview {
            self.report(
                Scope::Key,
                Ok("Preview: no network request was made.".into()),
            );
            cx.notify();
            return;
        }
        self.run(
            cx,
            KeyOperation::Test,
            move || super::load_config().and_then(|config| super::check_key(&config)),
            |this, result, _| {
                this.report(Scope::Key, result.map_err(|error| format!("{error:#}")));
            },
        );
    }

    fn remove_key(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        if !self.key_remove_armed {
            self.key_remove_armed = true;
            cx.notify();
            return;
        }
        self.key_remove_armed = false;
        if self.preview {
            self.key_changed(KeyStatus::Missing, cx);
            self.clear_message(Scope::Key);
            return;
        }
        self.run(
            cx,
            KeyOperation::Remove,
            move || {
                super::delete_keychain_key()?;
                super::load_config().map(|config| super::key_status(&config))
            },
            |this, result, cx| match result {
                Ok(status) => {
                    this.key_changed(status, cx);
                    this.clear_message(Scope::Key);
                }
                Err(error) => this.report(Scope::Key, Err(format!("{error:#}"))),
            },
        );
    }

    /// Moves a plaintext key from `openrouter.json` into the Keychain.
    fn move_key_to_keychain(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        if self.preview {
            self.config.api_key = None;
            self.key_changed(KeyStatus::Keychain("demo".into()), cx);
            self.clear_message(Scope::Key);
            return;
        }
        self.run(
            cx,
            KeyOperation::Move,
            move || {
                let key = super::load_config()?.api_key.ok_or_else(|| {
                    color_eyre::eyre::eyre!("The file no longer contains an API key.")
                })?;
                super::store_keychain_key(&key)?;
                let config = super::update_config(|latest| {
                    form::remove_migrated_key(latest, &key)
                        .map_err(|error| color_eyre::eyre::eyre!("{error}"))
                })?;
                Ok::<_, color_eyre::Report>((super::key_status(&config), config))
            },
            |this, result, cx| match result {
                Ok((status, config)) => {
                    this.config = config;
                    this.key_changed(status, cx);
                    this.clear_message(Scope::Key);
                }
                Err(error) => this.report(Scope::Key, Err(format!("{error:#}"))),
            },
        );
    }

    // ---- Models ----------------------------------------------------------

    fn ensure_catalog(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.catalog,
            CatalogState::Loading | CatalogState::Loaded(_)
        ) {
            return;
        }
        if self.preview {
            self.catalog = CatalogState::Loaded(
                self.config
                    .transcription
                    .models
                    .iter()
                    .map(|id| CatalogModel {
                        id: id.clone(),
                        name: id.clone(),
                        provider: "Preview".into(),
                    })
                    .collect(),
            );
            return;
        }
        self.catalog = CatalogState::Loading;
        let config = self.config.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { catalog::fetch(&config) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.catalog = match result {
                    Ok(models) => CatalogState::Loaded(models),
                    Err(error) => CatalogState::Failed(format!("{error:#}")),
                };
                this.highlight_current_model(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_picker(&mut self, slot: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.language_picker_open = false;
        self.ensure_catalog(cx);
        let search = cx.new(|cx| TextInput::picker(cx, "Search models or paste an id", ""));
        let subscriptions = vec![
            cx.subscribe(&search, |this, _, _: &Changed, cx| {
                if let Some(picker) = &mut this.picker {
                    picker.highlight = 0;
                    picker.scroll.scroll_to_item(0);
                }
                cx.notify();
            }),
            cx.subscribe(&search, |this, _, Navigate(direction): &Navigate, cx| {
                let count = this.picker_choices(cx).len();
                if let Some(picker) = &mut this.picker
                    && count > 0
                {
                    picker.highlight = if *direction < 0 {
                        picker.highlight.saturating_sub(1)
                    } else {
                        (picker.highlight + 1).min(count - 1)
                    };
                    picker.scroll.scroll_to_item(picker.highlight);
                }
                cx.notify();
            }),
            cx.subscribe_in(&search, window, |this, _, _: &Submitted, window, cx| {
                let choices = this.picker_choices(cx);
                if let Some(picker) = &this.picker
                    && let Some(choice) = choices.get(picker.highlight)
                {
                    let slot = picker.slot;
                    let id = choice.id().to_owned();
                    if this.choose_model(slot, Some(id), cx) {
                        this.model_focus[slot].focus(window);
                    }
                }
            }),
            cx.subscribe_in(&search, window, |this, _, _: &Dismissed, window, cx| {
                this.close_model_picker(window);
                cx.notify();
            }),
        ];
        search.focus_handle(cx).focus(window);
        self.picker = Some(ModelPicker {
            slot,
            search,
            highlight: 0,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        });
        self.highlight_current_model(cx);
        cx.notify();
    }

    fn close_model_picker(&mut self, window: &mut Window) {
        if let Some(picker) = self.picker.take() {
            self.model_focus[picker.slot].focus(window);
        }
    }

    pub fn close_pickers(&mut self, cx: &mut Context<Self>) {
        if self.picker.take().is_some() || self.language_picker_open {
            self.language_picker_open = false;
            cx.notify();
        }
    }

    fn toggle_model_picker(&mut self, slot: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.slot == slot)
        {
            self.close_model_picker(window);
            cx.notify();
        } else {
            self.open_picker(slot, window, cx);
        }
    }

    fn highlight_current_model(&mut self, cx: &App) {
        let Some(picker) = &self.picker else {
            return;
        };
        if !picker.search.read(cx).text().is_empty() {
            return;
        }
        let current = self.config.transcription.models.get(picker.slot);
        let choices = self.picker_choices(cx);
        let index = choices
            .iter()
            .position(|choice| current.is_some_and(|id| id == choice.id()))
            .unwrap_or(0);
        if let Some(picker) = &mut self.picker {
            picker.highlight = index;
            picker.scroll.scroll_to_item(index);
        }
    }

    fn picker_choices(&self, cx: &App) -> Vec<PickerChoice> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        let query = picker.search.read(cx).text();
        let mut choices = picker_choices(self.catalog.models(), query);
        if query.is_empty()
            && let Some(current) = self.config.transcription.models.get(picker.slot)
            && !choices.iter().any(|choice| choice.id() == current)
        {
            choices.insert(0, PickerChoice::Custom(current.clone()));
        }
        choices
    }

    fn choose_model(&mut self, slot: usize, model: Option<String>, cx: &mut Context<Self>) -> bool {
        let success = match (&model, slot) {
            (None, _) => "Fallback removed.".to_owned(),
            (Some(model), 0) => format!("{model} is now the primary model."),
            (Some(model), slot) => format!("{model} is fallback {slot}."),
        };
        let saved = self.commit(
            Scope::Model(slot),
            |config| form::set_model(config, slot, model.as_deref()),
            &success,
        );
        if saved {
            self.picker = None;
        }
        cx.notify();
        saved
    }

    fn promote_model(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.commit(
            Scope::Model(slot),
            |config| Ok(form::promote_model(config, slot)),
            "Order updated.",
        );
        cx.notify();
    }

    fn choose_language(&mut self, language: &str, cx: &mut Context<Self>) -> bool {
        let success = format!("Language: {}.", super::language_name(language));
        let saved = self.commit(
            Scope::Language,
            |config| form::set_language(config, language),
            &success,
        );
        if saved {
            self.language_picker_open = false;
        }
        cx.notify();
        saved
    }

    fn toggle_language_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.picker = None;
        self.language_picker_open = !self.language_picker_open;
        if self.language_picker_open {
            let index = super::LANGUAGES
                .iter()
                .position(|(code, _)| *code == self.config.transcription.language)
                .unwrap_or(0);
            self.language_picker_state
                .open(index, super::LANGUAGES.len(), window);
        } else {
            self.language_picker_state.trigger.focus(window);
        }
        cx.notify();
    }

    fn language_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if self
            .language_picker_state
            .navigate(key, super::LANGUAGES.len())
        {
        } else if matches!(key, "enter" | "space") {
            if let Some((code, _)) = super::LANGUAGES.get(self.language_picker_state.highlight)
                && self.choose_language(code, cx)
            {
                self.language_picker_state.trigger.focus(window);
            }
        } else if matches!(key, "escape" | "tab") {
            self.language_picker_open = false;
            self.language_picker_state.close(event, window);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub fn trim_silence(&self) -> bool {
        self.config.transcription.trim_silence
    }

    pub fn toggle_trim(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let enabled = !self.config.transcription.trim_silence;
        let success = if enabled {
            "Silence is trimmed before sending."
        } else {
            "Recordings are sent untrimmed."
        };
        let saved = self.commit(
            Scope::Microphone,
            |config| {
                let mut config = config.clone();
                config.transcription.trim_silence = enabled;
                Ok(config)
            },
            success,
        );
        cx.notify();
        if saved {
            Ok(())
        } else {
            Err(self
                .message
                .as_ref()
                .map(|(_, _, message)| message.clone())
                .unwrap_or_else(|| "Could not save silence trimming.".into()))
        }
    }

    // ---- Advanced --------------------------------------------------------

    fn advanced_form(&self, cx: &App) -> AdvancedForm {
        let text = |input: &Entity<TextInput>| input.read(cx).text().to_owned();
        AdvancedForm {
            base_url: text(&self.advanced.base_url),
            attempt_timeout_seconds: text(&self.advanced.attempt_timeout),
            total_timeout_seconds: text(&self.advanced.total_timeout),
            chunk_seconds: text(&self.advanced.chunk_seconds),
            rate_limit_retry_max_wait_ms: text(&self.advanced.rate_limit_wait),
            temperature: text(&self.advanced.temperature),
        }
    }

    fn load_advanced(&mut self, form: &AdvancedForm, cx: &mut Context<Self>) {
        let set = |input: &Entity<TextInput>, value: &str, cx: &mut Context<Self>| {
            input.update(cx, |input, cx| input.set_text(value, cx));
        };
        set(&self.advanced.base_url, &form.base_url, cx);
        set(
            &self.advanced.attempt_timeout,
            &form.attempt_timeout_seconds,
            cx,
        );
        set(
            &self.advanced.total_timeout,
            &form.total_timeout_seconds,
            cx,
        );
        set(&self.advanced.chunk_seconds, &form.chunk_seconds, cx);
        set(
            &self.advanced.rate_limit_wait,
            &form.rate_limit_retry_max_wait_ms,
            cx,
        );
        set(&self.advanced.temperature, &form.temperature, cx);
    }

    fn save_advanced(&mut self, cx: &mut Context<Self>) {
        if !self.advanced_dirty {
            return;
        }
        let form = self.advanced_form(cx);
        let original = self.advanced_saved.clone();
        if self.commit(
            Scope::Advanced,
            |config| form.apply_changes(&original, config),
            "Saved.",
        ) {
            self.advanced_saved = AdvancedForm::from_config(&self.config);
            self.load_advanced(&self.advanced_saved.clone(), cx);
            self.advanced_dirty = false;
            // The API URL may have changed; fetch the catalog again on demand.
            if matches!(
                self.catalog,
                CatalogState::Failed(_) | CatalogState::Loaded(_)
            ) {
                self.catalog = CatalogState::Idle;
            }
        }
        cx.notify();
    }

    fn restore_advanced_defaults(&mut self, cx: &mut Context<Self>) {
        let defaults = AdvancedForm::from_config(&Config::default());
        if self.commit(
            Scope::Advanced,
            |config| defaults.apply(config),
            "Advanced defaults restored.",
        ) {
            self.load_advanced(&defaults, cx);
            self.advanced_saved = defaults;
            self.advanced_dirty = false;
            self.catalog = CatalogState::Idle;
        }
        cx.notify();
    }

    fn reveal_config(&mut self, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        let result = super::config_path().and_then(|path| {
            std::process::Command::new("/usr/bin/open")
                .arg("-R")
                .arg(path)
                .spawn()
                .map(drop)
                .map_err(Into::into)
        });
        if let Err(error) = result {
            self.report(Scope::Advanced, Err(format!("{error:#}")));
            cx.notify();
        }
    }

    // ---- Rendering -------------------------------------------------------

    fn render_message(&self, scope: Scope) -> Option<AnyElement> {
        if self.busy() && scope == Scope::Key {
            return None;
        }
        let (ok, text) = {
            let (current, ok, text) = self.message.as_ref()?;
            if *current != scope {
                return None;
            }
            (*ok, text.clone())
        };
        if ok && scope != Scope::Key {
            return None;
        }
        Some(
            div()
                .w_full()
                .min_w_0()
                .whitespace_normal()
                .px_1()
                .pt_2()
                .text_size(px(11.0))
                .line_height(px(16.0))
                .text_color(rgb(if ok { TEXT_SOFT } else { NEGATIVE }))
                .child(text)
                .into_any_element(),
        )
    }

    fn render_error(&self, scope: Scope) -> Option<AnyElement> {
        self.message
            .as_ref()
            .filter(|(current, ok, _)| *current == scope && !ok)?;
        self.render_message(scope)
    }

    fn render_key_control(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let editing = self.key_editing || matches!(self.key_status, Some(KeyStatus::Missing));
        if editing {
            let has_key = matches!(
                self.key_status,
                Some(KeyStatus::Keychain(_) | KeyStatus::ConfigFile | KeyStatus::Environment)
            );
            return div()
                .w(px(KEY_INPUT_WIDTH))
                .flex_none()
                .flex()
                .flex_col()
                .gap_2()
                .when(!self.busy(), |column| column.child(self.key_input.clone()))
                .when(self.busy(), |column| {
                    column.child(
                        div()
                            .h(px(crate::desktop_ui::TEXT_INPUT_HEIGHT))
                            .px_3()
                            .flex()
                            .items_center()
                            .text_size(px(12.0))
                            .text_color(rgb(MUTED))
                            .child("••••••••••••"),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .id("openrouter-get-key")
                                .text_size(px(11.0))
                                .text_color(rgb(MUTED))
                                .hover(|link| link.text_color(rgb(TEXT_SOFT)))
                                .cursor_pointer()
                                .child("Get a key ↗")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if !this.preview {
                                        cx.open_url(KEYS_URL);
                                    }
                                })),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .when(has_key, |buttons| {
                                    buttons.child(
                                        button("Cancel", false)
                                            .id("openrouter-cancel-key")
                                            .when(self.busy(), |button| button.opacity(0.45))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.cancel_key_replacement(cx)
                                            })),
                                    )
                                })
                                .child(
                                    button(self.action_label(KeyOperation::Save, "Save key"), true)
                                        .id("openrouter-save-key")
                                        .when(self.busy(), |button| button.opacity(0.45))
                                        .on_click(cx.listener(|this, _, _, cx| this.save_key(cx))),
                                ),
                        ),
                )
                .into_any_element();
        }
        let (badge, actions): (String, Vec<AnyElement>) =
            match &self.key_status {
                None => ("Checking…".into(), Vec::new()),
                Some(KeyStatus::Keychain(suffix)) => (
                    format!("Key saved · …{suffix}"),
                    vec![
                        button(self.action_label(KeyOperation::Test, "Test"), false)
                            .id("openrouter-test-key")
                            .when(self.busy(), |button| button.opacity(0.45))
                            .on_click(cx.listener(|this, _, _, cx| this.test_key(cx)))
                            .into_any_element(),
                        button("Replace", false)
                            .id("openrouter-replace-key")
                            .when(self.busy(), |button| button.opacity(0.45))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.begin_key_replacement(window, cx)
                            }))
                            .into_any_element(),
                        button(
                            if self.key_operation == Some(KeyOperation::Remove) {
                                KeyOperation::Remove.label()
                            } else if self.key_remove_armed {
                                "Really remove?"
                            } else {
                                "Remove"
                            },
                            false,
                        )
                        .id("openrouter-remove-key")
                        .when(self.busy(), |button| button.opacity(0.45))
                        .when(self.key_remove_armed, |button| {
                            button.text_color(rgb(NEGATIVE))
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.remove_key(cx)))
                        .into_any_element(),
                    ],
                ),
                Some(KeyStatus::ConfigFile) => (
                    "Key in openrouter.json".into(),
                    vec![
                        button(self.action_label(KeyOperation::Test, "Test"), false)
                            .id("openrouter-test-key")
                            .when(self.busy(), |button| button.opacity(0.45))
                            .on_click(cx.listener(|this, _, _, cx| this.test_key(cx)))
                            .into_any_element(),
                        button(
                            self.action_label(KeyOperation::Move, "Move to Keychain"),
                            true,
                        )
                        .id("openrouter-move-key")
                        .when(self.busy(), |button| button.opacity(0.45))
                        .on_click(cx.listener(|this, _, _, cx| this.move_key_to_keychain(cx)))
                        .into_any_element(),
                    ],
                ),
                Some(KeyStatus::Environment) => (
                    "Key from OPENROUTER_API_KEY".into(),
                    vec![
                        button(self.action_label(KeyOperation::Test, "Test"), false)
                            .id("openrouter-test-key")
                            .when(self.busy(), |button| button.opacity(0.45))
                            .on_click(cx.listener(|this, _, _, cx| this.test_key(cx)))
                            .into_any_element(),
                    ],
                ),
                Some(KeyStatus::Missing) => unreachable!("handled by the editing branch"),
            };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .h(px(28.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .bg(rgb(0x17231a))
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0x91bd99))
                    .child(div().size(px(6.0)).rounded_full().bg(rgb(0x69d89f)))
                    .child(badge),
            )
            .children(actions)
            .into_any_element()
    }

    fn key_description(&self) -> &'static str {
        match &self.key_status {
            None => "Looking for your OpenRouter key…",
            Some(KeyStatus::Keychain(_)) => "Stored in the macOS Keychain, never in a file",
            Some(KeyStatus::ConfigFile) => {
                "Read from openrouter.json in plain text; moving it to the Keychain is safer"
            }
            Some(KeyStatus::Environment) => "Set by the environment; it overrides any saved key",
            Some(KeyStatus::Missing) => "Required to transcribe. Paste a key and press Return",
        }
    }

    fn render_key_row(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let description = self.key_description();
        let control = self.render_key_control(cx);
        self.row_message(
            settings_row("OpenRouter API key", description, control)
                .when(self.key_only, |row| row.px_0()),
            Scope::Key,
            true,
        )
    }

    fn row_message(&self, row: gpui::Div, scope: Scope, show: bool) -> gpui::Div {
        div()
            .border_b_1()
            .border_color(rgb(LINE))
            .child(row.border_b_0())
            .when(show, |panel| {
                panel.children(
                    self.render_message(scope)
                        .map(|message| div().px_4().pb_3().child(message)),
                )
            })
    }

    fn render_model_button(
        &self,
        slot: usize,
        model: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let catalog = self.catalog.models();
        let (label, detail) = match model {
            Some(id) => (catalog::label(id, catalog), Some(id.to_owned())),
            None => ("Choose a model".to_owned(), None),
        };
        let open = self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.slot == slot);
        let menu = open.then(|| self.render_picker(slot, model, cx));
        div()
            .relative()
            .flex_none()
            .child(
                disclosure_button(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(div().flex_none().text_color(rgb(TEXT)).child(label.clone()))
                        .when_some(detail.filter(|id| *id != label), |row, id| {
                            row.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(10.0))
                                    .text_color(rgb(FAINT))
                                    .child(id),
                            )
                        }),
                )
                .w(px(MODEL_BUTTON_WIDTH))
                .id(("openrouter-model", slot))
                .track_focus(&self.model_focus[slot].clone().tab_stop(!self.busy()))
                .focus(|style| style.border_color(rgb(ACCENT)))
                .when(self.busy(), |button| button.opacity(0.5))
                .on_click(cx.listener(move |this, event, window, cx| {
                    if matches!(event, gpui::ClickEvent::Mouse(_)) {
                        this.toggle_model_picker(slot, window, cx);
                    }
                }))
                .on_key_down(cx.listener(move |this, event, window, cx| {
                    if picker_open_key(event) {
                        this.toggle_model_picker(slot, window, cx);
                        cx.stop_propagation();
                    }
                })),
            )
            .children(menu.map(picker_popup))
            .into_any_element()
    }

    fn render_picker(
        &self,
        slot: usize,
        current: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(picker) = &self.picker else {
            return div().into_any_element();
        };
        let choices = self.picker_choices(cx);
        let highlight = picker.highlight.min(choices.len().saturating_sub(1));
        let status: Option<AnyElement> = match &self.catalog {
            CatalogState::Idle | CatalogState::Loading => {
                Some(picker_note("Loading OpenRouter's speech-to-text models…").into_any_element())
            }
            CatalogState::Failed(error) => Some(
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(11.0))
                            .text_color(rgb(NEGATIVE))
                            .child(format!("Could not load models: {error}")),
                    )
                    .child(
                        button("Retry", false)
                            .id("openrouter-catalog-retry")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.catalog = CatalogState::Idle;
                                this.ensure_catalog(cx);
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            ),
            CatalogState::Loaded(_) if choices.is_empty() => Some(
                picker_note("No models match. Paste a full id such as provider/model.")
                    .into_any_element(),
            ),
            CatalogState::Loaded(_) => None,
        };
        let rows = choices.into_iter().enumerate().map(|(index, choice)| {
            let selected = current == Some(choice.id());
            let highlighted = index == highlight;
            let (title, subtitle) = match &choice {
                PickerChoice::Catalog(model) => (
                    model.name.clone(),
                    if model.provider.is_empty() {
                        model.id.clone()
                    } else {
                        format!("{} · {}", model.provider, model.id)
                    },
                ),
                PickerChoice::Custom(id) => (
                    format!("Use “{id}”"),
                    "Custom model id, not in the catalog".to_owned(),
                ),
            };
            let id = choice.id().to_owned();
            div()
                .id(("openrouter-model-choice", index))
                .w_full()
                .px_3()
                .py(px(7.0))
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .rounded_sm()
                .when(highlighted, |row| row.bg(rgb(SURFACE_HOVER)))
                .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(if selected { TEXT } else { TEXT_SOFT }))
                                .truncate()
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(FAINT))
                                .truncate()
                                .child(subtitle),
                        ),
                )
                .when(selected, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(rgb(ACCENT))
                            .child("✓"),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    if this.choose_model(slot, Some(id.clone()), cx) {
                        this.model_focus[slot].focus(window);
                    }
                }))
        });
        div()
            .id("openrouter-model-picker")
            .max_h(px(420.0))
            .debug_selector(|| "openrouter-model-picker".into())
            .w(px(PICKER_WIDTH))
            .whitespace_normal()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(SURFACE))
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.picker = None;
                cx.notify();
            }))
            .child(div().flex_none().child(picker.search.clone()))
            .children(
                self.render_error(Scope::Model(slot))
                    .map(|message| div().w_full().min_w_0().flex_none().child(message)),
            )
            .children(status)
            .child(
                div()
                    .id("openrouter-model-choices")
                    .track_scroll(&picker.scroll)
                    .min_h_0()
                    .max_h(px(280.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_language_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.config.transcription.language.clone();
        let menu = self.language_picker_open.then(|| {
            let choose = Rc::new(cx.listener(|this, code: &&'static str, window, cx| {
                if this.choose_language(code, cx) {
                    this.language_picker_state.trigger.focus(window);
                }
            }));
            div()
                .id("openrouter-language-picker")
                .track_focus(&self.language_picker_state.menu)
                .on_key_down(cx.listener(Self::language_picker_key))
                .debug_selector(|| "openrouter-language-picker".into())
                .w(px(220.0))
                .max_h(px(300.0))
                .p_2()
                .flex()
                .flex_col()
                .rounded_md()
                .border_1()
                .border_color(rgb(LINE))
                .bg(rgb(SURFACE))
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.language_picker_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("language-choices")
                        .min_h_0()
                        .max_h(px(260.0))
                        .track_scroll(&self.language_picker_state.scroll)
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .children(super::LANGUAGES.iter().enumerate().map(
                            |(index, (code, name))| {
                                let selected = *code == current;
                                let choose = choose.clone();
                                let code: &'static str = code;
                                div()
                                    .id(("openrouter-language", index))
                                    .w_full()
                                    .h(px(30.0))
                                    .flex_none()
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .rounded_sm()
                                    .text_size(px(12.0))
                                    .text_color(rgb(if selected { TEXT } else { TEXT_SOFT }))
                                    .when(index == self.language_picker_state.highlight, |row| {
                                        row.bg(rgb(SURFACE_SELECTED))
                                    })
                                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                                    .child(*name)
                                    .when(code != super::AUTO_LANGUAGE, |row| {
                                        row.child(
                                            div()
                                                .text_size(px(10.0))
                                                .text_color(rgb(FAINT))
                                                .child(code),
                                        )
                                    })
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        choose(&code, window, cx);
                                    })
                            },
                        )),
                )
                .children(self.render_error(Scope::Language).map(|message| {
                    div()
                        .id("language-feedback")
                        .debug_selector(|| "language-feedback".into())
                        .flex_none()
                        .child(message)
                }))
        });
        div()
            .relative()
            .flex_none()
            .child(
                disclosure_button(super::language_name(&current).to_owned())
                    .id("openrouter-language")
                    .track_focus(
                        &self
                            .language_picker_state
                            .trigger
                            .clone()
                            .tab_stop(!self.busy()),
                    )
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .when(self.busy(), |button| button.opacity(0.5))
                    .on_click(cx.listener(|this, event, window, cx| {
                        if matches!(event, gpui::ClickEvent::Mouse(_)) {
                            this.toggle_language_picker(window, cx);
                        }
                    }))
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        if picker_open_key(event) {
                            this.toggle_language_picker(window, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
            .children(menu.map(picker_popup))
            .into_any_element()
    }

    fn render_models_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let models = self.config.transcription.models.clone();
        let mut panel = settings_panel();
        let primary = self.render_model_button(0, models.first().map(String::as_str), cx);
        panel = panel.child(self.row_message(
            settings_row("Primary model", "Transcribes every dictation", primary),
            Scope::Model(0),
            self.picker.as_ref().is_none_or(|picker| picker.slot != 0),
        ));
        let fallbacks = models.len().saturating_sub(1).min(MAX_FALLBACKS);
        for (slot, model) in models.iter().enumerate().skip(1).take(fallbacks) {
            let button = self.render_model_button(slot, Some(model.as_str()), cx);
            let control = div()
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    icon_button("↑", "Move up")
                        .id(("openrouter-promote", slot))
                        .when(self.busy(), |button| button.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| this.promote_model(slot, cx))),
                )
                .child(
                    icon_button("✕", "Remove")
                        .id(("openrouter-remove-model", slot))
                        .when(self.busy(), |button| button.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.choose_model(slot, None, cx);
                        })),
                )
                .child(button);
            panel = panel.child(
                self.row_message(
                    settings_row(
                        if slot == 1 {
                            "Fallback 1"
                        } else {
                            "Fallback 2"
                        },
                        if slot == 1 {
                            "Used when the primary model fails"
                        } else {
                            "Used when fallback 1 also fails"
                        },
                        control,
                    ),
                    Scope::Model(slot),
                    self.picker
                        .as_ref()
                        .is_none_or(|picker| picker.slot != slot),
                ),
            );
        }
        if fallbacks < MAX_FALLBACKS && !models.is_empty() {
            let slot = models.len().min(MAX_FALLBACKS);
            let open = self
                .picker
                .as_ref()
                .is_some_and(|picker| picker.slot == slot);
            let menu = open.then(|| self.render_picker(slot, None, cx));
            panel = panel.child(self.row_message(
                div()
                    .w_full()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .child(format!(
                                "{} of {MAX_FALLBACKS} fallbacks. Any error — rate limit, timeout, server error — moves on to the next model.",
                                fallbacks
                            )),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .child(
                                button("+ Add fallback", false)
                                    .id("openrouter-add-fallback")
                                    .track_focus(&self.model_focus[slot].clone().tab_stop(!self.busy()))
                                    .focus(|style| style.border_color(rgb(ACCENT)))
                                    .when(self.busy(), |button| button.opacity(0.5))
                                    .on_click(cx.listener(move |this, event, window, cx| {
                                        if matches!(event, gpui::ClickEvent::Mouse(_)) {
                                            this.toggle_model_picker(slot, window, cx);
                                        }
                                    }))
                                    .on_key_down(cx.listener(move |this, event, window, cx| {
                                        if picker_open_key(event) {
                                            this.toggle_model_picker(slot, window, cx);
                                            cx.stop_propagation();
                                        }
                                    })),
                            )
                            .children(menu.map(picker_popup)),
                    ),
                Scope::Model(slot),
                !open,
            ));
        }
        let extra = models.len().saturating_sub(MAX_FALLBACKS + 1);
        if extra > 0 {
            panel = panel.child(
                div()
                    .px_4()
                    .py_3()
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child(format!(
                        "{extra} more fallback model{} from openrouter.json are tried after these.",
                        if extra == 1 { "" } else { "s" }
                    )),
            );
        }
        div()
            .child(settings_section_label("MODELS"))
            .child(panel)
            .into_any_element()
    }

    fn render_advanced(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let header = div()
            .id("openrouter-advanced")
            .pt_5()
            .pb_2()
            .px_1()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .text_size(px(11.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(FAINT))
            .hover(|header| header.text_color(rgb(MUTED)))
            .child(if self.advanced_open { "▾" } else { "▸" })
            .child("ADVANCED")
            .on_click(cx.listener(|this, _, _, cx| {
                this.advanced_open = !this.advanced_open;
                cx.notify();
            }));
        if !self.advanced_open {
            return header.into_any_element();
        }
        let narrow = |input: &Entity<TextInput>| sized(input, NARROW_INPUT);
        let footer = div()
            .w_full()
            .px_4()
            .py_3()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(FAINT))
                    .child(if self.advanced_dirty {
                        "Unsaved changes. Press Return or Save."
                    } else {
                        ""
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
                            .when(self.busy(), |button| button.opacity(0.45))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.restore_advanced_defaults(cx)),
                            ),
                    )
                    .child(
                        button("Save", self.advanced_dirty)
                            .id("openrouter-save-advanced")
                            .when(self.busy() || !self.advanced_dirty, |button| {
                                button.opacity(0.45)
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.save_advanced(cx))),
                    ),
            );
        div()
            .child(header)
            .child(
                settings_panel()
                    .child(settings_row(
                        "Attempt timeout (s)",
                        "Deadline for one request to one model",
                        narrow(&self.advanced.attempt_timeout),
                    ))
                    .child(settings_row(
                        "Total timeout (s)",
                        "Deadline for the whole fallback chain",
                        narrow(&self.advanced.total_timeout),
                    ))
                    .child(settings_row(
                        "Chunk length (s)",
                        "Long recordings are split at a quiet point, from 10 to 200",
                        narrow(&self.advanced.chunk_seconds),
                    ))
                    .child(settings_row(
                        "Rate-limit retry (ms)",
                        "A 429 asking to wait at most this long is retried once on the same model; 0 falls back at once",
                        narrow(&self.advanced.rate_limit_wait),
                    ))
                    .child(settings_row(
                        "Temperature",
                        "0 to 1; empty leaves it to the provider",
                        narrow(&self.advanced.temperature),
                    ))
                    .child(settings_row(
                        "API URL",
                        "OpenRouter or a compatible endpoint",
                        sized(&self.advanced.base_url, WIDE_INPUT),
                    ))
                    .child(footer),
            )
            .children(self.render_message(Scope::Advanced))
            .into_any_element()
    }
}

/// Catalog models matching `query`, then a custom-id choice when the query
/// looks like an id the catalog does not list.
fn picker_choices(catalog: &[CatalogModel], query: &str) -> Vec<PickerChoice> {
    let query = query.trim();
    let mut choices: Vec<PickerChoice> = catalog
        .iter()
        .filter(|model| model.matches(query))
        .cloned()
        .map(PickerChoice::Catalog)
        .collect();
    let looks_like_id = query.contains('/') && !query.chars().any(char::is_whitespace);
    if looks_like_id && !catalog.iter().any(|model| model.id == query) {
        choices.push(PickerChoice::Custom(query.to_owned()));
    }
    choices
}

fn sized(input: &Entity<TextInput>, width: f32) -> AnyElement {
    div()
        .w(px(width))
        .flex_none()
        .child(input.clone())
        .into_any_element()
}

fn button(label: impl Into<SharedString>, primary: bool) -> gpui::Div {
    let button = compact_button(label.into())
        .flex_none()
        .border_1()
        .border_color(rgb(LINE));
    if primary {
        button.bg(rgb(SURFACE_SELECTED)).text_color(rgb(TEXT))
    } else {
        button
    }
}

fn icon_button(glyph: &'static str, _label: &'static str) -> gpui::Div {
    div()
        .size(px(26.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .text_size(px(11.0))
        .text_color(rgb(MUTED))
        .hover(|button| button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT)))
        .child(glyph)
}

fn picker_note(text: &'static str) -> gpui::Div {
    div()
        .px_3()
        .py_2()
        .text_size(px(11.0))
        .text_color(rgb(MUTED))
        .child(text)
}

impl Render for OpenRouterSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.key_only {
            let key_row = self.render_key_row(cx).px_0().border_b_0();
            return div()
                .child(div().border_t_1().border_color(rgb(LINE)).child(key_row))
                .into_any_element();
        }
        let key_row = self.render_key_row(cx);
        let language = self.render_language_control(cx);
        let models = self.render_models_panel(cx);
        let advanced = self.render_advanced(cx);
        div()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "tab" {
                    if this.picker.is_some() { this.close_model_picker(window); }
                    if event.keystroke.modifiers.shift { window.focus_prev(); }
                    else { window.focus_next(); }
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(settings_section_label("OPENROUTER"))
            .children(self.render_message(Scope::Configuration))
            .child(
                settings_panel()
                    .child(key_row)
                    .child(
                        self.row_message(settings_row(
                            "Language",
                            "Spoken language hint; Auto-detect lets the model decide",
                            language,
                        ), Scope::Language, !self.language_picker_open)
                        .border_b_0(),
                    ),
            )
            .child(models)
            .child(advanced)
            .child(
                div()
                    .px_1()
                    .pt_3()
                    .text_size(px(11.0))
                    .text_color(rgb(FAINT))
                    .child("Audio goes to OpenRouter and the model's provider. Failed recordings stay on this Mac for Retry in History."),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn importing_models_replaces_advanced_drafts_without_touching_key_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.advanced
                    .attempt_timeout
                    .update(cx, |input, cx| input.set_text("invalid", cx));
                view.advanced_dirty = true;
                view.key_input
                    .update(cx, |input, cx| input.set_text("fixture-unsaved-key", cx));
                let status = view.key_status.clone();
                let mut imported = view.config.clone();
                imported.transcription.models = vec!["fixture/imported".into()];
                imported.transcription.language = "pt".into();
                imported.transcription.attempt_timeout_seconds = 45;
                view.apply_imported_config(imported.clone(), cx);
                assert!(!view.advanced_dirty);
                assert_eq!(view.advanced.attempt_timeout.read(cx).text(), "45");
                assert_eq!(view.key_input.read(cx).text(), "fixture-unsaved-key");
                assert_eq!(view.key_status, status);
                view.advanced
                    .temperature
                    .update(cx, |input, cx| input.set_text("0.5", cx));
                view.advanced_dirty = true;
                view.save_advanced(cx);
                imported.transcription.temperature = Some(0.5);
                assert_eq!(view.config, imported);
                assert!(view.render_message(Scope::Advanced).is_none());
            });
        });
    }

    #[gpui::test]
    fn keyboard_reaches_selectors_and_returns_focus_after_selection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.key_input.focus_handle(cx).focus(window))
        });
        cx.simulate_keystrokes("tab");
        cx.update(|window, cx| {
            assert!(
                view.read(cx)
                    .language_picker_state
                    .trigger
                    .is_focused(window)
            )
        });
        cx.simulate_keystrokes("enter down enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(
                view.config.transcription.language,
                super::super::LANGUAGES[1].0
            );
            assert!(!view.language_picker_open);
            assert!(view.language_picker_state.trigger.is_focused(window));
            assert!(matches!(view.message, Some((Scope::Language, true, _))));
            assert!(view.render_message(Scope::Language).is_none());
        });
        cx.simulate_keystrokes("tab enter escape");
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(view.picker.is_none());
            assert!(view.model_focus[0].is_focused(window));
        });
    }

    #[gpui::test]
    fn a_rejected_model_keeps_the_picker_and_keyboard_focus(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.config.transcription.models = vec!["test/one".into(), "test/two".into()];
                view.catalog =
                    CatalogState::Loaded(vec![model("test/one", "One"), model("test/two", "Two")]);
                view.open_picker(1, window, cx);
            })
        });
        cx.simulate_input("test/one");
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| {
            let view = view.read(cx);
            let picker = view
                .picker
                .as_ref()
                .expect("invalid duplicate must leave the menu open");
            assert!(picker.search.focus_handle(cx).is_focused(window));
            assert_eq!(view.config.transcription.models[1], "test/two");
            assert!(matches!(view.message, Some((Scope::Model(1), false, _))));
        });
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| assert!(view.read(cx).model_focus[1].is_focused(window)));
    }

    #[gpui::test]
    fn selected_models_beyond_the_first_page_are_scrolled_into_view(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.config.transcription.models = vec!["test/29".into()];
                view.catalog = CatalogState::Loaded(
                    (0..30)
                        .map(|index| model(&format!("test/{index}"), &format!("Model {index:02}")))
                        .collect(),
                );
                view.open_picker(0, window, cx);
            })
        });
        cx.simulate_keystrokes("up");
        cx.update(|_, cx| {
            let view = view.read(cx);
            let picker = view.picker.as_ref().unwrap();
            assert_eq!(picker.highlight, 28);
            assert!(picker.scroll.offset().y < px(0.0));
        });
    }

    #[gpui::test]
    fn pending_key_operations_cannot_replace_or_clear_the_editor(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.key_input
                    .update(cx, |input, cx| input.set_text("sk-or-v1-fixture", cx));
                view.key_editing = true;
                view.key_operation = Some(KeyOperation::Save);
                view.begin_key_replacement(window, cx);
                view.cancel_key_replacement(cx);
                view.remove_key(cx);
                view.test_key(cx);
                assert_eq!(view.key_input.read(cx).text(), "sk-or-v1-fixture");
                assert!(view.key_editing);
                assert!(!view.key_remove_armed);
                assert_eq!(
                    view.action_label(KeyOperation::Save, "Save key"),
                    "Saving key"
                );
                assert!(view.message.is_none());
            })
        });
    }

    #[gpui::test]
    fn preview_edits_and_key_actions_stay_in_memory(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.config, Config::default());
                assert_eq!(view.key_status(), Some(&KeyStatus::Missing));
                view.choose_language("pt", cx);
                view.choose_model(0, Some("preview/model".into()), cx);
                assert_eq!(view.config.transcription.language, "pt");
                assert_eq!(view.config.transcription.models[0], "preview/model");
                view.ensure_catalog(cx);
                assert!(!view.catalog.models().is_empty());
                view.key_input
                    .update(cx, |input, cx| input.set_text("preview-key-0123456789", cx));
                view.save_key(cx);
                assert_eq!(view.key_status(), Some(&KeyStatus::Keychain("demo".into())));
                assert!(!view.key_editing);
                assert!(view.key_input.read(cx).text().is_empty());
                view.test_key(cx);
                assert!(
                    view.render_message(Scope::Key).is_some(),
                    "explicit key tests still report their result"
                );
                view.remove_key(cx);
                view.remove_key(cx);
                assert_eq!(view.key_status(), Some(&KeyStatus::Missing));
                view.move_key_to_keychain(cx);
                view.reveal_config(cx);
                view.run(
                    cx,
                    KeyOperation::Refresh,
                    || panic!("preview ran external work"),
                    |_, (), _| {},
                );
                assert!(!view.busy());
            });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn preferences_wait_for_key_migration(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let original = view.config.clone();
                view.key_operation = Some(KeyOperation::Refresh);
                view.choose_language("pt", cx);
                view.choose_model(0, Some("preview/model".into()), cx);
                assert!(view.toggle_trim(cx).is_err());
                view.restore_advanced_defaults(cx);
                assert_eq!(view.config, original);
                view.key_operation = None;
                view.choose_language("pt", cx);
                assert_eq!(view.config.transcription.language, "pt");
            });
        });
    }

    fn model(id: &str, name: &str) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            name: name.into(),
            provider: id.split('/').next().unwrap().into(),
        }
    }

    #[test]
    fn picker_offers_a_custom_id_only_when_the_query_looks_like_an_unknown_id() {
        let catalog = [
            model("openai/whisper-1", "Whisper 1"),
            model("deepgram/nova-3", "Nova-3"),
        ];
        assert_eq!(picker_choices(&catalog, "").len(), 2);
        assert_eq!(
            picker_choices(&catalog, "nova"),
            [PickerChoice::Catalog(catalog[1].clone())]
        );
        assert_eq!(
            picker_choices(&catalog, "acme/new-model"),
            [PickerChoice::Custom("acme/new-model".into())]
        );
        assert_eq!(
            picker_choices(&catalog, "openai/whisper-1"),
            [PickerChoice::Catalog(catalog[0].clone())]
        );
        assert!(picker_choices(&catalog, "no such thing").is_empty());
        assert!(picker_choices(&[], "acme/x y").is_empty());
    }
}
