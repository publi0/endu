//! The app window: Settings, Models, HUD, History, and Statistics, plus first-run
//! setup sheet.

use crate::i18n::t;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, Div, Entity, FocusHandle, FontWeight, IntoElement,
    KeyDownEvent, Modifiers as GpuiModifiers, ModifiersChangedEvent, MouseDownEvent,
    PathPromptOptions, Render, SharedString, Subscription, Timer, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions, actions, div, prelude::*, px, rgba, size,
};

use crate::app_settings::{
    AppSettings, DictationMode, HotkeyBinding, HotkeyKey, HotkeyModifiers, ModifierSide,
    RecordingAudioBehavior,
};
use crate::desktop_ui::{
    ACCENT, CANVAS, CONTROL_RADIUS, FAINT, LINE, MUTED, NEGATIVE, NavigationIcon,
    PANE_CONTENT_WIDTH, PANEL_RADIUS, POSITIVE, PickerState, SIDEBAR_WIDTH, SURFACE, SURFACE_HOVER,
    SURFACE_SELECTED, TEXT, TEXT_SOFT, compact_button, compact_panel, disclosure_button,
    error_message, header_button, hotkey_keycaps, mix_color, navigation_item, pane_body,
    pane_content, pane_header, pane_header_with_action, pane_list, picker_open_key, picker_popup,
    rgb, section_label, settings_copy, settings_panel, settings_row, settings_section_label,
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
use crate::post_processing_view::{PostProcessingChange, PostProcessingView};
use crate::recording_recovery::{RecordingRecovery, RecoveryEntry, RecoveryStatus};
use crate::sound_settings_view::{
    SoundEvent, SoundSettingsView, SoundVolumeChange, StartCueChange, StartCuePreview,
};
use crate::text_input::{
    Changed as TextChanged, Dismissed as TextDismissed, EditFinished as TextEditFinished,
    Submitted as TextSubmitted, TextInput,
};

mod history_pane;
mod hotkey_pane;
mod microphone_pane;
mod preferences_pane;
mod setup_pane;

use history_pane::*;
use hotkey_pane::*;

const WINDOW_WIDTH: f32 = 880.0;
const WINDOW_HEIGHT: f32 = 640.0;
/// The window's last position, kept in the app's user defaults rather than the
/// exported settings file. Its size is fixed.
const WINDOW_FRAME_KEY: &str = "HexMainWindowFrame";
const HOTKEY_MIN_WIDTH: f32 = 148.0;
const HOTKEY_SIDE_SELECTOR_WIDTH: f32 = 150.0;
const PERMISSION_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// How often the sidebar footer re-reads the installed bundle version.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

actions!(
    hex,
    [
        CloseWindow,
        HideApplication,
        MinimizeWindow,
        QuitApplication,
        ShowAbout,
        ShowHistory,
        ShowModels,
        ShowProviders,
        ShowMicrophone,
        ShowPostProcessing,
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
    let bounds = (!preview_mode)
        .then(saved_window_bounds)
        .flatten()
        .filter(|bounds| {
            // A frame from a disconnected display would open off screen.
            cx.displays()
                .iter()
                .any(|display| display.bounds().contains(&bounds.center()))
        })
        .unwrap_or_else(|| Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx));
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Endu".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            is_resizable: false,
            is_minimizable: true,
            tabbing_identifier: preview_mode.then(|| "hex-preview".into()),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| AppWindow::new(listener_start, history, preview, window, cx)),
    )?;
    *app_window.borrow_mut() = Some(handle);
    cx.activate(true);
    Ok(handle)
}

fn saved_window_bounds() -> Option<Bounds<gpui::Pixels>> {
    let defaults = objc2_foundation::NSUserDefaults::standardUserDefaults();
    let value = defaults.stringForKey(&objc2_foundation::NSString::from_str(WINDOW_FRAME_KEY))?;
    parse_window_frame(&value.to_string())
}

fn save_window_bounds(bounds: Bounds<gpui::Pixels>) {
    let value = objc2_foundation::NSString::from_str(&format_window_frame(bounds));
    let defaults = objc2_foundation::NSUserDefaults::standardUserDefaults();
    unsafe {
        defaults.setObject_forKey(
            Some(&value),
            &objc2_foundation::NSString::from_str(WINDOW_FRAME_KEY),
        )
    };
}

fn format_window_frame(bounds: Bounds<gpui::Pixels>) -> String {
    format!(
        "{},{}",
        f32::from(bounds.origin.x).round(),
        f32::from(bounds.origin.y).round()
    )
}

/// Restores the saved position at the fixed window size. Hex 3.5.0 also saved
/// a size; only its position is kept.
fn parse_window_frame(value: &str) -> Option<Bounds<gpui::Pixels>> {
    let values: Vec<f32> = value
        .split(',')
        .map(|part| {
            part.trim()
                .parse()
                .ok()
                .filter(|value: &f32| value.is_finite())
        })
        .collect::<Option<_>>()?;
    let (&[x, y] | &[x, y, _, _]) = values.as_slice() else {
        return None;
    };
    Some(Bounds::new(
        gpui::point(px(x), px(y)),
        size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewPane {
    Settings,
    Microphone,
    Models,
    Providers,
    PostProcessing,
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
    /// Previews never read the saved Appearance; this applies instead.
    pub appearance: crate::appearance::Appearance,
    pub language: crate::i18n::LanguagePreference,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Pane {
    #[default]
    Settings,
    Microphone,
    Models,
    Providers,
    PostProcessing,
    Hud,
    History,
    Statistics,
}

impl Pane {
    const ALL: [Self; 8] = [
        Self::Settings,
        Self::Microphone,
        Self::Providers,
        Self::Models,
        Self::PostProcessing,
        Self::Hud,
        Self::History,
        Self::Statistics,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Settings => t("Settings"),
            Self::Microphone => t("Microphone"),
            Self::Models => t("Models"),
            Self::Providers => t("Providers"),
            Self::PostProcessing => t("Post-processing"),
            Self::Hud => "HUD",
            Self::History => t("History"),
            Self::Statistics => t("Statistics"),
        }
    }

    fn icon(self) -> NavigationIcon {
        match self {
            Self::Settings => NavigationIcon::Settings,
            Self::Microphone => NavigationIcon::Microphone,
            Self::Models => NavigationIcon::Models,
            Self::Providers => NavigationIcon::Providers,
            Self::PostProcessing => NavigationIcon::PostProcessing,
            Self::Hud => NavigationIcon::Hud,
            Self::History => NavigationIcon::History,
            Self::Statistics => NavigationIcon::Statistics,
        }
    }

    fn on_reopen(self, status: SetupStatus) -> Self {
        if !crate::onboarding::permission_warnings(status).is_empty() {
            Self::Settings
        } else if !status.api_key {
            Self::Providers
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
        if crate::desktop_ui::reduce_motion() {
            self.position = self.target;
            self.velocity = 0.0;
            self.last_frame = Instant::now();
            return self.position;
        }
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
    StartCue,
    Retention,
    Trim,
    Hud,
    MicrophonePriority,
    DoubleTapSensitivity,
    PostProcessing,
    Appearance,
    Language,
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
    /// Counts each permission becoming ready while Endu runs, so its check
    /// draws itself then rather than on every visit.
    setup_granted: [u64; 3],
    setup_visible: bool,
    onboarding_completed: bool,
    permission_refresh_at: Instant,
    settings: AppSettings,
    settings_error: Option<String>,
    settings_feedback: Option<SettingsFeedback>,
    hud_settings: Entity<HudSettingsView>,
    post_processing_view: Entity<PostProcessingView>,
    sound_settings: Entity<SoundSettingsView>,
    microphone_priority: Entity<MicrophonePriorityView>,
    sensitivity_focus: [FocusHandle; 3],
    appearance_focus: [FocusHandle; 3],
    language_focus: [FocusHandle; 4],
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
    providers: Entity<crate::providers_view::ProvidersView>,
    model_options: Entity<crate::model_options_view::ModelOptionsView>,
    shared_keywords: Entity<crate::vocabulary_view::VocabularyView>,
    statistics: Entity<StatisticsView>,
    hotkey_capture: HotkeyCaptureState,
    hotkey_capture_animation: ToggleSpring,
    hotkey_width_spring: ToggleSpring,
    /// The control whose capture the width spring is animating.
    hotkey_width_origin: Option<HotkeyKind>,
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
    /// Store revisions behind the loaded lists; polling reloads only when they move.
    history_loaded: Option<((u64, usize), u64)>,
    recovery: Option<RecordingRecovery>,
    recovery_entries: Vec<RecoveryEntry>,
    selected_recovery: Option<String>,
    recovery_delete_armed: bool,
    /// The failed recording a Retry or Delete error belongs to, and the error.
    recovery_error: Option<(String, String)>,
    /// A Retry that copies its recovered text once the worker finishes.
    recovery_copy_pending: Option<String>,
    recovery_action_focus: [FocusHandle; 3],
    selected_history: Option<u64>,
    /// The entry last opened, highlighted in the list after going back.
    history_last_opened: Option<HistoryItem>,
    history_scroll: gpui::ScrollHandle,
    history_error: Option<String>,
    history_clear_armed: bool,
    /// The list row whose trash was clicked once and now awaits confirmation.
    history_row_delete_armed: Option<history_pane::HistoryItem>,
    /// Counts arming clicks so each one shakes the trash anew.
    history_delete_armed_count: u64,
    /// A row folding away; it is deleted once the fold finishes.
    history_removing: Option<history_pane::HistoryItem>,
    /// When the copy check appeared; it returns to the copy symbol after a moment.
    history_copied_at: Option<Instant>,
    history_copied_count: u64,
    /// A recording that Retry just recovered, so its text can fade in.
    history_recovered: Option<(String, Instant)>,
    history_retention_open: bool,
    history_retention_picker_state: PickerState,
    history_copied: Option<HistoryItem>,
    pending_update: Option<String>,
    update_error: Option<String>,
    update_restarting: bool,
    update_check_at: Option<Instant>,
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
                        | window.poll_history_feedback()
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
                        Some(tf!(
                            "Could not load app settings: {error}",
                            error = format!("{error:#}")
                        )),
                    )
                }
            }
        };
        if !preview_mode {
            crate::app_settings::set_dock_icon_visible(true);
        }
        preview
            .as_ref()
            .map_or(settings.appearance, |preview| preview.appearance)
            .apply_to_application();
        crate::i18n::set_language(
            preview
                .as_ref()
                .map_or(settings.language, |preview| preview.language)
                .resolve(),
        );
        crate::desktop_ui::sync_appearance(native_window.appearance());
        let window_focus = cx.focus_handle();
        window_focus.focus(native_window);
        let hotkey_focus = cx.focus_handle();
        let closing = cx.weak_entity();
        native_window.on_window_should_close(cx, move |_, cx| {
            let _ = closing.update(cx, |this, cx| this.finish_editing(cx));
            true
        });
        let mut subscriptions = vec![
            // The palette follows the effective appearance: the Appearance
            // setting, or the system while it is System.
            cx.observe_window_appearance(native_window, |_, window, cx| {
                if crate::desktop_ui::sync_appearance(window.appearance()) {
                    window.refresh();
                }
                cx.notify();
            }),
            cx.observe_window_bounds(native_window, move |_, window, _| {
                if !preview_mode && let WindowBounds::Windowed(bounds) = window.window_bounds() {
                    save_window_bounds(bounds);
                }
            }),
            cx.on_app_quit(|this, cx| {
                this.finish_editing(cx);
                async {}
            }),
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
        let lower_volume_input = cx.new(|cx| {
            TextInput::new(cx, "80", settings.lower_volume_percent.to_string()).commit_on_blur()
        });
        subscriptions.push(
            cx.subscribe(&lower_volume_input, |this, _, _: &TextEditFinished, cx| {
                this.save_lower_volume(cx)
            }),
        );
        subscriptions.push(
            cx.subscribe(&lower_volume_input, |this, _, _: &TextDismissed, cx| {
                this.lower_volume_input.update(cx, |input, cx| {
                    input.set_text(this.settings.lower_volume_percent.to_string(), cx)
                });
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
        let history_search = cx
            .new(|cx| TextInput::picker(cx, "", "").localized_placeholder(|| t("Search history")));
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
        let providers = cx.new(|cx| crate::providers_view::ProvidersView::new(preview_mode, cx));
        let config = openrouter_settings.read(cx).config_snapshot();
        if preview_mode {
            // Keep the chain fixture in Providers; no real credential is read.
            providers.update(cx, |view, cx| view.refresh(config.clone(), cx));
        }
        let model_options =
            cx.new(|cx| crate::model_options_view::ModelOptionsView::new(config, preview_mode, cx));
        let available = providers.read(cx).available_providers(cx);
        openrouter_settings.update(cx, |view, cx| {
            view.set_available_providers(available.clone(), cx)
        });
        model_options.update(cx, |view, cx| view.set_available_providers(available, cx));
        let shared_keywords = cx.new(|cx| {
            crate::vocabulary_view::VocabularyView::for_models(
                settings.vocabulary.clone(),
                preview_mode,
                cx,
            )
        });
        shared_keywords.update(cx, |view, cx| {
            view.set_models(&openrouter_settings.read(cx).config_snapshot(), cx)
        });
        subscriptions.push(cx.subscribe(
            &shared_keywords,
            |this, _, change: &crate::vocabulary_view::VocabularyChange, cx| {
                this.update_vocabulary(change.0.clone(), cx);
            },
        ));
        subscriptions.push(cx.observe(&shared_keywords, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(
            &openrouter_settings,
            |this, editor, _: &crate::openrouter::settings_view::ConfigChanged, cx| {
                let config = editor.read(cx).config_snapshot();
                this.synchronize_model_config(config, cx);
            },
        ));
        subscriptions.push(cx.subscribe(
            &model_options,
            |this, editor, _: &crate::openrouter::settings_view::ConfigChanged, cx| {
                let config = editor.read(cx).config_snapshot();
                this.synchronize_model_config(config, cx);
            },
        ));
        subscriptions.push(cx.observe(&model_options, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(
            &providers,
            |this, _, _: &crate::providers_view::ProvidersChanged, cx| {
                let available = this.providers.read(cx).available_providers(cx);
                this.openrouter_settings.update(cx, |view, cx| {
                    view.set_available_providers(available.clone(), cx)
                });
                this.model_options
                    .update(cx, |view, cx| view.set_available_providers(available, cx));
                let config = this.providers.read(cx).config_snapshot();
                this.synchronize_model_config(config, cx);
                this.poll_setup(true);
                cx.notify();
            },
        ));
        let hud_settings = cx.new(|cx| HudSettingsView::new(settings.hud, preview_mode, cx));
        let sound_settings = cx.new(|cx| {
            SoundSettingsView::new(settings.effective_sound_volumes(), settings.start_cue, cx)
        });
        let microphone_priority = cx.new(|cx| {
            MicrophonePriorityView::new(settings.microphone_priority.clone(), preview_mode, cx)
        });
        let post_processing_view = cx.new(|cx| {
            PostProcessingView::new(
                settings.post_processing,
                settings.vocabulary.clone(),
                preview_mode,
                cx,
            )
        });
        subscriptions.push(cx.observe(&post_processing_view, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(
            &post_processing_view,
            |this, _, change: &crate::vocabulary_view::VocabularyChange, cx| {
                this.update_vocabulary(change.0.clone(), cx);
            },
        ));
        subscriptions.push(cx.subscribe(
            &post_processing_view,
            |this, _, change: &PostProcessingChange, cx| {
                this.update_settings(SettingControl::PostProcessing, cx, |settings| {
                    settings.post_processing = change.0
                });
                let preferences = this.settings.post_processing;
                let error = this.feedback_error(SettingControl::PostProcessing);
                this.post_processing_view
                    .update(cx, |view, cx| view.set_preferences(preferences, error, cx));
            },
        ));
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
            &sound_settings,
            |this, _, change: &StartCueChange, cx| {
                let saved = this.update_settings(SettingControl::StartCue, cx, |settings| {
                    settings.start_cue = change.0
                });
                let cue = this.settings.start_cue;
                let error = this.feedback_error(SettingControl::StartCue);
                this.sound_settings
                    .update(cx, |view, cx| view.set_start_cue(cue, error, cx));
                // Choosing a cue previews it, as Play does.
                if saved {
                    crate::feedback::preview_start_cue();
                }
            },
        ));
        subscriptions.push(
            cx.subscribe(&sound_settings, |_, _, _: &StartCuePreview, _| {
                crate::feedback::preview_start_cue()
            }),
        );
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
        subscriptions.push(cx.observe(&openrouter_settings, |this, editor, cx| {
            let models = editor.read(cx).catalog_models();
            if this.model_options.read(cx).catalog() != models {
                let models = models.to_vec();
                this.model_options
                    .update(cx, |view, cx| view.set_catalog(models, cx));
            }
            cx.notify();
        }));
        subscriptions.push(
            cx.subscribe(&openrouter_setup, |this, _, event: &KeyChanged, cx| {
                this.providers.update(cx, |view, cx| {
                    view.sync_key_status(
                        crate::providers::Provider::OpenRouter,
                        event.0.clone(),
                        cx,
                    )
                });
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
                Some(PreviewPane::Providers) => Pane::Providers,
                Some(PreviewPane::PostProcessing) => Pane::PostProcessing,
                Some(PreviewPane::Microphone) => Pane::Microphone,
                Some(PreviewPane::Hud) => Pane::Hud,
                Some(PreviewPane::History) => Pane::History,
                Some(PreviewPane::Statistics) => Pane::Statistics,
                Some(PreviewPane::Settings) | None => Pane::Settings,
            },
            listener_start,
            setup_status,
            setup_granted: [0; 3],
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
            providers,
            model_options,
            shared_keywords,
            statistics: cx.new(|_| StatisticsView::new(preview_mode)),
            hotkey_capture: HotkeyCaptureState::Idle,
            hotkey_capture_animation: ToggleSpring::new(false),
            hotkey_width_spring: ToggleSpring::at(HOTKEY_MIN_WIDTH),
            hotkey_width_origin: None,
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
            post_processing_view,
            sound_settings,
            microphone_priority,
            sensitivity_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            appearance_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            language_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            preference_transfer_busy: false,
            preference_transfer_error: None,
            preference_transfer_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            history,
            history_search,
            history_entries: Vec::new(),
            history_loaded: None,
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
            recovery_error: None,
            recovery_copy_pending: None,
            recovery_action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            selected_history: None,
            history_last_opened: None,
            history_scroll: gpui::ScrollHandle::new(),
            history_error: None,
            history_clear_armed: false,
            history_row_delete_armed: None,
            history_delete_armed_count: 0,
            history_removing: None,
            history_copied_at: None,
            history_copied_count: 0,
            history_recovered: None,
            history_retention_open: preview
                .as_ref()
                .is_some_and(|preview| preview.open_history_retention),
            history_retention_picker_state: PickerState::new(cx),
            history_copied: None,
            pending_update: None,
            update_error: None,
            update_restarting: false,
            update_check_at: None,
        };
        window.history_retention_picker_state.highlight = HistoryRetention::ALL
            .iter()
            .position(|value| *value == window.settings.history_retention)
            .unwrap_or(0);
        window.reload_history(cx);
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
        if self.pane == Pane::Models && pane != Pane::Models {
            self.finish_model_options(cx);
        }
        self.model_options
            .update(cx, |view, cx| view.close_pickers(cx));
        self.providers.update(cx, |view, cx| view.close_pickers(cx));
        self.cancel_hotkey_capture(cx);
        self.openrouter_settings
            .update(cx, |view, cx| view.close_pickers(cx));
        self.microphone_priority
            .update(cx, |view, cx| view.close_picker(cx));
        self.hud_settings
            .update(cx, |view, cx| view.close_picker(cx));
        if pane != Pane::History {
            // Copying long after leaving History would surprise; Copy remains.
            self.recovery_copy_pending = None;
        }
        if self.pane != pane {
            crate::desktop_ui::note_navigation();
        }
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
            Pane::Settings
            | Pane::Microphone
            | Pane::Models
            | Pane::Providers
            | Pane::PostProcessing
            | Pane::Hud => self.permission_refresh_at = Instant::now(),
        }
        cx.notify();
    }

    pub(crate) fn focus_pane(&self, window: &mut Window) {
        self.window_focus.focus(window);
    }

    pub(crate) fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Settings, cx);
    }

    pub(crate) fn show_microphone(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Microphone, cx);
    }

    pub(crate) fn show_providers(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Providers, cx);
    }

    pub(crate) fn show_models(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Models, cx);
    }

    pub(crate) fn show_post_processing(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::PostProcessing, cx);
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
                        && !matches!(
                            self.pane,
                            Pane::Settings | Pane::Microphone | Pane::Models | Pane::Providers
                        )
                        && self.setup_status.api_key)
        {
            return false;
        }
        self.permission_refresh_at = Instant::now() + PERMISSION_REFRESH_INTERVAL;
        let mut changed = false;
        let status = crate::onboarding::status();
        if status != self.setup_status {
            let permissions = |status: crate::onboarding::SetupStatus| {
                [
                    status.microphone,
                    status.input_monitoring,
                    status.accessibility,
                ]
                .map(|state| state == crate::onboarding::PermissionState::Ready)
            };
            let (before, after) = (permissions(self.setup_status), permissions(status));
            for (index, count) in self.setup_granted.iter_mut().enumerate() {
                if after[index] && !before[index] {
                    *count += 1;
                }
            }
            // The last permission granted: the wordmark waves once.
            if after.iter().all(|ready| *ready) && !before.iter().all(|ready| *ready) {
                crate::desktop_ui::play_wordmark();
            }
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
                .map(|error| {
                    tf!(
                        "Could not save settings: {error}",
                        error = format!("{error:#}")
                    )
                })
                .unwrap_or_else(|| t("Saved.").into()),
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
                message: t("Enter a percentage from 0 to 100.").into(),
            });
            cx.notify();
            return;
        };
        if self.update_settings(SettingControl::LowerVolume, cx, |settings| {
            settings.lower_volume_percent = percent;
        }) {
            self.lower_volume_input
                .update(cx, |input, cx| input.set_text(percent.to_string(), cx));
        }
    }

    /// Flush valid drafts before a window or app exit removes its focus tree.
    fn update_vocabulary(
        &mut self,
        vocabulary: crate::vocabulary::Vocabulary,
        cx: &mut Context<Self>,
    ) {
        self.update_settings(SettingControl::PostProcessing, cx, |settings| {
            settings.vocabulary = vocabulary
        });
        let accepted = self.settings.vocabulary.clone();
        let error = self.feedback_error(SettingControl::PostProcessing);
        if error.is_none() && !self.preview && accepted.remote_hints && !accepted.terms.is_empty() {
            crate::openrouter::vocabulary_support::schedule(false);
        }
        self.shared_keywords.update(cx, |view, cx| {
            view.set_preferences(accepted.clone(), error.clone(), cx)
        });
        self.post_processing_view
            .update(cx, |view, cx| view.set_vocabulary(accepted, error, cx));
    }

    pub(crate) fn finish_editing(&mut self, cx: &mut Context<Self>) {
        self.finish_model_options(cx);
        if let Some(candidate) = self.shared_keywords.update(cx, |view, cx| view.pending(cx)) {
            match candidate {
                Ok(candidate) => self.update_vocabulary(candidate, cx),
                Err(error) => {
                    let accepted = self.settings.vocabulary.clone();
                    self.shared_keywords.update(cx, |view, cx| {
                        view.set_preferences(accepted, Some(error), cx)
                    });
                }
            }
        }
        self.providers
            .update(cx, |view, cx| view.finish_editing(cx));
        let config = self.providers.read(cx).config_snapshot();
        self.synchronize_model_config(config, cx);
        let pending = self
            .post_processing_view
            .update(cx, |view, cx| view.pending_vocabulary(cx));
        if let Some(candidate) = pending {
            match candidate {
                Ok(candidate) => self.update_vocabulary(candidate, cx),
                Err(error) => {
                    let accepted = self.settings.vocabulary.clone();
                    self.post_processing_view.update(cx, |view, cx| {
                        view.set_vocabulary(accepted, Some(error), cx)
                    });
                }
            }
        }

        if self.lower_volume_input.read(cx).has_pending_edit() {
            self.save_lower_volume(cx);
        }
        if let Some(distance) = self.hud_settings.read(cx).pending_distance(cx) {
            self.update_settings(SettingControl::Hud, cx, |settings| {
                settings.hud.edge_distance = distance
            });
            let preferences = self.settings.hud;
            let error = self.feedback_error(SettingControl::Hud);
            self.hud_settings
                .update(cx, |view, cx| view.set_preferences(preferences, error, cx));
        }
        self.openrouter_settings
            .update(cx, |view, cx| view.finish_editing(cx));
        self.openrouter_setup
            .update(cx, |view, cx| view.finish_editing(cx));
    }

    fn render_lower_volume_row(&self) -> Option<AnyElement> {
        (self.settings.recording_audio_behavior == RecordingAudioBehavior::LowerVolume).then(|| {
            self.setting_row(
                SettingControl::LowerVolume,
                t("Volume while dictating (%)"),
                t("Percentage of the previous volume to keep. Applies to the next dictation; the original level returns when you stop."),
                div().flex_none().w(px(crate::desktop_ui::NUMBER_INPUT_WIDTH))
                    .child(self.lower_volume_input.clone()),
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
            .border_color(rgb(crate::desktop_ui::DIVIDER))
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
            .pt(px(44.0))
            .pb_4()
            .flex()
            .flex_col()
            .child(crate::desktop_ui::sidebar_brand())
            .child(div().flex().flex_col().gap(px(2.0)).children(items))
            .child(div().flex_1())
            .child(self.render_update_footer(cx))
            .into_any_element()
    }

    /// The sidebar footer shows the running version, or a restart action when
    /// Homebrew has placed a newer bundle on disk.
    fn render_update_footer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // Reading the bundle plist is cheap but not free; re-check on a slow
        // cadence instead of every render frame.
        if self.update_check_at.is_none_or(|at| Instant::now() >= at) {
            let pending = crate::update_check::pending_update();
            crate::status_item::set_pending_update(pending.clone());
            self.pending_update = pending;
            self.update_check_at = Some(Instant::now() + UPDATE_CHECK_INTERVAL);
        }
        let Some(version) = self.pending_update.clone() else {
            return div()
                .flex_none()
                .pl_2()
                .h(px(30.0))
                .flex()
                .items_center()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(format!("Endu {}", env!("CARGO_PKG_VERSION")))
                .into_any_element();
        };
        let restarting = self.update_restarting;
        div()
            .flex_none()
            .p_3()
            .rounded(px(PANEL_RADIUS))
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(SURFACE))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(7.0)).rounded_full().bg(rgb(ACCENT)))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(t("Update ready")),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(rgb(MUTED))
                    .child(tf!("Endu {version} is installed. Restart to start using it.", version = version)),
            )
            .child(
                div()
                    .id("update-restart")
                    .h(px(28.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(CONTROL_RADIUS))
                    .bg(rgb(ACCENT))
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(crate::desktop_ui::ON_ACCENT))
                    .when(restarting, |button| button.opacity(0.6))
                    .when(!restarting, |button| {
                        button
                            .hover(|button| button.opacity(0.88))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(bundle) = crate::update_check::bundle_path() else {
                                    this.update_error = Some(
                                    t("Endu is not running from its app bundle. Reopen it manually.")
                                        .into(),
                                );
                                    cx.notify();
                                    return;
                                };
                                this.finish_editing(cx);
                                this.update_error = None;
                                this.update_restarting =
                                    crate::update_check::relaunch_and_quit(&bundle);
                                if !this.update_restarting {
                                    this.update_error = Some(
                                        t("Could not schedule the restart. Quit Endu and reopen it.")
                                            .into(),
                                    );
                                }
                                cx.notify();
                            }))
                    })
                    .child(if restarting {
                        t("Restarting…")
                    } else {
                        t("Restart now")
                    }),
            )
            .children(self.update_error.as_ref().map(|error| {
                div()
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(rgb(NEGATIVE))
                    .child(error.clone())
            }))
            .into_any_element()
    }

    /// Normal cross-pane edits rebase editor state while keeping unrelated
    /// drafts. Only an explicit import resets drafts and picker state.
    fn synchronize_model_config(
        &mut self,
        config: crate::openrouter::Config,
        cx: &mut Context<Self>,
    ) {
        if self.openrouter_settings.read(cx).config_snapshot() != config {
            self.openrouter_settings
                .update(cx, |view, cx| view.refresh_config(config.clone(), cx));
        }
        if self.model_options.read(cx).config_snapshot() != config {
            self.model_options
                .update(cx, |view, cx| view.refresh(config.clone(), cx));
        }
        if self.providers.read(cx).config_snapshot() != config {
            self.providers
                .update(cx, |view, cx| view.refresh(config.clone(), cx));
        }
        self.shared_keywords
            .update(cx, |view, cx| view.set_models(&config, cx));
        cx.notify();
    }

    fn finish_model_options(&mut self, cx: &mut Context<Self>) {
        let config = self.openrouter_settings.read(cx).config_snapshot();
        if self.model_options.read(cx).config_snapshot() != config {
            self.model_options
                .update(cx, |view, cx| view.refresh(config, cx));
        }
        self.model_options.update(cx, |view, cx| {
            view.finish_editing(cx);
        });
        // Effects from ConfigChanged are delivered after this callback. Rebase
        // synchronously before another editor saves its own draft.
        let config = self.model_options.read(cx).config_snapshot();
        self.synchronize_model_config(config, cx);
    }

    fn render_models(&self, cx: &Context<Self>) -> AnyElement {
        let available = self.providers.read(cx).available_providers(cx);
        let mut eligible = self.openrouter_settings.read(cx).config_snapshot();
        eligible
            .transcription
            .models
            .retain(|id| available.contains(&crate::providers::ModelRef::parse(id).provider));
        configuration_pane(
            t("Models"),
            "models-scroll",
            div()
                .child(
                    div()
                        .debug_selector(|| "models-chain".into())
                        .child(self.openrouter_settings.clone()),
                )
                .child(
                    div()
                        .debug_selector(|| "models-options".into())
                        .child(self.model_options.clone()),
                )
                .when(
                    crate::providers::has_keyword_support(&eligible, !self.preview),
                    |pane| {
                        pane.child(
                            div()
                                .debug_selector(|| "models-keywords".into())
                                .child(self.shared_keywords.clone()),
                        )
                    },
                ),
        )
    }

    fn render_clipboard_fallback(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .child(settings_row(
                t("Copy when auto-paste fails"),
                t("Keeps the dictation on the clipboard when Endu detects a paste error or the destination changes."),
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
        let hotkey_control = self.render_hotkey_setting_control(HotkeyKind::Dictation, window, cx);
        let paste_last_control =
            self.render_hotkey_setting_control(HotkeyKind::PasteLast, window, cx);
        let mode_control =
            crate::desktop_ui::settings_choice(
                "dictation-mode",
                DictationMode::ALL
                    .iter()
                    .position(|value| *value == self.settings.dictation_mode),
                DictationMode::ALL.len(),
            )
            .children(DictationMode::ALL.into_iter().enumerate().map(
                |(index, mode)| {
                    crate::desktop_ui::settings_segmented_item(
                        mode == self.settings.dictation_mode,
                        DictationMode::ALL.len(),
                    )
                    .id(("dictation-mode", index))
                    .track_focus(&self.dictation_mode_focus[index])
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .child(mode.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.set_dictation_mode(mode, cx)))
                },
            ));
        let mode_description = match self.settings.dictation_mode {
            DictationMode::TapOrHold => {
                t("Tap to keep recording; press again to stop. Or hold and release to finish.")
            }
            DictationMode::Hold => {
                t("Hold the shortcut while speaking; release to transcribe and paste.")
            }
            DictationMode::DoubleTap => {
                t("Hold to dictate, or double-tap to keep recording until the next press.")
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
        let sensitivity_control =
            crate::desktop_ui::settings_choice(
                "double-tap-sensitivity",
                DoubleTapSensitivity::ALL
                    .iter()
                    .position(|value| *value == self.settings.double_tap_sensitivity),
                DoubleTapSensitivity::ALL.len(),
            )
            .children(DoubleTapSensitivity::ALL.into_iter().enumerate().map(
                |(index, sensitivity)| {
                    crate::desktop_ui::settings_segmented_item(
                        self.settings.double_tap_sensitivity == sensitivity,
                        DoubleTapSensitivity::ALL.len(),
                    )
                    .id(("double-tap-sensitivity", index))
                    .track_focus(&self.sensitivity_focus[index])
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .child(sensitivity.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_settings(
                            SettingControl::DoubleTapSensitivity,
                            cx,
                            |settings| settings.double_tap_sensitivity = sensitivity,
                        );
                    }))
                },
            ));
        let appearance_control = crate::desktop_ui::settings_choice(
            "appearance",
            crate::appearance::Appearance::ALL
                .iter()
                .position(|value| *value == self.settings.appearance),
            crate::appearance::Appearance::ALL.len(),
        )
        .children(
            crate::appearance::Appearance::ALL
                .into_iter()
                .enumerate()
                .map(|(index, appearance)| {
                    crate::desktop_ui::settings_segmented_item(
                        self.settings.appearance == appearance,
                        crate::appearance::Appearance::ALL.len(),
                    )
                    .id(("appearance", index))
                    .track_focus(&self.appearance_focus[index])
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .child(appearance.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.update_settings(SettingControl::Appearance, cx, |settings| {
                            settings.appearance = appearance
                        }) {
                            appearance.apply(cx);
                        }
                    }))
                }),
        );
        let language_control = crate::desktop_ui::settings_choice(
            "language",
            crate::i18n::LanguagePreference::ALL
                .iter()
                .position(|value| *value == self.settings.language),
            crate::i18n::LanguagePreference::ALL.len(),
        )
        .children(
            crate::i18n::LanguagePreference::ALL
                .into_iter()
                .enumerate()
                .map(|(index, language)| {
                    crate::desktop_ui::settings_segmented_item(
                        self.settings.language == language,
                        crate::i18n::LanguagePreference::ALL.len(),
                    )
                    .id(("language", index))
                    .track_focus(&self.language_focus[index])
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .child(language.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.update_settings(SettingControl::Language, cx, |settings| {
                            settings.language = language
                        }) {
                            apply_language(language, cx);
                        }
                    }))
                }),
        );
        let launch_at_login_control = if self.launch_at_login_status.is_none() {
            div()
                .text_size(px(11.0))
                .text_color(rgb(MUTED))
                .child(if self.login_item_worker.is_some() {
                    t("Checking…")
                } else {
                    t("Unavailable")
                })
                .into_any_element()
        } else if self.launch_at_login_status == Some(LoginItemStatus::RequiresApproval) {
            compact_button(t("Open Settings"))
                .id("launch-at-login-approval")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.request_login_item(LoginItemRequest::OpenSettings);
                    cx.notify();
                }))
                .into_any_element()
        } else {
            toggle(launch_at_login_position)
        };
        configuration_pane(
            t("Settings"),
            "settings-scroll",
            div()
                .children(permission_warnings)
                .child(settings_section_label(t("Dictation")))
                .child(
                    settings_panel()
                        .child(self.setting_row(SettingControl::Dictation,
                            t("Dictation shortcut"),
                            t("Start and stop dictation with this shortcut"),
                            hotkey_control,
                        ))
                        .child(
                            self.setting_row(SettingControl::DictationMode,
                                t("Recording gesture"), mode_description, mode_control,
                            ),
                        )
                        .child(
                            div()
                                .h(px(72.0 * double_tap_only_visibility))
                                .overflow_hidden()
                                .opacity(double_tap_only_visibility)
                                .child(
                                    self.setting_row(SettingControl::DoubleTapOnly,
                                        t("Double-tap only"),
                                        t("Wait for two complete shortcut taps before recording"),
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
                                t("Double-tap timing"),
                                tf!("Time between taps: {ms} ms. Choose how quickly the second tap must follow.", ms = self.settings.double_tap_sensitivity.window().as_millis()),
                                sensitivity_control,
                            )
                        ))
                        .child(self.setting_row(SettingControl::EnterSubmit,
                            t("Enter to paste and send"),
                            t("During locked recording, Enter stops, transcribes, pastes, then presses Enter in the input. This can send a chat message."),
                            toggle(if self.settings.enter_to_submit { 1.0 } else { 0.0 }),
                        ).border_b_0().id("enter-to-submit")
                            .track_focus(&self.enter_submit_focus)
                            .focus(|style| style.bg(rgb(SURFACE_HOVER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.update_settings(SettingControl::EnterSubmit, cx, |settings| settings.enter_to_submit = !settings.enter_to_submit);
                            }))),
                )
                .child(settings_section_label(t("Paste last")))
                .child(
                    settings_panel().child(
                        self.setting_row(SettingControl::PasteLast,
                            t("Paste last dictation"),
                            t("Pastes the most recent transcript again; also in the menu bar"),
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(paste_last_control)
                                .when(self.settings.paste_last_hotkey.is_some(), |row| {
                                    row.child(
                                        compact_button(t("Disable"))
                                            .id("disable-paste-last-hotkey")
                                            .flex_none()
                                            .border_1()
                                            .border_color(rgb(LINE))
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
                .child(settings_section_label(t("Application")))
                .child(
                    settings_panel()
                        .child(self.setting_row(
                            SettingControl::Language,
                            t("Language"),
                            t("System follows your Mac's language"),
                            language_control,
                        ))
                        .child(self.setting_row(
                            SettingControl::Appearance,
                            t("Appearance"),
                            t("System follows your Mac's light or dark mode"),
                            appearance_control,
                        ))
                        .child(
                            settings_row(
                                t("Launch at login"),
                                t("Start Endu when you sign in to your Mac"),
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
                                t("Show Dock icon"),
                                t("When off, Endu lives in the menu bar while this window is closed"),
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
                .child(settings_section_label(t("Sounds")))
                .child(self.sound_settings.clone())
                .child(settings_section_label(t("Preferences")))
                .child(self.render_preference_transfer(cx)),
        )
    }
}

/// Switches the interface language at once: the window repaints and the app
/// menus are rebuilt; the menu bar item retitles itself when it next opens.
pub(crate) fn apply_language(language: crate::i18n::LanguagePreference, cx: &mut App) {
    crate::i18n::set_language(language.resolve());
    crate::desktop::set_app_menus(cx);
    cx.refresh_windows();
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
                Pane::Microphone => self.render_microphone(window, cx),
                Pane::Models => self.render_models(cx),
                Pane::Providers => configuration_pane(
                    t("Providers"),
                    "providers-scroll",
                    div().child(self.providers.clone()),
                ),
                Pane::PostProcessing => self.post_processing_view.clone().into_any_element(),
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
                if this.pane == Pane::History
                    && !this.setup_visible
                    && this.history_detail_key(event, window, cx)
                {
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
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                this.finish_editing(cx);
                window.remove_window();
            }))
            .on_action(|_: &MinimizeWindow, window, _| window.minimize_window())
            .on_action(|_: &ToggleFullscreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|this, _: &ShowSettings, window, cx| {
                this.select_pane(Pane::Settings, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowMicrophone, window, cx| {
                this.select_pane(Pane::Microphone, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowProviders, window, cx| {
                this.select_pane(Pane::Providers, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowModels, window, cx| {
                this.select_pane(Pane::Models, cx);
                this.focus_pane(window);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowPostProcessing, window, cx| {
                this.show_post_processing(cx);
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
                                .child(error_message(t("Settings error:"), error)),
                        )
                    })
                    .child(div().flex_1().min_h_0().child(content)),
            )
            .children(setup)
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

#[cfg(test)]
mod tests {
    use super::preferences_pane::write_preferences_export_to;
    use super::*;
    use gpui::Focusable;

    #[test]
    fn saved_window_positions_round_trip_at_the_fixed_size() {
        let bounds = Bounds::new(
            gpui::point(px(120.0), px(80.0)),
            size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
        );
        assert_eq!(format_window_frame(bounds), "120,80");
        assert_eq!(parse_window_frame("120,80"), Some(bounds));
        // A 3.5.0 frame keeps its position but never its size.
        assert_eq!(parse_window_frame("120,80,1400,1000"), Some(bounds));
        for invalid in ["", "1", "1,2,3", "1,2,3,4,5", "a,b", "NaN,0"] {
            assert_eq!(parse_window_frame(invalid), None, "{invalid}");
        }
    }

    fn preview_fixture(window: &mut Window, cx: &mut Context<AppWindow>) -> AppWindow {
        AppWindow::new(
            None,
            None,
            Some(AppWindowPreview {
                pane: PreviewPane::Settings,
                onboarding: false,
                permissions_missing: false,
                open_history_retention: false,
                appearance: crate::appearance::Appearance::System,
                language: crate::i18n::LanguagePreference::English,
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
                    view.model_options.read(cx).config_snapshot().transcription,
                    models.transcription
                );
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
        cx.simulate_resize(size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)));
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
                bounds.left() >= px(SIDEBAR_WIDTH) && bounds.right() <= px(WINDOW_WIDTH),
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
    fn microphone_pane_owns_input_controls_without_resetting_settings(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        assert!(cx.debug_bounds("microphone-setting").is_none());
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_microphone(cx)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("microphone-setting").is_some());
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.pane, Pane::Microphone);
                view.toggle_trim_silence(cx);
                assert!(!view.openrouter_settings.read(cx).trim_silence());
                view.show_settings(cx);
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("microphone-setting").is_none());
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.show_microphone(cx);
                assert!(!view.openrouter_settings.read(cx).trim_silence());
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("microphone-setting").is_some());
    }

    #[gpui::test]
    fn microphone_and_channel_choices_are_keyboard_accessible(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_microphone(cx)));
        cx.run_until_parked();
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
    fn lower_volume_saves_on_focus_change_without_a_button(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_microphone(cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.settings.recording_audio_behavior = RecordingAudioBehavior::LowerVolume;
                cx.notify();
            })
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.read(cx)
                .lower_volume_input
                .focus_handle(cx)
                .focus(window)
        });
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("35");
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(view.read(cx).settings.lower_volume_percent, 35));
        cx.update(|window, cx| {
            view.read(cx)
                .lower_volume_input
                .focus_handle(cx)
                .focus(window)
        });
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("65");
        assert!(cx.simulate_close());
        cx.cx
            .read(|cx| assert_eq!(view.read(cx).settings.lower_volume_percent, 65));
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
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_microphone(cx)));
        cx.run_until_parked();
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
                .transcribe_original(
                    &[0.1; 1600],
                    Some("Notes"),
                    crate::post_processing::Preferences::default(),
                    |_| Err(color_eyre::eyre::eyre!("offline fixture"))
                )
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
    fn model_options_live_between_the_chain_and_keywords_only_in_models(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.simulate_resize(size(px(WINDOW_WIDTH), px(2400.0)));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.openrouter_settings.read(cx).config_snapshot();
                config.transcription.models = vec!["openai::gpt-4o-transcribe".into()];
                view.synchronize_model_config(config, cx);
                view.show_models(cx);
            })
        });
        cx.run_until_parked();
        let chain = cx.debug_bounds("models-chain").unwrap();
        let options = cx.debug_bounds("models-options").unwrap();
        let keywords = cx.debug_bounds("models-keywords").unwrap();
        assert!(chain.bottom() <= options.top());
        assert!(options.bottom() <= keywords.top());
        assert!(cx.debug_bounds("model-options-context").is_some());
        assert!(
            cx.debug_bounds("providers-credentials-and-limits")
                .is_none()
        );
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_providers(cx)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("providers-credentials-and-limits")
                .is_some()
        );
        assert!(cx.debug_bounds("models-options").is_none());
        assert!(cx.debug_bounds("model-options-context").is_none());
    }

    /// Every probed control and panel must stay horizontally inside each
    /// panel or content column it overlaps. A control pushed past a clipping
    /// edge is cut off instead of wrapping or shrinking its neighbours.
    fn assert_layout_probes_fit(cx: &mut gpui::VisualTestContext, scene: &str) {
        use crate::desktop_ui::{LayoutProbe, layout_probes};
        let probes: Vec<_> = layout_probes::take()
            .into_iter()
            .filter_map(|(name, kind)| {
                let name: &'static str = Box::leak(name.into_boxed_str());
                cx.debug_bounds(name)
                    .filter(|bounds| bounds.size.width > px(0.0) && bounds.size.height > px(0.0))
                    .map(|bounds| (name, kind, bounds))
            })
            .collect();
        for kind in [LayoutProbe::Item, LayoutProbe::Container] {
            assert!(
                probes.iter().any(|(_, probe, _)| *probe == kind),
                "{scene}: no {kind:?} probes rendered"
            );
        }
        let tolerance = px(0.5);
        let within = |inner: &Bounds<gpui::Pixels>, outer: &Bounds<gpui::Pixels>| {
            inner.left() >= outer.left() - tolerance && inner.right() <= outer.right() + tolerance
        };
        for (name, probe, bounds) in &probes {
            assert!(
                bounds.left() >= px(SIDEBAR_WIDTH) - tolerance
                    && bounds.right() <= px(WINDOW_WIDTH) + tolerance,
                "{scene}: {name} {bounds:?} leaves the pane"
            );
            for (container_name, kind, container) in &probes {
                let overlaps = bounds.left() < container.right()
                    && container.left() < bounds.right()
                    && bounds.top() < container.bottom()
                    && container.top() < bounds.bottom();
                if container_name == name || *kind != LayoutProbe::Container || !overlaps {
                    continue;
                }
                // Overlapping containers must nest, in either direction.
                let nested = *probe == LayoutProbe::Container && within(container, bounds);
                assert!(
                    within(bounds, container) || nested,
                    "{scene}: {name} {bounds:?} crosses the edge of {container_name} {container:?}"
                );
            }
        }
    }

    #[gpui::test]
    fn controls_stay_inside_their_panels_in_every_pane(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.simulate_resize(size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)));
        for pane in Pane::ALL {
            crate::desktop_ui::layout_probes::record();
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_pane(pane, cx)));
            cx.run_until_parked();
            assert_layout_probes_fit(cx, &format!("{pane:?}"));
        }
        // A one-model chain places the fallback hint beside its Add button.
        crate::desktop_ui::layout_probes::record();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.openrouter_settings.read(cx).config_snapshot();
                config.transcription.models = vec!["openai::gpt-4o-transcribe".into()];
                view.synchronize_model_config(config, cx);
                view.show_models(cx);
            })
        });
        cx.run_until_parked();
        assert_layout_probes_fit(cx, "Models with one model");
        // Every History dictation, including one with a fallback request.
        let folder = std::env::temp_dir().join(format!(
            "hex-history-layout-{}-{}",
            std::process::id(),
            crate::history::now_ms()
        ));
        let ids: Vec<u64> = cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.history = history_pane::preview_history_in(folder.clone());
                view.reload_history(cx);
                view.history_entries.iter().map(|entry| entry.id).collect()
            })
        });
        assert!(ids.len() >= 3);
        for id in ids {
            crate::desktop_ui::layout_probes::record();
            cx.update(|_, cx| {
                view.update(cx, |view, cx| {
                    view.select_pane(Pane::History, cx);
                    view.selected_recovery = None;
                    view.selected_history = Some(id);
                    cx.notify();
                })
            });
            cx.run_until_parked();
            assert_layout_probes_fit(cx, &format!("History entry {id}"));
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[gpui::test]
    fn history_entries_open_across_the_pane_and_return_to_the_list(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        let folder = std::env::temp_dir().join(format!(
            "hex-history-navigation-{}-{}",
            std::process::id(),
            crate::history::now_ms()
        ));
        let ids: Vec<u64> = cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.history = history_pane::preview_history_in(folder.clone());
                view.select_pane(Pane::History, cx);
                view.window_focus.focus(window);
                view.history_entries.iter().map(|entry| entry.id).collect()
            })
        });
        assert!(ids.len() >= 2);
        let open =
            |cx: &mut gpui::VisualTestContext| cx.update(|_, cx| view.read(cx).open_history_item());
        cx.run_until_parked();
        assert_eq!(open(cx), None, "History starts on the list");
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.show_history_item(HistoryItem::Dictation(ids[0]), cx)
            })
        });
        cx.simulate_keystrokes("down");
        assert_eq!(open(cx), Some(HistoryItem::Dictation(ids[1])));
        cx.simulate_keystrokes("up up");
        assert_eq!(open(cx), Some(HistoryItem::Dictation(ids[0])));
        cx.simulate_keystrokes("escape");
        assert_eq!(open(cx), None);
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).history_last_opened,
                Some(HistoryItem::Dictation(ids[0]))
            );
        });
        // Escape in the search field cancels the search draft, not the entry.
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.show_history_item(HistoryItem::Dictation(ids[1]), cx);
                gpui::Focusable::focus_handle(view.history_search.read(cx), cx).focus(window);
            })
        });
        cx.simulate_keystrokes("escape");
        assert_eq!(open(cx), Some(HistoryItem::Dictation(ids[1])));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[gpui::test]
    fn failed_recordings_keep_their_place_and_act_from_the_list(cx: &mut gpui::TestAppContext) {
        let folder = std::env::temp_dir().join(format!(
            "hex-history-timeline-{}-{}",
            std::process::id(),
            crate::history::now_ms()
        ));
        let store = RecordingRecovery::open(folder.clone()).unwrap();
        assert!(
            store
                .transcribe_original(
                    &[0.1; 1600],
                    Some("Notes"),
                    crate::post_processing::Preferences::default(),
                    |_| Err(color_eyre::eyre::eyre!("offline fixture"))
                )
                .is_err()
        );
        let failed_at = store.entries("")[0].timestamp_ms;
        let recovery = HistoryItem::Recovery(store.entries("")[0].id.clone());
        let (view, cx) = cx.add_window_view(preview_fixture);
        let (older, newer) = cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut history = crate::history::HistoryStore::open(
                    folder.join("normal-history.json"),
                    HistoryRetention::Week,
                    failed_at,
                );
                let mut record = |text: &str, timestamp_ms| {
                    history
                        .record(
                            crate::history::HistoryDraft {
                                text: text.into(),
                                application: Some("Notes".into()),
                                audio_ms: 100,
                                inference_ms: 100,
                                total_ms: 100,
                                transcription: None,
                            },
                            timestamp_ms,
                        )
                        .unwrap()
                };
                let older = record("Before the failure", failed_at - 60_000);
                let newer = record("After the failure", failed_at + 60_000);
                view.recovery = Some(store.clone());
                view.history = Some(History::new(history));
                view.select_pane(Pane::History, cx);
                (older.unwrap(), newer.unwrap())
            })
        });
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).history_items(),
                [
                    HistoryItem::Dictation(newer),
                    recovery.clone(),
                    HistoryItem::Dictation(older),
                ],
                "a failed recording stays in recording order"
            );
        });
        cx.run_until_parked();
        let center = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
            cx.debug_bounds(selector).unwrap().center()
        };
        let retry = center(cx, "recovery-row-retry-0");
        cx.simulate_click(retry, gpui::Modifiers::none());
        let deadline = Instant::now() + Duration::from_secs(3);
        while store.retry_in_progress() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.open_history_item(), None, "Retry stays on the list");
                view.poll_history(cx);
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert!(
                !view.can_retry_recovery(&view.recovery_entries[0]),
                "a recovered recording no longer offers Retry"
            );
            assert_eq!(
                view.history_copied,
                Some(recovery.clone()),
                "Retry & copy copies the recovered text"
            );
            assert_eq!(view.recovery_copy_pending, None);
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("Recovered preview dictation.".into())
            );
        });
        // Copy stays available for the recovered text.
        cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("other".into())));
        let copy = center(cx, "recovery-row-copy-0");
        cx.simulate_click(copy, gpui::Modifiers::none());
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.open_history_item(), None, "Copy stays on the list");
            assert_eq!(view.history_copied, Some(recovery.clone()));
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("Recovered preview dictation.".into())
            );
        });
        drop(store);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[gpui::test]
    fn removing_and_restoring_provider_key_refreshes_model_editors(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        let id = "openai::gpt-transcribe";
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.openrouter_settings.read(cx).config_snapshot();
                config.transcription.models = vec![id.into()];
                crate::providers::initialize_model(&mut config, id, None);
                view.synchronize_model_config(config, cx);
                view.providers.update(cx, |providers, cx| {
                    providers.sync_key_status(
                        crate::providers::Provider::OpenAi,
                        crate::openrouter::KeyStatus::Missing,
                        cx,
                    )
                });
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let saved = view.openrouter_settings.read(cx).config_snapshot();
                assert_eq!(saved.transcription.models, vec![id]);
                assert!(saved.transcription.model_options.contains_key(id));
                assert!(
                    !view
                        .providers
                        .read(cx)
                        .available_providers(cx)
                        .contains(&crate::providers::Provider::OpenAi)
                );
                view.providers.update(cx, |providers, cx| {
                    providers.sync_key_status(
                        crate::providers::Provider::OpenAi,
                        crate::openrouter::KeyStatus::Keychain("demo".into()),
                        cx,
                    )
                });
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert!(
                    view.providers
                        .read(cx)
                        .available_providers(cx)
                        .contains(&crate::providers::Provider::OpenAi)
                );
                assert_eq!(
                    view.openrouter_settings.read(cx).config_snapshot(),
                    view.model_options.read(cx).config_snapshot()
                );
            })
        });
    }

    #[gpui::test]
    fn closing_flushes_advanced_drafts_before_export_and_model_sync(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.show_providers(cx);
                // The helper emits the same Changed event as the TextInput control.
                view.providers.update(cx, |providers, cx| {
                    providers.stage_timeout_draft("67", cx);
                });
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let before = view.providers.read(cx).config_snapshot();
                assert_ne!(before.transcription.attempt_timeout_seconds, 67);
                // The same synchronous path is used by close, quit and export.
                view.finish_editing(cx);
                let saved = view.openrouter_settings.read(cx).config_snapshot();
                assert_eq!(saved.transcription.attempt_timeout_seconds, 67);
                assert_eq!(view.providers.read(cx).config_snapshot(), saved);
                assert_eq!(view.model_options.read(cx).config_snapshot(), saved);

                let bytes =
                    crate::preferences_transfer::export_bytes(&view.settings, &saved).unwrap();
                let transferred = crate::preferences_transfer::preview_bundle(
                    crate::preferences_transfer::decode(&bytes).unwrap(),
                    &view.settings,
                    &saved,
                )
                .unwrap();
                assert_eq!(transferred.config.transcription.attempt_timeout_seconds, 67);
                view.show_models(cx);
                assert_eq!(view.pane, Pane::Models);
            })
        });
        // Queued callbacks from the first save must not overwrite the second.
        cx.run_until_parked();
        cx.update(|_, cx| {
            let view = view.read(cx);
            let saved = view.openrouter_settings.read(cx).config_snapshot();
            assert_eq!(saved.transcription.attempt_timeout_seconds, 67);
            assert_eq!(view.providers.read(cx).config_snapshot(), saved);
            assert_eq!(view.model_options.read(cx).config_snapshot(), saved);
        });
    }

    #[gpui::test]
    fn model_drafts_survive_global_sync_flush_on_close_and_reset_on_import(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.bind_keys(crate::text_input::key_bindings()));
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.simulate_resize(size(px(WINDOW_WIDTH), px(2400.0)));
        cx.update(|window, cx| {
            window.activate_window();
            view.update(cx, |view, cx| {
                let mut config = view.openrouter_settings.read(cx).config_snapshot();
                config.transcription.models = vec!["openai::gpt-4o-transcribe".into()];
                view.synchronize_model_config(config, cx);
                view.show_models(cx);
            });
        });
        cx.run_until_parked();
        let context = cx.debug_bounds("model-options-context").unwrap();
        cx.simulate_click(context.center(), GpuiModifiers::default());
        cx.simulate_input("Keep this draft");
        // A global setting changes while Context is still being edited.
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.providers.read(cx).config_snapshot();
                config.transcription.attempt_timeout_seconds = 45;
                view.providers.update(cx, |editor, cx| {
                    editor.refresh(config, cx);
                    cx.emit(crate::providers_view::ProvidersChanged);
                });
            })
        });
        cx.run_until_parked();
        // The close/quit path flushes before removing the focus tree.
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.finish_editing(cx);
                let config = view.openrouter_settings.read(cx).config_snapshot();
                assert_eq!(
                    crate::providers::options(&config, "openai::gpt-4o-transcribe").prompt,
                    "Keep this draft"
                );
                assert_eq!(config.transcription.attempt_timeout_seconds, 45);
                assert_eq!(view.providers.read(cx).config_snapshot(), config);
                assert_eq!(view.model_options.read(cx).config_snapshot(), config);
                let bytes =
                    crate::preferences_transfer::export_bytes(&view.settings, &config).unwrap();
                let exported = crate::preferences_transfer::preview_bundle(
                    crate::preferences_transfer::decode(&bytes).unwrap(),
                    &view.settings,
                    &config,
                )
                .unwrap();
                assert_eq!(
                    exported.config.transcription.model_options,
                    config.transcription.model_options
                );
            })
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("Discard this old draft");
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.openrouter_settings.read(cx).config_snapshot();
                let options = config
                    .transcription
                    .model_options
                    .get_mut("openai::gpt-4o-transcribe")
                    .unwrap();
                options.prompt = "Imported context".into();
                view.accept_imported_preferences(
                    crate::preferences_transfer::ImportOutcome {
                        settings: view.settings.clone(),
                        config: config.clone(),
                    },
                    cx,
                );
                view.finish_editing(cx);
                assert_eq!(view.model_options.read(cx).config_snapshot(), config);
            })
        });
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        cx.update(|_, cx| {
            let config = view.read(cx).openrouter_settings.read(cx).config_snapshot();
            assert_eq!(
                crate::providers::options(&config, "openai::gpt-4o-transcribe").prompt,
                "Imported context"
            );
        });
        let context = cx.debug_bounds("model-options-context").unwrap();
        cx.simulate_click(context.center(), GpuiModifiers::default());
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("Save when leaving Models");
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_providers(cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let config = view.read(cx).providers.read(cx).config_snapshot();
            assert_eq!(
                crate::providers::options(&config, "openai::gpt-4o-transcribe").prompt,
                "Save when leaving Models"
            );
        });
    }

    #[gpui::test]
    fn models_and_providers_have_distinct_navigation_and_keep_key_state(
        cx: &mut gpui::TestAppContext,
    ) {
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
                assert!(view.render_key_notice(cx).is_some());
                view.show_providers(cx);
                assert_eq!(view.pane, Pane::Providers);
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
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_microphone(cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert!(view.openrouter_settings.read(cx).trim_silence());
                view.toggle_trim_silence(cx);
                assert!(!view.openrouter_settings.read(cx).trim_silence());
                assert!(view.settings_error.is_none());
                view.show_models(cx);
                view.show_settings(cx);
                view.show_microphone(cx);
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
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_microphone(cx)));
        cx.run_until_parked();
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
    fn provider_options_and_shared_keywords_stay_in_sync_across_panes(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut config = view.openrouter_settings.read(cx).config_snapshot();
                config.transcription.models = vec![
                    "deepgram::nova-2".into(),
                    "acme/no-hints".into(),
                    "openai::gpt-transcribe".into(),
                ];
                assert!(crate::providers::has_keyword_support(&config, false));
                view.openrouter_settings.update(cx, |editor, cx| {
                    editor.apply_imported_config(config.clone(), cx);
                    cx.emit(crate::openrouter::settings_view::ConfigChanged);
                });
                let vocabulary = crate::vocabulary::Vocabulary {
                    terms: vec!["Nimbus-Files".into()],
                    ..Default::default()
                };
                view.shared_keywords.update(cx, |_, cx| {
                    cx.emit(crate::vocabulary_view::VocabularyChange(vocabulary))
                });
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.settings.vocabulary.terms, vec!["Nimbus-Files"]);
                assert_eq!(
                    view.providers.read(cx).config_snapshot(),
                    view.openrouter_settings.read(cx).config_snapshot()
                );
                assert_eq!(
                    view.model_options.read(cx).config_snapshot(),
                    view.openrouter_settings.read(cx).config_snapshot()
                );
                let mut config = view.providers.read(cx).config_snapshot();
                config.transcription.models.pop();
                assert!(!crate::providers::has_keyword_support(&config, false));
                config.transcription.model_options.insert(
                    "deepgram::nova-2".into(),
                    crate::providers::ModelOptions {
                        language: "pt".into(),
                        streaming: true,
                        ..Default::default()
                    },
                );
                view.providers.update(cx, |editor, cx| {
                    editor.apply_imported_config(config, cx);
                    cx.emit(crate::providers_view::ProvidersChanged);
                });
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.settings.vocabulary.terms, vec!["Nimbus-Files"]);
                let config = view.openrouter_settings.read(cx).config_snapshot();
                assert!(!crate::providers::has_keyword_support(&config, false));
                assert!(crate::providers::options(&config, "deepgram::nova-2").streaming);
                assert_eq!(view.model_options.read(cx).config_snapshot(), config);
                view.show_providers(cx);
                assert_eq!(view.pane, Pane::Providers);
                view.show_models(cx);
                assert_eq!(view.pane, Pane::Models);
            })
        });
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
    fn reopening_targets_providers_for_a_missing_key_and_settings_for_permissions() {
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
            Pane::Providers
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
