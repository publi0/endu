//! Muse Voice's shared file/live configuration. No credentials in file requests.
//! https://dev.meta.ai/docs/api-reference/voice/schemas

use color_eyre::Result;
use color_eyre::eyre::bail;
use serde_json::{Value, json};

use super::ModelOptions;

pub(super) const MODEL: &str = "muse-voice-transcribe-1.0";

/// Meta requires language names, not ISO codes. Never substitute an unsupported
/// explicit hint with another language; Auto deliberately omits the field.
pub(super) fn language_bias(language: &str) -> Result<Option<&'static str>> {
    Ok(Some(match language.trim() {
        "" | "auto" => return Ok(None),
        "ar" => "Arabic",
        "nl" => "Dutch",
        "en" => "English",
        "fr" => "French",
        "de" => "German",
        "hi" => "Hindi",
        "id" => "Indonesian",
        "it" => "Italian",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Mandarin Chinese",
        "pl" => "Polish",
        "pt" => "Portuguese",
        "es" => "Spanish",
        "tr" => "Turkish",
        "vi" => "Vietnamese",
        _ => bail!(
            "Muse Voice does not support this language hint. Choose Auto or a supported language."
        ),
    }))
}

pub(super) fn settings(
    model: &str,
    options: &ModelOptions,
    encoding: &str,
    keywords: &[String],
) -> Result<Value> {
    if model != MODEL {
        bail!("This Meta model is not supported for dictation.");
    }
    let mut body = json!({
        "model": model,
        "audioEncoding": encoding,
        "mode": "PUSH_TO_TALK",
        "partialMode": "CUMULATIVE",
        "emitAudioProgress": false
    });
    if let Some(language) = language_bias(&options.language)? {
        body["languageBias"] = json!([language]);
    }
    if !keywords.is_empty() {
        body["keywords"] = json!(keywords);
    }
    Ok(body)
}

pub(super) fn handshake(
    model: &str,
    options: &ModelOptions,
    keywords: &[String],
    key: &str,
) -> Result<Value> {
    let mut body = settings(model, options, "PCM_16KHZ", keywords)?;
    body["authorization"] = json!({"accessToken": format!("Bearer {key}")});
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_uses_named_languages_and_never_silently_changes_an_explicit_hint() {
        assert_eq!(language_bias("pt").unwrap(), Some("Portuguese"));
        assert_eq!(language_bias("zh").unwrap(), Some("Mandarin Chinese"));
        for auto in ["auto", "", " auto "] {
            assert_eq!(language_bias(auto).unwrap(), None);
        }
        for unsupported in ["ru", "uk", "sv", "da", "fi", "cs", "el", "ro", "hu"] {
            assert!(language_bias(unsupported).is_err());
        }
    }

    #[test]
    fn meta_only_advertises_and_sends_documented_transcription_options() {
        let options = ModelOptions {
            language: "pt".into(),
            prompt: "PRIVATE_CONTEXT".into(),
            temperature: Some(0.8),
            ..ModelOptions::default()
        };
        let body = settings(MODEL, &options, "WAV", &["Nimbus Files".into()]).unwrap();
        assert_eq!(body["languageBias"], json!(["Portuguese"]));
        assert_eq!(body["keywords"], json!(["Nimbus Files"]));
        assert_eq!(body["mode"], "PUSH_TO_TALK");
        assert!(!body.to_string().contains("PRIVATE_CONTEXT"));
        assert!(body.get("temperature").is_none());
        assert!(body.get("authorization").is_none());
        let auto = settings(MODEL, &ModelOptions::default(), "PCM_16KHZ", &[]).unwrap();
        assert!(auto.get("languageBias").is_none());
        assert!(auto.get("keywords").is_none());
        assert!(settings("muse-spark", &options, "WAV", &[]).is_err());
    }
}
