#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod app_paths;
#[cfg(target_os = "macos")]
mod app_settings;
#[cfg(target_os = "macos")]
mod app_window;
#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod context;
#[cfg(target_os = "macos")]
mod desktop;
#[cfg(target_os = "macos")]
mod desktop_ui;
#[cfg(target_os = "macos")]
mod dictation;
#[cfg(target_os = "macos")]
mod dictation_audio;
#[cfg(target_os = "macos")]
mod dictation_indicator;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod events;
#[cfg(target_os = "macos")]
mod feedback;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod history;
#[cfg(target_os = "macos")]
mod instance;
#[cfg(target_os = "macos")]
mod keyboard;
#[cfg(target_os = "macos")]
mod listener;
#[cfg(target_os = "macos")]
mod login_item;
#[cfg(target_os = "macos")]
mod microphone;
#[cfg(target_os = "macos")]
mod onboarding;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod openrouter;
#[cfg(target_os = "macos")]
mod paste;
#[cfg(target_os = "macos")]
mod paste_notice;
#[cfg(target_os = "macos")]
mod permission_guide;
#[cfg(target_os = "macos")]
mod pipeline;
#[cfg(target_os = "macos")]
mod recording_environment;
#[cfg(target_os = "macos")]
mod status_item;
#[cfg(target_os = "macos")]
mod suppression;
#[cfg(target_os = "macos")]
mod text_input;
#[cfg(target_os = "macos")]
mod volume_fade;

use std::sync::atomic::AtomicBool;

use color_eyre::Result;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
use clap::{Parser, Subcommand, ValueEnum};

#[cfg(target_os = "macos")]
#[derive(Parser)]
#[command(version, about = "Voice dictation transcribed through OpenRouter")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[cfg(target_os = "macos")]
#[derive(Subcommand)]
enum Command {
    /// Run the desktop app (the default).
    App {
        /// Override the configured microphone preference order.
        #[arg(long)]
        device: Option<String>,
        /// Preview the dictation HUD without starting recognition.
        #[arg(long)]
        preview_dictation: bool,
    },
    /// Launch an isolated, deterministic UI preview without app services.
    Preview {
        /// UI surface to open.
        #[arg(value_enum)]
        target: AppPreviewTarget,
        /// Show missing post-onboarding permissions in Settings.
        #[arg(long)]
        permissions_missing: bool,
        /// Open the explicit history retention choices in the History preview.
        #[arg(long)]
        open_history_retention: bool,
    },
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, ValueEnum)]
enum AppPreviewTarget {
    DictationHud,
    PasteNotice,
    Onboarding,
    Settings,
    Models,
    History,
    Statistics,
}

#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    color_eyre::install()?;
    let log_dir = app_paths::init_process_logging(&SHUTDOWN)?;
    let event_path = log_dir.join("live.ndjson");
    let command = Cli::parse().command.unwrap_or(Command::App {
        device: None,
        preview_dictation: false,
    });
    match command {
        Command::App {
            device,
            preview_dictation,
        } => {
            let _instance = instance::acquire("listener")?;
            let launch = if preview_dictation {
                desktop::Launch::DictationHudPreview
            } else {
                desktop::Launch::App(desktop::ListenerConfig { event_path, device })
            };
            desktop::run(&SHUTDOWN, launch)
        }
        Command::Preview {
            target,
            permissions_missing,
            open_history_retention,
        } => {
            let pane = match target {
                AppPreviewTarget::DictationHud => {
                    return desktop::run(&SHUTDOWN, desktop::Launch::DictationHudPreview);
                }
                AppPreviewTarget::PasteNotice => {
                    return desktop::run(&SHUTDOWN, desktop::Launch::PasteNoticePreview);
                }
                AppPreviewTarget::Onboarding | AppPreviewTarget::Settings => {
                    app_window::PreviewPane::Settings
                }
                AppPreviewTarget::History => app_window::PreviewPane::History,
                AppPreviewTarget::Models => app_window::PreviewPane::Models,
                AppPreviewTarget::Statistics => app_window::PreviewPane::Statistics,
            };
            desktop::run(
                &SHUTDOWN,
                desktop::Launch::Shell(app_window::AppWindowPreview {
                    pane,
                    onboarding: matches!(target, AppPreviewTarget::Onboarding),
                    permissions_missing,
                    open_history_retention,
                }),
            )
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> Result<()> {
    let _ = &SHUTDOWN;
    color_eyre::eyre::bail!("HEX runs on macOS only")
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn no_subcommand_runs_the_app_and_previews_parse() {
        assert!(Cli::try_parse_from(["hex"]).unwrap().command.is_none());
        assert!(matches!(
            Cli::try_parse_from(["hex", "preview", "settings", "--permissions-missing"])
                .unwrap()
                .command,
            Some(Command::Preview {
                permissions_missing: true,
                ..
            })
        ));
        assert!(Cli::try_parse_from(["hex", "preview", "statistics"]).is_ok());
        assert!(Cli::try_parse_from(["hex", "preview", "paste-notice"]).is_ok());
        assert!(matches!(
            Cli::try_parse_from(["hex", "preview", "models"])
                .unwrap()
                .command,
            Some(Command::Preview {
                target: super::AppPreviewTarget::Models,
                ..
            })
        ));
        assert!(Cli::try_parse_from(["hex", "preview", "voice-action"]).is_err());
    }
}
