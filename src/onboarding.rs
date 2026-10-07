use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::{AnyClass, Bool};
use objc2_foundation::NSString;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionState {
    Ready,
    NeedsRequest,
    NeedsSettings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionKind {
    Microphone,
    InputMonitoring,
    Accessibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionAction {
    RequestMicrophone,
    OpenMicrophoneSettings,
    OpenInputMonitoringSettings,
    OpenAccessibilitySettings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionWarning {
    pub kind: PermissionKind,
    pub action: PermissionAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetupStatus {
    pub microphone: PermissionState,
    pub input_monitoring: PermissionState,
    pub accessibility: PermissionState,
    pub api_key: bool,
}

impl SetupStatus {
    pub fn ready(self) -> bool {
        self.microphone == PermissionState::Ready
            && self.input_monitoring == PermissionState::Ready
            && self.accessibility == PermissionState::Ready
            && self.api_key
    }
}

pub fn permission_warnings(status: SetupStatus) -> Vec<PermissionWarning> {
    let mut warnings = Vec::with_capacity(3);
    match status.microphone {
        PermissionState::Ready => {}
        PermissionState::NeedsRequest => warnings.push(PermissionWarning {
            kind: PermissionKind::Microphone,
            action: PermissionAction::RequestMicrophone,
        }),
        PermissionState::NeedsSettings => warnings.push(PermissionWarning {
            kind: PermissionKind::Microphone,
            action: PermissionAction::OpenMicrophoneSettings,
        }),
    }
    if status.input_monitoring != PermissionState::Ready {
        warnings.push(PermissionWarning {
            kind: PermissionKind::InputMonitoring,
            action: PermissionAction::OpenInputMonitoringSettings,
        });
    }
    if status.accessibility != PermissionState::Ready {
        warnings.push(PermissionWarning {
            kind: PermissionKind::Accessibility,
            action: PermissionAction::OpenAccessibilitySettings,
        });
    }
    warnings
}

pub fn completion_recorded() -> bool {
    let Ok(directory) = crate::app_paths::support_dir() else {
        return false;
    };
    completion_recorded_at(&directory)
}

pub fn record_completion() -> color_eyre::Result<()> {
    let directory = crate::app_paths::support_dir()?;
    record_completion_at(&directory)
}

pub(crate) fn completion_recorded_at(directory: &Path) -> bool {
    if directory.join("onboarding-complete").is_file() {
        return true;
    }
    if directory.join("onboarding-pending").is_file() || !directory.join("settings.json").is_file()
    {
        return false;
    }
    if let Err(error) = record_completion_at(directory) {
        tracing::warn!(%error, "could not migrate onboarding completion");
    }
    true
}

pub(crate) fn record_pending_at(directory: &Path) -> color_eyre::Result<()> {
    fs::create_dir_all(directory)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(directory.join("onboarding-pending"))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.sync_all()?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

pub(crate) fn record_completion_at(directory: &Path) -> color_eyre::Result<()> {
    fs::create_dir_all(directory)?;
    let destination = directory.join("onboarding-complete");
    let temporary = directory.join(".onboarding-complete.tmp");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.sync_all()?;
    fs::rename(temporary, destination)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

/// Permission health plus whether a selected provider key is available. The key
/// lookup may touch the Keychain, so callers poll this off the hot path.
pub fn status() -> SetupStatus {
    status_with_api_key(crate::providers::is_configured())
}

pub fn status_with_api_key(api_key: bool) -> SetupStatus {
    SetupStatus {
        microphone: microphone_state(),
        input_monitoring: settings(CGPreflightListenEventAccess()),
        accessibility: settings(CGPreflightPostEventAccess()),
        api_key,
    }
}

pub fn request_microphone() {
    let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
        tracing::error!("AVCaptureDevice is unavailable");
        return;
    };
    let handler = RcBlock::new(|granted: Bool| {
        tracing::info!(
            granted = granted.as_bool(),
            "microphone permission request finished"
        );
    });
    unsafe {
        let _: () = msg_send![
            class,
            requestAccessForMediaType: &*AVMediaTypeAudio,
            completionHandler: &*handler
        ];
    }
}

pub fn open_permission_settings(permission: &str) {
    let pane = match permission {
        "microphone" => "Privacy_Microphone",
        "input" => "Privacy_ListenEvent",
        "accessibility" => "Privacy_Accessibility",
        _ => "Privacy",
    };
    if let Err(error) = Command::new("/usr/bin/open")
        .arg(format!(
            "x-apple.systempreferences:com.apple.preference.security?{pane}"
        ))
        .spawn()
    {
        tracing::error!(%error, "could not open Privacy & Security settings");
    }
}

fn microphone_state() -> PermissionState {
    let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
        return PermissionState::NeedsRequest;
    };
    let status: isize =
        unsafe { msg_send![class, authorizationStatusForMediaType: &*AVMediaTypeAudio] };
    microphone_authorization_state(status)
}

const fn settings(value: bool) -> PermissionState {
    if value {
        PermissionState::Ready
    } else {
        PermissionState::NeedsSettings
    }
}

const fn microphone_authorization_state(status: isize) -> PermissionState {
    match status {
        0 => PermissionState::NeedsRequest,
        3 => PermissionState::Ready,
        _ => PermissionState::NeedsSettings,
    }
}

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVMediaTypeAudio: *const NSString;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    safe fn CGPreflightListenEventAccess() -> bool;
    safe fn CGPreflightPostEventAccess() -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictation_setup_needs_every_permission_and_a_key() {
        let mut status = SetupStatus {
            microphone: PermissionState::Ready,
            input_monitoring: PermissionState::Ready,
            accessibility: PermissionState::Ready,
            api_key: true,
        };
        assert!(status.ready());
        status.api_key = false;
        assert!(!status.ready());
        status.api_key = true;
        status.microphone = PermissionState::NeedsRequest;
        assert!(!status.ready());
        status.microphone = PermissionState::Ready;
        status.input_monitoring = PermissionState::NeedsSettings;
        assert!(!status.ready());
        status.input_monitoring = PermissionState::Ready;
        status.accessibility = PermissionState::NeedsSettings;
        assert!(!status.ready());
    }

    #[test]
    fn denied_microphone_permission_requires_system_settings() {
        assert_eq!(
            microphone_authorization_state(0),
            PermissionState::NeedsRequest
        );
        assert_eq!(
            microphone_authorization_state(2),
            PermissionState::NeedsSettings
        );
        assert_eq!(microphone_authorization_state(3), PermissionState::Ready);
    }

    #[test]
    fn healthy_permissions_have_no_warning_projection() {
        assert!(permission_warnings(ready_status()).is_empty());
    }

    #[test]
    fn warning_projection_preserves_permission_order_and_actions() {
        let warnings = permission_warnings(SetupStatus {
            microphone: PermissionState::NeedsRequest,
            input_monitoring: PermissionState::NeedsSettings,
            accessibility: PermissionState::NeedsSettings,
            ..ready_status()
        });
        assert_eq!(
            warnings,
            [
                PermissionWarning {
                    kind: PermissionKind::Microphone,
                    action: PermissionAction::RequestMicrophone,
                },
                PermissionWarning {
                    kind: PermissionKind::InputMonitoring,
                    action: PermissionAction::OpenInputMonitoringSettings,
                },
                PermissionWarning {
                    kind: PermissionKind::Accessibility,
                    action: PermissionAction::OpenAccessibilitySettings,
                },
            ]
        );
    }

    #[test]
    fn denied_microphone_maps_to_settings_in_warning_projection() {
        let warning = permission_warnings(SetupStatus {
            microphone: PermissionState::NeedsSettings,
            ..ready_status()
        });
        assert_eq!(warning[0].action, PermissionAction::OpenMicrophoneSettings);
    }

    #[test]
    fn existing_rust_settings_migrate_onboarding_completion() {
        let directory =
            std::env::temp_dir().join(format!("hex-onboarding-migration-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("settings.json"), b"{}").unwrap();

        assert!(completion_recorded_at(&directory));
        let marker = directory.join("onboarding-complete");
        assert!(marker.is_file());
        assert_eq!(
            fs::metadata(marker).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn fresh_install_does_not_skip_onboarding() {
        let directory =
            std::env::temp_dir().join(format!("hex-onboarding-fresh-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();

        assert!(!completion_recorded_at(&directory));
        assert!(!directory.join("onboarding-complete").exists());

        fs::remove_dir_all(directory).unwrap();
    }

    fn ready_status() -> SetupStatus {
        SetupStatus {
            microphone: PermissionState::Ready,
            input_monitoring: PermissionState::Ready,
            accessibility: PermissionState::Ready,
            api_key: true,
        }
    }
}
