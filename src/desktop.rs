//! The GPUI application: the menu bar item, the app window, the dictation
//! HUD, and the listener thread they share a lifetime with.

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
use crate::listener::ListenerControl;
use crate::status_item::StatusItemAction;

pub struct ListenerConfig {
    pub event_path: PathBuf,
    pub device: Option<String>,
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
    listener_controls: SyncSender<ListenerControl>,
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
            Err(error) => tracing::error!(%error, "could not open HEX"),
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
    let show_dock_icon = settings.show_dock_icon;
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
    let (control_sender, control_receiver) = mpsc::sync_channel(8);
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
                tracing::error!(%error, "could not open HEX");
            }
        });
    }
    let quit_worker = listener_worker.clone();
    application.run(move |cx| {
        if let Some(recovery) = recovery {
            cx.set_global(recovery);
        }
        let status_actions = if preview.is_none() && !hud_preview {
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
            tracing::error!(%error, "could not open HEX");
        }
        cx.on_app_quit(move |_| {
            shutdown.store(true, Ordering::Relaxed);
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

fn install_menus(cx: &mut App, ui: &Rc<Ui>) {
    use crate::app_window::{
        CloseWindow, HideApplication, MinimizeWindow, QuitApplication, ShowHistory, ShowHud,
        ShowModels, ShowSettings, ShowStatistics, ToggleFullscreen,
    };
    cx.bind_keys([
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-q", QuitApplication, None),
        KeyBinding::new("cmd-h", HideApplication, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullscreen, None),
        KeyBinding::new("cmd-,", ShowSettings, None),
        KeyBinding::new("cmd-1", ShowSettings, None),
        KeyBinding::new("cmd-2", ShowHistory, None),
        KeyBinding::new("cmd-3", ShowStatistics, None),
        KeyBinding::new("cmd-4", ShowModels, None),
        KeyBinding::new("cmd-5", ShowHud, None),
    ]);
    cx.bind_keys(crate::text_input::key_bindings());
    let settings_ui = ui.clone();
    cx.on_action(move |_: &ShowSettings, cx| {
        settings_ui.open_pane(cx, |window, cx| window.show_settings(cx));
    });
    let models_ui = ui.clone();
    cx.on_action(move |_: &ShowModels, cx| {
        models_ui.open_pane(cx, |window, cx| window.show_models(cx));
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
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
    cx.on_action(|_: &QuitApplication, cx| cx.quit());
    cx.on_action(|_: &HideApplication, _| crate::app_settings::hide_application());
    cx.set_menus(vec![
        Menu {
            name: "Hex".into(),
            items: vec![
                MenuItem::action("Settings", ShowSettings),
                MenuItem::action("Models", ShowModels),
                MenuItem::action("HUD", ShowHud),
                MenuItem::action("History", ShowHistory),
                MenuItem::action("Statistics", ShowStatistics),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide Hex", HideApplication),
                MenuItem::separator(),
                MenuItem::action("Quit Hex", QuitApplication),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![MenuItem::action("Close Window", CloseWindow)],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", MinimizeWindow),
                MenuItem::action("Enter Full Screen", ToggleFullscreen),
            ],
        },
    ]);
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
    loop {
        if shutdown.load(Ordering::Relaxed) {
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
        if ui.status_actions.is_some() {
            let _ = cx.update(|_| crate::status_item::animate());
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
                StatusItemAction::OpenModels => {
                    ui.open_pane(cx, |window, cx| window.show_models(cx))
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
                StatusItemAction::Quit => cx.quit(),
            });
            if let Err(error) = result {
                tracing::error!(%error, "could not handle a menu bar action");
                return;
            }
        }
        Timer::after(Duration::from_millis(16)).await;
    }
}

fn spawn_hud_preview(sender: crate::dictation_indicator::DictationIndicatorSender) {
    thread::spawn(move || {
        loop {
            sender.send(DictationIndicatorEvent::Preparing);
            thread::sleep(Duration::from_millis(900));
            sender.send(DictationIndicatorEvent::Started);
            thread::sleep(Duration::from_millis(450));
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(4) {
                let wave = (started.elapsed().as_secs_f32() * 5.0).sin() * 0.5 + 0.5;
                sender.send(DictationIndicatorEvent::Meter {
                    average: 0.025 + wave * 0.09,
                    peak: 0.15 + wave * 0.55,
                });
                thread::sleep(Duration::from_millis(20));
            }
            sender.send(DictationIndicatorEvent::Submitted { job_id: 0 });
            sender.send(DictationIndicatorEvent::Transcribing { job_id: 0 });
            thread::sleep(Duration::from_millis(900));
            sender.send(DictationIndicatorEvent::JobCompleted { job_id: 0 });
            thread::sleep(Duration::from_secs(1));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn menu_actions_reopen_a_closed_window(cx: &mut gpui::TestAppContext) {
        use crate::app_window::{
            PreviewPane, ShowHistory, ShowHud, ShowModels, ShowSettings, ShowStatistics,
        };

        let (listener_controls, _controls) = mpsc::sync_channel(1);
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
            }),
        });
        cx.update(|cx| install_menus(cx, &ui));
        let actions: [&dyn gpui::Action; 5] = [
            &ShowSettings,
            &ShowModels,
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
