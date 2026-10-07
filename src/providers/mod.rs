//! Provider identities, model capabilities and persistent per-model options.
use crate::openrouter::Config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

pub mod batch;
pub mod keys;
pub mod streaming;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    OpenRouter,
    OpenAi,
    Deepgram,
    ElevenLabs,
}
impl Provider {
    pub const ALL: [Self; 4] = [
        Self::OpenRouter,
        Self::OpenAi,
        Self::Deepgram,
        Self::ElevenLabs,
    ];
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
            Self::Deepgram => "deepgram",
            Self::ElevenLabs => "elevenlabs",
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::OpenAi => "OpenAI",
            Self::Deepgram => "Deepgram",
            Self::ElevenLabs => "ElevenLabs",
        }
    }
    pub const fn keys_url(self) -> &'static str {
        match self {
            Self::OpenRouter => "https://openrouter.ai/keys",
            Self::OpenAi => "https://platform.openai.com/api-keys",
            Self::Deepgram => "https://console.deepgram.com/",
            Self::ElevenLabs => "https://elevenlabs.io/app/settings/api-keys",
        }
    }
    pub const fn env(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Deepgram => "DEEPGRAM_API_KEY",
            Self::ElevenLabs => "ELEVENLABS_API_KEY",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelRef<'a> {
    pub provider: Provider,
    pub model: &'a str,
}
impl<'a> ModelRef<'a> {
    pub fn parse(id: &'a str) -> Self {
        for provider in Provider::ALL {
            if let Some(model) = id
                .strip_prefix(provider.id())
                .and_then(|s| s.strip_prefix("::"))
            {
                return Self { provider, model };
            }
        }
        Self {
            provider: Provider::OpenRouter,
            model: id,
        }
    }
    pub fn key(self) -> String {
        if self.provider == Provider::OpenRouter {
            self.model.into()
        } else {
            format!("{}::{}", self.provider.id(), self.model)
        }
    }
    pub fn label(self) -> String {
        format!("{} · {}", self.provider.label(), self.model)
    }
    pub fn capabilities(self) -> Capabilities {
        if self.provider == Provider::OpenRouter {
            return Capabilities {
                batch: true,
                keywords: self.model == "microsoft/mai-transcribe-2",
                temperature: true,
                ..Capabilities::default()
            };
        }
        native_models()
            .into_iter()
            .find(|m| m.provider == self.provider && m.id == self.model)
            .map_or(Capabilities::default(), |m| m.capabilities)
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    pub batch: bool,
    pub streaming: bool,
    pub keywords: bool,
    pub prompt: bool,
    pub temperature: bool,
    pub formatting: bool,
    pub no_verbatim: bool,
}
#[derive(Clone)]
pub struct NativeModel {
    pub provider: Provider,
    pub id: &'static str,
    pub name: &'static str,
    pub capabilities: Capabilities,
}
pub fn native_models() -> Vec<NativeModel> {
    let openai = Capabilities {
        batch: true,
        keywords: true,
        prompt: true,
        temperature: true,
        ..Capabilities::default()
    };
    let scribe = Capabilities {
        batch: true,
        keywords: true,
        no_verbatim: true,
        ..Capabilities::default()
    };
    vec![
        NativeModel {
            provider: Provider::OpenAi,
            id: "gpt-transcribe",
            name: "GPT Transcribe",
            capabilities: openai,
        },
        NativeModel {
            provider: Provider::OpenAi,
            id: "gpt-live-transcribe",
            name: "GPT Live Transcribe",
            capabilities: Capabilities {
                batch: false,
                streaming: true,
                temperature: false,
                ..openai
            },
        },
        NativeModel {
            provider: Provider::OpenAi,
            id: "gpt-4o-transcribe",
            name: "GPT-4o Transcribe",
            capabilities: openai,
        },
        NativeModel {
            provider: Provider::OpenAi,
            id: "gpt-4o-mini-transcribe",
            name: "GPT-4o Mini Transcribe",
            capabilities: openai,
        },
        NativeModel {
            provider: Provider::OpenAi,
            id: "whisper-1",
            name: "Whisper",
            capabilities: openai,
        },
        NativeModel {
            provider: Provider::Deepgram,
            id: "nova-3",
            name: "Nova-3",
            capabilities: Capabilities {
                batch: true,
                streaming: true,
                keywords: true,
                formatting: true,
                ..Capabilities::default()
            },
        },
        NativeModel {
            provider: Provider::Deepgram,
            id: "nova-2",
            name: "Nova-2",
            capabilities: Capabilities {
                batch: true,
                streaming: true,
                formatting: true,
                ..Capabilities::default()
            },
        },
        NativeModel {
            provider: Provider::ElevenLabs,
            id: "scribe_v2",
            name: "Scribe v2",
            capabilities: scribe,
        },
        NativeModel {
            provider: Provider::ElevenLabs,
            id: "scribe_v2_realtime",
            name: "Scribe v2 Realtime",
            capabilities: Capabilities {
                batch: false,
                streaming: true,
                ..scribe
            },
        },
    ]
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOptions {
    pub language: String,
    /// Opt-in for models with both modes. Realtime-only models can be disabled;
    /// the chain then skips them, without silently selecting a different model.
    pub streaming: bool,
    pub prompt: String,
    pub temperature: Option<f32>,
    pub smart_format: bool,
    pub punctuate: bool,
    pub numerals: bool,
    pub no_verbatim: bool,
}
impl Default for ModelOptions {
    fn default() -> Self {
        Self {
            language: "auto".into(),
            streaming: false,
            prompt: String::new(),
            temperature: None,
            smart_format: false,
            punctuate: true,
            numerals: false,
            no_verbatim: false,
        }
    }
}
impl ModelOptions {
    pub fn validate(&self) -> Result<(), String> {
        if !crate::openrouter::LANGUAGES
            .iter()
            .any(|(code, _)| *code == self.language)
        {
            return Err("Choose a supported language.".into());
        }
        if self.prompt.len() > 2000
            || self
                .prompt
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err("Context must contain at most 2,000 bytes of text.".into());
        }
        if self
            .temperature
            .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        {
            return Err("Temperature must be between 0 and 1.".into());
        }
        Ok(())
    }
    pub fn inherited(mut self, source: Capabilities, target: Capabilities) -> Self {
        self.streaming = source.streaming && target.streaming && self.streaming;
        if !(source.prompt && target.prompt) {
            self.prompt.clear();
        }
        if !(source.temperature && target.temperature) {
            self.temperature = None;
        }
        if !(source.formatting && target.formatting) {
            self.smart_format = false;
            self.punctuate = true;
            self.numerals = false;
        }
        if !(source.no_verbatim && target.no_verbatim) {
            self.no_verbatim = false;
        }
        self
    }
}
pub fn options(config: &Config, id: &str) -> ModelOptions {
    config
        .transcription
        .model_options
        .get(&ModelRef::parse(id).key())
        .cloned()
        .unwrap_or_else(|| ModelOptions {
            language: config.transcription.language.clone(),
            temperature: config.transcription.temperature,
            ..ModelOptions::default()
        })
}
pub fn initialize_model(config: &mut Config, id: &str, previous: Option<&str>) {
    let key = ModelRef::parse(id).key();
    if config.transcription.model_options.contains_key(&key) {
        return;
    }
    let target = ModelRef::parse(id).capabilities();
    let inherited = previous
        .map(|old| options(config, old).inherited(ModelRef::parse(old).capabilities(), target))
        .unwrap_or_else(|| options(config, id));
    config.transcription.model_options.insert(key, inherited);
}
pub fn validate_profiles(profiles: &BTreeMap<String, ModelOptions>) -> Result<(), String> {
    if profiles.len() > 128 {
        return Err("At most 128 saved model profiles.".into());
    }
    for (id, options) in profiles {
        if id.is_empty() || id.len() > 200 || id.chars().any(char::is_whitespace) {
            return Err("Invalid model profile id.".into());
        }
        options.validate()?;
    }
    Ok(())
}
pub fn has_keyword_support(config: &Config, allow_cached_evidence: bool) -> bool {
    config.transcription.models.iter().any(|id| {
        ModelRef::parse(id).capabilities().keywords
            || (allow_cached_evidence
                && crate::openrouter::vocabulary_support::has_verified_support(config, id))
    })
}

static RUNTIME: LazyLock<RwLock<Option<Config>>> = LazyLock::new(|| RwLock::new(None));
pub fn apply_runtime(config: &Config) {
    *RUNTIME.write().unwrap_or_else(|e| e.into_inner()) = Some(config.clone());
}
pub fn runtime_config() -> Option<Config> {
    RUNTIME.read().unwrap_or_else(|e| e.into_inner()).clone()
}
pub fn is_configured() -> bool {
    crate::openrouter::load_config().is_ok_and(|c| {
        c.transcription.models.iter().any(|id| {
            let model = ModelRef::parse(id);
            let caps = model.capabilities();
            (caps.batch || (caps.streaming && options(&c, id).streaming))
                && keys::api_key(model.provider, &c).is_ok()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keywords_are_enabled_by_any_fallback_not_just_the_primary() {
        let mut c = Config::default();
        c.transcription.models = vec!["acme/no-hints".into(), "deepgram::nova-2".into()];
        assert!(!has_keyword_support(&c, false));
        c.transcription.models.push("elevenlabs::scribe_v2".into());
        assert!(has_keyword_support(&c, false));
        c.transcription.models.remove(2);
        assert!(!has_keyword_support(&c, false));
    }
    #[test]
    fn legacy_config_retains_models_and_has_no_native_profiles() {
        let c: Config = serde_json::from_str(
            r#"{"transcription":{"models":["openai/gpt-transcribe"],"language":"pt"}}"#,
        )
        .unwrap();
        assert_eq!(c.transcription.models, vec!["openai/gpt-transcribe"]);
        assert!(c.transcription.model_options.is_empty());
        assert_eq!(options(&c, "openai/gpt-transcribe").language, "pt");
        assert!(!options(&c, "openai/gpt-transcribe").streaming);
    }
    #[test]
    fn unknown_models_never_advertise_native_features() {
        let m = ModelRef::parse("openai::unknown");
        assert_eq!(m.capabilities(), Capabilities::default());
        assert!(
            !ModelRef::parse("openai/gpt-live-transcribe")
                .capabilities()
                .streaming
        );
    }

    #[test]
    fn identity_does_not_confuse_openrouter_vendor_with_direct_provider() {
        assert_eq!(
            ModelRef::parse("openai/gpt-transcribe").provider,
            Provider::OpenRouter
        );
        assert_eq!(
            ModelRef::parse("openai::gpt-transcribe").provider,
            Provider::OpenAi
        );
    }
    #[test]
    fn new_profiles_inherit_compatible_options_once() {
        let mut c = Config::default();
        c.transcription.model_options.insert(
            "deepgram::nova-3".into(),
            ModelOptions {
                streaming: true,
                language: "pt".into(),
                smart_format: true,
                ..ModelOptions::default()
            },
        );
        initialize_model(&mut c, "deepgram::nova-2", Some("deepgram::nova-3"));
        assert!(options(&c, "deepgram::nova-2").streaming);
        initialize_model(&mut c, "openai::gpt-transcribe", Some("deepgram::nova-3"));
        let p = options(&c, "openai::gpt-transcribe");
        assert_eq!(p.language, "pt");
        assert!(!p.streaming);
        assert!(!p.smart_format);
        c.transcription
            .model_options
            .get_mut("deepgram::nova-3")
            .unwrap()
            .language = "en".into();
        initialize_model(&mut c, "deepgram::nova-2", Some("deepgram::nova-3"));
        assert_eq!(options(&c, "deepgram::nova-2").language, "pt");
    }
}
