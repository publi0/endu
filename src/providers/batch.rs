//! One bounded, ordered transcription chain across direct providers and OpenRouter.
//! Requests, credentials, vocabulary and remote bodies never become diagnostics.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{bail, eyre};
use serde_json::{Value, json};

use super::http_pool::{AGENT, HttpPool, POOL_IDLE, origin};
use super::pricing::{self, Billing};
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
    /// The body carries the Ogg Opus encoding rather than the WAV.
    opus: bool,
}

/// The clip offered to a request: always the WAV, plus its Ogg Opus encoding
/// when one exists. `request` sends Opus only where `accepts_opus` allows it.
#[derive(Clone, Copy)]
struct Upload<'a> {
    wav: &'a [u8],
    opus: Option<&'a [u8]>,
}

impl<'a> Upload<'a> {
    fn wav(bytes: &'a [u8]) -> Self {
        Self {
            wav: bytes,
            opus: None,
        }
    }

    /// The same clip with Opus dropped for models that do not accept it.
    fn for_model(self, model: ModelRef<'_>) -> Self {
        Self {
            wav: self.wav,
            opus: self.opus.filter(|_| accepts_opus(model)),
        }
    }

    fn bytes(self) -> &'a [u8] {
        self.opus.unwrap_or(self.wav)
    }

    fn is_opus(self) -> bool {
        self.opus.is_some()
    }

    fn filename(self) -> &'static str {
        if self.is_opus() {
            "clip.ogg"
        } else {
            "clip.wav"
        }
    }

    fn mime(self) -> &'static str {
        if self.is_opus() {
            "audio/ogg"
        } else {
            "audio/wav"
        }
    }
}

/// Models that accepted Ogg Opus uploads in a paid comparison against WAV
/// (docs/experiments/opus-upload-2026-10-21.md). OpenAI accepts it although
/// its guide lists no Ogg. OpenRouter depends on the routed provider, so only
/// the tested MAI route qualifies; Meta rejects anything but WAV.
fn accepts_opus(model: ModelRef<'_>) -> bool {
    match model.provider {
        Provider::Deepgram
        | Provider::ElevenLabs
        | Provider::Grok
        | Provider::Google
        | Provider::OpenAi => true,
        Provider::OpenRouter => model.model == "microsoft/mai-transcribe-2",
        Provider::Meta => false,
    }
}

/// Models that rejected an Opus upload in this process; they receive the WAV
/// directly afterwards. Bounded: the oldest remembered model gives way to a
/// new one, in insertion order.
const MAX_OPUS_REJECTIONS: usize = 64;
type OpusRejections = std::collections::VecDeque<String>;

/// What this process learned about each model's Opus uploads: models that
/// refused it, and models whose Opus upload already returned text. Each list
/// is bounded the same way.
struct OpusMemory {
    rejected: OpusRejections,
    confirmed: OpusRejections,
}

impl OpusMemory {
    const fn new() -> Self {
        Self {
            rejected: OpusRejections::new(),
            confirmed: OpusRejections::new(),
        }
    }
}

#[cfg(not(test))]
static OPUS_MEMORY: std::sync::Mutex<OpusMemory> = std::sync::Mutex::new(OpusMemory::new());
#[cfg(test)]
thread_local! {
    // Per test thread, so one test's outcome never changes another's uploads.
    static OPUS_MEMORY: std::cell::RefCell<OpusMemory> =
        const { std::cell::RefCell::new(OpusMemory::new()) };
}

fn with_opus_memory<T>(f: impl FnOnce(&mut OpusMemory) -> T) -> T {
    #[cfg(not(test))]
    return f(&mut OPUS_MEMORY
        .lock()
        .unwrap_or_else(|error| error.into_inner()));
    #[cfg(test)]
    OPUS_MEMORY.with(|memory| f(&mut memory.borrow_mut()))
}

#[cfg(test)]
fn with_opus_rejections<T>(f: impl FnOnce(&mut OpusRejections) -> T) -> T {
    with_opus_memory(|memory| f(&mut memory.rejected))
}

fn remember_model(models: &mut OpusRejections, id: &str) {
    if models.iter().any(|known| known == id) {
        return;
    }
    if models.len() >= MAX_OPUS_REJECTIONS {
        models.pop_front();
    }
    models.push_back(id.to_owned());
}

fn opus_refused(id: &str) -> bool {
    with_opus_memory(|memory| memory.rejected.iter().any(|known| known == id))
}

fn remember_opus_rejection(id: &str) {
    with_opus_memory(|memory| remember_model(&mut memory.rejected, id));
}

/// Whether an Opus upload to this model has already returned text, so the
/// model is known to decode it: an empty answer is then the real outcome.
fn opus_confirmed(id: &str) -> bool {
    with_opus_memory(|memory| memory.confirmed.iter().any(|known| known == id))
}

fn remember_opus_confirmed(id: &str) {
    with_opus_memory(|memory| remember_model(&mut memory.confirmed, id));
}

#[cfg(test)]
type Encoder = fn(&[f32]) -> Option<Vec<u8>>;
#[cfg(test)]
thread_local! {
    static OPUS_ENCODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAKE_OPUS: std::cell::Cell<Option<Encoder>> =
        const { std::cell::Cell::new(None) };
}

/// The clip's Ogg Opus encoding. Tests count calls and may inject an encoder,
/// so chain logic is covered where no system encoder exists.
fn encode_opus(samples: &[f32]) -> Option<Vec<u8>> {
    #[cfg(test)]
    {
        OPUS_ENCODES.with(|count| count.set(count.get() + 1));
        if let Some(fake) = FAKE_OPUS.with(std::cell::Cell::get) {
            return fake(samples);
        }
    }
    super::opus::encode(samples)
}

/// Whether an error answers the Ogg Opus container rather than another
/// field. 415 always does; 400/422 only when the body names the audio format
/// or its decoding, so an unrelated validation error costs no second upload.
/// The body is reduced to lowercase words and every pattern must match whole
/// words in sequence, so `invalid_file_format` and `content-type` match while
/// "payload decoded size" or "Base64 decoding failed" (a request-encoding
/// error) do not. Bare "invalid audio" and "decode" are deliberately absent:
/// they also describe content ("too short", "no speech") and JSON payload
/// errors. The body is never logged or kept.
fn opus_rejected(request: &Request, response: &Response) -> bool {
    const FORMAT: &[&str] = &[
        "ogg",
        "opus",
        "codec",
        "codecs",
        "mime",
        "mimetype",
        "filetype",
        "audio format",
        "file format",
        "container format",
        "unsupported format",
        "unsupported audio",
        "unsupported media",
        "corrupt or unsupported data",
        "could not decode audio",
        "could not decode the audio",
        "failed to decode audio",
        "failed to decode the audio",
        "unable to decode audio",
        "unable to decode the audio",
        "audio decoding",
        "file type",
        "content type",
        "media type",
        "mime type",
    ];
    if !request.opus {
        return false;
    }
    match response.status {
        415 => true,
        400 | 422 => {
            let body = String::from_utf8_lossy(&response.body).to_ascii_lowercase();
            let words: Vec<_> = body
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|word| !word.is_empty())
                .collect();
            let words = format!(" {} ", words.join(" "));
            FORMAT
                .iter()
                .any(|pattern| words.contains(&format!(" {pattern} ")))
        }
        _ => false,
    }
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
    audio: Upload<'_>,
    vocabulary: &Snapshot,
    hints: &HintPlan,
    without_hints: bool,
) -> Result<Request> {
    let terms = if without_hints {
        Vec::new()
    } else {
        keywords(model, vocabulary)
    };
    let audio = audio.for_model(model);
    match model.provider {
        Provider::OpenRouter => {
            let mut body = json!({ "model": model.model, "input_audio": {"data": encode_base64(audio.bytes()), "format": if audio.is_opus() { "ogg" } else { "wav" }} });
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
                opus: audio.is_opus(),
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
            let (content_type, body) = multipart(&fields, audio);
            Ok(Request {
                provider: model.provider,
                url: "https://api.openai.com/v1/audio/transcriptions".into(),
                content_type,
                body,
                keyword_count: count,
                opus: audio.is_opus(),
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
                content_type: audio.mime().into(),
                body: audio.bytes().to_vec(),
                keyword_count: terms.len(),
                opus: audio.is_opus(),
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
            let (content_type, body) = multipart(&fields, audio);
            Ok(Request {
                provider: model.provider,
                url: "https://api.elevenlabs.io/v1/speech-to-text".into(),
                content_type,
                body,
                keyword_count: terms.len(),
                opus: audio.is_opus(),
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
            let (content_type, body) = multipart(&fields, audio);
            Ok(Request {
                provider: model.provider,
                url: "https://api.x.ai/v1/stt".into(),
                content_type,
                body,
                keyword_count: terms.len(),
                opus: audio.is_opus(),
            })
        }
        Provider::Meta => {
            let settings = super::meta::settings(model.model, options, "WAV", &terms)?;
            let (content_type, body) = multipart_audio_typed(
                &[("request", settings.to_string())],
                audio,
                "audio",
                Some("application/json"),
            );
            Ok(Request {
                provider: model.provider,
                url: "https://api.meta.ai/v1/asr/transcribe".into(),
                content_type,
                body,
                keyword_count: terms.len(),
                opus: audio.is_opus(),
            })
        }
        Provider::Google => {
            // https://ai.google.dev/gemini-api/docs/transcribe and /api/interactions-api
            // `store: false` keeps Google from persisting the Interaction. Audio
            // goes inline; never upload it as a stored file.
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
                "input": [{"type":"audio", "data":encode_base64(audio.bytes()), "mime_type":audio.mime()}],
                "generation_config": {"transcription_config":transcription}
            });
            Ok(Request {
                provider: model.provider,
                url: "https://generativelanguage.googleapis.com/v1beta/interactions".into(),
                content_type: "application/json".into(),
                body: body.to_string().into_bytes(),
                keyword_count: terms.len(),
                opus: audio.is_opus(),
            })
        }
    }
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn multipart(fields: &[(&str, String)], audio: Upload<'_>) -> (String, Vec<u8>) {
    multipart_audio(fields, audio, "file")
}

fn multipart_audio(
    fields: &[(&str, String)],
    audio: Upload<'_>,
    audio_field: &str,
) -> (String, Vec<u8>) {
    multipart_audio_typed(fields, audio, audio_field, None)
}

fn multipart_audio_typed(
    fields: &[(&str, String)],
    audio: Upload<'_>,
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
            && !audio
                .bytes()
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
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{audio_field}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n", audio.filename(), audio.mime()).as_bytes());
    body.extend_from_slice(audio.bytes());
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
    post(&AGENT, request, header, &authorization, timeout)
}

/// The network half of `send`, after URL validation and key resolution; tests
/// reach it only with loopback servers and a dummy header. A request that may
/// have been handed a pooled socket and failed while the request was still
/// being written (the socket was dropped by the server or a network change)
/// is repeated once on a new connection with the same credential, within what
/// remains of `timeout`. The server never had the whole request, so nothing it
/// could have transcribed or billed is sent twice. A failure while waiting for
/// the response, once the whole request may have arrived, is never repeated.
fn post(
    pool: &HttpPool,
    request: &Request,
    header: &str,
    authorization: &str,
    timeout: Duration,
) -> Result<Response> {
    let started = Instant::now();
    let origin = origin(&request.url);
    let checkout = pool.checkout(origin.as_deref());
    // A request to an origin this agent never pooled opens a new connection,
    // so a failure there is not a stale socket and is not repeated.
    let mut may_retry = checkout.pooled;
    let mut idle_age = POOL_IDLE;
    let mut response = loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        let mut body = std::io::Cursor::new(request.body.as_slice());
        let sent = checkout
            .agent
            .post(&request.url)
            .config()
            // Retain OpenRouter-compatible HTTP localhost endpoints; direct provider
            // URLs are fixed HTTPS constants and never configurable through a profile.
            .https_only(request.provider != Provider::OpenRouter)
            .timeout_global(Some(remaining))
            .max_idle_age(idle_age)
            .build()
            .header(header, authorization)
            .header("Content-Type", request.content_type.as_str())
            .header("Content-Length", request.body.len())
            .header("X-Title", "Endu")
            .send(ureq::SendBody::from_reader(&mut body));
        // ureq reads the body only to write it, so an unread remainder proves
        // the request was cut off while it was being sent.
        let unsent = body.position() < request.body.len() as u64;
        match sent {
            Ok(response) => break response,
            Err(error)
                if may_retry
                    && dropped_while_sending(&error, unsent)
                    && started.elapsed() < timeout =>
            {
                may_retry = false;
                // Zero discards this origin's pooled sockets (ureq drops each
                // one it skips) and forces a new connection.
                idle_age = Duration::ZERO;
            }
            Err(error) => return Err(TransportFailure(http_error_kind(&error, false), 0).into()),
        }
    };
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
    pool.used(&checkout, origin.as_deref());
    Ok(Response {
        status,
        retry_after,
        body,
    })
}

/// A socket that failed under the request before the server could have read
/// all of it: the signature of a kept-alive connection the server or a network
/// change dropped. Write-side errors qualify on their own. A reset qualifies
/// only while part of the body was still unsent (`unsent`), since a reset or
/// EOF while awaiting the response may follow a request the server fully
/// received; so does EINVAL, which macOS returns when ureq updates the write
/// timeout of a socket that was reset mid-body. Timeouts are not included:
/// the global attempt budget is already spent.
fn dropped_while_sending(error: &ureq::Error, unsent: bool) -> bool {
    let ureq::Error::Io(error) = error else {
        return false;
    };
    match error.kind() {
        std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::NotConnected
        | std::io::ErrorKind::ConnectionAborted => true,
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::InvalidInput => unsent,
        _ => false,
    }
}

/// Bounds the TLS warm-up; a slow host simply stays cold.
const PREWARM_TIMEOUT: Duration = Duration::from_secs(5);
/// A HEAD answer has no body; anything larger is not drained into the pool.
const PREWARM_BODY_LIMIT: u64 = 64 * 1024;
static PREWARMING: AtomicBool = AtomicBool::new(false);

/// Opens the connection for the recorded upload while the user speaks, so the
/// request after Finish skips TCP and TLS (ureq still resolves DNS for every
/// request). It sends a bare HEAD to the provider's origin: no credentials,
/// audio or vocabulary. A primary that streams live already opens its own
/// socket and needs no warm-up. The caller (the audio owner) only claims the
/// single flight and spawns; configuration, key status and the request run on
/// the warm-up thread.
pub fn prewarm() {
    spawn_prewarm(upload_origin_unless_live);
}

/// Warms the first recorded upload after a live session failed or was
/// invalidated during capture, so the recovery request after Finish skips
/// TCP and TLS. Same single flight and thread as `prewarm`.
pub fn prewarm_recovery() {
    spawn_prewarm(recorded_upload_origin);
}

type OriginSelector = fn(&Config, &dyn Fn(Provider) -> bool) -> Option<PrewarmOrigin>;

fn spawn_prewarm(select: OriginSelector) {
    if cfg!(test) {
        return;
    }
    let Some(flight) = PrewarmFlight::acquire() else {
        return;
    };
    // A failed spawn drops the closure and, with it, the flight.
    let _ = std::thread::Builder::new()
        .name("endu-prewarm".into())
        .spawn(move || {
            let _flight = flight;
            let Some(config) = super::runtime_config() else {
                return;
            };
            // Cached state or Keychain attributes only, as the selectors use;
            // a warm-up never resolves or decrypts a key.
            let has_key = |provider| {
                !matches!(
                    keys::key_status(provider, &config),
                    crate::openrouter::KeyStatus::Missing
                )
            };
            if let Some(origin) = select(&config, &has_key) {
                warm(&AGENT, &origin);
            }
        });
}

fn warm(pool: &HttpPool, target: &PrewarmOrigin) {
    let origin = origin(&target.url);
    let checkout = pool.checkout(origin.as_deref());
    let response = checkout
        .agent
        .head(&target.url)
        .config()
        .https_only(target.https_only)
        .timeout_global(Some(PREWARM_TIMEOUT))
        .build()
        .call();
    // Draining the empty body returns the connection to the pool.
    if let Ok(mut response) = response
        && response
            .body_mut()
            .with_config()
            .limit(PREWARM_BODY_LIMIT)
            .read_to_vec()
            .is_ok()
    {
        pool.used(&checkout, origin.as_deref());
    }
}

/// Owns `PREWARMING` for one warm-up; released on drop, including a panic.
struct PrewarmFlight(());

impl PrewarmFlight {
    fn acquire() -> Option<Self> {
        (!PREWARMING.swap(true, Ordering::AcqRel)).then_some(Self(()))
    }
}

impl Drop for PrewarmFlight {
    fn drop(&mut self) {
        PREWARMING.store(false, Ordering::Release);
    }
}

/// Providers that drop an idle kept-alive socket within seconds. Deepgram
/// closes it after ~5 s, so a socket warmed while the user speaks is usually
/// gone by Finish; warming it would only add a request per dictation.
fn closes_idle_quickly(provider: Provider) -> bool {
    provider == Provider::Deepgram
}

#[derive(Debug, PartialEq)]
struct PrewarmOrigin {
    url: String,
    https_only: bool,
}

/// The origin of the first recorded upload when no live session opens. The
/// primary is read exactly as `LiveCapture::start` reads it.
fn upload_origin_unless_live(
    config: &Config,
    has_key: &dyn Fn(Provider) -> bool,
) -> Option<PrewarmOrigin> {
    if streaming::live_primary(config).is_some() {
        return None;
    }
    recorded_upload_origin(config, has_key)
}

/// The origin of the first HTTP upload the chain makes for a completed clip,
/// in chain order, skipping slots it would skip or answer without HTTP: blank,
/// invalid options, live-only (a completed WebSocket) and missing keys. The
/// URL comes from the same request builder the chain uses.
fn recorded_upload_origin(
    config: &Config,
    has_key: &dyn Fn(Provider) -> bool,
) -> Option<PrewarmOrigin> {
    let (id, options) = config
        .transcription
        .models
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(|id| (id, super::options(config, id)))
        .find(|(id, options)| {
            let model = ModelRef::parse(id);
            model.capabilities().batch && options.validate().is_ok() && has_key(model.provider)
        })?;
    let model = ModelRef::parse(id);
    if closes_idle_quickly(model.provider) {
        return None;
    }
    let request = request(
        config,
        model,
        &options,
        Upload::wav(&[]),
        &Snapshot::default(),
        &HintPlan::default(),
        true,
    )
    .ok()?;
    crate::openrouter::http::validate_url(&request.url).ok()?;
    let url = url::Url::parse(&request.url).ok()?;
    Some(PrewarmOrigin {
        url: format!("{}/", url.origin().ascii_serialization()),
        https_only: model.provider != Provider::OpenRouter,
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
    /// Audio and language of the request being recorded, for cost estimates.
    billing: Billing,
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
        let mode = if streaming {
            RequestMode::Live
        } else {
            RequestMode::Recorded
        };
        self.executions.push(ExecutionReport {
            provider: model.provider.id().into(),
            model: model.model.into(),
            streaming,
            keyword_count,
            outcome: if success { "success" } else { "failed" }.into(),
            cost_usd,
            estimated_cost_usd: self.estimate(model, mode, keyword_count, success, cost_usd),
        });
    }
    /// Only successful requests without a reported cost are estimated: a
    /// failed request's billing is unknown, so it stays "not reported".
    fn estimate(
        &self,
        model: ModelRef<'_>,
        mode: RequestMode,
        keyword_count: usize,
        success: bool,
        reported: Option<f64>,
    ) -> Option<f64> {
        (success && reported.is_none())
            .then(|| pricing::estimate(model, mode, self.billing, keyword_count))
            .flatten()
    }
    fn bill(&mut self, audio_ms: u64, options: &ModelOptions) {
        self.billing = Billing {
            audio_ms,
            auto_language: options.language.trim().is_empty() || options.language == AUTO_LANGUAGE,
        };
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
        let estimated_cost_usd =
            self.estimate(model, mode, keyword_count, outcome.is_ok(), cost_usd);
        self.telemetry.attempts.push(AttemptSample {
            model: model.key(),
            mode,
            success: outcome.is_ok(),
            error: outcome.err(),
            latency_ms: outcome.ok(),
            keyword_count,
            cost_usd,
            estimated_cost_usd,
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
    /// What remains of the chain deadline: `spent` adds measured request,
    /// encode and wait time to `initial_spent`; wall time covers the rest.
    fn remaining(&self, started: Instant, initial_spent: Duration, spent: Duration) -> Duration {
        self.config
            .total_timeout()
            .saturating_sub(initial_spent.saturating_add(started.elapsed()).max(spent))
    }

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
        // These samples' Opus encoding, made on the first model that takes it.
        let opus = std::cell::OnceCell::new();
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
            progress.bill(duration_ms(samples.len()), &options);
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
            // Set once this model's Opus upload was answered: every later
            // request in this loop sends the WAV, whatever the shared set says.
            let mut opus_retried = false;
            // A 2xx Opus answer without text: remember the model as refusing
            // Opus only if the WAV resend then transcribes.
            let mut opus_unconfirmed = false;
            loop {
                check_cancelled(self.cancelled)?;
                let remaining = self.remaining(started, initial_spent, spent);
                if remaining.is_zero() {
                    progress.fail(id, ErrorKind::Timeout, "not tried, chain deadline reached");
                    break;
                }
                let mut timeout = self.config.attempt_timeout().min(remaining);
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
                            let available = self.remaining(started, initial_spent, spent);
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
                let build = |audio| {
                    request(
                        self.config,
                        model,
                        &options,
                        audio,
                        self.vocabulary,
                        self.hints,
                        without_hints,
                    )
                };
                // Validate the request without audio first, so an invalid
                // endpoint or option never costs an encode.
                let valid = build(Upload::wav(&[]));
                let mut audio = Upload::wav(&wav);
                if valid.is_ok() && accepts_opus(model) && !opus_retried && !opus_refused(id) {
                    // The encode counts against this attempt's deadline as
                    // well as the chain's.
                    let at = Instant::now();
                    audio.opus = opus.get_or_init(|| encode_opus(samples)).as_deref();
                    let encoding = at.elapsed();
                    spent = spent.saturating_add(encoding);
                    check_cancelled(self.cancelled)?;
                    let remaining = self.remaining(started, initial_spent, spent);
                    timeout = self
                        .config
                        .attempt_timeout()
                        .saturating_sub(encoding)
                        .min(remaining);
                    if timeout.is_zero() {
                        progress.fail(
                            id,
                            ErrorKind::Timeout,
                            "not tried, deadline reached while encoding audio",
                        );
                        break;
                    }
                }
                let request = match valid.and_then(|_| build(audio)) {
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
                                    rate_retried || without_hints || opus_retried,
                                );
                                progress.telemetry.used_fallback |= model_index > 0;
                                progress.execution(model, false, request.keyword_count, true, cost);
                                if opus_unconfirmed {
                                    remember_opus_rejection(id);
                                } else if request.opus {
                                    remember_opus_confirmed(id);
                                }
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
                                    rate_retried || without_hints || opus_retried,
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
                                // Any 2xx the parser rejects (empty text,
                                // malformed body) for an Opus clip may be a
                                // mis-decode: resend the WAV once on the same
                                // model. A model whose Opus upload already
                                // returned text decodes it, so its empty
                                // answer is the real outcome and is not
                                // billed twice.
                                if request.opus && !opus_retried && !opus_confirmed(id) {
                                    opus_retried = true;
                                    opus_unconfirmed = true;
                                    continue;
                                }
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
                            rate_retried || without_hints || opus_retried,
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
                        if opus_rejected(&request, &response) {
                            progress.fail(
                                id,
                                ErrorKind::from_status(response.status),
                                &format!("HTTP {}: compressed audio rejected", response.status),
                            );
                            remember_opus_rejection(id);
                            opus_retried = true;
                            continue;
                        }
                        let wait = response.retry_after.unwrap_or(Duration::from_secs(1));
                        let available = self.remaining(started, initial_spent, spent);
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
                                rate_retried || without_hints || opus_retried,
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
    // Returning without a sample (cancellation) leaves streamed audio unaccounted,
    // so the session reports it as discarded when its last owner drops.
    let account = live.as_ref().map(streaming::PendingLive::account);
    let record_sample = |sample: Sample| {
        if let Some(account) = &account {
            account.settle();
        }
        record_sample(sample);
    };
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
        let streamed = live.is_some();
        let sent_ms = duration_ms(
            live.as_ref()
                .map_or(0, streaming::PendingLive::sent_samples),
        );
        // Dropping the live session stops it at once.
        drop(live);
        record_sample(Sample {
            // Streamed silence was already sent, so it counts as a failure.
            skipped_silent: !streamed,
            recorded_ms,
            sent_ms,
            telemetry: Some(DictationTelemetry::default()),
            ..Sample::default()
        });
        if streamed {
            return Err(crate::openrouter::transcribe::NoSpeech.into());
        }
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
                progress.bill(sent_ms, &super::options(config, &result.model));
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
        ..Sample::default()
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
    use std::time::SystemTime;

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
                Upload::wav(&wav),
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
    fn prewarm_targets_the_first_recorded_upload_origin_only() {
        let mut grok = config(&["grok::grok-voice-transcribe-2.0", "deepgram::nova-3"]);
        grok.transcription.model_options.insert(
            ModelRef::parse("grok::grok-voice-transcribe-2.0").key(),
            ModelOptions {
                language: "pt".into(),
                streaming: false,
                ..Default::default()
            },
        );
        assert_eq!(
            upload_origin_unless_live(&grok, &|_| true),
            Some(PrewarmOrigin {
                url: "https://api.x.ai/".into(),
                https_only: true,
            })
        );

        // A live primary opens its own socket; the fallback is not warmed.
        grok.transcription.model_options.insert(
            ModelRef::parse("grok::grok-voice-transcribe-2.0").key(),
            ModelOptions {
                language: "pt".into(),
                streaming: true,
                ..Default::default()
            },
        );
        assert_eq!(upload_origin_unless_live(&grok, &|_| true), None);
        assert_eq!(
            upload_origin_unless_live(&config(&["elevenlabs::scribe_v2_realtime"]), &|_| true),
            None
        );
        assert_eq!(
            upload_origin_unless_live(&config(&[" ", ""]), &|_| true),
            None
        );

        // The OpenRouter origin follows the configured, validated base URL.
        let mut routed = config(&["microsoft/mai-transcribe-2"]);
        assert_eq!(
            upload_origin_unless_live(&routed, &|_| true).map(|origin| origin.url),
            Some("https://openrouter.ai/".into())
        );
        routed.base_url = "http://127.0.0.1:8080/api/v1".into();
        assert_eq!(
            upload_origin_unless_live(&routed, &|_| true),
            Some(PrewarmOrigin {
                url: "http://127.0.0.1:8080/".into(),
                https_only: false,
            })
        );
        routed.base_url = "http://localhost.evil/api/v1".into();
        assert_eq!(upload_origin_unless_live(&routed, &|_| true), None);
    }

    #[test]
    fn prewarm_and_live_capture_read_the_same_primary() {
        let cases = [
            config(&[" ", "grok::grok-voice-transcribe-2.0"]),
            config(&["grok::grok-voice-transcribe-2.0", "openai::whisper-1"]),
            config(&[
                "elevenlabs::scribe_v2_realtime",
                "microsoft/mai-transcribe-2",
            ]),
            config(&["microsoft/mai-transcribe-2"]),
            config(&["deepgram::nova-2", "openai::whisper-1"]),
        ];
        for case in &cases {
            // Never both a live socket and a warmed upload for one capture.
            assert!(
                streaming::live_primary(case).is_none()
                    || upload_origin_unless_live(case, &|_| true).is_none()
            );
        }
        // A blank first slot never streams live, so the recorded upload is
        // warmed even though the next slot could stream.
        assert_eq!(streaming::live_primary(&cases[0]), None);
        assert_eq!(
            upload_origin_unless_live(&cases[0], &|_| true).map(|origin| origin.url),
            Some("https://api.x.ai/".into())
        );
        assert_eq!(
            streaming::live_primary(&cases[1]),
            Some("grok::grok-voice-transcribe-2.0")
        );
        assert_eq!(
            upload_origin_unless_live(&cases[3], &|_| true).map(|origin| origin.url),
            Some("https://openrouter.ai/".into())
        );
        // Nova-2 with Auto uploads to Deepgram, which drops idle sockets in ~5 s.
        assert_eq!(streaming::live_primary(&cases[4]), None);
        assert_eq!(upload_origin_unless_live(&cases[4], &|_| true), None);
    }

    #[test]
    fn recovery_warms_the_first_recorded_http_upload_with_a_key() {
        let chain = config(&[
            "elevenlabs::scribe_v2_realtime",
            "microsoft/mai-transcribe-2",
            "grok::grok-voice-transcribe-2.0",
        ]);
        let all = |_: Provider| true;
        // The live-only primary answers a completed clip over a WebSocket.
        assert_eq!(upload_origin_unless_live(&chain, &all), None);
        assert_eq!(
            recorded_upload_origin(&chain, &all),
            Some(PrewarmOrigin {
                url: "https://openrouter.ai/".into(),
                https_only: false,
            })
        );
        assert_eq!(
            recorded_upload_origin(&chain, &|provider| provider != Provider::OpenRouter)
                .map(|origin| origin.url),
            Some("https://api.x.ai/".into())
        );
        assert_eq!(recorded_upload_origin(&chain, &|_| false), None);

        // A streaming primary that can upload retries itself with the recording.
        let grok = config(&[
            "grok::grok-voice-transcribe-2.0",
            "microsoft/mai-transcribe-2",
        ]);
        assert_eq!(
            recorded_upload_origin(&grok, &all).map(|origin| origin.url),
            Some("https://api.x.ai/".into())
        );
        // Deepgram would take the upload but closes idle sockets within seconds.
        let deepgram = config(&[
            "elevenlabs::scribe_v2_realtime",
            "deepgram::nova-3",
            "openai::whisper-1",
        ]);
        assert_eq!(recorded_upload_origin(&deepgram, &all), None);
    }

    #[test]
    fn prewarm_flight_is_released_when_the_warm_up_panics() {
        let flight = PrewarmFlight::acquire().expect("no other warm-up in tests");
        assert!(PrewarmFlight::acquire().is_none());
        let joined = std::thread::spawn(move || {
            let _flight = flight;
            panic!("warm-up failed");
        })
        .join();
        assert!(joined.is_err());
        assert!(PrewarmFlight::acquire().is_some());
        assert!(!PREWARMING.load(Ordering::Acquire));
    }

    /// One HTTP/1.1 request (headers plus Content-Length body); `None` at EOF.
    fn read_http_request(stream: &mut std::net::TcpStream) -> Option<Vec<u8>> {
        use std::io::Read;
        let mut data = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if data.len() >= end + 4 + length {
                    return Some(data);
                }
            }
            let read = stream.read(&mut buffer).ok()?;
            if read == 0 {
                return None;
            }
            data.extend_from_slice(&buffer[..read]);
        }
    }

    fn loopback(provider: Provider, listener: &std::net::TcpListener) -> Request {
        Request {
            provider,
            url: format!("http://{}/api/v1/audio", listener.local_addr().unwrap()),
            content_type: "audio/wav".into(),
            body: b"RIFF".to_vec(),
            keyword_count: 0,
            opus: false,
        }
    }

    /// Whether anything connected to `listener` since it was last accepted.
    fn connected(listener: &std::net::TcpListener) -> bool {
        listener.set_nonblocking(true).unwrap();
        listener.accept().is_ok()
    }

    /// Reads one request's head only, leaving any body unread in the socket.
    fn read_http_head(stream: &mut std::net::TcpStream) {
        use std::io::Read;
        let mut data = Vec::new();
        let mut byte = [0];
        while !data.ends_with(b"\r\n\r\n") {
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            data.push(byte[0]);
        }
    }

    /// `post` with a dummy bearer credential, as the loopback tests send it.
    fn post_bearer(pool: &HttpPool, request: &Request, timeout: Duration) -> Result<Response> {
        post(pool, request, "Authorization", "Bearer test", timeout)
    }

    const OK_EMPTY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const OK_TEXT: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"text\":\"ok\"}";

    /// Larger than loopback socket buffers, so the client is still writing
    /// when the server drops the connection.
    fn large_upload(listener: &std::net::TcpListener) -> Request {
        Request {
            body: vec![0; 32 << 20],
            ..loopback(Provider::OpenRouter, listener)
        }
    }

    #[test]
    fn a_pooled_socket_dropped_while_sending_is_retried_once_on_a_fresh_connection() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let warm = loopback(Provider::OpenRouter, &listener);
        let upload = large_upload(&listener);
        let server = std::thread::spawn(move || {
            let (mut kept, _) = listener.accept().unwrap();
            read_http_request(&mut kept).unwrap();
            kept.write_all(OK_EMPTY).unwrap();
            // The next request reuses the kept-alive socket, which closes with
            // the body still arriving, as a server idle close racing the
            // request would. Unread data makes the close a reset.
            read_http_head(&mut kept);
            drop(kept);
            let (mut fresh, _) = listener.accept().unwrap();
            let received = read_http_request(&mut fresh).unwrap();
            fresh.write_all(OK_TEXT).unwrap();
            (listener, received.len())
        });
        let pool = HttpPool::new();
        let timeout = Duration::from_secs(20);
        let first = post_bearer(&pool, &warm, timeout).unwrap();
        assert_eq!(first.status, 200);
        let second = post_bearer(&pool, &upload, timeout).unwrap();
        assert_eq!(
            (second.status, second.body.as_slice()),
            (200, &br#"{"text":"ok"}"#[..])
        );
        let (listener, received) = server.join().unwrap();
        assert!(
            received > upload.body.len(),
            "the retry sent the whole body"
        );
        assert!(!connected(&listener), "exactly one retry");
    }

    #[test]
    fn a_pooled_socket_closed_after_the_whole_request_is_not_retried() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let request = loopback(Provider::OpenRouter, &listener);
        let server = std::thread::spawn(move || {
            let (mut kept, _) = listener.accept().unwrap();
            read_http_request(&mut kept).unwrap();
            kept.write_all(OK_EMPTY).unwrap();
            // The server has the whole request and may have processed (and
            // billed) it; resending would upload the audio twice.
            read_http_request(&mut kept).unwrap();
            drop(kept);
            listener
        });
        let pool = HttpPool::new();
        let timeout = Duration::from_secs(10);
        post_bearer(&pool, &request, timeout).unwrap();
        let error = post_bearer(&pool, &request, timeout).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<TransportFailure>()
                .map(|failure| failure.0),
            Some(ErrorKind::Network)
        );
        let listener = server.join().unwrap();
        assert!(!connected(&listener));
    }

    #[test]
    fn a_new_connection_dropped_while_sending_is_not_retried() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upload = large_upload(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_head(&mut stream);
            drop(stream);
            listener
        });
        let error = post_bearer(&HttpPool::new(), &upload, Duration::from_secs(20)).unwrap_err();
        assert!(error.downcast_ref::<TransportFailure>().is_some());
        let listener = server.join().unwrap();
        assert!(!connected(&listener), "no pooled socket, no retry");
    }

    #[test]
    fn a_suspect_pool_discards_sockets_to_every_host() {
        use std::io::{Read, Write};
        let host_a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host_b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let to_a = loopback(Provider::OpenRouter, &host_a);
        let to_b = loopback(Provider::OpenRouter, &host_b);
        let (served, slept) = std::sync::mpsc::channel();
        let server_a = std::thread::spawn(move || {
            let (mut kept, _) = host_a.accept().unwrap();
            read_http_request(&mut kept).unwrap();
            kept.write_all(OK_EMPTY).unwrap();
            // After the sleep the request arrives on a new connection, and the
            // kept-alive socket was closed with the discarded pool.
            let (mut fresh, _) = host_a.accept().unwrap();
            read_http_request(&mut fresh).unwrap();
            fresh.write_all(OK_TEXT).unwrap();
            kept.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            assert_eq!(kept.read(&mut [0; 16]).unwrap(), 0);
            host_a
        });
        let server_b = std::thread::spawn(move || {
            let (mut kept, _) = host_b.accept().unwrap();
            read_http_request(&mut kept).unwrap();
            kept.write_all(OK_EMPTY).unwrap();
            served.send(()).unwrap();
            let (mut fresh, _) = host_b.accept().unwrap();
            read_http_request(&mut fresh).unwrap();
            fresh.write_all(OK_EMPTY).unwrap();
            host_b
        });
        let pool = HttpPool::new();
        let timeout = Duration::from_secs(10);
        post_bearer(&pool, &to_a, timeout).unwrap();
        post_bearer(&pool, &to_b, timeout).unwrap();
        slept.recv().unwrap();
        let (key_a, key_b) = (origin(&to_a.url), origin(&to_b.url));
        assert!(pool.checkout(key_a.as_deref()).pooled);
        // The Mac slept ten minutes while `Instant` stood still.
        let wall = SystemTime::now() + Duration::from_secs(600);
        let after_sleep = pool.checkout_at(key_b.as_deref(), wall, Instant::now());
        assert!(!after_sleep.pooled);
        drop(after_sleep);
        post_bearer(&pool, &to_b, timeout).unwrap();
        // Host B's fresh request must not have vouched for host A's socket.
        assert!(!pool.checkout(key_a.as_deref()).pooled);
        let response = post_bearer(&pool, &to_a, timeout).unwrap();
        assert_eq!(response.body.as_slice(), &br#"{"text":"ok"}"#[..]);
        let (host_a, host_b) = (server_a.join().unwrap(), server_b.join().unwrap());
        assert!(!connected(&host_a) && !connected(&host_b));
    }

    #[test]
    fn a_failure_after_the_response_began_is_not_retried() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let request = loopback(Provider::OpenRouter, &listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n{}")
                .unwrap();
            drop(stream);
            listener
        });
        let error = post_bearer(&HttpPool::new(), &request, Duration::from_secs(10)).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<TransportFailure>()
                .map(|failure| failure.0),
            Some(ErrorKind::InvalidResponse)
        );
        let listener = server.join().unwrap();
        assert!(!connected(&listener));
    }

    #[test]
    fn request_config_keeps_the_shared_agent_redirect_and_https_rules() {
        use std::io::Write;
        let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let target = format!("http://{}/stolen", elsewhere.local_addr().unwrap());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let request = loopback(Provider::OpenRouter, &listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream).unwrap();
            let reply =
                format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\n\r\n");
            stream.write_all(reply.as_bytes()).unwrap();
            listener
        });
        let timeout = Duration::from_secs(10);
        let response = post_bearer(&AGENT, &request, timeout).unwrap();
        // The redirect is a plain response; the credential never follows it.
        assert_eq!(response.status, 302);
        let listener = server.join().unwrap();
        assert!(!connected(&elsewhere));

        // Direct providers require HTTPS even on loopback.
        listener.set_nonblocking(false).unwrap();
        let direct = loopback(Provider::Deepgram, &listener);
        let error = post(&AGENT, &direct, "Authorization", "Token test", timeout).unwrap_err();
        assert!(error.downcast_ref::<TransportFailure>().is_some());
        assert!(!connected(&listener));
    }

    const DEEPGRAM_DONE: &str =
        r#"{"results":{"channels":[{"alternatives":[{"transcript":"done"}]}]}}"#;

    fn run_chain(
        config: &Config,
        vocabulary: &Snapshot,
        cancelled: Option<&AtomicBool>,
        progress: &mut Progress,
        send: &mut RequestSender<'_>,
    ) -> Result<Success> {
        Chain {
            config,
            vocabulary,
            hints: &HintPlan::default(),
            cancelled,
        }
        .run(
            &[0.1; 16_000],
            Duration::ZERO,
            progress,
            send,
            &mut |_, _, _, _| panic!("no completed websocket"),
            &mut |_| panic!("no sleep"),
        )
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn opus_goes_only_to_providers_that_accept_it() {
        use_encoder(false);
        let ids = [
            "openai/gpt-transcribe",
            "meta::muse-voice-transcribe-1.0",
            "microsoft/mai-transcribe-2",
            "openai::gpt-transcribe",
            "grok::grok-voice-transcribe-2.0",
        ];
        let config = config(&ids);
        let mut uploads = Vec::new();
        let success = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, _| {
                let body = String::from_utf8_lossy(&request.body).into_owned();
                uploads.push((request.provider, request.opus, body));
                let status = if request.provider == Provider::Grok {
                    200
                } else {
                    503
                };
                (
                    Ok(response(status, r#"{"text":"done"}"#, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .unwrap();
        assert_eq!(success.model, ids[4]);
        let [routed, meta, mai, openai, grok] = &uploads[..] else {
            panic!("one attempt per model");
        };
        // Other OpenRouter routes and Meta keep the WAV clip.
        assert!(!routed.1 && routed.2.contains(r#""format":"wav""#));
        assert!(
            !meta.1
                && meta
                    .2
                    .contains("filename=\"clip.wav\"\r\nContent-Type: audio/wav")
        );
        // Other platforms have no system encoder and keep the WAV clip.
        let encoded = cfg!(target_os = "macos");
        assert_eq!(mai.1, encoded);
        assert_eq!(
            mai.2.contains(r#""format":"ogg""#),
            encoded,
            "the MAI route names the Ogg container"
        );
        for (_, opus, body) in [openai, grok] {
            assert_eq!(*opus, encoded);
            if *opus {
                assert!(body.contains("filename=\"clip.ogg\"\r\nContent-Type: audio/ogg"));
                assert!(body.contains("OggS") && !body.contains("RIFF"));
            }
        }
    }

    #[test]
    fn request_sends_opus_only_where_documented_and_names_the_ogg_container() {
        let config = config(&[]);
        let upload = Upload {
            wav: b"RIFF_FIXTURE",
            opus: Some(b"OggS_FIXTURE"),
        };
        let build = |id: &str| {
            request(
                &config,
                ModelRef::parse(id),
                &ModelOptions::default(),
                upload,
                &Snapshot::default(),
                &HintPlan::default(),
                false,
            )
            .unwrap()
        };
        let deepgram = build("deepgram::nova-3");
        assert!(deepgram.opus);
        assert_eq!(deepgram.content_type, "audio/ogg");
        assert_eq!(deepgram.body, b"OggS_FIXTURE");
        for id in ["elevenlabs::scribe_v2", "grok::grok-voice-transcribe-2.0"] {
            let request = build(id);
            let body = String::from_utf8_lossy(&request.body);
            assert!(request.opus);
            assert!(body.contains(
                "filename=\"clip.ogg\"\r\nContent-Type: audio/ogg\r\n\r\nOggS_FIXTURE\r\n"
            ));
            assert!(!body.contains("RIFF"));
        }
        let openai = build("openai::gpt-transcribe");
        let body = String::from_utf8_lossy(&openai.body);
        assert!(openai.opus);
        assert!(
            body.contains(
                "filename=\"clip.ogg\"\r\nContent-Type: audio/ogg\r\n\r\nOggS_FIXTURE\r\n"
            )
        );
        assert!(!body.contains("RIFF"));
        let mai = build("microsoft/mai-transcribe-2");
        let routed: Value = serde_json::from_slice(&mai.body).unwrap();
        assert!(mai.opus);
        assert_eq!(routed["input_audio"]["format"], "ogg");
        assert_eq!(
            routed["input_audio"]["data"],
            encode_base64(b"OggS_FIXTURE")
        );
        let google = build("google::gemini-3.5-transcribe");
        let body: Value = serde_json::from_slice(&google.body).unwrap();
        assert!(google.opus);
        assert_eq!(body["input"][0]["mime_type"], "audio/ogg");
        assert_eq!(body["input"][0]["data"], encode_base64(b"OggS_FIXTURE"));

        // Handing Opus to a WAV-only model still sends the WAV, including any
        // OpenRouter route other than the tested MAI one.
        for id in ["openai/gpt-transcribe", "meta::muse-voice-transcribe-1.0"] {
            let request = build(id);
            assert!(!request.opus);
            assert!(!contains(&request.body, b"OggS"));
        }
        let routed: Value = serde_json::from_slice(&build("openai/gpt-transcribe").body).unwrap();
        assert_eq!(routed["input_audio"]["format"], "wav");
        assert_eq!(
            routed["input_audio"]["data"],
            encode_base64(b"RIFF_FIXTURE")
        );
        assert!(contains(
            &build("meta::muse-voice-transcribe-1.0").body,
            b"filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF_FIXTURE"
        ));
    }

    fn fake_opus(_: &[f32]) -> Option<Vec<u8>> {
        Some(format!("OggS_FAKE_{}", OPUS_ENCODES.with(std::cell::Cell::get)).into_bytes())
    }

    /// Installs the numbered fake encoder (or the system one) on this test
    /// thread, and clears the encode count and remembered rejections.
    fn use_encoder(fake: bool) {
        let encoder: Encoder = fake_opus;
        FAKE_OPUS.with(|cell| cell.set(fake.then_some(encoder)));
        OPUS_ENCODES.with(|count| count.set(0));
        with_opus_memory(|memory| {
            memory.rejected.clear();
            memory.confirmed.clear();
        });
    }

    fn opus_encodes() -> usize {
        OPUS_ENCODES.with(std::cell::Cell::get)
    }

    #[test]
    fn only_format_errors_count_as_an_opus_rejection() {
        let request = |opus| Request {
            provider: Provider::Deepgram,
            url: String::new(),
            content_type: String::new(),
            body: Vec::new(),
            keyword_count: 0,
            opus,
        };
        let rejected =
            |opus, status, body: &str| opus_rejected(&request(opus), &response(status, body, None));
        assert!(rejected(true, 415, "{}"));
        assert!(!rejected(false, 415, "{}"));
        let cases = [
            // Deepgram's documented wording for an undecodable upload.
            (
                r#"{"err_code":"Bad Request","err_msg":"Bad Request: failed to process audio: corrupt or unsupported data"}"#,
                true,
            ),
            (
                r#"{"detail":{"status":"invalid_file_format","message":"The uploaded file has an invalid format."}}"#,
                true,
            ),
            (r#"{"error":"Unsupported audio format"}"#, true),
            (r#"{"error":"Invalid audio format: expected wav"}"#, true),
            (r#"{"error":"audio/ogg is not supported"}"#, true),
            (
                r#"{"error":{"message":"Could not decode the audio file"}}"#,
                true,
            ),
            (r#"{"error":"failed to decode audio"}"#, true),
            (r#"{"detail":"Invalid file type"}"#, true),
            (r#"{"error":"codec not supported: opus"}"#, true),
            (
                r#"{"error":{"code":400,"message":"Unsupported MIME type: audio/ogg","status":"INVALID_ARGUMENT"}}"#,
                true,
            ),
            (r#"{"error":"Content-Type not supported"}"#, true),
            ("{}", false),
            (
                r#"{"error":{"code":400,"message":"Request contains an invalid argument.","status":"INVALID_ARGUMENT"}}"#,
                false,
            ),
            (
                r#"{"error":{"code":400,"message":"Invalid value at 'input[0].data' (TYPE_BYTES), Base64 decoding failed for \"abc\"","status":"INVALID_ARGUMENT"}}"#,
                false,
            ),
            (
                r#"{"error":"payload decoded size exceeds the limit"}"#,
                false,
            ),
            (
                r#"{"error":"Text formatting requires a supported language"}"#,
                false,
            ),
            (r#"{"error":"invalid audio: too short"}"#, false),
            (r#"{"error":"Invalid audio: no speech detected"}"#, false),
            (r#"{"detail":"Audio file is too short"}"#, false),
            (r#"{"err_msg":"Bad Request: invalid language 'xx'"}"#, false),
            (r#"{"error":"invalid response_format"}"#, false),
            (r#"{"error":"invalid option: toggle_logging"}"#, false),
            (r#"{"error":"temperature must be between 0 and 1"}"#, false),
            (r#"{"error":"corrupted request id"}"#, false),
        ];
        for (body, expected) in cases {
            for status in [400, 422] {
                assert_eq!(rejected(true, status, body), expected, "{status} {body}");
                assert!(!rejected(false, status, body), "{status} {body}");
            }
            assert!(!rejected(true, 500, body), "{body}");
        }
    }

    #[test]
    fn remembered_rejections_evict_the_oldest_model_first() {
        use_encoder(true);
        remember_opus_rejection("model-b");
        remember_opus_rejection("model-a");
        for index in 0..MAX_OPUS_REJECTIONS - 2 {
            remember_opus_rejection(&format!("fill-{index:02}"));
        }
        // Remembering a known model neither duplicates nor evicts it.
        remember_opus_rejection("model-a");
        assert_eq!(
            with_opus_rejections(|known| known.len()),
            MAX_OPUS_REJECTIONS
        );
        remember_opus_rejection("model-new");
        assert_eq!(
            with_opus_rejections(|known| known.len()),
            MAX_OPUS_REJECTIONS
        );
        assert!(!opus_refused("model-b"));
        for id in ["model-a", "fill-00", "model-new"] {
            assert!(opus_refused(id), "{id}");
        }
        remember_opus_rejection("model-newer");
        assert!(!opus_refused("model-a") && opus_refused("fill-00"));
    }

    #[test]
    fn one_encode_per_chain_across_retries_and_fallbacks() {
        use_encoder(true);
        let config = config(&[
            "openai::gpt-transcribe",
            "deepgram::nova-3",
            "elevenlabs::scribe_v2",
            "grok::grok-voice-transcribe-2.0",
        ]);
        let mut sent = Vec::<(Provider, Vec<u8>)>::new();
        let success = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, _| {
                sent.push((request.provider, request.body.clone()));
                let status = match sent.len() {
                    2 => 415,
                    5 => 200,
                    _ => 503,
                };
                (
                    Ok(response(status, r#"{"text":"done"}"#, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .unwrap();
        assert_eq!(success.model, "grok::grok-voice-transcribe-2.0");
        assert_eq!(opus_encodes(), 1);
        let providers: Vec<_> = sent.iter().map(|(provider, _)| *provider).collect();
        assert_eq!(
            providers,
            [
                Provider::OpenAi,
                Provider::Deepgram,
                Provider::Deepgram,
                Provider::ElevenLabs,
                Provider::Grok,
            ]
        );
        assert_eq!(sent[1].1, b"OggS_FAKE_1");
        assert!(sent[2].1.starts_with(b"RIFF"));
        for (_, body) in &sent[3..] {
            assert!(contains(body, b"OggS_FAKE_1") && !contains(body, b"RIFF"));
        }
    }

    #[test]
    fn no_encode_without_an_opus_provider_a_deadline_or_after_cancellation() {
        use_encoder(true);
        // The WAV-only primary answers before any Opus provider is reached.
        let config = config(&["openai/gpt-transcribe", "deepgram::nova-3"]);
        run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, _| {
                assert!(!request.opus);
                (
                    Ok(response(200, r#"{"text":"done"}"#, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .unwrap();
        assert_eq!(opus_encodes(), 0);

        // An exhausted chain deadline sends and encodes nothing.
        let mut progress = Progress::default();
        let failure = Chain {
            config: &config,
            vocabulary: &vocabulary(&[]),
            hints: &HintPlan::default(),
            cancelled: None,
        }
        .run(
            &[0.1; 16_000],
            config.total_timeout(),
            &mut progress,
            &mut |_, _| panic!("no request after the deadline"),
            &mut |_, _, _, _| panic!("no completed websocket"),
            &mut |_| panic!("no sleep"),
        );
        assert!(failure.is_err());
        assert_eq!(progress.failures.len(), 2);
        assert_eq!(opus_encodes(), 0);

        // A cancelled chain neither encodes nor sends.
        let cancelled = AtomicBool::new(true);
        let error = run_chain(
            &config,
            &vocabulary(&[]),
            Some(&cancelled),
            &mut Progress::default(),
            &mut |_, _| panic!("no request after cancellation"),
        )
        .err()
        .expect("cancelled chain");
        assert_eq!(error.to_string(), "Transcription was cancelled.");
        assert_eq!(opus_encodes(), 0);
    }

    #[test]
    fn encoding_time_counts_against_the_attempt_deadline() {
        fn slow_opus(samples: &[f32]) -> Option<Vec<u8>> {
            std::thread::sleep(Duration::from_millis(60));
            fake_opus(samples)
        }
        use_encoder(true);
        FAKE_OPUS.with(|cell| cell.set(Some(slow_opus)));
        let mut config = config(&["deepgram::nova-3", "grok::grok-voice-transcribe-2.0"]);
        config.transcription.attempt_timeout_seconds = 5;
        config.transcription.total_timeout_seconds = 60;
        let attempt = config.attempt_timeout();
        let mut timeouts = Vec::new();
        run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, timeout| {
                timeouts.push((request.provider, request.opus, timeout));
                let status = if request.provider == Provider::Grok {
                    200
                } else {
                    503
                };
                (
                    Ok(response(status, r#"{"text":"done"}"#, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .unwrap();
        assert_eq!(opus_encodes(), 1);
        let [
            (Provider::Deepgram, true, first),
            (Provider::Grok, true, second),
        ] = timeouts[..]
        else {
            panic!("one Opus attempt per model: {timeouts:?}");
        };
        // The encoding attempt loses the encode time; the fallback reuses the
        // clip and keeps its full attempt budget.
        assert!(first <= attempt - Duration::from_millis(60), "{first:?}");
        assert!(second > attempt - Duration::from_millis(30), "{second:?}");
    }

    #[test]
    fn wav_only_providers_never_retry_a_format_error() {
        let config = config(&["meta::muse-voice-transcribe-1.0", "openai/gpt-transcribe"]);
        let mut sent = Vec::new();
        let failure = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, _| {
                sent.push((request.provider, request.opus));
                (
                    Ok(response(
                        400,
                        r#"{"error":"unsupported audio format"}"#,
                        None,
                    )),
                    Duration::from_millis(10),
                )
            },
        );
        assert!(failure.is_err());
        assert_eq!(
            sent,
            [(Provider::Meta, false), (Provider::OpenRouter, false)]
        );
    }

    #[test]
    fn rejected_opus_upload_retries_once_with_the_wav_clip_and_is_remembered() {
        use_encoder(true);
        let config = config(&["deepgram::nova-3"]);
        let mut progress = Progress::default();
        let mut sent = Vec::new();
        let success = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut progress,
            &mut |request, _| {
                sent.push((request.opus, request.content_type.clone()));
                let status = if request.opus { 415 } else { 200 };
                (
                    Ok(response(status, DEEPGRAM_DONE, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .unwrap();
        assert_eq!(success.text, "done");
        assert_eq!(
            sent,
            [(true, "audio/ogg".into()), (false, "audio/wav".into())]
        );
        assert_eq!(
            progress.failures[0].detail,
            "deepgram::nova-3: HTTP 415: compressed audio rejected"
        );
        assert!(progress.telemetry(true).retried);
        assert!(opus_refused("deepgram::nova-3"));

        // The next dictation sends the WAV first and is not a retry.
        let mut progress = Progress::default();
        let mut sent = Vec::new();
        run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut progress,
            &mut |request, _| {
                sent.push(request.opus);
                (
                    Ok(response(200, DEEPGRAM_DONE, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .unwrap();
        assert_eq!(sent, [false]);
        assert!(!progress.telemetry(true).retried);

        // A WAV rejection is final: no further request to the model.
        let mut attempts = 0;
        let failure = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |_, _| {
                attempts += 1;
                (Ok(response(415, "{}", None)), Duration::from_millis(10))
            },
        );
        assert!(failure.is_err());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn unrelated_errors_on_an_opus_upload_do_not_resend_the_wav() {
        use_encoder(true);
        let config = config(&["deepgram::nova-3", "grok::grok-voice-transcribe-2.0"]);
        let mut progress = Progress::default();
        let mut sent = Vec::new();
        let failure = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut progress,
            &mut |request, _| {
                sent.push((request.provider, request.opus));
                let body = if request.provider == Provider::Deepgram {
                    r#"{"err_msg":"Bad Request: invalid language 'xx'"}"#
                } else {
                    r#"{"error":"Text formatting requires a supported language"}"#
                };
                (Ok(response(400, body, None)), Duration::from_millis(10))
            },
        );
        assert!(failure.is_err());
        assert_eq!(sent, [(Provider::Deepgram, true), (Provider::Grok, true)]);
        assert!(progress.failures.iter().all(|failure| {
            failure
                .detail
                .ends_with("HTTP 400: transcription request failed")
        }));
        assert!(!opus_refused("deepgram::nova-3"));
        assert!(!opus_refused("grok::grok-voice-transcribe-2.0"));
    }

    #[test]
    fn hint_then_opus_rejection_resend_in_order_within_the_deadline() {
        use_encoder(true);
        let config = config(&["deepgram::nova-3"]);
        let mut progress = Progress::default();
        let mut sent = Vec::new();
        let mut elapsed = Duration::ZERO;
        let success = run_chain(
            &config,
            &vocabulary(&["Zephyr Files"]),
            None,
            &mut progress,
            &mut |request, timeout| {
                assert!(timeout <= config.attempt_timeout());
                assert!(elapsed + timeout <= config.total_timeout());
                sent.push((request.keyword_count > 0, request.opus));
                let (status, body) = match sent.len() {
                    1 => (400, r#"{"err_msg":"invalid keyterm"}"#),
                    2 => (415, "{}"),
                    _ => (200, DEEPGRAM_DONE),
                };
                elapsed += Duration::from_millis(10);
                (Ok(response(status, body, None)), Duration::from_millis(10))
            },
        )
        .unwrap();
        assert_eq!(success.text, "done");
        assert_eq!(sent, [(true, true), (false, true), (false, false)]);
        let details: Vec<_> = progress
            .failures
            .iter()
            .map(|f| f.detail.as_str())
            .collect();
        assert_eq!(
            details,
            [
                "deepgram::nova-3: HTTP 400: vocabulary parameter rejected",
                "deepgram::nova-3: HTTP 415: compressed audio rejected",
            ]
        );
    }

    #[test]
    fn rate_limited_opus_upload_retries_with_opus() {
        use_encoder(true);
        let config = config(&["deepgram::nova-3"]);
        let mut sent = Vec::new();
        let mut slept = Vec::new();
        let success = Chain {
            config: &config,
            vocabulary: &vocabulary(&[]),
            hints: &HintPlan::default(),
            cancelled: None,
        }
        .run(
            &[0.1; 16_000],
            Duration::ZERO,
            &mut Progress::default(),
            &mut |request, _| {
                sent.push(request.opus);
                let status = if sent.len() == 1 { 429 } else { 200 };
                (
                    Ok(response(
                        status,
                        DEEPGRAM_DONE,
                        Some(Duration::from_secs(1)),
                    )),
                    Duration::from_millis(10),
                )
            },
            &mut |_, _, _, _| panic!("no completed websocket"),
            &mut |wait| slept.push(wait),
        )
        .unwrap();
        assert_eq!(success.text, "done");
        assert_eq!(sent, [true, true]);
        assert_eq!(slept, [Duration::from_secs(1)]);
        assert!(!opus_refused("deepgram::nova-3"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn opus_rejected_by_one_model_still_reaches_the_next_provider() {
        use_encoder(false);
        let config = config(&["deepgram::nova-3", "elevenlabs::scribe_v2"]);
        let mut sent = Vec::<(Provider, bool, Vec<u8>)>::new();
        let success = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, _| {
                sent.push((request.provider, request.opus, request.body.clone()));
                let status = match sent.len() {
                    1 => 415,
                    2 => 503,
                    _ => 200,
                };
                (
                    Ok(response(status, r#"{"text":"done"}"#, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .unwrap();
        assert_eq!(success.model, "elevenlabs::scribe_v2");
        let [
            (_, true, opus),
            (_, false, wav),
            (Provider::ElevenLabs, true, multipart),
        ] = &sent[..]
        else {
            panic!("opus, wav, then opus to the fallback");
        };
        assert!(opus.starts_with(b"OggS") && wav.starts_with(b"RIFF"));
        // The fallback reuses the clip's single encoding.
        assert!(contains(multipart, opus));
        assert!(opus_refused("deepgram::nova-3"));
        assert!(!opus_refused("elevenlabs::scribe_v2"));
        assert_eq!(opus_encodes(), 1);
    }

    #[test]
    fn chunked_recording_encodes_each_chunk_and_keeps_the_rejection() {
        use_encoder(true);
        take_samples();
        let config = telemetry_config(&["deepgram::nova-3", "grok::grok-voice-transcribe-2.0"]);
        let mut sent = Vec::<(Provider, bool, Vec<u8>)>::new();
        let result = transcribe_configured(
            &[0.1; 15 * 16_000],
            &Snapshot::default(),
            None,
            &config,
            &mut |request, _| {
                sent.push((request.provider, request.opus, request.body.clone()));
                let (status, body) = match sent.len() {
                    1 => (415, "{}"),
                    2 => (
                        200,
                        r#"{"results":{"channels":[{"alternatives":[{"transcript":"one"}]}]}}"#,
                    ),
                    3 => (503, "{}"),
                    _ => (200, r#"{"text":"two"}"#),
                };
                (Ok(response(status, body, None)), Duration::from_millis(10))
            },
            &mut |_, _, _, _| panic!("no websocket"),
            &mut |_| panic!("no retry"),
        )
        .unwrap();
        assert_eq!(result.text, "one two");
        let summary: Vec<_> = sent
            .iter()
            .map(|(provider, opus, _)| (*provider, *opus))
            .collect();
        // Chunk two goes to Deepgram as WAV at once; its fallback still gets Opus.
        assert_eq!(
            summary,
            [
                (Provider::Deepgram, true),
                (Provider::Deepgram, false),
                (Provider::Deepgram, false),
                (Provider::Grok, true),
            ]
        );
        // One encode per chunk, whatever the chunks' lengths.
        assert_eq!(opus_encodes(), 2);
        assert_eq!(sent[0].2, b"OggS_FAKE_1");
        assert!(sent[1].2.starts_with(b"RIFF") && sent[2].2.starts_with(b"RIFF"));
        assert!(contains(&sent[3].2, b"OggS_FAKE_2") && !contains(&sent[3].2, b"RIFF"));
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].failures.len(), 2);
    }

    #[test]
    fn cancellation_during_the_wav_resend_stops_the_chain() {
        use_encoder(true);
        let config = config(&["deepgram::nova-3", "grok::grok-voice-transcribe-2.0"]);
        let cancelled = AtomicBool::new(false);
        let mut sent = Vec::new();
        let error = run_chain(
            &config,
            &vocabulary(&[]),
            Some(&cancelled),
            &mut Progress::default(),
            &mut |request, _| {
                sent.push((request.provider, request.opus));
                if !request.opus {
                    cancelled.store(true, Ordering::Release);
                }
                let status = if request.opus { 415 } else { 200 };
                (
                    Ok(response(status, DEEPGRAM_DONE, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .err()
        .expect("cancelled chain");
        assert_eq!(error.to_string(), "Transcription was cancelled.");
        assert_eq!(
            sent,
            [(Provider::Deepgram, true), (Provider::Deepgram, false)]
        );
    }

    #[test]
    fn empty_opus_transcript_resends_the_wav_once_and_remembers_only_on_success() {
        const EMPTY: &str = r#"{"results":{"channels":[{"alternatives":[{"transcript":" "}]}]}}"#;
        // Opus heard as nothing, the WAV transcribes: a silent mis-decode.
        use_encoder(true);
        let config = config(&["deepgram::nova-3", "grok::grok-voice-transcribe-2.0"]);
        let mut progress = Progress::default();
        let mut sent = Vec::new();
        let success = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut progress,
            &mut |request, _| {
                sent.push((request.provider, request.opus));
                let body = if request.opus { EMPTY } else { DEEPGRAM_DONE };
                (Ok(response(200, body, None)), Duration::from_millis(10))
            },
        )
        .unwrap();
        assert_eq!(
            (success.model.as_str(), success.text.as_str()),
            ("deepgram::nova-3", "done")
        );
        assert_eq!(
            sent,
            [(Provider::Deepgram, true), (Provider::Deepgram, false)]
        );
        assert_eq!(
            progress.failures[0].detail,
            "deepgram::nova-3: invalid transcription response"
        );
        let telemetry = progress.telemetry(true);
        assert!(telemetry.retried && !telemetry.used_fallback);
        assert_eq!(telemetry.attempts.len(), 2);
        assert_eq!(
            telemetry.attempts[0].error,
            Some(ErrorKind::InvalidResponse)
        );
        assert_eq!(progress.executions.len(), 2);
        assert!(opus_refused("deepgram::nova-3"));

        // Both empty, or the WAV resend failing: the clip may be silent, so
        // nothing is remembered and the chain falls through as before.
        for wav_status in [200, 503] {
            use_encoder(true);
            let mut sent = Vec::new();
            let success = run_chain(
                &config,
                &vocabulary(&[]),
                None,
                &mut Progress::default(),
                &mut |request, _| {
                    sent.push((request.provider, request.opus));
                    let (status, body) = match (request.provider, request.opus) {
                        (Provider::Grok, _) => (200, r#"{"text":"two"}"#),
                        (_, true) => (200, EMPTY),
                        (_, false) => (wav_status, EMPTY),
                    };
                    (Ok(response(status, body, None)), Duration::from_millis(10))
                },
            )
            .unwrap();
            assert_eq!(success.model, "grok::grok-voice-transcribe-2.0");
            assert_eq!(
                sent,
                [
                    (Provider::Deepgram, true),
                    (Provider::Deepgram, false),
                    (Provider::Grok, true),
                ]
            );
            assert!(!opus_refused("deepgram::nova-3"));
            assert_eq!(opus_encodes(), 1);
        }

        // An empty WAV answer never triggers an Opus resend.
        use_encoder(true);
        let wav_only = self::config(&["openai/gpt-transcribe"]);
        let mut sent = 0;
        let failure = run_chain(
            &wav_only,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |_, _| {
                sent += 1;
                (
                    Ok(response(200, r#"{"text":""}"#, None)),
                    Duration::from_millis(10),
                )
            },
        );
        assert!(failure.is_err());
        assert_eq!(sent, 1);
    }

    #[test]
    fn a_model_confirmed_on_opus_accepts_its_empty_answer_without_a_wav_resend() {
        const EMPTY: &str = r#"{"results":{"channels":[{"alternatives":[{"transcript":" "}]}]}}"#;
        use_encoder(true);
        let config = config(&["deepgram::nova-3", "grok::grok-voice-transcribe-2.0"]);
        // Unconfirmed: an empty Opus answer resends the WAV once, as before.
        assert!(!opus_confirmed("deepgram::nova-3"));
        let mut sent = Vec::new();
        let _ = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, _| {
                sent.push((request.provider, request.opus));
                let body = match (request.provider, request.opus) {
                    (Provider::Grok, _) => r#"{"text":"two"}"#,
                    _ => EMPTY,
                };
                (Ok(response(200, body, None)), Duration::from_millis(10))
            },
        )
        .unwrap();
        assert_eq!(
            sent,
            [
                (Provider::Deepgram, true),
                (Provider::Deepgram, false),
                (Provider::Grok, true),
            ]
        );
        assert!(!opus_confirmed("deepgram::nova-3"));
        assert!(opus_confirmed("grok::grok-voice-transcribe-2.0"));

        // An Opus upload that returns text confirms the model.
        let mut sent = Vec::new();
        let success = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, _| {
                sent.push((request.provider, request.opus));
                (
                    Ok(response(200, DEEPGRAM_DONE, None)),
                    Duration::from_millis(10),
                )
            },
        )
        .unwrap();
        assert_eq!(success.text, "done");
        assert_eq!(sent, [(Provider::Deepgram, true)]);
        assert!(opus_confirmed("deepgram::nova-3"));
        assert!(!opus_refused("deepgram::nova-3"));

        // Confirmed: a noisy clip heard as nothing is the real outcome. The
        // chain falls through without billing the same clip as WAV.
        let mut sent = Vec::new();
        let success = run_chain(
            &config,
            &vocabulary(&[]),
            None,
            &mut Progress::default(),
            &mut |request, _| {
                sent.push((request.provider, request.opus));
                let body = match request.provider {
                    Provider::Grok => r#"{"text":"two"}"#,
                    _ => EMPTY,
                };
                (Ok(response(200, body, None)), Duration::from_millis(10))
            },
        )
        .unwrap();
        assert_eq!(success.model, "grok::grok-voice-transcribe-2.0");
        assert_eq!(sent, [(Provider::Deepgram, true), (Provider::Grok, true)]);
        assert!(!opus_refused("deepgram::nova-3"));
    }

    #[test]
    fn a_cleared_rejection_never_sends_opus_twice_in_one_attempt_loop() {
        use_encoder(true);
        let config = config(&["deepgram::nova-3"]);
        let mut sent = Vec::new();
        let mut slept = Vec::new();
        let success = Chain {
            config: &config,
            vocabulary: &vocabulary(&[]),
            hints: &HintPlan::default(),
            cancelled: None,
        }
        .run(
            &[0.1; 16_000],
            Duration::ZERO,
            &mut Progress::default(),
            &mut |request, _| {
                sent.push(request.opus);
                // Another dictation evicted the shared entry meanwhile.
                with_opus_rejections(OpusRejections::clear);
                let status = match sent.len() {
                    1 => 415,
                    2 => 429,
                    _ => 200,
                };
                (
                    Ok(response(
                        status,
                        DEEPGRAM_DONE,
                        Some(Duration::from_secs(1)),
                    )),
                    Duration::from_millis(10),
                )
            },
            &mut |_, _, _, _| panic!("no completed websocket"),
            &mut |wait| slept.push(wait),
        )
        .unwrap();
        assert_eq!(success.text, "done");
        assert_eq!(sent, [true, false, false]);
        assert_eq!(slept, [Duration::from_secs(1)]);
        assert_eq!(opus_encodes(), 1);
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

    fn telemetry_config(models: &[&str]) -> Config {
        let mut config = config(models);
        config.transcription.trim_silence = false;
        config.transcription.chunk_seconds = 10;
        config
    }

    #[test]
    fn native_successes_are_estimated_and_openrouter_keeps_only_reported_costs() {
        for (id, body, reported) in [
            ("openai::gpt-transcribe", r#"{"text":"native"}"#, false),
            (
                "openai/gpt-transcribe",
                r#"{"text":"routed","usage":{"cost":0.001}}"#,
                true,
            ),
        ] {
            take_samples();
            let config = telemetry_config(&[id]);
            let result = transcribe_configured(
                &[0.1; 160_000],
                &Snapshot::default(),
                None,
                &config,
                &mut |_, _| (Ok(response(200, body, None)), Duration::from_millis(5)),
                &mut |_, _, _, _| panic!("no websocket"),
                &mut |_| panic!("no retry"),
            )
            .unwrap();
            let report = result.report.unwrap();
            let execution = &report.executions[0];
            let samples = take_samples();
            let attempt = &samples[0].telemetry.as_ref().unwrap().attempts[0];
            if reported {
                assert_eq!(execution.cost_usd, Some(0.001));
                assert_eq!(execution.estimated_cost_usd, None);
                assert_eq!(attempt.estimated_cost_usd, None);
            } else {
                // Ten seconds at $0.0045 per minute.
                let expected = 0.0045 * 10.0 / 60.0;
                assert_eq!(execution.cost_usd, None);
                assert!((execution.estimated_cost_usd.unwrap() - expected).abs() < 1e-12);
                assert!((attempt.estimated_cost_usd.unwrap() - expected).abs() < 1e-12);
            }
        }
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
            "meta::muse-voice-transcribe-1.0",
            "openai::gpt-transcribe",
            "openai/gpt-transcribe",
        ]);
        // An unsupported explicit language hint fails before any request.
        config.transcription.model_options.insert(
            "meta::muse-voice-transcribe-1.0".into(),
            ModelOptions {
                language: "ru".into(),
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
            &mut |_, _, _, _| panic!("no live-only model in this chain"),
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
                .any(|failure| failure.model == "meta::muse-voice-transcribe-1.0")
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
            "meta::muse-voice-transcribe-1.0",
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
                    3 => response(200, r#"{"transcript":" ","usage":{"cost":0.0003}}"#, None),
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
                ("meta", "muse-voice-transcribe-1.0", "failed", Some(0.0003)),
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
    fn grok_and_google_follow_their_native_batch_contracts() {
        let config = config(&[]);
        let vocabulary = vocabulary(&["Synthetic Nimbus"]);
        let hints = HintPlan::default();
        let options = ModelOptions {
            language: "pt".into(),
            prompt: "NOT_SUPPORTED_CONTEXT".into(),
            temperature: Some(0.5),
            ..Default::default()
        };
        let grok = request(
            &config,
            ModelRef::parse("grok::grok-voice-transcribe-2.0"),
            &options,
            Upload::wav(b"WAV_FIXTURE"),
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
            Upload::wav(b"WAV_FIXTURE"),
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
        for request in [&grok, &google] {
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
        let config = config(&[]);
        let vocabulary = vocabulary(&["Synthetic Nimbus"]);
        let hints = HintPlan::default();
        let options = ModelOptions {
            smart_format: false,
            no_verbatim: false,
            ..Default::default()
        };
        for id in [
            "grok::grok-voice-transcribe-2.0",
            "google::gemini-3.5-transcribe",
        ] {
            let request = request(
                &config,
                ModelRef::parse(id),
                &options,
                Upload::wav(b"WAV"),
                &vocabulary,
                &hints,
                true,
            )
            .unwrap();
            assert_eq!(request.keyword_count, 0);
            assert!(!String::from_utf8_lossy(&request.body).contains("Synthetic Nimbus"));
            match request.provider {
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
                Upload::wav(b"WAV"),
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
            parse(Provider::Grok, br#"{"text":" hello "}"#).unwrap().0,
            "hello"
        );
        let google = br#"{"status":"completed","error":null,"steps":[{"type":"user_input","content":[{"type":"text","text":"PRIVATE_INPUT"}]},{"type":"model_output","content":[{"type":"thought","text":"PRIVATE_THOUGHT"},{"type":"text","text":"hello"},{"type":"text","text":"world"}]}],"usage":{"total_tokens":42}}"#;
        let (text, usage) = parse(Provider::Google, google).unwrap();
        assert_eq!(text, "hello\nworld");
        assert_eq!(usage.tokens, 42);
        for (provider, body) in [
            (Provider::Meta, r#"{"transcript":" "}"#),
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
            "meta::muse-voice-transcribe-1.0",
            "grok::grok-voice-transcribe-2.0",
            "google::gemini-3.5-transcribe",
        ];
        let mut config = config(&ids);
        let vocabulary = vocabulary(&["Synthetic Nimbus"]);
        let hints = HintPlan::default();
        for valid_options in [true, false] {
            if !valid_options {
                // An unsupported explicit language fails before any request.
                config.transcription.model_options.insert(
                    ids[0].into(),
                    ModelOptions {
                        language: "ru".into(),
                        ..Default::default()
                    },
                );
            }
            let mut progress = Progress::default();
            let mut calls = Vec::new();
            let success = Chain {config:&config, vocabulary:&vocabulary,hints:&hints,cancelled:None}.run(
                &[0.1;160], Duration::ZERO, &mut progress,
                &mut |request, _| {
                    calls.push((request.provider, request.keyword_count));
                    (Ok(match request.provider {
                        Provider::Meta if request.keyword_count > 0 => response(422,"keywords unsupported PRIVATE_MARKER",None),
                        Provider::Meta => response(200,r#"{"transcript":""}"#,None),
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
            if valid_options {
                assert_eq!(
                    calls,
                    [
                        (Provider::Meta, 1),
                        (Provider::Meta, 0),
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
        let config = config(&[]);
        let vocabulary = vocabulary(&["Synthetic Nimbus"]);
        let hints = HintPlan::default();
        for (id, field) in [
            ("grok::grok-voice-transcribe-2.0", "keyterm"),
            ("google::gemini-3.5-transcribe", "custom_vocabulary"),
        ] {
            let model = ModelRef::parse(id);
            let sent = request(
                &config,
                model,
                &ModelOptions::default(),
                Upload::wav(b"WAV"),
                &vocabulary,
                &hints,
                false,
            )
            .unwrap();
            let without = request(
                &config,
                model,
                &ModelOptions::default(),
                Upload::wav(b"WAV"),
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
                opus: false,
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
                opus: false,
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
            Upload::wav(&wav),
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
                Upload::wav(&wav),
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
            Upload::wav(&wav),
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
            Upload::wav(&wav),
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
            Upload::wav(&wav),
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
            Upload::wav(b"fixture-wave"),
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
            "meta::muse-voice-transcribe-1.0",
            "deepgram::nova-3",
            "elevenlabs::scribe_v2",
        ]);
        let vocabulary = Snapshot::default();
        let hints = HintPlan::default();
        let mut progress = Progress::default();
        let mut calls = Vec::new();
        let success=Chain { config:&config,vocabulary:&vocabulary,hints:&hints,cancelled:None }.run(&[0.1;160],Duration::ZERO,&mut progress,
            &mut |request,_| { calls.push(request.provider); (Ok(match request.provider { Provider::OpenRouter=>response(503,"PRIVATE_PROVIDER_PAYLOAD",None), Provider::Meta=>response(200,r#"{"transcript":"   "}"#,None), _=>response(200,r#"{"results":{"channels":[{"alternatives":[{"transcript":"done"}]}]}}"#,None) }),Duration::from_millis(17)) },
            &mut |_,_,_,_| panic!("batch-capable models must not use completed WebSockets"), &mut |_| panic!("no wait")).unwrap();
        assert_eq!(success.model, "deepgram::nova-3");
        assert_eq!(success.text, "done");
        assert_eq!(success.latency_ms, 17);
        assert_eq!(
            calls,
            [Provider::OpenRouter, Provider::Meta, Provider::Deepgram]
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
            "meta::muse-voice-transcribe-1.0",
            "deepgram::nova-3",
        ]);
        let vocabulary = Snapshot::default();
        let hints = HintPlan::default();
        for final_succeeds in [true, false] {
            use_encoder(true);
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
                        Provider::Meta => response(200, r#"{"transcript":" \n\t "}"#, None),
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
                        Provider::Meta,
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
                // Deepgram's empty Opus answer is resent once as the WAV.
                assert_eq!(failure.failures.len(), 4);
                assert!(
                    failure
                        .failures
                        .iter()
                        .all(|failure| failure.kind == ErrorKind::InvalidResponse)
                );
                assert_eq!(
                    calls,
                    [
                        Provider::OpenRouter,
                        Provider::Meta,
                        Provider::Deepgram,
                        Provider::Deepgram
                    ]
                );
                assert!(waits.is_empty());
                assert!(!opus_refused("deepgram::nova-3"));
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
        // A saved "off" cannot disable a live-only model: it has no other transport.
        config
            .transcription
            .model_options
            .get_mut("openai::gpt-live-transcribe")
            .unwrap()
            .streaming = false;
        assert!(crate::providers::options(&config, "openai::gpt-live-transcribe").streaming);
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
    fn final_vad_fails_streamed_silence_without_hiding_sent_audio() {
        take_samples();
        let error = transcribe(
            &[0.0; 1600],
            &Snapshot::default(),
            Some(live_fixture("hallucinated text", 800, true)),
        )
        .err()
        .expect("streamed silence fails instead of pasting a hallucination");
        assert!(
            error
                .downcast_ref::<crate::openrouter::transcribe::NoSpeech>()
                .is_some()
        );
        assert_eq!(
            format!("{error:#}"),
            crate::openrouter::transcribe::NO_SPEECH
        );
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        // Audio already went out, so it is a failed dictation, not skipped silence.
        assert!(!samples[0].skipped_silent);
        assert_eq!(samples[0].words, None);
        assert_eq!(samples[0].sent_ms, 50);
        assert!(samples[0].telemetry.as_ref().unwrap().attempts.is_empty());
    }

    #[test]
    fn cancellation_never_resolves_or_retries_live_work() {
        take_samples();
        let live = live_fixture("must not be returned", 1600, false);
        live.cancellation_flag().store(true, Ordering::Release);
        streaming::take_discarded();
        assert!(transcribe(&[0.1; 1600], &Snapshot::default(), Some(live)).is_err());
        assert!(take_samples().is_empty());
        // The 100 ms already streamed is reported once, as discarded audio.
        let discarded = streaming::take_discarded();
        assert_eq!(discarded.len(), 1);
        assert_eq!(discarded[0].sent_ms, 100);
        let estimate = discarded[0].estimated_cost_usd.unwrap();
        assert!((estimate - 0.017 / 600.0).abs() < 1e-12, "{estimate}");
    }

    #[test]
    fn streamed_audio_in_a_recorded_dictation_is_never_also_discarded() {
        streaming::take_discarded();
        take_samples();
        transcribe(
            &[0.1; 1600],
            &Snapshot::default(),
            Some(live_fixture("hello world", 1600, false)),
        )
        .unwrap();
        assert_eq!(take_samples().len(), 1);
        assert!(streaming::take_discarded().is_empty());

        // Streamed silence is already a failed dictation with its sent audio.
        let silent = transcribe(
            &[0.0; 16_000],
            &Snapshot::default(),
            Some(live_fixture("hallucination", 16_000, true)),
        );
        assert!(silent.is_err());
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].sent_ms, 1_000);
        assert!(samples[0].discarded.is_none());
        assert!(streaming::take_discarded().is_empty());
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

    #[test]
    fn fallbacks_upload_the_finished_recording_even_when_streaming_is_saved_on() {
        take_samples();
        let mut config = telemetry_config(&["fixture/primary", "deepgram::nova-3"]);
        config.transcription.model_options.insert(
            "deepgram::nova-3".into(),
            ModelOptions {
                streaming: true,
                ..ModelOptions::default()
            },
        );
        let mut providers = Vec::new();
        let result = transcribe_configured(
            &[0.1; 1600],
            &Snapshot::default(),
            None,
            &config,
            &mut |request, _| {
                providers.push(request.provider);
                (
                    Ok(match request.provider {
                        Provider::OpenRouter => response(503, "unavailable", None),
                        _ => response(
                            200,
                            r#"{"results":{"channels":[{"alternatives":[{"transcript":"done"}]}]}}"#,
                            None,
                        ),
                    }),
                    Duration::from_millis(5),
                )
            },
            &mut |_, _, _, _| panic!("a fallback never opens a live session"),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(result.text, "done");
        assert_eq!(providers, [Provider::OpenRouter, Provider::Deepgram]);
        let samples = take_samples();
        assert!(samples[0].telemetry.as_ref().unwrap().used_fallback);
    }
}
