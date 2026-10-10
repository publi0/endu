//! Pure edits of [`Config`] for the Settings view, with validation. Kept free
//! of GPUI so it is testable on any platform.

use super::Config;
use crate::i18n::t;

/// The primary model plus at most this many fallbacks are editable in Settings.
pub const MAX_FALLBACKS: usize = 2;

/// The text fields under "Advanced".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdvancedForm {
    pub base_url: String,
    pub attempt_timeout_seconds: String,
    pub total_timeout_seconds: String,
    pub chunk_seconds: String,
    pub rate_limit_retry_max_wait_ms: String,
    /// Empty means "provider default".
    pub temperature: String,
}

impl AdvancedForm {
    pub fn from_config(config: &Config) -> Self {
        Self {
            base_url: config.base_url.clone(),
            attempt_timeout_seconds: config.transcription.attempt_timeout_seconds.to_string(),
            total_timeout_seconds: config.transcription.total_timeout_seconds.to_string(),
            chunk_seconds: config.transcription.chunk_seconds.to_string(),
            rate_limit_retry_max_wait_ms: config
                .transcription
                .rate_limit_retry_max_wait_ms
                .to_string(),
            temperature: config
                .transcription
                .temperature
                .map(|value| value.to_string())
                .unwrap_or_default(),
        }
    }

    /// Preserve external edits to fields that the user has not changed.
    pub fn apply_changes(&self, original: &Self, base: &Config) -> Result<Config, String> {
        let mut latest = Self::from_config(base);
        macro_rules! merge {
            ($($field:ident),+ $(,)?) => {
                $(if self.$field != original.$field {
                    latest.$field.clone_from(&self.$field);
                })+
            };
        }
        merge!(
            base_url,
            attempt_timeout_seconds,
            total_timeout_seconds,
            chunk_seconds,
            rate_limit_retry_max_wait_ms,
            temperature,
        );
        latest.apply(base)
    }

    /// Apply the form onto `base`, keeping every field it does not show.
    /// Errors name the offending field.
    pub fn apply(&self, base: &Config) -> Result<Config, String> {
        let mut config = base.clone();
        let base_url = self.base_url.trim().trim_end_matches('/');
        super::http::validate_url(base_url).map_err(|error| format!("API URL: {error}"))?;
        config.base_url = base_url.to_owned();
        config.transcription.attempt_timeout_seconds =
            number(&self.attempt_timeout_seconds, t("Attempt timeout"), 1, 600)?;
        config.transcription.total_timeout_seconds =
            number(&self.total_timeout_seconds, t("Total timeout"), 1, 1_800)?;
        config.transcription.chunk_seconds =
            number(&self.chunk_seconds, t("Chunk length"), 10, 200)?;
        config.transcription.rate_limit_retry_max_wait_ms = number(
            &self.rate_limit_retry_max_wait_ms,
            t("Rate-limit retry wait"),
            0,
            60_000,
        )?;
        let temperature = self.temperature.trim();
        config.transcription.temperature = if temperature.is_empty() {
            None
        } else {
            let value: f32 = temperature
                .replace(',', ".")
                .parse()
                .map_err(|_| t("Temperature must be a number between 0 and 1.").to_owned())?;
            if !(0.0..=1.0).contains(&value) {
                return Err(t("Temperature must be a number between 0 and 1.").into());
            }
            Some(value)
        };
        Ok(config)
    }
}

/// Sets the model at `slot` (0 is the primary, then fallbacks in order).
/// `None` removes a fallback; the primary cannot be removed. Models beyond
/// the editable slots (added by hand to the file) are kept.
pub fn set_model(base: &Config, slot: usize, model: Option<&str>) -> Result<Config, String> {
    if slot > MAX_FALLBACKS {
        return Err(tf!(
            "At most {max_fallbacks} fallback models.",
            max_fallbacks = MAX_FALLBACKS
        ));
    }
    let mut config = base.clone();
    match model.map(str::trim) {
        None | Some("") if slot == 0 => return Err(t("Choose a primary model.").into()),
        None | Some("") => {
            if slot < config.transcription.models.len() {
                config.transcription.models.remove(slot);
            }
        }
        Some(model) => {
            if model.chars().any(char::is_whitespace) {
                return Err(t("Model ids cannot contain spaces.").into());
            }
            let model = crate::providers::ModelRef::parse(model).key();
            if let Some(existing) = config
                .transcription
                .models
                .iter()
                .position(|current| crate::providers::ModelRef::parse(current).key() == model)
                && existing != slot
            {
                return Err(if existing == 0 {
                    tf!("{model} is already the primary model.", model = model)
                } else {
                    tf!(
                        "{model} is already fallback {existing}.",
                        model = model,
                        existing = existing
                    )
                });
            }
            if config
                .transcription
                .models
                .get(slot)
                .is_some_and(|current| crate::providers::ModelRef::parse(current).key() == model)
            {
                return Ok(config);
            }
            if slot > 0 && crate::providers::is_realtime_only(&model) {
                return Err(tf!(
                    "{model} only works live while recording, so it can only be the primary model.",
                    model = model
                ));
            }
            let previous = config
                .transcription
                .models
                .get(slot)
                .or_else(|| config.transcription.models.first())
                .cloned();
            crate::providers::initialize_model(&mut config, &model, previous.as_deref());
            crate::providers::validate_profiles(&config.transcription.model_options)?;
            if slot < config.transcription.models.len() {
                config.transcription.models[slot] = model;
            } else {
                config.transcription.models.push(model);
            }
        }
    }
    Ok(config)
}

/// Moves the fallback at `slot` one place earlier, swapping with its
/// predecessor (which may be the primary). A realtime-only primary cannot
/// move down into a fallback slot.
pub fn promote_model(base: &Config, slot: usize) -> Result<Config, String> {
    let mut config = base.clone();
    if slot > 0 && slot < config.transcription.models.len() {
        if crate::providers::is_realtime_only(&config.transcription.models[slot - 1]) {
            return Err(t("A model that only works live must stay the primary model.").into());
        }
        config.transcription.models.swap(slot - 1, slot);
    }
    Ok(config)
}

pub fn remove_migrated_key(base: &Config, stored_key: &str) -> Result<Config, String> {
    if base.api_key.as_deref().map(str::trim) != Some(stored_key.trim()) {
        return Err(t("The file's API key changed. Reopen Models before moving it.").into());
    }
    let mut config = base.clone();
    config.api_key = None;
    Ok(config)
}

fn number(text: &str, field: &str, min: u64, max: u64) -> Result<u64, String> {
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|value| (min..=max).contains(value))
        .ok_or_else(|| {
            tf!(
                "{field} must be a whole number from {min} to {max}.",
                field = field,
                min = min,
                max = max
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_models(models: &[&str]) -> Config {
        let mut config = Config::default();
        config.transcription.models = models.iter().map(|model| (*model).into()).collect();
        config
    }

    #[test]
    fn default_config_round_trips_through_the_advanced_form() {
        let config = Config::default();
        assert_eq!(
            AdvancedForm::from_config(&config).apply(&config).unwrap(),
            config
        );
    }

    #[test]
    fn advanced_edits_apply_and_unshown_fields_survive() {
        let base = Config {
            api_key: Some("sk-from-file".into()),
            ..with_models(&["a/one", "b/two"])
        };
        let mut form = AdvancedForm::from_config(&base);
        form.attempt_timeout_seconds = " 12 ".into();
        form.temperature = "0,2".into();
        form.base_url = "https://proxy.test/api/v1/".into();
        let config = form.apply(&base).unwrap();
        assert_eq!(config.transcription.attempt_timeout_seconds, 12);
        assert_eq!(config.transcription.temperature, Some(0.2));
        assert_eq!(config.base_url, "https://proxy.test/api/v1");
        assert_eq!(config.api_key.as_deref(), Some("sk-from-file"));
        assert_eq!(config.transcription.models, ["a/one", "b/two"]);
        form.temperature = " ".into();
        assert_eq!(form.apply(&base).unwrap().transcription.temperature, None);
    }

    #[test]
    fn advanced_edits_preserve_external_changes_to_untouched_fields() {
        let mut latest = Config::default();
        let original = AdvancedForm::from_config(&latest);
        let mut edited = original.clone();
        edited.attempt_timeout_seconds = "12".into();
        latest.base_url = "https://proxy.test/v1".into();
        latest.transcription.temperature = None;
        latest.transcription.language = "pt".into();
        let saved = edited.apply_changes(&original, &latest).unwrap();
        latest.transcription.attempt_timeout_seconds = 12;
        assert_eq!(saved, latest);
    }

    #[test]
    fn key_migration_removes_only_the_key_that_was_stored() {
        let mut latest = Config {
            api_key: Some("test-key".into()),
            ..Config::default()
        };
        latest.transcription.language = "pt".into();
        let saved = remove_migrated_key(&latest, "test-key").unwrap();
        assert_eq!(saved.api_key, None);
        assert_eq!(saved.transcription.language, "pt");
        latest.api_key = Some("replacement".into());
        assert!(remove_migrated_key(&latest, "test-key").is_err());
    }

    type Edit = fn(&mut AdvancedForm);

    #[test]
    fn invalid_advanced_fields_are_rejected_with_their_name() {
        let base = Config::default();
        let cases: [(&str, Edit); 5] = [
            ("Attempt timeout", |form| {
                form.attempt_timeout_seconds = "0".into()
            }),
            ("Total timeout", |form| {
                form.total_timeout_seconds = "abc".into()
            }),
            ("Chunk length", |form| form.chunk_seconds = "500".into()),
            ("Temperature", |form| form.temperature = "1.5".into()),
            ("API URL", |form| form.base_url = "openrouter.ai".into()),
        ];
        for (expected, edit) in cases {
            let mut form = AdvancedForm::from_config(&base);
            edit(&mut form);
            let error = form.apply(&base).unwrap_err();
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn model_slots_replace_append_and_remove() {
        let base = with_models(&["a/one"]);
        let config = set_model(&base, 1, Some(" b/two ")).unwrap();
        assert_eq!(config.transcription.models, ["a/one", "b/two"]);
        let config = set_model(&config, 2, Some("c/three")).unwrap();
        assert_eq!(config.transcription.models, ["a/one", "b/two", "c/three"]);
        let config = set_model(&config, 0, Some("z/zero")).unwrap();
        assert_eq!(config.transcription.models, ["z/zero", "b/two", "c/three"]);
        let config = set_model(&config, 1, None).unwrap();
        assert_eq!(config.transcription.models, ["z/zero", "c/three"]);
        // Removing an empty slot is a no-op.
        assert_eq!(set_model(&config, 2, None).unwrap(), config);
    }

    #[test]
    fn model_slots_reject_invalid_choices() {
        let base = with_models(&["a/one", "b/two"]);
        assert!(set_model(&base, 0, None).unwrap_err().contains("primary"));
        assert!(set_model(&base, 0, Some(" ")).is_err());
        assert!(
            set_model(&base, 1, Some("a/one"))
                .unwrap_err()
                .contains("primary")
        );
        assert!(set_model(&base, 3, Some("c/three")).is_err());
        assert!(set_model(&base, 1, Some("has space")).is_err());
        // Re-choosing the same model in place is fine.
        assert_eq!(set_model(&base, 1, Some("b/two")).unwrap(), base);
    }

    #[test]
    fn hand_added_models_beyond_the_slots_survive() {
        let base = with_models(&["a", "b", "c", "d"]);
        let config = set_model(&base, 2, Some("x")).unwrap();
        assert_eq!(config.transcription.models, ["a", "b", "x", "d"]);
    }

    #[test]
    fn promoting_swaps_with_the_previous_model() {
        let base = with_models(&["a", "b", "c"]);
        assert_eq!(
            promote_model(&base, 2).unwrap().transcription.models,
            ["a", "c", "b"]
        );
        assert_eq!(
            promote_model(&base, 1).unwrap().transcription.models,
            ["b", "a", "c"]
        );
        assert_eq!(promote_model(&base, 0).unwrap(), base);
        assert_eq!(promote_model(&base, 9).unwrap(), base);
    }

    #[test]
    fn live_only_models_stay_the_primary_model() {
        let live_only = "elevenlabs::scribe_v2_realtime";
        let base = with_models(&[live_only, "grok::grok-voice-transcribe-2.0"]);
        // A fallback cannot be live-only, and the live-only primary cannot move down.
        assert!(set_model(&base, 1, Some(live_only)).is_err());
        assert!(set_model(&with_models(&["a"]), 1, Some(live_only)).is_err());
        assert!(promote_model(&base, 1).is_err());
        // It can still become the primary model.
        let config = set_model(&with_models(&["a", "b"]), 0, Some(live_only)).unwrap();
        assert_eq!(config.transcription.models[0], live_only);
        assert!(crate::providers::options(&config, live_only).streaming);
    }

    #[test]
    fn replacement_initializes_compatible_profile_once_and_removal_keeps_it() {
        use crate::providers::{ModelOptions, options};
        let mut base = with_models(&["deepgram::nova-3"]);
        base.transcription.model_options.insert(
            "deepgram::nova-3".into(),
            ModelOptions {
                language: "pt".into(),
                streaming: true,
                smart_format: true,
                ..ModelOptions::default()
            },
        );
        let first = set_model(&base, 0, Some("openai::gpt-transcribe")).unwrap();
        assert_eq!(options(&first, "openai::gpt-transcribe").language, "pt");
        assert!(!options(&first, "openai::gpt-transcribe").streaming);
        let restored = set_model(&first, 0, Some("deepgram::nova-3")).unwrap();
        assert!(options(&restored, "deepgram::nova-3").streaming);
        let added = set_model(&restored, 1, Some("deepgram::nova-2")).unwrap();
        let removed = set_model(&added, 1, None).unwrap();
        assert!(
            removed
                .transcription
                .model_options
                .contains_key("deepgram::nova-2")
        );
        assert!(
            set_model(
                &with_models(&["openai/model"]),
                1,
                Some("openrouter::openai/model")
            )
            .is_err()
        );
    }

    #[test]
    fn fallbacks_can_still_be_reordered_below_a_live_only_primary() {
        let live_only = "elevenlabs::scribe_v2_realtime";
        let base = with_models(&[live_only, "a", "b"]);
        assert_eq!(
            promote_model(&base, 2).unwrap().transcription.models,
            [live_only, "b", "a"]
        );
        assert!(promote_model(&base, 1).is_err());
        // Replacing a fallback with a live-only model fails without changing anything.
        assert!(set_model(&base, 2, Some(live_only)).is_err());
        assert!(set_model(&base, 2, Some("openai::gpt-live-transcribe")).is_err());
        // Swapping the live-only primary for an upload model is allowed.
        let config = set_model(&base, 0, Some("c")).unwrap();
        assert_eq!(config.transcription.models, ["c", "a", "b"]);
    }
}
