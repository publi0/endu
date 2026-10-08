//! Per-model options. Profiles stay keyed by provider/model when a model leaves
//! the chain, and every edit rebases its one field onto the latest config.

use crate::desktop_ui::{
    ACCENT, LINE, NEGATIVE, NUMBER_INPUT_WIDTH, PickerState, SETTINGS_CONTROL_WIDTH, SURFACE,
    SURFACE_HOVER, SURFACE_SELECTED, TEXT_SOFT, disclosure_button, picker_open_key, picker_popup,
    settings_panel, settings_row, settings_section_label, toggle,
};
use crate::openrouter::{Config, LANGUAGES, settings_view::ConfigChanged};
use crate::providers::{self, ModelOptions, ModelRef};
use crate::text_input::{Changed, Dismissed, EditFinished, Submitted, TextInput};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, IntoElement, KeyDownEvent,
    MouseDownEvent, Render, Subscription, Window, div, prelude::*, px, rgb,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Language,
    Streaming,
    Context,
    Temperature,
    SmartFormat,
    Punctuate,
    Numerals,
    NoVerbatim,
}
#[derive(Clone)]
enum Edit {
    Language(String),
    Toggle(Field, bool),
    Context(String),
    Temperature(Option<f32>),
}

pub struct ModelOptionsView {
    config: Config,
    available_providers: Vec<providers::Provider>,
    preview: bool,
    selected: Option<String>,
    saved: ModelOptions,
    context: Entity<TextInput>,
    temperature: Entity<TextInput>,
    dirty: [bool; 2],
    error: Option<(Field, String)>,
    model_picker: PickerState,
    language_picker: PickerState,
    open: Option<Field>,
    model_open: bool,
    toggles: [FocusHandle; 5],
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<ConfigChanged> for ModelOptionsView {}

impl ModelOptionsView {
    pub fn new(config: Config, preview: bool, cx: &mut Context<Self>) -> Self {
        let selected = if preview {
            config.transcription.models.first().cloned()
        } else {
            None
        };
        let saved = selected
            .as_deref()
            .map(|id| providers::options(&config, id))
            .unwrap_or_default();
        let context = cx.new(|cx| {
            TextInput::new(cx, "Optional context for this model", &saved.prompt).commit_on_blur()
        });
        let temperature =
            cx.new(|cx| TextInput::new(cx, "Default", temperature_text(&saved)).commit_on_blur());
        let mut subscriptions = Vec::new();
        for (index, input) in [context.clone(), temperature.clone()]
            .into_iter()
            .enumerate()
        {
            subscriptions.push(cx.subscribe(&input, move |this, _, _: &Changed, cx| {
                this.dirty[index] = true;
                this.clear_error(text_field(index));
                cx.notify();
            }));
            subscriptions.push(cx.subscribe(&input, move |this, _, _: &Submitted, cx| {
                this.save_text(index, cx);
            }));
            subscriptions.push(cx.subscribe(&input, move |this, _, _: &EditFinished, cx| {
                this.save_text(index, cx);
            }));
            subscriptions.push(cx.subscribe(&input, move |this, _, _: &Dismissed, cx| {
                this.dirty[index] = false;
                this.clear_error(text_field(index));
                this.load_fields(cx);
                cx.notify();
            }));
        }
        Self {
            config,
            available_providers: if preview {
                providers::Provider::ALL.to_vec()
            } else {
                Vec::new()
            },
            preview,
            selected,
            saved,
            context,
            temperature,
            dirty: [false; 2],
            error: None,
            model_picker: PickerState::new(cx),
            language_picker: PickerState::new(cx),
            open: None,
            model_open: false,
            toggles: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            _subscriptions: subscriptions,
        }
    }

    fn available_models(&self) -> Vec<String> {
        self.config
            .transcription
            .models
            .iter()
            .filter(|id| {
                self.available_providers
                    .contains(&ModelRef::parse(id).provider)
            })
            .cloned()
            .collect()
    }
    pub fn set_available_providers(
        &mut self,
        mut available: Vec<providers::Provider>,
        cx: &mut Context<Self>,
    ) {
        available.sort();
        available.dedup();
        if available == self.available_providers {
            return;
        }
        self.available_providers = available;
        self.close_pickers(cx);
        self.refresh(self.config.clone(), cx);
    }

    pub fn config_snapshot(&self) -> Config {
        self.config.clone()
    }
    pub fn refresh(&mut self, config: Config, cx: &mut Context<Self>) {
        let eligible: Vec<_> = config
            .transcription
            .models
            .iter()
            .filter(|id| {
                self.available_providers
                    .contains(&ModelRef::parse(id).provider)
            })
            .cloned()
            .collect();
        let selected = self
            .selected
            .as_ref()
            .filter(|id| eligible.contains(id))
            .cloned()
            .or_else(|| eligible.first().cloned());
        let saved = selected
            .as_deref()
            .map(|id| providers::options(&config, id))
            .unwrap_or_default();
        if selected != self.selected {
            self.dirty = [false; 2];
            self.error = None;
        } else {
            if saved.prompt != self.saved.prompt {
                self.dirty[0] = false;
                self.clear_error(Field::Context);
            }
            if saved.temperature != self.saved.temperature {
                self.dirty[1] = false;
                self.clear_error(Field::Temperature);
            }
        }
        self.selected = selected;
        self.saved = saved;
        self.config = config;
        self.load_fields(cx);
        cx.notify();
    }
    pub fn apply_imported_config(&mut self, config: Config, cx: &mut Context<Self>) {
        self.dirty = [false; 2];
        self.error = None;
        self.close_pickers(cx);
        self.refresh(config, cx);
    }
    pub fn close_pickers(&mut self, cx: &mut Context<Self>) {
        self.model_open = false;
        self.open = None;
        cx.notify();
    }
    pub fn finish_editing(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_text(0, cx) && self.save_text(1, cx)
    }
    fn load_fields(&mut self, cx: &mut Context<Self>) {
        if !self.dirty[0] {
            self.context
                .update(cx, |input, cx| input.set_text(&self.saved.prompt, cx));
        }
        if !self.dirty[1] {
            self.temperature.update(cx, |input, cx| {
                input.set_text(temperature_text(&self.saved), cx)
            });
        }
    }
    fn clear_error(&mut self, field: Field) {
        if self
            .error
            .as_ref()
            .is_some_and(|(current, _)| *current == field)
        {
            self.error = None;
        }
    }
    fn save_text(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        if !self.dirty[index] {
            return true;
        }
        let field = text_field(index);
        let edit = if index == 0 {
            Ok(Edit::Context(self.context.read(cx).text().to_owned()))
        } else {
            parse_temperature(self.temperature.read(cx).text()).map(Edit::Temperature)
        };
        let result = match edit {
            Ok(edit) => self.commit(field, edit, cx),
            Err(error) => {
                self.error = Some((field, error));
                cx.notify();
                false
            }
        };
        if result {
            self.dirty[index] = false;
            self.load_fields(cx);
        }
        result
    }
    fn commit(&mut self, field: Field, edit: Edit, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.selected.clone() else {
            return false;
        };
        let apply = |base: &Config| edit_model(base, &id, &edit);
        let result = if self.preview {
            apply(&self.config)
        } else {
            crate::openrouter::update_config(|base| {
                apply(base).map_err(|error| color_eyre::eyre::eyre!("{error}"))
            })
            .map_err(|error| format!("{error:#}"))
        };
        match result {
            Ok(config) => {
                self.saved = providers::options(&config, &id);
                self.config = config;
                self.clear_error(field);
                self.load_fields(cx);
                cx.emit(ConfigChanged);
                cx.notify();
                true
            }
            Err(error) => {
                self.error = Some((field, error));
                cx.notify();
                false
            }
        }
    }
    fn toggle_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.available_models().is_empty() || !self.finish_editing(cx) {
            return;
        }
        self.open = None;
        self.model_open = !self.model_open;
        if self.model_open {
            let available = self.available_models();
            let index = available
                .iter()
                .position(|id| Some(id) == self.selected.as_ref())
                .unwrap_or(0);
            self.model_picker.open(index, available.len(), window);
        } else {
            self.model_picker.trigger.focus(window);
        }
        cx.notify();
    }
    fn choose_model(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.finish_editing(cx) {
            return;
        }
        let Some(id) = self.available_models().get(index).cloned() else {
            return;
        };
        self.selected = Some(id.clone());
        self.saved = providers::options(&self.config, &id);
        self.dirty = [false; 2];
        self.error = None;
        self.load_fields(cx);
        self.model_open = false;
        self.model_picker.trigger.focus(window);
        cx.notify();
    }
    fn model_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if self
            .model_picker
            .navigate(key, self.available_models().len())
        {
        } else if matches!(key, "enter" | "space") {
            self.choose_model(self.model_picker.highlight, window, cx);
        } else if matches!(key, "escape" | "tab") {
            self.model_open = false;
            self.model_picker.close(event, window);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }
    fn toggle_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_open = false;
        if self.open == Some(Field::Language) {
            self.open = None;
            self.language_picker.trigger.focus(window);
        } else {
            self.open = Some(Field::Language);
            let index = LANGUAGES
                .iter()
                .position(|(code, _)| *code == self.saved.language)
                .unwrap_or(0);
            self.language_picker.open(index, LANGUAGES.len(), window);
        }
        cx.notify();
    }
    fn choose_language(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((code, _)) = LANGUAGES.get(index)
            && self.commit(Field::Language, Edit::Language((*code).into()), cx)
        {
            self.open = None;
            self.language_picker.trigger.focus(window);
        }
    }
    fn language_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if self.language_picker.navigate(key, LANGUAGES.len()) {
        } else if matches!(key, "enter" | "space") {
            self.choose_language(self.language_picker.highlight, window, cx);
        } else if matches!(key, "escape" | "tab") {
            self.open = None;
            self.language_picker.close(event, window);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }
    fn error_note(&self, field: Field) -> Option<AnyElement> {
        self.error
            .as_ref()
            .filter(|(current, _)| *current == field)
            .map(|(_, error)| {
                div()
                    .w_full()
                    .min_w_0()
                    .px_4()
                    .pb_3()
                    .whitespace_normal()
                    .text_size(px(11.0))
                    .text_color(rgb(NEGATIVE))
                    .child(error.clone())
                    .into_any_element()
            })
    }
    fn row(
        &self,
        field: Field,
        title: &'static str,
        detail: &'static str,
        control: AnyElement,
    ) -> AnyElement {
        div()
            .child(settings_row(title, detail, control).border_b_0())
            .when(self.open != Some(field), |row| {
                row.children(self.error_note(field))
            })
            .border_b_1()
            .border_color(rgb(LINE))
            .into_any_element()
    }
    fn toggle_row(
        &self,
        field: Field,
        index: usize,
        title: &'static str,
        detail: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let control = div()
            .id(("model-option-toggle", index))
            .debug_selector(move || format!("model-option-toggle-{index}"))
            .track_focus(&self.toggles[index])
            .w(px(SETTINGS_CONTROL_WIDTH))
            .h(px(crate::desktop_ui::CONTROL_HEIGHT))
            .flex()
            .items_center()
            .justify_end()
            .rounded_sm()
            .focus(|style| style.bg(rgb(SURFACE_HOVER)))
            .child(toggle(if enabled { 1.0 } else { 0.0 }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.commit(field, Edit::Toggle(field, !enabled), cx);
            }))
            .into_any_element();
        self.row(field, title, detail, control)
    }
    fn render_model_selector(&self, cx: &mut Context<Self>) -> AnyElement {
        let label = self
            .selected
            .as_deref()
            .map(|id| ModelRef::parse(id).label())
            .unwrap_or_else(|| "No model with a configured provider".into());
        let menu =
            self.model_open.then(|| {
                menu_frame("provider-options-model-menu")
                    .track_focus(&self.model_picker.menu)
                    .on_key_down(cx.listener(Self::model_keys))
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.model_open = false;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .id("provider-model-choices")
                            .max_h(px(240.0))
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.model_picker.scroll)
                            .children(self.available_models().iter().enumerate().map(
                                |(index, id)| {
                                    choice(
                                        ModelRef::parse(id).label(),
                                        index == self.model_picker.highlight,
                                    )
                                    .id(("provider-model-choice", index))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.choose_model(index, window, cx)
                                        },
                                    ))
                                },
                            )),
                    )
                    .into_any_element()
            });
        div()
            .relative()
            .flex_none()
            .child(
                disclosure_button(label)
                    .id("provider-options-model")
                    .track_focus(&self.model_picker.trigger)
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .on_click(cx.listener(|this, event, window, cx| {
                        if matches!(event, gpui::ClickEvent::Mouse(_)) {
                            this.toggle_model_picker(window, cx);
                        }
                    }))
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        if picker_open_key(event) {
                            this.toggle_model_picker(window, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
            .children(menu.map(picker_popup))
            .into_any_element()
    }
    fn render_language(&self, cx: &mut Context<Self>) -> AnyElement {
        let menu = (self.open == Some(Field::Language)).then(|| {
            menu_frame("provider-language-menu")
                .track_focus(&self.language_picker.menu)
                .on_key_down(cx.listener(Self::language_keys))
                .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.open = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("provider-language-choices")
                        .max_h(px(240.0))
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.language_picker.scroll)
                        .children(LANGUAGES.iter().enumerate().map(|(index, (_, name))| {
                            choice((*name).into(), index == self.language_picker.highlight)
                                .id(("provider-language-choice", index))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.choose_language(index, window, cx)
                                }))
                        })),
                )
                .children(self.error_note(Field::Language))
                .into_any_element()
        });
        div()
            .relative()
            .flex_none()
            .child(
                disclosure_button(
                    crate::openrouter::language_name(&self.saved.language).to_owned(),
                )
                .id("provider-model-language")
                .track_focus(&self.language_picker.trigger)
                .focus(|style| style.border_color(rgb(ACCENT)))
                .on_click(cx.listener(|this, event, window, cx| {
                    if matches!(event, gpui::ClickEvent::Mouse(_)) {
                        this.toggle_language(window, cx);
                    }
                }))
                .on_key_down(cx.listener(|this, event, window, cx| {
                    if picker_open_key(event) {
                        this.toggle_language(window, cx);
                        cx.stop_propagation();
                    }
                })),
            )
            .children(menu.map(picker_popup))
            .into_any_element()
    }
}

impl Render for ModelOptionsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut panel = settings_panel().child(settings_row(
            "Model",
            "Options are remembered for each model, including after removal from the chain.",
            self.render_model_selector(cx),
        ));
        if let Some(id) = &self.selected {
            let caps = ModelRef::parse(id).capabilities();
            panel = panel.child(self.row(
                Field::Language,
                "Language",
                "Spoken-language hint for this model",
                self.render_language(cx),
            ));
            if caps.streaming {
                panel = panel.child(self.toggle_row(Field::Streaming, 0, "Streaming", "Transcribe while recording. Silence trimming applies only to recorded-audio requests.", self.saved.streaming, cx));
            }
            if caps.prompt {
                panel = panel.child(
                    self.row(
                        Field::Context,
                        "Context",
                        "Optional instructions or context supported by this model",
                        div()
                            .debug_selector(|| "model-options-context".into())
                            .w(px(SETTINGS_CONTROL_WIDTH))
                            .child(self.context.clone())
                            .into_any_element(),
                    ),
                );
            }
            if caps.temperature {
                panel = panel.child(
                    self.row(
                        Field::Temperature,
                        "Temperature",
                        "0 to 1; leave empty to use the provider default",
                        div()
                            .w(px(NUMBER_INPUT_WIDTH))
                            .child(self.temperature.clone())
                            .into_any_element(),
                    ),
                );
            }
            if caps.formatting {
                let google = ModelRef::parse(id).provider == providers::Provider::Google;
                panel = panel.child(self.toggle_row(
                    Field::SmartFormat,
                    1,
                    if google {
                        "Smart transcription"
                    } else {
                        "Smart formatting"
                    },
                    formatting_description(ModelRef::parse(id).provider),
                    self.saved.smart_format,
                    cx,
                ));
            }
            if caps.punctuate {
                panel = panel.child(self.toggle_row(
                    Field::Punctuate,
                    2,
                    "Punctuation",
                    "Ask the provider to add punctuation",
                    self.saved.punctuate,
                    cx,
                ));
            }
            if caps.numerals {
                panel = panel.child(self.toggle_row(
                    Field::Numerals,
                    3,
                    "Numerals",
                    "Ask the provider to render numbers as digits",
                    self.saved.numerals,
                    cx,
                ));
            }
            if caps.no_verbatim {
                panel = panel.child(self.toggle_row(
                    Field::NoVerbatim,
                    4,
                    "Clean transcription",
                    "Use this model's non-verbatim transcription option",
                    self.saved.no_verbatim,
                    cx,
                ));
            }
        }
        div()
            .on_key_down(|event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                }
            })
            .child(settings_section_label("MODEL OPTIONS"))
            .child(panel)
    }
}

fn formatting_description(provider: providers::Provider) -> &'static str {
    if provider == providers::Provider::Google {
        "Remove fillers and repetitions and format the transcript together"
    } else {
        "Let the provider format dates, amounts and similar expressions"
    }
}

fn text_field(index: usize) -> Field {
    if index == 0 {
        Field::Context
    } else {
        Field::Temperature
    }
}
fn temperature_text(options: &ModelOptions) -> String {
    options
        .temperature
        .map(|v| v.to_string())
        .unwrap_or_default()
}
fn parse_temperature(text: &str) -> Result<Option<f32>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    text.trim()
        .replace(',', ".")
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
        .map(Some)
        .ok_or_else(|| "Temperature must be between 0 and 1.".into())
}
fn edit_model(base: &Config, id: &str, edit: &Edit) -> Result<Config, String> {
    let model = ModelRef::parse(id);
    let caps = model.capabilities();
    let mut options = providers::options(base, id);
    match edit {
        Edit::Language(value) => options.language.clone_from(value),
        Edit::Context(value) if caps.prompt => options.prompt.clone_from(value),
        Edit::Temperature(value) if caps.temperature => options.temperature = *value,
        Edit::Toggle(Field::Streaming, value) if caps.streaming => options.streaming = *value,
        Edit::Toggle(Field::SmartFormat, value) if caps.formatting => options.smart_format = *value,
        Edit::Toggle(Field::Punctuate, value) if caps.punctuate => options.punctuate = *value,
        Edit::Toggle(Field::Numerals, value) if caps.numerals => options.numerals = *value,
        Edit::Toggle(Field::NoVerbatim, value) if caps.no_verbatim => options.no_verbatim = *value,
        _ => return Err("This model does not support that option.".into()),
    }
    options.validate()?;
    let mut config = base.clone();
    config
        .transcription
        .model_options
        .insert(model.key(), options);
    providers::validate_profiles(&config.transcription.model_options)?;
    Ok(config)
}
fn menu_frame(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(SETTINGS_CONTROL_WIDTH))
        .max_h(px(320.0))
        .p_2()
        .flex()
        .flex_col()
        .rounded_md()
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(SURFACE))
        .shadow_lg()
        .occlude()
}
fn choice(label: String, active: bool) -> gpui::Div {
    div()
        .min_h(px(crate::desktop_ui::CONTROL_HEIGHT))
        .px_3()
        .py_2()
        .text_size(px(12.0))
        .text_color(rgb(TEXT_SOFT))
        .when(active, |row| row.bg(rgb(SURFACE_SELECTED)))
        .hover(|row| row.bg(rgb(SURFACE_HOVER)))
        .child(div().w_full().min_w_0().whitespace_normal().child(label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Focusable;

    #[gpui::test]
    fn provider_formatting_controls_follow_each_capability(cx: &mut gpui::TestAppContext) {
        let mut config = Config::default();
        let google = providers::native_models()
            .iter()
            .find(|model| model.provider == providers::Provider::Google)
            .unwrap();
        config.transcription.models = vec![format!("google::{}", google.id)];
        let (view, cx) = cx.add_window_view(|_, cx| ModelOptionsView::new(config, true, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("model-option-toggle-1").is_some());
        assert!(cx.debug_bounds("model-option-toggle-2").is_none());
        assert!(cx.debug_bounds("model-option-toggle-3").is_none());
        assert!(cx.debug_bounds("model-option-toggle-4").is_none());
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert!(
                    edit_model(
                        &view.config,
                        view.selected.as_deref().unwrap(),
                        &Edit::Toggle(Field::Punctuate, true)
                    )
                    .is_err()
                );
                let mut config = view.config_snapshot();
                config.transcription.models = vec!["grok::grok-voice-transcribe-2.0".into()];
                view.refresh(config, cx);
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("model-option-toggle-1").is_some());
        assert!(cx.debug_bounds("model-option-toggle-2").is_none());
        assert!(cx.debug_bounds("model-option-toggle-3").is_none());
        assert!(cx.debug_bounds("model-option-toggle-4").is_some());
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.config_snapshot();
                config.transcription.models = vec!["deepgram::nova-3".into()];
                view.refresh(config, cx);
            })
        });
        cx.run_until_parked();
        for selector in [
            "model-option-toggle-1",
            "model-option-toggle-2",
            "model-option-toggle-3",
        ] {
            assert!(cx.debug_bounds(selector).is_some());
        }
    }

    fn fixture() -> Config {
        let mut config = Config::default();
        config.transcription.models =
            vec!["openai::gpt-transcribe".into(), "deepgram::nova-3".into()];
        config.transcription.model_options.insert(
            "openai::gpt-transcribe".into(),
            ModelOptions {
                prompt: "Original context".into(),
                temperature: Some(0.2),
                ..ModelOptions::default()
            },
        );
        config.transcription.model_options.insert(
            "deepgram::nova-3".into(),
            ModelOptions {
                language: "pt".into(),
                streaming: true,
                ..ModelOptions::default()
            },
        );
        config
    }

    #[gpui::test]
    fn unknown_credentials_do_not_expose_a_model_before_first_refresh(
        cx: &mut gpui::TestAppContext,
    ) {
        let original = fixture();
        let (view, cx) =
            cx.add_window_view(|_, cx| ModelOptionsView::new(original.clone(), false, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.set_available_providers(Vec::new(), cx);
                assert!(view.selected.is_none());
                assert!(view.available_models().is_empty());
                assert_eq!(view.config_snapshot(), original);
                view.set_available_providers(vec![providers::Provider::OpenAi], cx);
                assert_eq!(view.selected.as_deref(), Some("openai::gpt-transcribe"));
                assert_eq!(view.saved.prompt, "Original context");
            })
        });
    }

    #[gpui::test]
    fn options_picker_filters_credentials_without_deleting_profiles(cx: &mut gpui::TestAppContext) {
        let original = fixture();
        let (view, cx) =
            cx.add_window_view(|_, cx| ModelOptionsView::new(original.clone(), true, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_available_providers(vec![providers::Provider::Deepgram], cx);
                assert_eq!(view.available_models(), vec!["deepgram::nova-3"]);
                assert_eq!(view.selected.as_deref(), Some("deepgram::nova-3"));
                view.choose_model(0, window, cx);
                assert_eq!(view.selected.as_deref(), Some("deepgram::nova-3"));
                view.set_available_providers(Vec::new(), cx);
                assert!(view.selected.is_none());
                assert!(view.available_models().is_empty());
                assert_eq!(view.config_snapshot(), original);
                view.set_available_providers(vec![providers::Provider::OpenAi], cx);
                assert_eq!(view.selected.as_deref(), Some("openai::gpt-transcribe"));
                assert_eq!(view.saved.prompt, "Original context");
            })
        });
    }

    #[test]
    fn edits_are_scoped_to_one_profile_and_capability() {
        let base = fixture();
        let edited = edit_model(
            &base,
            "openai::gpt-transcribe",
            &Edit::Context("New context".into()),
        )
        .unwrap();
        assert_eq!(edited.transcription.models, base.transcription.models);
        assert_eq!(
            providers::options(&edited, "deepgram::nova-3"),
            providers::options(&base, "deepgram::nova-3")
        );
        assert!(
            edit_model(
                &base,
                "deepgram::nova-3",
                &Edit::Context("Unsupported".into())
            )
            .is_err()
        );
        let enabled = edit_model(
            &base,
            "deepgram::nova-3",
            &Edit::Toggle(Field::Streaming, true),
        )
        .unwrap();
        assert!(providers::options(&enabled, "deepgram::nova-3").streaming);
        assert_eq!(parse_temperature(" ").unwrap(), None);
        assert_eq!(parse_temperature("0,5").unwrap(), Some(0.5));
        assert!(parse_temperature("NaN").is_err());
    }

    #[gpui::test]
    fn context_saves_on_blur_and_model_switches_keep_independent_profiles(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| ModelOptionsView::new(fixture(), true, cx));
        // GPUI intentionally omits focus paths for an inactive native window.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| view.read(cx).context.focus_handle(cx).focus(window));
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("Updated context");
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).context.read(cx).text(), "Updated context");
            assert!(view.read(cx).context.read(cx).has_pending_edit());
            assert_eq!(
                providers::options(&view.read(cx).config, "openai::gpt-transcribe").prompt,
                "Original context"
            )
        });
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(
                    providers::options(&view.config, "openai::gpt-transcribe").prompt,
                    "Updated context"
                );
                view.choose_model(1, window, cx);
                assert_eq!(view.saved.language, "pt");
                assert!(view.saved.streaming);
                view.choose_language(2, window, cx);
                assert_eq!(
                    providers::options(&view.config, "deepgram::nova-3").language,
                    LANGUAGES[2].0
                );
                view.choose_model(0, window, cx);
                assert_eq!(view.context.read(cx).text(), "Updated context");
            })
        });
    }

    #[gpui::test]
    fn escaping_a_numeric_draft_cancels_it_and_picker_key_up_does_not_reopen(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(|_, cx| ModelOptionsView::new(fixture(), true, cx));
        cx.update(|window, cx| view.read(cx).temperature.focus_handle(cx).focus(window));
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("0.");
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            assert_eq!(view.read(cx).temperature.read(cx).text(), "0.2");
            assert_eq!(view.read(cx).saved.temperature, Some(0.2));
            view.read(cx).model_picker.trigger.focus(window);
        });
        cx.simulate_keystrokes("enter down enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            assert_eq!(view.read(cx).selected.as_deref(), Some("deepgram::nova-3"));
            assert!(!view.read(cx).model_open);
            assert!(view.read(cx).model_picker.trigger.is_focused(window));
        });
    }

    #[gpui::test]
    fn unrelated_config_refresh_preserves_draft_but_import_replaces_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| ModelOptionsView::new(fixture(), true, cx));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.context
                    .update(cx, |input, cx| input.set_text("Draft", cx));
                view.dirty[0] = true;
                let mut config = view.config.clone();
                config
                    .transcription
                    .model_options
                    .get_mut("openai::gpt-transcribe")
                    .unwrap()
                    .language = "pt".into();
                view.refresh(config.clone(), cx);
                assert_eq!(view.context.read(cx).text(), "Draft");
                view.apply_imported_config(config, cx);
                assert_eq!(view.context.read(cx).text(), "Original context");
                assert!(!view.dirty[0]);
            })
        });
    }
}
