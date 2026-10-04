//! The app window: Settings, History, and Statistics, plus the first-run
//! setup sheet.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, Div, Entity, FocusHandle, FontWeight, IntoElement,
    KeyDownEvent, Modifiers as GpuiModifiers, ModifiersChangedEvent, MouseDownEvent, Render,
    SharedString, Subscription, Timer, TitlebarOptions, Window, WindowBounds, WindowHandle,
    WindowOptions, actions, deferred, div, prelude::*, px, rgb, rgba, size,
};

use crate::app_settings::{
    AppSettings, HotkeyBinding, HotkeyKey, HotkeyModifiers, ModifierSide, RecordingAudioBehavior,
};
use crate::desktop_ui::{
    ACCENT, CANVAS, CONTROL_HEIGHT, FAINT, LINE, MUTED, NEGATIVE, NavigationIcon,
    PANE_CONTENT_WIDTH, PANE_LIST_WIDTH, PANEL_RADIUS, SIDEBAR_WIDTH, SURFACE, SURFACE_HOVER,
    SURFACE_SELECTED, TEXT, TEXT_SOFT, compact_button, compact_panel, disclosure_button,
    error_message, header_button, hotkey_keycaps, mix_color, navigation_item, pane_body,
    pane_content, pane_header, pane_header_with_action, pane_list, section_label, settings_copy,
    settings_panel, settings_row, settings_section_label, sidebar_frame, sliding_segmented_control,
    sliding_segmented_item, toggle, window_frame,
};
use crate::history::{History, HistoryEntry, HistoryRetention};
use crate::login_item::{LoginItemRequest, LoginItemResponse, LoginItemStatus, LoginItemWorker};
use crate::onboarding::{
    PermissionAction, PermissionKind, PermissionState, PermissionWarning, SetupStatus,
};
use crate::openrouter::settings_view::{KeyChanged, OpenRouterSettings};
use crate::openrouter::stats_view::StatisticsView;
use crate::text_input::{Changed as TextChanged, TextInput};

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
        ShowSettings,
        ShowStatistics,
        ToggleFullscreen,
    ]
);

/// The microphone menu is deferred above the settings panel and occludes
/// everything beneath it, so hovering or choosing a device never reaches the
/// controls under the menu. A mouse-down anywhere else dismisses it.
fn microphone_picker_menu(
    choices: Vec<Option<String>>,
    selected: Option<String>,
    error: Option<String>,
    choose: impl Fn(&Option<String>, &mut Window, &mut App) + 'static,
    dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    let choose = Rc::new(choose);
    div()
        .id("microphone-picker")
        .absolute()
        .top(px(CONTROL_HEIGHT + 4.0))
        .right_0()
        .w(px(220.0))
        .max_h(px(240.0))
        .p_2()
        .overflow_y_scroll()
        .rounded_sm()
        .border_1()
        .border_color(rgb(LINE))
        .bg(rgb(SURFACE))
        .shadow_lg()
        .occlude()
        .on_mouse_down_out(dismiss)
        .children(choices.into_iter().enumerate().map(|(index, device)| {
            let is_selected = selected == device;
            let label = device.clone().unwrap_or_else(|| "Automatic".into());
            let choose = choose.clone();
            div()
                .id(("microphone-choice", index))
                .w_full()
                .h(px(34.0))
                .px_3()
                .flex()
                .items_center()
                .justify_between()
                .rounded_sm()
                .text_size(px(12.0))
                .text_color(if is_selected {
                    rgb(TEXT)
                } else {
                    rgb(TEXT_SOFT)
                })
                .when(is_selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                .child(label)
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    choose(&device, window, cx);
                })
        }))
        .when_some(error, |picker, error| {
            picker.child(
                div()
                    .px_3()
                    .pt_2()
                    .text_size(px(11.0))
                    .text_color(rgb(NEGATIVE))
                    .child(error),
            )
        })
}

fn settings_pane(content: Div) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(pane_header("Settings"))
        .child(
            div()
                .id("settings-scroll")
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
    History,
    Statistics,
}

impl Pane {
    const ALL: [Self; 3] = [Self::Settings, Self::History, Self::Statistics];

    fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::History => "History",
            Self::Statistics => "Statistics",
        }
    }

    fn icon(self) -> NavigationIcon {
        match self {
            Self::Settings => NavigationIcon::Settings,
            Self::History => NavigationIcon::History,
            Self::Statistics => NavigationIcon::Statistics,
        }
    }

    fn on_reopen(self, status: SetupStatus) -> Self {
        if crate::onboarding::permission_warnings(status).is_empty() && status.api_key {
            self
        } else {
            Self::Settings
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
    microphone_devices: Vec<String>,
    microphone_picker_open: bool,
    microphone_picker_error: Option<String>,
    launch_at_login_status: Option<LoginItemStatus>,
    login_item_worker: Option<LoginItemWorker>,
    launch_at_login_error: Option<String>,
    launch_at_login_toggle: ToggleSpring,
    release_microphone_toggle: ToggleSpring,
    double_tap_toggle: ToggleSpring,
    double_tap_only_visibility: ToggleSpring,
    dock_icon_toggle: ToggleSpring,
    sound_volume_spring: ToggleSpring,
    recording_audio_spring: ToggleSpring,
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
    _subscriptions: Vec<Subscription>,
    history: Option<History>,
    history_search: Entity<TextInput>,
    history_entries: Vec<HistoryEntry>,
    selected_history: Option<u64>,
    history_error: Option<String>,
    history_clear_armed: bool,
    history_retention_open: bool,
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
                        | window.poll_history(cx);
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
            microphone_picker_error: None,
            launch_at_login_status,
            login_item_worker,
            launch_at_login_error,
            launch_at_login_toggle: ToggleSpring::new(
                launch_at_login_status == Some(LoginItemStatus::Enabled),
            ),
            release_microphone_toggle: ToggleSpring::new(settings.release_microphone_while_idle),
            double_tap_toggle: ToggleSpring::new(settings.double_tap_lock),
            double_tap_only_visibility: ToggleSpring::new(
                settings.double_tap_lock && settings.dictation_hotkey.key.is_some(),
            ),
            dock_icon_toggle: ToggleSpring::new(settings.show_dock_icon),
            sound_volume_spring: ToggleSpring::at(sound_volume_index(&settings) as f32),
            recording_audio_spring: ToggleSpring::at(recording_audio_index(
                settings.recording_audio_behavior,
            ) as f32),
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
            _subscriptions: subscriptions,
            settings,
            settings_error,
            history,
            history_search,
            history_entries: Vec::new(),
            selected_history: None,
            history_error: None,
            history_clear_armed: false,
            history_retention_open: preview
                .as_ref()
                .is_some_and(|preview| preview.open_history_retention),
            history_copied: None,
        };
        window.reload_history(cx);
        if window.preview {
            window.selected_history = window.history_entries.first().map(|entry| entry.id);
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
        self.pane = pane;
        self.history_retention_open = false;
        self.microphone_picker_open = false;
        match pane {
            Pane::History => self.reload_history(cx),
            Pane::Statistics => self.statistics.update(cx, |view, cx| {
                view.refresh();
                cx.notify();
            }),
            Pane::Settings => self.permission_refresh_at = Instant::now(),
        }
        cx.notify();
    }

    pub(crate) fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Settings, cx);
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
                        && self.pane != Pane::Settings
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
        self.settings_error = result
            .as_ref()
            .err()
            .map(|error| format!("Could not save settings: {error:#}"));
        cx.notify();
        result.is_ok()
    }

    fn set_double_tap_lock(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.update_settings(cx, |settings| {
            settings.double_tap_lock = enabled;
            if !enabled {
                settings.double_tap_only = false;
            }
        }) {
            self.double_tap_toggle.set_enabled(enabled);
        }
    }

    fn set_double_tap_only(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.update_settings(cx, |settings| {
            settings.double_tap_only =
                enabled && settings.double_tap_lock && settings.dictation_hotkey.key.is_some();
        });
    }

    // ---- History -----------------------------------------------------------

    fn reload_history(&mut self, cx: &App) {
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
        if self.preview || self.pane != Pane::History || self.history.is_none() {
            return false;
        }
        let previous = std::mem::take(&mut self.history_entries);
        self.reload_history(cx);
        self.history_entries != previous
    }

    fn set_history_retention(&mut self, retention: HistoryRetention, cx: &mut Context<Self>) {
        if self.settings.history_retention == retention {
            return;
        }
        if !self.update_settings(cx, |settings| settings.history_retention = retention) {
            return;
        }
        if let Some(history) = &self.history
            && let Err(error) = history.set_retention(retention)
        {
            self.history_error = Some(error.to_string());
        }
        self.reload_history(cx);
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

    fn render_history(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let retention = self.settings.history_retention;
        let search = div().w(px(220.0)).child(self.history_search.clone());
        let retention_control = header_button(format!("Keep: {}", retention.label()))
            .id("history-retention")
            .on_click(cx.listener(|this, _, _, cx| {
                this.history_retention_open = true;
                cx.notify();
            }));
        let retention_control = div().relative().child(retention_control).when(
            self.history_retention_open,
            |control| {
                control.child(deferred(
                    div()
                        .id("history-retention-menu")
                        .absolute()
                        .top(px(36.0))
                        .right_0()
                        .w(px(160.0))
                        .p_1()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(LINE))
                        .bg(rgb(SURFACE))
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.history_retention_open = false;
                            cx.notify();
                        }))
                        .children(HistoryRetention::ALL.into_iter().enumerate().map(
                            |(index, choice)| {
                                compact_button(choice.label())
                                    .id(("history-retention-choice", index))
                                    .w_full()
                                    .when(choice == retention, |item| {
                                        item.bg(rgb(SURFACE_SELECTED))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.history_retention_open = false;
                                        this.set_history_retention(choice, cx);
                                    }))
                            },
                        )),
                ))
            },
        );
        let clear = header_button(if self.history_clear_armed {
            "Really clear all?"
        } else {
            "Clear all"
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
        let rows: Vec<AnyElement> = self
            .history_entries
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
                        this.history_clear_armed = false;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let retention_off = retention.is_off();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header_with_action("History", Some(header_action)))
            .child(
                pane_body().p_5().child(
                    pane_content()
                        .flex_row()
                        .gap_5()
                        .child(
                            pane_list(
                                "history-list",
                                if retention_off {
                                    Some("History is off. New dictations are not retained.")
                                } else if self.history_entries.is_empty()
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

    // ---- Navigation --------------------------------------------------------

    fn render_navigation(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let items = Pane::ALL.into_iter().enumerate().map(|(index, pane)| {
            navigation_item(pane.icon(), self.pane == pane)
                .id(("app-nav", index))
                .child(pane.label())
                .on_click(cx.listener(move |this, _, _, cx| this.select_pane(pane, cx)))
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
                        .child("Saved"),
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
                                    if this.update_settings(cx, |settings| {
                                        set_hotkey_binding(settings, kind, candidate);
                                    }) {
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
            .into_any_element()
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
        if !self.update_settings(cx, |settings| set_hotkey_binding(settings, kind, binding)) {
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

    fn render_microphone_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let choices = std::iter::once(None)
            .chain(self.microphone_devices.iter().cloned().map(Some))
            .collect::<Vec<_>>();
        microphone_picker_menu(
            choices,
            self.settings.microphone.clone(),
            self.microphone_picker_error.clone(),
            cx.listener(|this, device: &Option<String>, _, cx| {
                if this.update_settings(cx, |settings| settings.microphone = device.clone()) {
                    this.microphone_picker_open = false;
                    this.microphone_picker_error = None;
                }
            }),
            cx.listener(|this, _, _, cx| {
                this.microphone_picker_open = false;
                this.microphone_picker_error = None;
                cx.notify();
            }),
        )
        .into_any_element()
    }

    fn toggle_microphone_picker(&mut self, cx: &mut Context<Self>) {
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
        cx.notify();
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
        if self.setup_visible || self.setup_status.api_key || self.pane == Pane::Settings {
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
                            compact_button("Open Settings")
                                .id("open-key-settings")
                                .flex_none()
                                .bg(rgb(ACCENT))
                                .text_color(rgb(TEXT))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.select_pane(Pane::Settings, cx)
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

    fn render_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let permission_warnings = self.render_permission_warnings(cx);
        let hotkey_control = self.render_hotkey_setting_control(HotkeyKind::Dictation, window, cx);
        let paste_last_control =
            self.render_hotkey_setting_control(HotkeyKind::PasteLast, window, cx);
        let double_tap_position = self.double_tap_toggle.render_position(window);
        self.double_tap_only_visibility.set_enabled(
            self.settings.double_tap_lock && self.settings.dictation_hotkey.key.is_some(),
        );
        let double_tap_only_visibility = self
            .double_tap_only_visibility
            .render_position(window)
            .clamp(0.0, 1.0);
        let dock_icon_position = self.dock_icon_toggle.render_position(window);
        let launch_at_login_position = self.launch_at_login_toggle.render_position(window);
        let release_microphone_position = self.release_microphone_toggle.render_position(window);
        let microphone_label = self
            .settings
            .microphone
            .clone()
            .unwrap_or_else(|| "Automatic".into());
        let microphone_picker = self
            .microphone_picker_open
            .then(|| self.render_microphone_picker(cx));
        let sound_volume_position = self.sound_volume_spring.render_position(window);
        let sound_volume = sliding_segmented_control(sound_volume_position, &[34.0; 5]).children(
            [
                ("Off", 0.0_f32),
                ("25%", 0.25),
                ("50%", 0.5),
                ("75%", 0.75),
                ("100%", 1.0),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (label, volume))| {
                let selected = if volume == 0.0 {
                    !self.settings.sound_effects
                } else {
                    self.settings.sound_effects
                        && (self.settings.sound_effect_volume - volume).abs() < 0.01
                };
                sliding_segmented_item(34.0, selected)
                    .id(("sound-volume", index))
                    .text_size(px(9.0))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.update_settings(cx, |settings| {
                            settings.sound_effects = volume > 0.0;
                            if volume > 0.0 {
                                settings.sound_effect_volume = volume;
                            }
                        }) {
                            return;
                        }
                        this.sound_volume_spring.set_target(index as f32);
                        if volume > 0.0 && !this.preview {
                            crate::feedback::play(crate::feedback::Tone::DictationStart);
                        }
                    }))
            }),
        );
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
        let audio_widths = [50.0, 90.0, 80.0];
        let audio_behavior = sliding_segmented_control(recording_audio_position, &audio_widths)
            .children(
                [
                    RecordingAudioBehavior::Mute,
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
                                && this.update_settings(cx, |settings| {
                                    settings.recording_audio_behavior = behavior
                                })
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
                                && this.update_settings(cx, |settings| {
                                    settings.release_microphone_while_idle = release
                                })
                            {
                                this.release_microphone_toggle.set_enabled(release);
                            }
                        }))
                    }),
            );
        settings_pane(
            div()
                .children(permission_warnings)
                .child(self.openrouter_settings.clone())
                .child(settings_section_label("DICTATION"))
                .child(
                    settings_panel()
                        .child(settings_row(
                            "Dictation shortcut",
                            "Hold to dictate, release to transcribe and paste",
                            hotkey_control,
                        ))
                        .child(
                            settings_row(
                                "Double-tap to lock",
                                "Double-tap the shortcut for hands-free dictation; press it again to finish",
                                toggle(double_tap_position),
                            )
                            .id("double-tap-setting")
                            .when(double_tap_only_visibility < 0.01, |row| row.border_b_0())
                            .on_click(cx.listener(|this, _, _, cx| {
                                let enabled = !this.settings.double_tap_lock;
                                this.set_double_tap_lock(enabled, cx);
                            })),
                        )
                        .child(
                            div()
                                .h(px(72.0 * double_tap_only_visibility))
                                .overflow_hidden()
                                .opacity(double_tap_only_visibility)
                                .child(
                                    settings_row(
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
                        ),
                )
                .child(settings_section_label("PASTE LAST"))
                .child(
                    settings_panel().child(
                        settings_row(
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
                                                this.update_settings(cx, |settings| {
                                                    settings.paste_last_hotkey = None
                                                });
                                            })),
                                    )
                                }),
                        )
                        .border_b_0(),
                    ),
                )
                .child(settings_section_label("MICROPHONE"))
                .child(
                    settings_panel()
                        .child(settings_row(
                            "Input device",
                            "Automatic picks the preferred available microphone",
                            div()
                                .relative()
                                .flex_none()
                                .child(
                                    disclosure_button(microphone_label)
                                        .id("microphone-setting")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.toggle_microphone_picker(cx)
                                        })),
                                )
                                .children(microphone_picker.map(deferred)),
                        ))
                        .child(settings_row(
                            "Microphone mode",
                            if self.settings.release_microphone_while_idle {
                                "Opens on the shortcut: the orange indicator only shows while dictating, but the first syllable can be lost"
                            } else {
                                "Keeps the microphone open so a short pre-roll catches the start of speech. Audio is never saved"
                            },
                            microphone_mode,
                        ))
                        .child(
                            settings_row(
                                "While dictating",
                                "What happens to other audio once a hold becomes a dictation",
                                audio_behavior,
                            )
                            .border_b_0(),
                        ),
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
                            settings_row(
                                "Show Dock icon",
                                "When off, Hex lives in the menu bar while this window is closed",
                                toggle(dock_icon_position),
                            )
                            .id("dock-icon-setting")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.update_settings(cx, |settings| {
                                    settings.show_dock_icon = !settings.show_dock_icon
                                }) {
                                    this.dock_icon_toggle
                                        .set_enabled(this.settings.show_dock_icon);
                                }
                            })),
                        )
                        .child(
                            settings_row(
                                "Sound volume",
                                "Recording, cancellation, and error tones",
                                sound_volume,
                            )
                            .border_b_0(),
                        ),
                ),
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
                                    .child("Hold the shortcut, speak, and release: Hex trims the silence, transcribes through OpenRouter, and pastes the text. It is ready once these permissions and your key are in place."),
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
                                        "Saved. Choose models and fallbacks in Settings later.",
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
        let content = match self.pane {
            Pane::Settings => self.render_settings(window, cx),
            Pane::History => self.render_history(cx),
            Pane::Statistics => self.statistics.clone().into_any_element(),
        };
        let setup = self.setup_visible.then(|| self.render_setup(cx));
        window_frame()
            .track_focus(&self.window_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key != "escape" {
                    return;
                }
                if this.history_retention_open || this.microphone_picker_open {
                    this.history_retention_open = false;
                    this.microphone_picker_open = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(|_: &MinimizeWindow, window, _| window.minimize_window())
            .on_action(|_: &ToggleFullscreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|this, _: &ShowSettings, window, cx| {
                this.select_pane(Pane::Settings, cx);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowHistory, window, cx| {
                this.select_pane(Pane::History, cx);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowStatistics, window, cx| {
                this.select_pane(Pane::Statistics, cx);
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
        RecordingAudioBehavior::PauseMedia => 1,
        RecordingAudioBehavior::DoNothing => 2,
    }
}

fn sound_volume_index(settings: &AppSettings) -> usize {
    if !settings.sound_effects {
        return 0;
    }
    [0.25_f32, 0.5, 0.75, 1.0]
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            (settings.sound_effect_volume - **left)
                .abs()
                .total_cmp(&(settings.sound_effect_volume - **right).abs())
        })
        .map_or(0, |(index, _)| index + 1)
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
    fn every_pane_renders_in_preview(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(preview_fixture);
        for pane in Pane::ALL {
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_pane(pane, cx)));
            cx.run_until_parked();
        }
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
    fn missing_key_or_permissions_reopen_on_settings() {
        let ready = SetupStatus {
            microphone: PermissionState::Ready,
            input_monitoring: PermissionState::Ready,
            accessibility: PermissionState::Ready,
            api_key: true,
        };
        assert_eq!(Pane::History.on_reopen(ready), Pane::History);
        assert_eq!(
            Pane::History.on_reopen(SetupStatus {
                api_key: false,
                ..ready
            }),
            Pane::Settings
        );
        assert_eq!(
            Pane::Statistics.on_reopen(SetupStatus {
                accessibility: PermissionState::NeedsSettings,
                ..ready
            }),
            Pane::Settings
        );
    }
}
