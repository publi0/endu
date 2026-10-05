//! Explicit channel preferences and in-memory diagnostics. No gain processing.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// A configured input preference or an entry in the available-device catalog.
/// A UID identifies the device even after a rename. Name matching is only used
/// for devices whose backend cannot provide a stable identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DevicePreference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
}

impl DevicePreference {
    pub fn matches(&self, available: &Self) -> bool {
        match &self.id {
            Some(id) => available.id.as_ref() == Some(id),
            None => self.name == available.name,
        }
    }
}

/// Preference order followed by the system fallback, without selecting a
/// similarly named device when a saved UID is absent. Missing entries remain
/// in the saved list so a reconnected device can regain its priority.
pub(crate) fn preferred_input_indices(
    available: &[DevicePreference],
    preferences: &[DevicePreference],
    fallback: Option<usize>,
) -> Vec<usize> {
    let mut indices = Vec::new();
    for preference in preferences {
        if let Some(index) = available
            .iter()
            .position(|device| preference.matches(device))
            && !indices.contains(&index)
        {
            indices.push(index);
        }
    }
    if let Some(index) = fallback.filter(|index| *index < available.len())
        && !indices.contains(&index)
    {
        indices.push(index);
    }
    indices
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChannelSelection {
    pub device_id: String,
    pub device_name: String,
    /// One-based channel number as displayed to the user.
    pub channel: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputDescription {
    pub device_id: Option<String>,
    pub name: String,
    pub channels: u16,
    pub channel: Option<u16>,
    pub requested_channel: Option<u16>,
    pub fallback_from: Option<String>,
}

impl InputDescription {
    pub fn channel_label(&self) -> String {
        match self.channel {
            Some(channel) => format!("Channel {channel} of {}", self.channels),
            None if self.channels == 1 => "Mono input".into(),
            None => format!("Mix of {} channels", self.channels),
        }
    }

    pub fn channel_unavailable(&self) -> bool {
        self.requested_channel.is_some() && self.channel != self.requested_channel
    }

    pub fn for_preview(selection: Option<&ChannelSelection>) -> Self {
        let requested_channel = selection
            .filter(|selection| selection.device_id == "preview-input")
            .map(|selection| selection.channel);
        Self {
            device_id: Some("preview-input".into()),
            name: "Built-in Microphone".into(),
            channels: 2,
            channel: resolve_channel(requested_channel, 2),
            requested_channel,
            fallback_from: None,
        }
    }
}

pub fn resolve_channel(requested: Option<u16>, channels: u16) -> Option<u16> {
    requested.filter(|channel| *channel > 0 && *channel <= channels)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Levels {
    pub samples: u64,
    pub invalid_samples: u64,
    pub peak: f64,
    pub rms: f64,
    pub full_scale_samples: u64,
}

impl Levels {
    pub fn measure(samples: &[f32]) -> Self {
        let mut valid = 0_u64;
        let mut invalid = 0;
        let mut squares = 0.0_f64;
        let mut peak = 0.0_f64;
        let mut full_scale_samples = 0;
        for &sample in samples {
            if !sample.is_finite() {
                invalid += 1;
                continue;
            }
            let value = f64::from(sample).abs();
            valid += 1;
            squares += value * value;
            peak = peak.max(value);
            full_scale_samples += u64::from(value >= 0.999);
        }
        Self {
            samples: valid,
            invalid_samples: invalid,
            peak,
            rms: if valid == 0 {
                0.0
            } else {
                (squares / valid as f64).sqrt()
            },
            full_scale_samples,
        }
    }

    pub fn rms_dbfs(&self) -> Option<f64> {
        dbfs(self.rms)
    }
    pub fn peak_dbfs(&self) -> Option<f64> {
        dbfs(self.peak)
    }

    pub fn warning(&self) -> Option<&'static str> {
        if self.invalid_samples > 0 {
            Some("Invalid audio samples detected. Check the input device.")
        } else if self.full_scale_samples > 0 {
            Some("Possible clipping. Check the microphone's input gain.")
        } else if self.peak == 0.0 {
            Some("No signal in this recording. Check the microphone and input channel.")
        } else if self.rms_dbfs().is_some_and(|value| value < -45.0)
            && self.peak_dbfs().is_some_and(|value| value < -35.0)
        {
            Some("Very quiet recording. Check the input gain or select the microphone's channel.")
        } else {
            None
        }
    }
}

fn dbfs(level: f64) -> Option<f64> {
    (level > 0.0).then(|| 20.0 * level.log10())
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordingDiagnostic {
    pub input: Option<InputDescription>,
    pub levels: Levels,
    pub duration_ms: u64,
}

impl RecordingDiagnostic {
    pub fn for_preview() -> Self {
        Self {
            input: Some(InputDescription::for_preview(None)),
            levels: Levels::measure(&[0.25, -0.25]),
            duration_ms: 1_200,
        }
    }
}

static LAST: OnceLock<Mutex<Option<RecordingDiagnostic>>> = OnceLock::new();

/// Called by the transcription worker, before silence trimming. Does not retain
/// samples, write files, open a device, or block the capture callback.
pub fn record(samples: &[f32], input: Option<InputDescription>) {
    let diagnostic = RecordingDiagnostic {
        input,
        levels: Levels::measure(samples),
        duration_ms: samples.len() as u64 * 1_000 / 16_000,
    };
    *LAST
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(diagnostic);
}

pub fn latest() -> Option<RecordingDiagnostic> {
    LAST.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: Option<&str>, name: &str) -> DevicePreference {
        DevicePreference {
            id: id.map(str::to_owned),
            name: name.into(),
        }
    }

    #[test]
    fn input_preferences_follow_uids_across_renames_without_matching_impostors() {
        let available = [
            device(Some("usb-b"), "USB mic"),
            device(Some("usb-a"), "Renamed mic"),
            device(None, "Legacy mic"),
        ];
        let preferences = [device(Some("usb-a"), "USB mic"), device(None, "Legacy mic")];
        assert_eq!(
            preferred_input_indices(&available, &preferences, Some(0)),
            [1, 2, 0]
        );
        let disconnected = [device(Some("usb-b"), "USB mic"), device(None, "Legacy mic")];
        assert_eq!(
            preferred_input_indices(&disconnected, &preferences, Some(0)),
            [1, 0]
        );
        assert_eq!(
            preferred_input_indices(&available, &preferences, Some(0)),
            [1, 2, 0]
        );
    }

    #[test]
    fn missing_and_duplicate_preferences_fall_through_once_to_the_default() {
        let available = [device(Some("a"), "A"), device(Some("b"), "B")];
        let preferences = [
            device(Some("missing"), "A"),
            available[1].clone(),
            available[1].clone(),
        ];
        assert_eq!(
            preferred_input_indices(&available, &preferences, Some(1)),
            [1]
        );
        assert_eq!(preferred_input_indices(&available, &[], Some(0)), [0]);
        assert!(preferred_input_indices(&available, &[], None).is_empty());
        assert_eq!(
            serde_json::from_str::<DevicePreference>(r#"{"name":"Legacy mic"}"#).unwrap(),
            device(None, "Legacy mic")
        );
    }

    #[test]
    fn levels_measure_known_signal_without_changing_samples() {
        let samples: Vec<f32> = (0..1_600)
            .map(|index| 0.5 * (std::f32::consts::TAU * index as f32 / 16.0).sin())
            .collect();
        let original = samples.clone();
        let levels = Levels::measure(&samples);
        assert!((levels.rms - 0.5 / 2.0_f64.sqrt()).abs() < 0.00001);
        assert!((levels.peak_dbfs().unwrap() + 6.0206).abs() < 0.001);
        assert_eq!(levels.warning(), None);
        assert_eq!(samples, original);
    }

    #[test]
    fn silence_quiet_clipping_and_invalid_samples_are_distinguished() {
        let silent = Levels::measure(&[0.0; 16]);
        assert_eq!(silent.rms_dbfs(), None);
        assert_eq!(silent.peak_dbfs(), None);
        assert!(silent.warning().unwrap().contains("No signal"));
        assert!(
            Levels::measure(&[0.001, -0.001])
                .warning()
                .unwrap()
                .contains("quiet")
        );
        let clipped = Levels::measure(&[1.0, -1.0, 0.0]);
        assert_eq!(clipped.full_scale_samples, 2);
        assert!(clipped.warning().unwrap().contains("Possible clipping"));
        let invalid = Levels::measure(&[f32::NAN, f32::INFINITY, 0.5]);
        assert_eq!(invalid.invalid_samples, 2);
        assert_eq!(invalid.samples, 1);
        assert_eq!(invalid.rms, 0.5);
        assert!(invalid.warning().unwrap().contains("Invalid"));
    }

    #[test]
    fn unavailable_channel_falls_back_to_the_existing_mix() {
        assert_eq!(resolve_channel(None, 2), None);
        assert_eq!(resolve_channel(Some(2), 2), Some(2));
        assert_eq!(resolve_channel(Some(3), 2), None);
        assert_eq!(resolve_channel(Some(0), 2), None);
    }
}
