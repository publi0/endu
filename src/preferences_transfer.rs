//! A versioned preferences allowlist. Credentials and local history never travel.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use color_eyre::eyre::{Result, bail, eyre};
use serde::{Deserialize, Serialize};

use crate::app_settings::{
    AppSettings, DictationMode, HotkeyBinding, HotkeyKey, HotkeyModifiers, ModifierSide,
    RecordingAudioBehavior,
};
use crate::hud_settings::{
    HudBrightness, HudColor, HudPosition, HudPreferences, HudScreen, HudSize, MonitorId,
};
use crate::interaction_settings::{DoubleTapSensitivity, SoundVolumes};
use crate::microphone::{ChannelSelection, DevicePreference};
use crate::openrouter::{self, Config, TranscriptionConfig};

pub const MAX_FILE_BYTES: usize = 256 * 1024;
const VERSION: u32 = 1;
const MAX_MODELS: usize = 16;
const MAX_DEVICES: usize = 16;
const MAX_NAME_CHARS: usize = 256;
const MAX_MODEL_CHARS: usize = 200;
const MAX_KEY_LABEL_CHARS: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreferenceBundle {
    version: u32,
    app: AppPreferences,
    transcription: TranscriptionPreferences,
}

// These transfer types deliberately do not embed AppSettings or Config. Adding a
// persisted field to either must never silently expand the export format.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AppPreferences {
    release_microphone_while_idle: bool,
    sounds: SoundPreferences,
    microphone: Option<String>,
    microphone_priority: Vec<DevicePreferences>,
    microphone_channel: Option<ChannelPreferences>,
    recording_audio_behavior: RecordingAudioBehavior,
    #[serde(default)]
    lower_volume_percent: Option<u8>,
    #[serde(default)]
    dictation_mode: Option<DictationMode>,
    #[serde(default)]
    enter_to_submit: Option<bool>,
    #[serde(default)]
    copy_on_paste_failure: Option<bool>,
    double_tap_lock: bool,
    double_tap_only: bool,
    double_tap_sensitivity: DoubleTapSensitivity,
    dictation_hotkey: ShortcutPreferences,
    paste_last_hotkey: Option<ShortcutPreferences>,
    show_dock_icon: bool,
    hud: HudTransfer,
    #[serde(default)]
    post_processing: Option<crate::post_processing::Preferences>,
    #[serde(default)]
    vocabulary: Option<crate::vocabulary::Vocabulary>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SoundPreferences {
    enabled: bool,
    start: f32,
    stop: f32,
    error_cancel: f32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DevicePreferences {
    id: Option<String>,
    name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChannelPreferences {
    device_id: String,
    device_name: String,
    channel: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ShortcutPreferences {
    modifiers: ModifierPreferences,
    key: Option<KeyPreferences>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModifierPreferences {
    control: Option<ModifierSide>,
    option: Option<ModifierSide>,
    shift: Option<ModifierSide>,
    command: Option<ModifierSide>,
    function: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeyPreferences {
    code: u16,
    label: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HudTransfer {
    position: HudPosition,
    recording_color: HudColor,
    transcription_color: HudColor,
    size: HudSize,
    brightness: HudBrightness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    voice_reactive: Option<bool>,
    screen: HudScreen,
    fixed_monitor: Option<MonitorId>,
    // Do not use HudPreferences's deserializer: imports reject, rather than clamp.
    edge_distance: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranscriptionPreferences {
    #[serde(default)]
    model_options: Option<std::collections::BTreeMap<String, crate::providers::ModelOptions>>,
    models: Vec<String>,
    language: String,
    attempt_timeout_seconds: u64,
    total_timeout_seconds: u64,
    chunk_seconds: u64,
    rate_limit_retry_max_wait_ms: u64,
    temperature: Option<f32>,
    trim_silence: bool,
}

pub struct ImportOutcome {
    pub settings: AppSettings,
    pub config: Config,
}

pub fn export_bytes(settings: &AppSettings, config: &Config) -> Result<Vec<u8>> {
    let bundle = PreferenceBundle {
        version: VERSION,
        app: AppPreferences::from_settings(settings),
        transcription: TranscriptionPreferences::from_config(&config.transcription),
    };
    bundle.validate()?;
    let bytes =
        serde_json::to_vec_pretty(&bundle).map_err(|_| eyre!("Could not encode preferences."))?;
    check_size(&bytes)?;
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<PreferenceBundle> {
    check_size(bytes)?;
    // Serde diagnostics can quote arbitrary field names/values from the file.
    // Keep neither that text nor its error source in a user-visible error chain.
    let bundle: PreferenceBundle = serde_json::from_slice(bytes)
        .map_err(|_| eyre!("This is not a valid Hex preferences file."))?;
    bundle.validate()?;
    Ok(bundle)
}

/// Apply a decoded file to an isolated preview without file I/O or runtime effects.
pub fn preview_bundle(
    bundle: PreferenceBundle,
    current: &AppSettings,
    config: &Config,
) -> Result<ImportOutcome> {
    bundle.validate()?;
    let settings = bundle.app.apply_to(current);
    let mut config = config.clone();
    config.transcription = bundle.transcription.into_config(&config.transcription);
    Ok(ImportOutcome { settings, config })
}

pub fn import_bundle(bundle: PreferenceBundle, current: &AppSettings) -> Result<ImportOutcome> {
    let outcome = import_at(
        bundle,
        current,
        &crate::app_settings::path()?,
        &openrouter::config_path()?,
        |settings, path| settings.write_to(path),
        |config, path| openrouter::save_config_at(path, config),
    )?;
    // write_to never applies runtime preferences. Both saves have now succeeded.
    outcome.settings.apply_runtime();
    crate::providers::apply_runtime(&outcome.config);
    Ok(outcome)
}

fn check_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_FILE_BYTES {
        bail!("Preferences files must be no larger than 256 KiB.");
    }
    Ok(())
}

fn import_at(
    bundle: PreferenceBundle,
    current: &AppSettings,
    app_path: &Path,
    config_path: &Path,
    save_app: impl FnOnce(&AppSettings, &Path) -> Result<()>,
    mut save_models: impl FnMut(&Config, &Path) -> Result<()>,
) -> Result<ImportOutcome> {
    bundle.validate()?;
    let settings = bundle.app.apply_to(current);
    openrouter::with_config_edits(|| {
        // Read without load_config_at's template creation so an aborted import
        // can restore the original absence of a Models file as well.
        let (original, existed) = match fs::read(config_path) {
            Ok(bytes) => (
                serde_json::from_slice::<Config>(&bytes).map_err(|_| {
                    eyre!("Existing Models settings are unreadable; import was not applied.")
                })?,
                true,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (Config::default(), false)
            }
            Err(_) => bail!("Could not read existing Models settings; import was not applied."),
        };
        let mut config = original.clone();
        config.transcription = bundle.transcription.into_config(&config.transcription);
        save_models(&config, config_path).map_err(|_| {
            eyre!("Could not save Models preferences; app preferences were not changed.")
        })?;
        if save_app(&settings, app_path).is_err() {
            let rollback = if existed {
                save_models(&original, config_path)
            } else {
                fs::remove_file(config_path).map_err(Into::into)
            };
            if rollback.is_err() {
                bail!(
                    "Could not save app preferences, and restoring Models also failed. Models may have changed; app preferences were not activated."
                );
            }
            bail!(
                "Could not save app preferences. Models were restored; imported preferences were not activated."
            );
        }
        Ok(ImportOutcome { settings, config })
    })
}

impl PreferenceBundle {
    fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            bail!("This preferences file uses an unsupported version.");
        }
        if let Some(profiles) = &self.transcription.model_options {
            crate::providers::validate_profiles(profiles).map_err(|e| eyre!("{e}"))?;
        }
        let app = &self.app;
        if let Some(vocabulary) = &app.vocabulary {
            vocabulary
                .validate()
                .map_err(|message| color_eyre::eyre::eyre!(message))?;
        }
        for volume in [app.sounds.start, app.sounds.stop, app.sounds.error_cancel] {
            if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
                bail!("Sound volumes must be between 0 and 1.");
            }
        }
        if app.hud.edge_distance > crate::hud_settings::MAX_EDGE_DISTANCE {
            bail!("HUD edge distance must be between 0 and 160.");
        }
        if app.hud.screen == HudScreen::FixedMonitor && app.hud.fixed_monitor.is_none() {
            bail!("A fixed HUD screen must include a monitor identifier.");
        }
        let dictation = app.dictation_hotkey.binding();
        app.dictation_hotkey.validate()?;
        if let Some(paste) = &app.paste_last_hotkey {
            paste.validate()?;
            if paste.key.is_none() {
                bail!("Paste Last needs a keyboard key, optionally with modifiers.");
            }
            if dictation.overlaps(&paste.binding()) {
                bail!("Dictation and Paste Last shortcuts must not conflict.");
            }
        }
        if app.lower_volume_percent.is_some_and(|value| value > 100) {
            bail!("Volume while dictating must be between 0 and 100.");
        }
        if app
            .dictation_mode
            .is_some_and(|mode| (mode == DictationMode::DoubleTap) != app.double_tap_lock)
        {
            bail!("Dictation mode and double-tap settings must agree.");
        }
        if app.double_tap_only
            && (app.effective_dictation_mode() != DictationMode::DoubleTap
                || dictation.key.is_none())
        {
            bail!("Double-tap-only requires double-tap locking and a shortcut with a key.");
        }
        if let Some(name) = &app.microphone {
            validate_name(name, MAX_NAME_CHARS, "Microphone names")?;
        }
        if app.microphone_priority.len() > MAX_DEVICES {
            bail!("Microphone priority supports at most 16 devices.");
        }
        let mut devices = HashSet::new();
        for device in &app.microphone_priority {
            validate_name(&device.name, MAX_NAME_CHARS, "Microphone names")?;
            if let Some(id) = &device.id {
                validate_name(id, MAX_NAME_CHARS, "Microphone identifiers")?;
            }
            let identity = match &device.id {
                Some(id) => (true, id.as_str()),
                None => (false, device.name.as_str()),
            };
            if !devices.insert(identity) {
                bail!("Microphone priority must not contain duplicate devices.");
            }
        }
        if let Some(channel) = &app.microphone_channel {
            validate_name(&channel.device_id, MAX_NAME_CHARS, "Microphone identifiers")?;
            validate_name(&channel.device_name, MAX_NAME_CHARS, "Microphone names")?;
            if !(1..=256).contains(&channel.channel) {
                bail!("Microphone channel must be between 1 and 256.");
            }
        }
        let transcription = &self.transcription;
        if transcription.models.is_empty() || transcription.models.len() > MAX_MODELS {
            bail!("Choose between 1 and 16 transcription models.");
        }
        let mut models = HashSet::new();
        for model in &transcription.models {
            validate_name(model, MAX_MODEL_CHARS, "Model identifiers")?;
            if model.chars().any(char::is_whitespace) || !models.insert(model) {
                bail!("Model identifiers must be unique and contain no whitespace.");
            }
        }
        if !openrouter::LANGUAGES
            .iter()
            .any(|(code, _)| *code == transcription.language)
        {
            bail!("The transcription language is unsupported.");
        }
        if !(1..=600).contains(&transcription.attempt_timeout_seconds)
            || !(1..=1_800).contains(&transcription.total_timeout_seconds)
            || !(10..=200).contains(&transcription.chunk_seconds)
            || transcription.rate_limit_retry_max_wait_ms > 60_000
        {
            bail!("Transcription limits must match the supported Advanced settings ranges.");
        }
        if transcription
            .temperature
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            bail!("Temperature must be between 0 and 1.");
        }
        Ok(())
    }
}

fn validate_name(value: &str, limit: usize, field: &'static str) -> Result<()> {
    if value.trim().is_empty()
        || value.chars().count() > limit
        || value.chars().any(char::is_control)
    {
        bail!("{field} must be nonempty, bounded text without control characters.");
    }
    Ok(())
}

impl ShortcutPreferences {
    fn from_binding(binding: &HotkeyBinding) -> Self {
        Self {
            modifiers: ModifierPreferences {
                control: binding.modifiers.control,
                option: binding.modifiers.option,
                shift: binding.modifiers.shift,
                command: binding.modifiers.command,
                function: binding.modifiers.function,
            },
            key: binding.key.as_ref().map(|key| KeyPreferences {
                code: key.code,
                label: key.label.clone(),
            }),
        }
    }

    fn binding(&self) -> HotkeyBinding {
        HotkeyBinding {
            modifiers: HotkeyModifiers {
                control: self.modifiers.control,
                option: self.modifiers.option,
                shift: self.modifiers.shift,
                command: self.modifiers.command,
                function: self.modifiers.function,
            },
            key: self.key.as_ref().map(|key| HotkeyKey {
                code: key.code,
                label: key.label.clone(),
            }),
        }
    }

    fn validate(&self) -> Result<()> {
        let binding = self.binding();
        if binding.is_empty() {
            bail!("Shortcuts must not be empty.");
        }
        if let Some(key) = &self.key {
            if key.code > 127 || (53..=63).contains(&key.code) {
                bail!("Shortcut key code is unsupported or reserved.");
            }
            // Match the shortcut editor's protection against swallowing normal
            // typing. A file's display label is untrusted; verify the physical key.
            if binding.modifiers.is_empty()
                && !matches!(
                    key.code,
                    122 | 120
                        | 99
                        | 118
                        | 96
                        | 97
                        | 98
                        | 100
                        | 101
                        | 109
                        | 103
                        | 111
                        | 105
                        | 107
                        | 113
                        | 106
                        | 64
                        | 79
                        | 80
                        | 90
                )
            {
                bail!("A shortcut without modifiers must use an F1–F20 key.");
            }
            validate_name(&key.label, MAX_KEY_LABEL_CHARS, "Shortcut labels")?;
        }
        Ok(())
    }
}

impl AppPreferences {
    fn effective_dictation_mode(&self) -> DictationMode {
        self.dictation_mode.unwrap_or(if self.double_tap_lock {
            DictationMode::DoubleTap
        } else {
            DictationMode::Hold
        })
    }

    fn from_settings(settings: &AppSettings) -> Self {
        let sounds = settings
            .sound_volumes
            .unwrap_or_else(|| SoundVolumes::from_legacy(settings.sound_effect_volume));
        let hud = settings.hud;
        Self {
            release_microphone_while_idle: settings.release_microphone_while_idle,
            sounds: SoundPreferences {
                enabled: settings.sound_effects,
                start: sounds.start,
                stop: sounds.stop,
                error_cancel: sounds.error_cancel,
            },
            microphone: settings.microphone.clone(),
            microphone_priority: settings
                .microphone_priority
                .iter()
                .map(|device| DevicePreferences {
                    id: device.id.clone(),
                    name: device.name.clone(),
                })
                .collect(),
            microphone_channel: settings.microphone_channel.as_ref().map(|channel| {
                ChannelPreferences {
                    device_id: channel.device_id.clone(),
                    device_name: channel.device_name.clone(),
                    channel: channel.channel,
                }
            }),
            recording_audio_behavior: settings.recording_audio_behavior,
            lower_volume_percent: Some(settings.lower_volume_percent),
            dictation_mode: Some(settings.dictation_mode),
            enter_to_submit: Some(settings.enter_to_submit),
            copy_on_paste_failure: Some(settings.copy_on_paste_failure),
            double_tap_lock: settings.dictation_mode == DictationMode::DoubleTap,
            double_tap_only: settings.dictation_mode == DictationMode::DoubleTap
                && settings.double_tap_only
                && settings.dictation_hotkey.key.is_some(),
            double_tap_sensitivity: settings.double_tap_sensitivity,
            dictation_hotkey: ShortcutPreferences::from_binding(&settings.dictation_hotkey),
            paste_last_hotkey: settings
                .paste_last_hotkey
                .as_ref()
                .map(ShortcutPreferences::from_binding),
            show_dock_icon: settings.show_dock_icon,
            post_processing: Some(settings.post_processing),
            vocabulary: Some(settings.vocabulary.clone()),
            hud: HudTransfer {
                position: hud.position,
                recording_color: hud.recording_color,
                transcription_color: hud.transcription_color,
                size: hud.size,
                brightness: hud.brightness,
                voice_reactive: Some(hud.voice_reactive),
                screen: hud.screen,
                fixed_monitor: hud.fixed_monitor,
                edge_distance: hud.edge_distance,
            },
        }
    }

    fn apply_to(&self, current: &AppSettings) -> AppSettings {
        // Clone only to retain local-only preferences. Every imported assignment
        // is explicit, including sounds that were disabled on the source machine.
        let mut settings = current.clone();
        settings.release_microphone_while_idle = self.release_microphone_while_idle;
        settings.sound_effects = self.sounds.enabled;
        settings.sound_volumes = Some(SoundVolumes {
            start: self.sounds.start,
            stop: self.sounds.stop,
            error_cancel: self.sounds.error_cancel,
        });
        settings.microphone.clone_from(&self.microphone);
        settings.microphone_priority = self
            .microphone_priority
            .iter()
            .map(|device| DevicePreference {
                id: device.id.clone(),
                name: device.name.clone(),
            })
            .collect();
        settings.microphone_channel =
            self.microphone_channel
                .as_ref()
                .map(|channel| ChannelSelection {
                    device_id: channel.device_id.clone(),
                    device_name: channel.device_name.clone(),
                    channel: channel.channel,
                });
        settings.recording_audio_behavior = self.recording_audio_behavior;
        if let Some(value) = self.lower_volume_percent {
            settings.lower_volume_percent = value;
        }
        settings.dictation_mode = self.effective_dictation_mode();
        if let Some(value) = self.enter_to_submit {
            settings.enter_to_submit = value;
        }
        if let Some(value) = self.copy_on_paste_failure {
            settings.copy_on_paste_failure = value;
        }
        settings.double_tap_lock = settings.dictation_mode == DictationMode::DoubleTap;
        settings.double_tap_only = self.double_tap_only;
        settings.double_tap_sensitivity = self.double_tap_sensitivity;
        settings.dictation_hotkey = self.dictation_hotkey.binding();
        settings.paste_last_hotkey = self
            .paste_last_hotkey
            .as_ref()
            .map(ShortcutPreferences::binding);
        settings.show_dock_icon = self.show_dock_icon;
        if let Some(preferences) = self.post_processing {
            settings.post_processing = preferences;
        }
        if let Some(vocabulary) = &self.vocabulary {
            settings.vocabulary = vocabulary.clone();
        }
        let hud = self.hud;
        settings.hud = HudPreferences {
            position: hud.position,
            recording_color: hud.recording_color,
            transcription_color: hud.transcription_color,
            size: hud.size,
            brightness: hud.brightness,
            voice_reactive: hud.voice_reactive.unwrap_or(current.hud.voice_reactive),
            screen: hud.screen,
            fixed_monitor: hud.fixed_monitor,
            edge_distance: hud.edge_distance,
        };
        settings
    }
}

impl TranscriptionPreferences {
    fn from_config(config: &TranscriptionConfig) -> Self {
        Self {
            model_options: Some(config.model_options.clone()),
            models: config.models.clone(),
            language: config.language.clone(),
            attempt_timeout_seconds: config.attempt_timeout_seconds,
            total_timeout_seconds: config.total_timeout_seconds,
            chunk_seconds: config.chunk_seconds,
            rate_limit_retry_max_wait_ms: config.rate_limit_retry_max_wait_ms,
            temperature: config.temperature,
            trim_silence: config.trim_silence,
        }
    }

    fn into_config(self, previous: &TranscriptionConfig) -> TranscriptionConfig {
        TranscriptionConfig {
            model_options: self
                .model_options
                .unwrap_or_else(|| previous.model_options.clone()),
            models: self.models,
            language: self.language,
            attempt_timeout_seconds: self.attempt_timeout_seconds,
            total_timeout_seconds: self.total_timeout_seconds,
            chunk_seconds: self.chunk_seconds,
            rate_limit_retry_max_wait_ms: self.rate_limit_retry_max_wait_ms,
            temperature: self.temperature,
            trim_silence: self.trim_silence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn settings() -> AppSettings {
        // Construct without AppSettings::default's native keyboard lookup.
        AppSettings {
            release_microphone_while_idle: false,
            sound_effects: false,
            sound_effect_volume: 0.4,
            sound_volumes: None,
            microphone: None,
            microphone_priority: Vec::new(),
            microphone_channel: None,
            recording_audio_behavior: RecordingAudioBehavior::DoNothing,
            lower_volume_percent: 80,
            dictation_mode: DictationMode::DoubleTap,
            enter_to_submit: false,
            copy_on_paste_failure: false,
            double_tap_lock: true,
            double_tap_only: false,
            double_tap_sensitivity: DoubleTapSensitivity::Normal,
            dictation_hotkey: HotkeyBinding {
                modifiers: HotkeyModifiers::option(),
                key: None,
            },
            paste_last_hotkey: Some(HotkeyBinding {
                modifiers: HotkeyModifiers {
                    option: Some(ModifierSide::Either),
                    shift: Some(ModifierSide::Either),
                    ..HotkeyModifiers::default()
                },
                key: Some(HotkeyKey {
                    code: 9,
                    label: "V".into(),
                }),
            }),
            show_dock_icon: false,
            history_retention: crate::history::HistoryRetention::Off,
            hud: HudPreferences::default(),
            post_processing: crate::post_processing::Preferences::default(),
            vocabulary: crate::vocabulary::Vocabulary::default(),
        }
    }

    fn bundle() -> PreferenceBundle {
        decode(&export_bytes(&settings(), &Config::default()).unwrap()).unwrap()
    }

    struct Files {
        directory: PathBuf,
        app: PathBuf,
        models: PathBuf,
    }

    impl Files {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "hex-preferences-transfer-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&directory).unwrap();
            Self {
                app: directory.join("settings.json"),
                models: directory.join("openrouter.json"),
                directory,
            }
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn model_profiles_round_trip_and_older_exports_preserve_them() {
        let source = settings();
        let mut config = Config::default();
        config.transcription.model_options.insert(
            "deepgram::nova-3".into(),
            crate::providers::ModelOptions {
                language: "pt".into(),
                streaming: true,
                smart_format: true,
                ..Default::default()
            },
        );
        let bytes = export_bytes(&source, &config).unwrap();
        let imported =
            preview_bundle(decode(&bytes).unwrap(), &source, &Config::default()).unwrap();
        assert_eq!(
            imported.config.transcription.model_options,
            config.transcription.model_options
        );
        let mut legacy: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        legacy["transcription"]
            .as_object_mut()
            .unwrap()
            .remove("model_options");
        let imported = preview_bundle(
            decode(&serde_json::to_vec(&legacy).unwrap()).unwrap(),
            &source,
            &config,
        )
        .unwrap();
        assert_eq!(
            imported.config.transcription.model_options,
            config.transcription.model_options
        );
        legacy["transcription"]["model_options"] =
            json!({"deepgram::nova-3":{"api_key":"never-import"}});
        assert!(decode(&serde_json::to_vec(&legacy).unwrap()).is_err());
    }

    #[test]
    fn exported_preferences_round_trip_without_credentials_or_local_state() {
        let mut source = settings();
        source.microphone = Some("Fixture mic".into());
        source.microphone_priority = vec![DevicePreference {
            id: Some("fixture-uid".into()),
            name: "Fixture mic".into(),
        }];
        source.microphone_channel = Some(ChannelSelection {
            device_id: "fixture-uid".into(),
            device_name: "Fixture mic".into(),
            channel: 2,
        });
        source.hud.edge_distance = 80;
        source.hud.recording_color = HudColor::Green;
        source.double_tap_sensitivity = DoubleTapSensitivity::Tolerant;
        let config = Config {
            api_key: Some("PRIVATE_KEY_MARKER".into()),
            base_url: "https://private-endpoint.example.test".into(),
            ..Config::default()
        };
        let bytes = export_bytes(&source, &config).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for forbidden in [
            "PRIVATE_KEY_MARKER",
            "private-endpoint",
            "\"api_key\"",
            "\"base_url\"",
            "\"history_retention\"",
            "\"history\"",
            "\"audio\"",
            "\"logs\"",
            "\"permissions\"",
            "\"onboarding\"",
            "\"login_item\"",
        ] {
            assert!(!text.contains(forbidden));
        }
        let imported = decode(&bytes).unwrap();
        let mut local = settings();
        local.history_retention = crate::history::HistoryRetention::Week;
        let applied = imported.app.apply_to(&local);
        assert_eq!(applied.history_retention, local.history_retention);
        assert!(!applied.sound_effects);
        assert_eq!(applied.sound_volumes, Some(SoundVolumes::from_legacy(0.4)));
        assert_eq!(applied.microphone_priority, source.microphone_priority);
        assert_eq!(applied.microphone_channel, source.microphone_channel);
        let transferred = Config {
            transcription: imported
                .transcription
                .into_config(&TranscriptionConfig::default()),
            ..Config::default()
        };
        assert_eq!(export_bytes(&applied, &transferred).unwrap(), bytes);
    }

    #[test]
    fn gesture_and_output_preferences_transfer_together() {
        for mode in DictationMode::ALL {
            let mut source = settings();
            source.dictation_mode = mode;
            source.recording_audio_behavior = RecordingAudioBehavior::LowerVolume;
            source.lower_volume_percent = 35;
            source.enter_to_submit = true;
            source.copy_on_paste_failure = true;
            source.double_tap_sensitivity = DoubleTapSensitivity::Tolerant;
            source.post_processing = crate::post_processing::Preferences {
                lowercase: true,
                remove_final_period: true,
                ..Default::default()
            };
            source.vocabulary.terms = vec!["Nimbus-Files".into()];
            source.vocabulary.approximate = true;
            let bytes = export_bytes(&source, &Config::default()).unwrap();
            let imported = preview_bundle(
                decode(&bytes).unwrap(),
                &AppSettings::default(),
                &Config::default(),
            )
            .unwrap();
            assert_eq!(imported.settings.dictation_mode, mode);
            assert_eq!(imported.settings.vocabulary, source.vocabulary);
            assert_eq!(imported.settings.post_processing, source.post_processing);
            assert_eq!(
                imported.settings.double_tap_lock,
                mode == DictationMode::DoubleTap
            );
            assert!(imported.settings.enter_to_submit);
            assert!(imported.settings.copy_on_paste_failure);
            assert_eq!(
                imported.settings.recording_audio_behavior,
                RecordingAudioBehavior::LowerVolume
            );
            assert_eq!(imported.settings.lower_volume_percent, 35);
            assert_eq!(
                imported.settings.double_tap_sensitivity,
                DoubleTapSensitivity::Tolerant
            );
            assert_eq!(
                export_bytes(&imported.settings, &imported.config).unwrap(),
                bytes
            );
        }
    }

    #[test]
    fn voice_reaction_exports_both_choices_and_legacy_imports_preserve_local_choice() {
        for voice_reactive in [false, true] {
            let mut source = settings();
            source.hud.voice_reactive = voice_reactive;
            let bytes = export_bytes(&source, &Config::default()).unwrap();
            let mut local = settings();
            local.hud.voice_reactive = !voice_reactive;
            let imported = decode(&bytes).unwrap();
            assert_eq!(
                imported.app.apply_to(&local).hud.voice_reactive,
                voice_reactive
            );

            let mut legacy: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            legacy["app"]["hud"]
                .as_object_mut()
                .unwrap()
                .remove("voice_reactive");
            let imported = decode(&serde_json::to_vec(&legacy).unwrap()).unwrap();
            assert_eq!(
                imported.app.apply_to(&local).hud.voice_reactive,
                local.hud.voice_reactive
            );
        }
    }

    #[test]
    fn legacy_exports_preserve_gesture_intent_and_unrepresented_preferences() {
        let mut value = serde_json::to_value(bundle()).unwrap();
        for field in [
            "dictation_mode",
            "enter_to_submit",
            "copy_on_paste_failure",
            "lower_volume_percent",
            "post_processing",
        ] {
            value["app"].as_object_mut().unwrap().remove(field);
        }
        let current = AppSettings {
            enter_to_submit: true,
            copy_on_paste_failure: true,
            lower_volume_percent: 40,
            post_processing: crate::post_processing::Preferences {
                single_line: true,
                ..Default::default()
            },
            ..AppSettings::default()
        };
        for (legacy_lock, mode) in [
            (true, DictationMode::DoubleTap),
            (false, DictationMode::Hold),
        ] {
            value["app"]["double_tap_lock"] = json!(legacy_lock);
            let imported = decode(&serde_json::to_vec(&value).unwrap()).unwrap();
            let applied = imported.app.apply_to(&current);
            assert_eq!(applied.dictation_mode, mode);
            assert_eq!(applied.double_tap_lock, legacy_lock);
            assert!(applied.enter_to_submit);
            assert!(applied.copy_on_paste_failure);
            assert_eq!(applied.lower_volume_percent, 40);
            assert_eq!(applied.post_processing, current.post_processing);
        }
    }

    #[test]
    fn malformed_or_untrusted_files_are_rejected_without_echoing_content() {
        let base = serde_json::to_value(bundle()).unwrap();
        let mut cases = Vec::new();
        for pointer in [
            "",
            "/app",
            "/app/sounds",
            "/app/hud",
            "/app/post_processing",
            "/transcription",
            "/app/dictation_hotkey",
            "/app/dictation_hotkey/modifiers",
        ] {
            let mut value = base.clone();
            value.pointer_mut(pointer).unwrap()["PRIVATE_MARKER_FIELD"] =
                json!("PRIVATE_MARKER_VALUE");
            cases.push(value);
        }
        for (pointer, invalid) in [
            ("/version", json!(999)),
            ("/app/sounds/start", json!(1.01)),
            ("/app/sounds/stop", json!(-0.1)),
            ("/app/lower_volume_percent", json!(101)),
            ("/app/dictation_mode", json!("hold")),
            ("/app/hud/edge_distance", json!(161)),
            ("/app/hud/screen", json!("fixed_monitor")),
            ("/app/double_tap_only", json!(true)),
            (
                "/app/dictation_hotkey/key",
                json!({"code":200,"label":"PRIVATE_MARKER_VALUE"}),
            ),
            (
                "/app/dictation_hotkey/key",
                json!({"code":53,"label":"Escape"}),
            ),
            (
                "/app/dictation_hotkey/key",
                json!({"code":9,"label":"x".repeat(MAX_KEY_LABEL_CHARS + 1)}),
            ),
            ("/app/microphone", json!("PRIVATE_MARKER_VALUE\n")),
            (
                "/app/microphone_priority",
                json!([{"id":"duplicate","name":"one"},{"id":"duplicate","name":"two"}]),
            ),
            (
                "/app/microphone_channel",
                json!({"device_id":"fixture","device_name":"Fixture","channel":0}),
            ),
            ("/transcription/models", json!([])),
            (
                "/transcription/models",
                json!(
                    (0..=MAX_MODELS)
                        .map(|index| format!("fixture/{index}"))
                        .collect::<Vec<_>>()
                ),
            ),
            (
                "/transcription/models",
                json!(["x".repeat(MAX_MODEL_CHARS + 1)]),
            ),
            ("/transcription/models", json!(["duplicate", "duplicate"])),
            ("/transcription/language", json!("PRIVATE_MARKER_VALUE")),
            ("/transcription/attempt_timeout_seconds", json!(601)),
            ("/transcription/total_timeout_seconds", json!(1801)),
            ("/transcription/chunk_seconds", json!(201)),
            ("/transcription/rate_limit_retry_max_wait_ms", json!(60001)),
            ("/transcription/temperature", json!(2)),
        ] {
            let mut value = base.clone();
            *value.pointer_mut(pointer).unwrap() = invalid;
            cases.push(value);
        }
        let mut conflict = base.clone();
        conflict["app"]["dictation_hotkey"] = conflict["app"]["paste_last_hotkey"].clone();
        cases.push(conflict);
        for value in cases {
            let error = decode(&serde_json::to_vec(&value).unwrap()).unwrap_err();
            assert!(!format!("{error:#}").contains("PRIVATE_MARKER"));
        }
        assert!(decode(&vec![b' '; MAX_FILE_BYTES + 1]).is_err());
        let error = decode(br#"{"PRIVATE_MARKER_VALUE": "#).unwrap_err();
        assert!(!format!("{error:#}").contains("PRIVATE_MARKER"));
        let mut nonfinite = bundle();
        nonfinite.app.sounds.error_cancel = f32::NAN;
        assert!(nonfinite.validate().is_err());
    }

    #[test]
    fn import_preserves_latest_local_key_endpoint_history_and_onboarding() {
        let files = Files::new();
        let local = settings();
        fs::write(&files.app, serde_json::to_vec(&local).unwrap()).unwrap();
        let marker = files.directory.join("onboarding-complete");
        fs::write(&marker, "existing marker").unwrap();
        let original = Config {
            api_key: Some("LOCAL_TEST_KEY".into()),
            base_url: "https://local.example.test/v1".into(),
            ..Config::default()
        };
        openrouter::save_config_at(&files.models, &original).unwrap();
        let mut imported = bundle();
        imported.transcription.models = vec!["fixture/new-model".into()];
        imported.app.sounds.start = 0.1;
        let outcome = import_at(
            imported,
            &local,
            &files.app,
            &files.models,
            |settings, path| settings.write_to(path),
            |config, path| openrouter::save_config_at(path, config),
        )
        .unwrap();
        assert_eq!(outcome.config.api_key, original.api_key);
        assert_eq!(outcome.config.base_url, original.base_url);
        assert_eq!(outcome.config.transcription.models, ["fixture/new-model"]);
        assert_eq!(outcome.settings.history_retention, local.history_retention);
        assert_eq!(fs::read_to_string(marker).unwrap(), "existing marker");
        assert!(!files.directory.join("onboarding-pending").exists());
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&files.app).unwrap()).unwrap();
        assert_eq!(saved["sound_volumes"]["start"], json!(0.1));
    }

    #[test]
    fn preview_applies_only_transferable_preferences_in_memory() {
        let current = settings();
        let original = Config {
            api_key: Some("LOCAL_PREVIEW_KEY".into()),
            base_url: "https://preview.example.test/v1".into(),
            ..Config::default()
        };
        let mut imported = bundle();
        imported.app.hud.edge_distance = 90;
        imported.transcription.models = vec!["fixture/preview".into()];
        let outcome = preview_bundle(imported, &current, &original).unwrap();
        assert_eq!(outcome.settings.hud.edge_distance, 90);
        assert_eq!(current.hud.edge_distance, 12);
        assert_eq!(
            outcome.settings.history_retention,
            current.history_retention
        );
        assert_eq!(outcome.config.api_key, original.api_key);
        assert_eq!(outcome.config.base_url, original.base_url);
        assert_eq!(outcome.config.transcription.models, ["fixture/preview"]);
        assert_ne!(
            outcome.config.transcription.models,
            original.transcription.models
        );
    }

    #[test]
    fn app_save_failure_rolls_models_back_and_reports_rollback_failure_distinctly() {
        for rollback_fails in [false, true] {
            let files = Files::new();
            let local = settings();
            let app_bytes = serde_json::to_vec(&local).unwrap();
            fs::write(&files.app, &app_bytes).unwrap();
            let original = Config {
                api_key: Some("LOCAL_TEST_KEY".into()),
                ..Config::default()
            };
            openrouter::save_config_at(&files.models, &original).unwrap();
            let mut imported = bundle();
            imported.transcription.models = vec!["fixture/new-model".into()];
            let mut saves = 0;
            let error = import_at(
                imported,
                &local,
                &files.app,
                &files.models,
                |_, _| Err(eyre!("PRIVATE_ERROR_MARKER")),
                |config, path| {
                    saves += 1;
                    if rollback_fails && saves == 2 {
                        bail!("PRIVATE_ERROR_MARKER");
                    }
                    openrouter::save_config_at(path, config)
                },
            )
            .err()
            .unwrap();
            assert_eq!(saves, 2);
            assert!(!format!("{error:#}").contains("PRIVATE_ERROR_MARKER"));
            assert_eq!(
                error.to_string().contains("restoring Models also failed"),
                rollback_fails
            );
            assert_eq!(fs::read(&files.app).unwrap(), app_bytes);
            let after: Config = serde_json::from_slice(&fs::read(&files.models).unwrap()).unwrap();
            assert_eq!(after.api_key, original.api_key);
            if !rollback_fails {
                assert_eq!(after, original);
            }
        }
    }

    #[test]
    fn failed_import_restores_absent_models_and_validation_never_writes() {
        let files = Files::new();
        assert!(
            import_at(
                bundle(),
                &settings(),
                &files.app,
                &files.models,
                |_, _| Err(eyre!("app save failed")),
                |config, path| openrouter::save_config_at(path, config)
            )
            .is_err()
        );
        assert!(!files.models.exists());
        let mut invalid = bundle();
        invalid.app.hud.edge_distance = 500;
        assert!(
            import_at(
                invalid,
                &settings(),
                &files.app,
                &files.models,
                |_, _| panic!("invalid preferences must not write app settings"),
                |_, _| panic!("invalid preferences must not write models")
            )
            .is_err()
        );
        assert!(!files.models.exists());
        assert!(!files.app.exists());
    }

    #[test]
    fn failed_models_save_never_attempts_to_change_app_settings() {
        let files = Files::new();
        assert!(
            import_at(
                bundle(),
                &settings(),
                &files.app,
                &files.models,
                |_, _| panic!("app write must wait for successful Models write"),
                |_, _| Err(eyre!("model write failed"))
            )
            .is_err()
        );
        assert!(!files.app.exists());
    }

    #[test]
    fn bare_typing_keys_and_forged_function_labels_are_rejected() {
        let mut imported = bundle();
        imported.app.dictation_hotkey.modifiers = ModifierPreferences {
            control: None,
            option: None,
            shift: None,
            command: None,
            function: false,
        };
        for label in ["A", "F13"] {
            imported.app.dictation_hotkey.key = Some(KeyPreferences {
                code: 0,
                label: label.into(),
            });
            assert!(imported.validate().is_err());
        }
        imported.app.dictation_hotkey.key = Some(KeyPreferences {
            code: 105,
            label: "F13".into(),
        });
        assert!(imported.validate().is_ok());
        imported.app.dictation_hotkey.key = Some(KeyPreferences {
            code: 0,
            label: "A".into(),
        });
        imported.app.dictation_hotkey.modifiers.command = Some(ModifierSide::Either);
        assert!(imported.validate().is_ok());
    }

    #[test]
    fn model_readers_wait_until_the_import_commits_or_rolls_back() {
        use std::sync::mpsc;
        use std::time::Duration;
        const TIMEOUT: Duration = Duration::from_secs(5);
        for app_succeeds in [false, true] {
            let files = Files::new();
            let original = Config::default();
            openrouter::save_config_at(&files.models, &original).unwrap();
            let current = settings();
            fs::write(&files.app, serde_json::to_vec(&current).unwrap()).unwrap();
            let mut imported = bundle();
            imported.transcription.models = vec!["fixture/imported".into()];
            std::thread::scope(|scope| {
                let (pending, app_save_pending) = mpsc::channel();
                let (finish, may_finish) = mpsc::channel();
                let app_path = &files.app;
                let models_path = &files.models;
                let importer = scope.spawn(move || {
                    import_at(
                        imported,
                        &current,
                        app_path,
                        models_path,
                        |settings, path| {
                            pending.send(()).unwrap();
                            may_finish.recv_timeout(TIMEOUT).unwrap();
                            if app_succeeds {
                                settings.write_to(path)
                            } else {
                                Err(eyre!("test app save failure"))
                            }
                        },
                        |config, path| openrouter::save_config_at(path, config),
                    )
                });
                app_save_pending.recv_timeout(TIMEOUT).unwrap();
                let (reading, read_started) = mpsc::channel();
                let (read, read_done) = mpsc::channel();
                let reader = scope.spawn(move || {
                    reading.send(()).unwrap();
                    read.send(openrouter::load_config_at(models_path)).unwrap();
                });
                read_started.recv_timeout(TIMEOUT).unwrap();
                let early = read_done.recv_timeout(Duration::from_millis(50));
                // Always release the writer before asserting, even if the reader regresses.
                finish.send(()).unwrap();
                let outcome = importer.join().unwrap();
                assert_eq!(outcome.is_ok(), app_succeeds);
                reader.join().unwrap();
                assert!(
                    matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
                    "reader observed an unfinished import"
                );
                let config = read_done.recv_timeout(TIMEOUT).unwrap().unwrap();
                if app_succeeds {
                    assert_eq!(config.transcription.models, ["fixture/imported"]);
                } else {
                    assert_eq!(config, original);
                }
            });
        }
    }
}
