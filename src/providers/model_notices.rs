//! Per-model notices for selected primary/fallback rows. Pure configuration
//! inspection: no runtime globals, credentials, file access, or network calls.

use super::{ModelRef, Provider, options};
use crate::i18n::t;
use crate::openrouter::{AUTO_LANGUAGE, Config};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelNotice {
    pub text: &'static str,
    pub is_error: bool,
}

/// The caller controls placement: render only beneath a selected model, never
/// as provider-wide copy or next to unselected search results.
pub fn model_notices(config: &Config, id: &str) -> Vec<ModelNotice> {
    let model = ModelRef::parse(id);
    let options = options(config, id);
    let mut notices = Vec::new();

    // Published direct API conditions, checked against the separate batch and
    // realtime references. Do not infer these prices for OpenRouter routes.
    // https://elevenlabs.io/docs/api-reference/speech-to-text/convert
    // https://elevenlabs.io/docs/api-reference/speech-to-text/v-1-speech-to-text-realtime
    if model.provider == Provider::ElevenLabs {
        let text = match model.model {
            "scribe_v2" => Some(t(
                "Using keyterms adds 20% to the price. Over 100 terms also means a 20-second minimum charge per request.",
            )),
            "scribe_v2_realtime" => Some(t("Using keyterms adds 20% to the price.")),
            _ => None,
        };
        if let Some(text) = text {
            notices.push(ModelNotice {
                text,
                is_error: false,
            });
        }
    }

    if model.provider == Provider::Deepgram
        && model.model == "nova-2"
        && options.streaming
        && options.language == AUTO_LANGUAGE
    {
        notices.push(ModelNotice {
            text: t("Nova-2 uses recorded audio with Auto. Choose an explicit language to stream."),
            is_error: false,
        });
    }

    if model.provider == Provider::Grok
        && model.model == "grok-voice-transcribe-2.0"
        && options.smart_format
        && !super::batch::grok_format_supported(&options.language)
    {
        notices.push(ModelNotice {
            text: if options.language == AUTO_LANGUAGE {
                t("Smart formatting is inactive with Auto. Choose a supported language to enable it.")
            } else {
                t("Smart formatting is inactive for this language. Choose a supported language to enable it.")
            },
            is_error: false,
        });
    }

    if model.provider == Provider::Meta && super::meta::language_bias(&options.language).is_err() {
        notices.push(ModelNotice {
            text: t("Muse Voice does not support this language hint. Choose Auto or a supported language."),
            is_error: true,
        });
    }

    notices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ModelOptions;

    fn profile(config: &mut Config, id: &str, options: ModelOptions) {
        config
            .transcription
            .model_options
            .insert(ModelRef::parse(id).key(), options);
    }

    #[test]
    fn meta_language_warning_is_only_beneath_the_affected_selected_model() {
        let mut config = Config::default();
        let id = "meta::muse-voice-transcribe-1.0";
        for language in ["auto", "pt", "en"] {
            profile(
                &mut config,
                id,
                ModelOptions {
                    language: language.into(),
                    ..Default::default()
                },
            );
            assert!(model_notices(&config, id).is_empty());
        }
        profile(
            &mut config,
            id,
            ModelOptions {
                language: "ru".into(),
                ..Default::default()
            },
        );
        let notices = model_notices(&config, id);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].is_error);
        assert!(model_notices(&config, "openai::gpt-transcribe").is_empty());
        assert!(model_notices(&config, "meta/muse-voice-transcribe-1.0").is_empty());
    }

    #[test]
    fn elevenlabs_prices_are_conditional_and_specific_to_the_known_direct_model() {
        let config = Config::default();
        let batch = model_notices(&config, "elevenlabs::scribe_v2");
        assert_eq!(
            batch,
            [ModelNotice {
                text: "Using keyterms adds 20% to the price. Over 100 terms also means a 20-second minimum charge per request.",
                is_error: false,
            }]
        );
        let realtime = model_notices(&config, "elevenlabs::scribe_v2_realtime");
        assert_eq!(
            realtime,
            [ModelNotice {
                text: "Using keyterms adds 20% to the price.",
                is_error: false,
            }]
        );
        assert!(
            realtime
                .iter()
                .all(|notice| !notice.text.contains("minimum") && !notice.text.contains("100"))
        );
        for id in [
            "elevenlabs::scribe_v1",
            "elevenlabs::unknown",
            "elevenlabs/scribe_v2",
            "elevenlabs/scribe_v2_realtime",
            "openrouter::elevenlabs/scribe_v2",
            "openrouter::elevenlabs::scribe_v2",
            "openai::scribe_v2",
        ] {
            assert!(
                model_notices(&config, id).is_empty(),
                "unexpected notice for {id}"
            );
        }
    }

    #[test]
    fn live_only_models_always_stream_and_never_warn_about_being_skipped() {
        let mut config = Config::default();
        for id in [
            "openai::gpt-live-transcribe",
            "elevenlabs::scribe_v2_realtime",
            "google::gemini-3.5-transcribe-live",
        ] {
            profile(
                &mut config,
                id,
                ModelOptions {
                    streaming: false,
                    ..Default::default()
                },
            );
            assert!(crate::providers::is_realtime_only(id));
            assert!(crate::providers::options(&config, id).streaming);
            assert!(
                model_notices(&config, id)
                    .iter()
                    .all(|notice| !notice.is_error)
            );
        }
    }

    #[test]
    fn nova_two_auto_note_is_removed_when_language_is_explicit_or_streaming_is_off() {
        let id = "deepgram::nova-2";
        let mut config = Config::default();
        let notices = model_notices(&config, id);
        assert_eq!(notices.len(), 1);
        assert!(!notices[0].is_error);
        assert!(notices[0].text.contains("recorded audio"));
        for options in [
            ModelOptions {
                language: "pt".into(),
                ..Default::default()
            },
            ModelOptions {
                streaming: false,
                ..Default::default()
            },
        ] {
            profile(&mut config, id, options);
            assert!(model_notices(&config, id).is_empty());
        }
        for id in [
            "deepgram::nova-3",
            "deepgram/nova-2",
            "deepgram::nova-2-future",
        ] {
            assert!(model_notices(&config, id).is_empty());
        }
    }

    #[test]
    fn grok_format_note_uses_the_adapter_language_rules_and_follows_the_toggle() {
        let id = "grok::grok-voice-transcribe-2.0";
        let mut config = Config::default();
        for language in ["auto", "ko"] {
            profile(
                &mut config,
                id,
                ModelOptions {
                    language: language.into(),
                    smart_format: true,
                    ..Default::default()
                },
            );
            let notices = model_notices(&config, id);
            assert_eq!(notices.len(), 1);
            assert!(!notices[0].is_error);
            assert!(notices[0].text.contains("inactive"));
            profile(
                &mut config,
                id,
                ModelOptions {
                    language: language.into(),
                    smart_format: false,
                    ..Default::default()
                },
            );
            assert!(model_notices(&config, id).is_empty());
        }
        profile(
            &mut config,
            id,
            ModelOptions {
                language: "pt".into(),
                smart_format: true,
                ..Default::default()
            },
        );
        assert!(model_notices(&config, id).is_empty());
        for other in [
            "grok::unknown",
            "x-ai/grok-voice-transcribe-2.0",
            "google::gemini-3.5-transcribe",
        ] {
            assert!(model_notices(&config, other).is_empty());
        }
    }
}
