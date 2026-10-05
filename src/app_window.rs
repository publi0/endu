//! The app window: Settings, Models, HUD, History, and Statistics, plus first-run
//! setup sheet.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, Div, Entity, FocusHandle, FontWeight, IntoElement,
    KeyDownEvent, Modifiers as GpuiModifiers, ModifiersChangedEvent, MouseDownEvent,
    PathPromptOptions, Render, SharedString, Subscription, Timer, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions, actions, div, prelude::*, px, rgb, rgba, size,
};

use crate::app_settings::{
    AppSettings, DictationMode, HotkeyBinding, HotkeyKey, HotkeyModifiers, ModifierSide,
    RecordingAudioBehavior,
};
use crate::desktop_ui::{
    ACCENT, CANVAS, FAINT, LINE, MUTED, NEGATIVE, NavigationIcon, PANE_CONTENT_WIDTH,
    PANE_LIST_WIDTH, PANEL_RADIUS, PickerState, SIDEBAR_WIDTH, SURFACE, SURFACE_HOVER,
    SURFACE_SELECTED, TEXT, TEXT_SOFT, compact_button, compact_panel, disclosure_button,
    error_message, header_button, hotkey_keycaps, mix_color, navigation_item, pane_body,
    pane_content, pane_header, pane_header_with_action, pane_list, picker_open_key, picker_popup,
    section_label, settings_copy, settings_panel, settings_row, settings_section_label,
    sidebar_frame, sliding_segmented_control, sliding_segmented_item, toggle, window_frame,
};
use crate::history::{History, HistoryEntry, HistoryRetention};
use crate::hud_settings_view::{HudChange, HudSettingsView};
use crate::interaction_settings::DoubleTapSensitivity;
use crate::login_item::{LoginItemRequest, LoginItemResponse, LoginItemStatus, LoginItemWorker};
use crate::microphone_priority_view::{MicrophonePriorityView, PriorityChange};
use crate::onboarding::{
    PermissionAction, PermissionKind, PermissionState, PermissionWarning, SetupStatus,
};
use crate::openrouter::settings_view::{KeyChanged, OpenRouterSettings};
use crate::openrouter::stats_view::StatisticsView;
use crate::recording_recovery::{RecordingRecovery, RecoveryEntry, RecoveryStatus};
use crate::sound_settings_view::{SoundEvent, SoundSettingsView, SoundVolumeChange};
use crate::text_input::{Changed as TextChanged, Submitted as TextSubmitted, TextInput};

const WINDOW_WIDTH: f32 = 1040.0;
const WINDOW_HEIGHT: f32 = 720.0;
const MINIMUM_WIDTH: f32 = 860.0;
const MINIMUM_HEIGHT: f32 = 560.0;
const HOTKEY_MIN_WIDTH: f32 = 148.0;
const HOTKEY_SIDE_SELECTOR_WIDTH: f32 = 116.0;
const PERMISSION_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

actions!(
    hex,
    [
        CloseWindow,
        HideApplication,
        MinimizeWindow,
        QuitApplication,
        ShowHistory,
        ShowModels,
        ShowHud,
        ShowSettings,
        ShowStatistics,
        ToggleFullscreen,
    ]
);

/// The microphone menu is deferred above the settings panel and occludes
/// everything beneath it, so hovering or choosing a device never reaches the
/// controls under the menu. A mouse-down anywhere else dismisses it.
fn selection_picker_menu<T: Clone + PartialEq + 'static>(
    picker_id: &'static str,
    picker: &PickerState,
    choices: Vec<T>,
    selected: T,
    label: impl Fn(&T) -> String,
    error: Option<String>,
    handlers: (
        impl Fn(&T, &mut Window, &mut App) + 'static,
        impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
        impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    ),
) -> gpui::Stateful<Div> {
    let (choose, dismiss, keys) = handlers;
    let choose = Rc::new(choose);
    let rows = choices.into_iter().enumerate().map(|(index, device)| {
        let is_selected = selected == device;
        let label = label(&device);
        let choose = choose.clone();
        div()
            .id((picker_id, index))
            .w_full()
            .h(px(34.0))
            .flex_none()
            .px_3()
            .flex()
            .items_center()
            .justify_between()
            .rounded_sm()
            .text_size(px(12.0))
            .text_color(rgb(if is_selected { TEXT } else { TEXT_SOFT }))
            .when(index == picker.highlight, |row| {
                row.bg(rgb(SURFACE_SELECTED))
            })
            .hover(|row| row.bg(rgb(SURFACE_HOVER)))
            .child(label)
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                choose(&device, window, cx);
            })
    });
    div()
        .id(picker_id)
        .debug_selector(move || picker_id.into())
        .track_focus(&picker.menu)
        .w(px(220.0))
        .max_h(px(300.0))
        .p_2()
        .flex()
        .flex_col()
        .rounded_sm()
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(SURFACE))
        .shadow_lg()
        .occlude()
        .on_mouse_down_out(dismiss)
        .on_key_down(keys)
        .child(
            div()
                .id("picker-choices")
                .min_h_0()
                .max_h(px(240.0))
                .track_scroll(&picker.scroll)
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(rows),
        )
        .when_some(error, |picker, error| {
            picker.child(
                div()
                    .id("picker-feedback")
                    .debug_selector(|| "picker-feedback".into())
                    .flex_none()
                    .px_3()
                    .pt_2()
                    .text_size(px(11.0))
                    .text_color(rgb(NEGATIVE))
                    .child(error),
            )
        })
}

fn configuration_pane(title: &'static str, scroll_id: &'static str, content: Div) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(pane_header(title))
        .child(
            div()
                .id(scroll_id)
                .flex_1()
                .overflow_y_scroll()
                .px_8()
                .pt_1()
                .pb_7()
                .child(
                    div().w_full().flex().justify_center().child(
                        content
                            .w_full()
                            .max_w(px(PANE_CONTENT_WIDTH))
                            .min_w_0()
                            .relative(),
                    ),
                ),
        )
        .into_any_element()
}

type AppWindowSlot = Rc<RefCell<Option<WindowHandle<AppWindow>>>>;

/// Opens the app window, or focuses and refreshes the open one.
pub fn open_or_focus(
    app_window: &AppWindowSlot,
    listener_start: Option<SyncSender<()>>,
    history: Option<History>,
    cx: &mut App,
) -> gpui::Result<WindowHandle<AppWindow>> {
    if let Some(handle) = app_window.borrow().as_ref().copied()
        && handle
            .update(cx, |this, window, cx| {
                this.listener_start = listener_start.clone();
                this.history = history.clone();
                this.refresh(cx);
                window.activate_window();
            })
            .is_ok()
    {
        cx.activate(true);
        return Ok(handle);
    }
    open_new(app_window, listener_start, history, None, cx)
}

pub fn open_preview(
    app_window: &AppWindowSlot,
    preview: AppWindowPreview,
    cx: &mut App,
) -> gpui::Result<WindowHandle<AppWindow>> {
    if let Some(handle) = app_window.borrow().as_ref().copied()
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.activate(true);
        return Ok(handle);
    }
    open_new(app_window, None, None, Some(preview), cx)
}

fn open_new(
    app_window: &AppWindowSlot,
    listener_start: Option<SyncSender<()>>,
    history: Option<History>,
    preview: Option<AppWindowPreview>,
    cx: &mut App,
) -> gpui::Result<WindowHandle<AppWindow>> {
    let preview_mode = preview.is_some();
    let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Hex".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            is_resizable: !preview_mode,
            is_minimizable: true,
            window_min_size: Some(size(px(MINIMUM_WIDTH), px(MINIMUM_HEIGHT))),
            tabbing_identifier: preview_mode.then(|| "hex-preview".into()),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| AppWindow::new(listener_start, history, preview, window, cx)),
    )?;
    *app_window.borrow_mut() = Some(handle);
    cx.activate(true);
    Ok(handle)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewPane {
    Settings,
    Models,
    Hud,
    History,
    Statistics,
}

/// One isolated, deterministic window for screenshots.
#[derive(Clone)]
pub struct AppWindowPreview {
    pub pane: PreviewPane,
    pub onboarding: bool,
    pub permissions_missing: bool,
    pub open_history_retention: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Pane {
    #[default]
    Settings,
    Models,
    Hud,
    History,
    Statistics,
}

impl Pane {
    const ALL: [Self; 5] = [
        Self::Settings,
        Self::Models,
        Self::Hud,
        Self::History,
        Self::Statistics,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::Models => "Models",
            Self::Hud => "HUD",
            Self::History => "History",
            Self::Statistics => "Statistics",
        }
    }

    fn icon(self) -> NavigationIcon {
        match self {
            Self::Settings => NavigationIcon::Settings,
            Self::Models => NavigationIcon::Models,
            Self::Hud => NavigationIcon::Hud,
            Self::History => NavigationIcon::History,
            Self::Statistics => NavigationIcon::Statistics,
        }
    }

    fn on_reopen(self, status: SetupStatus) -> Self {
        if !crate::onboarding::permission_warnings(status).is_empty() {
            Self::Settings
        } else if !status.api_key {
            Self::Models
        } else {
            self
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HotkeyKind {
    Dictation,
    PasteLast,
}

enum HotkeyCaptureState {
    Idle,
    Listening {
        kind: HotkeyKind,
        modifiers: HotkeyModifiers,
        message: Option<&'static str>,
        started_at: Instant,
    },
    Saved {
        kind: HotkeyKind,
        saved_at: Instant,
    },
}

impl HotkeyCaptureState {
    fn is_listening(&self) -> bool {
        matches!(self, Self::Listening { .. })
    }
}

/// A critically damped spring for toggles and sliding selectors.
struct ToggleSpring {
    position: f32,
    velocity: f32,
    target: f32,
    last_frame: Instant,
}

impl ToggleSpring {
    fn new(enabled: bool) -> Self {
        Self::at(if enabled { 1.0 } else { 0.0 })
    }

    fn at(position: f32) -> Self {
        Self {
            position,
            velocity: 0.0,
            target: position,
            last_frame: Instant::now(),
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.set_target(if enabled { 1.0 } else { 0.0 });
    }

    fn set_target(&mut self, target: f32) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.last_frame = Instant::now();
    }

    fn advance(&mut self, elapsed: Duration) {
        /// Critically damped spring stiffness in rad/s; higher settles faster.
        const STIFFNESS: f32 = 40.0;
        let mut remaining = elapsed.as_secs_f32().min(0.1);
        while remaining > 0.0 {
            let dt = remaining.min(1.0 / 240.0);
            let acceleration = -STIFFNESS.powi(2) * (self.position - self.target)
                - 2.0 * STIFFNESS * self.velocity;
            self.velocity += acceleration * dt;
            self.position += self.velocity * dt;
            remaining -= dt;
        }
        if self.is_settled() {
            self.position = self.target;
            self.velocity = 0.0;
        }
    }

    fn render_position(&mut self, window: &mut Window) -> f32 {
        let now = Instant::now();
        self.advance(now.duration_since(self.last_frame));
        self.last_frame = now;
        if !self.is_settled() {
            window.request_animation_frame();
        }
        self.position
    }

    fn is_settled(&self) -> bool {
        (self.position - self.target).abs() < 0.001 && self.velocity.abs() < 0.01
    }

    fn enabled(&self) -> bool {
        self.target >= 0.5
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingControl {
    Dictation,
    PasteLast,
    DictationMode,
    EnterSubmit,
    ClipboardFallback,
    DoubleTapOnly,
    Microphone,
    Channel,
    MicrophoneMode,
    AudioBehavior,
    LowerVolume,
    Dock,
    Sound,
    Retention,
    Trim,
    Hud,
    MicrophonePriority,
    DoubleTapSensitivity,
}

struct SettingsFeedback {
    control: SettingControl,
    success: bool,
    message: String,
}

fn hotkey_feedback_scope(kind: HotkeyKind) -> SettingControl {
    match kind {
        HotkeyKind::Dictation => SettingControl::Dictation,
        HotkeyKind::PasteLast => SettingControl::PasteLast,
    }
}

pub struct AppWindow {
    preview: bool,
    pane: Pane,
    listener_start: Option<SyncSender<()>>,
    setup_status: SetupStatus,
    setup_visible: bool,
    onboarding_completed: bool,
    permission_refresh_at: Instant,
    settings: AppSettings,
    settings_error: Option<String>,
    settings_feedback: Option<SettingsFeedback>,
    hud_settings: Entity<HudSettingsView>,
    sound_settings: Entity<SoundSettingsView>,
    microphone_priority: Entity<MicrophonePriorityView>,
    sensitivity_focus: [FocusHandle; 3],
    preference_transfer_busy: bool,
    preference_transfer_error: Option<String>,
    preference_transfer_focus: [FocusHandle; 2],
    microphone_devices: Vec<String>,
    microphone_picker_open: bool,
    microphone_picker_state: PickerState,
    microphone_picker_error: Option<String>,
    microphone_channel_picker_open: bool,
    microphone_channel_picker_state: PickerState,
    microphone_description: Option<crate::microphone::InputDescription>,
    microphone_description_error: Option<String>,
    microphone_refresh_at: Instant,
    microphone_diagnostic: Option<crate::microphone::RecordingDiagnostic>,
    launch_at_login_status: Option<LoginItemStatus>,
    login_item_worker: Option<LoginItemWorker>,
    launch_at_login_error: Option<String>,
    launch_at_login_toggle: ToggleSpring,
    release_microphone_toggle: ToggleSpring,
    dictation_mode_focus: [FocusHandle; 3],
    enter_submit_focus: FocusHandle,
    double_tap_only_visibility: ToggleSpring,
    dock_icon_toggle: ToggleSpring,
    recording_audio_spring: ToggleSpring,
    lower_volume_input: Entity<TextInput>,
    openrouter_settings: Entity<OpenRouterSettings>,
    openrouter_setup: Entity<OpenRouterSettings>,
    statistics: Entity<StatisticsView>,
    hotkey_capture: HotkeyCaptureState,
    hotkey_capture_animation: ToggleSpring,
    hotkey_width_spring: ToggleSpring,
    hotkey_side_animations: [ToggleSpring; 2],
    hotkey_side_selection_springs: [ToggleSpring; 2],
    window_focus: FocusHandle,
    hotkey_focus: FocusHandle,
    hotkey_reset_focus: [FocusHandle; 2],
    clipboard_fallback_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    history: Option<History>,
    history_search: Entity<TextInput>,
    history_entries: Vec<HistoryEntry>,
    recovery: Option<RecordingRecovery>,
    recovery_entries: Vec<RecoveryEntry>,
    selected_recovery: Option<String>,
    recovery_delete_armed: bool,
    recovery_copied: bool,
    recovery_error: Option<String>,
    recovery_action_focus: [FocusHandle; 3],
    selected_history: Option<u64>,
    history_error: Option<String>,
    history_clear_armed: bool,
    history_retention_open: bool,
    history_retention_picker_state: PickerState,
    history_copied: Option<u64>,
}

impl AppWindow {
    fn new(
        listener_start: Option<SyncSender<()>>,
        history: Option<History>,
        preview: Option<AppWindowPreview>,
        native_window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |window, cx| {
            loop {
                Timer::after(Duration::from_millis(500)).await;
                let updated = window.update(cx, |window, cx| {
                    let changed = window.poll_setup(false)
                        | window.poll_login_item()
                        | window.poll_history(cx)
                        | window.poll_microphone();
                    if changed {
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
        let preview_mode = preview.is_some();
        let (settings, settings_error) = if preview_mode {
            (AppSettings::default(), None)
        } else {
            match AppSettings::load() {
                Ok(settings) => (settings, None),
                Err(error) => {
                    tracing::error!(%error, "could not load app settings");
                    (
                        AppSettings::default(),
                        Some(format!("Could not load app settings: {error:#}")),
                    )
                }
            }
        };
        if !preview_mode {
            crate::app_settings::set_dock_icon_visible(true);
        }
        let window_focus = cx.focus_handle();
        window_focus.focus(native_window);
        let hotkey_focus = cx.focus_handle();
        let mut subscriptions = vec![
            cx.on_blur(&hotkey_focus, native_window, |this, _, cx| {
                this.cancel_hotkey_capture(cx)
            }),
            cx.observe_window_activation(native_window, |this, window, cx| {
                if window.is_window_active() {
                    this.permission_refresh_at = Instant::now();
                    if this.poll_setup(true) {
                        cx.notify();
                    }
                    if this.pane == Pane::Statistics {
                        this.statistics.update(cx, |view, cx| {
                            view.refresh();
                            cx.notify();
                        });
                    }
                }
            }),
        ];
        let lower_volume_input =
            cx.new(|cx| TextInput::new(cx, "80", settings.lower_volume_percent.to_string()));
        subscriptions.push(
            cx.subscribe(&lower_volume_input, |this, _, _: &TextSubmitted, cx| {
                this.save_lower_volume(cx);
            }),
        );
        subscriptions.push(
            cx.subscribe(&lower_volume_input, |this, _, _: &TextChanged, cx| {
                if this
                    .settings_feedback
                    .as_ref()
                    .is_some_and(|feedback| feedback.control == SettingControl::LowerVolume)
                {
                    this.settings_feedback = None;
                }
                cx.notify();
            }),
        );
        let history_search = cx.new(|cx| TextInput::picker(cx, "Search history", ""));
        subscriptions.push(
            cx.subscribe(&history_search, |this, _, _: &TextChanged, cx| {
                this.reload_history(cx);
                cx.notify();
            }),
        );
        let setup_status = match &preview {
            Some(preview) if preview.onboarding => SetupStatus {
                microphone: PermissionState::NeedsRequest,
                input_monitoring: PermissionState::NeedsRequest,
                accessibility: PermissionState::NeedsRequest,
                api_key: false,
            },
            Some(preview) if preview.permissions_missing => SetupStatus {
                microphone: PermissionState::NeedsRequest,
                input_monitoring: PermissionState::NeedsSettings,
                accessibility: PermissionState::NeedsSettings,
                api_key: true,
            },
            Some(_) => SetupStatus {
                microphone: PermissionState::Ready,
                input_monitoring: PermissionState::Ready,
                accessibility: PermissionState::Ready,
                api_key: true,
            },
            None => crate::onboarding::status(),
        };
        let onboarding_completed = preview_mode || crate::onboarding::completion_recorded();
        let setup_visible = preview.as_ref().is_some_and(|preview| preview.onboarding)
            || !onboarding_completed && !setup_status.ready();
        let microphone_devices = if preview_mode {
            vec!["Built-in Microphone".into()]
        } else {
            crate::audio::input_device_names().unwrap_or_else(|error| {
                tracing::warn!(%error, "could not list microphones for settings");
                Vec::new()
            })
        };
        let launch_at_login_status = preview_mode.then_some(LoginItemStatus::Disabled);
        let (login_item_worker, launch_at_login_error) = if preview_mode {
            (None, None)
        } else {
            match LoginItemWorker::new() {
                Ok(worker) => (Some(worker), None),
                Err(error) => (None, Some(error)),
            }
        };
        let history = if preview_mode {
            preview
                .as_ref()
                .is_some_and(|preview| preview.pane == PreviewPane::History)
                .then(preview_history)
                .flatten()
        } else {
            history
        };
        let side = |binding: Option<&HotkeyBinding>| binding.and_then(standalone_modifier_side);
        let dictation_side = side(Some(&settings.dictation_hotkey));
        let paste_side = side(settings.paste_last_hotkey.as_ref());
        let openrouter_settings = crate::openrouter::settings_view::new(preview_mode, cx);
        let openrouter_setup = crate::openrouter::settings_view::new_key_setup(preview_mode, cx);
        let hud_settings = cx.new(|cx| HudSettingsView::new(settings.hud, preview_mode, cx));
        let sound_settings =
            cx.new(|cx| SoundSettingsView::new(settings.effective_sound_volumes(), cx));
        let microphone_priority = cx.new(|cx| {
            MicrophonePriorityView::new(settings.microphone_priority.clone(), preview_mode, cx)
        });
        subscriptions.push(cx.observe(&hud_settings, |_, _, cx| cx.notify()));
        subscriptions.push(cx.observe(&sound_settings, |_, _, cx| cx.notify()));
        subscriptions.push(cx.observe(&microphone_priority, |_, _, cx| cx.notify()));
        subscriptions.push(
            cx.subscribe(&hud_settings, |this, _, change: &HudChange, cx| {
                this.update_settings(SettingControl::Hud, cx, |settings| {
                    settings.hud = change.preferences.normalized()
                });
                let preferences = this.settings.hud;
                let error = this.feedback_error(SettingControl::Hud);
                this.hud_settings
                    .update(cx, |view, cx| view.set_preferences(preferences, error, cx));
            }),
        );
        subscriptions.push(cx.subscribe(
            &sound_settings,
            |this, _, change: &SoundVolumeChange, cx| {
                let mut volumes = this.settings.effective_sound_volumes();
                match change.event {
                    SoundEvent::Start => volumes.start = change.volume,
                    SoundEvent::Stop => volumes.stop = change.volume,
                    SoundEvent::ErrorCancel => volumes.error_cancel = change.volume,
                }
                let saved = this.update_settings(SettingControl::Sound, cx, |settings| {
                    settings.sound_volumes = Some(volumes.normalized());
                    settings.sound_effects =
                        volumes.start > 0.0 || volumes.stop > 0.0 || volumes.error_cancel > 0.0;
                });
                let preferences = this.settings.effective_sound_volumes();
                let error = this
                    .feedback_error(SettingControl::Sound)
                    .map(|message| (change.event, message));
                this.sound_settings
                    .update(cx, |view, cx| view.set_preferences(preferences, error, cx));
                if saved && !this.preview && change.volume > 0.0 {
                    crate::feedback::play(match change.event {
                        SoundEvent::Start => crate::feedback::Tone::DictationStart,
                        SoundEvent::Stop => crate::feedback::Tone::DictationStop,
                        SoundEvent::ErrorCancel => crate::feedback::Tone::Cancel,
                    });
                }
            },
        ));
        subscriptions.push(cx.subscribe(
            &microphone_priority,
            |this, _, change: &PriorityChange, cx| {
                this.update_settings(SettingControl::MicrophonePriority, cx, |settings| {
                    settings.microphone_priority = change.0.clone()
                });
                let preferences = this.settings.microphone_priority.clone();
                let error = this.feedback_error(SettingControl::MicrophonePriority);
                this.microphone_priority
                    .update(cx, |view, cx| view.set_preferences(preferences, error, cx));
                this.refresh_microphone_description();
            },
        ));
        let (microphone_description, microphone_description_error) = if preview_mode {
            (
                Some(crate::microphone::InputDescription::for_preview(
                    settings.microphone_channel.as_ref(),
                )),
                None,
            )
        } else {
            match crate::audio::input_description(
                settings.microphone.as_deref(),
                &settings.microphone_priority,
            ) {
                Ok(description) => (Some(description), None),
                Err(error) => (None, Some(error.to_string())),
            }
        };
        subscriptions.push(cx.observe(&openrouter_settings, |_, _, cx| cx.notify()));
        subscriptions.push(
            cx.subscribe(&openrouter_setup, |this, _, event: &KeyChanged, cx| {
                this.openrouter_settings.update(cx, |view, cx| {
                    view.sync_key_status(event.0.clone(), cx);
                });
            }),
        );
        subscriptions.push(cx.subscribe(
            &openrouter_settings,
            |this, _, event: &KeyChanged, cx| {
                this.openrouter_setup.update(cx, |view, cx| {
                    view.sync_key_status(event.0.clone(), cx);
                });
            },
        ));
        let mut window = Self {
            preview: preview_mode,
            pane: match preview.as_ref().map(|preview| preview.pane) {
                Some(PreviewPane::Models) => Pane::Models,
                Some(PreviewPane::Hud) => Pane::Hud,
                Some(PreviewPane::History) => Pane::History,
                Some(PreviewPane::Statistics) => Pane::Statistics,
                Some(PreviewPane::Settings) | None => Pane::Settings,
            },
            listener_start,
            setup_status,
            setup_visible,
            onboarding_completed,
            permission_refresh_at: Instant::now() + PERMISSION_REFRESH_INTERVAL,
            microphone_devices,
            microphone_picker_open: false,
            microphone_picker_state: PickerState::new(cx),
            microphone_picker_error: None,
            microphone_channel_picker_open: false,
            microphone_channel_picker_state: PickerState::new(cx),
            microphone_description,
            microphone_description_error,
            microphone_refresh_at: Instant::now() + Duration::from_secs(5),
            microphone_diagnostic: preview_mode
                .then(crate::microphone::RecordingDiagnostic::for_preview),
            launch_at_login_status,
            login_item_worker,
            launch_at_login_error,
            launch_at_login_toggle: ToggleSpring::new(
                launch_at_login_status == Some(LoginItemStatus::Enabled),
            ),
            release_microphone_toggle: ToggleSpring::new(settings.release_microphone_while_idle),
            dictation_mode_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            enter_submit_focus: cx.focus_handle().tab_stop(true),
            double_tap_only_visibility: ToggleSpring::new(
                settings.dictation_mode == DictationMode::DoubleTap
                    && settings.dictation_hotkey.key.is_some(),
            ),
            dock_icon_toggle: ToggleSpring::new(settings.show_dock_icon),
            recording_audio_spring: ToggleSpring::at(recording_audio_index(
                settings.recording_audio_behavior,
            ) as f32),
            lower_volume_input,
            openrouter_settings,
            openrouter_setup,
            statistics: cx.new(|_| StatisticsView::new(preview_mode)),
            hotkey_capture: HotkeyCaptureState::Idle,
            hotkey_capture_animation: ToggleSpring::new(false),
            hotkey_width_spring: ToggleSpring::at(HOTKEY_MIN_WIDTH),
            hotkey_side_animations: [
                ToggleSpring::new(dictation_side.is_some()),
                ToggleSpring::new(paste_side.is_some()),
            ],
            hotkey_side_selection_springs: [
                ToggleSpring::at(
                    hotkey_side_index(dictation_side.unwrap_or(ModifierSide::Either)) as f32,
                ),
                ToggleSpring::at(
                    hotkey_side_index(paste_side.unwrap_or(ModifierSide::Either)) as f32,
                ),
            ],
            window_focus,
            hotkey_focus,
            hotkey_reset_focus: std::array::from_fn(|_| cx.focus_handle()),
            clipboard_fallback_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
            settings,
            settings_error,
            settings_feedback: None,
            hud_settings,
            sound_settings,
            microphone_priority,
            sensitivity_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            preference_transfer_busy: false,
            preference_transfer_error: None,
            preference_transfer_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            history,
            history_search,
            history_entries: Vec::new(),
            recovery: if preview_mode {
                preview
                    .as_ref()
                    .is_some_and(|preview| preview.pane == PreviewPane::History)
                    .then(preview_recording_recovery)
                    .flatten()
            } else {
                cx.try_global::<RecordingRecovery>().cloned()
            },
            recovery_entries: Vec::new(),
            selected_recovery: None,
            recovery_delete_armed: false,
            recovery_copied: false,
            recovery_error: None,
            recovery_action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            selected_history: None,
            history_error: None,
            history_clear_armed: false,
            history_retention_open: preview
                .as_ref()
                .is_some_and(|preview| preview.open_history_retention),
            history_retention_picker_state: PickerState::new(cx),
            history_copied: None,
        };
        window.history_retention_picker_state.highlight = HistoryRetention::ALL
            .iter()
            .position(|value| *value == window.settings.history_retention)
            .unwrap_or(0);
        window.reload_history(cx);
        if window.preview {
            window.selected_recovery = window
                .recovery_entries
                .first()
                .map(|entry| entry.id.clone());
            if window.selected_recovery.is_none() {
                window.selected_history = window.history_entries.first().map(|entry| entry.id);
            }
        }
        window
    }

    /// Re-reads disk-backed state. Also called when an open window is reopened.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reload_history(cx);
        self.statistics.update(cx, |view, cx| {
            view.refresh();
            cx.notify();
        });
        self.permission_refresh_at = Instant::now();
        self.poll_setup(true);
        let pane = self.pane.on_reopen(self.setup_status);
        if pane != self.pane {
            self.select_pane(pane, cx);
        }
        cx.notify();
    }

    fn select_pane(&mut self, pane: Pane, cx: &mut Context<Self>) {
        self.cancel_hotkey_capture(cx);
        self.openrouter_settings
            .update(cx, |view, cx| view.close_pickers(cx));
        self.microphone_priority
            .update(cx, |view, cx| view.close_picker(cx));
        self.hud_settings
            .update(cx, |view, cx| view.close_picker(cx));
        self.pane = pane;
        self.history_retention_open = false;
        self.microphone_picker_open = false;
        self.microphone_channel_picker_open = false;
        match pane {
            Pane::History => self.reload_history(cx),
            Pane::Statistics => self.statistics.update(cx, |view, cx| {
                view.refresh();
                cx.notify();
            }),
            Pane::Settings | Pane::Models | Pane::Hud => {
                self.permission_refresh_at = Instant::now()
            }
        }
        cx.notify();
    }

    pub(crate) fn focus_pane(&self, window: &mut Window) {
        self.window_focus.focus(window);
    }

    pub(crate) fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Settings, cx);
    }

    pub(crate) fn show_models(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Models, cx);
    }

    pub(crate) fn show_hud(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Hud, cx);
    }

    pub(crate) fn show_history(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::History, cx);
    }

    pub(crate) fn show_statistics(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Statistics, cx);
    }

    fn poll_setup(&mut self, force: bool) -> bool {
        if self.preview
            || !force
                && (Instant::now() < self.permission_refresh_at
                    || !self.setup_visible
                        && self.listener_start.is_none()
                        && !matches!(self.pane, Pane::Settings | Pane::Models)
                        && self.setup_status.api_key)
        {
            return false;
        }
        self.permission_refresh_at = Instant::now() + PERMISSION_REFRESH_INTERVAL;
        let mut changed = false;
        let status = crate::onboarding::status();
        if status != self.setup_status {
            self.setup_status = status;
            changed = true;
        }
        if self.setup_status.ready() {
            if !self.onboarding_completed {
                if let Err(error) = crate::onboarding::record_completion() {
                    tracing::warn!(%error, "could not record onboarding completion");
                }
                self.onboarding_completed = true;
            }
            if let Some(start) = self.listener_start.take() {
                let _ = start.try_send(());
            }
            if self.setup_visible {
                self.setup_visible = false;
                changed = true;
            }
        }
        changed
    }

    fn poll_login_item(&mut self) -> bool {
        let Some(worker) = &mut self.login_item_worker else {
            return false;
        };
        let response = worker.poll();
        let changed = match response {
            Ok(Some(response)) => self.apply_login_item_response(response),
            Ok(None) => false,
            Err(error) => {
                self.login_item_worker_failed(error);
                return true;
            }
        };
        let request_changed = self.request_login_item(LoginItemRequest::Status);
        changed || request_changed
    }

    fn apply_login_item_response(&mut self, response: LoginItemResponse) -> bool {
        let before = (
            self.launch_at_login_status,
            self.launch_at_login_toggle.enabled(),
        );
        let error_changed = match response.result {
            Ok(status) => {
                let clear_error = self.launch_at_login_status.is_none()
                    || matches!(response.request, LoginItemRequest::SetEnabled(_))
                    || self.launch_at_login_status != Some(status)
                        && status == LoginItemStatus::Enabled;
                self.launch_at_login_status = Some(status);
                clear_error && self.launch_at_login_error.take().is_some()
            }
            Err(error) => {
                if let Some(status) = error.status {
                    self.launch_at_login_status = Some(status);
                }
                let changed = self.launch_at_login_error.as_deref() != Some(error.message.as_str());
                self.launch_at_login_error = Some(error.message);
                changed
            }
        };
        let enabled = self
            .login_item_worker
            .as_ref()
            .and_then(LoginItemWorker::desired_enabled)
            .unwrap_or(self.launch_at_login_status == Some(LoginItemStatus::Enabled));
        self.launch_at_login_toggle.set_enabled(enabled);
        error_changed
            || before
                != (
                    self.launch_at_login_status,
                    self.launch_at_login_toggle.enabled(),
                )
    }

    fn request_login_item(&mut self, request: LoginItemRequest) -> bool {
        let Some(worker) = &mut self.login_item_worker else {
            return false;
        };
        if let Err(error) = worker.request(request) {
            self.login_item_worker_failed(error);
            return true;
        }
        false
    }

    fn login_item_worker_failed(&mut self, error: String) {
        self.login_item_worker = None;
        self.launch_at_login_error = Some(error);
        self.launch_at_login_toggle
            .set_enabled(self.launch_at_login_status == Some(LoginItemStatus::Enabled));
    }

    fn set_launch_at_login(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.preview {
            self.launch_at_login_status = Some(if enabled {
                LoginItemStatus::Enabled
            } else {
                LoginItemStatus::Disabled
            });
        } else if self.launch_at_login_status.is_none() || self.login_item_worker.is_none() {
            return;
        }
        self.launch_at_login_toggle.set_enabled(enabled);
        self.request_login_item(LoginItemRequest::SetEnabled(enabled));
        cx.notify();
    }

    // ---- Settings persistence ------------------------------------------

    /// Saves before assigning so a failed save leaves the live settings
    /// untouched. Returns whether the change was saved.
    fn update_settings(
        &mut self,
        control: SettingControl,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut AppSettings),
    ) -> bool {
        let mut candidate = self.settings.clone();
        update(&mut candidate);
        let preview = self.preview;
        let result =
            self.settings.commit_with(
                candidate,
                |candidate| {
                    if preview { Ok(()) } else { candidate.save() }
                },
            );
        self.settings_feedback = Some(SettingsFeedback {
            control,
            success: result.is_ok(),
            message: result
                .as_ref()
                .err()
                .map(|error| format!("Could not save settings: {error:#}"))
                .unwrap_or_else(|| "Saved.".into()),
        });
        if result.is_ok() {
            self.settings_error = None;
        }
        cx.notify();
        result.is_ok()
    }

    fn save_lower_volume(&mut self, cx: &mut Context<Self>) {
        let percent = self
            .lower_volume_input
            .read(cx)
            .text()
            .trim()
            .parse::<u8>()
            .ok();
        let Some(percent) = percent.filter(|percent| *percent <= 100) else {
            self.settings_feedback = Some(SettingsFeedback {
                control: SettingControl::LowerVolume,
                success: false,
                message: "Enter a percentage from 0 to 100.".into(),
            });
            cx.notify();
            return;
        };
        self.update_settings(SettingControl::LowerVolume, cx, |settings| {
            settings.lower_volume_percent = percent;
        });
    }

    fn render_lower_volume_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        (self.settings.recording_audio_behavior == RecordingAudioBehavior::LowerVolume).then(|| {
            self.setting_row(
                SettingControl::LowerVolume,
                "Volume while dictating",
                "Percentage of the previous volume to keep. Applies to the next dictation; the original level returns when you stop.",
                div().flex().items_center().gap_2()
                    .child(div().w(px(64.0)).child(self.lower_volume_input.clone()))
                    .child(div().text_color(rgb(MUTED)).child("%"))
                    .child(compact_button("Save").id("save-lower-volume")
                        .on_click(cx.listener(|this, _, _, cx| this.save_lower_volume(cx)))),
            ).border_b_0().into_any_element()
        })
    }

    fn feedback_error(&self, control: SettingControl) -> Option<String> {
        self.settings_feedback
            .as_ref()
            .filter(|feedback| feedback.control == control && !feedback.success)
            .map(|feedback| feedback.message.clone())
    }

    fn setting_feedback(&self, control: SettingControl) -> Option<AnyElement> {
        let feedback = self
            .settings_feedback
            .as_ref()
            .filter(|feedback| feedback.control == control)?;
        if feedback.success {
            return None;
        }
        Some(
            div()
                .text_size(px(11.0))
                .text_color(rgb(NEGATIVE))
                .child(feedback.message.clone())
                .into_any_element(),
        )
    }

    fn setting_row(
        &self,
        control: SettingControl,
        title: &'static str,
        description: impl Into<SharedString>,
        content: impl IntoElement,
    ) -> gpui::Div {
        let popup = match control {
            SettingControl::Microphone => self.microphone_picker_open,
            SettingControl::Channel => self.microphone_channel_picker_open,
            SettingControl::Retention => self.history_retention_open,
            _ => false,
        };
        div()
            .border_b_1()
            .border_color(rgb(LINE))
            .child(settings_row(title, description, content).border_b_0())
            .when(!popup, |row| {
                row.children(
                    self.setting_feedback(control)
                        .map(|message| div().px_4().pb_3().child(message)),
                )
            })
    }

    fn set_dictation_mode(&mut self, mode: DictationMode, cx: &mut Context<Self>) {
        self.update_settings(SettingControl::DictationMode, cx, |settings| {
            settings.dictation_mode = mode;
            settings.double_tap_lock = mode == DictationMode::DoubleTap;
            if !settings.double_tap_lock {
                settings.double_tap_only = false;
            }
        });
    }

    fn set_double_tap_only(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.update_settings(SettingControl::DoubleTapOnly, cx, |settings| {
            settings.double_tap_only = enabled
                && settings.dictation_mode == DictationMode::DoubleTap
                && settings.dictation_hotkey.key.is_some();
        });
    }

    // ---- History -----------------------------------------------------------

    fn reload_history(&mut self, cx: &App) {
        let query = self.history_search.read(cx).text().to_string();
        self.recovery_entries = self
            .recovery
            .as_ref()
            .map(|store| store.entries(&query))
            .unwrap_or_default();
        if self
            .selected_recovery
            .as_ref()
            .is_some_and(|id| !self.recovery_entries.iter().any(|entry| &entry.id == id))
        {
            self.selected_recovery = None;
            self.recovery_delete_armed = false;
            self.recovery_copied = false;
            self.recovery_error = None;
        }
        let Some(history) = &self.history else {
            self.history_entries.clear();
            self.selected_history = None;
            return;
        };
        let query = self.history_search.read(cx).text().to_string();
        self.history_entries = history.search(&query);
        if self
            .selected_history
            .is_some_and(|id| !self.history_entries.iter().any(|entry| entry.id == id))
        {
            self.selected_history = None;
        }
    }

    /// Keeps the visible list current while the pane is open.
    fn poll_history(&mut self, cx: &App) -> bool {
        if self.preview && self.recovery.is_none()
            || self.pane != Pane::History
            || self.history.is_none() && self.recovery.is_none()
        {
            return false;
        }
        let previous = std::mem::take(&mut self.history_entries);
        let previous_recovery = self.recovery_entries.clone();
        self.reload_history(cx);
        self.history_entries != previous || self.recovery_entries != previous_recovery
    }

    fn set_history_retention(
        &mut self,
        retention: HistoryRetention,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.settings.history_retention == retention {
            return true;
        }
        if !self.update_settings(SettingControl::Retention, cx, |settings| {
            settings.history_retention = retention
        }) {
            return false;
        }
        if let Some(history) = &self.history
            && let Err(error) = history.set_retention(retention)
        {
            self.history_error = Some(error.to_string());
        }
        self.reload_history(cx);
        cx.notify();
        true
    }

    fn toggle_retention_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.history_retention_open = !self.history_retention_open;
        if self.history_retention_open {
            let selected = HistoryRetention::ALL
                .iter()
                .position(|value| *value == self.settings.history_retention)
                .unwrap_or(0);
            self.history_retention_picker_state
                .open(selected, HistoryRetention::ALL.len(), window);
        } else {
            self.history_retention_picker_state.trigger.focus(window);
        }
        cx.notify();
    }

    fn choose_retention(
        &mut self,
        choice: HistoryRetention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.set_history_retention(choice, cx) {
            self.history_retention_open = false;
            self.history_retention_picker_state.trigger.focus(window);
        }
        cx.notify();
    }

    fn retention_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if self
            .history_retention_picker_state
            .navigate(key, HistoryRetention::ALL.len())
        {
        } else if matches!(key, "enter" | "space") {
            if let Some(choice) =
                HistoryRetention::ALL.get(self.history_retention_picker_state.highlight)
            {
                self.choose_retention(*choice, window, cx);
            }
        } else if matches!(key, "escape" | "tab") {
            self.history_retention_open = false;
            self.history_retention_picker_state.close(event, window);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn copy_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(entry) = self.history_entries.iter().find(|entry| entry.id == id) else {
            return;
        };
        match arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(entry.text.clone()))
        {
            Ok(()) => {
                self.history_copied = Some(id);
                self.history_error = None;
            }
            Err(error) => self.history_error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn delete_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(history) = &self.history {
            if let Err(error) = history.delete(id) {
                self.history_error = Some(error.to_string());
            }
            self.reload_history(cx);
        }
        cx.notify();
    }

    fn clear_history(&mut self, cx: &mut Context<Self>) {
        if !self.history_clear_armed {
            self.history_clear_armed = true;
            cx.notify();
            return;
        }
        self.history_clear_armed = false;
        if let Some(history) = &self.history {
            if let Err(error) = history.clear() {
                self.history_error = Some(error.to_string());
            }
            self.reload_history(cx);
        }
        cx.notify();
    }

    fn retry_recovery(&mut self, id: &str, cx: &mut Context<Self>) {
        self.recovery_copied = false;
        if let Some(store) = &self.recovery {
            self.recovery_error = (if self.preview {
                store.retry_with(id, |_| {
                    Ok(crate::openrouter::transcribe::Transcription {
                        text: "Recovered preview dictation.".into(),
                        report: None,
                    })
                })
            } else {
                store.retry(id)
            })
            .err()
            .map(|error| error.to_string());
            self.recovery_delete_armed = false;
            self.reload_history(cx);
            cx.notify();
        }
    }

    fn render_recovery_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.recovery_entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let id = entry.id.clone();
                let selected = self.selected_recovery.as_ref() == Some(&id);
                div()
                    .id(("recovery-entry", index))
                    .w_full()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(if entry.status == RecoveryStatus::Recovered {
                                TEXT_SOFT
                            } else {
                                NEGATIVE
                            }))
                            .child(entry.title()),
                    )
                    .child(
                        div()
                            .mt_1()
                            .w_full()
                            .truncate()
                            .text_size(px(10.0))
                            .text_color(rgb(FAINT))
                            .child(format!(
                                "{} · {} audio · {}",
                                entry.application_label(),
                                seconds_label(entry.audio_ms),
                                event_age(entry.timestamp_ms)
                            )),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_recovery = Some(id.clone());
                        this.selected_history = None;
                        this.recovery_delete_armed = false;
                        this.recovery_copied = false;
                        this.recovery_error = None;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect()
    }

    fn render_recovery_detail(&self, entry: &RecoveryEntry, cx: &mut Context<Self>) -> AnyElement {
        let id = entry.id.clone();
        let delete_id = id.clone();
        let recovered = entry.status == RecoveryStatus::Recovered;
        let retry_busy = self
            .recovery
            .as_ref()
            .is_some_and(RecordingRecovery::retry_in_progress);
        let can_retry = !entry.busy && !retry_busy && !recovered;
        let mut actions = div().flex().flex_wrap().gap_2();
        if let Some(text) = entry.text.clone() {
            actions = actions.child(
                header_button(if self.recovery_copied {
                    "Copied"
                } else {
                    "Copy text"
                })
                .id("recovery-copy")
                .track_focus(&self.recovery_action_focus[0])
                .focus(|style| style.border_color(rgb(ACCENT)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                    this.recovery_copied = true;
                    this.recovery_error = None;
                    cx.notify();
                })),
            );
        }
        if !recovered {
            actions = actions.child(
                header_button(if entry.busy { "Transcribing" } else { "Retry" })
                    .id("recovery-retry")
                    .track_focus(&self.recovery_action_focus[1].clone().tab_stop(can_retry))
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .when(!can_retry, |button| button.opacity(0.45))
                    .when(can_retry, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.retry_recovery(&id, cx);
                        }))
                    }),
            );
        }
        actions = actions.child(
            header_button(if self.recovery_delete_armed {
                "Delete permanently?"
            } else {
                "Delete"
            })
            .id("recovery-delete")
            .track_focus(&self.recovery_action_focus[2].clone().tab_stop(!entry.busy))
            .focus(|style| style.border_color(rgb(ACCENT)))
            .when(entry.busy, |button| button.opacity(0.45))
            .when(!entry.busy, |button| {
                button.on_click(cx.listener(move |this, _, _, cx| {
                    if !this.recovery_delete_armed {
                        this.recovery_delete_armed = true;
                        cx.notify();
                        return;
                    }
                    if let Some(store) = &this.recovery {
                        this.recovery_error = store
                            .delete(&delete_id)
                            .err()
                            .map(|error| error.to_string());
                        this.recovery_delete_armed = false;
                        this.reload_history(cx);
                        cx.notify();
                    }
                }))
            }),
        );
        div().id("recovery-detail").flex_1().min_w_0().h_full().overflow_y_scroll().px_6().py_6()
            .child(div().text_size(px(18.0)).font_weight(FontWeight::SEMIBOLD).child(entry.title()))
            .child(div().mt_2().text_size(px(11.0)).text_color(rgb(MUTED))
                .child(format!("{} audio · {}", seconds_label(entry.audio_ms), event_age(entry.timestamp_ms))))
            .child(div().mt_3().child(detail_row("Application", entry.application_label())))
            .child(div().mt_4().child(actions))
            .children(self.recovery_error.clone().map(|error| div().mt_3()
                .text_size(px(12.0)).text_color(rgb(NEGATIVE)).child(error)))
            .children(entry.message.clone().map(|message| div().mt_5()
                .child(section_label("Failure reason"))
                .child(div().pt_2().text_size(px(12.0)).line_height(px(18.0)).text_color(rgb(NEGATIVE)).child(message))))
            .child(div().mt_5().pt_4().border_t_1().border_color(rgb(LINE)).text_size(px(12.0)).text_color(rgb(TEXT_SOFT))
                .child(if entry.volatile { "This recording is only in memory. Keep Hex open until it is recovered." }
                    else if recovered { "Recovered text is saved locally until you delete it. The temporary audio has been removed." }
                    else { "Audio is saved on this Mac until recovery or deletion. Retry uses your current Models settings and saves the text here, without automatic paste." }))
            .children(entry.text.clone().map(|text| div().mt_5().text_size(px(13.0)).line_height(px(20.0)).child(text)))
            .into_any_element()
    }

    fn render_history(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let retention = self.settings.history_retention;
        let search = div().w(px(220.0)).child(self.history_search.clone());
        let retention_control = header_button(format!("Keep: {}", retention.label()))
            .id("history-retention")
            .track_focus(&self.history_retention_picker_state.trigger)
            .focus(|style| style.border_color(rgb(ACCENT)))
            .on_click(cx.listener(|this, event, window, cx| {
                if matches!(event, gpui::ClickEvent::Mouse(_)) {
                    this.toggle_retention_picker(window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event, window, cx| {
                if picker_open_key(event) {
                    this.toggle_retention_picker(window, cx);
                    cx.stop_propagation();
                }
            }));
        let retention_control = div().relative().child(retention_control).when(
            self.history_retention_open,
            |control| {
                control.child(picker_popup(selection_picker_menu(
                    "history-retention-menu",
                    &self.history_retention_picker_state,
                    HistoryRetention::ALL.to_vec(),
                    retention,
                    |choice| choice.label().to_owned(),
                    self.feedback_error(SettingControl::Retention),
                    (
                        cx.listener(|this, choice: &HistoryRetention, window, cx| {
                            this.choose_retention(*choice, window, cx);
                        }),
                        cx.listener(|this, _, _, cx| {
                            this.history_retention_open = false;
                            cx.notify();
                        }),
                        cx.listener(Self::retention_picker_key),
                    ),
                )))
            },
        );
        let retention_control = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(retention_control)
            .when(!self.history_retention_open, |control| {
                control.children(self.setting_feedback(SettingControl::Retention))
            });
        let clear = header_button(if self.history_clear_armed {
            "Clear text history?"
        } else {
            "Clear dictations"
        })
        .id("history-clear")
        .when(self.history_clear_armed, |button| {
            button.text_color(rgb(NEGATIVE))
        })
        .on_click(cx.listener(|this, _, _, cx| this.clear_history(cx)));
        let header_action = div()
            .flex()
            .items_center()
            .gap_3()
            .child(search)
            .child(retention_control)
            .child(clear)
            .into_any_element();
        let mut rows = self.render_recovery_rows(cx);
        rows.extend(
            self.history_entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let id = entry.id;
                    let selected = self.selected_history == Some(id);
                    let mut meta = entry.application.clone().unwrap_or_default();
                    if let Some(model) = entry
                        .transcription
                        .as_ref()
                        .and_then(|report| report.model.as_deref())
                    {
                        if !meta.is_empty() {
                            meta.push_str(" · ");
                        }
                        meta.push_str(model.rsplit('/').next().unwrap_or(model));
                    }
                    if entry
                        .transcription
                        .as_ref()
                        .is_some_and(|report| !report.failed.is_empty())
                    {
                        meta.push_str(" · fallback");
                    }
                    div()
                        .id(("history-entry", index))
                        .w_full()
                        .px_4()
                        .py_3()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_4()
                        .border_b_1()
                        .border_color(rgb(LINE))
                        .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                        .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .w_full()
                                        .text_size(px(12.0))
                                        .text_color(rgb(TEXT_SOFT))
                                        .line_height(px(18.0))
                                        .truncate()
                                        .child(entry.text.replace('\n', " ")),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(FAINT))
                                        .truncate()
                                        .child(meta),
                                ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(rgb(FAINT))
                                .child(event_age(entry.timestamp_ms)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected_history = Some(id);
                            this.selected_recovery = None;
                            this.recovery_delete_armed = false;
                            this.recovery_error = None;
                            this.history_clear_armed = false;
                            cx.notify();
                        }))
                        .into_any_element()
                }),
        );
        let retention_off = retention.is_off();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header_with_action("History", Some(header_action)))
            .children(self.recovery.as_ref().and_then(RecordingRecovery::load_warning).map(|warning|
                div().px_5().py_2().text_size(px(12.0)).text_color(rgb(NEGATIVE)).child(warning.to_owned())))
            .child(
                pane_body().p_5().child(
                    pane_content()
                        .flex_row()
                        .gap_5()
                        .child(
                            pane_list(
                                "history-list",
                                if retention_off && self.recovery_entries.is_empty() {
                                    Some("History is off. Failed recordings are still kept for recovery.")
                                } else if self.history_entries.is_empty() && self.recovery_entries.is_empty()
                                    && self.history_error.is_none()
                                {
                                    Some("No dictations retained yet.")
                                } else {
                                    None
                                },
                                "History could not be loaded.",
                                self.history_error.clone(),
                            )
                            .rounded(px(PANEL_RADIUS))
                            .border_1()
                            .border_color(rgb(LINE))
                            .bg(rgb(SURFACE))
                            .overflow_x_hidden()
                            .child(
                                div()
                                    .w(px(PANE_LIST_WIDTH - 2.0))
                                    .flex()
                                    .flex_col()
                                    .children(rows),
                            ),
                        )
                        .child(
                            compact_panel()
                                .flex_1()
                                .min_w(px(0.0))
                                .h_full()
                                .flex()
                                .flex_col()
                                .child(self.render_history_detail(cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_history_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(entry) = self
            .selected_recovery
            .as_ref()
            .and_then(|id| self.recovery_entries.iter().find(|entry| &entry.id == id))
        {
            return self.render_recovery_detail(entry, cx);
        }
        let Some(entry) = self
            .selected_history
            .and_then(|id| self.history_entries.iter().find(|entry| entry.id == id))
        else {
            return detail_placeholder("Select a dictation.");
        };
        let id = entry.id;
        let copied = self.history_copied == Some(id);
        let action_button = |label: &'static str, id_suffix: &'static str| {
            div()
                .id(SharedString::from(format!("history-action-{id_suffix}")))
                .h(px(30.0))
                .px_3()
                .flex()
                .items_center()
                .rounded_sm()
                .bg(rgb(SURFACE))
                .text_size(px(12.0))
                .text_color(rgb(TEXT_SOFT))
                .hover(|button| button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT)))
                .child(label)
        };
        let mut timing = format!("{} audio", seconds_label(entry.audio_ms));
        if entry.total_ms > 0 {
            timing.push_str(&format!(" · {} ms from release to paste", entry.total_ms));
        }
        div()
            .id("history-detail")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_y_scroll()
            .px_6()
            .py_6()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_size(px(18.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Dictation"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(rgb(FAINT))
                                            .child(event_age(entry.timestamp_ms)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        action_button(
                                            if copied { "Copied" } else { "Copy" },
                                            "copy",
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.copy_history_entry(id, cx)
                                            }),
                                        ),
                                    )
                                    .child(action_button("Delete", "delete").on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.delete_history_entry(id, cx)
                                        }),
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .mt_5()
                            .pt_5()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .child(section_label("Text"))
                            .child(
                                div()
                                    .pt_3()
                                    .text_size(px(13.0))
                                    .line_height(px(20.0))
                                    .text_color(rgb(TEXT))
                                    .child(entry.text.clone()),
                            ),
                    )
                    .child(
                        div()
                            .mt_5()
                            .pt_2()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .when_some(entry.application.clone(), |detail, application| {
                                detail.child(detail_row("Application", application))
                            })
                            .children(entry.transcription.iter().flat_map(|report| {
                                report
                                    .history_rows()
                                    .into_iter()
                                    .map(|(label, value)| detail_row(label, value))
                            }))
                            .child(detail_row("Timing", timing)),
                    ),
            )
            .into_any_element()
    }

    fn reconcile_recovery_focus(&self, window: &mut Window) {
        let selected = self
            .selected_recovery
            .as_ref()
            .and_then(|id| self.recovery_entries.iter().find(|entry| &entry.id == id));
        if self.recovery_action_focus[1].is_focused(window)
            && selected.is_none_or(|entry| entry.status == RecoveryStatus::Recovered)
        {
            if selected.is_some_and(|entry| entry.text.is_some()) {
                self.recovery_action_focus[0].focus(window);
            } else {
                self.window_focus.focus(window);
            }
        } else if selected.is_none()
            && self
                .recovery_action_focus
                .iter()
                .any(|focus| focus.is_focused(window))
        {
            self.window_focus.focus(window);
        }
    }

    // ---- Navigation --------------------------------------------------------

    fn render_navigation(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let items = Pane::ALL.into_iter().enumerate().map(|(index, pane)| {
            navigation_item(pane.icon(), self.pane == pane)
                .id(("app-nav", index))
                .child(pane.label())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_pane(pane, cx);
                    this.focus_pane(window);
                }))
        });
        sidebar_frame()
            .w(px(SIDEBAR_WIDTH))
            .px(px(14.0))
            .pt(px(52.0))
            .pb_4()
            .flex()
            .flex_col()
            .child(div().flex().flex_col().gap(px(2.0)).children(items))
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .pl_2()
                    .h(px(30.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child(format!("Hex {}", env!("CARGO_PKG_VERSION"))),
            )
            .into_any_element()
    }

    // ---- Shortcuts -----------------------------------------------------------

    fn render_hotkey_control(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let binding_keycaps = hotkey_binding(&self.settings, kind)
            .map_or_else(|| vec!["Off".into()], HotkeyBinding::keycaps);
        let idle_width = hotkey_idle_width(binding_keycaps.len());
        if matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Saved { saved_at, .. }
                if saved_at.elapsed() >= Duration::from_millis(700)
        ) {
            self.hotkey_capture = HotkeyCaptureState::Idle;
            self.hotkey_capture_animation.set_enabled(false);
            self.hotkey_width_spring.set_target(idle_width);
        } else if matches!(self.hotkey_capture, HotkeyCaptureState::Idle) {
            self.hotkey_width_spring.set_target(idle_width);
        }
        let intensity = self.hotkey_capture_animation.render_position(window);
        let animated_control_width = self.hotkey_width_spring.render_position(window);
        let this_capture = matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Listening { kind: active, .. }
                | HotkeyCaptureState::Saved { kind: active, .. }
                if active == kind
        );
        let control_width = if this_capture {
            animated_control_width
        } else {
            idle_width
        };
        let capture_active = !matches!(self.hotkey_capture, HotkeyCaptureState::Idle);
        let control_intensity = if this_capture { intensity } else { 0.0 };
        let capture_color = if matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Saved { kind: active, .. } if active == kind
        ) {
            rgb(0x1b2420)
        } else {
            rgb(0x251c1b)
        };
        let pulse = match &self.hotkey_capture {
            HotkeyCaptureState::Listening {
                kind: active,
                started_at,
                ..
            } if *active == kind => {
                window.request_animation_frame();
                (started_at.elapsed().as_secs_f32() * 4.5).sin() * 0.5 + 0.5
            }
            HotkeyCaptureState::Saved { kind: active, .. } if *active == kind => {
                window.request_animation_frame();
                1.0
            }
            HotkeyCaptureState::Idle
            | HotkeyCaptureState::Listening { .. }
            | HotkeyCaptureState::Saved { .. } => 0.0,
        };
        let content = match &self.hotkey_capture {
            HotkeyCaptureState::Listening {
                kind: active,
                modifiers,
                message,
                ..
            } if *active == kind => {
                let label = message.unwrap_or(if modifiers.is_empty() {
                    "Press shortcut"
                } else {
                    "Release to save or add key"
                });
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(5.0 + pulse * 3.0))
                            .ml_2()
                            .rounded_full()
                            .bg(rgb(NEGATIVE))
                            .opacity(0.6 + pulse * 0.4),
                    )
                    .when(!modifiers.is_empty() && message.is_none(), |content| {
                        content.child(hotkey_keycaps(
                            HotkeyBinding {
                                modifiers: *modifiers,
                                key: None,
                            }
                            .keycaps(),
                            0.85,
                        ))
                    })
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(TEXT_SOFT))
                            .child(label),
                    )
                    .child(
                        div()
                            .id(match kind {
                                HotkeyKind::Dictation => "cancel-dictation-hotkey-capture",
                                HotkeyKind::PasteLast => "cancel-paste-last-hotkey-capture",
                            })
                            .h(px(26.0))
                            .ml_1()
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded(px(4.0))
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .hover(|button| {
                                button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT_SOFT))
                            })
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.cancel_hotkey_capture(cx);
                            })),
                    )
                    .into_any_element()
            }
            HotkeyCaptureState::Saved { kind: active, .. } if *active == kind => div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(10.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_SOFT))
                        .child("✓"),
                )
                .child(hotkey_keycaps(binding_keycaps.clone(), 1.0))
                .into_any_element(),
            HotkeyCaptureState::Idle
            | HotkeyCaptureState::Listening { .. }
            | HotkeyCaptureState::Saved { .. } => div()
                .flex()
                .items_center()
                .gap_2()
                .child(hotkey_keycaps(binding_keycaps, 1.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(TEXT_SOFT))
                        .child("Change shortcut"),
                )
                .into_any_element(),
        };
        div()
            .id(match kind {
                HotkeyKind::Dictation => "dictation-hotkey-control",
                HotkeyKind::PasteLast => "paste-last-hotkey-control",
            })
            .track_focus(&self.hotkey_focus)
            .w(px(control_width))
            .min_w(px(HOTKEY_MIN_WIDTH))
            .h(px(32.0))
            .px(px(4.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(px(6.0))
            .border_1()
            .border_color(mix_color(
                rgb(LINE),
                rgb(TEXT_SOFT),
                control_intensity * 0.55,
            ))
            .bg(mix_color(
                rgb(CANVAS),
                capture_color,
                control_intensity * (0.55 + pulse * 0.15),
            ))
            .when(!capture_active, |control| {
                control.hover(|control| control.bg(rgb(SURFACE_HOVER)))
            })
            .child(content)
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.hotkey_capture.is_listening() && this_capture {
                    this.cancel_hotkey_capture(cx);
                } else {
                    this.begin_hotkey_capture(kind, window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.capture_hotkey_key(event, cx);
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                this.capture_hotkey_modifiers(event, cx);
            }))
            .into_any_element()
    }

    fn render_hotkey_setting_control(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hotkey = self.render_hotkey_control(kind, window, cx);
        let can_reset = hotkey_binding(&self.settings, kind) != Some(&default_hotkey_binding(kind));
        let side = hotkey_binding(&self.settings, kind).and_then(standalone_modifier_side);
        let side_animation = &mut self.hotkey_side_animations[hotkey_kind_index(kind)];
        side_animation.set_enabled(side.is_some());
        let side_position = side_animation.render_position(window).clamp(0.0, 1.0);
        let selected = side.unwrap_or(ModifierSide::Either);
        let side_widths = [34.0, 44.0, 34.0];
        let side_selection_spring =
            &mut self.hotkey_side_selection_springs[hotkey_kind_index(kind)];
        side_selection_spring.set_target(hotkey_side_index(selected) as f32);
        let selection_position = side_selection_spring.render_position(window);
        let side_selector = div()
            .w(px(HOTKEY_SIDE_SELECTOR_WIDTH * side_position))
            .mr(px(8.0 * side_position))
            .flex_none()
            .overflow_hidden()
            .opacity(side_position)
            .child(
                sliding_segmented_control(selection_position, &side_widths)
                    .w(px(HOTKEY_SIDE_SELECTOR_WIDTH))
                    .children(
                        [
                            ("Left", ModifierSide::Left),
                            ("Either", ModifierSide::Either),
                            ("Right", ModifierSide::Right),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, side))| {
                            let candidate = hotkey_side_binding(&self.settings, kind, side);
                            sliding_segmented_item(side_widths[index], selected == side)
                                .id(("hotkey-side", hotkey_kind_index(kind) * 3 + index))
                                .text_size(px(9.0))
                                .when(candidate.is_none(), |item| item.opacity(0.35))
                                .child(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let Some(candidate) =
                                        hotkey_side_binding(&this.settings, kind, side)
                                    else {
                                        return;
                                    };
                                    if this.update_settings(
                                        hotkey_feedback_scope(kind),
                                        cx,
                                        |settings| {
                                            set_hotkey_binding(settings, kind, candidate);
                                        },
                                    ) {
                                        this.hotkey_side_selection_springs[hotkey_kind_index(kind)]
                                            .set_target(index as f32);
                                    }
                                }))
                        }),
                    ),
            );
        div()
            .flex()
            .items_center()
            .child(side_selector)
            .child(hotkey)
            .child(
                compact_button("Reset")
                    .id(("reset-hotkey", hotkey_kind_index(kind)))
                    .debug_selector(move || format!("reset-hotkey-{}", hotkey_kind_index(kind)))
                    .track_focus(
                        &self.hotkey_reset_focus[hotkey_kind_index(kind)]
                            .clone()
                            .tab_stop(can_reset),
                    )
                    .flex_none()
                    .ml_2()
                    .h(px(32.0))
                    .border_1()
                    .border_color(rgb(LINE))
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .when(!can_reset, |button| button.opacity(0.35))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if can_reset {
                            this.reset_hotkey_binding(kind, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn reset_hotkey_binding(&mut self, kind: HotkeyKind, cx: &mut Context<Self>) -> bool {
        self.cancel_hotkey_capture(cx);
        let binding = default_hotkey_binding(kind);
        if hotkey_binding(&self.settings, kind) == Some(&binding) {
            return true;
        }
        if hotkey_binding_conflicts(&self.settings, kind, &binding) {
            let other = match kind {
                HotkeyKind::Dictation => "Paste last dictation",
                HotkeyKind::PasteLast => "Dictation",
            };
            self.settings_feedback = Some(SettingsFeedback {
                control: hotkey_feedback_scope(kind),
                success: false,
                message: format!(
                    "The default shortcut is used by {other}. Change that shortcut first."
                ),
            });
            cx.notify();
            return false;
        }
        self.update_settings(hotkey_feedback_scope(kind), cx, |settings| {
            set_hotkey_binding(settings, kind, binding);
        })
    }

    fn begin_hotkey_capture(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preview {
            crate::app_settings::set_hotkey_capture_active(true);
        }
        self.hotkey_width_spring.set_target(hotkey_capture_width(0));
        self.hotkey_capture = HotkeyCaptureState::Listening {
            kind,
            modifiers: HotkeyModifiers::default(),
            message: None,
            started_at: Instant::now(),
        };
        self.hotkey_capture_animation.set_enabled(true);
        self.hotkey_focus.focus(window);
        cx.notify();
    }

    fn cancel_hotkey_capture(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.hotkey_capture, HotkeyCaptureState::Idle) {
            if !self.preview {
                crate::app_settings::set_hotkey_capture_active(false);
            }
            self.hotkey_capture = HotkeyCaptureState::Idle;
            self.hotkey_capture_animation.set_enabled(false);
            self.hotkey_width_spring.set_target(HOTKEY_MIN_WIDTH);
            cx.notify();
        }
    }

    fn capture_hotkey_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.hotkey_capture.is_listening() {
            return;
        }
        cx.stop_propagation();
        if event.is_held {
            return;
        }
        if event.keystroke.key == "escape" {
            self.cancel_hotkey_capture(cx);
            return;
        }
        let modifiers = hotkey_modifiers(event.keystroke.modifiers).without_side_constraints();
        let key = match hotkey_key(&event.keystroke.key) {
            Ok(key) => key,
            Err(message) => {
                self.set_hotkey_capture_message(message, cx);
                return;
            }
        };
        if modifiers.is_empty() && !is_function_key(&key.label) {
            self.set_hotkey_capture_message("Add a modifier", cx);
            return;
        }
        let binding = HotkeyBinding {
            modifiers,
            key: Some(key),
        };
        self.hotkey_width_spring
            .set_target(hotkey_capture_width(binding.keycaps().len()));
        self.save_hotkey_binding(binding, cx);
    }

    fn capture_hotkey_modifiers(&mut self, event: &ModifiersChangedEvent, cx: &mut Context<Self>) {
        let current = hotkey_modifiers(event.modifiers);
        if !current.is_empty() {
            self.hotkey_width_spring.set_target(hotkey_capture_width(
                HotkeyBinding {
                    modifiers: current,
                    key: None,
                }
                .keycaps()
                .len(),
            ));
        }
        let released = {
            let HotkeyCaptureState::Listening {
                modifiers, message, ..
            } = &mut self.hotkey_capture
            else {
                return;
            };
            if !current.is_empty() {
                if current.count() >= modifiers.count() {
                    *modifiers = current;
                    *message = None;
                }
                cx.notify();
                None
            } else if message.is_some() {
                *modifiers = HotkeyModifiers::default();
                *message = None;
                cx.notify();
                None
            } else {
                (!modifiers.is_empty()).then_some(*modifiers)
            }
        };
        if let Some(modifiers) = released {
            let modifiers = if modifiers.count() == 1 {
                modifiers
            } else {
                modifiers.without_side_constraints()
            };
            self.save_hotkey_binding(
                HotkeyBinding {
                    modifiers,
                    key: None,
                },
                cx,
            );
        }
    }

    fn set_hotkey_capture_message(&mut self, next_message: &'static str, cx: &mut Context<Self>) {
        if let HotkeyCaptureState::Listening { message, .. } = &mut self.hotkey_capture {
            *message = Some(next_message);
            cx.notify();
        }
    }

    fn save_hotkey_binding(&mut self, binding: HotkeyBinding, cx: &mut Context<Self>) {
        if binding.is_empty() {
            self.set_hotkey_capture_message("Press a shortcut", cx);
            return;
        }
        let kind = match self.hotkey_capture {
            HotkeyCaptureState::Listening { kind, .. } => kind,
            HotkeyCaptureState::Idle | HotkeyCaptureState::Saved { .. } => return,
        };
        if hotkey_binding_conflicts(&self.settings, kind, &binding) {
            self.set_hotkey_capture_message("Already in use", cx);
            return;
        }
        let keycap_count = binding.keycaps().len();
        if !self.update_settings(hotkey_feedback_scope(kind), cx, |settings| {
            set_hotkey_binding(settings, kind, binding)
        }) {
            self.set_hotkey_capture_message("Could not save shortcut. Try again.", cx);
            return;
        }
        self.hotkey_width_spring
            .set_target(hotkey_saved_width(keycap_count));
        if !self.preview {
            crate::app_settings::set_hotkey_capture_active(false);
        }
        self.hotkey_capture = HotkeyCaptureState::Saved {
            kind,
            saved_at: Instant::now(),
        };
        cx.notify();
    }

    // ---- Settings --------------------------------------------------------------

    fn microphone_choices(&self) -> Vec<Option<String>> {
        std::iter::once(None)
            .chain(self.microphone_devices.iter().cloned().map(Some))
            .collect()
    }

    fn choose_microphone(&mut self, device: Option<String>, cx: &mut Context<Self>) -> bool {
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

    fn render_microphone_picker(&self, cx: &mut Context<Self>) -> AnyElement {
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

    fn microphone_picker_key(
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

    fn toggle_microphone_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn refresh_microphone_description(&mut self) -> bool {
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

    fn poll_microphone(&mut self) -> bool {
        if self.preview || self.pane != Pane::Settings {
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

    fn select_microphone_channel(
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

    fn toggle_microphone_channel_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn microphone_channel_picker_key(
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

    fn render_microphone_channel(&self, cx: &mut Context<Self>) -> AnyElement {
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

    fn render_microphone_diagnostic(&self) -> AnyElement {
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
                    div()
                        .flex()
                        .flex_col()
                        .items_end()
                        .text_size(px(11.0))
                        .text_color(rgb(TEXT_SOFT))
                        .child(format!("RMS {}", db(report.levels.rms_dbfs())))
                        .child(format!("Peak {}", db(report.levels.peak_dbfs()))),
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

    fn render_permission_warnings(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
    fn render_key_notice(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.setup_visible || self.setup_status.api_key || self.pane == Pane::Models {
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
                                    "Add your OpenRouter key to start dictating",
                                    "Hex transcribes through OpenRouter. The key is stored in your Keychain.",
                                )),
                        )
                        .child(
                            compact_button("Open Models")
                                .id("open-key-models")
                                .flex_none()
                                .bg(rgb(ACCENT))
                                .text_color(rgb(TEXT))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.select_pane(Pane::Models, cx);
                                    this.focus_pane(window);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn perform_permission_action(&mut self, warning: PermissionWarning) {
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

    fn toggle_trim_silence(&mut self, cx: &mut Context<Self>) {
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

    fn render_models(&self) -> AnyElement {
        configuration_pane(
            "Models",
            "models-scroll",
            div().child(self.openrouter_settings.clone()),
        )
    }

    fn export_preferences(&mut self, cx: &mut Context<Self>) {
        if self.preference_transfer_busy {
            return;
        }
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

    fn key_operation_pending(&self, cx: &App) -> bool {
        self.openrouter_settings.read(cx).has_key_operation()
            || self.openrouter_setup.read(cx).has_key_operation()
    }

    fn import_preferences(&mut self, cx: &mut Context<Self>) {
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

    fn accept_imported_preferences(
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
        self.openrouter_setup.update(cx, |view, cx| {
            view.apply_imported_config(imported.config.clone(), cx)
        });
        self.openrouter_settings.update(cx, |view, cx| {
            view.apply_imported_config(imported.config, cx)
        });
        self.refresh_microphone_description();
        // While the preferences window is open its Dock icon stays available;
        // the existing close/drop path applies the imported background setting.
        cx.notify();
    }

    fn render_preference_transfer(&self, cx: &mut Context<Self>) -> AnyElement {
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

    fn render_clipboard_fallback(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .child(settings_row(
                "Copy when auto-paste fails",
                "Keeps the dictation on the clipboard when Hex detects a paste error or the destination changes.",
                toggle(if self.settings.copy_on_paste_failure { 1.0 } else { 0.0 }),
            )
                .border_b_0()
                .id("clipboard-fallback-setting")
                .track_focus(&self.clipboard_fallback_focus)
                .focus(|style| style.bg(rgb(SURFACE_HOVER)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_settings(SettingControl::ClipboardFallback, cx, |settings| {
                        settings.copy_on_paste_failure = !settings.copy_on_paste_failure;
                    });
                })))
            .when_some(self.feedback_error(SettingControl::ClipboardFallback), |row, error| {
                row.child(div().px_4().pb_3().text_size(px(11.0)).text_color(rgb(NEGATIVE)).child(error))
            })
            .into_any_element()
    }

    fn render_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
        let hotkey_control = self.render_hotkey_setting_control(HotkeyKind::Dictation, window, cx);
        let paste_last_control =
            self.render_hotkey_setting_control(HotkeyKind::PasteLast, window, cx);
        let mode_control = div().flex_none().flex().gap_1().children(
            DictationMode::ALL
                .into_iter()
                .enumerate()
                .map(|(index, mode)| {
                    compact_button(mode.label())
                        .id(("dictation-mode", index))
                        .track_focus(&self.dictation_mode_focus[index])
                        .border_1()
                        .border_color(rgb(LINE))
                        .focus(|style| style.border_color(rgb(ACCENT)))
                        .when(mode == self.settings.dictation_mode, |button| {
                            button.bg(rgb(SURFACE_SELECTED)).text_color(rgb(TEXT))
                        })
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_dictation_mode(mode, cx)),
                        )
                }),
        );
        let mode_description = match self.settings.dictation_mode {
            DictationMode::TapOrHold => {
                "Tap to keep recording; press again to stop. Or hold and release to finish."
            }
            DictationMode::Hold => {
                "Hold the shortcut while speaking; release to transcribe and paste."
            }
            DictationMode::DoubleTap => {
                "Hold to dictate, or double-tap to keep recording until the next press."
            }
        };
        self.double_tap_only_visibility.set_enabled(
            self.settings.dictation_mode == DictationMode::DoubleTap
                && self.settings.dictation_hotkey.key.is_some(),
        );
        let double_tap_only_visibility = self
            .double_tap_only_visibility
            .render_position(window)
            .clamp(0.0, 1.0);
        let dock_icon_position = self.dock_icon_toggle.render_position(window);
        let launch_at_login_position = self.launch_at_login_toggle.render_position(window);
        let release_microphone_position = self.release_microphone_toggle.render_position(window);
        let sensitivity_control = div().flex_none().flex().gap_1().children(
            DoubleTapSensitivity::ALL
                .into_iter()
                .enumerate()
                .map(|(index, sensitivity)| {
                    compact_button(sensitivity.label())
                        .id(("double-tap-sensitivity", index))
                        .track_focus(&self.sensitivity_focus[index])
                        .border_1()
                        .border_color(rgb(LINE))
                        .focus(|style| style.border_color(rgb(ACCENT)))
                        .when(
                            self.settings.double_tap_sensitivity == sensitivity,
                            |button| button.bg(rgb(SURFACE_SELECTED)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_settings(
                                SettingControl::DoubleTapSensitivity,
                                cx,
                                |settings| settings.double_tap_sensitivity = sensitivity,
                            );
                        }))
                }),
        );
        let microphone_label = self
            .settings
            .microphone
            .clone()
            .unwrap_or_else(|| "Automatic".into());
        let microphone_picker = self
            .microphone_picker_open
            .then(|| self.render_microphone_picker(cx));
        let launch_at_login_control = if self.launch_at_login_status.is_none() {
            div()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(if self.login_item_worker.is_some() {
                    "Checking…"
                } else {
                    "Unavailable"
                })
                .into_any_element()
        } else if self.launch_at_login_status == Some(LoginItemStatus::RequiresApproval) {
            compact_button("Open Settings")
                .id("launch-at-login-approval")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.request_login_item(LoginItemRequest::OpenSettings);
                    cx.notify();
                }))
                .into_any_element()
        } else {
            toggle(launch_at_login_position)
        };
        let recording_audio_position = self.recording_audio_spring.render_position(window);
        let audio_widths = [50.0, 96.0, 90.0, 80.0];
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
        let microphone_mode = sliding_segmented_control(release_microphone_position, &[114.0; 2])
            .children(
                [("Keep ready (fast)", false), ("Release when idle", true)]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, release))| {
                        sliding_segmented_item(
                            114.0,
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
        configuration_pane(
            "Settings",
            "settings-scroll",
            div()
                .children(permission_warnings)
                .child(settings_section_label("DICTATION"))
                .child(
                    settings_panel()
                        .child(self.setting_row(SettingControl::Dictation,
                            "Dictation shortcut",
                            "Start and stop dictation with this shortcut",
                            hotkey_control,
                        ))
                        .child(
                            self.setting_row(SettingControl::DictationMode,
                                "Recording gesture", mode_description, mode_control,
                            ),
                        )
                        .child(
                            div()
                                .h(px(72.0 * double_tap_only_visibility))
                                .overflow_hidden()
                                .opacity(double_tap_only_visibility)
                                .child(
                                    self.setting_row(SettingControl::DoubleTapOnly,
                                        "Double-tap only",
                                        "Wait for two complete shortcut taps before recording",
                                        toggle(if self.settings.double_tap_only { 1.0 } else { 0.0 }),
                                    )
                                    .border_b_0()
                                    .id("double-tap-only-setting")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let enabled = !this.settings.double_tap_only;
                                        this.set_double_tap_only(enabled, cx);
                                    })),
                                ),
                        )
                        .when(self.settings.dictation_mode == DictationMode::DoubleTap, |panel| panel.child(
                            self.setting_row(SettingControl::DoubleTapSensitivity,
                                "Double-tap timing",
                                format!("Time between taps: {} ms. Choose how quickly the second tap must follow.", self.settings.double_tap_sensitivity.window().as_millis()),
                                sensitivity_control,
                            )
                        ))
                        .child(self.setting_row(SettingControl::EnterSubmit,
                            "Enter to paste and send",
                            "During locked recording, Enter stops, transcribes, pastes, then presses Enter in the input. This can send a chat message.",
                            toggle(if self.settings.enter_to_submit { 1.0 } else { 0.0 }),
                        ).border_b_0().id("enter-to-submit")
                            .track_focus(&self.enter_submit_focus)
                            .focus(|style| style.bg(rgb(SURFACE_HOVER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.update_settings(SettingControl::EnterSubmit, cx, |settings| settings.enter_to_submit = !settings.enter_to_submit);
                            }))),
                )
                .child(settings_section_label("PASTE LAST"))
                .child(
                    settings_panel().child(
                        self.setting_row(SettingControl::PasteLast,
                            "Paste last dictation",
                            "Pastes the most recent transcript again; also in the menu bar",
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(paste_last_control)
                                .when(self.settings.paste_last_hotkey.is_some(), |row| {
                                    row.child(
                                        compact_button("Disable")
                                            .id("disable-paste-last-hotkey")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.update_settings(SettingControl::PasteLast, cx, |settings| {
                                                    settings.paste_last_hotkey = None
                                                });
                                            })),
                                    )
                                }),
                        ),
                    ).child(self.render_clipboard_fallback(cx)),
                )
                .child(settings_section_label("MICROPHONE"))
                .child(
                    settings_panel()
                        .child(self.setting_row(SettingControl::Microphone,
                            "Input device",
                            "Automatic picks the preferred available microphone",
                            div()
                                .relative()
                                .flex_none()
                                .child(
                                    disclosure_button(microphone_label)
                                        .id("microphone-setting")
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
                            "Cuts silence and long pauses before sending, so less audio is billed. Recordings with no speech are not sent",
                            trim_control,
                        ))
                        .child(
                            self.setting_row(SettingControl::AudioBehavior,
                                "While dictating",
                                match self.settings.recording_audio_behavior {
                                    RecordingAudioBehavior::Mute => "Fades system audio out and back in quickly; preserves detected manual volume changes",
                                    RecordingAudioBehavior::LowerVolume => "Lowers system audio with a quick fade; preserves detected manual volume changes",
                                    _ => "What happens to other audio once a hold becomes a dictation",
                                },
                                audio_behavior,
                            )
                            .when(self.settings.recording_audio_behavior != RecordingAudioBehavior::LowerVolume, |row| row.border_b_0()),
                        )
                        .children(self.render_lower_volume_row(cx)),
                )
                .child(settings_section_label("APPLICATION"))
                .child(
                    settings_panel()
                        .child(
                            settings_row(
                                "Launch at login",
                                "Start Hex when you sign in to your Mac",
                                launch_at_login_control,
                            )
                            .id("launch-at-login-setting")
                            .when(
                                matches!(
                                    self.launch_at_login_status,
                                    Some(LoginItemStatus::Enabled | LoginItemStatus::Disabled)
                                ) && (self.preview || self.login_item_worker.is_some()),
                                |row| {
                                    row.on_click(cx.listener(|this, _, _, cx| {
                                        let enabled = !this.launch_at_login_toggle.enabled();
                                        this.set_launch_at_login(enabled, cx);
                                    }))
                                },
                            ),
                        )
                        .when_some(self.launch_at_login_error.clone(), |panel, error| {
                            panel.child(
                                div()
                                    .px_4()
                                    .py_2()
                                    .text_size(px(11.0))
                                    .text_color(rgb(NEGATIVE))
                                    .child(error),
                            )
                        })
                        .child(
                            self.setting_row(SettingControl::Dock,
                                "Show Dock icon",
                                "When off, Hex lives in the menu bar while this window is closed",
                                toggle(dock_icon_position),
                            )
                            .border_b_0()
                            .id("dock-icon-setting")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.update_settings(SettingControl::Dock, cx, |settings| {
                                    settings.show_dock_icon = !settings.show_dock_icon
                                }) {
                                    this.dock_icon_toggle
                                        .set_enabled(this.settings.show_dock_icon);
                                }
                            })),
                        )
                        ,
                )
                .child(settings_section_label("SOUNDS"))
                .child(self.sound_settings.clone())
                .child(settings_section_label("PREFERENCES"))
                .child(self.render_preference_transfer(cx)),
        )
    }

    fn render_setup(&mut self, cx: &mut Context<Self>) -> AnyElement {
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
                PermissionState::NeedsRequest | PermissionState::Ready => compact_button("Allow")
                    .id("setup-microphone")
                    .bg(rgb(SURFACE_SELECTED))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.preview {
                            crate::onboarding::request_microphone();
                        }
                        cx.notify();
                    })),
            };
            permission_rows.push(setup_row(
                "Microphone",
                "Record while you hold the shortcut.",
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
            .bg(rgba(0x000000dd))
            .child(
                div()
                    .id("setup")
                    .w(px(640.0))
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
                                    .child("Tap the shortcut to keep recording and tap again to stop, or hold and release. Hex trims silence, transcribes through OpenRouter, and pastes the text once these permissions and your key are in place."),
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
                            .child(setup_group_label("OPENROUTER"))
                            .child(if status.api_key {
                                div()
                                    .border_t_1()
                                    .border_color(rgb(LINE))
                                    .child(setup_row(
                                        "OpenRouter API key",
                                        "Choose models and fallbacks in Models.",
                                        setup_ready_badge(),
                                    ))
                                    .into_any_element()
                            } else {
                                self.openrouter_setup.clone().into_any_element()
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Drop for AppWindow {
    fn drop(&mut self) {
        if !self.preview {
            crate::app_settings::set_hotkey_capture_active(false);
            crate::app_settings::set_dock_icon_visible(crate::app_settings::dock_icon_visible(
                self.settings.show_dock_icon,
                crate::status_item::installed(),
            ));
        }
    }
}

impl Render for AppWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notice = self.render_key_notice(cx);
        let content = if self.setup_visible {
            div().into_any_element()
        } else {
            match self.pane {
                Pane::Settings => self.render_settings(window, cx),
                Pane::Models => self.render_models(),
                Pane::Hud => self.hud_settings.clone().into_any_element(),
                Pane::History => {
                    self.reconcile_recovery_focus(window);
                    self.render_history(cx)
                }
                Pane::Statistics => self.statistics.clone().into_any_element(),
            }
        };
        let setup = self.setup_visible.then(|| self.render_setup(cx));
        window_frame()
            .track_focus(&self.window_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "tab"
                    && matches!(this.hotkey_capture, HotkeyCaptureState::Idle)
                {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key != "escape" {
                    return;
                }
                if this.history_retention_open
                    || this.microphone_picker_open
                    || this.microphone_channel_picker_open
                {
                    this.history_retention_open = false;
                    this.microphone_picker_open = false;
                    this.microphone_channel_picker_open = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(|_: &MinimizeWindow, window, _| window.minimize_window())
            .on_action(|_: &ToggleFullscreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|this, _: &ShowSettings, window, cx| {
                this.select_pane(Pane::Settings, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowModels, window, cx| {
                this.select_pane(Pane::Models, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowHud, window, cx| {
                this.select_pane(Pane::Hud, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowHistory, window, cx| {
                this.select_pane(Pane::History, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowStatistics, window, cx| {
                this.select_pane(Pane::Statistics, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .child(self.render_navigation(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .children(notice)
                    .when_some(self.settings_error.clone(), |column, error| {
                        column.child(
                            div()
                                .px_5()
                                .py_2()
                                .child(error_message("Settings error:", error)),
                        )
                    })
                    .child(div().flex_1().min_h_0().child(content)),
            )
            .children(setup)
    }
}

/// A real saved WAV and failure entry, entirely synthetic and isolated from
/// app services. The preview Retry handler also uses a local fixture.
fn preview_recording_recovery() -> Option<RecordingRecovery> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_PREVIEW: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "hex-recovery-preview-{}-{}",
        std::process::id(),
        NEXT_PREVIEW.fetch_add(1, Ordering::Relaxed),
    ));
    let store = RecordingRecovery::open(directory).ok()?;
    let samples: Vec<f32> = (0..294_400)
        .map(|index| (index as f32 * std::f32::consts::TAU * 220.0 / 16_000.0).sin() * 0.08)
        .collect();
    let _ = store.transcribe_original(&samples, Some("Codex"), |_| {
        Err(crate::openrouter::transcribe::ChainFailure { failures: vec![crate::openrouter::stats::Failure {
            model: "openai/whisper-large-v3-turbo".into(),
            kind: crate::openrouter::stats::ErrorKind::Timeout,
            detail: "openai/whisper-large-v3-turbo: network error (exit status: 28): curl: (28) Operation timed out after 30000 milliseconds with 0 bytes received".into(),
        }] }.into())
    });
    Some(store)
}

/// Deterministic History fixtures for the preview.
fn preview_history() -> Option<History> {
    use crate::history::{HistoryDraft, HistoryStore, now_ms};
    use crate::openrouter::{AudioTrim, StepReport};
    let directory =
        std::env::temp_dir().join(format!("hex-history-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).ok()?;
    let mut store = HistoryStore::open(
        directory.join("history.json"),
        HistoryRetention::Week,
        now_ms(),
    );
    let now = now_ms();
    let report = |model: &str, latency_ms, failed: &[&str], recorded_ms, sent_ms| StepReport {
        model: Some(model.into()),
        latency_ms,
        failed: failed.iter().map(|model| (*model).into()).collect(),
        audio: Some(AudioTrim {
            recorded_ms,
            sent_ms,
        }),
    };
    let fixtures = [
        (
            26 * 60 * 60 * 1_000,
            "The parser needs a retry when the socket drops.",
            Some("Zed"),
            report("openai/whisper-large-v3-turbo", 640, &[], 3_400, 2_600),
        ),
        (
            3 * 60 * 60 * 1_000,
            "Sounds good, shipping it after lunch.",
            Some("Slack"),
            report(
                "openai/gpt-4o-mini-transcribe",
                1_180,
                &["openai/whisper-large-v3-turbo"],
                2_900,
                2_100,
            ),
        ),
        (
            4 * 60 * 1_000,
            "Remember to validate the artifact before publishing the release, and double-check that the notes mention the new model pickers.",
            Some("Messages"),
            report("openai/whisper-large-v3-turbo", 710, &[], 9_400, 6_100),
        ),
    ];
    for (age_ms, text, application, transcription) in fixtures {
        let _ = store.record(
            HistoryDraft {
                text: text.into(),
                application: application.map(Into::into),
                audio_ms: transcription.audio.map_or(0, |audio| audio.recorded_ms),
                inference_ms: transcription.latency_ms,
                total_ms: transcription.latency_ms + 120,
                transcription: Some(transcription),
            },
            now.saturating_sub(age_ms),
        );
    }
    Some(History::new(store))
}

fn set_hotkey_binding(settings: &mut AppSettings, kind: HotkeyKind, binding: HotkeyBinding) {
    match kind {
        HotkeyKind::Dictation => {
            if binding.key.is_none() {
                settings.double_tap_only = false;
            }
            settings.dictation_hotkey = binding;
        }
        HotkeyKind::PasteLast => settings.paste_last_hotkey = Some(binding),
    }
}

fn hotkey_idle_width(keycap_count: usize) -> f32 {
    (112.0 + hotkey_keycaps_width(keycap_count)).max(HOTKEY_MIN_WIDTH)
}

const fn hotkey_kind_index(kind: HotkeyKind) -> usize {
    match kind {
        HotkeyKind::Dictation => 0,
        HotkeyKind::PasteLast => 1,
    }
}

const fn hotkey_side_index(side: ModifierSide) -> usize {
    match side {
        ModifierSide::Left => 0,
        ModifierSide::Either => 1,
        ModifierSide::Right => 2,
    }
}

fn standalone_modifier_side(binding: &HotkeyBinding) -> Option<ModifierSide> {
    if binding.key.is_some() || binding.modifiers.count() != 1 {
        return None;
    }
    binding
        .modifiers
        .control
        .or(binding.modifiers.option)
        .or(binding.modifiers.shift)
        .or(binding.modifiers.command)
}

fn set_standalone_modifier_side(binding: &mut HotkeyBinding, side: ModifierSide) {
    if standalone_modifier_side(binding).is_none() {
        return;
    }
    if binding.modifiers.control.is_some() {
        binding.modifiers.control = Some(side);
    } else if binding.modifiers.option.is_some() {
        binding.modifiers.option = Some(side);
    } else if binding.modifiers.shift.is_some() {
        binding.modifiers.shift = Some(side);
    } else if binding.modifiers.command.is_some() {
        binding.modifiers.command = Some(side);
    }
}

fn hotkey_binding_conflicts(
    settings: &AppSettings,
    kind: HotkeyKind,
    binding: &HotkeyBinding,
) -> bool {
    let others = match kind {
        HotkeyKind::Dictation => settings.paste_last_hotkey.clone(),
        HotkeyKind::PasteLast => Some(settings.dictation_hotkey.clone()),
    };
    crate::app_settings::hotkey_conflicts(binding, others)
}

fn default_hotkey_binding(kind: HotkeyKind) -> HotkeyBinding {
    match kind {
        HotkeyKind::Dictation => HotkeyBinding::default(),
        HotkeyKind::PasteLast => HotkeyBinding::paste_last_default(),
    }
}

fn hotkey_binding(settings: &AppSettings, kind: HotkeyKind) -> Option<&HotkeyBinding> {
    match kind {
        HotkeyKind::Dictation => Some(&settings.dictation_hotkey),
        HotkeyKind::PasteLast => settings.paste_last_hotkey.as_ref(),
    }
}

fn hotkey_side_binding(
    settings: &AppSettings,
    kind: HotkeyKind,
    side: ModifierSide,
) -> Option<HotkeyBinding> {
    let mut binding = hotkey_binding(settings, kind)?.clone();
    standalone_modifier_side(&binding)?;
    set_standalone_modifier_side(&mut binding, side);
    (!hotkey_binding_conflicts(settings, kind, &binding)).then_some(binding)
}

fn hotkey_capture_width(keycap_count: usize) -> f32 {
    if keycap_count == 0 {
        180.0
    } else {
        236.0 + hotkey_keycaps_width(keycap_count)
    }
}

fn hotkey_saved_width(keycap_count: usize) -> f32 {
    (53.0 + hotkey_keycaps_width(keycap_count)).max(HOTKEY_MIN_WIDTH)
}

fn hotkey_keycaps_width(keycap_count: usize) -> f32 {
    if keycap_count == 0 {
        0.0
    } else {
        34.0 * keycap_count as f32 + 3.0 * keycap_count.saturating_sub(1) as f32
    }
}

fn recording_audio_index(behavior: RecordingAudioBehavior) -> usize {
    match behavior {
        RecordingAudioBehavior::Mute => 0,
        RecordingAudioBehavior::LowerVolume => 1,
        RecordingAudioBehavior::PauseMedia => 2,
        RecordingAudioBehavior::DoNothing => 3,
    }
}

fn setup_row(title: &'static str, description: &'static str, control: AnyElement) -> Div {
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

fn setup_group_label(label: &'static str) -> Div {
    div()
        .pb_2()
        .text_size(px(10.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .child(label)
}

fn setup_ready_badge() -> AnyElement {
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

const fn permission_warning_copy(kind: PermissionKind) -> (&'static str, &'static str) {
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

const fn permission_action_label(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::RequestMicrophone => "Allow",
        PermissionAction::OpenMicrophoneSettings => "Open Settings",
        PermissionAction::OpenInputMonitoringSettings
        | PermissionAction::OpenAccessibilitySettings => "Grant Access",
    }
}

const fn permission_warning_id(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Microphone => "permission-warning-microphone",
        PermissionKind::InputMonitoring => "permission-warning-input-monitoring",
        PermissionKind::Accessibility => "permission-warning-accessibility",
    }
}

fn hotkey_modifiers(modifiers: GpuiModifiers) -> HotkeyModifiers {
    hotkey_modifiers_with_flags(modifiers, crate::suppression::physical_modifier_flags())
}

fn hotkey_modifiers_with_flags(modifiers: GpuiModifiers, flags: u64) -> HotkeyModifiers {
    let physical = crate::app_settings::modifiers_from_flags(flags);
    if !physical.is_empty() {
        return physical;
    }
    HotkeyModifiers {
        control: modifiers.control.then_some(Default::default()),
        option: modifiers.alt.then_some(Default::default()),
        shift: modifiers.shift.then_some(Default::default()),
        command: modifiers.platform.then_some(Default::default()),
        function: modifiers.function,
    }
}

fn hotkey_key(key: &str) -> Result<HotkeyKey, &'static str> {
    let special = match key {
        "space" => Some((49, "Space")),
        "tab" => Some((48, "Tab")),
        "enter" => Some((36, "Return")),
        "backspace" => Some((51, "Delete")),
        "up" => Some((126, "Up")),
        "down" => Some((125, "Down")),
        "left" => Some((123, "Left")),
        "right" => Some((124, "Right")),
        "pageup" => Some((116, "Page Up")),
        "pagedown" => Some((121, "Page Down")),
        "home" => Some((115, "Home")),
        "end" => Some((119, "End")),
        "delete" => Some((117, "Forward Delete")),
        "insert" => Some((114, "Help")),
        "f1" => Some((122, "F1")),
        "f2" => Some((120, "F2")),
        "f3" => Some((99, "F3")),
        "f4" => Some((118, "F4")),
        "f5" => Some((96, "F5")),
        "f6" => Some((97, "F6")),
        "f7" => Some((98, "F7")),
        "f8" => Some((100, "F8")),
        "f9" => Some((101, "F9")),
        "f10" => Some((109, "F10")),
        "f11" => Some((103, "F11")),
        "f12" => Some((111, "F12")),
        "f13" => Some((105, "F13")),
        "f14" => Some((107, "F14")),
        "f15" => Some((113, "F15")),
        "f16" => Some((106, "F16")),
        "f17" => Some((64, "F17")),
        "f18" => Some((79, "F18")),
        "f19" => Some((80, "F19")),
        "f20" => Some((90, "F20")),
        _ => None,
    };
    if let Some((code, label)) = special {
        return Ok(HotkeyKey {
            code,
            label: label.into(),
        });
    }
    let mut characters = key.chars();
    let Some(character) = characters.next() else {
        return Err("Unsupported key");
    };
    if characters.next().is_some() {
        return Err("Unsupported key");
    }
    let code = crate::keyboard::key_code_for(character).map_err(|_| "Unsupported key")?;
    Ok(HotkeyKey {
        code,
        label: character.to_uppercase().collect(),
    })
}

fn is_function_key(label: &str) -> bool {
    label
        .strip_prefix('F')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=20).contains(&number))
}

fn detail_placeholder(message: &'static str) -> AnyElement {
    div()
        .flex_1()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(rgb(FAINT))
        .child(message)
        .into_any_element()
}

fn detail_row(label: &'static str, value: impl Into<String>) -> AnyElement {
    div()
        .py_3()
        .flex()
        .items_start()
        .gap_4()
        .border_b_1()
        .border_color(rgb(LINE))
        .child(
            div()
                .w(px(104.0))
                .flex_none()
                .text_size(px(11.0))
                .text_color(rgb(FAINT))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(rgb(TEXT_SOFT))
                .child(value.into()),
        )
        .into_any_element()
}

fn seconds_label(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1_000.0)
}

fn event_age(timestamp_ms: u64) -> String {
    let seconds = crate::events::now_ms().saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn preferences_path_allowed(path: &std::path::Path) -> bool {
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

fn read_preferences_export(
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

fn write_preferences_export(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
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

fn write_preferences_export_to(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn preview_fixture(window: &mut Window, cx: &mut Context<AppWindow>) -> AppWindow {
        AppWindow::new(
            None,
            None,
            Some(AppWindowPreview {
                pane: PreviewPane::Settings,
                onboarding: false,
                permissions_missing: false,
                open_history_retention: false,
            }),
            window,
            cx,
        )
    }

    #[test]
    fn preference_export_collision_preserves_the_existing_file() {
        let folder = std::env::temp_dir().join(format!(
            "hex-export-collision-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        let target = folder.join("preferences.json");
        let temporary = folder.join("existing.tmp");
        std::fs::write(&temporary, b"existing content").unwrap();
        assert!(write_preferences_export_to(&target, &temporary, b"new export").is_err());
        assert_eq!(std::fs::read(&temporary).unwrap(), b"existing content");
        assert!(!target.exists());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[gpui::test]
    fn imported_preferences_sync_new_controls_models_and_follow_up_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::Focusable;
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.simulate_resize(size(px(WINDOW_WIDTH), px(2400.0)));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let mut desired = view.settings.clone();
                desired.dictation_mode = DictationMode::DoubleTap;
                desired.double_tap_lock = true;
                desired.double_tap_only = true;
                desired.dictation_hotkey.key = Some(HotkeyKey {
                    code: 100,
                    label: "F8".into(),
                });
                desired.double_tap_sensitivity = DoubleTapSensitivity::ALL[2];
                desired.enter_to_submit = true;
                desired.copy_on_paste_failure = true;
                desired.recording_audio_behavior = RecordingAudioBehavior::LowerVolume;
                desired.lower_volume_percent = 35;
                desired.release_microphone_while_idle = true;
                desired.hud.position = crate::hud_settings::HudPosition::Bottom;
                desired.hud.recording_color = crate::hud_settings::HudColor::Green;
                desired.sound_volumes = Some(crate::interaction_settings::SoundVolumes {
                    start: 0.25,
                    stop: 0.75,
                    error_cancel: 0.0,
                });
                desired.microphone_priority = vec![crate::microphone::DevicePreference {
                    id: Some("preview-input".into()),
                    name: "Built-in Microphone".into(),
                }];
                let mut models = crate::openrouter::Config::default();
                models.transcription.models = vec!["fixture/new-model".into()];
                models.transcription.language = "pt".into();
                models.transcription.trim_silence = false;
                let bytes = crate::preferences_transfer::export_bytes(&desired, &models).unwrap();
                let bundle = crate::preferences_transfer::decode(&bytes).unwrap();
                let imported = crate::preferences_transfer::preview_bundle(
                    bundle,
                    &view.settings,
                    &view.openrouter_settings.read(cx).config_snapshot(),
                )
                .unwrap();
                view.lower_volume_input
                    .update(cx, |input, cx| input.set_text("invalid draft", cx));
                view.microphone_picker_open = true;
                view.microphone_channel_picker_open = true;
                view.begin_hotkey_capture(HotkeyKind::Dictation, window, cx);
                view.accept_imported_preferences(imported, cx);
                assert!(matches!(view.hotkey_capture, HotkeyCaptureState::Idle));
                assert!(!view.microphone_picker_open && !view.microphone_channel_picker_open);
                assert_eq!(view.lower_volume_input.read(cx).text(), "35");
                assert_eq!(view.settings.dictation_mode, DictationMode::DoubleTap);
                assert!(view.settings.enter_to_submit && view.settings.copy_on_paste_failure);
                assert!(view.double_tap_only_visibility.enabled());
                assert!(!view.hotkey_side_animations[0].enabled());
                assert!(view.release_microphone_toggle.enabled());
                assert_eq!(view.recording_audio_spring.target, 1.0);
                for editor in [&view.openrouter_settings, &view.openrouter_setup] {
                    assert_eq!(
                        editor.read(cx).config_snapshot().transcription,
                        models.transcription
                    );
                }
                assert_eq!(
                    view.settings.microphone_priority,
                    desired.microphone_priority
                );
                assert_eq!(view.settings.hud, desired.hud);
                assert_eq!(
                    view.settings.effective_sound_volumes(),
                    desired.effective_sound_volumes()
                );
                assert!(view.settings_feedback.is_none());
                view.show_settings(cx);
            });
        });
        cx.run_until_parked();
        // Reconfirming the child's selected Start level must emit the imported
        // value, and editing Stop must preserve the other imported levels.
        cx.update(|window, cx| {
            view.read(cx).sound_settings.focus_handle(cx).focus(window);
        });
        cx.simulate_keystrokes("enter tab home enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.settings.effective_sound_volumes().start, 0.25);
                assert_eq!(view.settings.effective_sound_volumes().stop, 0.0);
                assert_eq!(view.settings.effective_sound_volumes().error_cancel, 0.0);
                view.set_dictation_mode(DictationMode::TapOrHold, cx);
                assert!(!view.settings.double_tap_only);
                assert!(view.settings.enter_to_submit && view.settings.copy_on_paste_failure);
                assert_eq!(view.settings.lower_volume_percent, 35);
                assert!(
                    view.setting_feedback(SettingControl::DictationMode)
                        .is_none()
                );
            });
        });
    }

    #[gpui::test]
    fn reset_restores_both_shortcuts_and_reenables_paste_last(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.simulate_resize(size(px(MINIMUM_WIDTH), px(MINIMUM_HEIGHT)));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.settings.paste_last_hotkey = None;
                view.settings.dictation_hotkey = HotkeyBinding::paste_last_default();
                view.settings.double_tap_only = true;
                view.begin_hotkey_capture(HotkeyKind::Dictation, window, cx);
                assert!(view.reset_hotkey_binding(HotkeyKind::Dictation, cx));
                assert_eq!(view.settings.dictation_hotkey, HotkeyBinding::default());
                assert!(!view.settings.double_tap_only);
                assert!(matches!(view.hotkey_capture, HotkeyCaptureState::Idle));
                assert!(view.settings.paste_last_hotkey.is_none());
                view.hotkey_reset_focus[1].focus(window);
            })
        });
        cx.run_until_parked();
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(
                view.settings.paste_last_hotkey,
                Some(HotkeyBinding::paste_last_default())
            );
            assert!(matches!(view.hotkey_capture, HotkeyCaptureState::Idle));
            assert!(view.setting_feedback(SettingControl::PasteLast).is_none());
        });
        for selector in ["reset-hotkey-0", "reset-hotkey-1"] {
            let bounds = cx.debug_bounds(selector).unwrap();
            assert!(
                bounds.left() >= px(SIDEBAR_WIDTH) && bounds.right() <= px(MINIMUM_WIDTH),
                "{selector}: {bounds:?}"
            );
        }
    }

    #[gpui::test]
    fn reset_refuses_to_overwrite_another_shortcuts_binding(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.settings.dictation_hotkey = HotkeyBinding::paste_last_default();
                view.settings.paste_last_hotkey = None;
                assert!(!view.reset_hotkey_binding(HotkeyKind::PasteLast, cx));
                assert!(view.settings.paste_last_hotkey.is_none());
                assert_eq!(
                    view.settings.dictation_hotkey,
                    HotkeyBinding::paste_last_default()
                );
                assert!(view.feedback_error(SettingControl::PasteLast).is_some());

                view.settings.paste_last_hotkey = Some(HotkeyBinding::default());
                assert!(!view.reset_hotkey_binding(HotkeyKind::Dictation, cx));
                assert_eq!(
                    view.settings.dictation_hotkey,
                    HotkeyBinding::paste_last_default()
                );
                assert_eq!(
                    view.settings.paste_last_hotkey,
                    Some(HotkeyBinding::default())
                );
                assert!(view.feedback_error(SettingControl::Dictation).is_some());
            })
        });
    }

    #[gpui::test]
    fn customization_views_save_through_the_parent_and_survive_pane_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut prefs = view.settings.hud;
                prefs.position = crate::hud_settings::HudPosition::Bottom;
                view.hud_settings
                    .update(cx, |_, cx| cx.emit(HudChange { preferences: prefs }));
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(
                    view.settings.hud.position,
                    crate::hud_settings::HudPosition::Bottom
                );
                view.sound_settings.update(cx, |_, cx| {
                    cx.emit(SoundVolumeChange {
                        event: SoundEvent::Start,
                        volume: 1.0,
                    })
                });
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.settings.effective_sound_volumes().start, 1.0);
                assert_eq!(view.settings.effective_sound_volumes().stop, 0.5);
                view.show_settings(cx);
                view.show_hud(cx);
                assert_eq!(
                    view.settings.hud.position,
                    crate::hud_settings::HudPosition::Bottom
                );
                assert!(view.setting_feedback(SettingControl::Sound).is_none());
            })
        });
    }

    #[gpui::test]
    fn recording_gesture_and_enter_option_are_keyboard_accessible(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|window, cx| {
            assert_eq!(
                view.read(cx).settings.dictation_mode,
                DictationMode::TapOrHold
            );
            assert!(!view.read(cx).settings.enter_to_submit);
            view.read(cx).dictation_mode_focus[1].focus(window);
        });
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            assert_eq!(view.read(cx).settings.dictation_mode, DictationMode::Hold);
            view.read(cx).enter_submit_focus.focus(window);
        });
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("space").unwrap(),
        });
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert!(view.settings.enter_to_submit);
                view.set_dictation_mode(DictationMode::DoubleTap, cx);
                assert!(view.settings.double_tap_lock);
                view.settings.double_tap_only = true;
                view.set_dictation_mode(DictationMode::TapOrHold, cx);
                assert!(!view.settings.double_tap_lock);
                assert!(!view.settings.double_tap_only);
                assert!(view.settings.enter_to_submit);
            })
        });
    }

    #[gpui::test]
    fn clipboard_fallback_is_opt_in_and_keyboard_accessible(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| assert!(!view.read(cx).settings.copy_on_paste_failure));
        cx.update(|window, cx| view.read(cx).clipboard_fallback_focus.focus(window));
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|_, cx| assert!(view.read(cx).settings.copy_on_paste_failure));
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("space").unwrap(),
        });
        cx.update(|_, cx| assert!(!view.read(cx).settings.copy_on_paste_failure));
    }

    #[gpui::test]
    fn microphone_and_channel_choices_are_keyboard_accessible(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|window, cx| view.read(cx).microphone_picker_state.trigger.focus(window));
        cx.simulate_keystrokes("enter down enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(!view.microphone_picker_open);
            assert_eq!(
                view.settings.microphone,
                view.microphone_devices.first().cloned()
            );
            assert!(view.microphone_picker_state.trigger.is_focused(window));
            assert!(view.setting_feedback(SettingControl::Microphone).is_none());
        });
        cx.simulate_keystrokes("tab tab enter end enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(
                view.settings.microphone_channel.as_ref().unwrap().channel,
                2
            );
            assert!(
                view.microphone_channel_picker_state
                    .trigger
                    .is_focused(window)
            );
        });
        cx.simulate_keystrokes("enter escape");
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(!view.microphone_channel_picker_open);
            assert!(
                view.microphone_channel_picker_state
                    .trigger
                    .is_focused(window)
            );
        });
    }

    #[gpui::test]
    fn lower_volume_editor_saves_valid_percentages_and_keeps_invalid_input_local(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.settings.recording_audio_behavior = RecordingAudioBehavior::LowerVolume;
                assert_eq!(view.lower_volume_input.read(cx).text(), "80");
                for percent in ["35", "0", "100"] {
                    view.lower_volume_input
                        .update(cx, |input, cx| input.set_text(percent, cx));
                    view.save_lower_volume(cx);
                    assert_eq!(view.settings.lower_volume_percent.to_string(), percent);
                    assert!(view.feedback_error(SettingControl::LowerVolume).is_none());
                }
                for invalid in ["101", "-1", "abc", ""] {
                    view.lower_volume_input
                        .update(cx, |input, cx| input.set_text(invalid, cx));
                    view.save_lower_volume(cx);
                    assert_eq!(view.settings.lower_volume_percent, 100);
                    assert!(view.feedback_error(SettingControl::LowerVolume).is_some());
                }
            })
        });
    }

    #[gpui::test]
    fn retention_keyboard_selection_restores_focus(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.select_pane(Pane::History, cx);
                view.history_retention_picker_state.trigger.focus(window);
            })
        });
        cx.simulate_keystrokes("enter home enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(view.settings.history_retention, HistoryRetention::ALL[0]);
            assert!(!view.history_retention_open);
            assert!(
                view.history_retention_picker_state
                    .trigger
                    .is_focused(window)
            );
        });
        cx.simulate_keystrokes("enter escape");
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert!(!view.history_retention_open);
            assert!(
                view.history_retention_picker_state
                    .trigger
                    .is_focused(window)
            );
        });
    }

    #[gpui::test]
    fn setup_keyboard_focus_cannot_reach_hidden_settings(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = preview_fixture(window, cx);
            view.setup_visible = true;
            view
        });
        for _ in 0..12 {
            cx.simulate_keystrokes("tab");
            cx.update(|window, cx| {
                let view = view.read(cx);
                assert!(!view.microphone_picker_state.trigger.is_focused(window));
                assert!(
                    !view
                        .microphone_channel_picker_state
                        .trigger
                        .is_focused(window)
                );
                assert!(
                    !view
                        .history_retention_picker_state
                        .trigger
                        .is_focused(window)
                );
            });
        }
    }

    #[gpui::test]
    fn microphone_errors_stay_visible_outside_the_scrolling_choices(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.simulate_resize(gpui::size(px(1040.0), px(720.0)));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.microphone_devices =
                    (0..30).map(|index| format!("Microphone {index}")).collect();
                view.toggle_microphone_picker(window, cx);
                view.microphone_picker_error = Some("Could not save the microphone choice.".into());
            })
        });
        cx.run_until_parked();
        let before = cx.debug_bounds("picker-feedback").unwrap();
        cx.simulate_keystrokes("end");
        let after = cx.debug_bounds("picker-feedback").unwrap();
        let menu = cx.debug_bounds("microphone-picker").unwrap();
        assert_eq!(before, after, "feedback must not scroll with choices");
        assert!(after.top() >= menu.top() && after.bottom() <= menu.bottom());
        assert!(menu.left() >= px(0.0) && menu.right() <= px(1040.0));
        assert!(menu.top() >= px(0.0) && menu.bottom() <= px(720.0));
    }

    #[gpui::test]
    fn login_item_response_reconciles_optimistic_toggle_and_unknown_state(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, _| {
                view.launch_at_login_status = None;
                assert!(view.apply_login_item_response(LoginItemResponse {
                    request: LoginItemRequest::Status,
                    result: Ok(LoginItemStatus::Disabled),
                }));
                assert_eq!(view.launch_at_login_status, Some(LoginItemStatus::Disabled));
                view.launch_at_login_toggle.set_enabled(true);
                view.launch_at_login_error = Some("Registration failed".into());
                // Even with unchanged confirmed status and error, rollback needs a redraw.
                assert!(view.apply_login_item_response(LoginItemResponse {
                    request: LoginItemRequest::SetEnabled(true),
                    result: Err(crate::login_item::LoginItemFailure {
                        status: Some(LoginItemStatus::Disabled),
                        message: "Registration failed".into(),
                    }),
                }));
                assert!(!view.launch_at_login_toggle.enabled());
                assert!(view.apply_login_item_response(LoginItemResponse {
                    request: LoginItemRequest::SetEnabled(false),
                    result: Ok(LoginItemStatus::Disabled),
                }));
                assert!(view.launch_at_login_error.is_none());
                assert!(view.apply_login_item_response(LoginItemResponse {
                    request: LoginItemRequest::Status,
                    result: Ok(LoginItemStatus::Enabled),
                }));
                assert!(view.launch_at_login_toggle.enabled());
                assert!(!view.apply_login_item_response(LoginItemResponse {
                    request: LoginItemRequest::Status,
                    result: Ok(LoginItemStatus::Enabled),
                }));
            });
        });
    }

    #[gpui::test]
    fn recovery_survives_history_clear_and_preview_retry_is_local(cx: &mut gpui::TestAppContext) {
        let folder = std::env::temp_dir().join(format!(
            "hex-recovery-view-{}-{}",
            std::process::id(),
            crate::history::now_ms()
        ));
        let store = RecordingRecovery::open(folder.clone()).unwrap();
        assert!(
            store
                .transcribe_original(&[0.1; 1600], Some("Notes"), |_| Err(
                    color_eyre::eyre::eyre!("offline fixture")
                ))
                .is_err()
        );
        let id = store.entries("")[0].id.clone();
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.recovery = Some(store.clone());
                let now = crate::history::now_ms();
                let mut history = crate::history::HistoryStore::open(
                    folder.join("normal-history.json"),
                    HistoryRetention::Week,
                    now,
                );
                history
                    .record(
                        crate::history::HistoryDraft {
                            text: "Successful dictation".into(),
                            application: Some("Notes".into()),
                            audio_ms: 100,
                            inference_ms: 100,
                            total_ms: 100,
                            transcription: None,
                        },
                        now,
                    )
                    .unwrap();
                view.history = Some(History::new(history));
                view.settings.history_retention = HistoryRetention::Off;
                view.select_pane(Pane::History, cx);
                view.reload_history(cx);
                view.selected_recovery = Some(id.clone());
                view.history_clear_armed = true;
                view.clear_history(cx);
                assert_eq!(view.recovery_entries.len(), 1);
                assert!(view.history_entries.is_empty());
                view.retry_recovery("ffffffffffffffffffffffffffffffff", cx);
                assert!(view.recovery_error.is_some());
                assert!(
                    view.history_error.is_none(),
                    "a Retry error must stay beside the recovery actions"
                );
                view.recovery_action_focus[1].focus(window);
                assert!(view.recovery_action_focus[1].is_focused(window));
                view.retry_recovery(&id, cx);
                assert!(view.recovery_error.is_none());
            })
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while store.retry_in_progress() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.recovery_entries[0].status = RecoveryStatus::Failed;
                view.recovery_entries[0].text = None;
                assert!(view.poll_history(cx));
                // The production polling task notifies after a changed result.
                // Without that notification this test updates data but never
                // renders the Retry-to-Copy focus transition.
                cx.notify();
                assert_eq!(
                    view.recovery_entries[0].text.as_deref(),
                    Some("Recovered preview dictation.")
                );
                assert_eq!(view.recovery_entries[0].status, RecoveryStatus::Recovered);
                assert_eq!(
                    view.recovery_entries[0].application.as_deref(),
                    Some("Notes")
                );
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(view.read(cx).recovery_action_focus[0].is_focused(window));
        });
        drop(store);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[gpui::test]
    fn every_pane_renders_in_preview(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        for pane in Pane::ALL {
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_pane(pane, cx)));
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    fn models_has_its_own_navigation_and_reuses_the_key_editor(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let status = crate::openrouter::KeyStatus::Keychain("demo".into());
                view.openrouter_settings.update(cx, |editor, cx| {
                    editor.sync_key_status(status.clone(), cx);
                });
                view.setup_status.api_key = false;
                view.show_settings(cx);
                assert!(view.render_key_notice(cx).is_some());
                view.show_models(cx);
                assert_eq!(view.pane, Pane::Models);
                assert!(view.render_key_notice(cx).is_none());
                view.show_history(cx);
                view.show_models(cx);
                assert_eq!(
                    view.openrouter_settings.read(cx).key_status(),
                    Some(&status)
                );
            });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn microphone_trim_control_keeps_its_value_across_panes(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert!(view.openrouter_settings.read(cx).trim_silence());
                view.toggle_trim_silence(cx);
                assert!(!view.openrouter_settings.read(cx).trim_silence());
                assert!(view.settings_error.is_none());
                view.show_models(cx);
                view.show_settings(cx);
                assert!(!view.openrouter_settings.read(cx).trim_silence());
                view.toggle_trim_silence(cx);
                assert!(view.openrouter_settings.read(cx).trim_silence());
            });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn explicit_microphone_channel_stays_bound_to_its_device(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let device = view.microphone_description.clone().unwrap();
                assert_eq!(device.channel, None);
                view.select_microphone_channel(&device, Some(2), cx);
                assert_eq!(
                    view.microphone_description.as_ref().unwrap().channel,
                    Some(2)
                );
                assert_eq!(
                    view.settings.microphone, None,
                    "automatic device selection must stay unchanged"
                );
                let saved = view.settings.microphone_channel.clone();
                let mut stale = device.clone();
                stale.device_id = Some("different-device".into());
                view.select_microphone_channel(&stale, Some(1), cx);
                assert_eq!(view.settings.microphone_channel, saved);
                assert!(view.feedback_error(SettingControl::Channel).is_some());
                view.select_microphone_channel(&device, None, cx);
                assert_eq!(view.settings.microphone_channel, None);
                assert_eq!(view.microphone_description.as_ref().unwrap().channel, None);
                assert!(view.settings_error.is_none());
            });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn setup_key_changes_update_the_settings_editor(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        let status = crate::openrouter::KeyStatus::Keychain("demo".into());
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.openrouter_setup.update(cx, |_, cx| {
                    cx.emit(KeyChanged(status.clone()));
                });
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(
                    view.openrouter_settings.read(cx).key_status(),
                    Some(&status)
                );
                view.openrouter_settings.update(cx, |_, cx| {
                    cx.emit(KeyChanged(crate::openrouter::KeyStatus::Missing));
                });
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(
                    view.openrouter_setup.read(cx).key_status(),
                    Some(&crate::openrouter::KeyStatus::Missing),
                );
            });
        });
    }

    #[test]
    fn shortcut_side_changes_reject_overlap() {
        let settings = AppSettings {
            paste_last_hotkey: Some(HotkeyBinding {
                modifiers: HotkeyModifiers {
                    option: Some(ModifierSide::Right),
                    ..Default::default()
                },
                key: None,
            }),
            ..AppSettings::default()
        };
        // Left Option does not overlap Right Option; Either would.
        assert!(
            hotkey_side_binding(&settings, HotkeyKind::Dictation, ModifierSide::Left).is_some()
        );
        assert!(
            hotkey_side_binding(&settings, HotkeyKind::Dictation, ModifierSide::Right).is_none()
        );
        assert!(
            hotkey_side_binding(&settings, HotkeyKind::Dictation, ModifierSide::Either).is_none()
        );
    }

    #[test]
    fn side_selection_only_changes_a_standalone_modifier() {
        let mut binding = HotkeyBinding::default();
        set_standalone_modifier_side(&mut binding, ModifierSide::Right);
        assert_eq!(binding.modifiers.option, Some(ModifierSide::Right));
        let mut chord = HotkeyBinding::paste_last_default();
        let before = chord.clone();
        set_standalone_modifier_side(&mut chord, ModifierSide::Left);
        assert_eq!(chord, before);
        assert_eq!(standalone_modifier_side(&chord), None);
    }

    #[test]
    fn hotkey_capture_preserves_the_function_modifier() {
        let modifiers = hotkey_modifiers_with_flags(
            GpuiModifiers {
                function: true,
                ..Default::default()
            },
            0,
        );
        assert!(modifiers.function);
        assert!(is_function_key("F5"));
        assert!(!is_function_key("F21"));
        assert!(!is_function_key("Fn"));
    }

    #[test]
    fn toggle_spring_is_frame_rate_independent_and_settles() {
        let simulate = |frame_rate: u32| {
            let mut spring = ToggleSpring::new(false);
            spring.set_enabled(true);
            for _ in 0..frame_rate {
                spring.advance(Duration::from_secs_f32(1.0 / frame_rate as f32));
            }
            spring
        };
        let at_60_hz = simulate(60);
        let at_120_hz = simulate(120);
        assert!(at_60_hz.is_settled());
        assert!(at_120_hz.is_settled());
        assert!((at_60_hz.position - at_120_hz.position).abs() < 0.001);
        assert_eq!(at_60_hz.position, 1.0);
        assert!(at_60_hz.enabled());
    }

    #[test]
    fn inactive_hotkey_controls_keep_their_idle_width() {
        assert_eq!(hotkey_idle_width(0), HOTKEY_MIN_WIDTH);
        assert!(hotkey_idle_width(3) > hotkey_idle_width(1));
        assert!(hotkey_capture_width(2) > hotkey_saved_width(2));
    }

    #[test]
    fn reopening_targets_models_for_a_missing_key_and_settings_for_permissions() {
        let ready = SetupStatus {
            microphone: PermissionState::Ready,
            input_monitoring: PermissionState::Ready,
            accessibility: PermissionState::Ready,
            api_key: true,
        };
        assert_eq!(Pane::History.on_reopen(ready), Pane::History);
        assert_eq!(Pane::Models.on_reopen(ready), Pane::Models);
        assert_eq!(
            Pane::History.on_reopen(SetupStatus {
                api_key: false,
                ..ready
            }),
            Pane::Models
        );
        assert_eq!(
            Pane::Statistics.on_reopen(SetupStatus {
                accessibility: PermissionState::NeedsSettings,
                ..ready
            }),
            Pane::Settings
        );
        assert_eq!(
            Pane::Models.on_reopen(SetupStatus {
                api_key: false,
                microphone: PermissionState::NeedsRequest,
                ..ready
            }),
            Pane::Settings
        );
    }
}
