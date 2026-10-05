use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use color_eyre::eyre::{Result, eyre};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use serde::{Deserialize, Serialize};

static COPY_ON_PASTE_FAILURE: AtomicBool = AtomicBool::new(false);
static RECORDING_AUDIO_BEHAVIOR: AtomicU8 = AtomicU8::new(0);
static DOUBLE_TAP_SENSITIVITY: AtomicU8 = AtomicU8::new(1);
static LOWER_VOLUME_PERCENT: AtomicU8 = AtomicU8::new(80);
static DICTATION_MODE: AtomicU8 = AtomicU8::new(0);
static ENTER_TO_SUBMIT: AtomicBool = AtomicBool::new(false);
static DOUBLE_TAP_ONLY: AtomicBool = AtomicBool::new(false);
static RELEASE_MICROPHONE_WHILE_IDLE: AtomicBool = AtomicBool::new(false);
static HOTKEYS: OnceLock<RwLock<RuntimeHotkeys>> = OnceLock::new();
static PASTE_KEY_CODE: OnceLock<u16> = OnceLock::new();
static HOTKEY_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
static SETTINGS_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static MICROPHONE_SELECTION: OnceLock<RwLock<RuntimeMicrophoneSelection>> = OnceLock::new();

#[derive(Default)]
struct RuntimeMicrophoneSelection {
    revision: u64,
    device: Option<String>,
    channel: Option<crate::microphone::ChannelSelection>,
    priority: Vec<crate::microphone::DevicePreference>,
}

pub const SHIFT_KEY_MASK: u64 = 1 << 17;
pub const CONTROL_KEY_MASK: u64 = 1 << 18;
pub const OPTION_KEY_MASK: u64 = 1 << 19;
pub const COMMAND_KEY_MASK: u64 = 1 << 20;
pub const FUNCTION_KEY_MASK: u64 = 1 << 23;
pub const HOTKEY_MODIFIERS_MASK: u64 =
    SHIFT_KEY_MASK | CONTROL_KEY_MASK | OPTION_KEY_MASK | COMMAND_KEY_MASK | FUNCTION_KEY_MASK;
pub const LEFT_CONTROL_MASK: u64 = 0x0000_0001;
pub const LEFT_SHIFT_MASK: u64 = 0x0000_0002;
pub const RIGHT_SHIFT_MASK: u64 = 0x0000_0004;
pub const LEFT_COMMAND_MASK: u64 = 0x0000_0008;
pub const RIGHT_COMMAND_MASK: u64 = 0x0000_0010;
pub const LEFT_OPTION_MASK: u64 = 0x0000_0020;
pub const RIGHT_OPTION_MASK: u64 = 0x0000_0040;
pub const RIGHT_CONTROL_MASK: u64 = 0x0000_2000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModifierSide {
    Left,
    Right,
    #[default]
    Either,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct HotkeyModifiers {
    #[serde(deserialize_with = "deserialize_modifier_side")]
    pub control: Option<ModifierSide>,
    #[serde(deserialize_with = "deserialize_modifier_side")]
    pub option: Option<ModifierSide>,
    #[serde(deserialize_with = "deserialize_modifier_side")]
    pub shift: Option<ModifierSide>,
    #[serde(deserialize_with = "deserialize_modifier_side")]
    pub command: Option<ModifierSide>,
    pub function: bool,
}

fn deserialize_modifier_side<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<ModifierSide>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Bool(false)) => Ok(None),
        Some(serde_json::Value::Bool(true)) => Ok(Some(ModifierSide::Either)),
        Some(serde_json::Value::String(value)) => {
            serde_json::from_value(serde_json::Value::String(value))
                .map(Some)
                .map_err(D::Error::custom)
        }
        Some(_) => Err(D::Error::custom(
            "modifier side must be left, right, either, or null",
        )),
    }
}

impl HotkeyModifiers {
    pub const fn option() -> Self {
        Self {
            option: Some(ModifierSide::Either),
            control: None,
            shift: None,
            command: None,
            function: false,
        }
    }

    pub const fn is_empty(self) -> bool {
        self.control.is_none()
            && self.option.is_none()
            && self.shift.is_none()
            && self.command.is_none()
            && !self.function
    }

    pub const fn count(self) -> u8 {
        self.control.is_some() as u8
            + self.option.is_some() as u8
            + self.shift.is_some() as u8
            + self.command.is_some() as u8
            + self.function as u8
    }

    pub const fn without_side_constraints(self) -> Self {
        Self {
            control: either_side(self.control),
            option: either_side(self.option),
            shift: either_side(self.shift),
            command: either_side(self.command),
            function: self.function,
        }
    }

    fn contains(self, required: Self) -> bool {
        modifier_contains(self.control, required.control)
            && modifier_contains(self.option, required.option)
            && modifier_contains(self.shift, required.shift)
            && modifier_contains(self.command, required.command)
            && (!required.function || self.function)
    }

    fn keycaps(self) -> impl Iterator<Item = &'static str> {
        [
            (self.function, "fn"),
            (self.control.is_some(), side_keycap(self.control, "⌃")),
            (self.option.is_some(), side_keycap(self.option, "⌥")),
            (self.shift.is_some(), side_keycap(self.shift, "⇧")),
            (self.command.is_some(), side_keycap(self.command, "⌘")),
        ]
        .into_iter()
        .filter_map(|(enabled, label)| enabled.then_some(label))
    }
}

const fn either_side(side: Option<ModifierSide>) -> Option<ModifierSide> {
    match side {
        Some(_) => Some(ModifierSide::Either),
        None => None,
    }
}

fn modifier_contains(candidate: Option<ModifierSide>, required: Option<ModifierSide>) -> bool {
    match (candidate, required) {
        (_, None) => true,
        (Some(ModifierSide::Either), Some(_)) | (Some(_), Some(ModifierSide::Either)) => true,
        (Some(candidate), Some(required)) => candidate == required,
        (None, Some(_)) => false,
    }
}

fn side_keycap(side: Option<ModifierSide>, symbol: &'static str) -> &'static str {
    match (side, symbol) {
        (Some(ModifierSide::Left), "⌃") => "L⌃",
        (Some(ModifierSide::Right), "⌃") => "R⌃",
        (Some(ModifierSide::Left), "⌥") => "L⌥",
        (Some(ModifierSide::Right), "⌥") => "R⌥",
        (Some(ModifierSide::Left), "⇧") => "L⇧",
        (Some(ModifierSide::Right), "⇧") => "R⇧",
        (Some(ModifierSide::Left), "⌘") => "L⌘",
        (Some(ModifierSide::Right), "⌘") => "R⌘",
        _ => symbol,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HotkeyKey {
    pub code: u16,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct HotkeyBinding {
    pub modifiers: HotkeyModifiers,
    pub key: Option<HotkeyKey>,
}

impl Default for HotkeyBinding {
    fn default() -> Self {
        Self {
            modifiers: HotkeyModifiers::option(),
            key: None,
        }
    }
}

impl HotkeyBinding {
    pub fn paste_last_default() -> Self {
        Self {
            modifiers: HotkeyModifiers {
                option: Some(ModifierSide::Either),
                shift: Some(ModifierSide::Either),
                ..Default::default()
            },
            key: Some(HotkeyKey {
                code: paste_key_code(),
                label: "V".into(),
            }),
        }
    }
}

fn paste_key_code() -> u16 {
    *PASTE_KEY_CODE.get_or_init(|| crate::keyboard::key_code_for('v').unwrap_or(9))
}

impl HotkeyBinding {
    pub fn keycaps(&self) -> Vec<String> {
        self.modifiers
            .keycaps()
            .map(str::to_string)
            .chain(self.key.iter().map(|key| key.label.clone()))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.modifiers.is_empty() && self.key.is_none()
    }

    pub fn runtime(&self) -> RuntimeHotkey {
        RuntimeHotkey {
            modifiers: self.modifiers,
            key_code: self.key.as_ref().map(|key| key.code),
        }
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.key.as_ref().map(|key| key.code) == other.key.as_ref().map(|key| key.code)
            && modifiers_overlap(self.modifiers, other.modifiers)
    }
}

fn modifiers_overlap(left: HotkeyModifiers, right: HotkeyModifiers) -> bool {
    modifier_overlap(left.control, right.control)
        && modifier_overlap(left.option, right.option)
        && modifier_overlap(left.shift, right.shift)
        && modifier_overlap(left.command, right.command)
        && left.function == right.function
}

fn modifier_overlap(left: Option<ModifierSide>, right: Option<ModifierSide>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(ModifierSide::Either), Some(_)) | (Some(_), Some(ModifierSide::Either)) => true,
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeHotkey {
    pub modifiers: HotkeyModifiers,
    pub key_code: Option<u16>,
}

impl RuntimeHotkey {
    pub fn is_empty(self) -> bool {
        self.modifiers.is_empty() && self.key_code.is_none()
    }

    pub fn exact_modifiers(self, flags: u64) -> bool {
        modifiers_from_flags(flags).matches_exactly(self.modifiers)
            && self.required_modifiers_down(flags)
    }

    pub fn required_modifiers_down(self, flags: u64) -> bool {
        modifiers_from_flags(flags).contains(self.modifiers)
            // `Either` also represents side-less input and both sides held. Only the
            // original device flags distinguish those cases for a sided binding.
            && [
                (self.modifiers.control, LEFT_CONTROL_MASK, RIGHT_CONTROL_MASK),
                (self.modifiers.option, LEFT_OPTION_MASK, RIGHT_OPTION_MASK),
                (self.modifiers.shift, LEFT_SHIFT_MASK, RIGHT_SHIFT_MASK),
                (self.modifiers.command, LEFT_COMMAND_MASK, RIGHT_COMMAND_MASK),
            ]
            .into_iter()
            .all(|(side, left, right)| match side {
                Some(ModifierSide::Left) => flags & left != 0,
                Some(ModifierSide::Right) => flags & right != 0,
                Some(ModifierSide::Either) | None => true,
            })
    }

    pub fn matches_key_press(self, code: u16, flags: u64) -> bool {
        self.key_code == Some(code) && self.exact_modifiers(flags)
    }
}

impl HotkeyModifiers {
    pub fn matches_exactly(self, expected: Self) -> bool {
        modifiers_overlap(self, expected)
            && self.control.is_some() == expected.control.is_some()
            && self.option.is_some() == expected.option.is_some()
            && self.shift.is_some() == expected.shift.is_some()
            && self.command.is_some() == expected.command.is_some()
            && self.function == expected.function
    }
}

pub fn modifiers_from_flags(flags: u64) -> HotkeyModifiers {
    HotkeyModifiers {
        control: side_from_flags(
            flags,
            CONTROL_KEY_MASK,
            LEFT_CONTROL_MASK,
            RIGHT_CONTROL_MASK,
        ),
        option: side_from_flags(flags, OPTION_KEY_MASK, LEFT_OPTION_MASK, RIGHT_OPTION_MASK),
        shift: side_from_flags(flags, SHIFT_KEY_MASK, LEFT_SHIFT_MASK, RIGHT_SHIFT_MASK),
        command: side_from_flags(
            flags,
            COMMAND_KEY_MASK,
            LEFT_COMMAND_MASK,
            RIGHT_COMMAND_MASK,
        ),
        function: flags & FUNCTION_KEY_MASK != 0,
    }
}

fn side_from_flags(flags: u64, general: u64, left: u64, right: u64) -> Option<ModifierSide> {
    match (flags & left != 0, flags & right != 0) {
        (true, false) => Some(ModifierSide::Left),
        (false, true) => Some(ModifierSide::Right),
        (true, true) | (false, false) if flags & general != 0 => Some(ModifierSide::Either),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeHotkeys {
    pub dictation: RuntimeHotkey,
    pub paste_last: Option<RuntimeHotkey>,
}

impl Default for RuntimeHotkeys {
    fn default() -> Self {
        Self {
            dictation: HotkeyBinding::default().runtime(),
            paste_last: Some(HotkeyBinding::paste_last_default().runtime()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingAudioBehavior {
    Mute,
    LowerVolume,
    PauseMedia,
    #[default]
    DoNothing,
}

impl RecordingAudioBehavior {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mute => "Mute",
            Self::LowerVolume => "Lower",
            Self::PauseMedia => "Pause",
            Self::DoNothing => "Keep",
        }
    }

    const fn encoded(self) -> u8 {
        match self {
            Self::Mute => 0,
            Self::LowerVolume => 3,
            Self::PauseMedia => 1,
            Self::DoNothing => 2,
        }
    }

    fn decode(value: u8) -> Self {
        match value {
            1 => Self::PauseMedia,
            2 => Self::DoNothing,
            3 => Self::LowerVolume,
            _ => Self::Mute,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum DictationMode {
    #[default]
    TapOrHold,
    Hold,
    DoubleTap,
}

impl DictationMode {
    pub const ALL: [Self; 3] = [Self::TapOrHold, Self::Hold, Self::DoubleTap];

    pub fn label(self) -> &'static str {
        match self {
            Self::TapOrHold => "Tap or hold",
            Self::Hold => "Hold only",
            Self::DoubleTap => "Double tap",
        }
    }

    fn decode(value: u8) -> Self {
        match value {
            1 => Self::Hold,
            2 => Self::DoubleTap,
            _ => Self::TapOrHold,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AppSettings {
    pub release_microphone_while_idle: bool,
    pub sound_effects: bool,
    pub sound_effect_volume: f32,
    pub sound_volumes: Option<crate::interaction_settings::SoundVolumes>,
    pub microphone: Option<String>,
    pub microphone_priority: Vec<crate::microphone::DevicePreference>,
    pub microphone_channel: Option<crate::microphone::ChannelSelection>,
    pub recording_audio_behavior: RecordingAudioBehavior,
    pub lower_volume_percent: u8,
    pub dictation_mode: DictationMode,
    pub enter_to_submit: bool,
    pub double_tap_lock: bool,
    pub double_tap_only: bool,
    pub double_tap_sensitivity: crate::interaction_settings::DoubleTapSensitivity,
    pub dictation_hotkey: HotkeyBinding,
    pub paste_last_hotkey: Option<HotkeyBinding>,
    pub copy_on_paste_failure: bool,
    pub show_dock_icon: bool,
    pub history_retention: crate::history::HistoryRetention,
    pub hud: crate::hud_settings::HudPreferences,
    pub post_processing: crate::post_processing::Preferences,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            release_microphone_while_idle: false,
            sound_effects: true,
            sound_effect_volume: 0.5,
            sound_volumes: None,
            microphone: None,
            microphone_priority: Vec::new(),
            microphone_channel: None,
            recording_audio_behavior: RecordingAudioBehavior::DoNothing,
            lower_volume_percent: 80,
            dictation_mode: DictationMode::TapOrHold,
            enter_to_submit: false,
            double_tap_lock: false,
            double_tap_only: false,
            double_tap_sensitivity: crate::interaction_settings::DoubleTapSensitivity::default(),
            dictation_hotkey: HotkeyBinding::default(),
            paste_last_hotkey: Some(HotkeyBinding::paste_last_default()),
            copy_on_paste_failure: false,
            show_dock_icon: true,
            history_retention: crate::history::HistoryRetention::default(),
            hud: crate::hud_settings::HudPreferences::default(),
            post_processing: crate::post_processing::Preferences::default(),
        }
    }
}

impl AppSettings {
    /// Persist the entire candidate before replacing the current editor value.
    /// The candidate may include unrelated debounced edits, which must survive
    /// both failed immediate changes and successful commits.
    pub(crate) fn commit_with(
        &mut self,
        candidate: Self,
        save: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<()> {
        save(&candidate)?;
        *self = candidate;
        Ok(())
    }

    fn normalize_double_tap_settings(&mut self) {
        self.double_tap_lock = self.dictation_mode == DictationMode::DoubleTap;
        if !self.double_tap_lock || self.dictation_hotkey.key.is_none() {
            self.double_tap_only = false;
        }
    }

    pub fn load() -> Result<Self> {
        let settings = Self::load_from(&path()?)?;
        settings.apply_runtime();
        Ok(settings)
    }

    fn load_from(path: &std::path::Path) -> Result<Self> {
        match fs::read(path) {
            Ok(data) => {
                let mut settings: Self = serde_json::from_slice(&data)?;
                let loaded = serde_json::to_value(&settings)?;
                let raw: serde_json::Value = serde_json::from_slice(&data)?;
                if raw.get("dictation_mode").is_none() {
                    // The former default becomes tap-or-hold; keep explicit hold-only
                    // and double-tap-only choices made in older versions.
                    settings.dictation_mode = if raw
                        .get("double_tap_lock")
                        .and_then(serde_json::Value::as_bool)
                        == Some(false)
                    {
                        DictationMode::Hold
                    } else if settings.double_tap_only {
                        DictationMode::DoubleTap
                    } else {
                        DictationMode::TapOrHold
                    };
                }
                settings.normalize_double_tap_settings();
                settings.lower_volume_percent = settings.lower_volume_percent.min(100);
                if serde_json::to_value(&settings)? != loaded
                    && let Err(error) = settings.write_to(path)
                {
                    tracing::warn!(%error, "could not persist normalized settings");
                }
                Ok(settings)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = path()?;
        self.write_to(&path)?;
        self.apply_runtime();
        Ok(())
    }

    pub(crate) fn write_to(&self, path: &std::path::Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| eyre!("settings path has no parent"))?;
        fs::create_dir_all(parent)?;
        if !path.exists() {
            // Saving new preferences does not imply onboarding completion.
            crate::onboarding::record_pending_at(parent)?;
        }
        let temporary = settings_temporary_path(path);
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    pub fn effective_sound_volumes(&self) -> crate::interaction_settings::SoundVolumes {
        if !self.sound_effects {
            return crate::interaction_settings::SoundVolumes::from_legacy(0.0);
        }
        self.sound_volumes
            .unwrap_or_else(|| {
                crate::interaction_settings::SoundVolumes::from_legacy(self.sound_effect_volume)
            })
            .normalized()
    }

    pub(crate) fn apply_runtime(&self) {
        self.hud.apply_runtime();
        self.post_processing.apply_runtime();
        COPY_ON_PASTE_FAILURE.store(self.copy_on_paste_failure, Ordering::Release);
        RELEASE_MICROPHONE_WHILE_IDLE.store(self.release_microphone_while_idle, Ordering::Release);
        crate::feedback::set_enabled(self.sound_effects);
        crate::feedback::set_volumes(self.effective_sound_volumes());
        DOUBLE_TAP_SENSITIVITY.store(
            match self.double_tap_sensitivity {
                crate::interaction_settings::DoubleTapSensitivity::Short => 0,
                crate::interaction_settings::DoubleTapSensitivity::Normal => 1,
                crate::interaction_settings::DoubleTapSensitivity::Tolerant => 2,
            },
            Ordering::Relaxed,
        );
        RECORDING_AUDIO_BEHAVIOR.store(self.recording_audio_behavior.encoded(), Ordering::Relaxed);
        LOWER_VOLUME_PERCENT.store(self.lower_volume_percent.min(100), Ordering::Relaxed);
        DICTATION_MODE.store(self.dictation_mode as u8, Ordering::Release);
        ENTER_TO_SUBMIT.store(self.enter_to_submit, Ordering::Release);
        DOUBLE_TAP_ONLY.store(
            self.dictation_mode == DictationMode::DoubleTap
                && self.double_tap_only
                && self.dictation_hotkey.key.is_some(),
            Ordering::Relaxed,
        );
        *HOTKEYS
            .get_or_init(Default::default)
            .write()
            .unwrap_or_else(|error| error.into_inner()) = self.runtime_hotkeys();
        set_microphone_selection(
            self.microphone.as_deref(),
            self.microphone_channel.as_ref(),
            &self.microphone_priority,
        );
    }

    pub fn runtime_hotkeys(&self) -> RuntimeHotkeys {
        RuntimeHotkeys {
            dictation: self.dictation_hotkey.runtime(),
            paste_last: self.paste_last_hotkey.as_ref().map(HotkeyBinding::runtime),
        }
    }
}

fn settings_temporary_path(path: &std::path::Path) -> PathBuf {
    let sequence = SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("json.{}.{}.tmp", std::process::id(), sequence))
}

pub fn hotkey_conflicts(
    candidate: &HotkeyBinding,
    others: impl IntoIterator<Item = HotkeyBinding>,
) -> bool {
    others
        .into_iter()
        .any(|binding| candidate.overlaps(&binding))
}

fn set_microphone_selection(
    device: Option<&str>,
    channel: Option<&crate::microphone::ChannelSelection>,
    priority: &[crate::microphone::DevicePreference],
) {
    let device = device
        .map(str::trim)
        .filter(|device| !device.is_empty())
        .map(str::to_string);
    let state = MICROPHONE_SELECTION.get_or_init(Default::default);
    let mut state = state.write().unwrap_or_else(|error| error.into_inner());
    if state.device != device || state.channel.as_ref() != channel || state.priority != priority {
        state.revision = state.revision.wrapping_add(1);
        state.device = device;
        state.channel = channel.cloned();
        state.priority = priority.to_vec();
    }
}

pub fn microphone_selection() -> (
    u64,
    Option<String>,
    Vec<crate::microphone::DevicePreference>,
) {
    let state = MICROPHONE_SELECTION.get_or_init(Default::default);
    let state = state.read().unwrap_or_else(|error| error.into_inner());
    (state.revision, state.device.clone(), state.priority.clone())
}

pub fn microphone_channel(device_id: Option<&str>) -> Option<u16> {
    let state = MICROPHONE_SELECTION
        .get_or_init(Default::default)
        .read()
        .unwrap_or_else(|error| error.into_inner());
    state
        .channel
        .as_ref()
        .filter(|selection| Some(selection.device_id.as_str()) == device_id)
        .map(|selection| selection.channel)
}

pub fn recording_audio_behavior() -> RecordingAudioBehavior {
    RecordingAudioBehavior::decode(RECORDING_AUDIO_BEHAVIOR.load(Ordering::Relaxed))
}

pub fn lower_volume_percent() -> u8 {
    LOWER_VOLUME_PERCENT.load(Ordering::Relaxed).min(100)
}

pub fn copy_on_paste_failure() -> bool {
    COPY_ON_PASTE_FAILURE.load(Ordering::Acquire)
}

pub fn release_microphone_while_idle() -> bool {
    RELEASE_MICROPHONE_WHILE_IDLE.load(Ordering::Acquire)
}

pub fn dictation_mode() -> DictationMode {
    DictationMode::decode(DICTATION_MODE.load(Ordering::Acquire))
}

pub fn enter_to_submit() -> bool {
    ENTER_TO_SUBMIT.load(Ordering::Acquire)
}

pub fn double_tap_sensitivity() -> crate::interaction_settings::DoubleTapSensitivity {
    use crate::interaction_settings::DoubleTapSensitivity;
    match DOUBLE_TAP_SENSITIVITY.load(Ordering::Relaxed) {
        0 => DoubleTapSensitivity::Short,
        2 => DoubleTapSensitivity::Tolerant,
        _ => DoubleTapSensitivity::Normal,
    }
}

pub fn double_tap_only() -> bool {
    DOUBLE_TAP_ONLY.load(Ordering::Relaxed)
}

pub fn runtime_hotkeys() -> RuntimeHotkeys {
    *HOTKEYS
        .get_or_init(Default::default)
        .read()
        .unwrap_or_else(|error| error.into_inner())
}

pub fn dictation_hotkey() -> RuntimeHotkey {
    runtime_hotkeys().dictation
}

pub fn hotkey_capture_active() -> bool {
    HOTKEY_CAPTURE_ACTIVE.load(Ordering::Acquire)
}

pub fn set_hotkey_capture_active(active: bool) {
    HOTKEY_CAPTURE_ACTIVE.store(active, Ordering::Release);
}

/// The Dock policy while no Settings window is open. An open window always
/// shows the icon: hiding it deactivates HEX asynchronously, which would drop
/// that window behind other applications.
pub fn dock_icon_visible(show_dock_icon: bool, status_item_available: bool) -> bool {
    show_dock_icon || !status_item_available
}

pub fn set_dock_icon_visible(visible: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("Dock icon visibility must be changed on the main thread");
        return;
    };
    let application = NSApplication::sharedApplication(mtm);
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    if !application.setActivationPolicy(policy) {
        tracing::warn!(visible, "could not change Dock icon visibility");
    }
}

pub fn hide_application() {
    let Some(marker) = MainThreadMarker::new() else {
        tracing::warn!("cannot hide HEX outside the main thread");
        return;
    };
    NSApplication::sharedApplication(marker).hide(None);
}

pub(crate) fn path() -> Result<PathBuf> {
    Ok(crate::app_paths::support_dir()?.join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_receive_defaults() {
        let settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(!settings.release_microphone_while_idle);
        assert!(!settings.copy_on_paste_failure);
        assert!(settings.sound_effects);
        assert_eq!(settings.sound_effect_volume, 0.5);
        assert_eq!(settings.lower_volume_percent, 80);
        assert_eq!(settings.microphone, None);
        assert_eq!(settings.microphone_channel, None);
        assert_eq!(
            settings.recording_audio_behavior,
            RecordingAudioBehavior::DoNothing
        );
        assert_eq!(settings.dictation_mode, DictationMode::TapOrHold);
        assert!(!settings.enter_to_submit);
        assert!(!settings.double_tap_lock);
        assert!(!settings.double_tap_only);
        assert_eq!(settings.dictation_hotkey, HotkeyBinding::default());
        assert_eq!(
            settings.paste_last_hotkey,
            Some(HotkeyBinding::paste_last_default())
        );
        assert!(settings.show_dock_icon);
        assert_eq!(settings.hud, crate::hud_settings::HudPreferences::default());
        assert_eq!(
            settings.post_processing,
            crate::post_processing::Preferences::default()
        );
    }

    #[test]
    fn settings_from_older_builds_still_load() {
        let settings: AppSettings = serde_json::from_str(
            r#"{"commands_enabled":true,"edit_hotkey":null,"transcription":{"model":"parakeet_v3"},"voice_action":{"enabled":true},"double_tap_lock":false}"#,
        )
        .unwrap();
        assert!(!settings.double_tap_lock);
        assert_eq!(settings.lower_volume_percent, 80);
    }

    #[test]
    fn lower_volume_choice_and_percentage_round_trip() {
        let settings: AppSettings = serde_json::from_str(
            r#"{"recording_audio_behavior":"lower_volume","lower_volume_percent":35}"#,
        )
        .unwrap();
        let saved = serde_json::to_vec(&settings).unwrap();
        let loaded: AppSettings = serde_json::from_slice(&saved).unwrap();
        assert_eq!(
            loaded.recording_audio_behavior,
            RecordingAudioBehavior::LowerVolume
        );
        assert_eq!(loaded.lower_volume_percent, 35);
        for behavior in [
            RecordingAudioBehavior::Mute,
            RecordingAudioBehavior::LowerVolume,
            RecordingAudioBehavior::PauseMedia,
            RecordingAudioBehavior::DoNothing,
        ] {
            assert_eq!(RecordingAudioBehavior::decode(behavior.encoded()), behavior);
        }
    }

    #[test]
    fn explicit_channel_preference_round_trips_without_changing_the_device_preference() {
        let settings = AppSettings {
            microphone: None,
            microphone_channel: Some(crate::microphone::ChannelSelection {
                device_id: "stable-device-uid".into(),
                device_name: "USB interface".into(),
                channel: 2,
            }),
            ..AppSettings::default()
        };
        let decoded: AppSettings =
            serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(decoded.microphone, None);
        assert_eq!(decoded.microphone_channel, settings.microphone_channel);
        let legacy: AppSettings =
            serde_json::from_str(r#"{"microphone":"USB interface"}"#).unwrap();
        assert_eq!(legacy.microphone.as_deref(), Some("USB interface"));
        assert_eq!(legacy.microphone_channel, None);
    }

    #[test]
    fn hud_preferences_persist_without_overwriting_other_settings() {
        use crate::hud_settings::{HudColor, HudPosition};
        let directory = std::env::temp_dir().join(format!(
            "hex-hud-settings-{}-{}",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        fs::write(&path, br#"{"sound_effects":false,"microphone":"Fixture microphone","hud":{"position":"bottom"}}"#).unwrap();
        let mut settings = AppSettings::load_from(&path).unwrap();
        assert_eq!(settings.hud.position, HudPosition::Bottom);
        assert_eq!(settings.hud.recording_color, HudColor::Red);
        assert_eq!(settings.hud.transcription_color, HudColor::Blue);
        settings.hud.recording_color = HudColor::Teal;
        settings.hud.transcription_color = HudColor::Purple;
        settings.write_to(&path).unwrap();
        let reloaded = AppSettings::load_from(&path).unwrap();
        assert_eq!(reloaded.hud, settings.hud);
        assert!(!reloaded.sound_effects);
        assert_eq!(reloaded.microphone.as_deref(), Some("Fixture microphone"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn older_defaults_migrate_to_tap_or_hold_but_explicit_hold_is_preserved() {
        let directory = std::env::temp_dir().join(format!(
            "hex-gesture-migration-{}-{}",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        for (json, expected) in [
            (r#"{}"#, DictationMode::TapOrHold),
            (r#"{"double_tap_lock":true}"#, DictationMode::TapOrHold),
            (r#"{"double_tap_lock":false}"#, DictationMode::Hold),
            (
                r#"{"dictation_mode":"double_tap","double_tap_lock":true}"#,
                DictationMode::DoubleTap,
            ),
        ] {
            fs::write(&path, json).unwrap();
            let settings = AppSettings::load_from(&path).unwrap();
            assert_eq!(settings.dictation_mode, expected);
            assert!(!settings.enter_to_submit);
            settings.write_to(&path).unwrap();
            assert_eq!(
                AppSettings::load_from(&path).unwrap().dictation_mode,
                expected
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_settings_writers_use_distinct_temporary_paths() {
        let path = PathBuf::from("/tmp/settings.json");

        assert_ne!(
            settings_temporary_path(&path),
            settings_temporary_path(&path)
        );
    }

    #[test]
    fn new_settings_require_onboarding_across_reloads() {
        use crate::onboarding::{completion_recorded_at, record_completion_at};
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "hex-new-onboarding-{}-{}",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        assert!(!completion_recorded_at(&directory));

        let mut settings = AppSettings::load_from(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(settings.sound_effects, AppSettings::default().sound_effects);
        settings.sound_effects = false;
        settings.double_tap_lock = false;
        settings.write_to(&path).unwrap();
        assert!(path.is_file());
        assert!(!directory.join("models").exists());
        assert!(!completion_recorded_at(&directory));
        assert!(!directory.join("onboarding-complete").exists());
        assert_eq!(
            fs::metadata(directory.join("onboarding-pending"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
        );
        drop(settings);

        let reloaded = AppSettings::load_from(&path).unwrap();
        assert!(!reloaded.sound_effects);
        assert!(!reloaded.double_tap_lock);
        reloaded.write_to(&path).unwrap();
        assert!(!completion_recorded_at(&directory));
        assert!(!directory.join("onboarding-complete").exists());

        record_completion_at(&directory).unwrap();
        assert!(completion_recorded_at(&directory));
        drop(reloaded);
        AppSettings::load_from(&path).unwrap();
        assert!(completion_recorded_at(&directory));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn loading_persists_normalizations_only_when_they_change_settings() {
        let directory = std::env::temp_dir().join(format!(
            "hex-normalized-settings-{}-{}",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        fs::write(
            &path,
            br#"{"double_tap_lock":false,"double_tap_only":true,"voice_action":{"enabled":true}}"#,
        )
        .unwrap();

        let settings = AppSettings::load_from(&path).unwrap();
        assert!(!settings.double_tap_only);
        let persisted = fs::read_to_string(&path).unwrap();
        assert!(!persisted.contains("voice_action"));
        assert!(persisted.contains("\"double_tap_only\": false"));

        let written = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let reloaded = AppSettings::load_from(&path).unwrap();
        assert_eq!(reloaded.double_tap_only, settings.double_tap_only);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), written);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rewriting_legacy_rust_settings_preserves_onboarding_migration() {
        let directory = std::env::temp_dir().join(format!(
            "hex-legacy-onboarding-{}-{}",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        fs::write(&path, b"{}").unwrap();

        let settings = AppSettings::load_from(&path).unwrap();
        settings.write_to(&path).unwrap();
        assert!(!directory.join("onboarding-pending").exists());
        assert!(crate::onboarding::completion_recorded_at(&directory));
        assert!(directory.join("onboarding-complete").is_file());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn first_settings_write_requires_the_pending_marker() {
        let directory = std::env::temp_dir().join(format!(
            "hex-pending-onboarding-{}-{}",
            std::process::id(),
            SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("onboarding-pending")).unwrap();
        let path = directory.join("settings.json");

        assert!(AppSettings::default().write_to(&path).is_err());
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_sleep_prevention_setting_is_ignored() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"prevent_system_sleep":false}"#).unwrap();
        let serialized = serde_json::to_value(settings).unwrap();

        assert!(serialized.get("prevent_system_sleep").is_none());
    }

    #[test]
    fn hotkey_runtime_preserves_modifier_sides_and_key_code() {
        let binding = HotkeyBinding {
            modifiers: HotkeyModifiers {
                control: Some(ModifierSide::Left),
                option: None,
                shift: Some(ModifierSide::Right),
                command: None,
                function: true,
            },
            key: Some(HotkeyKey {
                code: 49,
                label: "Space".into(),
            }),
        };
        let runtime = binding.runtime();

        assert_eq!(runtime.modifiers, binding.modifiers);
        assert_eq!(runtime.key_code, Some(49));
    }

    #[test]
    fn modifier_sides_round_trip_and_legacy_booleans_decode_as_either() {
        let settings = AppSettings {
            dictation_hotkey: HotkeyBinding {
                modifiers: HotkeyModifiers {
                    option: Some(ModifierSide::Right),
                    command: Some(ModifierSide::Left),
                    ..Default::default()
                },
                key: Some(HotkeyKey {
                    code: 0,
                    label: "A".into(),
                }),
            },
            paste_last_hotkey: None,
            double_tap_only: true,
            ..Default::default()
        };

        let encoded = serde_json::to_string(&settings).unwrap();
        let decoded: AppSettings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.dictation_hotkey, settings.dictation_hotkey);
        assert_eq!(decoded.paste_last_hotkey, None);
        assert!(decoded.double_tap_only);

        let legacy: HotkeyModifiers = serde_json::from_str(
            r#"{"control":false,"option":true,"shift":false,"command":false,"function":false}"#,
        )
        .unwrap();
        assert_eq!(legacy.option, Some(ModifierSide::Either));
    }

    #[test]
    fn side_constraints_can_be_removed_from_chord_modifiers() {
        let modifiers = HotkeyModifiers {
            option: Some(ModifierSide::Left),
            command: Some(ModifierSide::Right),
            function: true,
            ..Default::default()
        };

        assert_eq!(
            modifiers.without_side_constraints(),
            HotkeyModifiers {
                option: Some(ModifierSide::Either),
                command: Some(ModifierSide::Either),
                function: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn side_aware_matching_and_conflicts_share_the_same_algebra() {
        let left = HotkeyBinding {
            modifiers: HotkeyModifiers {
                option: Some(ModifierSide::Left),
                ..Default::default()
            },
            key: Some(HotkeyKey {
                code: 0,
                label: "A".into(),
            }),
        };
        let right = HotkeyBinding {
            modifiers: HotkeyModifiers {
                option: Some(ModifierSide::Right),
                ..Default::default()
            },
            key: left.key.clone(),
        };
        let either = HotkeyBinding {
            modifiers: HotkeyModifiers {
                option: Some(ModifierSide::Either),
                ..Default::default()
            },
            key: left.key.clone(),
        };

        assert!(!left.overlaps(&right));
        assert!(left.overlaps(&either));
        assert!(
            left.runtime()
                .matches_key_press(0, OPTION_KEY_MASK | LEFT_OPTION_MASK,)
        );
        assert!(
            !left
                .runtime()
                .matches_key_press(0, OPTION_KEY_MASK | RIGHT_OPTION_MASK,)
        );
        assert!(
            either
                .runtime()
                .matches_key_press(0, OPTION_KEY_MASK | RIGHT_OPTION_MASK,)
        );
    }

    #[test]
    fn runtime_side_constraints_require_the_requested_device_flag() {
        for (modifier, general, left, right) in [
            (
                "control",
                CONTROL_KEY_MASK,
                LEFT_CONTROL_MASK,
                RIGHT_CONTROL_MASK,
            ),
            (
                "option",
                OPTION_KEY_MASK,
                LEFT_OPTION_MASK,
                RIGHT_OPTION_MASK,
            ),
            ("shift", SHIFT_KEY_MASK, LEFT_SHIFT_MASK, RIGHT_SHIFT_MASK),
            (
                "command",
                COMMAND_KEY_MASK,
                LEFT_COMMAND_MASK,
                RIGHT_COMMAND_MASK,
            ),
        ] {
            for side in ["left", "right", "either"] {
                let binding: HotkeyBinding = serde_json::from_value(serde_json::json!({
                    "modifiers": { (modifier): side },
                    "key": { "code": 0, "label": "A" },
                }))
                .unwrap();
                let binding = binding.runtime();
                for (device_flags, matches) in [
                    (0, side == "either"),
                    (left, side != "right"),
                    (right, side != "left"),
                    (left | right, true),
                ] {
                    let flags = general | device_flags;
                    assert_eq!(
                        binding.required_modifiers_down(flags),
                        matches,
                        "{side:?}, flags={flags:#x}"
                    );
                    assert_eq!(
                        binding.matches_key_press(0, flags),
                        matches,
                        "{side:?}, flags={flags:#x}"
                    );
                    assert!(!binding.exact_modifiers(flags | FUNCTION_KEY_MASK));
                    assert_eq!(
                        binding.required_modifiers_down(flags | FUNCTION_KEY_MASK),
                        matches
                    );
                }
            }
        }
    }

    #[test]
    fn default_paste_hotkeys_follow_build_capabilities() {
        for hotkeys in [
            RuntimeHotkeys::default(),
            AppSettings::default().runtime_hotkeys(),
        ] {
            assert_eq!(
                hotkeys.paste_last,
                Some(HotkeyBinding::paste_last_default().runtime())
            );
        }
    }
}
