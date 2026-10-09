//! Interface language. English text is the key: the catalogs map it to
//! Portuguese and Spanish, and anything missing falls back to English. The
//! language is resolved once from the preference (System follows macOS) and
//! read lock-free while rendering.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

mod es;
mod pt;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LanguagePreference {
    #[default]
    System,
    Portuguese,
    English,
    Spanish,
}

impl LanguagePreference {
    pub const ALL: [Self; 4] = [Self::System, Self::Portuguese, Self::English, Self::Spanish];

    /// Each language is named in itself, so it stays findable from any other.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => t("System"),
            Self::Portuguese => "Português",
            Self::English => "English",
            Self::Spanish => "Español",
        }
    }

    pub fn resolve(self) -> Language {
        match self {
            Self::System => system_language(),
            Self::Portuguese => Language::Portuguese,
            Self::English => Language::English,
            Self::Spanish => Language::Spanish,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum Language {
    #[default]
    English,
    Portuguese,
    Spanish,
}

impl Language {
    /// Picks the first supported language among macOS's preferred ones, so a
    /// Mac set to e.g. "pt-BR, en" opens in Portuguese; anything else is English.
    pub fn from_preferred(languages: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        for language in languages {
            let code = language.as_ref().to_ascii_lowercase();
            if code.starts_with("pt") {
                return Self::Portuguese;
            }
            if code.starts_with("es") {
                return Self::Spanish;
            }
            if code.starts_with("en") {
                return Self::English;
            }
        }
        Self::English
    }
}

fn system_language() -> Language {
    let preferred = objc2_foundation::NSLocale::preferredLanguages();
    Language::from_preferred(preferred.iter().map(|language| language.to_string()))
}

static CURRENT: AtomicU8 = AtomicU8::new(Language::English as u8);

pub fn set_language(language: Language) {
    CURRENT.store(language as u8, Ordering::Relaxed);
}

pub fn language() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Language::Portuguese,
        2 => Language::Spanish,
        _ => Language::English,
    }
}

static PORTUGUESE: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| pt::CATALOG.iter().copied().collect());
static SPANISH: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| es::CATALOG.iter().copied().collect());

pub fn translate(language: Language, english: &'static str) -> &'static str {
    let catalog = match language {
        Language::English => return english,
        Language::Portuguese => &PORTUGUESE,
        Language::Spanish => &SPANISH,
    };
    catalog.get(english).copied().unwrap_or(english)
}

/// The interface text for `english` in the current language.
pub fn t(english: &'static str) -> &'static str {
    translate(language(), english)
}

/// A formatted number with the decimal comma used in Portuguese and Spanish.
pub fn decimal(text: String) -> String {
    match language() {
        Language::English => text,
        Language::Portuguese | Language::Spanish => text.replace('.', ","),
    }
}

/// Replaces each `{name}` in a translated template with its value.
pub fn fill(template: &str, values: &[(&str, String)]) -> String {
    let mut text = template.to_owned();
    for (name, value) in values {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// Translates a template, then fills its named `{placeholders}`:
/// `tf!("Endu {version} is ready", version = version)`.
#[macro_export]
macro_rules! tf {
    ($template:literal $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::t($template),
            &[$((stringify!($name), ($value).to_string())),*],
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn placeholders(text: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let Some(end) = rest[start..].find('}') else {
                break;
            };
            names.insert(rest[start + 1..start + end].to_owned());
            rest = &rest[start + end + 1..];
        }
        names
    }

    /// Every `t("…")` and `tf!("…")` key in the sources, with escapes resolved.
    fn source_keys() -> BTreeSet<String> {
        fn visit(dir: &Path, keys: &mut BTreeSet<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, keys);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && !path.ends_with("i18n/pt.rs")
                    && !path.ends_with("i18n/es.rs")
                    && !path.ends_with("src/i18n.rs")
                {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for marker in ["t(", "tf!("] {
                        let mut rest = source.as_str();
                        while let Some(start) = rest.find(marker) {
                            let preceded_by_word = start > 0 && marker == "t(" && {
                                let before = rest.as_bytes()[start - 1];
                                before.is_ascii_alphanumeric() || before == b'_'
                            };
                            rest = &rest[start + marker.len()..];
                            let literal = rest.trim_start();
                            if preceded_by_word || !literal.starts_with('"') {
                                continue;
                            }
                            let mut key = String::new();
                            let mut chars = literal[1..].chars();
                            while let Some(c) = chars.next() {
                                match c {
                                    '"' => break,
                                    '\\' => match chars.next() {
                                        Some('n') => key.push('\n'),
                                        Some(other) => key.push(other),
                                        None => break,
                                    },
                                    _ => key.push(c),
                                }
                            }
                            keys.insert(key);
                        }
                    }
                }
            }
        }
        let mut keys = BTreeSet::new();
        visit(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut keys,
        );
        keys.remove("…");
        keys
    }

    #[test]
    fn every_interface_text_is_translated_with_its_placeholders() {
        let keys = source_keys();
        assert!(keys.len() > 300, "found only {} keys", keys.len());
        for (name, catalog) in [("Portuguese", &*PORTUGUESE), ("Spanish", &*SPANISH)] {
            let missing: Vec<_> = keys
                .iter()
                .filter(|key| !catalog.contains_key(key.as_str()))
                .collect();
            assert!(missing.is_empty(), "{name} lacks {missing:#?}");
            for (english, translated) in catalog.iter() {
                assert_eq!(
                    placeholders(english),
                    placeholders(translated),
                    "{name} changes the placeholders of {english:?}"
                );
            }
        }
    }

    #[test]
    fn catalogs_have_no_duplicate_keys() {
        for catalog in [pt::CATALOG, es::CATALOG] {
            let mut seen = BTreeSet::new();
            for (english, _) in catalog {
                assert!(seen.insert(*english), "duplicate key {english:?}");
            }
        }
    }

    #[test]
    fn system_language_picks_the_first_supported_preference() {
        assert_eq!(
            Language::from_preferred(["pt-BR", "en-US"]),
            Language::Portuguese
        );
        assert_eq!(Language::from_preferred(["es-419"]), Language::Spanish);
        assert_eq!(
            Language::from_preferred(["fr-FR", "es-ES"]),
            Language::Spanish
        );
        assert_eq!(Language::from_preferred(["de-DE"]), Language::English);
        assert_eq!(
            Language::from_preferred(Vec::<String>::new()),
            Language::English
        );
    }

    #[test]
    fn translation_falls_back_to_english_and_fills_placeholders() {
        assert_eq!(translate(Language::English, "Settings"), "Settings");
        assert_eq!(translate(Language::Portuguese, "Settings"), "Ajustes");
        assert_eq!(translate(Language::Spanish, "Settings"), "Ajustes");
        assert_eq!(
            translate(Language::Portuguese, "Not a catalog entry"),
            "Not a catalog entry"
        );
        assert_eq!(
            fill("Endu {version} is ready", &[("version", "4.0.0".into())]),
            "Endu 4.0.0 is ready"
        );
    }
}
