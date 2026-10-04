//! Pure edits of [`Config`] for the Settings view, with validation. Kept free
//! of GPUI so it is testable on any platform.

use super::Config;

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

    /// Apply the form onto `base`, keeping every field it does not show.
    /// Errors name the offending field.
    pub fn apply(&self, base: &Config) -> Result<Config, String> {
        let mut config = base.clone();
        let base_url = self.base_url.trim().trim_end_matches('/');
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err("API URL must start with https:// or http://.".into());
        }
        config.base_url = base_url.to_owned();
        config.transcription.attempt_timeout_seconds =
            number(&self.attempt_timeout_seconds, "Attempt timeout", 1, 600)?;
        config.transcription.total_timeout_seconds =
            number(&self.total_timeout_seconds, "Total timeout", 1, 1_800)?;
        config.transcription.chunk_seconds = number(&self.chunk_seconds, "Chunk length", 10, 200)?;
        config.transcription.rate_limit_retry_max_wait_ms = number(
            &self.rate_limit_retry_max_wait_ms,
            "Rate-limit retry wait",
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
                .map_err(|_| "Temperature must be a number between 0 and 1.".to_owned())?;
            if !(0.0..=1.0).contains(&value) {
                return Err("Temperature must be a number between 0 and 1.".into());
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
        return Err(format!("At most {MAX_FALLBACKS} fallback models."));
    }
    let mut config = base.clone();
    let models = &mut config.transcription.models;
    match model.map(str::trim) {
        None | Some("") if slot == 0 => return Err("Choose a primary model.".into()),
        None | Some("") => {
            if slot < models.len() {
                models.remove(slot);
            }
        }
        Some(model) => {
            if model.chars().any(char::is_whitespace) {
                return Err("Model ids cannot contain spaces.".into());
            }
            if let Some(existing) = models.iter().position(|current| current == model)
                && existing != slot
            {
                return Err(if existing == 0 {
                    format!("{model} is already the primary model.")
                } else {
                    format!("{model} is already fallback {existing}.")
                });
            }
            if slot < models.len() {
                models[slot] = model.to_owned();
            } else {
                models.push(model.to_owned());
            }
        }
    }
    Ok(config)
}

/// Moves the fallback at `slot` one place earlier, swapping with its
/// predecessor (which may be the primary).
pub fn promote_model(base: &Config, slot: usize) -> Config {
    let mut config = base.clone();
    if slot > 0 && slot < config.transcription.models.len() {
        config.transcription.models.swap(slot - 1, slot);
    }
    config
}

pub fn set_language(base: &Config, language: &str) -> Result<Config, String> {
    if !super::LANGUAGES.iter().any(|(code, _)| *code == language) {
        return Err(format!("Unsupported language: {language}."));
    }
    let mut config = base.clone();
    config.transcription.language = language.to_owned();
    Ok(config)
}

fn number(text: &str, field: &str, min: u64, max: u64) -> Result<u64, String> {
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|value| (min..=max).contains(value))
        .ok_or_else(|| format!("{field} must be a whole number from {min} to {max}."))
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
            promote_model(&base, 2).transcription.models,
            ["a", "c", "b"]
        );
        assert_eq!(
            promote_model(&base, 1).transcription.models,
            ["b", "a", "c"]
        );
        assert_eq!(promote_model(&base, 0), base);
        assert_eq!(promote_model(&base, 9), base);
    }

    #[test]
    fn languages_are_validated() {
        let config = set_language(&Config::default(), "pt").unwrap();
        assert_eq!(config.transcription.language, "pt");
        assert!(set_language(&config, "xx").is_err());
    }
}
