//! One bounded, ordered transcription chain across direct providers and OpenRouter.
//! Requests, credentials, vocabulary and remote bodies never become diagnostics.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{bail, eyre};
use serde_json::{Value, json};

use super::{ModelOptions, ModelRef, Provider, keys, streaming};
use crate::openrouter::http::Response;
use crate::openrouter::report::{AudioTrim, ExecutionReport, StepReport};
use crate::openrouter::stats::{
    self, AttemptSample, DictationTelemetry, ErrorKind, Failure, ModelLatency, RequestMode, Sample,
};
use crate::openrouter::transcribe::{
    ChainFailure, SAMPLE_RATE, Transcription, Usage, chunk_ranges, encode_base64, encode_wav,
};
use crate::openrouter::vad::{self, Trimmed};
use crate::openrouter::vocabulary_support::HintPlan;
use crate::openrouter::{AUTO_LANGUAGE, Config};
use crate::vocabulary::Snapshot;

const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 10 * 1024 * 1024;
const LEGACY_PROMPT_HINT_BYTES: usize = 200;
static BOUNDARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The provider-specific view of the shared dictionary, always retaining whole terms.
pub fn keywords(model: ModelRef<'_>, vocabulary: &Snapshot) -> Vec<String> {
    if !vocabulary.settings().remote_hints || !model.capabilities().keywords {
        return Vec::new();
    }
    let (count, bytes, chars, words) = match model.provider {
        Provider::OpenAi if model.model == "whisper-1" || model.model.starts_with("gpt-4o-") => {
            (50, LEGACY_PROMPT_HINT_BYTES, usize::MAX, usize::MAX)
        }
        Provider::OpenAi => (50, 400, usize::MAX, usize::MAX),
        // A UTF-8 byte budget is conservative for Deepgram's 500-token limit.
        Provider::Deepgram => (100, 400, usize::MAX, usize::MAX),
        Provider::ElevenLabs if model.model.ends_with("_realtime") => (50, 4_000, 20, 5),
        Provider::ElevenLabs => (1_000, 128_000, 49, 5),
        Provider::Microsoft => (2_000, 256_000, usize::MAX, usize::MAX),
        Provider::Grok => (100, 128_000, 50, usize::MAX),
        Provider::Google => (1_000, 128_000, usize::MAX, usize::MAX),
        // Meta publishes no term cap. Keep Hex's shared dictionary bounds;
        // retry without hints on an explicit server-side keyword rejection.
        Provider::Meta => (
            crate::vocabulary::MAX_TERMS,
            256_000,
            usize::MAX,
            usize::MAX,
        ),
        Provider::OpenRouter => return Vec::new(), // Routing-specific hints use HintPlan.
    };
    let mut used = 0;
    vocabulary
        .settings()
        .terms
        .iter()
        .filter(|term| {
            term.chars().count() <= chars
                && term
                    .split(|character: char| !character.is_alphanumeric())
                    .filter(|word| !word.is_empty())
                    .count()
                    <= words
        })
        .filter(|term| {
            !term
                .chars()
                .any(|ch| matches!(ch, '<' | '>' | '{' | '}' | '[' | ']' | '\\' | '\r' | '\n'))
        })
        .take(count)
        .take_while(|term| {
            used += term.len() + 1;
            used <= bytes
        })
        .cloned()
        .collect()
}

pub fn pcm16(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| {
            let sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            ((sample * f32::from(i16::MAX)).round() as i16).to_le_bytes()
        })
        .collect()
}

/// A request without its key. Deliberately has no Debug implementation.
struct Request {
    provider: Provider,
    url: String,
    content_type: String,
    body: Vec<u8>,
    keyword_count: usize,
}

fn language(options: &ModelOptions) -> Option<&str> {
    let language = options.language.trim();
    (!language.is_empty() && language != AUTO_LANGUAGE).then_some(language)
}

pub(crate) fn grok_format_supported(language: &str) -> bool {
    matches!(
        language,
        "ar" | "de" | "en" | "es" | "fr" | "ja" | "pt" | "ru" | "sv" | "vi" | "zh"
    )
}

fn prompt_with_terms(context: &str, terms: &[String]) -> (String, usize) {
    if terms.is_empty() {
        return (context.to_owned(), 0);
    }
    let mut prompt = if context.is_empty() {
        "Vocabulary: ".to_owned()
    } else {
        format!("{context}\n\nVocabulary: ")
    };
    let mut count = 0;
    for term in terms {
        let separator = if count == 0 { "" } else { ", " };
        if prompt.len() + separator.len() + term.len() > LEGACY_PROMPT_HINT_BYTES {
            break;
        }
        prompt.push_str(separator);
        prompt.push_str(term);
        count += 1;
    }
    if count == 0 {
        (context.to_owned(), 0)
    } else {
        (prompt, count)
    }
}

fn request(
    config: &Config,
    model: ModelRef<'_>,
    options: &ModelOptions,
    wav: &[u8],
    vocabulary: &Snapshot,
    hints: &HintPlan,
    without_hints: bool,
) -> Result<Request> {
    let terms = if without_hints {
        Vec::new()
    } else {
        keywords(model, vocabulary)
    };
    match model.provider {
        Provider::OpenRouter => {
            let mut body = json!({ "model": model.model, "input_audio": {"data": encode_base64(wav), "format": "wav"} });
            if let Some(language) = language(options) {
                body["language"] = json!(language);
            }
            if let Some(temperature) = options.temperature {
                body["temperature"] = json!(temperature);
            }
            let mut count = 0;
            if !without_hints && let Some(provider) = hints.for_model(model.model) {
                body["provider"] = provider.clone();
                count = hint_count(provider);
            }
            Ok(Request {
                provider: model.provider,
                url: config.endpoint("audio/transcriptions"),
                content_type: "application/json".into(),
                body: body.to_string().into_bytes(),
                keyword_count: count,
            })
        }
        Provider::OpenAi => {
            let mut fields = vec![
                ("model", model.model.to_owned()),
                ("response_format", "json".into()),
            ];
            if let Some(language) = language(options) {
                fields.push((
                    if model.model == "gpt-transcribe" {
                        "languages[]"
                    } else {
                        "language"
                    },
                    language.to_owned(),
                ));
            }
            let count = if model.model == "gpt-transcribe" {
                fields.extend(terms.iter().cloned().map(|term| ("keywords[]", term)));
                if !options.prompt.is_empty() {
                    fields.push(("prompt", options.prompt.clone()));
                }
                terms.len()
            } else {
                let (prompt, added) = prompt_with_terms(&options.prompt, &terms);
                if !prompt.is_empty() {
                    fields.push(("prompt", prompt));
                }
                added
            };
            if model.capabilities().temperature
                && let Some(temperature) = options.temperature
            {
                fields.push(("temperature", temperature.to_string()));
            }
            let (content_type, body) = multipart(&fields, wav);
            Ok(Request {
                provider: model.provider,
                url: "https://api.openai.com/v1/audio/transcriptions".into(),
                content_type,
                body,
                keyword_count: count,
            })
        }
        Provider::Deepgram => {
            let mut url =
                url::Url::parse("https://api.deepgram.com/v1/listen").expect("constant endpoint");
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("model", model.model);
                if let Some(language) = language(options) {
                    query.append_pair("language", language);
                } else {
                    query.append_pair("detect_language", "true");
                }
                query.append_pair("smart_format", bool_text(options.smart_format));
                query.append_pair("punctuate", bool_text(options.punctuate));
                query.append_pair("numerals", bool_text(options.numerals));
                for term in &terms {
                    query.append_pair("keyterm", term);
                }
            }
            Ok(Request {
                provider: model.provider,
                url: url.into(),
                content_type: "audio/wav".into(),
                body: wav.to_vec(),
                keyword_count: terms.len(),
            })
        }
        Provider::ElevenLabs => {
            let mut fields = vec![
                ("model_id", model.model.to_owned()),
                ("tag_audio_events", "false".into()),
                ("diarize", "false".into()),
                ("no_verbatim", bool_text(options.no_verbatim).into()),
            ];
            if let Some(language) = language(options) {
                fields.push(("language_code", language.to_owned()));
            }
            fields.extend(terms.iter().cloned().map(|term| ("keyterms", term)));
            let (content_type, body) = multipart(&fields, wav);
            Ok(Request {
                provider: model.provider,
                url: "https://api.elevenlabs.io/v1/speech-to-text".into(),
                content_type,
                body,
                keyword_count: terms.len(),
            })
        }
        Provider::Microsoft => {
            // https://learn.microsoft.com/azure/ai-services/speech-service/mai-transcribe
            // Follow the current REST example, including the explicit enhanced-mode switch.
            let mut definition = json!({"enhancedMode": {
                "enabled": true,
                "model": "MAI-Transcribe-2",
                "modelOptions": {"transcribeStyle": if options.no_verbatim { "clean" } else { "verbatim" }}
            }});
            if let Some(locale) = super::bcp47_language(&options.language) {
                definition["locales"] = json!([locale]);
            }
            if !terms.is_empty() {
                definition["phraseList"] = json!({"phrases": terms});
            }
            let mut url = super::microsoft_endpoint(config, false)
                .map_err(|_| eyre!("Configure a valid Microsoft Speech endpoint in Providers."))?;
            url.set_path("/speechtotext/transcriptions:transcribe");
            url.set_query(Some("api-version=2025-10-15"));
            let (content_type, body) =
                multipart_audio(&[("definition", definition.to_string())], wav, "audio");
            Ok(Request {
                provider: model.provider,
                url: url.into(),
                content_type,
                body,
                keyword_count: terms.len(),
            })
        }
        Provider::Grok => {
            // https://docs.x.ai/developers/model-capabilities/audio/speech-to-text
            let mut fields = vec![
                ("model", model.model.to_owned()),
                (
                    "format",
                    bool_text(options.smart_format && grok_format_supported(&options.language))
                        .into(),
                ),
                ("filler_words", bool_text(!options.no_verbatim).into()),
            ];
            if let Some(language) = language(options) {
                fields.push(("language", language.into()));
            }
            fields.extend(terms.iter().cloned().map(|term| ("keyterm", term)));
            // xAI ignores options placed after the audio: multipart keeps file last.
            let (content_type, body) = multipart(&fields, wav);
            Ok(Request {
                provider: model.provider,
                url: "https://api.x.ai/v1/stt".into(),
                content_type,
                body,
                keyword_count: terms.len(),
            })
        }
        Provider::Meta => {
            let settings = super::meta::settings(model.model, options, "WAV", &terms)?;
            let (content_type, body) = multipart_audio_typed(
                &[("request", settings.to_string())],
                wav,
                "audio",
                Some("application/json"),
            );
            Ok(Request {
                provider: model.provider,
                url: "https://api.meta.ai/v1/asr/transcribe".into(),
                content_type,
                body,
                keyword_count: terms.len(),
            })
        }
        Provider::Google => {
            // https://ai.google.dev/gemini-api/docs/transcribe and /api/interactions-api
            let mut transcription = json!({"mode": if options.smart_format {
                json!("smart")
            } else {
                json!({"type":"verbatim"})
            }});
            if let Some(locale) = super::bcp47_language(&options.language) {
                transcription["language_codes"] = json!([locale]);
                if options.language == "zh" {
                    transcription["language_codes"] = json!(["cmn-Hans-CN"]);
                }
            }
            if !terms.is_empty() {
                transcription["custom_vocabulary"] = json!(terms);
            }
            let body = json!({
                "model": model.model,
                "store": false,
                "input": [{"type":"audio", "data":encode_base64(wav), "mime_type":"audio/wav"}],
                "generation_config": {"transcription_config":transcription}
            });
            Ok(Request {
                provider: model.provider,
                url: "https://generativelanguage.googleapis.com/v1beta/interactions".into(),
                content_type: "application/json".into(),
                body: body.to_string().into_bytes(),
                keyword_count: terms.len(),
            })
        }
    }
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn multipart(fields: &[(&str, String)], wav: &[u8]) -> (String, Vec<u8>) {
    multipart_audio(fields, wav, "file")
}

fn multipart_audio(fields: &[(&str, String)], wav: &[u8], audio_field: &str) -> (String, Vec<u8>) {
    multipart_audio_typed(fields, wav, audio_field, None)
}

fn multipart_audio_typed(
    fields: &[(&str, String)],
    wav: &[u8],
    audio_field: &str,
    field_type: Option<&str>,
) -> (String, Vec<u8>) {
    let boundary = loop {
        let value = format!(
            "hex-audio-{}-{}",
            std::process::id(),
            BOUNDARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        if !fields.iter().any(|(_, field)| field.contains(&value))
            && !wav
                .windows(value.len())
                .any(|bytes| bytes == value.as_bytes())
        {
            break value;
        }
    };
    let mut body = Vec::new();
    for (name, value) in fields {
        let content_type =
            field_type.map_or(String::new(), |kind| format!("Content-Type: {kind}\r\n"));
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n{content_type}\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{audio_field}\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes());
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn hint_count(provider: &Value) -> usize {
    provider
        .get("options")
        .and_then(Value::as_object)
        .map_or(0, |routes| {
            routes
                .values()
                .map(|options| {
                    options
                        .pointer("/phraseList/phrases")
                        .or_else(|| options.get("keywords"))
                        .or_else(|| options.get("keyterm"))
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .or_else(|| {
                            options.get("prompt").and_then(Value::as_str).map(|text| {
                                if text.is_empty() {
                                    0
                                } else {
                                    text.split(", ").count()
                                }
                            })
                        })
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
        })
}

#[derive(Clone, Copy, Debug)]
struct TransportFailure(ErrorKind, usize);

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.0 {
            ErrorKind::Timeout => "transcription request timed out",
            ErrorKind::Auth => "the provider key is missing or invalid",
            ErrorKind::Rejected => "the provider URL is invalid or unsafe",
            ErrorKind::InvalidResponse => "transcription response was too large or unreadable",
            _ => "transcription network request failed",
        })
    }
}
impl std::error::Error for TransportFailure {}

fn http_error_kind(error: &ureq::Error, reading_body: bool) -> ErrorKind {
    match error {
        ureq::Error::Timeout(_) => ErrorKind::Timeout,
        ureq::Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            ErrorKind::Timeout
        }
        _ if reading_body => ErrorKind::InvalidResponse,
        _ => ErrorKind::Network,
    }
}

fn http_body_failure(error: &ureq::Error, keyword_count: usize) -> TransportFailure {
    // Response headers have already arrived, so the request and its hints were
    // sent even if the body stalls or exceeds the configured response bound.
    TransportFailure(http_error_kind(error, true), keyword_count)
}

fn send(request: &Request, config: &Config, timeout: Duration) -> Result<Response> {
    crate::openrouter::http::validate_url(&request.url)
        .map_err(|_| TransportFailure(ErrorKind::Rejected, 0))?;
    if cfg!(test) {
        bail!("HTTP transcription is disabled in unit tests; inject a transport.");
    }
    if request.body.len() > MAX_REQUEST_BYTES {
        return Err(TransportFailure(ErrorKind::Rejected, 0).into());
    }
    let key = keys::api_key(request.provider, config)
        .map_err(|_| TransportFailure(ErrorKind::Auth, 0))?;
    let (header, authorization) = keys::authorization(request.provider, &key);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            // Retain OpenRouter-compatible HTTP localhost endpoints; direct provider
            // URLs are fixed HTTPS constants and never configurable through a profile.
            .https_only(request.provider != Provider::OpenRouter)
            .timeout_global(Some(timeout))
            .build(),
    );
    let mut response = agent
        .post(&request.url)
        .header(header, authorization)
        .header("Content-Type", request.content_type.as_str())
        .header("X-Title", "Hex")
        .send(request.body.as_slice())
        .map_err(|error| TransportFailure(http_error_kind(&error, false), 0))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && (0.0..=86_400.0).contains(value))
        .map(Duration::from_secs_f64);
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|error| http_body_failure(&error, request.keyword_count))?;
    Ok(Response {
        status,
        retry_after,
        body,
    })
}

fn reported_cost(value: &Value) -> Option<f64> {
    value
        .pointer("/usage/cost")
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}
fn response_cost(body: &[u8]) -> Option<f64> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .as_ref()
        .and_then(reported_cost)
}

fn parse(provider: Provider, body: &[u8]) -> Result<(String, Usage)> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| eyre!("The provider returned invalid transcription JSON."))?;
    if value.get("error").is_some_and(|error| !error.is_null()) || value.get("err_code").is_some() {
        bail!("The provider returned a transcription error.");
    }
    let text = if provider == Provider::Google {
        if value.get("status").and_then(Value::as_str) != Some("completed") {
            bail!("The provider did not complete the transcription.");
        }
        // output_text is an SDK convenience property. REST returns typed steps;
        // never include thoughts, tools or echoed user input in a dictation.
        value
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|step| step.get("type").and_then(Value::as_str) == Some("model_output"))
            .filter_map(|step| step.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|content| content.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|content| content.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        match provider {
            Provider::Deepgram => value.pointer("/results/channels/0/alternatives/0/transcript"),
            Provider::Microsoft => value.pointer("/combinedPhrases/0/text"),
            Provider::Meta => value.get("transcript"),
            _ => value.get("text"),
        }
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("The provider returned no transcription text."))?
        .to_owned()
    };
    if text.trim().is_empty() {
        bail!("The provider returned empty transcription text.");
    }
    let usage = value.get("usage");
    let cost = reported_cost(&value);
    Ok((
        text.trim().to_owned(),
        Usage {
            tokens: usage
                .and_then(|usage| usage.get("total_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            cost_usd: cost.unwrap_or_default(),
            cost_reported: cost.is_some(),
        },
    ))
}

fn keywords_rejected(
    model: ModelRef<'_>,
    request: &Request,
    response: &Response,
    hints: &HintPlan,
) -> bool {
    if request.keyword_count == 0 || !matches!(response.status, 400 | 422) {
        return false;
    }
    if model.provider == Provider::OpenRouter {
        return hints.rejected(model.model, response);
    }
    let body = String::from_utf8_lossy(&response.body).to_ascii_lowercase();
    match model.provider {
        Provider::OpenAi if model.model == "gpt-transcribe" => body.contains("keywords"),
        Provider::OpenAi => body.contains("prompt"),
        Provider::Deepgram | Provider::ElevenLabs | Provider::Grok => body.contains("keyterm"),
        Provider::Microsoft => body.contains("phraselist") || body.contains("phrases"),
        Provider::Google => {
            body.contains("custom_vocabulary")
                || body.contains("customvocabulary")
                || body.contains("custom vocabulary")
        }
        Provider::Meta => body.contains("keywords"),
        Provider::OpenRouter => false,
    }
}

struct Success {
    text: String,
    model: String,
    usage: Usage,
    latency_ms: u64,
}

#[derive(Default)]
struct Progress {
    failures: Vec<Failure>,
    executions: Vec<ExecutionReport>,
    telemetry: DictationTelemetry,
}

impl Progress {
    fn execution(
        &mut self,
        model: ModelRef<'_>,
        streaming: bool,
        keyword_count: usize,
        success: bool,
        cost_usd: Option<f64>,
    ) {
        self.executions.push(ExecutionReport {
            provider: model.provider.id().into(),
            model: model.model.into(),
            streaming,
            keyword_count,
            outcome: if success { "success" } else { "failed" }.into(),
            cost_usd,
        });
    }
    fn attempt(
        &mut self,
        model: ModelRef<'_>,
        mode: RequestMode,
        keyword_count: usize,
        outcome: std::result::Result<u64, ErrorKind>,
        cost_usd: Option<f64>,
        retried: bool,
    ) {
        self.telemetry.retried |= retried;
        self.telemetry.attempts.push(AttemptSample {
            model: model.key(),
            mode,
            success: outcome.is_ok(),
            error: outcome.err(),
            latency_ms: outcome.ok(),
            keyword_count,
            cost_usd,
        });
    }
    fn telemetry(&self, dictation_succeeded: bool) -> DictationTelemetry {
        let mut telemetry = self.telemetry.clone();
        if dictation_succeeded {
            telemetry.live_recovered = telemetry
                .attempts
                .iter()
                .any(|attempt| attempt.mode == RequestMode::Live && !attempt.success)
                && telemetry
                    .attempts
                    .iter()
                    .any(|attempt| attempt.mode == RequestMode::Recorded && attempt.success);
        } else {
            // Successful requests remain visible even if a later chunk fails,
            // but only a complete dictation can be counted as a rescued result.
            telemetry.used_fallback = false;
            telemetry.live_recovered = false;
        }
        telemetry
    }
    fn fail(&mut self, id: &str, kind: ErrorKind, detail: &str) {
        // Details are Hex's own sanitized wording (status codes, timeouts), the same
        // text a failed dictation already logs. Keep every attempt, including ones a
        // later fallback recovered, so intermittent provider failures stay diagnosable.
        tracing::warn!(model = id, kind = ?kind, "{detail}");
        self.failures.push(Failure {
            model: id.to_owned(),
            kind,
            detail: format!("{id}: {detail}"),
        });
    }
}

type RequestSender<'a> = dyn FnMut(&Request, Duration) -> (Result<Response>, Duration) + 'a;
type CompletedSender<'a> =
    dyn FnMut(&str, &[f32], &Snapshot, Duration) -> Result<streaming::LiveResult> + 'a;

struct Chain<'a> {
    config: &'a Config,
    vocabulary: &'a Snapshot,
    hints: &'a HintPlan,
    cancelled: Option<&'a AtomicBool>,
}

impl Chain<'_> {
    fn run(
        &self,
        samples: &[f32],
        initial_spent: Duration,
        progress: &mut Progress,
        send: &mut RequestSender<'_>,
        completed: &mut CompletedSender<'_>,
        sleep: &mut dyn FnMut(Duration),
    ) -> Result<Success> {
        let started = Instant::now();
        let mut spent = initial_spent;
        let wav =
            encode_wav(samples).map_err(|_| eyre!("Could not encode transcription audio."))?;
        for (model_index, id) in self
            .config
            .transcription
            .models
            .iter()
            .enumerate()
            .map(|(index, id)| (index, id.trim()))
            .filter(|(_, id)| !id.is_empty())
        {
            let model = ModelRef::parse(id);
            let capability = model.capabilities();
            let options = super::options(self.config, id);
            if options.validate().is_err() {
                progress.fail(id, ErrorKind::Rejected, "invalid model options");
                continue;
            }
            if !(capability.batch || capability.streaming && options.streaming) {
                progress.fail(
                    id,
                    ErrorKind::Rejected,
                    "selected model has no enabled transcription transport",
                );
                continue;
            }
            let mut rate_retried = false;
            let mut without_hints = false;
            loop {
                check_cancelled(self.cancelled)?;
                let elapsed = initial_spent.saturating_add(started.elapsed()).max(spent);
                let remaining = self.config.total_timeout().saturating_sub(elapsed);
                if remaining.is_zero() {
                    progress.fail(id, ErrorKind::Timeout, "not tried, chain deadline reached");
                    break;
                }
                let timeout = self.config.attempt_timeout().min(remaining);
                if !capability.batch {
                    let at = Instant::now();
                    let mut without_remote = self.vocabulary.settings().clone();
                    without_remote.remote_hints = false;
                    let without_remote = Snapshot::new(without_remote);
                    let vocabulary = if without_hints {
                        &without_remote
                    } else {
                        self.vocabulary
                    };
                    let result = completed(id, samples, vocabulary, timeout);
                    spent = spent.saturating_add(at.elapsed());
                    check_cancelled(self.cancelled)?;
                    match result {
                        Ok(result) if !result.text.trim().is_empty() => {
                            progress.attempt(
                                model,
                                RequestMode::Recorded,
                                result.keyword_count,
                                Ok(result.latency_ms),
                                None,
                                rate_retried || without_hints,
                            );
                            progress.telemetry.used_fallback |= model_index > 0;
                            progress.execution(model, false, result.keyword_count, true, None);
                            return Ok(Success {
                                text: result.text.trim().into(),
                                model: id.into(),
                                usage: Usage::default(),
                                latency_ms: result.latency_ms,
                            });
                        }
                        Ok(result) => {
                            progress.attempt(
                                model,
                                RequestMode::Recorded,
                                result.keyword_count,
                                Err(ErrorKind::InvalidResponse),
                                None,
                                rate_retried || without_hints,
                            );
                            progress.execution(model, false, result.keyword_count, false, None);
                            progress.fail(
                                id,
                                ErrorKind::InvalidResponse,
                                "empty transcription response",
                            );
                            break;
                        }
                        Err(error) => {
                            let live_error = error.downcast_ref::<streaming::LiveError>();
                            let status = live_error.and_then(|error| error.status);
                            if live_error.is_none_or(|error| error.attempted) {
                                let kind =
                                    live_error.map_or(ErrorKind::Network, |error| error.error_kind);
                                progress.attempt(
                                    model,
                                    RequestMode::Recorded,
                                    live_error.map_or(0, |error| error.keyword_count),
                                    Err(kind),
                                    None,
                                    rate_retried || without_hints,
                                );
                            }
                            progress.execution(
                                model,
                                false,
                                live_error.map_or(0, |error| error.keyword_count),
                                false,
                                None,
                            );
                            if !without_hints
                                && !keywords(model, self.vocabulary).is_empty()
                                && live_error.is_some_and(|error| error.keywords_rejected)
                            {
                                progress.fail(
                                    id,
                                    ErrorKind::Rejected,
                                    "vocabulary parameter rejected",
                                );
                                without_hints = true;
                                continue;
                            }
                            let available = self.config.total_timeout().saturating_sub(
                                initial_spent.saturating_add(started.elapsed()).max(spent),
                            );
                            if status == Some(429)
                                && !rate_retried
                                && available > Duration::from_secs(1)
                                && self.config.transcription.rate_limit_retry_max_wait_ms >= 1_000
                            {
                                sleep(Duration::from_secs(1));
                                spent = spent.saturating_add(Duration::from_secs(1));
                                rate_retried = true;
                                continue;
                            }
                            progress.fail(
                                id,
                                live_error.map_or(ErrorKind::Network, |error| error.error_kind),
                                &status.map_or_else(
                                    || "realtime transcription failed".into(),
                                    |status| {
                                        format!("HTTP {status}: realtime transcription failed")
                                    },
                                ),
                            );
                            break;
                        }
                    }
                }
                let request = match request(
                    self.config,
                    model,
                    &options,
                    &wav,
                    self.vocabulary,
                    self.hints,
                    without_hints,
                ) {
                    Ok(request) => request,
                    Err(_) => {
                        progress.execution(model, false, 0, false, None);
                        progress.fail(
                            id,
                            ErrorKind::Rejected,
                            "provider endpoint or model options are invalid",
                        );
                        break;
                    }
                };
                let (response, latency) = send(&request, timeout);
                spent = spent.saturating_add(latency);
                check_cancelled(self.cancelled)?;
                match response {
                    Ok(response) if response.is_success() => {
                        match parse(model.provider, &response.body) {
                            Ok((text, usage)) => {
                                let cost = usage.cost_reported.then_some(usage.cost_usd);
                                progress.attempt(
                                    model,
                                    RequestMode::Recorded,
                                    request.keyword_count,
                                    Ok(latency.as_millis() as u64),
                                    cost,
                                    rate_retried || without_hints,
                                );
                                progress.telemetry.used_fallback |= model_index > 0;
                                progress.execution(model, false, request.keyword_count, true, cost);
                                return Ok(Success {
                                    text,
                                    model: id.into(),
                                    usage,
                                    latency_ms: latency.as_millis() as u64,
                                });
                            }
                            Err(_) => {
                                let cost = response_cost(&response.body);
                                progress.attempt(
                                    model,
                                    RequestMode::Recorded,
                                    request.keyword_count,
                                    Err(ErrorKind::InvalidResponse),
                                    cost,
                                    rate_retried || without_hints,
                                );
                                progress.execution(
                                    model,
                                    false,
                                    request.keyword_count,
                                    false,
                                    cost,
                                );
                                progress.fail(
                                    id,
                                    ErrorKind::InvalidResponse,
                                    "invalid transcription response",
                                );
                            }
                        }
                    }
                    Ok(response) => {
                        let cost = response_cost(&response.body);
                        progress.attempt(
                            model,
                            RequestMode::Recorded,
                            request.keyword_count,
                            Err(ErrorKind::from_status(response.status)),
                            cost,
                            rate_retried || without_hints,
                        );
                        progress.execution(model, false, request.keyword_count, false, cost);
                        if response.status == 401 {
                            keys::invalidate(model.provider);
                        }
                        if !without_hints
                            && keywords_rejected(model, &request, &response, self.hints)
                        {
                            if model.provider == Provider::OpenRouter {
                                HintPlan::invalidate(self.config, model.model);
                            }
                            progress.fail(
                                id,
                                ErrorKind::from_status(response.status),
                                &format!("HTTP {}: vocabulary parameter rejected", response.status),
                            );
                            without_hints = true;
                            continue;
                        }
                        let wait = response.retry_after.unwrap_or(Duration::from_secs(1));
                        let available = self.config.total_timeout().saturating_sub(
                            initial_spent.saturating_add(started.elapsed()).max(spent),
                        );
                        if response.status == 429
                            && !rate_retried
                            && self.config.transcription.rate_limit_retry_max_wait_ms > 0
                            && wait
                                <= Duration::from_millis(
                                    self.config.transcription.rate_limit_retry_max_wait_ms,
                                )
                            && wait < available
                        {
                            sleep(wait);
                            spent = spent.saturating_add(wait);
                            rate_retried = true;
                            continue;
                        }
                        progress.fail(
                            id,
                            ErrorKind::from_status(response.status),
                            &format!("HTTP {}: transcription request failed", response.status),
                        );
                    }
                    Err(error) => {
                        // No untrusted error formatting: URLs may contain the vocabulary.
                        let kind = error
                            .downcast_ref::<TransportFailure>()
                            .map_or(ErrorKind::Network, |error| error.0);
                        let keyword_count = error
                            .downcast_ref::<TransportFailure>()
                            .map_or(0, |error| error.1);
                        // Rejected here means local URL/body preflight, not HTTP 4xx.
                        if kind != ErrorKind::Rejected {
                            progress.attempt(
                                model,
                                RequestMode::Recorded,
                                keyword_count,
                                Err(kind),
                                None,
                                rate_retried || without_hints,
                            );
                        }
                        progress.execution(model, false, keyword_count, false, None);
                        progress.fail(
                            id,
                            kind,
                            match kind {
                                ErrorKind::Timeout => "transcription request timed out",
                                ErrorKind::Auth => "the provider key is missing or invalid",
                                ErrorKind::Rejected => "the provider URL is invalid or unsafe",
                                _ => "transcription request could not complete",
                            },
                        );
                    }
                }
                break;
            }
        }
        Err(ChainFailure {
            failures: progress.failures.clone(),
        }
        .into())
    }
}

fn check_cancelled(cancelled: Option<&AtomicBool>) -> Result<()> {
    #[cfg(all(target_os = "macos", not(test)))]
    if crate::SHUTDOWN.load(Ordering::Acquire) {
        bail!("Transcription stopped during shutdown.");
    }
    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        bail!("Transcription was cancelled.");
    }
    Ok(())
}

fn duration_ms(samples: usize) -> u64 {
    samples as u64 * 1_000 / u64::from(SAMPLE_RATE)
}

#[cfg(test)]
thread_local! {
    static TEST_SAMPLES: std::cell::RefCell<Vec<Sample>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn record_sample(sample: Sample) {
    #[cfg(not(test))]
    stats::record(&sample);
    #[cfg(test)]
    TEST_SAMPLES.with(|samples| samples.borrow_mut().push(sample));
}

pub fn transcribe(
    samples: &[f32],
    vocabulary: &Snapshot,
    live: Option<streaming::PendingLive>,
) -> Result<Transcription> {
    if samples.is_empty() {
        return Ok(Transcription {
            text: String::new(),
            report: None,
        });
    }
    if cfg!(test) && live.is_none() {
        bail!("Use an injected chain or live fixture for unit transcription tests.");
    }
    let config = match &live {
        Some(live) => live.config.clone(),
        None => crate::openrouter::load_config()?,
    };
    let vocabulary = live
        .as_ref()
        .map_or_else(|| vocabulary.clone(), |live| live.vocabulary.clone());
    let cancelled = live.as_ref().map(streaming::PendingLive::cancellation_flag);
    transcribe_configured(
        samples,
        &vocabulary,
        live,
        &config,
        &mut |request, timeout| {
            let at = Instant::now();
            let result = send(request, &config, timeout);
            (result, at.elapsed())
        },
        &mut |id, samples, vocabulary, timeout| {
            streaming::transcribe_completed(
                &config,
                id,
                samples,
                vocabulary,
                timeout,
                cancelled.clone(),
            )
        },
        &mut |wait| {
            let at = Instant::now();
            while at.elapsed() < wait && check_cancelled(cancelled.as_deref()).is_ok() {
                std::thread::sleep(
                    wait.saturating_sub(at.elapsed())
                        .min(Duration::from_millis(50)),
                );
            }
        },
    )
}

fn transcribe_configured(
    samples: &[f32],
    vocabulary: &Snapshot,
    live: Option<streaming::PendingLive>,
    config: &Config,
    send: &mut RequestSender<'_>,
    completed: &mut CompletedSender<'_>,
    sleep: &mut dyn FnMut(Duration),
) -> Result<Transcription> {
    let cancelled = live.as_ref().map(streaming::PendingLive::cancellation_flag);
    check_cancelled(cancelled.as_deref())?;
    let recorded_ms = duration_ms(samples.len());
    let started = Instant::now();
    let mut progress = Progress::default();
    let mut model_latency = BTreeMap::<String, ModelLatency>::new();
    let mut models = Vec::<String>::new();
    let mut usage = Usage::default();
    let mut texts = Vec::new();
    let mut live_succeeded = false;
    let mut live_latency_ms = 0;
    let mut sent_ms = recorded_ms;
    // The final exact clip can be silent even if a speculative live provider
    // returned text. Never accept that hallucination or label already-sent PCM as 0.
    let trimmed = if config.transcription.trim_silence {
        Some(vad::trim(samples))
    } else {
        None
    };
    if matches!(trimmed, Some(Trimmed::Silent)) {
        let sent_ms = duration_ms(
            live.as_ref()
                .map_or(0, streaming::PendingLive::sent_samples),
        );
        drop(live);
        record_sample(Sample {
            skipped_silent: true,
            recorded_ms,
            sent_ms,
            telemetry: Some(DictationTelemetry::default()),
            ..Sample::default()
        });
        return Ok(Transcription {
            text: String::new(),
            report: None,
        });
    }
    if let Some(live) = live {
        let model_id = live.model_id().to_owned();
        let sent_samples = live.sent_samples_counter();
        match live.resolve(config.attempt_timeout().min(config.total_timeout())) {
            Ok(result) if !result.text.trim().is_empty() => {
                check_cancelled(cancelled.as_deref())?;
                sent_ms = duration_ms(sent_samples.load(Ordering::Acquire));
                progress.attempt(
                    ModelRef::parse(&result.model),
                    RequestMode::Live,
                    result.keyword_count,
                    Ok(result.latency_ms),
                    None,
                    false,
                );
                progress.execution(
                    ModelRef::parse(&result.model),
                    true,
                    result.keyword_count,
                    true,
                    None,
                );
                model_latency
                    .entry(result.model.clone())
                    .or_default()
                    .record(result.latency_ms);
                models.push(result.model);
                texts.push(result.text.trim().to_owned());
                live_succeeded = true;
                live_latency_ms = result.latency_ms;
            }
            Ok(result) => {
                check_cancelled(cancelled.as_deref())?;
                progress.attempt(
                    ModelRef::parse(&model_id),
                    RequestMode::Live,
                    result.keyword_count,
                    Err(ErrorKind::InvalidResponse),
                    None,
                    false,
                );
                progress.execution(
                    ModelRef::parse(&model_id),
                    true,
                    result.keyword_count,
                    false,
                    None,
                );
                tracing::warn!(
                    model = %model_id,
                    "live transcription returned no text; recovering from the recorded clip"
                );
                progress.fail(
                    &model_id,
                    ErrorKind::InvalidResponse,
                    "empty live transcription response; trying completed audio",
                );
            }
            Err(error) => {
                check_cancelled(cancelled.as_deref())?;
                let live_error = error.downcast_ref::<streaming::LiveError>();
                let status = live_error.and_then(|error| error.status);
                // LiveError text is Hex's own fixed wording plus a status code, never
                // provider payloads, so it is safe to keep for diagnosing fallbacks.
                tracing::warn!(
                    model = %model_id,
                    reason = %live_error.map_or_else(|| "unclassified".to_owned(), ToString::to_string),
                    "live transcription failed; recovering from the recorded clip"
                );
                if live_error.is_none_or(|error| error.attempted) {
                    progress.attempt(
                        ModelRef::parse(&model_id),
                        RequestMode::Live,
                        live_error.map_or(0, |error| error.keyword_count),
                        Err(live_error.map_or(ErrorKind::Network, |error| error.error_kind)),
                        None,
                        false,
                    );
                }
                progress.execution(
                    ModelRef::parse(&model_id),
                    true,
                    live_error.map_or(0, |error| error.keyword_count),
                    false,
                    None,
                );
                progress.fail(
                    &model_id,
                    live_error.map_or(ErrorKind::Network, |error| error.error_kind),
                    &status.map_or_else(
                        || "live transcription failed; trying completed audio".into(),
                        |status| format!("HTTP {status}: live transcription failed"),
                    ),
                );
            }
        }
    }
    if !live_succeeded {
        let samples = match &trimmed {
            Some(Trimmed::Speech(speech)) => speech.as_slice(),
            _ => samples,
        };
        sent_ms = duration_ms(samples.len());
        #[cfg(not(test))]
        if vocabulary.settings().remote_hints && !vocabulary.settings().terms.is_empty() {
            crate::openrouter::vocabulary_support::schedule(false);
        }
        let hints = if cfg!(test) {
            HintPlan::default()
        } else {
            HintPlan::cached(config, vocabulary)
        };
        let chain = Chain {
            config,
            vocabulary,
            hints: &hints,
            cancelled: cancelled.as_deref(),
        };
        let chunk_samples =
            config.transcription.chunk_seconds.clamp(10, 200) as usize * SAMPLE_RATE as usize;
        for (index, range) in chunk_ranges(samples, chunk_samples).into_iter().enumerate() {
            let initial_spent = if index == 0 {
                started.elapsed()
            } else {
                Duration::ZERO
            };
            let success = chain.run(
                &samples[range],
                initial_spent,
                &mut progress,
                send,
                completed,
                sleep,
            );
            match success {
                Ok(success) => {
                    usage.tokens = usage.tokens.saturating_add(success.usage.tokens);
                    // Each reported cost is finite/nonnegative; the sum can still
                    // overflow when a provider reports extreme values per chunk.
                    usage.cost_usd = (usage.cost_usd + success.usage.cost_usd).min(f64::MAX);
                    model_latency
                        .entry(success.model.clone())
                        .or_default()
                        .record(success.latency_ms);
                    if !models.contains(&success.model) {
                        models.push(success.model);
                    }
                    if !success.text.is_empty() {
                        texts.push(success.text);
                    }
                }
                Err(error) => {
                    check_cancelled(cancelled.as_deref())?;
                    record_sample(Sample {
                        recorded_ms,
                        sent_ms,
                        latency_ms: started.elapsed().as_millis() as u64,
                        tokens: usage.tokens,
                        cost_usd: usage.cost_usd,
                        model_latency,
                        telemetry: Some(progress.telemetry(false)),
                        failures: progress.failures,
                        ..Sample::default()
                    });
                    return Err(error);
                }
            }
        }
    }
    let text = texts.join(" ");
    let latency_ms = if live_succeeded {
        live_latency_ms
    } else {
        started.elapsed().as_millis() as u64
    };
    let model = models.join(", ");
    record_sample(Sample {
        words: Some(stats::word_count(&text)),
        models,
        model_latency,
        recorded_ms,
        sent_ms,
        latency_ms,
        tokens: usage.tokens,
        cost_usd: usage.cost_usd,
        failures: progress.failures.clone(),
        telemetry: Some(progress.telemetry(true)),
        skipped_silent: false,
    });
    let mut failed = Vec::new();
    for failure in progress.failures {
        if !failed.contains(&failure.model) {
            failed.push(failure.model);
        }
    }
    Ok(Transcription {
        text,
        report: Some(StepReport {
            model: Some(model),
            latency_ms,
            failed,
            audio: Some(AudioTrim {
                recorded_ms,
                sent_ms,
            }),
            executions: progress.executions,
            omitted_executions: 0,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocabulary::Vocabulary;

    #[test]
    fn meta_upload_uses_typed_request_part_named_language_and_authoritative_wav() {
        let id = "meta::muse-voice-transcribe-1.0";
        let model = ModelRef::parse(id);
        let config = config(&[id]);
        let words = vocabulary(&["Nimbus Files", "Álvaro"]);
        let wav = encode_wav(&[0.1; 1600]).unwrap();
        for without_hints in [false, true] {
            let request = request(
                &config,
                model,
                &ModelOptions {
                    language: "pt".into(),
                    ..Default::default()
                },
                &wav,
                &words,
                &HintPlan::default(),
                without_hints,
            )
            .unwrap();
            assert_eq!(request.url, "https://api.meta.ai/v1/asr/transcribe");
            let body = String::from_utf8_lossy(&request.body);
            let settings: Value = serde_json::from_str(
                body.split("name=\"request\"\r\nContent-Type: application/json\r\n\r\n")
                    .nth(1)
                    .unwrap()
                    .split("\r\n")
                    .next()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(settings["audioEncoding"], "WAV");
            assert_eq!(settings["mode"], "PUSH_TO_TALK");
            assert_eq!(settings["languageBias"], json!(["Portuguese"]));
            assert_eq!(settings.get("keywords").is_none(), without_hints);
            assert_eq!(request.keyword_count, if without_hints { 0 } else { 2 });
            assert!(settings.get("authorization").is_none());
            assert!(request.body.windows(wav.len()).any(|part| part == wav));
            assert!(
                body.contains("name=\"audio\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav")
            );
        }
        let (text, usage) = parse(
            Provider::Meta,
            br#"{"transcript":" full transcription ","turns":[]}"#,
        )
        .unwrap();
        assert_eq!(text, "full transcription");
        assert!(!usage.cost_reported);
        for body in [
            r#"{"text":"wrong field"}"#,
            r#"{"transcript":" "}"#,
            r#"{"error":{"message":"PRIVATE_MARKER"}}"#,
        ] {
            assert!(
                !parse(Provider::Meta, body.as_bytes())
                    .unwrap_err()
                    .to_string()
                    .contains("PRIVATE_MARKER")
            );
        }
    }

    #[test]
    fn meta_hint_retry_and_cross_provider_fallback_record_actual_attempts() {
        let ids = ["meta::muse-voice-transcribe-1.0", "openai::gpt-transcribe"];
        let config = config(&ids);
        let words = vocabulary(&["Nimbus Files"]);
        let mut progress = Progress::default();
        let mut calls = Vec::new();
        let success = Chain {
            config: &config,
            vocabulary: &words,
            hints: &HintPlan::default(),
            cancelled: None,
        }
        .run(
            &[0.1; 1600],
            Duration::ZERO,
            &mut progress,
            &mut |request, _| {
                calls.push((request.provider, request.keyword_count));
                let response = match (request.provider, request.keyword_count) {
                    (Provider::Meta, 1) => response(
                        400,
                        r#"{"error":{"param":"keywords","message":"unsupported PRIVATE_MARKER"}}"#,
                        None,
                    ),
                    (Provider::Meta, 0) => response(503, "PRIVATE_MARKER", None),
                    (Provider::OpenAi, 1) => response(200, r#"{"text":"done"}"#, None),
                    _ => panic!("unexpected attempt"),
                };
                (Ok(response), Duration::from_millis(10))
            },
            &mut |_, _, _, _| panic!("no completed websocket"),
            &mut |_| panic!("no sleep"),
        )
        .unwrap();
        assert_eq!(success.model, ids[1]);
        assert_eq!(
            calls,
            [
                (Provider::Meta, 1),
                (Provider::Meta, 0),
                (Provider::OpenAi, 1)
            ]
        );
        assert_eq!(progress.executions[0].provider, "meta");
        assert_eq!(progress.executions[0].keyword_count, 1);
        let telemetry = progress.telemetry(true);
        assert!(telemetry.used_fallback && telemetry.retried);
        assert_eq!(telemetry.attempts.len(), 3);
        assert_eq!(telemetry.attempts[0].mode, RequestMode::Recorded);
        assert!(
            progress
                .failures
                .iter()
                .all(|failure| !failure.detail.contains("PRIVATE_MARKER"))
        );
    }

    #[test]
    fn meta_live_success_and_file_recovery_keep_history_mode_and_cost_unknown() {
        for live_succeeds in [true, false] {
            take_samples();
            let id = "meta::muse-voice-transcribe-1.0";
            let mut config = config(&[id]);
            config.transcription.trim_silence = false;
            let words = vocabulary(&["Nimbus Files"]);
            let live_result = if live_succeeds {
                Ok(streaming::LiveResult {
                    text: "live result".into(),
                    model: id.into(),
                    keyword_count: 1,
                    latency_ms: 125,
                })
            } else {
                let mut error = streaming::fixture_error(None, false);
                error
                    .downcast_mut::<streaming::LiveError>()
                    .unwrap()
                    .keyword_count = 1;
                Err(error)
            };
            let live =
                streaming::PendingLive::fixture(config.clone(), words.clone(), live_result, 1600);
            let mut requests = 0;
            let result = transcribe_configured(
                &[0.1; 1600],
                &words,
                Some(live),
                &config,
                &mut |request, _| {
                    requests += 1;
                    assert_eq!(request.provider, Provider::Meta);
                    (
                        Ok(response(200, r#"{"transcript":"recovered result"}"#, None)),
                        Duration::from_millis(25),
                    )
                },
                &mut |_, _, _, _| panic!("Meta recovery uses upload"),
                &mut |_| panic!("no wait"),
            )
            .unwrap();
            assert_eq!(requests, usize::from(!live_succeeds));
            let report = result.report.unwrap();
            assert_eq!(report.executions[0].provider, "meta");
            assert!(report.executions[0].streaming);
            assert_eq!(report.executions[0].keyword_count, 1);
            let samples = take_samples();
            assert_eq!(samples.len(), 1);
            let telemetry = samples[0].telemetry.as_ref().unwrap();
            assert_eq!(telemetry.live_recovered, !live_succeeds);
            assert!(!telemetry.used_fallback);
            assert!(
                telemetry
                    .attempts
                    .iter()
                    .all(|attempt| attempt.cost_usd.is_none())
            );
            if !live_succeeds {
                assert!(!report.executions[1].streaming);
                assert_eq!(telemetry.attempts[1].mode, RequestMode::Recorded);
            }
        }
    }

    fn vocabulary(terms: &[&str]) -> Snapshot {
        Snapshot::new(Vocabulary {
            terms: terms.iter().map(|term| (*term).into()).collect(),
            ..Vocabulary::default()
        })
    }
    fn config(models: &[&str]) -> Config {
        let mut config = Config::default();
        config.transcription.models = models.iter().map(|model| (*model).into()).collect();
        config
    }
    fn response(status: u16, body: &str, wait: Option<Duration>) -> Response {
        Response {
            status,
            body: body.as_bytes().to_vec(),
            retry_after: wait,
        }
    }
    fn field(request: &Request, name: &str, value: &str) -> bool {
        String::from_utf8_lossy(&request.body)
            .contains(&format!("name=\"{name}\"\r\n\r\n{value}\r\n"))
    }

    fn microsoft_config(models: &[&str]) -> Config {
        let mut config = config(models);
        config.microsoft.endpoint = "https://fixture.cognitiveservices.azure.com".into();
        config
    }

    fn definition(request: &Request) -> Value {
        let body = String::from_utf8_lossy(&request.body);
        let value = body
            .split("name=\"definition\"\r\n\r\n")
            .nth(1)
            .unwrap()
            .split("\r\n")
            .next()
            .unwrap();
        serde_json::from_str(value).unwrap()
    }

    fn telemetry_config(models: &[&str]) -> Config {
        let mut config = config(models);
        config.transcription.trim_silence = false;
        config.transcription.chunk_seconds = 10;
        config
    }

    #[test]
    fn timeouts_remain_timeouts_before_headers_and_while_reading_the_body() {
        for error in [
            ureq::Error::Timeout(ureq::Timeout::RecvBody),
            ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "PRIVATE_BODY",
            )),
            ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
        ] {
            assert_eq!(http_error_kind(&error, false), ErrorKind::Timeout);
            assert_eq!(http_error_kind(&error, true), ErrorKind::Timeout);
        }
        assert_eq!(
            http_error_kind(&ureq::Error::BodyExceedsLimit(MAX_RESPONSE_BYTES), true),
            ErrorKind::InvalidResponse
        );
        take_samples();
        let config = telemetry_config(&["fixture/primary"]);
        let error = transcribe_configured(
            &[0.1; 1600],
            &Snapshot::default(),
            None,
            &config,
            &mut |_, _| {
                (
                    Err(
                        http_body_failure(&ureq::Error::Timeout(ureq::Timeout::RecvBody), 0).into(),
                    ),
                    Duration::from_secs(1),
                )
            },
            &mut |_, _, _, _| panic!("HTTP only"),
            &mut |_| panic!("no retry"),
        )
        .err()
        .expect("transcription must fail");
        assert_eq!(
            error.downcast_ref::<ChainFailure>().unwrap().failures[0].kind,
            ErrorKind::Timeout
        );
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].failures[0].kind, ErrorKind::Timeout);
        assert_eq!(
            samples[0].telemetry.as_ref().unwrap().attempts[0].error,
            Some(ErrorKind::Timeout)
        );
    }

    #[test]
    fn body_timeout_preserves_sent_keywords_in_statistics_and_history_report() {
        take_samples();
        let config = telemetry_config(&["openai::gpt-transcribe", "fixture/fallback"]);
        let vocabulary = vocabulary(&["Synthetic One", "Synthetic Two"]);
        let result = transcribe_configured(
            &[0.1; 1600],
            &vocabulary,
            None,
            &config,
            &mut |request, _| {
                (
                    if request.provider == Provider::OpenAi {
                        assert_eq!(request.keyword_count, 2);
                        Err(http_body_failure(
                            &ureq::Error::Timeout(ureq::Timeout::RecvBody),
                            request.keyword_count,
                        )
                        .into())
                    } else {
                        Ok(response(200, r#"{"text":"recovered"}"#, None))
                    },
                    Duration::from_millis(50),
                )
            },
            &mut |_, _, _, _| panic!("batch only"),
            &mut |_| panic!("no retry"),
        )
        .unwrap();
        let report = result.report.unwrap();
        assert_eq!(report.executions[0].keyword_count, 2);
        assert_eq!(report.executions[0].outcome, "failed");
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].failures[0].kind, ErrorKind::Timeout);
        let telemetry = samples[0].telemetry.as_ref().unwrap();
        assert_eq!(telemetry.attempts[0].keyword_count, 2);
        assert_eq!(telemetry.attempts[0].error, Some(ErrorKind::Timeout));
        assert_eq!(telemetry.attempts[0].latency_ms, None);
        assert!(telemetry.used_fallback);
    }

    #[test]
    fn live_and_completed_error_categories_and_sent_keywords_reach_legacy_and_new_stats() {
        for kind in [ErrorKind::Auth, ErrorKind::Timeout] {
            take_samples();
            let config = telemetry_config(&["openai::gpt-live-transcribe"]);
            let fixture = || {
                let mut error = streaming::fixture_error(None, false);
                let typed = error.downcast_mut::<streaming::LiveError>().unwrap();
                typed.error_kind = kind;
                typed.keyword_count = 4;
                error
            };
            let live = streaming::PendingLive::fixture(
                config.clone(),
                Snapshot::default(),
                Err(fixture()),
                800,
            );
            let error = transcribe_configured(
                &[0.1; 1600],
                &Snapshot::default(),
                Some(live),
                &config,
                &mut |_, _| panic!("realtime-only model"),
                &mut |_, _, _, _| Err(fixture()),
                &mut |_| panic!("no retry"),
            )
            .err()
            .expect("transcription must fail");
            assert!(
                error
                    .downcast_ref::<ChainFailure>()
                    .unwrap()
                    .failures
                    .iter()
                    .all(|failure| failure.kind == kind)
            );
            let samples = take_samples();
            assert_eq!(samples.len(), 1);
            assert_eq!(samples[0].failures.len(), 2);
            assert!(
                samples[0]
                    .failures
                    .iter()
                    .all(|failure| failure.kind == kind)
            );
            let telemetry = samples[0].telemetry.as_ref().unwrap();
            assert_eq!(telemetry.attempts.len(), 2);
            assert!(
                telemetry
                    .attempts
                    .iter()
                    .all(|attempt| attempt.error == Some(kind) && attempt.keyword_count == 4)
            );
        }
    }

    #[test]
    fn complete_dictation_telemetry_counts_429_and_hint_retry_without_false_fallback() {
        take_samples();
        let config = telemetry_config(&["openai::gpt-transcribe"]);
        let mut calls = 0;
        let mut waits = 0;
        let result = transcribe_configured(
            &[0.1; 1600],
            &vocabulary(&["Synthetic Name"]),
            None,
            &config,
            &mut |request, _| {
                calls += 1;
                assert_eq!(request.keyword_count, if calls == 3 { 0 } else { 1 });
                (
                    Ok(match calls {
                        1 => response(
                            429,
                            r#"{"error":"PRIVATE_RATE_LIMIT","usage":{"cost":0.001}}"#,
                            Some(Duration::ZERO),
                        ),
                        2 => response(
                            422,
                            r#"{"error":"keywords unsupported PRIVATE_HINT","usage":{"cost":0.002}}"#,
                            None,
                        ),
                        _ => response(
                            200,
                            r#"{"text":"complete","usage":{"cost":0,"total_tokens":7}}"#,
                            None,
                        ),
                    }),
                    Duration::from_millis(if calls == 3 { 42 } else { 99 }),
                )
            },
            &mut |_, _, _, _| panic!("batch model"),
            &mut |_| waits += 1,
        )
        .unwrap();
        assert_eq!(result.text, "complete");
        assert_eq!(calls, 3);
        assert_eq!(waits, 1);
        let report = result.report.unwrap();
        assert_eq!(report.omitted_executions, 0);
        assert_eq!(
            report
                .executions
                .iter()
                .map(|request| request.cost_usd)
                .collect::<Vec<_>>(),
            [Some(0.001), Some(0.002), Some(0.0)],
        );
        assert!(!format!("{report:?}").contains("PRIVATE"));
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        let telemetry = samples[0].telemetry.as_ref().unwrap();
        assert_eq!(telemetry.attempts.len(), 3);
        assert!(telemetry.retried);
        assert!(!telemetry.used_fallback);
        assert!(!telemetry.live_recovered);
        assert_eq!(
            telemetry
                .attempts
                .iter()
                .map(|attempt| attempt.error)
                .collect::<Vec<_>>(),
            [
                Some(ErrorKind::RateLimited),
                Some(ErrorKind::Rejected),
                None
            ]
        );
        assert_eq!(
            telemetry
                .attempts
                .iter()
                .map(|attempt| attempt.latency_ms)
                .collect::<Vec<_>>(),
            [None, None, Some(42)]
        );
        assert_eq!(
            telemetry
                .attempts
                .iter()
                .map(|attempt| attempt.keyword_count)
                .collect::<Vec<_>>(),
            [1, 1, 0]
        );
        assert_eq!(
            telemetry
                .attempts
                .iter()
                .map(|attempt| attempt.cost_usd)
                .collect::<Vec<_>>(),
            [Some(0.001), Some(0.002), Some(0.0)]
        );
        assert!(
            telemetry
                .attempts
                .iter()
                .all(|attempt| attempt.mode == RequestMode::Recorded)
        );
        assert!(!format!("{telemetry:?}").contains("PRIVATE"));
    }

    #[test]
    fn local_preflight_is_excluded_but_auth_attempt_and_native_route_identity_are_preserved() {
        take_samples();
        let mut config = telemetry_config(&[
            "microsoft::MAI-Transcribe-2",
            "openai::gpt-live-transcribe",
            "openai::gpt-transcribe",
            "openai/gpt-transcribe",
        ]);
        config.transcription.model_options.insert(
            "openai::gpt-live-transcribe".into(),
            ModelOptions {
                streaming: false,
                ..Default::default()
            },
        );
        let mut calls = 0;
        let result = transcribe_configured(
            &[0.1; 1600],
            &Snapshot::default(),
            None,
            &config,
            &mut |request, _| {
                calls += 1;
                let response = if request.provider == Provider::OpenAi {
                    Err(TransportFailure(ErrorKind::Auth, 0).into())
                } else {
                    assert_eq!(request.provider, Provider::OpenRouter);
                    Ok(response(200, r#"{"text":"done"}"#, None))
                };
                (response, Duration::from_millis(31))
            },
            &mut |_, _, _, _| panic!("realtime profile was not enabled"),
            &mut |_| panic!("no retry"),
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert!(
            result
                .report
                .unwrap()
                .executions
                .iter()
                .all(|request| request.cost_usd.is_none())
        );
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        let telemetry = samples[0].telemetry.as_ref().unwrap();
        assert_eq!(
            telemetry
                .attempts
                .iter()
                .map(|attempt| attempt.model.as_str())
                .collect::<Vec<_>>(),
            ["openai::gpt-transcribe", "openai/gpt-transcribe"]
        );
        assert_eq!(telemetry.attempts[0].error, Some(ErrorKind::Auth));
        assert_eq!(telemetry.attempts[0].keyword_count, 0);
        assert_eq!(telemetry.attempts[1].cost_usd, None);
        assert!(telemetry.used_fallback);
        assert!(!telemetry.retried);
        assert!(!telemetry.live_recovered);
        assert!(
            samples[0]
                .failures
                .iter()
                .any(|failure| failure.model == "microsoft::MAI-Transcribe-2")
        );
    }

    #[test]
    fn all_chunks_produce_one_sample_and_earlier_success_survives_a_later_failure() {
        for fail_second_chunk in [false, true] {
            take_samples();
            let config = telemetry_config(&["fixture/primary", "fixture/fallback"]);
            let audio = vec![0.1; 160_160];
            assert_eq!(chunk_ranges(&audio, 160_000).len(), 2);
            let mut calls = 0;
            let result = transcribe_configured(
                &audio,
                &Snapshot::default(),
                None,
                &config,
                &mut |_, _| {
                    calls += 1;
                    let failed = calls % 2 == 1 || (fail_second_chunk && calls == 4);
                    (
                        Ok(if failed {
                            response(503, "unavailable", None)
                        } else {
                            response(200, r#"{"text":"chunk","usage":{"cost":0.002}}"#, None)
                        }),
                        Duration::from_millis(75),
                    )
                },
                &mut |_, _, _, _| panic!("batch only"),
                &mut |_| panic!("no retry"),
            );
            assert_eq!(result.is_err(), fail_second_chunk);
            assert_eq!(calls, 4);
            let samples = take_samples();
            assert_eq!(samples.len(), 1);
            let sample = &samples[0];
            assert_eq!(sample.words.is_none(), fail_second_chunk);
            let telemetry = sample.telemetry.as_ref().unwrap();
            assert_eq!(telemetry.attempts.len(), 4);
            assert_eq!(
                telemetry
                    .attempts
                    .iter()
                    .filter(|attempt| attempt.success)
                    .count(),
                if fail_second_chunk { 1 } else { 2 }
            );
            assert_eq!(telemetry.used_fallback, !fail_second_chunk);
            assert!(!telemetry.live_recovered);
            assert!(!telemetry.retried);
            assert_eq!(telemetry.attempts[1].latency_ms, Some(75));
            assert_eq!(telemetry.attempts[1].cost_usd, Some(0.002));
            assert!(
                telemetry
                    .attempts
                    .iter()
                    .filter(|attempt| !attempt.success)
                    .all(|attempt| attempt.latency_ms.is_none())
            );
        }
    }

    #[test]
    fn history_request_costs_preserve_retry_fallback_invalid_response_and_chunk_order() {
        take_samples();
        let config = telemetry_config(&[
            "fixture/primary",
            "openai::gpt-transcribe",
            "deepgram::nova-3",
        ]);
        let audio = vec![0.1; 160_160];
        assert_eq!(chunk_ranges(&audio, 160_000).len(), 2);
        let mut calls = 0;
        let result = transcribe_configured(
            &audio, &Snapshot::default(), None, &config,
            &mut |_, _| {
                calls += 1;
                (Ok(match calls {
                    1 => response(429, r#"{"usage":{"cost":0.0001}}"#, Some(Duration::ZERO)),
                    2 => response(503, r#"{"error":"PRIVATE_BODY","usage":{"cost":0.0002}}"#, None),
                    3 => response(200, r#"{"text":" ","usage":{"cost":0.0003}}"#, None),
                    4 => response(200, r#"{"results":{"channels":[{"alternatives":[{"transcript":"first"}]}]}}"#, None),
                    5 => response(200, r#"{"text":"second","usage":{"cost":0.004}}"#, None),
                    _ => panic!("unexpected request"),
                }),Duration::from_millis(25))
            },
            &mut |_,_,_,_| panic!("no websocket"),
            &mut |wait| assert_eq!(wait,Duration::ZERO),
        ).unwrap();
        assert_eq!(result.text, "first second");
        assert_eq!(calls, 5);
        let report = result.report.unwrap();
        assert_eq!(report.omitted_executions, 0);
        assert_eq!(
            report
                .executions
                .iter()
                .map(|request| (
                    request.provider.as_str(),
                    request.model.as_str(),
                    request.outcome.as_str(),
                    request.cost_usd
                ))
                .collect::<Vec<_>>(),
            [
                ("openrouter", "fixture/primary", "failed", Some(0.0001)),
                ("openrouter", "fixture/primary", "failed", Some(0.0002)),
                ("openai", "gpt-transcribe", "failed", Some(0.0003)),
                ("deepgram", "nova-3", "success", None),
                ("openrouter", "fixture/primary", "success", Some(0.004)),
            ]
        );
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        let telemetry = samples[0].telemetry.as_ref().unwrap();
        assert_eq!(
            report
                .executions
                .iter()
                .map(|request| request.cost_usd)
                .collect::<Vec<_>>(),
            telemetry
                .attempts
                .iter()
                .map(|request| request.cost_usd)
                .collect::<Vec<_>>()
        );
        assert!(telemetry.used_fallback && telemetry.retried);
        assert!(!format!("{report:?}").contains("PRIVATE_BODY"));
    }

    #[test]
    fn extreme_reported_usage_saturates_across_chunks_before_statistics() {
        take_samples();
        let config = telemetry_config(&["fixture/primary"]);
        let audio = vec![0.1; 160_160];
        assert_eq!(chunk_ranges(&audio, 160_000).len(), 2);
        let body =
            json!({"text":"chunk", "usage":{"total_tokens":u64::MAX, "cost":f64::MAX}}).to_string();
        let mut calls = 0;
        transcribe_configured(
            &audio,
            &Snapshot::default(),
            None,
            &config,
            &mut |_, _| {
                calls += 1;
                (Ok(response(200, &body, None)), Duration::from_millis(10))
            },
            &mut |_, _, _, _| panic!("batch only"),
            &mut |_| panic!("no retry"),
        )
        .unwrap();
        assert_eq!(calls, 2);
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].tokens, u64::MAX);
        assert_eq!(samples[0].cost_usd, f64::MAX);
        let telemetry = samples[0].telemetry.as_ref().unwrap();
        assert_eq!(telemetry.attempts.len(), 2);
        assert!(
            telemetry
                .attempts
                .iter()
                .all(|attempt| attempt.success && attempt.cost_usd.is_some_and(f64::is_finite))
        );
    }

    #[test]
    fn failed_live_can_be_recovered_on_same_model_without_counting_model_fallback() {
        for recovered in [false, true] {
            take_samples();
            let config = telemetry_config(&["deepgram::nova-3"]);
            let live = streaming::PendingLive::fixture(
                config.clone(),
                Snapshot::default(),
                Err(streaming::fixture_error(Some(503), false)),
                800,
            );
            let result = transcribe_configured(
                &[0.1; 1600],
                &Snapshot::default(),
                Some(live),
                &config,
                &mut |_, _| {
                    (
                        Ok(if recovered {
                            response(
                                200,
                                r#"{"results":{"channels":[{"alternatives":[{"transcript":"recovered"}]}]}}"#,
                                None,
                            )
                        } else {
                            response(503, "unavailable", None)
                        }),
                        Duration::from_millis(93),
                    )
                },
                &mut |_, _, _, _| panic!("Deepgram completed uses batch"),
                &mut |_| panic!("no retry"),
            );
            assert_eq!(result.is_ok(), recovered);
            let samples = take_samples();
            assert_eq!(samples.len(), 1);
            let telemetry = samples[0].telemetry.as_ref().unwrap();
            assert_eq!(telemetry.attempts.len(), 2);
            assert_eq!(telemetry.attempts[0].mode, RequestMode::Live);
            assert_eq!(telemetry.attempts[1].mode, RequestMode::Recorded);
            assert_eq!(telemetry.attempts[1].latency_ms, recovered.then_some(93));
            assert_eq!(telemetry.live_recovered, recovered);
            assert!(!telemetry.used_fallback);
            assert!(!telemetry.retried);
        }
    }

    #[test]
    fn completed_websocket_retries_are_recorded_mode_and_success_latency_only() {
        take_samples();
        let mut config = telemetry_config(&["openai::gpt-live-transcribe"]);
        config.transcription.model_options.insert(
            "openai::gpt-live-transcribe".into(),
            ModelOptions {
                streaming: true,
                ..Default::default()
            },
        );
        let mut calls = 0;
        transcribe_configured(
            &[0.1; 1600],
            &Snapshot::default(),
            None,
            &config,
            &mut |_, _| panic!("no batch transport"),
            &mut |id, _, _, _| {
                calls += 1;
                if calls == 1 {
                    Err(streaming::fixture_error(Some(429), false))
                } else {
                    Ok(streaming::LiveResult {
                        text: "finished".into(),
                        model: id.into(),
                        keyword_count: 0,
                        latency_ms: 61,
                    })
                }
            },
            &mut |_| {},
        )
        .unwrap();
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        let telemetry = samples[0].telemetry.as_ref().unwrap();
        assert_eq!(telemetry.attempts.len(), 2);
        assert_eq!(telemetry.attempts[0].error, Some(ErrorKind::RateLimited));
        assert_eq!(telemetry.attempts[0].latency_ms, None);
        assert_eq!(telemetry.attempts[1].latency_ms, Some(61));
        assert!(
            telemetry
                .attempts
                .iter()
                .all(|attempt| attempt.mode == RequestMode::Recorded)
        );
        assert!(telemetry.retried);
        assert!(!telemetry.live_recovered);
        assert!(!telemetry.used_fallback);
    }

    #[test]
    fn telemetry_distinguishes_explicit_zero_missing_and_invalid_costs() {
        for (body, expected) in [
            (r#"{"text":"ok","usage":{"cost":0}}"#, Some(0.0)),
            (r#"{"text":"ok","usage":{"cost":0.004}}"#, Some(0.004)),
            (r#"{"text":"ok","usage":{"cost":null}}"#, None),
            (r#"{"text":"ok","usage":{"cost":-1}}"#, None),
            (r#"{"text":"ok","usage":{"cost":"0.004"}}"#, None),
            (r#"{"text":"ok"}"#, None),
        ] {
            let (_, usage) = parse(Provider::OpenRouter, body.as_bytes()).unwrap();
            assert_eq!(usage.cost_reported, expected.is_some());
            assert_eq!(usage.cost_usd, expected.unwrap_or_default());
            assert_eq!(response_cost(body.as_bytes()), expected);
        }
    }

    #[test]
    fn microsoft_grok_and_google_follow_their_native_batch_contracts() {
        let config = microsoft_config(&[]);
        let vocabulary = vocabulary(&["Synthetic Nimbus"]);
        let hints = HintPlan::default();
        let options = ModelOptions {
            language: "pt".into(),
            prompt: "NOT_SUPPORTED_CONTEXT".into(),
            temperature: Some(0.5),
            ..Default::default()
        };
        let microsoft = request(
            &config,
            ModelRef::parse("microsoft::MAI-Transcribe-2"),
            &options,
            b"WAV_FIXTURE",
            &vocabulary,
            &hints,
            false,
        )
        .unwrap();
        assert_eq!(
            microsoft.url,
            "https://fixture.cognitiveservices.azure.com/speechtotext/transcriptions:transcribe?api-version=2025-10-15"
        );
        let definition = definition(&microsoft);
        assert_eq!(definition["enhancedMode"]["enabled"], true);
        assert_eq!(definition["enhancedMode"]["model"], "MAI-Transcribe-2");
        assert_eq!(
            definition["enhancedMode"]["modelOptions"]["transcribeStyle"],
            "clean"
        );
        assert_eq!(definition["locales"], json!(["pt-BR"]));
        assert_eq!(
            definition["phraseList"]["phrases"],
            json!(["Synthetic Nimbus"])
        );
        assert!(
            String::from_utf8_lossy(&microsoft.body)
                .contains("name=\"audio\"; filename=\"clip.wav\"")
        );
        assert_eq!(microsoft.keyword_count, 1);

        let grok = request(
            &config,
            ModelRef::parse("grok::grok-voice-transcribe-2.0"),
            &options,
            b"WAV_FIXTURE",
            &vocabulary,
            &hints,
            false,
        )
        .unwrap();
        assert_eq!(grok.url, "https://api.x.ai/v1/stt");
        for (name, value) in [
            ("model", "grok-voice-transcribe-2.0"),
            ("format", "true"),
            ("language", "pt"),
            ("filler_words", "false"),
            ("keyterm", "Synthetic Nimbus"),
        ] {
            assert!(field(&grok, name, value));
        }
        let multipart = String::from_utf8_lossy(&grok.body);
        assert!(
            multipart.find("name=\"keyterm\"").unwrap() < multipart.find("name=\"file\"").unwrap()
        );

        let google = request(
            &config,
            ModelRef::parse("google::gemini-3.5-transcribe"),
            &options,
            b"WAV_FIXTURE",
            &vocabulary,
            &hints,
            false,
        )
        .unwrap();
        assert_eq!(
            google.url,
            "https://generativelanguage.googleapis.com/v1beta/interactions"
        );
        let body: Value = serde_json::from_slice(&google.body).unwrap();
        assert_eq!(body["model"], "gemini-3.5-transcribe");
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["type"], "audio");
        assert_eq!(body["input"][0]["mime_type"], "audio/wav");
        assert_eq!(body["input"][0]["data"], encode_base64(b"WAV_FIXTURE"));
        assert_eq!(
            body["generation_config"]["transcription_config"],
            json!({"mode":"smart","language_codes":["pt-BR"],"custom_vocabulary":["Synthetic Nimbus"]})
        );
        for request in [&microsoft, &grok, &google] {
            let body = String::from_utf8_lossy(&request.body);
            for absent in [
                "NOT_SUPPORTED_CONTEXT",
                "temperature",
                "diarization",
                "timestamp",
                "punctuate",
                "numerals",
            ] {
                assert!(!body.contains(absent), "unsupported field {absent}");
            }
            assert_eq!(request.keyword_count, 1);
        }
    }

    #[test]
    fn new_provider_options_preserve_false_auto_and_whole_keyword_limits() {
        let config = microsoft_config(&[]);
        let vocabulary = vocabulary(&["Synthetic Nimbus"]);
        let hints = HintPlan::default();
        let options = ModelOptions {
            smart_format: false,
            no_verbatim: false,
            ..Default::default()
        };
        for id in [
            "microsoft::MAI-Transcribe-2",
            "grok::grok-voice-transcribe-2.0",
            "google::gemini-3.5-transcribe",
        ] {
            let request = request(
                &config,
                ModelRef::parse(id),
                &options,
                b"WAV",
                &vocabulary,
                &hints,
                true,
            )
            .unwrap();
            assert_eq!(request.keyword_count, 0);
            assert!(!String::from_utf8_lossy(&request.body).contains("Synthetic Nimbus"));
            match request.provider {
                Provider::Microsoft => {
                    let body = definition(&request);
                    assert!(body.get("locales").is_none());
                    assert_eq!(
                        body["enhancedMode"]["modelOptions"]["transcribeStyle"],
                        "verbatim"
                    );
                }
                Provider::Grok => {
                    assert!(field(&request, "format", "false"));
                    assert!(field(&request, "filler_words", "true"));
                    assert!(!String::from_utf8_lossy(&request.body).contains("name=\"language\""));
                }
                Provider::Google => {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(
                        body["generation_config"]["transcription_config"],
                        json!({"mode":{"type":"verbatim"}})
                    );
                }
                _ => unreachable!(),
            }
        }
        for code in ["auto", "ko", "nl"] {
            assert!(!grok_format_supported(code));
            let options = ModelOptions {
                language: code.into(),
                ..Default::default()
            };
            let request = request(
                &config,
                ModelRef::parse("grok::grok-voice-transcribe-2.0"),
                &options,
                b"WAV",
                &vocabulary,
                &hints,
                false,
            )
            .unwrap();
            assert!(field(&request, "format", "false"));
        }
        let names = Snapshot::new(Vocabulary {
            terms: (0..1100).map(|index| format!("Name{index}")).collect(),
            ..Default::default()
        });
        assert_eq!(
            keywords(ModelRef::parse("google::gemini-3.5-transcribe"), &names).len(),
            1000
        );
        assert_eq!(
            keywords(ModelRef::parse("grok::grok-voice-transcribe-2.0"), &names).len(),
            100
        );
        let long = "A".repeat(51);
        let whole = "Á".repeat(50);
        assert_eq!(
            keywords(
                ModelRef::parse("grok::grok-voice-transcribe-2.0"),
                &self::vocabulary(&[&long, &whole])
            ),
            [whole]
        );
    }

    #[test]
    fn new_response_parsers_reject_empty_partial_and_nontranscript_google_content() {
        assert_eq!(
            parse(
                Provider::Microsoft,
                br#"{"combinedPhrases":[{"text":" hello "}]}"#
            )
            .unwrap()
            .0,
            "hello"
        );
        assert_eq!(
            parse(Provider::Grok, br#"{"text":" hello "}"#).unwrap().0,
            "hello"
        );
        let google = br#"{"status":"completed","error":null,"steps":[{"type":"user_input","content":[{"type":"text","text":"PRIVATE_INPUT"}]},{"type":"model_output","content":[{"type":"thought","text":"PRIVATE_THOUGHT"},{"type":"text","text":"hello"},{"type":"text","text":"world"}]}],"usage":{"total_tokens":42}}"#;
        let (text, usage) = parse(Provider::Google, google).unwrap();
        assert_eq!(text, "hello\nworld");
        assert_eq!(usage.tokens, 42);
        for (provider, body) in [
            (Provider::Microsoft, r#"{"combinedPhrases":[{"text":" "}]}"#),
            (Provider::Grok, r#"{"text":""}"#),
            (
                Provider::Google,
                r#"{"status":"in_progress","steps":[{"type":"model_output","content":[{"type":"text","text":"partial"}]}]}"#,
            ),
            (
                Provider::Google,
                r#"{"status":"completed","steps":[{"type":"model_output","content":[{"type":"thought","text":"PRIVATE_THOUGHT"}]}]}"#,
            ),
        ] {
            assert!(parse(provider, body.as_bytes()).is_err());
        }
        let error = parse(
            Provider::Google,
            br#"{"error":{"message":"PRIVATE_REMOTE_ERROR"}}"#,
        )
        .unwrap_err();
        assert!(!error.to_string().contains("PRIVATE_REMOTE_ERROR"));
    }

    #[test]
    fn new_provider_hint_rejection_empty_response_and_configuration_errors_keep_fallback() {
        let ids = [
            "microsoft::MAI-Transcribe-2",
            "grok::grok-voice-transcribe-2.0",
            "google::gemini-3.5-transcribe",
        ];
        let mut config = microsoft_config(&ids);
        let vocabulary = vocabulary(&["Synthetic Nimbus"]);
        let hints = HintPlan::default();
        for valid_endpoint in [true, false] {
            if !valid_endpoint {
                config.microsoft.endpoint = "https://PRIVATE_MARKER.evil.test".into();
            }
            let mut progress = Progress::default();
            let mut calls = Vec::new();
            let success = Chain {config:&config, vocabulary:&vocabulary,hints:&hints,cancelled:None}.run(
                &[0.1;160], Duration::ZERO, &mut progress,
                &mut |request, _| {
                    calls.push((request.provider, request.keyword_count));
                    (Ok(match request.provider {
                        Provider::Microsoft if request.keyword_count > 0 => response(422,"phraseList unsupported PRIVATE_MARKER",None),
                        Provider::Microsoft => response(200,r#"{"combinedPhrases":[{"text":""}]}"#,None),
                        Provider::Grok => response(503,"PRIVATE_MARKER",None),
                        Provider::Google => response(200,r#"{"status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":"done"}]}]}"#,None),
                        _ => panic!("unexpected provider"),
                    }),Duration::ZERO)
                }, &mut |_,_,_,_| panic!("no websocket"), &mut |_| panic!("no wait")
            ).unwrap();
            assert_eq!(success.model, ids[2]);
            assert_eq!(success.text, "done");
            assert!(
                !progress
                    .failures
                    .iter()
                    .any(|failure| failure.detail.contains("PRIVATE_MARKER"))
            );
            if valid_endpoint {
                assert_eq!(
                    calls,
                    [
                        (Provider::Microsoft, 1),
                        (Provider::Microsoft, 0),
                        (Provider::Grok, 1),
                        (Provider::Google, 1)
                    ]
                );
                assert!(
                    progress
                        .failures
                        .iter()
                        .any(|failure| failure.kind == ErrorKind::InvalidResponse)
                );
            } else {
                assert_eq!(calls, [(Provider::Grok, 1), (Provider::Google, 1)]);
                assert_eq!(progress.executions[0].keyword_count, 0);
                assert_eq!(progress.failures[0].kind, ErrorKind::Rejected);
            }
        }
    }

    #[test]
    fn new_provider_hint_rejection_only_retries_fields_that_were_sent() {
        let config = microsoft_config(&[]);
        let vocabulary = vocabulary(&["Synthetic Nimbus"]);
        let hints = HintPlan::default();
        for (id, field) in [
            ("microsoft::MAI-Transcribe-2", "phraseList"),
            ("grok::grok-voice-transcribe-2.0", "keyterm"),
            ("google::gemini-3.5-transcribe", "custom_vocabulary"),
        ] {
            let model = ModelRef::parse(id);
            let sent = request(
                &config,
                model,
                &ModelOptions::default(),
                b"WAV",
                &vocabulary,
                &hints,
                false,
            )
            .unwrap();
            let without = request(
                &config,
                model,
                &ModelOptions::default(),
                b"WAV",
                &vocabulary,
                &hints,
                true,
            )
            .unwrap();
            assert!(keywords_rejected(
                model,
                &sent,
                &response(422, field, None),
                &hints
            ));
            assert!(!keywords_rejected(
                model,
                &sent,
                &response(503, field, None),
                &hints
            ));
            assert!(!keywords_rejected(
                model,
                &without,
                &response(422, field, None),
                &hints
            ));
        }
    }

    #[test]
    fn unsafe_endpoints_are_rejected_before_credentials_or_transport() {
        let config = Config::default();
        for url in [
            "http://public.example/v1",
            "http://localhost.evil.example/v1",
            "http://127.0.0.1.evil.example/v1",
            "http://localhost@evil.example/v1",
            "https://private-marker@provider.example/v1",
            "http://127.0.0.1\\@evil.example/v1",
            "https://provider.example/v1#private-marker",
        ] {
            let request = Request {
                provider: Provider::OpenRouter,
                url: url.into(),
                content_type: "application/json".into(),
                body: b"{}".to_vec(),
                keyword_count: 0,
            };
            let error = send(&request, &config, Duration::from_secs(1)).unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<TransportFailure>()
                    .map(|error| error.0),
                Some(ErrorKind::Rejected),
            );
            assert!(!error.to_string().contains("private-marker"));
        }
        for url in [
            "https://provider.example/v1",
            "http://localhost:8080/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:8080/v1",
        ] {
            let request = Request {
                provider: Provider::OpenRouter,
                url: url.into(),
                content_type: "application/json".into(),
                body: b"{}".to_vec(),
                keyword_count: 0,
            };
            let error = send(&request, &config, Duration::from_secs(1)).unwrap_err();
            assert_eq!(
                error.to_string(),
                "HTTP transcription is disabled in unit tests; inject a transport."
            );
        }
    }

    #[test]
    fn provider_request_contracts_keep_supported_options_and_whole_keywords() {
        let config = Config::default();
        let vocabulary = vocabulary(&["Zephyr Files", "Álvaro"]);
        let options = ModelOptions {
            language: "pt".into(),
            prompt: "Technical meeting".into(),
            temperature: Some(0.2),
            smart_format: true,
            punctuate: false,
            numerals: true,
            no_verbatim: true,
            ..ModelOptions::default()
        };
        let wav = encode_wav(&[0.1; 160]).unwrap();
        let hints = HintPlan::default();
        let openai = request(
            &config,
            ModelRef::parse("openai::gpt-transcribe"),
            &options,
            &wav,
            &vocabulary,
            &hints,
            false,
        )
        .unwrap();
        assert_eq!(openai.url, "https://api.openai.com/v1/audio/transcriptions");
        assert!(field(&openai, "model", "gpt-transcribe"));
        assert!(field(&openai, "response_format", "json"));
        assert!(field(&openai, "languages[]", "pt"));
        assert!(!field(&openai, "language", "pt"));
        assert!(field(&openai, "keywords[]", "Zephyr Files"));
        assert!(field(&openai, "keywords[]", "Álvaro"));
        assert!(field(&openai, "prompt", "Technical meeting"));
        assert!(field(&openai, "temperature", "0.2"));
        assert_eq!(openai.keyword_count, 2);
        assert!(openai.body.windows(wav.len()).any(|bytes| bytes == wav));
        for model in [
            "openai::gpt-4o-transcribe",
            "openai::gpt-4o-mini-transcribe",
            "openai::whisper-1",
        ] {
            let legacy = request(
                &config,
                ModelRef::parse(model),
                &options,
                &wav,
                &vocabulary,
                &hints,
                false,
            )
            .unwrap();
            assert!(field(&legacy, "language", "pt"));
            assert!(field(
                &legacy,
                "prompt",
                "Technical meeting\n\nVocabulary: Zephyr Files, Álvaro"
            ));
            assert!(!String::from_utf8_lossy(&legacy.body).contains("name=\"keywords[]\""));
            assert_eq!(legacy.keyword_count, 2);
        }
        let deepgram = request(
            &config,
            ModelRef::parse("deepgram::nova-3"),
            &options,
            &wav,
            &vocabulary,
            &hints,
            false,
        )
        .unwrap();
        let query: Vec<_> = url::Url::parse(&deepgram.url)
            .unwrap()
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        assert!(query.contains(&("keyterm".into(), "Zephyr Files".into())));
        assert!(query.contains(&("keyterm".into(), "Álvaro".into())));
        assert!(query.contains(&("smart_format".into(), "true".into())));
        assert!(query.contains(&("punctuate".into(), "false".into())));
        assert_eq!(deepgram.body, wav);
        assert_eq!(deepgram.content_type, "audio/wav");
        let auto = request(
            &config,
            ModelRef::parse("deepgram::nova-2"),
            &ModelOptions::default(),
            &wav,
            &vocabulary,
            &hints,
            false,
        )
        .unwrap();
        assert!(auto.url.contains("detect_language=true"));
        assert!(!auto.url.contains("keyterm"));
        let scribe = request(
            &config,
            ModelRef::parse("elevenlabs::scribe_v2"),
            &options,
            &wav,
            &vocabulary,
            &hints,
            false,
        )
        .unwrap();
        for (name, value) in [
            ("model_id", "scribe_v2"),
            ("tag_audio_events", "false"),
            ("diarize", "false"),
            ("language_code", "pt"),
            ("no_verbatim", "true"),
            ("keyterms", "Zephyr Files"),
        ] {
            assert!(field(&scribe, name, value));
        }
    }

    #[test]
    fn prompt_and_scribe_limits_skip_whole_terms_without_truncating_context() {
        let context = "x".repeat(200);
        let (prompt, count) = prompt_with_terms(&context, &["Zephyr Files".into()]);
        assert_eq!(prompt, context);
        assert_eq!(count, 0);
        let long = "A".repeat(50);
        let accepted = "B".repeat(49);
        let vocabulary =
            vocabulary(&[&long, &accepted, "One-Two-Three-Four-Five-Six", "Neo Brand"]);
        assert_eq!(
            keywords(ModelRef::parse("elevenlabs::scribe_v2"), &vocabulary),
            [accepted, "Neo Brand".into()]
        );
        assert_eq!(
            keywords(
                ModelRef::parse("elevenlabs::scribe_v2_realtime"),
                &vocabulary
            ),
            ["Neo Brand"]
        );
        assert_eq!(pcm16(&[f32::NAN, 2.0, -2.0]), [0, 0, 255, 127, 1, 128]);
    }

    #[test]
    fn legacy_openrouter_ids_keep_their_route_and_verified_hint_shape() {
        let config = Config::default();
        let id = "microsoft/mai-transcribe-2";
        let hints = HintPlan::fixture(
            id,
            json!({"options":{"azure":{"phraseList":{"phrases":["Zephyr Files"]}}}}),
        );
        let request = request(
            &config,
            ModelRef::parse(id),
            &ModelOptions::default(),
            b"fixture-wave",
            &vocabulary(&["Zephyr Files"]),
            &hints,
            false,
        )
        .unwrap();
        assert_eq!(request.provider, Provider::OpenRouter);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], id);
        assert_eq!(
            body.pointer("/provider/options/azure/phraseList/phrases/0")
                .unwrap(),
            "Zephyr Files"
        );
        assert_eq!(request.keyword_count, 1);
    }

    #[test]
    fn mixed_provider_failures_fall_back_and_never_echo_remote_payloads() {
        let config = config(&[
            "openai/vendor-model",
            "openai::gpt-transcribe",
            "deepgram::nova-3",
            "elevenlabs::scribe_v2",
        ]);
        let vocabulary = Snapshot::default();
        let hints = HintPlan::default();
        let mut progress = Progress::default();
        let mut calls = Vec::new();
        let success=Chain { config:&config,vocabulary:&vocabulary,hints:&hints,cancelled:None }.run(&[0.1;160],Duration::ZERO,&mut progress,
            &mut |request,_| { calls.push(request.provider); (Ok(match request.provider { Provider::OpenRouter=>response(503,"PRIVATE_PROVIDER_PAYLOAD",None), Provider::OpenAi=>response(200,r#"{"text":"   "}"#,None), _=>response(200,r#"{"results":{"channels":[{"alternatives":[{"transcript":"done"}]}]}}"#,None) }),Duration::from_millis(17)) },
            &mut |_,_,_,_| panic!("batch-capable models must not use completed WebSockets"), &mut |_| panic!("no wait")).unwrap();
        assert_eq!(success.model, "deepgram::nova-3");
        assert_eq!(success.text, "done");
        assert_eq!(success.latency_ms, 17);
        assert_eq!(
            calls,
            [Provider::OpenRouter, Provider::OpenAi, Provider::Deepgram]
        );
        assert_eq!(progress.failures.len(), 2);
        assert!(
            !progress
                .failures
                .iter()
                .any(|failure| failure.detail.contains("PRIVATE_PROVIDER_PAYLOAD"))
        );
        assert_eq!(
            progress
                .executions
                .iter()
                .map(|execution| execution.outcome.as_str())
                .collect::<Vec<_>>(),
            ["failed", "failed", "success"]
        );
    }

    #[test]
    fn empty_responses_fall_back_after_rate_limit_retry_and_fail_when_all_are_empty() {
        let config = config(&[
            "openai/vendor-model",
            "openai::gpt-transcribe",
            "deepgram::nova-3",
        ]);
        let vocabulary = Snapshot::default();
        let hints = HintPlan::default();
        for final_succeeds in [true, false] {
            let mut progress = Progress::default();
            let mut calls = Vec::new();
            let mut waits = Vec::new();
            let result = Chain {
                config: &config,
                vocabulary: &vocabulary,
                hints: &hints,
                cancelled: None,
            }
            .run(
                &[0.1; 160],
                Duration::ZERO,
                &mut progress,
                &mut |request, _| {
                    calls.push(request.provider);
                    let response = match request.provider {
                        Provider::OpenRouter if final_succeeds && calls.len() == 1 => {
                            response(429, "rate limit", Some(Duration::from_millis(100)))
                        }
                        Provider::OpenRouter if final_succeeds => response(503, "unavailable", None),
                        Provider::Deepgram => response(
                            200,
                            if final_succeeds {
                                r#"{"results":{"channels":[{"alternatives":[{"transcript":"done"}]}]}}"#
                            } else {
                                r#"{"results":{"channels":[{"alternatives":[{"transcript":" \n\t "}]}]}}"#
                            },
                            None,
                        ),
                        _ => response(200, r#"{"text":" \n\t "}"#, None),
                    };
                    (Ok(response), Duration::ZERO)
                },
                &mut |_, _, _, _| panic!("no websocket"),
                &mut |wait| waits.push(wait),
            );
            if final_succeeds {
                let success = result.unwrap();
                assert_eq!(success.model, "deepgram::nova-3");
                assert_eq!(success.text, "done");
                assert_eq!(
                    calls,
                    [
                        Provider::OpenRouter,
                        Provider::OpenRouter,
                        Provider::OpenAi,
                        Provider::Deepgram
                    ]
                );
                assert_eq!(waits, [Duration::from_millis(100)]);
                assert_eq!(progress.failures.len(), 2);
                assert_eq!(progress.failures[1].kind, ErrorKind::InvalidResponse);
                assert_eq!(progress.executions.len(), 4);
            } else {
                let Err(error) = result else {
                    panic!("empty responses must fail the chain")
                };
                let failure = error.downcast_ref::<ChainFailure>().unwrap();
                assert_eq!(failure.failures.len(), 3);
                assert!(
                    failure
                        .failures
                        .iter()
                        .all(|failure| failure.kind == ErrorKind::InvalidResponse)
                );
                assert_eq!(
                    calls,
                    [Provider::OpenRouter, Provider::OpenAi, Provider::Deepgram]
                );
                assert!(waits.is_empty());
                assert!(
                    progress
                        .executions
                        .iter()
                        .all(|execution| execution.outcome == "failed")
                );
            }
        }
    }

    #[test]
    fn rate_limit_and_keyword_rejection_retry_within_one_original_budget() {
        let mut config = config(&["openai::gpt-transcribe"]);
        config.transcription.total_timeout_seconds = 3;
        let vocabulary = vocabulary(&["Zephyr Files"]);
        let hints = HintPlan::default();
        let mut progress = Progress::default();
        let mut counts = Vec::new();
        let mut waits = Vec::new();
        let success = Chain {
            config: &config,
            vocabulary: &vocabulary,
            hints: &hints,
            cancelled: None,
        }
        .run(
            &[0.1; 160],
            Duration::ZERO,
            &mut progress,
            &mut |request, timeout| {
                assert!(timeout <= Duration::from_secs(3));
                counts.push(request.keyword_count);
                (
                    Ok(match counts.len() {
                        1 => response(429, "rate limit", Some(Duration::from_millis(100))),
                        2 => response(422, "keywords unsupported PRIVATE_MARKER", None),
                        _ => response(200, r#"{"text":"done"}"#, None),
                    }),
                    Duration::from_millis(25),
                )
            },
            &mut |_, _, _, _| panic!("no websocket"),
            &mut |wait| waits.push(wait),
        )
        .unwrap();
        assert_eq!(counts, [1, 1, 0]);
        assert_eq!(waits, [Duration::from_millis(100)]);
        assert_eq!(success.latency_ms, 25);
        assert!(!progress.failures[0].detail.contains("PRIVATE_MARKER"));
        config.transcription.total_timeout_seconds = 1;
        let mut calls = 0;
        let result = Chain {
            config: &config,
            vocabulary: &vocabulary,
            hints: &hints,
            cancelled: None,
        }
        .run(
            &[0.1; 160],
            Duration::ZERO,
            &mut Progress::default(),
            &mut |_, timeout| {
                calls += 1;
                if calls > 1 {
                    assert!(timeout <= Duration::from_millis(100));
                }
                (
                    Ok(response(422, "keywords unsupported", None)),
                    Duration::from_millis(900),
                )
            },
            &mut |_, _, _, _| panic!("no websocket"),
            &mut |_| panic!("no wait"),
        );
        assert!(result.is_err());
        assert_eq!(calls, 2);
    }

    #[test]
    fn missing_key_is_auth_with_zero_sent_keywords_before_fallback() {
        let config = config(&["openai::gpt-transcribe", "deepgram::nova-3"]);
        let vocabulary = vocabulary(&["Zephyr Files"]);
        let hints = HintPlan::default();
        let mut progress = Progress::default();
        let result = Chain {
            config: &config,
            vocabulary: &vocabulary,
            hints: &hints,
            cancelled: None,
        }
        .run(
            &[0.1; 160],
            Duration::ZERO,
            &mut progress,
            &mut |request, _| {
                (
                    if request.provider == Provider::OpenAi {
                        Err(TransportFailure(ErrorKind::Auth, 0).into())
                    } else {
                        Ok(response(401, "PRIVATE_PROVIDER_MESSAGE", None))
                    },
                    Duration::ZERO,
                )
            },
            &mut |_, _, _, _| panic!("no websocket"),
            &mut |_| panic!("no wait"),
        );
        assert!(result.is_err());
        assert_eq!(progress.failures.len(), 2);
        assert!(
            progress
                .failures
                .iter()
                .all(|failure| failure.kind == ErrorKind::Auth)
        );
        assert_eq!(progress.executions[0].keyword_count, 0);
        assert_eq!(progress.executions[1].keyword_count, 1);
        if let Err(error) = result {
            assert!(!error.to_string().contains("PRIVATE_PROVIDER_MESSAGE"));
        }
    }

    #[test]
    fn realtime_only_uses_completed_websocket_and_keyword_retry_stays_local_to_attempt() {
        let mut config = config(&["openai::gpt-live-transcribe"]);
        config.transcription.model_options.insert(
            "openai::gpt-live-transcribe".into(),
            ModelOptions {
                streaming: true,
                ..ModelOptions::default()
            },
        );
        let vocabulary = vocabulary(&["Zephyr Files"]);
        let hints = HintPlan::default();
        let mut progress = Progress::default();
        let mut count = 0;
        let success = Chain {
            config: &config,
            vocabulary: &vocabulary,
            hints: &hints,
            cancelled: None,
        }
        .run(
            &[0.1; 160],
            Duration::ZERO,
            &mut progress,
            &mut |_, _| panic!("realtime-only models never use POST"),
            &mut |id, _, vocabulary, _| {
                count += 1;
                if count == 1 {
                    assert!(vocabulary.settings().remote_hints);
                    Err(streaming::fixture_error(Some(400), true))
                } else {
                    assert!(!vocabulary.settings().remote_hints);
                    Ok(streaming::LiveResult {
                        text: "recovered".into(),
                        model: id.into(),
                        keyword_count: 0,
                        latency_ms: 21,
                    })
                }
            },
            &mut |_| panic!("no wait"),
        )
        .unwrap();
        assert_eq!(success.text, "recovered");
        assert_eq!(count, 2);
        assert!(
            progress
                .executions
                .iter()
                .all(|execution| !execution.streaming)
        );
        assert!(
            progress
                .executions
                .iter()
                .all(|execution| execution.cost_usd.is_none())
        );
        assert!(
            vocabulary.settings().remote_hints,
            "shared snapshot must not be changed by retry"
        );
        config
            .transcription
            .model_options
            .get_mut("openai::gpt-live-transcribe")
            .unwrap()
            .streaming = false;
        assert!(
            Chain {
                config: &config,
                vocabulary: &vocabulary,
                hints: &hints,
                cancelled: None
            }
            .run(
                &[0.1; 160],
                Duration::ZERO,
                &mut Progress::default(),
                &mut |_, _| panic!("disabled"),
                &mut |_, _, _, _| panic!("disabled"),
                &mut |_| {}
            )
            .is_err()
        );
    }

    fn live_fixture(text: &str, sent: usize, trim: bool) -> streaming::PendingLive {
        let mut config = config(&["openai::gpt-live-transcribe"]);
        config.transcription.trim_silence = trim;
        streaming::PendingLive::fixture(
            config,
            Snapshot::default(),
            Ok(streaming::LiveResult {
                text: text.into(),
                model: "openai::gpt-live-transcribe".into(),
                keyword_count: 2,
                latency_ms: 425,
            }),
            sent,
        )
    }
    fn take_samples() -> Vec<Sample> {
        TEST_SAMPLES.with(|samples| std::mem::take(&mut *samples.borrow_mut()))
    }

    #[test]
    fn live_success_has_one_stats_sample_and_its_actual_finish_latency() {
        take_samples();
        let result = transcribe(
            &[0.1; 1600],
            &Snapshot::default(),
            Some(live_fixture("hello world", 1600, false)),
        )
        .unwrap();
        let report = result.report.unwrap();
        assert_eq!(report.latency_ms, 425);
        assert!(report.executions[0].streaming);
        assert_eq!(report.executions[0].keyword_count, 2);
        assert_eq!(report.executions[0].cost_usd, None);
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].words, Some(2));
        assert_eq!(samples[0].latency_ms, 425);
        let telemetry = samples[0].telemetry.as_ref().unwrap();
        assert_eq!(telemetry.attempts.len(), 1);
        assert_eq!(telemetry.attempts[0].mode, RequestMode::Live);
        assert_eq!(telemetry.attempts[0].latency_ms, Some(425));
        assert_eq!(telemetry.attempts[0].cost_usd, None);
        assert!(!telemetry.used_fallback && !telemetry.retried && !telemetry.live_recovered);
        assert_eq!(
            samples[0].model_latency["openai::gpt-live-transcribe"].total_ms,
            425
        );
    }

    #[test]
    fn grok_finalized_segments_with_empty_done_remain_one_live_request() {
        take_samples();
        let config = telemetry_config(&["grok::grok-voice-transcribe-2.0"]);
        let live_result = streaming::fixture_grok_live_result(
            &[
                json!({"type":"transcript.partial","is_final":true,"speech_final":false,"start":0.0,"duration":0.4,"text":"Nome"}),
                json!({"type":"transcript.partial","is_final":true,"speech_final":true,"start":0.0,"duration":0.8,"text":"Nome correto"}),
            ],
            &[json!({"type":"transcript.done","duration":1.0,"text":""})],
        );
        let live = streaming::PendingLive::fixture(
            config.clone(),
            Snapshot::default(),
            live_result,
            16_000,
        );
        let result = transcribe_configured(
            &[0.1; 16_000],
            &Snapshot::default(),
            Some(live),
            &config,
            &mut |_, _| panic!("valid Grok streaming must not resend the audio over HTTP"),
            &mut |_, _, _, _| panic!("must not start another WebSocket"),
            &mut |_| panic!("must not retry"),
        )
        .unwrap();
        assert_eq!(result.text, "Nome correto");
        let report = result.report.unwrap();
        assert!(report.failed.is_empty());
        assert_eq!(report.executions.len(), 1);
        assert!(report.executions[0].streaming);
        assert_eq!(report.executions[0].provider, "grok");
        assert_eq!(report.executions[0].outcome, "success");
        assert_eq!(report.executions[0].keyword_count, 2);
        assert_eq!(report.executions[0].cost_usd, None);
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        let telemetry = samples[0].telemetry.as_ref().unwrap();
        assert_eq!(telemetry.attempts.len(), 1);
        assert_eq!(telemetry.attempts[0].mode, RequestMode::Live);
        assert!(telemetry.attempts[0].success);
        assert!(!telemetry.live_recovered && !telemetry.used_fallback && !telemetry.retried);
    }

    #[test]
    fn grok_segments_without_terminal_confirmation_still_recover_from_recording() {
        take_samples();
        let config = telemetry_config(&["grok::grok-voice-transcribe-2.0"]);
        let incomplete = streaming::fixture_grok_live_result(
            &[
                json!({"type":"transcript.partial","is_final":true,"speech_final":true,"start":0.0,"duration":0.8,"text":"Incomplete utterance"}),
            ],
            &[],
        );
        assert!(incomplete.is_err());
        let live = streaming::PendingLive::fixture(
            config.clone(),
            Snapshot::default(),
            incomplete,
            16_000,
        );
        let mut calls = 0;
        let result = transcribe_configured(
            &[0.1; 16_000],
            &Snapshot::default(),
            Some(live),
            &config,
            &mut |request, _| {
                calls += 1;
                assert_eq!(request.provider, Provider::Grok);
                (
                    Ok(response(200, r#"{"text":"Complete recording"}"#, None)),
                    Duration::from_millis(30),
                )
            },
            &mut |_, _, _, _| panic!("Grok supports HTTP recovery"),
            &mut |_| panic!("no retry"),
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(result.text, "Complete recording");
        let report = result.report.unwrap();
        assert_eq!(report.executions.len(), 2);
        assert!(report.executions[0].streaming);
        assert_eq!(report.executions[0].outcome, "failed");
        assert!(!report.executions[1].streaming);
        assert_eq!(report.executions[1].outcome, "success");
    }

    #[test]
    fn final_vad_discards_live_hallucinations_without_hiding_sent_audio() {
        take_samples();
        let result = transcribe(
            &[0.0; 1600],
            &Snapshot::default(),
            Some(live_fixture("hallucinated text", 800, true)),
        )
        .unwrap();
        assert!(result.text.is_empty());
        assert!(result.report.is_none());
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert!(samples[0].skipped_silent);
        assert_eq!(samples[0].sent_ms, 50);
        assert!(samples[0].telemetry.as_ref().unwrap().attempts.is_empty());
    }

    #[test]
    fn cancellation_never_resolves_or_retries_live_work() {
        take_samples();
        let live = live_fixture("must not be returned", 1600, false);
        live.cancellation_flag().store(true, Ordering::Release);
        assert!(transcribe(&[0.1; 1600], &Snapshot::default(), Some(live)).is_err());
        assert!(take_samples().is_empty());
    }

    #[test]
    fn blank_live_result_is_a_failed_attempt_instead_of_a_successful_empty_dictation() {
        take_samples();
        let result = transcribe(
            &[0.1; 1600],
            &Snapshot::default(),
            Some(live_fixture(" ", 1600, false)),
        );
        assert!(result.is_err());
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].words, None);
        assert!(
            samples[0]
                .failures
                .iter()
                .any(|failure| failure.kind == ErrorKind::InvalidResponse)
        );
    }
}
