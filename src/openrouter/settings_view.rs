//! The Models pane: the API key, language, primary and fallback models,
//! and advanced request limits. Silence trimming shares this configuration
//! but is controlled from Settings under Microphone.
//! Every control saves `openrouter.json` immediately; the next dictation
//! reads it.

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, KeyDownEvent, MouseDownEvent, Render, ScrollHandle, SharedString, Subscription,
    Window, div, prelude::*, px, rgb,
};

use super::catalog::{self, CatalogModel};
use super::form::{self, AdvancedForm, MAX_FALLBACKS};
use super::{Config, KeyStatus};
use crate::desktop_ui::{
    ACCENT, FAINT, LINE, MUTED, NEGATIVE, SURFACE, SURFACE_HOVER, SURFACE_SELECTED, TEXT,
    TEXT_SOFT, compact_button, disclosure_button, picker_open_key, picker_popup, settings_panel,
    settings_row, settings_section_label,
};
use crate::providers::{Provider, keys};
use crate::text_input::{Changed, Dismissed, EditFinished, Navigate, Submitted, TextInput};

const MODEL_BUTTON_WIDTH: f32 = crate::desktop_ui::SETTINGS_CONTROL_WIDTH;
const KEY_INPUT_WIDTH: f32 = crate::desktop_ui::SETTINGS_CONTROL_WIDTH;
const NARROW_INPUT: f32 = crate::desktop_ui::NUMBER_INPUT_WIDTH;
const WIDE_INPUT: f32 = crate::desktop_ui::SETTINGS_CONTROL_WIDTH;
const PICKER_WIDTH: f32 = crate::desktop_ui::SETTINGS_CONTROL_WIDTH;

pub fn new<V: 'static>(preview: bool, cx: &mut Context<V>) -> Entity<OpenRouterSettings> {
    cx.new(|cx| OpenRouterSettings::new(false, preview, cx))
}

/// Just the API key row, for the first-run setup sheet.
pub fn new_key_setup<V: 'static>(preview: bool, cx: &mut Context<V>) -> Entity<OpenRouterSettings> {
    cx.new(|cx| OpenRouterSettings::new(true, preview, cx))
}

pub struct KeyChanged(pub KeyStatus);
pub struct ConfigChanged;
#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    Models,
    Setup,
    Key(Provider),
    Global,
}

pub fn new_provider_key<V: 'static>(
    provider: Provider,
    preview: bool,
    cx: &mut Context<V>,
) -> Entity<OpenRouterSettings> {
    cx.new(|cx| OpenRouterSettings::with_mode(ViewMode::Key(provider), preview, cx))
}
pub fn new_global_options<V: 'static>(
    preview: bool,
    cx: &mut Context<V>,
) -> Entity<OpenRouterSettings> {
    cx.new(|cx| OpenRouterSettings::with_mode(ViewMode::Global, preview, cx))
}

/// Where the last action's outcome is shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scope {
    Configuration,
    Key,
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
    mode: ViewMode,
    key_provider: Provider,
    preview: bool,
    config: Config,
    key_status: Option<KeyStatus>,
    key_revision: u64,
    key_editing: bool,
    key_remove_armed: bool,
    key_input: Entity<TextInput>,
    available_providers: Vec<Provider>,
    catalog: CatalogState,
    catalog_revision: u64,
    picker: Option<ModelPicker>,
    model_focus: [FocusHandle; MAX_FALLBACKS + 1],
    advanced_open: bool,
    advanced_focus: FocusHandle,
    advanced: AdvancedInputs,
    advanced_saved: AdvancedForm,
    advanced_dirty: bool,
    key_operation: Option<KeyOperation>,
    message: Option<(Scope, bool, String)>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<KeyChanged> for OpenRouterSettings {}
impl EventEmitter<ConfigChanged> for OpenRouterSettings {}

impl OpenRouterSettings {
    fn new(key_only: bool, preview: bool, cx: &mut Context<Self>) -> Self {
        Self::with_mode(
            if key_only {
                ViewMode::Setup
            } else {
                ViewMode::Models
            },
            preview,
            cx,
        )
    }
    fn with_mode(mode: ViewMode, preview: bool, cx: &mut Context<Self>) -> Self {
        let key_provider = match mode {
            ViewMode::Key(provider) => provider,
            _ => Provider::OpenRouter,
        };
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
        let key_input = cx.new(|cx| TextInput::new(cx, "Paste API key", "").commit_on_blur());
        subscriptions.push(cx.subscribe(&key_input, |this, _, _: &Submitted, cx| {
            this.save_key(cx);
        }));
        subscriptions.push(cx.subscribe(&key_input, |this, _, _: &EditFinished, cx| {
            if (this.key_editing || matches!(this.key_status, Some(KeyStatus::Missing)))
                && !this.key_input.read(cx).text().trim().is_empty()
            {
                this.save_key(cx);
            }
        }));
        subscriptions.push(cx.subscribe(&key_input, |this, _, _: &Dismissed, cx| {
            this.cancel_key_replacement(cx);
        }));
        let form = AdvancedForm::from_config(&config);
        let mut field = |placeholder: &'static str, value: &str, cx: &mut Context<Self>| {
            let value = value.to_owned();
            let entity = cx.new(|cx| TextInput::new(cx, placeholder, &value).commit_on_blur());
            subscriptions.push(cx.subscribe(&entity, |this, _, _: &Changed, cx| {
                this.advanced_dirty = true;
                this.clear_message(Scope::Advanced);
                cx.notify();
            }));
            subscriptions.push(cx.subscribe(&entity, |this, _, _: &Submitted, cx| {
                this.save_advanced(cx);
            }));
            subscriptions.push(cx.subscribe(&entity, |this, _, _: &EditFinished, cx| {
                this.save_advanced(cx);
            }));
            subscriptions.push(cx.subscribe(&entity, |this, _, _: &Dismissed, cx| {
                this.load_advanced(&this.advanced_saved.clone(), cx);
                this.advanced_dirty = false;
                this.clear_message(Scope::Advanced);
                cx.notify();
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
            mode,
            key_provider,
            preview,
            config,
            key_status: None,
            key_revision: 0,
            key_editing: false,
            key_remove_armed: false,
            key_input,
            available_providers: if preview {
                Provider::ALL.to_vec()
            } else {
                Vec::new()
            },
            catalog: CatalogState::Idle,
            catalog_revision: 0,
            picker: None,
            model_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            advanced_open: false,
            advanced_focus: cx.focus_handle().tab_stop(true),
            advanced,
            advanced_saved: form,
            advanced_dirty: false,
            key_operation: None,
            message,
            _subscriptions: subscriptions,
        };
        if preview {
            view.sync_key_status(KeyStatus::Missing, cx);
        } else if matches!(mode, ViewMode::Key(_) | ViewMode::Setup) {
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
        cx: &mut Context<Self>,
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
                cx.emit(ConfigChanged);
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

    #[cfg(test)]
    pub(crate) fn stage_attempt_timeout_draft(&mut self, value: &str, cx: &mut Context<Self>) {
        self.advanced.attempt_timeout.update(cx, |input, cx| {
            input.set_text(value, cx);
            cx.emit(Changed);
        });
    }

    pub(crate) fn finish_editing(&mut self, cx: &mut Context<Self>) {
        if self.mode == ViewMode::Global {
            self.save_advanced(cx);
        }
        if matches!(self.mode, ViewMode::Key(_) | ViewMode::Setup)
            && self.key_input.read(cx).has_pending_edit()
            && !self.key_input.read(cx).text().trim().is_empty()
            && (self.key_editing || matches!(self.key_status, Some(KeyStatus::Missing)))
        {
            self.save_key(cx);
        }
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
        self.catalog_revision = self.catalog_revision.wrapping_add(1);
        self.message = None;
        cx.notify();
    }

    pub(crate) fn refresh_config(&mut self, config: Config, cx: &mut Context<Self>) {
        let endpoint_changed = self.config.base_url != config.base_url;
        self.config = config;
        if !self.advanced_dirty {
            self.advanced_saved = AdvancedForm::from_config(&self.config);
            self.load_advanced(&self.advanced_saved.clone(), cx);
        }
        if endpoint_changed {
            self.catalog = CatalogState::Idle;
            self.catalog_revision = self.catalog_revision.wrapping_add(1);
        }
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

    pub fn key_status(&self) -> Option<&KeyStatus> {
        self.key_status.as_ref()
    }

    fn refresh_key_status(&mut self, cx: &mut Context<Self>) {
        let config = self.config.clone();
        let revision = self.key_revision;
        let provider = self.key_provider;
        self.run(
            cx,
            KeyOperation::Refresh,
            move || keys::key_status(provider, &config),
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
        if let Err(error) = keys::validate_key(self.key_provider, &key) {
            self.report(Scope::Key, Err(error.to_string()));
            cx.notify();
            return;
        }
        if self.preview {
            self.key_changed(KeyStatus::Keychain("demo".into()), cx);
            self.clear_message(Scope::Key);
            return;
        }
        let provider = self.key_provider;
        self.run(
            cx,
            KeyOperation::Save,
            move || {
                keys::store_keychain_key(provider, &key)?;
                let config = super::load_config()?;
                Ok::<_, color_eyre::Report>((
                    keys::key_status(provider, &config),
                    keys::check_key(provider, &config),
                ))
            },
            |this, result, cx| match result {
                Ok((status, check)) => {
                    this.key_changed(status, cx);
                    match check {
                        Ok(_) => this.clear_message(Scope::Key),
                        Err(error) => this.report(
                            Scope::Key,
                            Err(format!(
                                "Key stored, but the provider could not validate it: {error:#}"
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
        let provider = self.key_provider;
        self.run(
            cx,
            KeyOperation::Test,
            move || super::load_config().and_then(|config| keys::check_key(provider, &config)),
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
        let provider = self.key_provider;
        self.run(
            cx,
            KeyOperation::Remove,
            move || {
                keys::delete_keychain_key(provider)?;
                super::load_config().map(|config| keys::key_status(provider, &config))
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
        if self.key_provider != Provider::OpenRouter || self.busy() {
            return;
        }
        if self.preview {
            self.config.api_key = None;
            self.key_changed(KeyStatus::Keychain("demo".into()), cx);
            self.clear_message(Scope::Key);
            return;
        }
        let provider = self.key_provider;
        self.run(
            cx,
            KeyOperation::Move,
            move || {
                let key = super::load_config()?.api_key.ok_or_else(|| {
                    color_eyre::eyre::eyre!("The file no longer contains an API key.")
                })?;
                keys::store_keychain_key(provider, &key)?;
                let config = super::update_config(|latest| {
                    form::remove_migrated_key(latest, &key)
                        .map_err(|error| color_eyre::eyre::eyre!("{error}"))
                })?;
                Ok::<_, color_eyre::Report>((keys::key_status(provider, &config), config))
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

    pub fn set_available_providers(
        &mut self,
        mut available: Vec<Provider>,
        cx: &mut Context<Self>,
    ) {
        available.sort();
        available.dedup();
        if self.available_providers == available {
            return;
        }
        let routed_changed = self.available_providers.contains(&Provider::OpenRouter)
            != available.contains(&Provider::OpenRouter);
        self.available_providers = available;
        if routed_changed {
            self.catalog_revision = self.catalog_revision.wrapping_add(1);
            self.catalog = CatalogState::Idle;
            if self.picker.is_some() {
                self.ensure_catalog(cx);
            }
        }
        self.highlight_current_model(cx);
        cx.notify();
    }

    fn model_available(&self, id: &str) -> bool {
        self.available_providers
            .contains(&crate::providers::ModelRef::parse(id).provider)
    }

    fn ensure_catalog(&mut self, cx: &mut Context<Self>) {
        if !self.available_providers.contains(&Provider::OpenRouter) {
            self.catalog = CatalogState::Loaded(Vec::new());
            return;
        }
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
                        provider: catalog::provider_label(id).into(),
                    })
                    .collect(),
            );
            return;
        }
        self.catalog = CatalogState::Loading;
        let config = self.config.clone();
        let revision = self.catalog_revision;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { catalog::fetch(&config) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.catalog_revision != revision {
                    return;
                }
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
        self.ensure_catalog(cx);
        let search = cx.new(|cx| TextInput::picker(cx, "Search name, provider or feature", ""));
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
        if self.picker.take().is_some() {
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
        let catalog = catalog::available_catalog(self.catalog.models());
        let mut choices = picker_choices(&catalog, query, |id| {
            self.model_available(id) && self.verified_keywords(id)
        });
        choices.retain(|choice| self.model_available(choice.id()));
        if query.is_empty()
            && let Some(current) = self.config.transcription.models.get(picker.slot)
            && self.model_available(current)
            && !choices.iter().any(|choice| choice.id() == current)
        {
            choices.insert(0, PickerChoice::Custom(current.clone()));
        }
        choices
    }

    fn choose_model(&mut self, slot: usize, model: Option<String>, cx: &mut Context<Self>) -> bool {
        if model.as_deref().is_some_and(|id| !self.model_available(id)) {
            self.report(
                Scope::Model(slot),
                Err("Add this provider’s key in Providers before selecting its models.".into()),
            );
            cx.notify();
            return false;
        }
        let success = match (&model, slot) {
            (None, _) => "Fallback removed.".to_owned(),
            (Some(model), 0) => format!("{model} is now the primary model."),
            (Some(model), slot) => format!("{model} is fallback {slot}."),
        };
        let saved = self.commit(
            cx,
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
            cx,
            Scope::Model(slot),
            |config| Ok(form::promote_model(config, slot)),
            "Order updated.",
        );
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
            cx,
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
            cx,
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
            cx,
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
                            .child(
                                self.key_operation
                                    .map(KeyOperation::label)
                                    .unwrap_or("••••••••••••"),
                            ),
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
                                        cx.open_url(this.key_provider.keys_url());
                                    }
                                })),
                        )
                        .child(div().flex().gap_2().when(has_key, |buttons| {
                            buttons.child(
                                button("Cancel", false)
                                    .id("openrouter-cancel-key")
                                    .when(self.busy(), |button| button.opacity(0.45))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_key_replacement(cx)
                                    })),
                            )
                        })),
                )
                .into_any_element();
        }
        let (badge, actions): (String, Vec<AnyElement>) = match &self.key_status {
            None => ("Checking…".into(), Vec::new()),
            Some(KeyStatus::Keychain(suffix)) => (
                // The suffix is known only once the key was read for a request,
                // because reading it for display could show a Keychain prompt.
                if suffix.is_empty() {
                    "Key saved".into()
                } else {
                    format!("Key saved · …{suffix}")
                },
                vec![
                    button(self.action_label(KeyOperation::Test, "Test"), false)
                        .id("openrouter-test-key")
                        .when(self.busy(), |button| button.opacity(0.45))
                        .on_click(cx.listener(|this, _, _, cx| this.test_key(cx)))
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
                format!("Key from {}", self.key_provider.env()),
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
                    .id("openrouter-key-badge")
                    .h(px(crate::desktop_ui::CONTROL_HEIGHT))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(crate::desktop_ui::CONTROL_RADIUS))
                    .bg(rgb(0x17231a))
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0x91bd99))
                    // Clicking the saved-key badge opens the replacement editor;
                    // the standalone Replace button was removed as redundant.
                    .when(
                        matches!(self.key_status, Some(KeyStatus::Keychain(_))),
                        |badge| {
                            badge
                                .cursor_pointer()
                                .hover(|badge| badge.bg(rgb(0x1d2c21)))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.begin_key_replacement(window, cx)
                                }))
                        },
                    )
                    .child(div().size(px(6.0)).rounded_full().bg(rgb(0x69d89f)))
                    .child(badge),
            )
            .children(actions)
            .into_any_element()
    }

    fn key_description(&self) -> &'static str {
        if matches!(self.mode, ViewMode::Key(_))
            && matches!(
                self.key_status,
                None | Some(KeyStatus::Keychain(_) | KeyStatus::Missing)
            )
        {
            return "";
        }
        match &self.key_status {
            None => "Looking for this provider’s key…",
            Some(KeyStatus::Keychain(_)) => "Stored in the macOS Keychain, never in a file",
            Some(KeyStatus::ConfigFile) => {
                "Read from openrouter.json in plain text; moving it to the Keychain is safer"
            }
            Some(KeyStatus::Environment) => "Set by the environment; it overrides any saved key",
            Some(KeyStatus::Missing) => "Uses your macOS Keychain",
        }
    }

    fn render_key_row(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let description = self.key_description();
        let control = self.render_key_control(cx);
        self.row_message(
            settings_row(self.key_provider.label(), description, control)
                .when(self.mode == ViewMode::Setup, |row| row.px_0()),
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

    fn verified_keywords(&self, id: &str) -> bool {
        !crate::providers::ModelRef::parse(id)
            .capabilities()
            .keywords
            && !self.preview
            && super::vocabulary_support::has_verified_support(&self.config, id)
    }

    fn render_capabilities(&self, id: &str, width: f32, selector: String) -> gpui::Div {
        let verified_keywords = self.verified_keywords(id);
        let debug_selector = selector.clone();
        div()
            .debug_selector(move || debug_selector.clone())
            .w(px(width))
            .h(px(16.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.0))
            .children(
                catalog::capability_badges(id, verified_keywords)
                    .into_iter()
                    .enumerate()
                    .map(|(index, (symbol, label))| {
                        let label_selector = format!("{selector}-label-{index}");
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(3.0))
                            .child(
                                div().size(px(10.0)).flex_none().child(
                                    gpui_symbols::Icon::new(symbol)
                                        .size(px(10.0))
                                        .color(rgb(MUTED))
                                        .rendering_mode(gpui_symbols::RenderingMode::Monochrome),
                                ),
                            )
                            .child(
                                div()
                                    .debug_selector(move || label_selector.clone())
                                    .flex_none()
                                    .text_size(px(10.0))
                                    .line_height(px(16.0))
                                    .text_color(rgb(MUTED))
                                    .child(label),
                            )
                    }),
            )
    }

    fn render_model_button(
        &self,
        slot: usize,
        model: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = model
            .map(|id| {
                format!(
                    "{} · {}",
                    catalog::provider_label(id),
                    catalog::label(id, self.catalog.models())
                )
            })
            .unwrap_or_else(|| "Choose a model".into());
        let open = self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.slot == slot);
        let menu = open.then(|| self.render_picker(slot, model, cx));
        div()
            .debug_selector(move || format!("model-control-{slot}"))
            .relative()
            .flex_none()
            .w(px(MODEL_BUTTON_WIDTH))
            .h(px(crate::desktop_ui::CONTROL_HEIGHT + 20.0))
            .flex()
            .flex_col()
            .gap_1()
            .child(
                disclosure_button(label)
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
            .children(model.map(|id| {
                if self.model_available(id) {
                    self.render_capabilities(
                        id,
                        MODEL_BUTTON_WIDTH,
                        format!("model-capabilities-{slot}"),
                    )
                } else {
                    div()
                        .w(px(MODEL_BUTTON_WIDTH))
                        .h(px(16.0))
                        .text_size(px(11.0))
                        .text_color(rgb(NEGATIVE))
                        .child("Provider key unavailable · connect in Providers")
                }
            }))
            .children(menu.map(picker_popup))
            .into_any_element()
    }

    fn render_model_notices(&self, slot: usize, id: &str) -> Option<AnyElement> {
        let notices = crate::providers::model_notices(&self.config, id);
        if notices.is_empty() {
            return None;
        }
        Some(
            div()
                .debug_selector(move || format!("model-notices-{slot}"))
                .w_full()
                .px_4()
                .pb_3()
                .flex()
                .justify_end()
                .child(
                    div()
                        .debug_selector(move || format!("model-notice-copy-{slot}"))
                        .w(px(MODEL_BUTTON_WIDTH))
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(notices.into_iter().map(|notice| {
                            div()
                                .whitespace_normal()
                                .text_size(px(11.0))
                                .line_height(px(16.0))
                                .text_color(rgb(if notice.is_error { NEGATIVE } else { MUTED }))
                                .child(notice.text)
                        })),
                )
                .into_any_element(),
        )
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
        let status: Option<AnyElement> = if self.available_providers.is_empty() {
            Some(
                picker_note("Add a provider key in Providers to choose models.").into_any_element(),
            )
        } else {
            match &self.catalog {
            CatalogState::Idle | CatalogState::Loading => {
                Some(picker_note("Loading OpenRouter models…").into_any_element())
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
                            .child(format!("OpenRouter catalog unavailable: {error}. Showing known models for connected providers.")),
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
        }
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
                    format!("{} · Custom model", catalog::provider_label(id)),
                ),
            };
            let capabilities = self.render_capabilities(
                choice.id(),
                PICKER_WIDTH - 64.0,
                format!("model-option-capabilities-{index}"),
            );
            let id = choice.id().to_owned();
            div()
                .id(("openrouter-model-choice", index))
                .debug_selector(move || format!("model-option-row-{index}"))
                .w_full()
                .h(px(64.0))
                .flex_none()
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
                        .flex_1()
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
                        )
                        .child(capabilities),
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

    fn render_models_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let models = self.config.transcription.models.clone();
        let mut panel = settings_panel();
        let primary = self.render_model_button(0, models.first().map(String::as_str), cx);
        let primary_notices = models
            .first()
            .and_then(|id| self.render_model_notices(0, id));
        panel = panel.child(
            self.row_message(
                settings_row("Primary model", "", primary)
                    .when(primary_notices.is_some(), |row| row.pb_1())
                    .debug_selector(|| "model-row-0".into()),
                Scope::Model(0),
                self.picker.as_ref().is_none_or(|picker| picker.slot != 0),
            )
            .debug_selector(|| "model-slot-0".into())
            .children(primary_notices),
        );
        let fallbacks = models.len().saturating_sub(1).min(MAX_FALLBACKS);
        for (slot, model) in models.iter().enumerate().skip(1).take(fallbacks) {
            let button = self.render_model_button(slot, Some(model.as_str()), cx);
            let notices = self.render_model_notices(slot, model);
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
                        "",
                        control,
                    )
                    .when(notices.is_some(), |row| row.pb_1())
                    .debug_selector(move || format!("model-row-{slot}")),
                    Scope::Model(slot),
                    self.picker
                        .as_ref()
                        .is_none_or(|picker| picker.slot != slot),
                )
                .debug_selector(move || format!("model-slot-{slot}"))
                .children(notices),
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

    fn toggle_advanced(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Moving focus commits the old input and leaves a reachable target
        // after collapse; hidden inputs must never retain keyboard focus.
        self.advanced_focus.focus(window);
        self.advanced_open = !self.advanced_open;
        cx.notify();
    }

    pub(crate) fn render_advanced(
        &mut self,
        extra: Option<AnyElement>,
        extra_errors: Vec<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = div()
            .id("openrouter-advanced")
            .debug_selector(|| "openrouter-advanced".into())
            .track_focus(&self.advanced_focus)
            .focus(|header| header.bg(rgb(SURFACE_SELECTED)))
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
            .hover(|header| header.bg(rgb(SURFACE_HOVER)))
            .child(if self.advanced_open { "▾" } else { "▸" })
            .child("ADVANCED")
            .on_click(cx.listener(|this, event, window, cx| {
                if matches!(event, gpui::ClickEvent::Mouse(_)) {
                    this.toggle_advanced(window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if modifiers.platform || modifiers.control || modifiers.alt {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "enter" | "space" => {
                        if !event.is_held {
                            this.toggle_advanced(window, cx);
                        }
                        cx.stop_propagation();
                    }
                    "tab" => {
                        if modifiers.shift {
                            window.focus_prev();
                        } else {
                            window.focus_next();
                        }
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }));
        let collapsed_feedback = div()
            .debug_selector(|| "advanced-collapsed-feedback".into())
            .when(!self.advanced_open, |feedback| {
                feedback
                    .children(self.render_error(Scope::Advanced))
                    .children(extra_errors.into_iter().map(|error| {
                        div()
                            .px_1()
                            .pt_2()
                            .text_size(px(11.0))
                            .line_height(px(16.0))
                            .text_color(rgb(NEGATIVE))
                            .child(error)
                    }))
            });
        if !self.advanced_open {
            return div()
                .child(header)
                .child(collapsed_feedback)
                .into_any_element();
        }
        let narrow = |input: &Entity<TextInput>| sized(input, NARROW_INPUT);
        let footer = div()
            .w_full()
            .px_4()
            .py_3()
            .flex()
            .items_center()
            .justify_end()
            .gap_4()
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
                    ),
            );
        div()
            .child(header)
            .child(collapsed_feedback)
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
                        "OpenRouter API URL",
                        "Used only by models routed through OpenRouter",
                        sized(&self.advanced.base_url, WIDE_INPUT),
                    ))
                    .child(footer),
            )
            .children(self.render_message(Scope::Advanced))
            .children(extra)
            .into_any_element()
    }
}

/// Catalog models matching `query`, then a custom-id choice when the query
/// looks like an id the catalog does not list.
fn picker_choices(
    catalog: &[CatalogModel],
    query: &str,
    mut verified_keywords: impl FnMut(&str) -> bool,
) -> Vec<PickerChoice> {
    let query = query.trim();
    let mut choices: Vec<PickerChoice> = catalog
        .iter()
        .filter(|model| {
            model.matches(query, false)
                || (verified_keywords(&model.id) && model.matches(query, true))
        })
        .cloned()
        .map(PickerChoice::Catalog)
        .collect();
    let looks_like_id =
        (query.contains('/') || query.contains("::")) && !query.chars().any(char::is_whitespace);
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
        .size(px(crate::desktop_ui::CONTROL_HEIGHT))
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
        match self.mode {
            ViewMode::Key(_) | ViewMode::Setup => {
                let row = self.render_key_row(cx).border_b_0();
                return div().child(row).into_any_element();
            }
            ViewMode::Global => return self.render_advanced(None, Vec::new(), cx),
            ViewMode::Models => {}
        }
        let models = self.render_models_panel(cx);
        div().on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "tab" {
                if this.picker.is_some() { this.close_model_picker(window); }
                if event.keystroke.modifiers.shift { window.focus_prev(); }
                else { window.focus_next(); }
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .children(self.render_message(Scope::Configuration))
        .child(models)
        .child(div().px_1().pt_3().text_size(px(11.0)).text_color(rgb(FAINT))
            .child("Models are tried from top to bottom until one succeeds. Keys and request limits are in Providers."))
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn feature_search_combines_terms_without_bypassing_provider_keys(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                for slot in 0..=MAX_FALLBACKS {
                    view.set_available_providers(vec![Provider::Deepgram], cx);
                    view.open_picker(slot, window, cx);
                    let search = view.picker.as_ref().unwrap().search.clone();
                    search.update(cx, |input, cx| input.set_text("  StReAmInG keywords  ", cx));
                    let choices = view.picker_choices(cx);
                    assert_eq!(choices.len(), 1);
                    assert_eq!(choices[0].id(), "deepgram::nova-3");
                    search.update(cx, |input, cx| input.set_text("google streaming", cx));
                    assert!(view.picker_choices(cx).is_empty());
                    view.set_available_providers(vec![Provider::Google], cx);
                    let choices = view.picker_choices(cx);
                    assert_eq!(choices.len(), 1);
                    assert_eq!(choices[0].id(), "google::gemini-3.5-transcribe-live");
                }
            });
        });
    }

    #[test]
    fn feature_search_uses_only_explicit_cached_keyword_evidence() {
        let catalog = [model("fixture/unknown-route", "Unknown route")];
        assert!(picker_choices(&catalog, "keywords", |_| false).is_empty());
        assert_eq!(
            picker_choices(&catalog, "keywords", |id| id == "fixture/unknown-route").len(),
            1
        );
        assert!(picker_choices(&catalog, "keywords streaming", |_| true).is_empty());
    }

    #[gpui::test]
    fn selected_notices_stay_under_their_own_primary_or_fallback_picker(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.simulate_resize(gpui::size(px(760.0), px(1800.0)));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.config.transcription.models = vec![
                    "elevenlabs::scribe_v2".into(),
                    "deepgram::nova-3".into(),
                    "elevenlabs::scribe_v2_realtime".into(),
                ];
                cx.notify();
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("model-notices-1").is_none());
        for (picker, note, row) in [
            ("model-control-0", "model-notice-copy-0", "model-slot-0"),
            ("model-control-2", "model-notice-copy-2", "model-slot-2"),
        ] {
            let picker = cx.debug_bounds(picker).unwrap();
            let note = cx.debug_bounds(note).unwrap();
            let row = cx.debug_bounds(row).unwrap();
            assert_eq!(note.size.width, px(MODEL_BUTTON_WIDTH));
            assert!((note.left() - picker.left()).abs() <= px(1.0));
            assert!(note.top() >= picker.bottom());
            assert!(note.bottom() <= row.bottom());
        }
        let before = cx.debug_bounds("model-slot-2").unwrap().size.height;
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert!(view.choose_model(2, Some("google::gemini-3.5-transcribe".into()), cx));
                assert!(
                    view.render_model_notices(2, &view.config.transcription.models[2])
                        .is_none()
                );
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("model-slot-2").unwrap().size.height < before);
    }

    #[gpui::test]
    fn microsoft_connection_notice_updates_on_the_selected_fallback(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.simulate_resize(gpui::size(px(760.0), px(1800.0)));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.config.transcription.models = vec![
                    "deepgram::nova-3".into(),
                    "microsoft::MAI-Transcribe-2-Streaming".into(),
                ];
                cx.notify();
            })
        });
        cx.run_until_parked();
        let initial = cx.debug_bounds("model-slot-1").unwrap().size.height;
        assert!(cx.debug_bounds("model-notice-copy-1").unwrap().size.height > px(0.0));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.config_snapshot();
                config.microsoft.streaming_endpoint =
                    "https://fixture.services.ai.azure.com".into();
                view.refresh_config(config, cx);
                assert!(
                    view.render_model_notices(1, &view.config.transcription.models[1])
                        .is_some()
                );
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.config_snapshot();
                config.microsoft.deployment = "fixture-deployment".into();
                view.refresh_config(config, cx);
                assert!(
                    view.render_model_notices(1, &view.config.transcription.models[1])
                        .is_none()
                );
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("model-slot-1").unwrap().size.height < initial);
    }

    #[gpui::test]
    fn model_choices_only_include_providers_with_registered_keys(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_available_providers(vec![Provider::OpenAi], cx);
                view.open_picker(0, window, cx);
                assert!(
                    view.catalog.models().is_empty(),
                    "no OpenRouter catalog should be loaded without its key"
                );
                let choices = view.picker_choices(cx);
                assert!(!choices.is_empty());
                assert!(choices.iter().all(|choice| {
                    crate::providers::ModelRef::parse(choice.id()).provider == Provider::OpenAi
                }));
                assert!(view.choose_model(0, Some("openai::gpt-transcribe".into()), cx));
                let saved = view.config_snapshot();
                view.set_available_providers(vec![Provider::Deepgram], cx);
                view.open_picker(0, window, cx);
                assert!(view.picker_choices(cx).iter().all(|choice| {
                    crate::providers::ModelRef::parse(choice.id()).provider == Provider::Deepgram
                }));
                assert!(!view.choose_model(0, Some("openai::gpt-transcribe".into()), cx));
                assert_eq!(
                    view.config_snapshot(),
                    saved,
                    "losing a key must not delete saved selections or profiles"
                );
                let search = view.picker.as_ref().unwrap().search.clone();
                search.update(cx, |input, cx| input.set_text("openai::custom-model", cx));
                assert!(
                    view.picker_choices(cx).is_empty(),
                    "custom IDs cannot bypass the provider filter"
                );
                view.set_available_providers(Vec::new(), cx);
                assert!(view.picker_choices(cx).is_empty());
                assert_eq!(view.config_snapshot(), saved);
            })
        });
    }

    #[gpui::test]
    fn selected_model_controls_and_capabilities_keep_compact_bounds(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                // Nova-3 exercises the densest current feature line: four badges.
                view.config.transcription.models = vec![
                    "deepgram::nova-3".into(),
                    "openai::gpt-transcribe".into(),
                    "elevenlabs::scribe_v2".into(),
                ];
                cx.notify();
            })
        });
        for width in [1040.0, 860.0] {
            cx.simulate_resize(gpui::size(
                px(width - crate::desktop_ui::SIDEBAR_WIDTH),
                px(720.0),
            ));
            cx.run_until_parked();
            for (row, control, badge) in [
                ("model-row-0", "model-control-0", "model-capabilities-0"),
                ("model-row-1", "model-control-1", "model-capabilities-1"),
                ("model-row-2", "model-control-2", "model-capabilities-2"),
            ] {
                let row = cx.debug_bounds(row).unwrap();
                let control = cx.debug_bounds(control).unwrap();
                let badge = cx.debug_bounds(badge).unwrap();
                assert_eq!(control.size.width, px(MODEL_BUTTON_WIDTH));
                assert_eq!(
                    control.size.height,
                    px(crate::desktop_ui::CONTROL_HEIGHT + 20.0)
                );
                assert_eq!(badge.size.height, px(16.0));
                assert_eq!(badge.size.width, px(MODEL_BUTTON_WIDTH));
                assert!(badge.size.width > px(150.0));
                assert!(row.size.height <= px(100.0), "inflated model row: {row:?}");
                assert!(control.bottom() <= row.bottom() + px(1.0));
                assert!(badge.bottom() <= control.bottom() + px(1.0));
            }
            let badge = cx.debug_bounds("model-capabilities-0").unwrap();
            for label in [
                "model-capabilities-0-label-0",
                "model-capabilities-0-label-3",
            ] {
                let label = cx.debug_bounds(label).unwrap();
                assert!(
                    label.size.width > px(5.0),
                    "capability label collapsed: {label:?}"
                );
                assert!(
                    label.left() >= badge.left() - px(1.0)
                        && label.right() <= badge.right() + px(1.0)
                );
            }
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.open_picker(0, window, cx);
                    let picker = view.picker.as_mut().unwrap();
                    picker
                        .search
                        .update(cx, |input, cx| input.set_text("deepgram::nova-3", cx));
                    picker.highlight = 0;
                    picker.scroll.scroll_to_item(0);
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let choice = cx.debug_bounds("model-option-row-0").unwrap();
            assert!(
                (choice.size.height - px(64.0)).abs() <= px(1.0),
                "inflated picker row: {choice:?}"
            );
            let badge = cx.debug_bounds("model-option-capabilities-0").unwrap();
            let label = cx
                .debug_bounds("model-option-capabilities-0-label-3")
                .unwrap();
            assert!(badge.size.width > px(150.0));
            assert!(label.size.width > px(5.0));
            assert!(
                label.right() <= badge.right() + px(1.0),
                "last badge is outside its line: {label:?} / {badge:?}"
            );
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.close_model_picker(window);
                    cx.notify();
                })
            });
        }
    }

    #[gpui::test]
    fn global_fields_save_on_blur_and_escape_reverts(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) =
            cx.add_window_view(|_, cx| OpenRouterSettings::with_mode(ViewMode::Global, true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.advanced_open = true;
                cx.notify();
            })
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.read(cx)
                .advanced
                .attempt_timeout
                .focus_handle(cx)
                .focus(window)
        });
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("45");
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).advanced.attempt_timeout.read(cx).text(), "45");
            assert!(
                view.read(cx)
                    .advanced
                    .attempt_timeout
                    .read(cx)
                    .has_pending_edit()
            );
            assert_eq!(
                view.read(cx).config.transcription.attempt_timeout_seconds,
                30
            );
        });
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).config.transcription.attempt_timeout_seconds,
                45
            )
        });
        cx.update(|window, cx| {
            view.read(cx)
                .advanced
                .attempt_timeout
                .focus_handle(cx)
                .focus(window)
        });
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("0");
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).config.transcription.attempt_timeout_seconds,
                45
            );
            assert!(view.read(cx).render_error(Scope::Advanced).is_some());
        });
        cx.update(|window, cx| {
            view.read(cx)
                .advanced
                .attempt_timeout
                .focus_handle(cx)
                .focus(window)
        });
        cx.simulate_keystrokes("escape");
        cx.update(|_, cx| assert_eq!(view.read(cx).advanced.attempt_timeout.read(cx).text(), "45"));
    }

    #[gpui::test]
    fn provider_key_saves_on_blur_and_escape_cancels(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| {
            OpenRouterSettings::with_mode(ViewMode::Key(Provider::OpenAi), true, cx)
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| view.read(cx).key_input.focus_handle(cx).focus(window));
        cx.simulate_input("preview-key-0123456789");
        cx.update(|_, cx| {
            assert!(view.read(cx).key_input.read(cx).has_pending_edit());
            assert_eq!(view.read(cx).key_status(), Some(&KeyStatus::Missing));
        });
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).key_status(),
                Some(&KeyStatus::Keychain("demo".into()))
            )
        });
        cx.update(|window, cx| view.update(cx, |view, cx| view.begin_key_replacement(window, cx)));
        cx.simulate_input("cancelled-preview-key");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(!view.read(cx).key_editing);
            assert!(view.read(cx).key_input.read(cx).text().is_empty());
        });
    }

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
    fn models_selector_supports_keyboard_and_native_choices_when_openrouter_fails(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| OpenRouterSettings::new(false, true, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(!view.has_key_operation());
                view.model_focus[0].focus(window);
                view.toggle_model_picker(0, window, cx);
                view.catalog = CatalogState::Failed("Offline fixture".into());
                assert!(
                    view.picker_choices(cx)
                        .iter()
                        .any(|choice| choice.id() == "deepgram::nova-3")
                );
            })
        });
        cx.simulate_input("deepgram::nova-3");
        cx.simulate_keystrokes("enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(view.config.transcription.models[0], "deepgram::nova-3");
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
            assert_eq!(picker.highlight, catalog::available_catalog(&[]).len() + 28);
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
                view.choose_model(0, Some("preview/model".into()), cx);
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
                view.choose_model(0, Some("preview/model".into()), cx);
                assert!(view.toggle_trim(cx).is_err());
                view.restore_advanced_defaults(cx);
                assert_eq!(view.config, original);
                view.key_operation = None;
                view.choose_model(0, Some("preview/model".into()), cx);
                assert_eq!(view.config.transcription.models[0], "preview/model");
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
        assert_eq!(picker_choices(&catalog, "", |_| false).len(), 2);
        assert_eq!(
            picker_choices(&catalog, "nova", |_| false),
            [PickerChoice::Catalog(catalog[1].clone())]
        );
        assert_eq!(
            picker_choices(&catalog, "acme/new-model", |_| false),
            [PickerChoice::Custom("acme/new-model".into())]
        );
        assert_eq!(
            picker_choices(&catalog, "openai/whisper-1", |_| false),
            [PickerChoice::Catalog(catalog[0].clone())]
        );
        assert!(picker_choices(&catalog, "no such thing", |_| false).is_empty());
        assert!(picker_choices(&[], "acme/x y", |_| false).is_empty());
    }
}
