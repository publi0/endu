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
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || query.split_whitespace().all(|word| {
                self.id.to_lowercase().contains(word)
                    || self.name.to_lowercase().contains(word)
                    || self.provider.to_lowercase().contains(word)
            })
    }
}

/// Fetches the models that output transcriptions. The endpoint is public, so
/// a missing key is not an error here.
pub fn fetch(config: &Config) -> Result<Vec<CatalogModel>> {
    let key = super::api_key(config).unwrap_or_default();
    let response = http::get(
        &config.endpoint("models?output_modalities=transcription"),
        &key,
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
                provider,
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
        .map_or_else(|| id.to_owned(), |model| model.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    provider: "acme".into(),
                },
                CatalogModel {
                    id: "deepgram/nova-3".into(),
                    name: "Nova-3".into(),
                    provider: "Deepgram".into(),
                },
                CatalogModel {
                    id: "openai/whisper-large-v3-turbo".into(),
                    name: "Whisper Large V3 Turbo".into(),
                    provider: "OpenAI".into(),
                },
            ]
        );
    }

    #[test]
    fn search_matches_every_word_across_id_name_and_provider() {
        let model = CatalogModel {
            id: "openai/whisper-large-v3-turbo".into(),
            name: "Whisper Large V3 Turbo".into(),
            provider: "OpenAI".into(),
        };
        assert!(model.matches(""));
        assert!(model.matches("openai turbo"));
        assert!(model.matches("WHISPER"));
        assert!(!model.matches("whisper nova"));
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
}
