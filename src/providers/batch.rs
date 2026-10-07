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
use crate::openrouter::stats::{self, ErrorKind, Failure, ModelLatency, Sample};
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
    }
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn multipart(fields: &[(&str, String)], wav: &[u8]) -> (String, Vec<u8>) {
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
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes());
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
struct TransportFailure(ErrorKind);

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

fn send(request: &Request, config: &Config, timeout: Duration) -> Result<Response> {
    crate::openrouter::http::validate_url(&request.url)
        .map_err(|_| TransportFailure(ErrorKind::Rejected))?;
    if cfg!(test) {
        bail!("HTTP transcription is disabled in unit tests; inject a transport.");
    }
    if request.body.len() > MAX_REQUEST_BYTES {
        bail!("Transcription request is too large.");
    }
    let key =
        keys::api_key(request.provider, config).map_err(|_| TransportFailure(ErrorKind::Auth))?;
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
        .map_err(|error| {
            TransportFailure(match error {
                ureq::Error::Timeout(_) => ErrorKind::Timeout,
                ureq::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    ErrorKind::Timeout
                }
                _ => ErrorKind::Network,
            })
        })?;
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
        .map_err(|_| TransportFailure(ErrorKind::InvalidResponse))?;
    Ok(Response {
        status,
        retry_after,
        body,
    })
}

fn parse(provider: Provider, body: &[u8]) -> Result<(String, Usage)> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| eyre!("The provider returned invalid transcription JSON."))?;
    if value.get("error").is_some() || value.get("err_code").is_some() {
        bail!("The provider returned a transcription error.");
    }
    let text = match provider {
        Provider::Deepgram => value.pointer("/results/channels/0/alternatives/0/transcript"),
        _ => value.get("text"),
    }
    .and_then(Value::as_str)
    .ok_or_else(|| eyre!("The provider returned no transcription text."))?;
    if text.trim().is_empty() {
        bail!("The provider returned empty transcription text.");
    }
    let usage = value.get("usage");
    Ok((
        text.trim().to_owned(),
        Usage {
            tokens: usage
                .and_then(|usage| usage.get("total_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            cost_usd: usage
                .and_then(|usage| usage.get("cost"))
                .and_then(Value::as_f64)
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .unwrap_or_default(),
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
        Provider::Deepgram | Provider::ElevenLabs => body.contains("keyterm"),
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
}

impl Progress {
    fn execution(
        &mut self,
        model: ModelRef<'_>,
        streaming: bool,
        keyword_count: usize,
        success: bool,
    ) {
        self.executions.push(ExecutionReport {
            provider: model.provider.id().into(),
            model: model.model.into(),
            streaming,
            keyword_count,
            outcome: if success { "success" } else { "failed" }.into(),
        });
    }
    fn fail(&mut self, id: &str, kind: ErrorKind, detail: &str) {
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
        for id in self
            .config
            .transcription
            .models
            .iter()
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
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
                            progress.execution(model, false, result.keyword_count, true);
                            return Ok(Success {
                                text: result.text.trim().into(),
                                model: id.into(),
                                usage: Usage::default(),
                                latency_ms: result.latency_ms,
                            });
                        }
                        Ok(result) => {
                            progress.execution(model, false, result.keyword_count, false);
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
                            progress.execution(
                                model,
                                false,
                                live_error.map_or(0, |error| error.keyword_count),
                                false,
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
                                status.map_or(ErrorKind::Network, ErrorKind::from_status),
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
                let request = request(
                    self.config,
                    model,
                    &options,
                    &wav,
                    self.vocabulary,
                    self.hints,
                    without_hints,
                )?;
                let (response, latency) = send(&request, timeout);
                spent = spent.saturating_add(latency);
                check_cancelled(self.cancelled)?;
                match response {
                    Ok(response) if response.is_success() => {
                        match parse(model.provider, &response.body) {
                            Ok((text, usage)) => {
                                progress.execution(model, false, request.keyword_count, true);
                                return Ok(Success {
                                    text,
                                    model: id.into(),
                                    usage,
                                    latency_ms: latency.as_millis() as u64,
                                });
                            }
                            Err(_) => {
                                progress.execution(model, false, request.keyword_count, false);
                                progress.fail(
                                    id,
                                    ErrorKind::InvalidResponse,
                                    "invalid transcription response",
                                );
                            }
                        }
                    }
                    Ok(response) => {
                        progress.execution(model, false, request.keyword_count, false);
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
                        progress.execution(model, false, 0, false);
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
    if config
        .transcription
        .models
        .iter()
        .all(|id| id.trim().is_empty())
    {
        return Err(ChainFailure {
            failures: Vec::new(),
        }
        .into());
    }
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
                progress.execution(
                    ModelRef::parse(&result.model),
                    true,
                    result.keyword_count,
                    true,
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
                progress.execution(
                    ModelRef::parse(&model_id),
                    true,
                    result.keyword_count,
                    false,
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
                progress.execution(
                    ModelRef::parse(&model_id),
                    true,
                    live_error.map_or(0, |error| error.keyword_count),
                    false,
                );
                progress.fail(
                    &model_id,
                    status.map_or(ErrorKind::Network, ErrorKind::from_status),
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
            HintPlan::cached(&config, &vocabulary)
        };
        let chain = Chain {
            config: &config,
            vocabulary: &vocabulary,
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
            );
            match success {
                Ok(success) => {
                    usage.tokens += success.usage.tokens;
                    usage.cost_usd += success.usage.cost_usd;
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
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocabulary::Vocabulary;

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
                        Err(TransportFailure(ErrorKind::Auth).into())
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
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].words, Some(2));
        assert_eq!(samples[0].latency_ms, 425);
        assert_eq!(
            samples[0].model_latency["openai::gpt-live-transcribe"].total_ms,
            425
        );
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
