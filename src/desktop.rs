//! The GPUI application: the menu bar item, the app window, the dictation
//! HUD, and the listener thread they share a lifetime with.

use crate::i18n::t;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, eyre};
use gpui::{App, Application, KeyBinding, Menu, MenuItem, SystemMenuType, Timer, WindowHandle};

use crate::app_settings::AppSettings;
use crate::app_window::{AppWindow, AppWindowPreview};
use crate::dictation_indicator::{self, DictationIndicatorEvent, DictationIndicatorUi};
use crate::doorbell::Doorbell;
use crate::listener::{ListenerControl, ListenerControls};
use crate::status_item::StatusItemAction;

pub struct ListenerConfig {
    pub event_path: PathBuf,
    pub device: Option<String>,
}

static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Wakes the UI loop when it sleeps with nothing to animate.
static UI_WAKE: Doorbell = Doorbell::new();

/// While the HUD, notice or menu bar glyph animates, the UI loop turns at
/// display rate.
const ACTIVE_UI_TURN: Duration = Duration::from_millis(16);

/// At rest, the UI loop sleeps until an indicator event, a menu action, a
/// quit request or this fallback, which bounds a Control-C shutdown and the
/// once-a-minute update check.
const IDLE_UI_TURN: Duration = Duration::from_secs(1);

/// Asks the desktop loop to quit on its next tick. Safe from any thread;
/// used by the update relaunch so `open` can hand off to the new bundle.
pub fn request_quit() {
    QUIT_REQUESTED.store(true, Ordering::Relaxed);
    wake_ui();
}

/// Wakes the UI loop after queueing work for it. Safe from any thread.
pub fn wake_ui() {
    UI_WAKE.ring();
}

/// What the desktop process hosts for its lifetime.
pub enum Launch {
    /// The production app.
    App(ListenerConfig),
    /// The dictation HUD driven by a synthetic capture loop.
    DictationHudPreview(crate::hud_settings::HudPreferences),
    PasteNoticePreview,
    /// One isolated, deterministic window without app services.
    Shell(AppWindowPreview),
}

type AppWindowSlot = Rc<RefCell<Option<WindowHandle<AppWindow>>>>;

struct Ui {
    app_window: AppWindowSlot,
    listener_start: Option<SyncSender<()>>,
    history: Option<crate::history::History>,
    listener_controls: ListenerControls,
    status_actions: Option<Receiver<StatusItemAction>>,
    preview: Option<AppWindowPreview>,
}

impl Ui {
    fn open(&self, cx: &mut App) -> gpui::Result<WindowHandle<AppWindow>> {
        match self.preview.clone() {
            Some(preview) => crate::app_window::open_preview(&self.app_window, preview, cx),
            None => crate::app_window::open_or_focus(
                &self.app_window,
                self.listener_start.clone(),
                self.history.clone(),
                cx,
            ),
        }
    }

    fn open_pane(
        &self,
        cx: &mut App,
        show: impl FnOnce(&mut AppWindow, &mut gpui::Context<AppWindow>),
    ) {
        match self.open(cx) {
            Ok(handle) => {
                let _ = handle.update(cx, |view, window, cx| {
                    show(view, cx);
                    view.focus_pane(window);
                });
            }
            Err(error) => tracing::error!(%error, "could not open Endu"),
        }
    }
}

fn should_open_app_on_launch(
    show_dock_icon: bool,
    setup_ready: bool,
    onboarding_completed: bool,
    status_item_available: bool,
) -> bool {
    show_dock_icon || !setup_ready || !onboarding_completed || !status_item_available
}

pub fn run(shutdown: &'static AtomicBool, launch: Launch) -> Result<()> {
    if objc2::MainThreadMarker::new().is_none() {
        return Err(eyre!("desktop startup requires the main thread"));
    }
    let notice_preview = matches!(&launch, Launch::PasteNoticePreview);
    let (listener, hud_preview, preview) = match launch {
        Launch::App(listener) => (Some(listener), false, None),
        Launch::DictationHudPreview(preferences) => {
            preferences.apply_runtime();
            (None, true, None)
        }
        Launch::PasteNoticePreview => (None, true, None),
        Launch::Shell(preview) => (None, false, Some(preview)),
    };
    shutdown.store(false, Ordering::Relaxed);
    crate::keyboard::initialize_layout()?;
    let settings = if listener.is_some() {
        AppSettings::load()?
    } else {
        AppSettings::default()
    };
    if listener.is_some()
        && settings.vocabulary.remote_hints
        && !settings.vocabulary.terms.is_empty()
    {
        crate::openrouter::vocabulary_support::schedule(false);
    }
    let show_dock_icon = settings.show_dock_icon;
    // Previews keep their own override, applied when their window opens.
    let appearance = listener.is_some().then_some(settings.appearance);
    let setup_ready = listener.is_none() || crate::onboarding::status().ready();
    let onboarding_completed = listener.is_none() || crate::onboarding::completion_recorded();
    let history = listener.as_ref().and_then(|_| {
        crate::history::History::open_default(settings.history_retention)
            .inspect_err(|error| tracing::warn!(%error, "dictation history is unavailable"))
            .ok()
    });
    let recovery = listener
        .as_ref()
        .map(|_| crate::recording_recovery::RecordingRecovery::open_default())
        .transpose()?;
    let (indicator_sender, indicator_receiver) = dictation_indicator::channel();
    if notice_preview {
        let sender = indicator_sender.clone();
        thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                sender.send(DictationIndicatorEvent::ReadyToPaste {
                    copied_to_clipboard: false,
                });
                thread::sleep(Duration::from_secs(1));
            }
        });
    } else if hud_preview {
        spawn_hud_preview(indicator_sender.clone());
    }
    let (control_sender, control_receiver) = crate::listener::control_channel(8);
    let shutdown_wake = control_sender.clone();
    let (listener_start, listener_worker) = match listener {
        Some(listener) => {
            let events = crate::events::EventLog::create(&listener.event_path)?;
            let (start_sender, start) = mpsc::sync_channel(1);
            let indicator = indicator_sender.clone();
            let worker_history = history.clone();
            let worker_recovery = recovery.clone().expect("production recovery store");
            let worker = thread::spawn(move || {
                if !setup_ready && !wait_for_start(&start, shutdown) {
                    return;
                }
                if let Err(error) = crate::listener::listen(
                    events,
                    listener.device.as_deref(),
                    shutdown,
                    Some(indicator.clone()),
                    worker_history,
                    Some(control_receiver),
                    worker_recovery,
                ) {
                    tracing::error!(%error, "dictation listener stopped");
                    indicator.send(DictationIndicatorEvent::Failed);
                }
            });
            (Some(start_sender), Some(worker))
        }
        None => (None, None),
    };
    let listener_worker = Rc::new(RefCell::new(listener_worker));

    // Native HUD/notice previews can also receive menu and reopen actions.
    // Keep those actions isolated from real preferences and credentials too.
    let preview = preview.or_else(|| {
        hud_preview.then_some(AppWindowPreview {
            pane: crate::app_window::PreviewPane::Settings,
            onboarding: false,
            permissions_missing: false,
            open_history_retention: false,
            appearance: crate::appearance::Appearance::System,
            language: crate::i18n::LanguagePreference::English,
        })
    });
    let app_window: AppWindowSlot = Rc::new(RefCell::new(None));
    let application = Application::new();
    {
        let app_window = app_window.clone();
        let listener_start = listener_start.clone();
        let history = history.clone();
        let preview = preview.clone();
        application.on_reopen(move |cx| {
            let result = match preview.clone() {
                Some(preview) => crate::app_window::open_preview(&app_window, preview, cx),
                None => crate::app_window::open_or_focus(
                    &app_window,
                    listener_start.clone(),
                    history.clone(),
                    cx,
                ),
            };
            if let Err(error) = result {
                tracing::error!(%error, "could not open Endu");
            }
        });
    }
    let quit_worker = listener_worker.clone();
    let quit_wake = shutdown_wake.clone();
    application.run(move |cx| {
        // After GPUI installs its application class, so menus and alerts
        // follow the Appearance setting even before the window opens.
        if let Some(appearance) = appearance {
            appearance.apply_to_application();
        }
        if let Some(recovery) = recovery {
            cx.set_global(recovery);
        }
        // The HUD preview also shows the menu bar glyph, driven by the same
        // simulated events; its menu opens only the isolated preview window.
        let status_actions = if preview.is_none() || hud_preview {
            crate::status_item::install()
                .inspect_err(|error| tracing::error!(%error, "could not install the menu bar item"))
                .ok()
        } else {
            None
        };
        crate::app_settings::set_dock_icon_visible(crate::app_settings::dock_icon_visible(
            show_dock_icon,
            status_actions.is_some(),
        ));
        let ui = Rc::new(Ui {
            app_window: app_window.clone(),
            listener_start,
            history,
            listener_controls: control_sender,
            status_actions,
            preview,
        });
        install_menus(cx, &ui);
        let open_on_launch = !hud_preview
            && (ui.preview.is_some()
                || should_open_app_on_launch(
                    show_dock_icon,
                    setup_ready,
                    onboarding_completed,
                    ui.status_actions.is_some(),
                ));
        if open_on_launch && let Err(error) = ui.open(cx) {
            tracing::error!(%error, "could not open Endu");
        }
        cx.on_app_quit(move |_| {
            shutdown.store(true, Ordering::Relaxed);
            quit_wake.wake();
            join_listener(&quit_worker);
            async {}
        })
        .detach();
        let indicator_enabled = hud_preview || ui.preview.is_none();
        cx.spawn(async move |cx| {
            drive_ui(indicator_receiver, ui, shutdown, indicator_enabled, cx).await;
        })
        .detach();
    });
    shutdown.store(true, Ordering::Relaxed);
    shutdown_wake.wake();
    join_listener(&listener_worker);
    Ok(())
}

fn wait_for_start(start: &Receiver<()>, shutdown: &AtomicBool) -> bool {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return false;
        }
        match start.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => return true,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn join_listener(worker: &Rc<RefCell<Option<JoinHandle<()>>>>) {
    if let Some(worker) = worker.borrow_mut().take()
        && worker.join().is_err()
    {
        tracing::error!("dictation listener panicked during shutdown");
    }
}

/// The application menus, in the current interface language. Called again
/// when the language changes; actions stay registered by `install_menus`.
pub(crate) fn set_app_menus(cx: &mut App) {
    use crate::app_window::{
        CloseWindow, HideApplication, MinimizeWindow, QuitApplication, ShowAbout, ShowHistory,
        ShowHud, ShowMicrophone, ShowModels, ShowPostProcessing, ShowProviders, ShowSettings,
        ShowStatistics, ToggleFullscreen,
    };
    use crate::text_input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    cx.set_menus(vec![
        Menu {
            name: "Endu".into(),
            items: vec![
                MenuItem::action(t("About Endu"), ShowAbout),
                MenuItem::separator(),
                MenuItem::action(t("Settings"), ShowSettings),
                MenuItem::action(t("Microphone"), ShowMicrophone),
                MenuItem::action(t("Providers"), ShowProviders),
                MenuItem::action(t("Models"), ShowModels),
                MenuItem::action(t("Post-processing"), ShowPostProcessing),
                MenuItem::action("HUD", ShowHud),
                MenuItem::action(t("History"), ShowHistory),
                MenuItem::action(t("Statistics"), ShowStatistics),
                MenuItem::separator(),
                MenuItem::os_submenu(t("Services"), SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action(t("Hide Endu"), HideApplication),
                MenuItem::separator(),
                MenuItem::action(t("Quit Endu"), QuitApplication),
            ],
        },
        Menu {
            name: t("File").into(),
            items: vec![MenuItem::action(t("Close Window"), CloseWindow)],
        },
        // Text fields handle these through their own key context; the menu
        // makes the commands discoverable and lets macOS validate them.
        Menu {
            name: t("Edit").into(),
            items: vec![
                MenuItem::os_action(t("Undo"), Undo, gpui::OsAction::Undo),
                MenuItem::os_action(t("Redo"), Redo, gpui::OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action(t("Cut"), Cut, gpui::OsAction::Cut),
                MenuItem::os_action(t("Copy"), Copy, gpui::OsAction::Copy),
                MenuItem::os_action(t("Paste"), Paste, gpui::OsAction::Paste),
                MenuItem::os_action(t("Select All"), SelectAll, gpui::OsAction::SelectAll),
            ],
        },
        Menu {
            name: t("Window").into(),
            items: vec![
                MenuItem::action(t("Minimize"), MinimizeWindow),
                MenuItem::action(t("Enter Full Screen"), ToggleFullscreen),
            ],
        },
    ]);
}

fn install_menus(cx: &mut App, ui: &Rc<Ui>) {
    use crate::app_window::{
        CloseWindow, HideApplication, MinimizeWindow, QuitApplication, ShowAbout, ShowHistory,
        ShowHud, ShowMicrophone, ShowModels, ShowPostProcessing, ShowProviders, ShowSettings,
        ShowStatistics, ToggleFullscreen,
    };
    cx.bind_keys([
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-q", QuitApplication, None),
        KeyBinding::new("cmd-h", HideApplication, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullscreen, None),
        KeyBinding::new("cmd-,", ShowSettings, None),
        // Numbered shortcuts follow the sidebar order.
        KeyBinding::new("cmd-1", ShowSettings, None),
        KeyBinding::new("cmd-2", ShowMicrophone, None),
        KeyBinding::new("cmd-3", ShowProviders, None),
        KeyBinding::new("cmd-4", ShowModels, None),
        KeyBinding::new("cmd-5", ShowPostProcessing, None),
        KeyBinding::new("cmd-6", ShowHud, None),
        KeyBinding::new("cmd-7", ShowHistory, None),
        KeyBinding::new("cmd-8", ShowStatistics, None),
    ]);
    cx.bind_keys(crate::text_input::key_bindings());
    let settings_ui = ui.clone();
    cx.on_action(move |_: &ShowSettings, cx| {
        settings_ui.open_pane(cx, |window, cx| window.show_settings(cx));
    });
    let providers_ui = ui.clone();
    cx.on_action(move |_: &ShowProviders, cx| {
        providers_ui.open_pane(cx, |window, cx| window.show_providers(cx));
    });
    let models_ui = ui.clone();
    cx.on_action(move |_: &ShowModels, cx| {
        models_ui.open_pane(cx, |window, cx| window.show_models(cx));
    });
    let microphone_ui = ui.clone();
    cx.on_action(move |_: &ShowMicrophone, cx| {
        microphone_ui.open_pane(cx, |window, cx| window.show_microphone(cx));
    });
    let post_processing_ui = ui.clone();
    cx.on_action(move |_: &ShowPostProcessing, cx| {
        post_processing_ui.open_pane(cx, |window, cx| window.show_post_processing(cx));
    });
    let hud_ui = ui.clone();
    cx.on_action(move |_: &ShowHud, cx| {
        hud_ui.open_pane(cx, |window, cx| window.show_hud(cx));
    });
    let history_ui = ui.clone();
    cx.on_action(move |_: &ShowHistory, cx| {
        history_ui.open_pane(cx, |window, cx| window.show_history(cx));
    });
    let statistics_ui = ui.clone();
    cx.on_action(move |_: &ShowStatistics, cx| {
        statistics_ui.open_pane(cx, |window, cx| window.show_statistics(cx));
    });
    let close_window = ui.app_window.clone();
    cx.on_action(move |_: &CloseWindow, cx| {
        if let Some(window) = close_window.borrow_mut().take() {
            let _ = window.update(cx, |view, window, cx| {
                view.finish_editing(cx);
                window.remove_window();
            });
        }
    });
    cx.on_action(|_: &QuitApplication, cx| cx.quit());
    cx.on_action(|_: &HideApplication, _| crate::app_settings::hide_application());
    cx.on_action(|_: &ShowAbout, _| crate::app_settings::show_about_panel());
    set_app_menus(cx);
}

async fn drive_ui(
    indicator_events: Receiver<DictationIndicatorEvent>,
    ui: Rc<Ui>,
    shutdown: &AtomicBool,
    indicator_enabled: bool,
    cx: &mut gpui::AsyncApp,
) {
    let mut indicator = indicator_enabled.then(DictationIndicatorUi::new);
    let mut paste_notice: Option<crate::paste_notice::PasteNotice> = None;
    // The window also checks while open; this keeps the menu bar badge and
    // restart item current for people who only use the menu bar.
    let mut update_check_at = Instant::now();
    loop {
        if shutdown.load(Ordering::Relaxed) || QUIT_REQUESTED.swap(false, Ordering::Relaxed) {
            let _ = cx.update(|cx| cx.quit());
            return;
        }
        while let Ok(event) = indicator_events.try_recv() {
            // Lifecycle events also drive the status icon when the floating HUD is absent.
            if indicator.is_none() && matches!(event, DictationIndicatorEvent::Meter { .. }) {
                continue;
            }
            if let Err(error) = cx.update(|cx| {
                crate::status_item::handle_indicator(event);
                match event {
                    DictationIndicatorEvent::JobReadyToPaste {
                        copied_to_clipboard,
                        ..
                    }
                    | DictationIndicatorEvent::ReadyToPaste {
                        copied_to_clipboard,
                    } => {
                        crate::status_item::set_ready_to_paste(true);
                        if paste_notice.is_none() {
                            paste_notice = crate::paste_notice::PasteNotice::new()
                                .inspect_err(
                                    |error| tracing::warn!(%error, "could not show paste notice"),
                                )
                                .ok();
                        }
                        if let Some(notice) = &mut paste_notice {
                            notice.show(copied_to_clipboard);
                        }
                    }
                    DictationIndicatorEvent::PasteCommitted => {
                        crate::status_item::set_ready_to_paste(false);
                        if let Some(notice) = &mut paste_notice {
                            notice.hide();
                        }
                    }
                    DictationIndicatorEvent::Preparing | DictationIndicatorEvent::Started => {
                        if let Some(notice) = &mut paste_notice {
                            notice.hide();
                        }
                    }
                    _ => {}
                }
                if let Some(indicator) = &mut indicator {
                    indicator.handle(event, cx);
                }
            }) {
                tracing::error!(%error, "could not update the dictation indicator");
                return;
            }
        }
        if let Some(indicator) = &mut indicator {
            let _ = cx.update(|cx| indicator.follow_pointer(cx));
        }
        if let Some(notice) = &mut paste_notice {
            let _ = cx.update(|_| notice.maintain());
        }
        let mut animating = indicator
            .as_ref()
            .is_some_and(|indicator| !indicator.is_at_rest())
            || paste_notice
                .as_ref()
                .is_some_and(|notice| !notice.is_at_rest());
        if ui.status_actions.is_some() {
            let _ = cx.update(|_| crate::status_item::animate());
            animating |= crate::status_item::is_animating();
            if Instant::now() >= update_check_at {
                update_check_at = Instant::now() + UPDATE_CHECK_INTERVAL;
                let pending = crate::update_check::pending_update();
                let _ = cx.update(|_| crate::status_item::set_pending_update(pending));
            }
        }
        while let Some(action) = ui
            .status_actions
            .as_ref()
            .and_then(|actions| actions.try_recv().ok())
        {
            let result = cx.update(|cx| match action {
                StatusItemAction::OpenSettings => {
                    ui.open_pane(cx, |window, cx| window.show_settings(cx))
                }
                StatusItemAction::OpenProviders => {
                    ui.open_pane(cx, |window, cx| window.show_providers(cx))
                }
                StatusItemAction::OpenModels => {
                    ui.open_pane(cx, |window, cx| window.show_models(cx))
                }
                StatusItemAction::OpenMicrophone => {
                    ui.open_pane(cx, |window, cx| window.show_microphone(cx))
                }
                StatusItemAction::OpenPostProcessing => {
                    ui.open_pane(cx, |window, cx| window.show_post_processing(cx))
                }
                StatusItemAction::OpenHud => ui.open_pane(cx, |window, cx| window.show_hud(cx)),
                StatusItemAction::OpenHistory => {
                    ui.open_pane(cx, |window, cx| window.show_history(cx))
                }
                StatusItemAction::OpenStatistics => {
                    ui.open_pane(cx, |window, cx| window.show_statistics(cx))
                }
                StatusItemAction::PasteLast => {
                    let _ = ui.listener_controls.try_send(ListenerControl::PasteLast);
                }
                StatusItemAction::RestartToUpdate => restart_to_update(&ui, cx),
                StatusItemAction::Quit => cx.quit(),
            });
            if let Err(error) = result {
                tracing::error!(%error, "could not handle a menu bar action");
                return;
            }
        }
        if animating {
            Timer::after(ACTIVE_UI_TURN).await;
        } else {
            // Nothing is recording, pending or fading on screen: sleep until
            // a sender rings instead of turning at display rate.
            UI_WAKE.ring_or(Timer::after(IDLE_UI_TURN)).await;
        }
    }
}

const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Commits any field being edited, then hands off to the installed bundle.
/// When that cannot start, the window's update notice explains why.
fn restart_to_update(ui: &Ui, cx: &mut App) {
    let window = *ui.app_window.borrow();
    if let Some(window) = window {
        let _ = window.update(cx, |view, _, cx| view.finish_editing(cx));
    }
    let restarted = crate::update_check::bundle_path()
        .is_some_and(|bundle| crate::update_check::relaunch_and_quit(&bundle));
    if !restarted {
        ui.open_pane(cx, |window, cx| window.show_settings(cx));
    }
}

fn spawn_hud_preview(sender: crate::dictation_indicator::DictationIndicatorSender) {
    thread::spawn(move || {
        // Cycles in turn: normal speech ending in a check; a muted microphone
        // ("No audio" while recording and at the end); quiet speech ("Low audio").
        for cycle in 0_u64.. {
            let job_id = cycle;
            sender.send(DictationIndicatorEvent::Preparing);
            thread::sleep(Duration::from_millis(900));
            sender.send(DictationIndicatorEvent::Started);
            thread::sleep(Duration::from_millis(450));
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(4) {
                let wave = (started.elapsed().as_secs_f32() * 5.0).sin() * 0.5 + 0.5;
                let gain = match cycle % 3 {
                    0 => 1.0,
                    1 => 0.0,
                    _ => 0.012,
                };
                sender.send(DictationIndicatorEvent::Meter {
                    average: (0.025 + wave * 0.09) * gain,
                    peak: (0.15 + wave * 0.55) * gain,
                });
                thread::sleep(Duration::from_millis(20));
            }
            sender.send(DictationIndicatorEvent::Submitted { job_id });
            if cycle % 3 == 2 {
                sender.send(DictationIndicatorEvent::JobQuiet { job_id });
            }
            sender.send(DictationIndicatorEvent::Transcribing { job_id });
            thread::sleep(Duration::from_millis(900));
            sender.send(if cycle % 3 == 1 {
                DictationIndicatorEvent::JobNoAudio { job_id }
            } else {
                DictationIndicatorEvent::JobCompleted { job_id }
            });
            thread::sleep(Duration::from_secs(2));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn menu_actions_reopen_a_closed_window(cx: &mut gpui::TestAppContext) {
        use crate::app_window::{
            PreviewPane, ShowHistory, ShowHud, ShowMicrophone, ShowModels, ShowPostProcessing,
            ShowProviders, ShowSettings, ShowStatistics,
        };

        let (listener_controls, _controls) = crate::listener::control_channel(1);
        let ui = Rc::new(Ui {
            app_window: Rc::new(RefCell::new(None)),
            listener_start: None,
            history: None,
            listener_controls,
            status_actions: None,
            preview: Some(AppWindowPreview {
                pane: PreviewPane::Settings,
                onboarding: false,
                permissions_missing: false,
                open_history_retention: false,
                appearance: crate::appearance::Appearance::System,
                language: crate::i18n::LanguagePreference::English,
            }),
        });
        cx.update(|cx| install_menus(cx, &ui));
        let actions: [&dyn gpui::Action; 8] = [
            &ShowSettings,
            &ShowMicrophone,
            &ShowPostProcessing,
            &ShowModels,
            &ShowProviders,
            &ShowHud,
            &ShowHistory,
            &ShowStatistics,
        ];
        for action in actions {
            cx.update(|cx| {
                assert!(cx.windows().is_empty());
                assert!(cx.is_action_available(action));
                cx.dispatch_action(action);
            });
            cx.run_until_parked();
            cx.update(|cx| assert_eq!(cx.windows().len(), 1));
            let handle = ui.app_window.borrow_mut().take().unwrap();
            cx.update(|cx| {
                handle
                    .update(cx, |_, window, _| window.remove_window())
                    .unwrap();
            });
        }
    }

    #[test]
    fn dockless_startup_stays_quiet_only_when_setup_and_menu_bar_are_ready() {
        for setup_ready in [false, true] {
            for onboarding_completed in [false, true] {
                for status_item_available in [false, true] {
                    assert_eq!(
                        should_open_app_on_launch(
                            false,
                            setup_ready,
                            onboarding_completed,
                            status_item_available,
                        ),
                        !(setup_ready && onboarding_completed && status_item_available),
                    );
                    assert!(should_open_app_on_launch(
                        true,
                        setup_ready,
                        onboarding_completed,
                        status_item_available,
                    ));
                }
            }
        }
    }
}
