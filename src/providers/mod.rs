//! Provider identities, model capabilities and persistent per-model options.
use crate::openrouter::Config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

pub mod batch;
pub mod keys;
mod meta;
mod model_notices;
pub use model_notices::model_notices;
mod microsoft;
pub use microsoft::{MicrosoftConfig, microsoft_endpoint};
pub mod streaming;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    OpenRouter,
    OpenAi,
    Deepgram,
    ElevenLabs,
    Microsoft,
    Grok,
    Google,
    Meta,
}
impl Provider {
    pub const ALL: [Self; 8] = [
        Self::OpenRouter,
        Self::OpenAi,
        Self::Deepgram,
        Self::ElevenLabs,
        Self::Microsoft,
        Self::Grok,
        Self::Google,
        Self::Meta,
    ];
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
            Self::Deepgram => "deepgram",
            Self::ElevenLabs => "elevenlabs",
            Self::Microsoft => "microsoft",
            Self::Grok => "grok",
            Self::Google => "google",
            Self::Meta => "meta",
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::OpenAi => "OpenAI",
            Self::Deepgram => "Deepgram",
            Self::ElevenLabs => "ElevenLabs",
            Self::Microsoft => "Microsoft",
            Self::Grok => "Grok (xAI)",
            Self::Google => "Google",
            Self::Meta => "Meta",
        }
    }
    pub const fn keys_url(self) -> &'static str {
        match self {
            Self::OpenRouter => "https://openrouter.ai/keys",
            Self::OpenAi => "https://platform.openai.com/api-keys",
            Self::Deepgram => "https://console.deepgram.com/",
            Self::ElevenLabs => "https://elevenlabs.io/app/settings/api-keys",
            Self::Microsoft => "https://ai.azure.com/",
            Self::Grok => "https://console.x.ai/",
            Self::Google => "https://aistudio.google.com/apikey",
            Self::Meta => "https://dev.meta.ai/",
        }
    }
    pub const fn env(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Deepgram => "DEEPGRAM_API_KEY",
            Self::ElevenLabs => "ELEVENLABS_API_KEY",
            Self::Microsoft => "AZURE_MAI_API_KEY",
            Self::Grok => "XAI_API_KEY",
            Self::Google => "GEMINI_API_KEY",
            Self::Meta => "MODEL_API_KEY",
        }
    }
}

/// APIs requiring full BCP-47 locales use the same choices as Models.
/// Portuguese defaults to Brazil, matching Hex's dictation locale.
pub fn bcp47_language(language: &str) -> Option<&'static str> {
    Some(match language.trim() {
        "pt" => "pt-BR",
        "en" => "en-US",
        "es" => "es-ES",
        "fr" => "fr-FR",
        "de" => "de-DE",
        "it" => "it-IT",
        "nl" => "nl-NL",
        "pl" => "pl-PL",
        "ru" => "ru-RU",
        "uk" => "uk-UA",
        "tr" => "tr-TR",
        "ar" => "ar-EG",
        "hi" => "hi-IN",
        "zh" => "zh-CN",
        "ja" => "ja-JP",
        "ko" => "ko-KR",
        "vi" => "vi-VN",
        "id" => "id-ID",
        "sv" => "sv-SE",
        "da" => "da-DK",
        "fi" => "fi-FI",
        "cs" => "cs-CZ",
        "el" => "el-GR",
        "ro" => "ro-RO",
        "hu" => "hu-HU",
        _ => return None,
    })
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

    /// Whether live transport can use this language without choosing one for the user.
    pub fn can_stream_language(self, language: &str) -> bool {
        self.capabilities().streaming
            && !(self.provider == Provider::Deepgram
                && self.model == "nova-2"
                && (language.trim().is_empty()
                    || language.trim() == crate::openrouter::AUTO_LANGUAGE))
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
    pub punctuate: bool,
    pub numerals: bool,
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
                punctuate: true,
                numerals: true,
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
                punctuate: true,
                numerals: true,
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
        NativeModel {
            provider: Provider::Microsoft,
            id: "MAI-Transcribe-2",
            name: "MAI-Transcribe 2",
            capabilities: Capabilities {
                batch: true,
                keywords: true,
                no_verbatim: true,
                ..Capabilities::default()
            },
        },
        NativeModel {
            provider: Provider::Microsoft,
            id: "MAI-Transcribe-2-Streaming",
            name: "MAI-Transcribe 2 Streaming",
            capabilities: Capabilities {
                streaming: true,
                ..Capabilities::default()
            },
        },
        NativeModel {
            provider: Provider::Grok,
            id: "grok-voice-transcribe-2.0",
            name: "Grok Voice Transcribe 2.0",
            capabilities: Capabilities {
                batch: true,
                streaming: true,
                keywords: true,
                formatting: true,
                no_verbatim: true,
                ..Capabilities::default()
            },
        },
        NativeModel {
            provider: Provider::Google,
            id: "gemini-3.5-transcribe",
            name: "Gemini 3.5 Transcribe",
            capabilities: Capabilities {
                batch: true,
                keywords: true,
                formatting: true,
                ..Capabilities::default()
            },
        },
        NativeModel {
            provider: Provider::Google,
            id: "gemini-3.5-transcribe-live",
            name: "Gemini 3.5 Transcribe Live",
            capabilities: Capabilities {
                streaming: true,
                keywords: true,
                formatting: true,
                ..Capabilities::default()
            },
        },
        NativeModel {
            provider: Provider::Meta,
            id: meta::MODEL,
            name: "Muse Voice Transcribe 1.0",
            capabilities: Capabilities {
                batch: true,
                streaming: true,
                keywords: true,
                ..Capabilities::default()
            },
        },
    ]
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOptions {
    pub language: String,
    /// Enabled by default where supported. Realtime-only models can be disabled;
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
            streaming: true,
            prompt: String::new(),
            temperature: None,
            smart_format: true,
            punctuate: true,
            numerals: true,
            no_verbatim: true,
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
        let defaults = Self::default();
        if !source.streaming {
            self.streaming = defaults.streaming;
        }
        if !(source.prompt && target.prompt) {
            self.prompt.clear();
        }
        if !(source.temperature && target.temperature) {
            self.temperature = None;
        }
        if !source.formatting {
            self.smart_format = defaults.smart_format;
        }
        if !source.punctuate {
            self.punctuate = defaults.punctuate;
        }
        if !source.numerals {
            self.numerals = defaults.numerals;
        }
        if !source.no_verbatim {
            self.no_verbatim = defaults.no_verbatim;
        }
        self.for_capabilities(target)
    }

    fn for_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.streaming &= capabilities.streaming;
        self.smart_format &= capabilities.formatting;
        self.punctuate &= capabilities.punctuate;
        self.numerals &= capabilities.numerals;
        self.no_verbatim &= capabilities.no_verbatim;
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
        .for_capabilities(ModelRef::parse(id).capabilities())
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
                && (model.provider != Provider::Microsoft
                    || microsoft_endpoint(&c, !caps.batch).is_ok())
                && keys::api_key(model.provider, &c).is_ok()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_supported_defaults(options: &ModelOptions, capabilities: Capabilities) {
        assert_eq!(options.streaming, capabilities.streaming);
        assert_eq!(options.smart_format, capabilities.formatting);
        assert_eq!(options.punctuate, capabilities.punctuate);
        assert_eq!(options.numerals, capabilities.numerals);
        assert_eq!(options.no_verbatim, capabilities.no_verbatim);
    }

    #[test]
    fn new_native_models_keep_transport_capabilities_distinct_from_routes() {
        let cases = [
            (
                "meta::muse-voice-transcribe-1.0",
                Provider::Meta,
                true,
                true,
                true,
            ),
            (
                "microsoft::MAI-Transcribe-2",
                Provider::Microsoft,
                true,
                false,
                true,
            ),
            (
                "microsoft::MAI-Transcribe-2-Streaming",
                Provider::Microsoft,
                false,
                true,
                false,
            ),
            (
                "grok::grok-voice-transcribe-2.0",
                Provider::Grok,
                true,
                true,
                true,
            ),
            (
                "google::gemini-3.5-transcribe",
                Provider::Google,
                true,
                false,
                true,
            ),
            (
                "google::gemini-3.5-transcribe-live",
                Provider::Google,
                false,
                true,
                true,
            ),
        ];
        for (id, provider, batch, streaming, keywords) in cases {
            let model = ModelRef::parse(id);
            assert_eq!(model.provider, provider);
            assert_eq!(model.key(), id);
            let caps = model.capabilities();
            assert_eq!(
                (caps.batch, caps.streaming, caps.keywords),
                (batch, streaming, keywords)
            );
            assert!(!caps.punctuate && !caps.numerals);
            assert_supported_defaults(&options(&Config::default(), id), caps);
        }
        for route in [
            "microsoft/mai-transcribe-2",
            "google/gemini-3.5-transcribe",
            "x-ai/grok-voice-transcribe-2.0",
        ] {
            assert_eq!(ModelRef::parse(route).provider, Provider::OpenRouter);
        }
        for provider in Provider::ALL {
            assert_eq!(
                Provider::ALL
                    .iter()
                    .filter(|p| p.id() == provider.id())
                    .count(),
                1
            );
        }
    }

    #[test]
    fn every_explicit_ui_language_has_a_full_locale() {
        assert_eq!(bcp47_language("auto"), None);
        assert_eq!(bcp47_language("pt"), Some("pt-BR"));
        for (code, _) in crate::openrouter::LANGUAGES {
            if *code != "auto" {
                assert!(bcp47_language(code).is_some(), "{code}");
            }
        }
    }

    #[test]
    fn streaming_language_support_never_invents_a_nova_2_language() {
        let nova_2 = ModelRef::parse("deepgram::nova-2");
        assert!(!nova_2.can_stream_language("auto"));
        assert!(!nova_2.can_stream_language(""));
        assert!(!nova_2.can_stream_language(" auto "));
        assert!(nova_2.can_stream_language("pt"));
        assert!(nova_2.can_stream_language("en"));
        for id in [
            "deepgram::nova-3",
            "openai::gpt-live-transcribe",
            "elevenlabs::scribe_v2_realtime",
        ] {
            assert!(ModelRef::parse(id).can_stream_language("auto"));
            assert!(ModelRef::parse(id).can_stream_language("pt"));
        }
        for id in [
            "deepgram/nova-2",
            "openai::gpt-transcribe",
            "elevenlabs::scribe_v2",
            "deepgram::unknown",
        ] {
            assert!(!ModelRef::parse(id).can_stream_language("pt"));
        }
        let options = options(&Config::default(), "deepgram::nova-2");
        assert!(options.streaming);
        assert_eq!(options.language, "auto");
    }

    #[test]
    fn missing_profiles_enable_only_supported_toggles_and_keep_language_and_temperature() {
        let mut config = Config::default();
        config.transcription.language = "pt".into();
        config.transcription.temperature = Some(0.3);
        let mut ids: Vec<_> = native_models()
            .into_iter()
            .map(|model| {
                ModelRef {
                    provider: model.provider,
                    model: model.id,
                }
                .key()
            })
            .collect();
        ids.extend(["openai/gpt-transcribe".into(), "openai::unknown".into()]);
        for id in ids {
            let options = options(&config, &id);
            assert_supported_defaults(&options, ModelRef::parse(&id).capabilities());
            assert_eq!(options.language, "pt");
            assert_eq!(options.temperature, Some(0.3));
            assert!(options.prompt.is_empty());
        }
    }

    #[test]
    fn sparse_profiles_default_on_but_explicit_false_survives_round_trip() {
        let raw: ModelOptions = serde_json::from_str("{}").unwrap();
        assert!(
            raw.streaming && raw.smart_format && raw.punctuate && raw.numerals && raw.no_verbatim
        );
        let config: Config = serde_json::from_str(r#"{"transcription":{"model_options":{
            "deepgram::nova-3":{"language":"pt"},
            "elevenlabs::scribe_v2":{},
            "deepgram::nova-2":{"streaming":false,"smart_format":false,"punctuate":false,"numerals":false,"no_verbatim":false},
            "elevenlabs::scribe_v2_realtime":{"streaming":false,"no_verbatim":false}
        }}}"#).unwrap();
        let config: Config = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        for id in ["deepgram::nova-3", "elevenlabs::scribe_v2"] {
            assert_supported_defaults(&options(&config, id), ModelRef::parse(id).capabilities());
        }
        assert_eq!(options(&config, "deepgram::nova-3").language, "pt");
        for id in ["deepgram::nova-2", "elevenlabs::scribe_v2_realtime"] {
            let options = options(&config, id);
            assert!(
                !options.streaming
                    && !options.smart_format
                    && !options.punctuate
                    && !options.numerals
                    && !options.no_verbatim
            );
        }
    }

    #[test]
    fn inheritance_defaults_target_only_features_on_and_preserves_shared_false() {
        let mut config = Config::default();
        config.transcription.model_options.insert(
            "openai::gpt-transcribe".into(),
            ModelOptions {
                language: "pt".into(),
                prompt: "Existing context".into(),
                temperature: Some(0.4),
                streaming: false,
                smart_format: false,
                punctuate: false,
                numerals: false,
                no_verbatim: false,
            },
        );
        for id in [
            "deepgram::nova-3",
            "openai::gpt-live-transcribe",
            "elevenlabs::scribe_v2",
        ] {
            initialize_model(&mut config, id, Some("openai::gpt-transcribe"));
            let options = options(&config, id);
            assert_supported_defaults(&options, ModelRef::parse(id).capabilities());
            assert_eq!(options.language, "pt");
        }
        assert_eq!(
            options(&config, "openai::gpt-live-transcribe").prompt,
            "Existing context"
        );
        assert_eq!(
            options(&config, "openai::gpt-transcribe").temperature,
            Some(0.4)
        );
        let source = config
            .transcription
            .model_options
            .get_mut("deepgram::nova-3")
            .unwrap();
        source.streaming = false;
        source.smart_format = false;
        source.punctuate = false;
        source.numerals = false;
        initialize_model(&mut config, "deepgram::nova-2", Some("deepgram::nova-3"));
        let target = options(&config, "deepgram::nova-2");
        assert!(!target.streaming && !target.smart_format && !target.punctuate && !target.numerals);
        initialize_model(
            &mut config,
            "elevenlabs::scribe_v2_realtime",
            Some("deepgram::nova-3"),
        );
        let target = options(&config, "elevenlabs::scribe_v2_realtime");
        assert!(!target.streaming);
        assert!(target.no_verbatim);
        initialize_model(
            &mut config,
            "deepgram::nova-2",
            Some("openai::gpt-transcribe"),
        );
        assert!(
            !options(&config, "deepgram::nova-2").streaming,
            "reselection must not overwrite the saved profile"
        );
    }

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
