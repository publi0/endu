//! Persisted sound levels and double-tap timing, independent of capture timing.

use crate::i18n::t;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct SoundVolumes {
    pub start: f32,
    pub stop: f32,
    pub error_cancel: f32,
}

impl SoundVolumes {
    /// Preserve the old shared slider's audible levels, including the start cue gain.
    /// The existing sound_effects switch remains the master on/off preference.
    pub fn from_legacy(volume: f32) -> Self {
        let volume = normalized_volume(volume);
        Self {
            start: (volume * 1.5).min(1.0),
            stop: volume,
            error_cancel: volume,
        }
    }

    pub fn normalized(self) -> Self {
        Self {
            start: normalized_volume(self.start),
            stop: normalized_volume(self.stop),
            error_cancel: normalized_volume(self.error_cancel),
        }
    }
}

impl Default for SoundVolumes {
    fn default() -> Self {
        Self::from_legacy(0.5)
    }
}

fn normalized_volume(volume: f32) -> f32 {
    if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoubleTapSensitivity {
    Short,
    #[default]
    Normal,
    Tolerant,
}

impl DoubleTapSensitivity {
    pub const ALL: [Self; 3] = [Self::Short, Self::Normal, Self::Tolerant];

    pub fn label(self) -> &'static str {
        match self {
            Self::Short => t("Short"),
            Self::Normal => t("Normal"),
            Self::Tolerant => t("Tolerant"),
        }
    }

    pub const fn window(self) -> Duration {
        Duration::from_millis(match self {
            Self::Short => 200,
            Self::Normal => 300,
            Self::Tolerant => 450,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_volume_migration_preserves_mute_and_start_emphasis() {
        assert_eq!(
            SoundVolumes::from_legacy(0.0),
            SoundVolumes {
                start: 0.0,
                stop: 0.0,
                error_cancel: 0.0,
            }
        );
        assert_eq!(
            SoundVolumes::default(),
            SoundVolumes {
                start: 0.75,
                stop: 0.5,
                error_cancel: 0.5,
            }
        );
        let louder = SoundVolumes::from_legacy(0.8);
        assert_eq!(louder.start, 1.0);
        assert_eq!(louder.stop, 0.8);
        assert_eq!(louder.error_cancel, 0.8);
    }

    #[test]
    fn independent_levels_round_trip_and_invalid_gains_are_bounded() {
        let levels = SoundVolumes {
            start: 0.0,
            stop: 0.3,
            error_cancel: 0.8,
        };
        let json = serde_json::to_string(&levels).unwrap();
        assert_eq!(serde_json::from_str::<SoundVolumes>(&json).unwrap(), levels);
        assert_eq!(
            SoundVolumes {
                start: f32::NAN,
                stop: -1.0,
                error_cancel: 2.0,
            }
            .normalized(),
            SoundVolumes {
                start: 0.0,
                stop: 0.0,
                error_cancel: 1.0,
            }
        );
        assert_eq!(normalized_volume(f32::INFINITY), 0.0);
        assert_eq!(normalized_volume(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn sensitivity_presets_keep_the_existing_default() {
        assert_eq!(
            DoubleTapSensitivity::default().window(),
            Duration::from_millis(300)
        );
        for preset in DoubleTapSensitivity::ALL {
            let json = serde_json::to_string(&preset).unwrap();
            assert_eq!(
                serde_json::from_str::<DoubleTapSensitivity>(&json).unwrap(),
                preset
            );
        }
    }
}
