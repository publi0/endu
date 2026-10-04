//! Text-field projection of [`Config`] for the Settings view, with validation.
//! Kept free of GPUI so it is testable on any platform.

use super::Config;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Form {
    pub base_url: String,
    /// One model per line, in fallback order.
    pub transcription_models: String,
    pub attempt_timeout_seconds: String,
    pub total_timeout_seconds: String,
    pub chunk_seconds: String,
    pub rate_limit_retry_max_wait_ms: String,
    /// Empty means "provider default".
    pub temperature: String,
    pub trim_silence: bool,
    pub cleanup_enabled: bool,
    pub cleanup_models: String,
    pub cleanup_timeout_seconds: String,
    /// Empty means the built-in prompt.
    pub cleanup_prompt: String,
}

impl Form {
    pub fn from_config(config: &Config) -> Self {
        Self {
            base_url: config.base_url.clone(),
            transcription_models: config.transcription.models.join("\n"),
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
            trim_silence: config.transcription.trim_silence,
            cleanup_enabled: config.cleanup.enabled,
            cleanup_models: config.cleanup.models.join("\n"),
            cleanup_timeout_seconds: config.cleanup.timeout_seconds.to_string(),
            cleanup_prompt: config.cleanup.prompt.clone().unwrap_or_default(),
        }
    }

    /// Apply the form onto `base`, keeping fields the form does not show
    /// (such as a plaintext `api_key`). Errors name the offending field.
    pub fn apply(&self, base: &Config) -> Result<Config, String> {
        let mut config = base.clone();
        let base_url = self.base_url.trim().trim_end_matches('/');
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err("API URL must start with https:// or http://.".into());
        }
        config.base_url = base_url.to_owned();

        let models = parse_models(&self.transcription_models);
        if models.is_empty() {
            return Err("Add at least one transcription model.".into());
        }
        config.transcription.models = models;
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

        config.transcription.trim_silence = self.trim_silence;

        let cleanup_models = parse_models(&self.cleanup_models);
        if self.cleanup_enabled && cleanup_models.is_empty() {
            return Err("Add at least one cleanup model, or turn cleanup off.".into());
        }
        config.cleanup.enabled = self.cleanup_enabled;
        config.cleanup.models = cleanup_models;
        config.cleanup.timeout_seconds =
            number(&self.cleanup_timeout_seconds, "Cleanup timeout", 1, 300)?;
        let prompt = self.cleanup_prompt.trim();
        config.cleanup.prompt = (!prompt.is_empty()).then(|| prompt.to_owned());
        Ok(config)
    }
}

/// Models separated by newlines or commas, trimmed and de-duplicated in order.
fn parse_models(text: &str) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for model in text.split(['\n', ',']).map(str::trim) {
        if !model.is_empty() && !models.iter().any(|existing| existing == model) {
            models.push(model.to_owned());
        }
    }
    models
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

    #[test]
    fn default_config_round_trips_through_the_form() {
        let config = Config::default();
        assert_eq!(Form::from_config(&config).apply(&config).unwrap(), config);
    }

    #[test]
    fn edits_apply_and_unshown_fields_survive() {
        let base = Config {
            api_key: Some("sk-from-file".into()),
            ..Config::default()
        };
        let mut form = Form::from_config(&base);
        form.transcription_models = " a/one \n\nb/two, a/one ,c/three\n".into();
        form.attempt_timeout_seconds = " 12 ".into();
        form.temperature = "0,2".into();
        form.cleanup_enabled = true;
        form.trim_silence = false;
        form.cleanup_prompt = "  Clean it.  ".into();
        form.base_url = "https://proxy.test/api/v1/".into();
        let config = form.apply(&base).unwrap();
        assert_eq!(config.transcription.models, ["a/one", "b/two", "c/three"]);
        assert_eq!(config.transcription.attempt_timeout_seconds, 12);
        assert_eq!(config.transcription.temperature, Some(0.2));
        assert!(config.cleanup.enabled);
        assert!(!config.transcription.trim_silence);
        assert_eq!(config.cleanup.prompt.as_deref(), Some("Clean it."));
        assert_eq!(config.base_url, "https://proxy.test/api/v1");
        assert_eq!(config.api_key.as_deref(), Some("sk-from-file"));
    }

    #[test]
    fn blank_optional_fields_mean_defaults() {
        let mut form = Form::from_config(&Config::default());
        form.temperature = " ".into();
        form.cleanup_prompt = "\n".into();
        let config = form.apply(&Config::default()).unwrap();
        assert_eq!(config.transcription.temperature, None);
        assert_eq!(config.cleanup.prompt, None);
    }

    type Edit = fn(&mut Form);

    #[test]
    fn invalid_fields_are_rejected_with_their_name() {
        let base = Config::default();
        let cases: [(&str, Edit); 7] = [
            ("transcription model", |form| {
                form.transcription_models = " \n,".into()
            }),
            ("Attempt timeout", |form| {
                form.attempt_timeout_seconds = "0".into()
            }),
            ("Total timeout", |form| {
                form.total_timeout_seconds = "abc".into()
            }),
            ("Chunk length", |form| form.chunk_seconds = "500".into()),
            ("Temperature", |form| form.temperature = "1.5".into()),
            ("API URL", |form| form.base_url = "openrouter.ai".into()),
            ("cleanup model", |form| {
                form.cleanup_enabled = true;
                form.cleanup_models = String::new();
            }),
        ];
        for (expected, edit) in cases {
            let mut form = Form::from_config(&base);
            edit(&mut form);
            let error = form.apply(&base).unwrap_err();
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn cleanup_models_may_be_empty_while_cleanup_is_off() {
        let mut form = Form::from_config(&Config::default());
        form.cleanup_models = String::new();
        let config = form.apply(&Config::default()).unwrap();
        assert!(!config.cleanup.enabled);
        assert!(config.cleanup.models.is_empty());
    }
}
