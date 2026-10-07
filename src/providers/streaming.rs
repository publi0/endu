//! Live audio is speculative until Finish seals the exact clip. The capture owner
//! only copies bounded PCM blocks; credentials, codecs and sockets stay here.
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, mpsc};
use std::time::{Duration, Instant};

use color_eyre::eyre::Result;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler, WindowFunction};
use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::{ModelOptions, ModelRef, Provider};
use crate::openrouter::Config;
use crate::vocabulary::Snapshot;

const BLOCK: usize = 1_600; // 100 ms of mono 16 kHz audio.
const QUEUE_BLOCKS: usize = 32;
const MAX_TAIL: usize = BLOCK * QUEUE_BLOCKS;
const POLL: Duration = Duration::from_millis(20);
const MAX_LIFETIME: Duration = Duration::from_secs(60 * 60);
const MAX_TEXT: usize = 2 * 1024 * 1024;
const MAX_SESSIONS: usize = 4;
static SESSION_COUNT: LazyLock<Arc<AtomicUsize>> = LazyLock::new(|| Arc::new(AtomicUsize::new(0)));
struct SessionPermit(Arc<AtomicUsize>);
impl SessionPermit {
    fn acquire(pool: Arc<AtomicUsize>) -> Option<Self> {
        pool.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_SESSIONS).then_some(count + 1)
        })
        .ok()?;
        Some(Self(pool))
    }
}
impl Drop for SessionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub struct LiveResult {
    pub text: String,
    pub model: String,
    pub keyword_count: usize,
    pub latency_ms: u64,
}

/// Safe to include in History: never contains provider bodies, URLs or keys.
#[derive(Debug)]
pub struct LiveError {
    pub status: Option<u16>,
    pub keyword_count: usize,
    pub keywords_rejected: bool,
    message: &'static str,
}
impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)?;
        if let Some(status) = self.status {
            write!(f, " (HTTP {status})")?;
        }
        Ok(())
    }
}
impl std::error::Error for LiveError {}
fn failure(message: &'static str) -> color_eyre::Report {
    LiveError {
        status: None,
        keyword_count: 0,
        keywords_rejected: false,
        message,
    }
    .into()
}

#[cfg(test)]
pub(crate) fn fixture_error(status: Option<u16>, keywords_rejected: bool) -> color_eyre::Report {
    LiveError {
        status,
        keyword_count: 0,
        keywords_rejected,
        message: "Synthetic streaming failure.",
    }
    .into()
}

struct AudioBlock {
    start: usize,
    samples: Vec<f32>,
}
struct Seal {
    queued: usize,
    total: usize,
    tail: Vec<f32>,
    at: Instant,
}
struct SessionControl {
    stopped: AtomicBool,
    cancelled: Arc<AtomicBool>,
    _permit: Option<SessionPermit>,
    sent_samples: Arc<AtomicUsize>,
}
impl SessionControl {
    fn new(permit: Option<SessionPermit>, cancellation: Option<Arc<AtomicBool>>) -> Arc<Self> {
        Arc::new(Self {
            stopped: AtomicBool::new(false),
            cancelled: cancellation.unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            _permit: permit,
            sent_samples: Arc::new(AtomicUsize::new(0)),
        })
    }
    fn check(&self, deadline: Instant) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) {
            return Err(failure("Streaming transcription was cancelled."));
        }
        if Instant::now() >= deadline {
            return Err(failure("Streaming transcription timed out."));
        }
        Ok(())
    }
}

pub struct LiveCapture {
    config: Config,
    vocabulary: Snapshot,
    audio: mpsc::SyncSender<AudioBlock>,
    seal: mpsc::SyncSender<Seal>,
    result: Option<mpsc::Receiver<Result<LiveResult>>>,
    control: Arc<SessionControl>,
    queued: usize,
    invalid: Option<&'static str>,
}

impl LiveCapture {
    /// Memory-only configuration snapshots; the spawned worker resolves the key.
    pub fn start() -> Option<Self> {
        // Tests may set the global config; never resolve credentials or open sockets.
        if cfg!(test) {
            return None;
        }
        let config = super::runtime_config()?;
        let id = config.transcription.models.first()?;
        if !ModelRef::parse(id).capabilities().streaming || !super::options(&config, id).streaming {
            return None;
        }
        Some(Self::spawn(config, Snapshot::current()))
    }

    fn spawn(config: Config, vocabulary: Snapshot) -> Self {
        let (audio, audio_rx) = mpsc::sync_channel(QUEUE_BLOCKS);
        let (seal, seal_rx) = mpsc::sync_channel(1);
        let (send_result, result) = mpsc::sync_channel(1);
        let permit = SessionPermit::acquire(SESSION_COUNT.clone());
        let admitted = permit.is_some();
        let control = SessionControl::new(permit, None);
        let worker_config = config.clone();
        let worker_vocabulary = vocabulary.clone();
        let worker_control = control.clone();
        // Dropping a capture never joins a socket, DNS lookup or Keychain query.
        if admitted {
            let _ = std::thread::Builder::new()
                .name("hex-live-stt".into())
                .spawn(move || {
                    let outcome = run_live(
                        &worker_config,
                        &worker_vocabulary,
                        audio_rx,
                        seal_rx,
                        &worker_control,
                    );
                    let _ = send_result.try_send(outcome);
                });
        } else {
            let _ = send_result.try_send(Err(failure(
                "Streaming session limit reached; using the complete recording.",
            )));
        }
        Self {
            config,
            vocabulary,
            audio,
            seal,
            result: Some(result),
            control,
            queued: 0,
            invalid: (!admitted)
                .then_some("Streaming session limit reached; using the complete recording."),
        }
    }

    /// `prefix` is the exact immutable prefix of the eventual untrimmed clip.
    pub fn push_prefix(&mut self, prefix: &[f32]) {
        if self.invalid.is_some() {
            return;
        }
        if prefix.len() < self.queued {
            self.invalidate("A delayed input boundary invalidated the live transcript.");
            return;
        }
        let remaining = &prefix[self.queued..];
        if remaining.len() > MAX_TAIL {
            self.invalidate("Streaming audio fell behind; using the complete recording.");
            return;
        }
        // Keep sub-packet samples in Recording until a full 100 ms block exists.
        // Otherwise a 3 ms native callback would turn the 3.2 s queue into 96 ms.
        for samples in remaining.chunks_exact(BLOCK) {
            if self
                .audio
                .try_send(AudioBlock {
                    start: self.queued,
                    samples: samples.to_vec(),
                })
                .is_err()
            {
                self.invalidate("Streaming audio fell behind; using the complete recording.");
                break;
            }
            self.queued += samples.len();
        }
    }

    fn invalidate(&mut self, reason: &'static str) {
        self.invalid = Some(reason);
        self.control.stopped.store(true, Ordering::Release);
    }

    pub fn finish(mut self, samples: &[f32]) -> PendingLive {
        if samples.len() < self.queued {
            self.invalidate("A delayed input boundary invalidated the live transcript.");
        } else if self.invalid.is_none() {
            let tail = &samples[self.queued..];
            if tail.len() > MAX_TAIL {
                self.invalidate("Streaming audio fell behind; using the complete recording.");
            } else if self
                .seal
                .try_send(Seal {
                    queued: self.queued,
                    total: samples.len(),
                    tail: tail.to_vec(),
                    at: Instant::now(),
                })
                .is_err()
            {
                self.invalidate("Streaming transcription stopped before Finish.");
            }
        }
        PendingLive {
            config: self.config.clone(),
            vocabulary: self.vocabulary.clone(),
            result: self.result.take().expect("one streaming result"),
            control: self.control.clone(),
            invalid: self.invalid,
        }
    }
}
impl Drop for LiveCapture {
    fn drop(&mut self) {
        // Ownership moved to PendingLive when result was taken by Finish.
        if self.result.is_some() {
            self.control.stopped.store(true, Ordering::Release);
        }
    }
}

pub struct PendingLive {
    pub config: Config,
    pub vocabulary: Snapshot,
    result: mpsc::Receiver<Result<LiveResult>>,
    control: Arc<SessionControl>,
    invalid: Option<&'static str>,
}
impl PendingLive {
    /// Completed, in-memory result for production-path integration tests. No
    /// configuration application, Keychain access, worker or socket is involved.
    #[cfg(test)]
    pub(crate) fn fixture(
        config: Config,
        vocabulary: Snapshot,
        result: Result<LiveResult>,
        sent_samples: usize,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(result).expect("fixture receiver is alive");
        let control = SessionControl::new(None, None);
        control.sent_samples.store(sent_samples, Ordering::Release);
        Self {
            config,
            vocabulary,
            result: receiver,
            control,
            invalid: None,
        }
    }

    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.control.cancelled.clone()
    }
    pub fn sent_samples(&self) -> usize {
        self.sent_samples_counter().load(Ordering::Acquire)
    }
    pub fn sent_samples_counter(&self) -> Arc<AtomicUsize> {
        self.control.sent_samples.clone()
    }
    pub fn model_id(&self) -> &str {
        &self.config.transcription.models[0]
    }
    pub fn resolve(self, timeout: Duration) -> Result<LiveResult> {
        if let Some(reason) = self.invalid {
            return Err(failure(reason));
        }
        let deadline = Instant::now() + timeout;
        loop {
            self.control.check(deadline)?;
            match self
                .result
                .recv_timeout(POLL.min(deadline.saturating_duration_since(Instant::now())))
            {
                Ok(result) => {
                    self.control.check(deadline)?;
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(failure("Streaming worker stopped."));
                }
            }
        }
    }
}
impl Drop for PendingLive {
    fn drop(&mut self) {
        self.control.stopped.store(true, Ordering::Release);
    }
}

/// A fresh WebSocket for an already completed clip (not reported as live capture).
pub fn transcribe_completed(
    config: &Config,
    model_id: &str,
    samples: &[f32],
    vocabulary: &Snapshot,
    timeout: Duration,
    cancellation: Option<Arc<AtomicBool>>,
) -> Result<LiveResult> {
    let permit = SessionPermit::acquire(SESSION_COUNT.clone())
        .ok_or_else(|| failure("Streaming session limit reached."))?;
    let control = SessionControl::new(Some(permit), cancellation);
    let started = Instant::now();
    let deadline = started + timeout;
    control.check(deadline)?;
    let mut connection = Connection::open(config, model_id, vocabulary, deadline, &control)?;
    connection.ready(deadline, &control)?;
    for chunk in samples.chunks(BLOCK) {
        control.check(deadline)?;
        connection.audio(chunk, deadline)?;
        connection.poll(deadline, &control)?;
    }
    connection.finish(deadline)?;
    let text = connection.complete(deadline, &control)?;
    Ok(connection.result(text, started))
}

fn run_live(
    config: &Config,
    vocabulary: &Snapshot,
    audio: mpsc::Receiver<AudioBlock>,
    seal: mpsc::Receiver<Seal>,
    control: &Arc<SessionControl>,
) -> Result<LiveResult> {
    let started = Instant::now();
    let lifetime = started + MAX_LIFETIME;
    let attempt = Duration::from_secs(config.transcription.attempt_timeout_seconds.clamp(1, 600));
    let id = config
        .transcription
        .models
        .first()
        .ok_or_else(|| failure("No streaming model selected."))?;
    let mut connection = Connection::open(
        config,
        id,
        vocabulary,
        Instant::now() + attempt.min(Duration::from_secs(10)),
        control,
    )?;
    connection.ready(
        Instant::now() + attempt.min(Duration::from_secs(10)),
        control,
    )?;
    let mut received = 0;
    let mut ending: Option<Seal> = None;
    loop {
        control.check(lifetime)?;
        if ending.is_none() {
            ending = match seal.try_recv() {
                Ok(value) => Some(value),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(failure("Streaming capture was cancelled."));
                }
            };
        }
        if let Some(end) = &ending {
            control.check(end.at + attempt)?;
            if received > end.queued || end.total != end.queued + end.tail.len() {
                return Err(failure(
                    "Streaming audio boundaries did not match the recording.",
                ));
            }
            if received == end.queued {
                for chunk in end.tail.chunks(BLOCK) {
                    control.check(end.at + attempt)?;
                    connection.audio(chunk, end.at + attempt)?;
                    connection.poll(end.at + attempt, control)?;
                }
                connection.finish(end.at + attempt)?;
                let text = connection.complete(end.at + attempt, control)?;
                return Ok(connection.result(text, end.at));
            }
        }
        match audio.recv_timeout(POLL) {
            Ok(block) => {
                if block.start != received {
                    return Err(failure("Streaming audio was not contiguous."));
                }
                let deadline = ending
                    .as_ref()
                    .map_or(lifetime, |end| (end.at + attempt).min(lifetime));
                connection.audio(&block.samples, deadline)?;
                received += block.samples.len();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // Finish uses a separate mailbox. Its sender can disappear before
            // this receive notices the last queued frame; inspect it next loop.
            Err(mpsc::RecvTimeoutError::Disconnected) if ending.is_none() => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(failure(
                    "Streaming audio ended before its declared boundary.",
                ));
            }
        }
        let deadline = ending
            .as_ref()
            .map_or(lifetime, |end| (end.at + attempt).min(lifetime));
        connection.poll(deadline, control)?;
        connection.keep_alive()?;
    }
}

fn endpoint(model: ModelRef<'_>, options: &ModelOptions, keywords: &[String]) -> Result<url::Url> {
    let base = match model.provider {
        Provider::OpenAi => "wss://api.openai.com/v1/realtime?intent=transcription",
        Provider::ElevenLabs => "wss://api.elevenlabs.io/v1/speech-to-text/realtime",
        Provider::Deepgram => "wss://api.deepgram.com/v1/listen",
        Provider::OpenRouter => {
            return Err(failure(
                "OpenRouter does not support live transcription here.",
            ));
        }
    };
    let mut url = url::Url::parse(base).map_err(|_| failure("Invalid streaming endpoint."))?;
    let auto = options.language == "auto";
    match model.provider {
        Provider::ElevenLabs => {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("model_id", model.model)
                .append_pair("audio_format", "pcm_16000")
                .append_pair("commit_strategy", "manual")
                .append_pair(
                    "no_verbatim",
                    if options.no_verbatim { "true" } else { "false" },
                );
            if !auto {
                query.append_pair("language_code", &options.language);
            }
            for term in keywords {
                query.append_pair("keyterms", term);
            }
        }
        Provider::Deepgram => {
            if auto && model.model != "nova-3" {
                return Err(failure("Nova-2 streaming requires an explicit language."));
            }
            let mut query = url.query_pairs_mut();
            query
                .append_pair("model", model.model)
                .append_pair("encoding", "linear16")
                .append_pair("sample_rate", "16000")
                .append_pair("channels", "1")
                .append_pair("interim_results", "true")
                .append_pair("endpointing", "false")
                .append_pair("language", if auto { "multi" } else { &options.language })
                .append_pair(
                    "smart_format",
                    if options.smart_format {
                        "true"
                    } else {
                        "false"
                    },
                )
                .append_pair(
                    "punctuate",
                    if options.punctuate { "true" } else { "false" },
                )
                .append_pair("numerals", if options.numerals { "true" } else { "false" });
            for term in keywords {
                query.append_pair("keyterm", term);
            }
        }
        _ => {}
    }
    Ok(url)
}
fn rejected_keywords(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["keyword", "keyterm", "prompt"]
        .iter()
        .any(|field| message.contains(field))
}

fn session_update(model: &str, options: &ModelOptions, keywords: &[String]) -> Value {
    let mut transcription = json!({"model":model,"delay":"medium"});
    if !options.prompt.is_empty() {
        transcription["prompt"] = json!(options.prompt);
    }
    if !keywords.is_empty() {
        transcription["keywords"] = json!(keywords);
    }
    if options.language != "auto" {
        transcription["languages"] = json!([options.language]);
    }
    json!({"type":"session.update","session":{"type":"transcription","audio":{"input":{
        "format":{"type":"audio/pcm","rate":24000},"transcription":transcription,"turn_detection":null
    }}}})
}

struct Protocol {
    provider: Provider,
    ready: bool,
    finishing: bool,
    commit_item: Option<String>,
    completed_items: BTreeMap<String, String>,
    segments: BTreeMap<(u64, u64), String>,
}
impl Protocol {
    fn new(provider: Provider) -> Self {
        Self {
            provider,
            ready: provider == Provider::Deepgram,
            finishing: false,
            commit_item: None,
            completed_items: BTreeMap::new(),
            segments: BTreeMap::new(),
        }
    }
    fn event(&mut self, event: &Value) -> Result<Option<String>> {
        let kind = event
            .get("type")
            .or_else(|| event.get("message_type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind == "conversation.item.input_audio_transcription.failed"
            || kind == "error"
            || kind == "Error"
            || kind.ends_with("_error")
            || event.get("error").is_some_and(|error| !error.is_null())
            || matches!(
                kind,
                "auth_error" | "quota_exceeded" | "rate_limited" | "unaccepted_terms"
            )
        {
            let status = event
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok());
            return Err(LiveError {
                status,
                keyword_count: 0,
                keywords_rejected: rejected_keywords(&event.to_string()),
                message: "The streaming provider rejected the request.",
            }
            .into());
        }
        match self.provider {
            Provider::OpenAi => {
                if kind == "session.updated" {
                    self.ready = true;
                }
                if self.finishing && kind == "input_audio_buffer.committed" {
                    self.commit_item = event["item_id"].as_str().map(str::to_owned);
                }
                if self.finishing
                    && kind == "conversation.item.input_audio_transcription.completed"
                    && let (Some(id), Some(text)) =
                        (event["item_id"].as_str(), event["transcript"].as_str())
                {
                    if text.len() > MAX_TEXT || self.completed_items.len() >= 16 {
                        return Err(failure("Streaming response exceeded its limit."));
                    }
                    let retained: usize = self.completed_items.values().map(String::len).sum();
                    let replaced = self.completed_items.get(id).map_or(0, String::len);
                    if retained.saturating_sub(replaced).saturating_add(text.len()) > MAX_TEXT {
                        return Err(failure("Streaming response exceeded its limit."));
                    }
                    self.completed_items.insert(id.into(), text.into());
                }
                if let Some(id) = &self.commit_item {
                    return Ok(self.completed_items.remove(id));
                }
            }
            Provider::ElevenLabs => {
                if kind == "session_started" {
                    self.ready = true;
                }
                if kind == "committed_transcript" {
                    if !self.finishing {
                        return Err(failure("The streaming provider committed before Finish."));
                    }
                    let text = event["text"]
                        .as_str()
                        .ok_or_else(|| failure("Streaming response had no final text."))?;
                    if text.len() > MAX_TEXT {
                        return Err(failure("Streaming response exceeded its limit."));
                    }
                    return Ok(Some(text.to_owned()));
                }
            }
            Provider::Deepgram => {
                if kind == "Results" && event["is_final"].as_bool() == Some(true) {
                    let text = event["channel"]["alternatives"][0]["transcript"]
                        .as_str()
                        .ok_or_else(|| failure("Streaming response had no transcript."))?;
                    let start = event["start"]
                        .as_f64()
                        .filter(|n| n.is_finite() && *n >= 0.0)
                        .ok_or_else(|| failure("Streaming response had invalid timing."))?;
                    let duration = event["duration"]
                        .as_f64()
                        .filter(|n| n.is_finite() && *n >= 0.0)
                        .ok_or_else(|| failure("Streaming response had invalid timing."))?;
                    if text.len() > MAX_TEXT || self.segments.len() >= 10_000 {
                        return Err(failure("Streaming response exceeded its limit."));
                    }
                    let previous = self
                        .segments
                        .get(&(
                            (start * 16_000.0).round() as u64,
                            (duration * 16_000.0).round() as u64,
                        ))
                        .map_or(0, String::len);
                    let retained = self.segments.values().map(String::len).sum::<usize>();
                    if retained.saturating_sub(previous).saturating_add(text.len()) > MAX_TEXT {
                        return Err(failure("Streaming response exceeded its limit."));
                    }
                    let segment = (
                        (start * 16_000.0).round() as u64,
                        (duration * 16_000.0).round() as u64,
                    );
                    let retained: usize = self.segments.values().map(String::len).sum();
                    let replaced = self.segments.get(&segment).map_or(0, String::len);
                    if retained.saturating_sub(replaced).saturating_add(text.len()) > MAX_TEXT {
                        return Err(failure("Streaming response exceeded its limit."));
                    }
                    self.segments.insert(segment, text.to_owned());
                }
                if kind == "Metadata" && self.finishing {
                    return self.joined().map(Some);
                }
            }
            Provider::OpenRouter => {}
        }
        Ok(None)
    }
    fn joined(&self) -> Result<String> {
        if self.segments.values().map(String::len).sum::<usize>() > MAX_TEXT {
            return Err(failure("Streaming response exceeded its limit."));
        }
        Ok(self
            .segments
            .values()
            .map(|text| text.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "))
    }
    fn closed(&self) -> Result<Option<String>> {
        Err(failure(
            "Streaming connection closed without its terminal confirmation.",
        ))
    }
}

// tungstenite/TLS may perform several reads inside one call. Check the
// deadline and cancellation at the underlying I/O boundary, not only between
// complete WebSocket messages. Debug intentionally omits address and payload.
struct SocketIo {
    tcp: TcpStream,
    control: Arc<SessionControl>,
    deadline: Instant,
    read_interval: Duration,
}
impl std::fmt::Debug for SocketIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamingSocket")
    }
}
fn io_budget(
    control: &SessionControl,
    deadline: Instant,
    interval: Duration,
) -> io::Result<Duration> {
    if control.stopped.load(Ordering::Acquire) || control.cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "stream cancelled",
        ));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "stream deadline"));
    }
    Ok(interval.min(remaining))
}
impl Read for SocketIo {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.tcp.set_read_timeout(Some(io_budget(
            &self.control,
            self.deadline,
            self.read_interval,
        )?))?;
        self.tcp.read(bytes)
    }
}
impl Write for SocketIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.tcp.set_write_timeout(Some(io_budget(
            &self.control,
            self.deadline,
            Duration::from_secs(1),
        )?))?;
        self.tcp.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        io_budget(&self.control, self.deadline, Duration::from_secs(1))?;
        self.tcp.flush()
    }
}

struct Connection {
    socket: WebSocket<MaybeTlsStream<SocketIo>>,
    protocol: Protocol,
    encoder: PcmEncoder,
    model: String,
    keyword_count: usize,
    last_send: Instant,
    completed: Option<String>,
    wire_frames: usize,
    sent_samples: Arc<AtomicUsize>,
}
impl Connection {
    fn open(
        config: &Config,
        id: &str,
        vocabulary: &Snapshot,
        deadline: Instant,
        control: &Arc<SessionControl>,
    ) -> Result<Self> {
        if cfg!(test) {
            return Err(failure("Native streaming network is disabled in tests."));
        }
        control.check(deadline)?;
        let model = ModelRef::parse(id);
        if !model.capabilities().streaming {
            return Err(failure("This model does not support streaming."));
        }
        let options = super::options(config, id);
        options
            .validate()
            .map_err(|_| failure("Invalid streaming model options."))?;
        let keywords = super::batch::keywords(model, vocabulary);
        let url = endpoint(model, &options, &keywords)?;
        let key = super::keys::api_key(model.provider, config)
            .map_err(|_| failure("The streaming provider has no available API key."))?;
        control.check(deadline)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| failure("Could not create streaming request."))?;
        let (header, value) = match model.provider {
            Provider::ElevenLabs => ("xi-api-key", key),
            Provider::Deepgram => ("authorization", format!("Token {key}")),
            _ => ("authorization", format!("Bearer {key}")),
        };
        let mut value = tungstenite::http::HeaderValue::from_str(&value)
            .map_err(|_| failure("Invalid streaming credential."))?;
        value.set_sensitive(true);
        request.headers_mut().insert(header, value);
        let host = url
            .host_str()
            .ok_or_else(|| failure("Invalid streaming endpoint."))?
            .to_owned();
        let port = url.port_or_known_default().unwrap_or(443);
        // Resolver calls cannot block capture or the caller past its deadline.
        let (tx, rx) = mpsc::sync_channel(1);
        let dns_control = control.clone();
        std::thread::spawn(move || {
            let _permit = dns_control;
            let _ = tx.send(
                (host.as_str(), port)
                    .to_socket_addrs()
                    .map(|addresses| addresses.take(8).collect::<Vec<_>>()),
            );
        });
        let addresses = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| failure("Streaming DNS lookup timed out."))?
            .map_err(|_| failure("Streaming DNS lookup failed."))?;
        let mut connected = None;
        for address in addresses {
            control.check(deadline)?;
            if let Ok(stream) = TcpStream::connect_timeout(
                &address,
                Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())),
            ) {
                connected = Some(stream);
                break;
            }
        }
        let stream =
            connected.ok_or_else(|| failure("Could not connect to streaming provider."))?;
        let stream = SocketIo {
            tcp: stream,
            control: control.clone(),
            deadline,
            read_interval: Duration::from_secs(1),
        };
        let (mut socket, _) = tungstenite::client_tls_with_config(
            request,
            stream,
            Some(
                tungstenite::protocol::WebSocketConfig::default()
                    .max_message_size(Some(MAX_TEXT))
                    .max_frame_size(Some(MAX_TEXT)),
            ),
            None,
        )
        .map_err(|error| {
            let (status, keywords_rejected) = match &error {
                tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response)) => {
                    let status = response.status().as_u16();
                    let rejected = matches!(status, 400 | 422)
                        && response
                            .body()
                            .as_ref()
                            .is_some_and(|body| rejected_keywords(&String::from_utf8_lossy(body)));
                    (Some(status), rejected)
                }
                _ => (None, false),
            };
            color_eyre::Report::from(LiveError {
                status,
                keyword_count: 0,
                keywords_rejected,
                message: "Streaming handshake failed.",
            })
        })?;
        let stream = match socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream,
            MaybeTlsStream::Rustls(stream) => &mut stream.sock,
            _ => return Err(failure("Unsupported streaming transport.")),
        };
        stream.read_interval = Duration::from_millis(1);
        let mut connection = Self {
            socket,
            protocol: Protocol::new(model.provider),
            encoder: PcmEncoder::new(model.provider == Provider::OpenAi)?,
            model: id.to_owned(),
            keyword_count: keywords.len(),
            last_send: Instant::now(),
            completed: None,
            wire_frames: 0,
            sent_samples: control.sent_samples.clone(),
        };
        if model.provider == Provider::OpenAi {
            connection.json(session_update(model.model, &options, &keywords))?;
        }
        Ok(connection)
    }
    fn json(&mut self, value: Value) -> Result<()> {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .map_err(|_| failure("Streaming write failed."))?;
        self.last_send = Instant::now();
        Ok(())
    }
    fn ready(&mut self, deadline: Instant, control: &SessionControl) -> Result<()> {
        while !self.protocol.ready {
            self.poll(deadline, control)?;
        }
        Ok(())
    }
    fn set_deadline(&mut self, deadline: Instant) {
        match self.socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream.deadline = deadline,
            MaybeTlsStream::Rustls(stream) => stream.sock.deadline = deadline,
            _ => {}
        }
    }
    fn audio(&mut self, samples: &[f32], deadline: Instant) -> Result<()> {
        self.set_deadline(deadline);
        let pcm = self.encoder.push(samples, false)?;
        self.send_pcm(pcm)
    }
    fn send_pcm(&mut self, pcm: Vec<u8>) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let frames = pcm.len() / 2;
        match self.protocol.provider {
            Provider::OpenAi => self.json(json!({"type":"input_audio_buffer.append","audio":crate::openrouter::transcribe::encode_base64(&pcm)})),
            Provider::ElevenLabs => self.json(json!({"message_type":"input_audio_chunk","audio_base_64":crate::openrouter::transcribe::encode_base64(&pcm),"commit":false,"sample_rate":16000})),
            Provider::Deepgram => {
                self.socket.send(Message::Binary(pcm.into())).map_err(|_| failure("Streaming write failed."))?;
                self.last_send = Instant::now();
                Ok(())
            }
            _ => Err(failure("Unsupported streaming provider.")),
        }?;
        self.wire_frames += frames;
        let logical = if self.protocol.provider == Provider::OpenAi {
            self.wire_frames * 2 / 3
        } else {
            self.wire_frames
        };
        self.sent_samples.store(logical, Ordering::Release);
        Ok(())
    }
    fn finish(&mut self, deadline: Instant) -> Result<()> {
        self.set_deadline(deadline);
        let pcm = self.encoder.push(&[], true)?;
        self.send_pcm(pcm)?;
        self.protocol.finishing = true;
        self.json(match self.protocol.provider {
            Provider::OpenAi => json!({"type":"input_audio_buffer.commit"}),
            Provider::ElevenLabs => json!({"message_type":"input_audio_chunk","audio_base_64":"","commit":true,"sample_rate":16000}),
            _ => json!({"type":"CloseStream"}),
        })
    }
    fn keep_alive(&mut self) -> Result<()> {
        if self.protocol.provider == Provider::Deepgram
            && self.last_send.elapsed() >= Duration::from_secs(3)
        {
            self.json(json!({"type":"KeepAlive"}))?;
        }
        Ok(())
    }
    fn poll(&mut self, deadline: Instant, control: &SessionControl) -> Result<()> {
        control.check(deadline)?;
        self.set_deadline(deadline);
        let result = match self.socket.read() {
            Ok(Message::Text(text)) => {
                let value: Value = serde_json::from_str(&text)
                    .map_err(|_| failure("Invalid streaming response."))?;
                self.protocol.event(&value)
            }
            Ok(Message::Close(frame)) => {
                if frame.is_some_and(|frame| {
                    frame.code != tungstenite::protocol::frame::coding::CloseCode::Normal
                }) {
                    return Err(failure(
                        "Streaming provider closed the connection with an error.",
                    ));
                }
                self.protocol.closed()
            }
            Err(tungstenite::Error::ConnectionClosed) => self.protocol.closed(),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(_) => Err(failure("Streaming read failed.")),
            _ => Ok(None),
        }?;
        if result.is_some() {
            self.completed = result;
        }
        Ok(())
    }
    fn complete(&mut self, deadline: Instant, control: &SessionControl) -> Result<String> {
        loop {
            control.check(deadline)?;
            if let Some(text) = self.completed.take() {
                return Ok(text);
            }
            self.poll(deadline, control)?;
        }
    }
    fn result(&self, text: String, started: Instant) -> LiveResult {
        LiveResult {
            text,
            model: self.model.clone(),
            keyword_count: self.keyword_count,
            latency_ms: started.elapsed().as_millis() as u64,
        }
    }
}

/// Stateful 16 -> 24 kHz conversion for OpenAI; never restarts at packet edges.
struct PcmEncoder {
    resampler: Option<Fft<f32>>,
    input: Vec<f32>,
    source: usize,
    emitted: usize,
    delay: usize,
}
impl PcmEncoder {
    fn new(upsample: bool) -> Result<Self> {
        let resampler = if upsample {
            Some(
                Fft::new_custom(
                    16_000,
                    24_000,
                    1024,
                    1,
                    1,
                    WindowFunction::BlackmanHarris2,
                    FixedSync::Input,
                )
                .map_err(|_| failure("Could not initialize streaming audio conversion."))?,
            )
        } else {
            None
        };
        let delay = resampler.as_ref().map_or(0, Resampler::output_delay);
        Ok(Self {
            resampler,
            input: Vec::new(),
            source: 0,
            emitted: 0,
            delay,
        })
    }
    fn push(&mut self, samples: &[f32], finish: bool) -> Result<Vec<u8>> {
        self.source += samples.len();
        let Some(resampler) = &mut self.resampler else {
            return Ok(super::batch::pcm16(samples));
        };
        self.input.extend_from_slice(samples);
        let expected = self.source * 3 / 2;
        let mut output = Vec::new();
        let mut flushes = 0;
        loop {
            let count = resampler.input_frames_next();
            let full = self.input.len() >= count;
            if !full && (!finish || self.emitted >= expected || flushes >= 8) {
                break;
            }
            let take = self.input.len().min(count);
            let partial = (!full).then_some(Indexing {
                partial_len: Some(take),
                ..Default::default()
            });
            let input = InterleavedSlice::new(&self.input[..take], 1, take)
                .map_err(|_| failure("Invalid streaming audio conversion."))?;
            let converted = resampler
                .process(&input, partial.as_ref())
                .map_err(|_| failure("Streaming audio conversion failed."))?
                .take_data();
            self.input.drain(..take);
            let skip = self.delay.min(converted.len());
            self.delay -= skip;
            let end = (converted.len() - skip).min(expected.saturating_sub(self.emitted));
            output.extend_from_slice(&converted[skip..skip + end]);
            self.emitted += end;
            if !full {
                flushes += 1;
            }
        }
        if finish && self.emitted != expected {
            return Err(failure("Streaming audio conversion was incomplete."));
        }
        Ok(super::batch::pcm16(&output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        LiveCapture,
        mpsc::Receiver<AudioBlock>,
        mpsc::Receiver<Seal>,
        mpsc::SyncSender<Result<LiveResult>>,
    ) {
        let (audio, audio_rx) = mpsc::sync_channel(QUEUE_BLOCKS);
        let (seal, seal_rx) = mpsc::sync_channel(1);
        let (send_result, result) = mpsc::sync_channel(1);
        let mut config = Config::default();
        config.transcription.models = vec!["deepgram::nova-3".into()];
        let live = LiveCapture {
            config,
            vocabulary: Snapshot::default(),
            audio,
            seal,
            result: Some(result),
            control: SessionControl::new(None, None),
            queued: 0,
            invalid: None,
        };
        (live, audio_rx, seal_rx, send_result)
    }

    #[test]
    fn aggregate_segment_storage_is_bounded_before_session_completion() {
        let text = "x".repeat(MAX_TEXT / 2 + 1);
        let mut deepgram = Protocol::new(Provider::Deepgram);
        let result = |start| json!({"type":"Results","is_final":true,"start":start,"duration":1.0,"channel":{"alternatives":[{"transcript":text}]}});
        assert!(deepgram.event(&result(0.0)).is_ok());
        assert!(
            deepgram.event(&result(0.0)).is_ok(),
            "duplicate replaces its own storage"
        );
        assert!(deepgram.event(&result(1.0)).is_err());
        assert_eq!(deepgram.segments.len(), 1);
        let mut openai = Protocol::new(Provider::OpenAi);
        openai.finishing = true;
        let completed = |id| json!({"type":"conversation.item.input_audio_transcription.completed","item_id":id,"transcript":text});
        assert!(openai.event(&completed("first")).is_ok());
        assert!(openai.event(&completed("second")).is_err());
        assert_eq!(openai.completed_items.len(), 1);
    }

    #[test]
    fn seal_preserves_contiguous_audio_and_moves_cancellation_ownership() {
        let (mut live, audio, seal, result) = fixture();
        let samples: Vec<_> = (0..4_235).map(|n| n as f32 / 10_000.0).collect();
        live.push_prefix(&samples[..2_345]);
        live.push_prefix(&samples[..3_456]);
        let control = live.control.clone();
        let pending = live.finish(&samples);
        assert!(!control.stopped.load(Ordering::Acquire));
        let mut reconstructed = Vec::new();
        while let Ok(block) = audio.try_recv() {
            assert_eq!(block.start, reconstructed.len());
            reconstructed.extend(block.samples);
        }
        let end = seal.try_recv().unwrap();
        assert_eq!(end.queued, reconstructed.len());
        reconstructed.extend(end.tail);
        assert_eq!(end.total, samples.len());
        assert_eq!(reconstructed, samples);
        result
            .send(Ok(LiveResult {
                text: "finished".into(),
                model: pending.model_id().into(),
                keyword_count: 0,
                latency_ms: 12,
            }))
            .unwrap();
        assert_eq!(
            pending.resolve(Duration::from_secs(1)).unwrap().text,
            "finished"
        );
        assert!(control.stopped.load(Ordering::Acquire));
        assert!(!control.cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn tiny_capture_callbacks_are_coalesced_into_bounded_transport_packets() {
        let (mut live, audio, _seal, _result) = fixture();
        let samples = vec![0.25; BLOCK * 3 + 47];
        for end in (1..samples.len()).step_by(47) {
            live.push_prefix(&samples[..end]);
        }
        live.push_prefix(&samples);
        assert_eq!(live.queued, BLOCK * 3);
        let packets: Vec<_> = audio.try_iter().collect();
        assert_eq!(packets.len(), 3);
        assert!(packets.iter().all(|packet| packet.samples.len() == BLOCK));
    }

    #[test]
    fn backpressure_and_late_boundaries_invalidate_without_claiming_user_cancellation() {
        for overshoot in [false, true] {
            let (mut live, _audio, _seal, _result) = fixture();
            let samples = vec![0.25; MAX_TAIL + BLOCK];
            if overshoot {
                live.push_prefix(&samples[..BLOCK]);
            } else {
                for end in (BLOCK..=samples.len()).step_by(BLOCK) {
                    live.push_prefix(&samples[..end]);
                }
            }
            let pending = live.finish(if overshoot {
                &samples[..BLOCK - 1]
            } else {
                &samples
            });
            assert!(!pending.cancellation_flag().load(Ordering::Acquire));
            assert!(pending.resolve(Duration::ZERO).is_err());
        }
        let (mut live, _audio, _seal, _result) = fixture();
        live.push_prefix(&[0.25; BLOCK]);
        live.push_prefix(&[0.25; BLOCK - 1]);
        assert!(live.invalid.is_some());
    }

    #[test]
    fn dropping_capture_or_pending_cancels_without_joining_a_worker() {
        let (live, _audio, _seal, _result) = fixture();
        let control = live.control.clone();
        drop(live);
        assert!(control.stopped.load(Ordering::Acquire));
        let (live, _audio, _seal, _result) = fixture();
        let pending = live.finish(&[]);
        let control = pending.control.clone();
        let cancelled = pending.cancellation_flag();
        cancelled.store(true, Ordering::Release);
        assert!(pending.resolve(Duration::from_secs(1)).is_err());
        assert!(control.stopped.load(Ordering::Acquire));
    }

    #[test]
    fn openai_uses_one_manual_commit_and_only_its_matching_final() {
        let options = ModelOptions {
            language: "pt".into(),
            prompt: "A meeting".into(),
            ..Default::default()
        };
        let update = session_update("gpt-live-transcribe", &options, &["Nimbus".into()]);
        assert_eq!(
            update["session"]["audio"]["input"]["format"]["rate"],
            24_000
        );
        assert!(update["session"]["audio"]["input"]["turn_detection"].is_null());
        assert_eq!(
            update["session"]["audio"]["input"]["transcription"]["languages"],
            json!(["pt"])
        );
        let mut protocol = Protocol::new(Provider::OpenAi);
        protocol.event(&json!({"type":"session.updated"})).unwrap();
        assert!(protocol.ready);
        assert!(protocol.event(&json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"early","transcript":"ignored"})).unwrap().is_none());
        protocol.finishing = true;
        assert!(protocol.event(&json!({"type":"conversation.item.input_audio_transcription.delta","delta":"never output"})).unwrap().is_none());
        assert!(protocol.event(&json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"ours","transcript":"final"})).unwrap().is_none());
        assert_eq!(
            protocol
                .event(&json!({"type":"input_audio_buffer.committed","item_id":"ours"}))
                .unwrap(),
            Some("final".into())
        );
        let failure = protocol.event(&json!({"type":"conversation.item.input_audio_transcription.failed","error":{"message":"PRIVATE_PAYLOAD"}})).unwrap_err();
        assert!(!failure.to_string().contains("PRIVATE_PAYLOAD"));
    }

    #[test]
    fn deepgram_collects_final_segments_until_finish_and_deduplicates() {
        let mut protocol = Protocol::new(Provider::Deepgram);
        let event = |start, text, finality| json!({"type":"Results","start":start,"duration":1.0,"is_final":finality,"channel":{"alternatives":[{"transcript":text}]}});
        for value in [
            event(1.0, "world", true),
            event(0.0, "Hello", true),
            event(0.0, "Hello", true),
            event(2.0, "partial", false),
        ] {
            assert!(protocol.event(&value).unwrap().is_none());
        }
        assert!(
            protocol
                .event(&json!({"type":"Metadata"}))
                .unwrap()
                .is_none()
        );
        assert!(protocol.closed().is_err());
        protocol.finishing = true;
        assert!(
            protocol.closed().is_err(),
            "CloseStream alone cannot prove all segments arrived"
        );
        assert_eq!(
            protocol.event(&json!({"type":"Metadata"})).unwrap(),
            Some("Hello world".into())
        );
    }

    #[test]
    fn scribe_requires_a_manual_final_and_provider_errors_are_sanitized() {
        let mut protocol = Protocol::new(Provider::ElevenLabs);
        protocol
            .event(&json!({"message_type":"session_started"}))
            .unwrap();
        assert!(protocol.ready);
        assert!(
            protocol
                .event(&json!({"message_type":"partial_transcript","text":"partial"}))
                .unwrap()
                .is_none()
        );
        assert!(
            protocol
                .event(&json!({"message_type":"committed_transcript","text":"early"}))
                .is_err()
        );
        protocol.finishing = true;
        assert_eq!(
            protocol
                .event(&json!({"message_type":"committed_transcript","text":"final"}))
                .unwrap(),
            Some("final".into())
        );
        let error = protocol
            .event(&json!({"message_type":"error","status":429,"error":"PRIVATE_KEY_AND_BODY"}))
            .unwrap_err();
        assert_eq!(error.downcast_ref::<LiveError>().unwrap().status, Some(429));
        assert!(!format!("{error:#}").contains("PRIVATE"));
        let rejected = protocol
            .event(
                &json!({"message_type":"error","status":400,"error":"invalid keyterms: PRIVATE"}),
            )
            .unwrap_err();
        assert!(
            rejected
                .downcast_ref::<LiveError>()
                .unwrap()
                .keywords_rejected
        );
        assert!(!rejected.to_string().contains("PRIVATE"));
    }

    #[test]
    fn endpoints_encode_keywords_and_reject_unsupported_automatic_language() {
        let options = ModelOptions::default();
        let terms = vec!["A&B / name".into(), "Other name".into()];
        let url = endpoint(ModelRef::parse("deepgram::nova-3"), &options, &terms).unwrap();
        assert_eq!(url.query_pairs().filter(|(k, _)| k == "keyterm").count(), 2);
        assert!(
            url.query_pairs()
                .any(|(k, v)| k == "language" && v == "multi")
        );
        assert!(endpoint(ModelRef::parse("deepgram::nova-2"), &options, &[]).is_err());
        let url = endpoint(
            ModelRef::parse("elevenlabs::scribe_v2_realtime"),
            &options,
            &terms,
        )
        .unwrap();
        assert_eq!(
            url.query_pairs().filter(|(k, _)| k == "keyterms").count(),
            2
        );
        assert!(
            url.query_pairs()
                .any(|(k, v)| k == "commit_strategy" && v == "manual")
        );
        assert!(!url.query_pairs().any(|(k, _)| k == "language_code"));
    }

    #[test]
    fn pcm24_conversion_is_independent_of_packet_boundaries_and_flushes_tail() {
        for len in [1, 1023, 1024, 2049, 16_137] {
            let samples: Vec<_> = (0..len).map(|n| ((n as f32) * 0.03).sin() * 0.5).collect();
            let mut whole = PcmEncoder::new(true).unwrap();
            let mut expected = whole.push(&samples, false).unwrap();
            expected.extend(whole.push(&[], true).unwrap());
            let mut streaming = PcmEncoder::new(true).unwrap();
            let mut actual = Vec::new();
            for chunk in samples.chunks(317) {
                actual.extend(streaming.push(chunk, false).unwrap());
            }
            actual.extend(streaming.push(&[], true).unwrap());
            assert_eq!(actual.len(), len * 3 / 2 * 2);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn every_underlying_io_observes_cancellation_and_absolute_deadline() {
        let control = SessionControl::new(None, None);
        let deadline = Instant::now() + Duration::from_secs(10);
        assert_eq!(
            io_budget(&control, deadline, Duration::from_millis(1)).unwrap(),
            Duration::from_millis(1)
        );
        assert_eq!(
            io_budget(&control, Instant::now(), Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        control.cancelled.store(true, Ordering::Release);
        assert_eq!(
            io_budget(&control, deadline, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
    }

    #[test]
    fn completed_fallback_preserves_the_original_job_cancellation_signal() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let control = SessionControl::new(None, Some(cancelled.clone()));
        assert!(Arc::ptr_eq(&control.cancelled, &cancelled));
        let deadline = Instant::now() + Duration::from_secs(10);
        assert!(io_budget(&control, deadline, Duration::from_millis(1)).is_ok());
        cancelled.store(true, Ordering::Release);
        assert_eq!(
            io_budget(&control, deadline, Duration::from_millis(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
        let result = transcribe_completed(
            &Config::default(),
            "openai::gpt-live-transcribe",
            &[0.25; 1600],
            &Snapshot::default(),
            Duration::from_secs(1),
            Some(cancelled),
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }

    #[test]
    fn session_permits_bound_workers_and_release_only_after_all_owners_drop() {
        let pool = Arc::new(AtomicUsize::new(0));
        let mut owners: Vec<_> = (0..MAX_SESSIONS)
            .map(|_| Arc::new(SessionPermit::acquire(pool.clone()).unwrap()))
            .collect();
        assert!(SessionPermit::acquire(pool.clone()).is_none());
        let dns = owners[0].clone();
        owners.remove(0);
        assert!(SessionPermit::acquire(pool.clone()).is_none());
        drop(dns);
        assert!(SessionPermit::acquire(pool.clone()).is_some());
        drop(owners);
        assert_eq!(pool.load(Ordering::Acquire), 0);
    }

    #[test]
    fn unit_tests_cannot_open_streaming_network_or_resolve_credentials() {
        assert!(LiveCapture::start().is_none());
        let result = transcribe_completed(
            &Config::default(),
            "openai::gpt-live-transcribe",
            &[0.25; 1600],
            &Snapshot::default(),
            Duration::from_secs(1),
            None,
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("disabled in tests")
        );
    }
}
