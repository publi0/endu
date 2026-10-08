//! OpenRouter's speech-to-text model catalog, for the model pickers.

use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail};
use serde::Deserialize;

use super::{Config, http};

const CATALOG_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogModel {
    /// The id sent in requests, such as `openai/whisper-large-v3-turbo`.
    pub id: String,
    /// The display name without its provider prefix.
    pub name: String,
    pub provider: String,
}

impl CatalogModel {
    pub fn matches(&self, query: &str, verified_keywords: bool) -> bool {
        use crate::providers::{ModelRef, Provider};
        let model = ModelRef::parse(&self.id);
        let capabilities = model.capabilities();
        let query = search_text(query);
        let text = search_text(&format!(
            "{} {} {} {} {}",
            self.id,
            self.name,
            self.provider,
            model.provider.label(),
            match model.provider {
                Provider::Microsoft => "Azure",
                Provider::Meta => "Facebook Muse",
                _ => "",
            },
        ));
        let badges = capability_badges(&self.id, verified_keywords);
        query.split_whitespace().all(|word| {
            // Whole capability terms are filters, even when an unsupported
            // route's display name happens to contain that word (e.g. Live).
            if let Some(supported) = capability_term(word, capabilities, verified_keywords) {
                return supported;
            }
            text.contains(word)
                || badges
                    .iter()
                    .any(|(_, label)| search_text(label).starts_with(word))
                || [
                    ("punctuation", capabilities.punctuate),
                    ("numerals", capabilities.numerals),
                    ("temperature", capabilities.temperature),
                ]
                .iter()
                .any(|(label, supported)| *supported && label.starts_with(word))
        })
    }
}

fn capability_term(
    word: &str,
    capabilities: crate::providers::Capabilities,
    verified_keywords: bool,
) -> Option<bool> {
    Some(match word {
        "streaming" | "live" | "realtime" | "stream" | "tempo-real" | "vivo" => {
            capabilities.streaming
        }
        "batch" | "file" | "upload" | "arquivo" | "arquivos" | "gravado" | "gravacao" => {
            capabilities.batch
        }
        "keywords" | "keyword" | "keyterms" | "keyterm" | "vocabulary" | "vocabulario"
        | "palavras-chave" | "termos" => capabilities.keywords || verified_keywords,
        "context" | "prompt" | "contexto" | "instrucoes" => capabilities.prompt,
        "format" | "formatting" | "formato" | "formatacao" => capabilities.formatting,
        "clean" | "cleanup" | "limpeza" | "limpar" => capabilities.no_verbatim,
        "punctuation" | "punctuate" | "pontuacao" => capabilities.punctuate,
        "numerals" | "numbers" | "digits" | "numeros" | "digitos" => capabilities.numerals,
        "temperature" | "temperatura" => capabilities.temperature,
        _ => return None,
    })
}

fn search_text(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter_map(|character| {
            Some(match character {
                'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
                'é' | 'è' | 'ê' | 'ë' => 'e',
                'í' | 'ì' | 'î' | 'ï' => 'i',
                'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
                'ú' | 'ù' | 'û' | 'ü' => 'u',
                'ç' => 'c',
                '\u{0300}'..='\u{036f}' => return None,
                character => character,
            })
        })
        .collect()
}

/// Native choices remain available even when the OpenRouter catalog is offline.
pub fn native_catalog() -> Vec<CatalogModel> {
    crate::providers::native_models()
        .into_iter()
        .map(|model| CatalogModel {
            id: crate::providers::ModelRef {
                provider: model.provider,
                model: model.id,
            }
            .key(),
            name: model.name.into(),
            provider: model.provider.label().into(),
        })
        .collect()
}

pub fn available_catalog(remote: &[CatalogModel]) -> Vec<CatalogModel> {
    let mut models = native_catalog();
    for id in Config::default().transcription.models {
        let (_, name) = split_name(&id, "");
        models.push(CatalogModel {
            id,
            name,
            provider: "OpenRouter".into(),
        });
    }
    for model in remote {
        if let Some(existing) = models.iter_mut().find(|existing| existing.id == model.id) {
            *existing = model.clone();
        } else {
            models.push(model.clone());
        }
    }
    models
}

pub fn provider_label(id: &str) -> &'static str {
    crate::providers::ModelRef::parse(id).provider.label()
}

/// SF Symbol and short text; render each pair without shrinking or ellipsis.
pub fn capability_badges(id: &str, verified_keywords: bool) -> Vec<(&'static str, &'static str)> {
    let caps = crate::providers::ModelRef::parse(id).capabilities();
    let mut labels = Vec::new();
    if caps.batch {
        labels.push(("doc", "File"));
    }
    if caps.streaming {
        labels.push(("bolt.fill", "Streaming"));
    }
    if caps.keywords || verified_keywords {
        labels.push(("number", "Keywords"));
    }
    if caps.prompt {
        labels.push(("text.alignleft", "Context"));
    }
    if caps.formatting {
        labels.push(("textformat", "Format"));
    }
    if caps.no_verbatim {
        labels.push(("sparkles", "Clean"));
    }
    labels
}

/// Fetches the models that output transcriptions. The endpoint is public, so
/// a missing key is not an error here.
pub fn fetch(config: &Config) -> Result<Vec<CatalogModel>> {
    let response = http::get(
        &config.endpoint("models?output_modalities=transcription"),
        "",
        CATALOG_TIMEOUT,
    )
    .wrap_err("could not reach OpenRouter")?;
    if !response.is_success() {
        bail!(
            "OpenRouter returned HTTP {}: {}",
            response.status,
            super::excerpt(&response.body)
        );
    }
    parse(&response.body)
}

#[derive(Deserialize)]
struct Listing {
    #[serde(alias = "models")]
    data: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    #[serde(default)]
    name: String,
}

fn parse(body: &[u8]) -> Result<Vec<CatalogModel>> {
    let listing: Listing =
        serde_json::from_slice(body).wrap_err("OpenRouter returned an unexpected model list")?;
    let mut models: Vec<CatalogModel> = listing
        .data
        .into_iter()
        .filter(|entry| !entry.id.trim().is_empty())
        .map(|entry| {
            let (provider, name) = split_name(&entry.id, &entry.name);
            CatalogModel {
                id: entry.id,
                name,
                provider: format!("OpenRouter · {provider}"),
            }
        })
        .collect();
    models.sort_by(|left, right| {
        (left.provider.to_lowercase(), left.name.to_lowercase())
            .cmp(&(right.provider.to_lowercase(), right.name.to_lowercase()))
    });
    models.dedup_by(|left, right| left.id == right.id);
    Ok(models)
}

/// `"OpenAI: Whisper Large V3"` → (`OpenAI`, `Whisper Large V3`), falling back
/// to the id's provider segment when the name has no prefix.
fn split_name(id: &str, name: &str) -> (String, String) {
    if let Some((provider, rest)) = name.split_once(": ")
        && !provider.trim().is_empty()
        && !rest.trim().is_empty()
    {
        return (provider.trim().to_owned(), rest.trim().to_owned());
    }
    let provider = id.split_once('/').map_or("", |(provider, _)| provider);
    let name = if name.trim().is_empty() {
        id.split_once('/').map_or(id, |(_, model)| model)
    } else {
        name.trim()
    };
    (provider.to_owned(), name.to_owned())
}

/// A readable label for a configured model id, using the catalog when it
/// knows the id.
pub fn label(id: &str, catalog: &[CatalogModel]) -> String {
    catalog
        .iter()
        .find(|model| model.id == id)
        .map(|model| model.name.clone())
        .or_else(|| {
            native_catalog()
                .into_iter()
                .find(|model| model.id == id)
                .map(|model| model.name)
        })
        .unwrap_or_else(|| crate::providers::ModelRef::parse(id).model.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(id: &str) -> CatalogModel {
        native_catalog()
            .into_iter()
            .find(|model| model.id == id)
            .unwrap()
    }

    #[test]
    fn search_combines_provider_and_supported_capabilities_with_and() {
        let meta = native("meta::muse-voice-transcribe-1.0");
        for query in [
            "meta streaming keywords",
            "facebook streaming",
            "muse batch",
            "muse voice",
        ] {
            assert!(meta.matches(query, false));
        }
        assert!(!meta.matches("meta context", false));
        assert!(!meta.matches("meta formatting", false));
        let google_live = native("google::gemini-3.5-transcribe-live");
        let google_file = native("google::gemini-3.5-transcribe");
        assert!(google_live.matches(" \tGOOGLE \n STREAMING  ", false));
        assert!(google_live.matches("google live", false));
        assert!(google_live.matches("google realtime", false));
        assert!(google_live.matches("google stream", false));
        assert!(!google_file.matches("google streaming", false));
        assert!(google_file.matches("google batch upload file", false));
        assert!(!google_live.matches("google file", false));
        assert!(google_live.matches("goo stre keyw", false));
        assert!(google_live.matches("\t \n", false));
        assert!(native("deepgram::nova-3").matches("deepgram streaming keywords", false));
        assert!(!native("deepgram::nova-2").matches("deepgram streaming keywords", false));
        assert!(!google_live.matches("google streaming deepgram", false));
    }

    #[test]
    fn provider_aliases_and_portuguese_capabilities_are_searchable() {
        assert!(native("grok::grok-voice-transcribe-2.0").matches("xAI clean format", false));
        assert!(native("grok::grok-voice-transcribe-2.0").matches("xa", false));
        let mai = native("microsoft::MAI-Transcribe-2");
        assert!(mai.matches("AZURE ARQUIVO LIMPEZA VOCABULÁRIO", false));
        assert!(mai.matches("azu vocabulario", false));
        assert!(mai.matches("azure vocabulA\u{0301}rio", false));
        assert!(native("microsoft::MAI-Transcribe-2-Streaming").matches("azure streaming", false));
        assert!(!mai.matches("azure streaming", false));
        assert!(native("deepgram::nova-3").matches("formatação pontuação números", false));
        assert!(native("openai::gpt-transcribe").matches("contexto temperatura", false));
    }

    #[test]
    fn whole_capability_terms_do_not_match_unsupported_words_in_remote_names() {
        let model = CatalogModel {
            id: "acme/live-realtime-streaming-keywords-context-clean-formatting".into(),
            name: "Live Streaming Keywords Context Clean Formatting Punctuation Numerals".into(),
            provider: "OpenRouter · Acme".into(),
        };
        for word in [
            "live",
            "stream",
            "streaming",
            "realtime",
            "keywords",
            "keyterms",
            "vocabulary",
            "context",
            "prompt",
            "clean",
            "cleanup",
            "format",
            "formatting",
            "punctuation",
            "numerals",
        ] {
            assert!(!model.matches(word, false), "{word} is a support filter");
        }
        assert!(
            model.matches("acme/live", false),
            "model IDs still support substrings"
        );
        assert!(model.matches("openrouter file temperature", false));
        assert!(model.matches("openrouter keywords", true));
        assert!(!model.matches("openrouter keywords streaming", true));
        assert!(model.matches("keyw", true));
        assert!(
            !native("google::gemini-3.5-transcribe")
                .matches("context temperature punctuation numerals clean", false)
        );
    }

    #[test]
    fn verified_keyword_evidence_and_badge_prefixes_share_the_same_contract() {
        let model = CatalogModel {
            id: "acme/transcribe".into(),
            name: "Acme Transcribe".into(),
            provider: "OpenRouter".into(),
        };
        assert!(!model.matches("keywords", false));
        assert!(!model.matches("keyw", false));
        assert!(model.matches("keywords", true));
        assert!(model.matches("keyw", true));
        assert_eq!(
            model.matches("keywords", true),
            capability_badges(&model.id, true)
                .iter()
                .any(|(_, label)| *label == "Keywords")
        );
        assert!(native("openai::gpt-transcribe").matches("cont temp", false));
        assert!(native("deepgram::nova-3").matches("for punct num", false));
        assert!(native("elevenlabs::scribe_v2").matches("cle", false));
    }

    #[test]
    fn disabled_profile_options_do_not_remove_model_capabilities_from_search() {
        use crate::providers::{ModelOptions, options};
        let id = "deepgram::nova-3";
        let mut config = Config::default();
        config.transcription.model_options.insert(
            id.into(),
            ModelOptions {
                streaming: false,
                smart_format: false,
                punctuate: false,
                numerals: false,
                ..Default::default()
            },
        );
        let saved = options(&config, id);
        assert!(!saved.streaming && !saved.smart_format && !saved.punctuate && !saved.numerals);
        assert!(native(id).matches(
            "deepgram streaming keywords formatting punctuation numerals",
            false
        ));
    }

    #[test]
    fn catalog_entries_split_provider_and_sort() {
        let models = parse(
            br#"{"data":[
                {"id":"openai/whisper-large-v3-turbo","name":"OpenAI: Whisper Large V3 Turbo"},
                {"id":"deepgram/nova-3","name":"Deepgram: Nova-3"},
                {"id":"acme/raw-id","name":""},
                {"id":"deepgram/nova-3","name":"Deepgram: Nova-3"},
                {"id":"  ","name":"blank"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            models,
            [
                CatalogModel {
                    id: "acme/raw-id".into(),
                    name: "raw-id".into(),
                    provider: "OpenRouter · acme".into(),
                },
                CatalogModel {
                    id: "deepgram/nova-3".into(),
                    name: "Nova-3".into(),
                    provider: "OpenRouter · Deepgram".into(),
                },
                CatalogModel {
                    id: "openai/whisper-large-v3-turbo".into(),
                    name: "Whisper Large V3 Turbo".into(),
                    provider: "OpenRouter · OpenAI".into(),
                },
            ]
        );
    }

    #[test]
    fn search_matches_every_word_across_id_name_and_provider() {
        let model = CatalogModel {
            id: "openai/whisper-large-v3-turbo".into(),
            name: "Whisper Large V3 Turbo".into(),
            provider: "OpenRouter · OpenAI".into(),
        };
        assert!(model.matches("", false));
        assert!(model.matches("openai turbo", false));
        assert!(model.matches("WHISPER", false));
        assert!(!model.matches("whisper nova", false));
    }

    #[test]
    fn labels_fall_back_to_the_id() {
        let catalog = parse(br#"{"data":[{"id":"a/b","name":"A: Bee"}]}"#).unwrap();
        assert_eq!(label("a/b", &catalog), "Bee");
        assert_eq!(label("c/d", &catalog), "c/d");
    }

    #[test]
    fn unexpected_bodies_are_errors() {
        assert!(parse(b"<html>").is_err());
    }

    #[test]
    fn all_providers_are_available_offline_and_remote_names_replace_defaults() {
        let offline = available_catalog(&[]);
        for provider in crate::providers::Provider::ALL {
            assert!(
                offline
                    .iter()
                    .any(|model| crate::providers::ModelRef::parse(&model.id).provider == provider)
            );
        }
        let id = Config::default().transcription.models[0].clone();
        let remote = CatalogModel {
            id: id.clone(),
            name: "Remote model name".into(),
            provider: "OpenRouter".into(),
        };
        let merged = available_catalog(std::slice::from_ref(&remote));
        assert_eq!(merged.iter().filter(|model| model.id == id).count(), 1);
        assert!(merged.contains(&remote));
        assert_eq!(provider_label("openai/gpt-transcribe"), "OpenRouter");
        assert_eq!(provider_label("openai::gpt-transcribe"), "OpenAI");
        assert!(
            capability_badges("deepgram::nova-3", false)
                .iter()
                .any(|(_, label)| *label == "Streaming")
        );
    }
}
