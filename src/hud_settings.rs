//! Persisted HUD choices and a coherent snapshot for the renderer.

use std::fmt;
use std::str::FromStr;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum HudPosition {
    #[default]
    Top,
    Bottom,
}

impl HudPosition {
    pub const ALL: [Self; 2] = [Self::Top, Self::Bottom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Top => "Top",
            Self::Bottom => "Bottom",
        }
    }

    /// Cocoa's window top, keeping the visible content an equal distance from
    /// either edge of visibleFrame. The transparent HUD margins stay outside
    /// that spacing; Dock/menu-bar sizes and negative screen origins are valid.
    pub fn window_top(
        self,
        visible_bottom: f64,
        visible_height: f64,
        window_height: f64,
        content_height: f64,
        gap: f64,
    ) -> f64 {
        let margin = (window_height - content_height) / 2.0;
        let gap = gap.clamp(0.0, (visible_height - content_height).max(0.0));
        match self {
            Self::Top => visible_bottom + visible_height + margin - gap,
            Self::Bottom => visible_bottom + window_height - margin + gap,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum HudColor {
    Red,
    Orange,
    Green,
    Teal,
    Blue,
    Purple,
}

impl HudColor {
    pub const ALL: [Self; 6] = [
        Self::Red,
        Self::Orange,
        Self::Green,
        Self::Teal,
        Self::Blue,
        Self::Purple,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Red => "Red",
            Self::Orange => "Orange",
            Self::Green => "Green",
            Self::Teal => "Teal",
            Self::Blue => "Blue",
            Self::Purple => "Purple",
        }
    }

    pub fn swatch(self) -> u32 {
        match self {
            Self::Red => 0xf23b43,
            Self::Orange => 0xf59638,
            Self::Green => 0x3cbe78,
            Self::Teal => 0x32bfb0,
            Self::Blue => 0x477cf5,
            Self::Purple => 0xa567ed,
        }
    }

    fn hue(self) -> f32 {
        match self {
            Self::Red => 0.0,
            Self::Orange => 0.085,
            Self::Green => 0.37,
            Self::Teal => 0.48,
            Self::Blue => 0.622_222_2,
            Self::Purple => 0.77,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum HudSize {
    Small,
    #[default]
    Normal,
    Large,
}

impl HudSize {
    pub const ALL: [Self; 3] = [Self::Small, Self::Normal, Self::Large];

    pub fn label(self) -> &'static str {
        match self {
            Self::Small => "Small",
            Self::Normal => "Normal",
            Self::Large => "Large",
        }
    }

    pub fn scale(self) -> f32 {
        match self {
            Self::Small => 0.8,
            Self::Normal => 1.0,
            Self::Large => 1.25,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum HudBrightness {
    Subtle,
    #[default]
    Normal,
    Intense,
}

impl HudBrightness {
    pub const ALL: [Self; 3] = [Self::Subtle, Self::Normal, Self::Intense];

    pub fn label(self) -> &'static str {
        match self {
            Self::Subtle => "Subtle",
            Self::Normal => "Normal",
            Self::Intense => "Intense",
        }
    }

    pub fn factor(self) -> f32 {
        match self {
            Self::Subtle => 0.6,
            Self::Normal => 1.0,
            Self::Intense => 1.4,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum HudScreen {
    #[default]
    Pointer,
    ActiveWindow,
    FixedMonitor,
}

impl HudScreen {
    pub const ALL: [Self; 3] = [Self::Pointer, Self::ActiveWindow, Self::FixedMonitor];

    pub fn label(self) -> &'static str {
        match self {
            Self::Pointer => "Pointer",
            Self::ActiveWindow => "Active window",
            Self::FixedMonitor => "Fixed monitor",
        }
    }
}

/// A display UUID, independent of its transient display number or array index.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct MonitorId([u8; 16]);

impl MonitorId {
    pub(crate) fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for MonitorId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if [4, 6, 8, 10].contains(&index) {
                formatter.write_str("-")?;
            }
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for MonitorId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 36 {
            return Err("monitor must be a display UUID");
        }
        let mut bytes = [0; 16];
        let mut digits = 0;
        for (index, character) in value.chars().enumerate() {
            if [8, 13, 18, 23].contains(&index) {
                if character != '-' {
                    return Err("monitor must be a display UUID");
                }
                continue;
            }
            let digit = character
                .to_digit(16)
                .ok_or("monitor must be a display UUID")?;
            if digits >= 32 {
                return Err("monitor must be a display UUID");
            }
            bytes[digits / 2] = (bytes[digits / 2] << 4) | digit as u8;
            digits += 1;
        }
        if digits != 32 {
            return Err("monitor must be a display UUID");
        }
        Ok(Self(bytes))
    }
}

impl TryFrom<String> for MonitorId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<MonitorId> for String {
    fn from(value: MonitorId) -> Self {
        value.to_string()
    }
}

pub const MIN_EDGE_DISTANCE: u16 = 0;
pub const MAX_EDGE_DISTANCE: u16 = 160;

fn deserialize_edge_distance<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let distance = u64::deserialize(deserializer)?;
    Ok(distance.min(u64::from(MAX_EDGE_DISTANCE)) as u16)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct HudPreferences {
    pub position: HudPosition,
    pub recording_color: HudColor,
    pub transcription_color: HudColor,
    pub size: HudSize,
    pub brightness: HudBrightness,
    pub voice_reactive: bool,
    pub screen: HudScreen,
    pub fixed_monitor: Option<MonitorId>,
    #[serde(deserialize_with = "deserialize_edge_distance")]
    pub edge_distance: u16,
}

impl HudPreferences {
    const DEFAULT: Self = Self {
        position: HudPosition::Top,
        recording_color: HudColor::Red,
        transcription_color: HudColor::Blue,
        size: HudSize::Normal,
        brightness: HudBrightness::Normal,
        voice_reactive: true,
        screen: HudScreen::Pointer,
        fixed_monitor: None,
        edge_distance: 12,
    };

    pub fn normalized(mut self) -> Self {
        self.edge_distance = self
            .edge_distance
            .clamp(MIN_EDGE_DISTANCE, MAX_EDGE_DISTANCE);
        self
    }

    /// Zero rotations preserve every existing shader color exactly.
    pub fn hue_shifts(self) -> [f32; 2] {
        [
            self.recording_color.hue() - HudColor::Red.hue(),
            self.transcription_color.hue() - HudColor::Blue.hue(),
        ]
    }

    pub fn apply_runtime(self) {
        *HUD_PREFERENCES
            .write()
            .unwrap_or_else(|error| error.into_inner()) = self.normalized();
    }
}

impl Default for HudPreferences {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// One short lock covers the display UUID and every visual choice together.
// No device queries, allocation, rendering, or file I/O happen under this lock.
static HUD_PREFERENCES: RwLock<HudPreferences> = RwLock::new(HudPreferences::DEFAULT);

pub fn current() -> HudPreferences {
    *HUD_PREFERENCES
        .read()
        .unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_position_and_color_pair_round_trips_as_one_snapshot() {
        assert_eq!(HudPreferences::default().hue_shifts(), [0.0, 0.0]);
        for position in HudPosition::ALL {
            for recording_color in HudColor::ALL {
                for transcription_color in HudColor::ALL {
                    let settings = HudPreferences {
                        position,
                        recording_color,
                        transcription_color,
                        ..HudPreferences::default()
                    };
                    let json = serde_json::to_string(&settings).unwrap();
                    assert_eq!(
                        serde_json::from_str::<HudPreferences>(&json).unwrap(),
                        settings
                    );
                }
            }
        }
    }

    #[test]
    fn visible_capsule_keeps_its_gap_on_both_edges_and_shifted_screens() {
        for (bottom, height) in [(0.0, 900.0), (-1080.0, 1030.0), (80.0, 820.0)] {
            let window_height = 64.0;
            let content_height = 16.0;
            let margin = (window_height - content_height) / 2.0;
            let upper =
                HudPosition::Top.window_top(bottom, height, window_height, content_height, 12.0);
            assert_eq!(upper - margin, bottom + height - 12.0);
            let lower =
                HudPosition::Bottom.window_top(bottom, height, window_height, content_height, 12.0);
            assert_eq!(lower - window_height + margin, bottom + 12.0);
            let notice_top = HudPosition::Bottom.window_top(bottom, height, 76.0, 76.0, 44.0);
            assert!(
                notice_top - 76.0 > lower - margin,
                "notice must sit above the bottom HUD"
            );
        }
    }

    #[test]
    fn older_preferences_keep_the_original_size_brightness_and_placement() {
        let preferences: HudPreferences = serde_json::from_str(
            r#"{"position":"bottom","recording_color":"green","transcription_color":"purple"}"#,
        )
        .unwrap();
        assert_eq!(preferences.size, HudSize::Normal);
        assert!(preferences.voice_reactive);
        assert_eq!(preferences.size.scale(), 1.0);
        assert_eq!(preferences.brightness.factor(), 1.0);
        assert_eq!(preferences.edge_distance, 12);
        assert_eq!(preferences.screen, HudScreen::Pointer);
        assert_eq!(preferences.fixed_monitor, None);
        assert_eq!(preferences.position, HudPosition::Bottom);
        assert_eq!(preferences.recording_color, HudColor::Green);
        assert_eq!(preferences.transcription_color, HudColor::Purple);
    }

    #[test]
    fn voice_reaction_defaults_on_and_preserves_an_explicit_opt_out() {
        assert!(HudPreferences::default().voice_reactive);
        for voice_reactive in [false, true] {
            let preferences = HudPreferences {
                voice_reactive,
                ..Default::default()
            };
            let saved = serde_json::to_string(&preferences).unwrap();
            assert_eq!(
                serde_json::from_str::<HudPreferences>(&saved).unwrap(),
                preferences
            );
        }
    }

    #[test]
    fn display_uuid_round_trips_and_rejects_invalid_identifiers() {
        let value = "12345678-ABCD-4321-90AB-1234567890AB";
        let id: MonitorId = value.parse().unwrap();
        assert_eq!(id.to_string(), value.to_lowercase());
        let preferences = HudPreferences {
            size: HudSize::Large,
            brightness: HudBrightness::Subtle,
            screen: HudScreen::FixedMonitor,
            fixed_monitor: Some(id),
            edge_distance: 160,
            ..Default::default()
        };
        assert_eq!(
            serde_json::from_str::<HudPreferences>(&serde_json::to_string(&preferences).unwrap())
                .unwrap(),
            preferences
        );
        for invalid in [
            "",
            "42",
            "12345678_abcd-4321-90ab-1234567890ab",
            "z2345678-abcd-4321-90ab-1234567890ab",
        ] {
            assert!(invalid.parse::<MonitorId>().is_err());
        }
    }

    #[test]
    fn distance_is_bounded_in_configuration_and_on_small_displays() {
        let preferences: HudPreferences =
            serde_json::from_str(r#"{"edge_distance":999999}"#).unwrap();
        assert_eq!(preferences.edge_distance, MAX_EDGE_DISTANCE);
        let preferences = HudPreferences {
            edge_distance: u16::MAX,
            ..Default::default()
        }
        .normalized();
        assert_eq!(preferences.edge_distance, MAX_EDGE_DISTANCE);
        assert!(serde_json::from_str::<HudPreferences>(r#"{"edge_distance":-1}"#).is_err());
        for position in HudPosition::ALL {
            let top = position.window_top(-200.0, 100.0, 64.0, 16.0, 160.0);
            let content_bottom = top - 64.0 + 24.0;
            let content_top = top - 24.0;
            assert!(content_bottom >= -200.0 && content_top <= -100.0);
        }
    }

    #[test]
    fn every_size_keeps_the_edge_gap_and_notice_separation() {
        for size in HudSize::ALL {
            let scale = f64::from(size.scale());
            let content = 16.0 * scale;
            let window = 64.0 * scale;
            let margin = (window - content) / 2.0;
            for gap in [0.0, 12.0, 160.0] {
                let top = HudPosition::Top.window_top(-100.0, 900.0, window, content, gap);
                assert!((top - margin - (800.0 - gap)).abs() < 0.00001);
                let bottom = HudPosition::Bottom.window_top(-100.0, 900.0, window, content, gap);
                assert!((bottom - window + margin - (-100.0 + gap)).abs() < 0.00001);
                let notice =
                    HudPosition::Bottom.window_top(-100.0, 900.0, 76.0, 76.0, gap + content + 16.0);
                assert!((notice - 76.0 - (bottom - margin) - 16.0).abs() < 0.00001);
            }
        }
    }
}
