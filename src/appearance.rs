//! Light/dark appearance preferences. The window follows the application's
//! effective appearance, so overriding it also themes native menus, alerts and
//! the title bar consistently with the GPUI palette.

use crate::i18n::t;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => t("System"),
            Self::Light => t("Light"),
            Self::Dark => t("Dark"),
        }
    }

    /// Whether this choice paints dark, given the system's current appearance.
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::System => system_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }

    /// Applies a choice made inside the window. Changing the application's
    /// appearance makes AppKit call back into GPUI synchronously, and GPUI
    /// drops that update while the window is still handling the event, so
    /// apply it once the event finishes and repaint from the known result.
    pub fn apply(self, cx: &mut gpui::App) {
        cx.defer(move |cx| {
            self.apply_to_application();
            crate::desktop_ui::set_dark_appearance(self.is_dark(system_is_dark()));
            cx.refresh_windows();
        });
    }

    /// Overrides the application's appearance; System clears the override.
    /// Must run on the main thread; elsewhere it does nothing.
    pub fn apply_to_application(self) {
        use objc2_app_kit::{
            NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
        };
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return;
        };
        let name = match self {
            Self::System => None,
            // SAFETY: AppKit's appearance names are immutable framework constants.
            Self::Light => Some(unsafe { NSAppearanceNameAqua }),
            Self::Dark => Some(unsafe { NSAppearanceNameDarkAqua }),
        };
        let appearance = name.and_then(NSAppearance::appearanceNamed);
        NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
    }
}

/// The system-wide appearance, independent of Hex's own override. The global
/// `AppleInterfaceStyle` default is "Dark" in dark mode and absent otherwise,
/// including while macOS switches automatically.
pub fn system_is_dark() -> bool {
    use objc2_foundation::{NSString, NSUserDefaults};
    NSUserDefaults::standardUserDefaults()
        .stringForKey(&NSString::from_str("AppleInterfaceStyle"))
        .is_some_and(|style| style.to_string().eq_ignore_ascii_case("dark"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_is_the_default_and_manual_choices_override_it() {
        assert_eq!(Appearance::default(), Appearance::System);
        for system_dark in [false, true] {
            assert_eq!(Appearance::System.is_dark(system_dark), system_dark);
            assert!(!Appearance::Light.is_dark(system_dark));
            assert!(Appearance::Dark.is_dark(system_dark));
        }
        for appearance in Appearance::ALL {
            let json = serde_json::to_string(&appearance).unwrap();
            assert_eq!(
                serde_json::from_str::<Appearance>(&json).unwrap(),
                appearance
            );
        }
        assert_eq!(
            serde_json::from_str::<Appearance>("\"dark\"").unwrap(),
            Appearance::Dark
        );
    }
}
