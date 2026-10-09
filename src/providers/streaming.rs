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
use crate::openrouter::stats::ErrorKind;
use crate::vocabulary::Snapshot;

const BLOCK: usize = 1_600; // 100 ms of mono 16 kHz audio.
const QUEUE_BLOCKS: usize = 32;
const MAX_TAIL: usize = BLOCK * QUEUE_BLOCKS;
const POLL: Duration = Duration::from_millis(20);
const MAX_LIFETIME: Duration = Duration::from_secs(60 * 60);
const GOOGLE_LIFETIME: Duration = Duration::from_secs(10 * 60);

fn session_lifetime(provider: Provider) -> Duration {
    if provider == Provider::Google {
        GOOGLE_LIFETIME
    } else {
        MAX_LIFETIME
    }
}
fn validate_audio_length(provider: Provider, samples: usize) -> Result<()> {
    if provider == Provider::Google && samples > GOOGLE_LIFETIME.as_secs() as usize * 16_000 {
        return Err(failure(
            "Google live transcription is limited to ten minutes; using the complete recording.",
        ));
    }
    Ok(())
}
const MAX_TEXT: usize = 2 * 1024 * 1024;
/// ElevenLabs commits on its own once roughly 36 s of audio accumulate, even
/// in manual mode, and each commit starts a new segment. A commit inside a
/// phrase can lose the words right after it, so commit first and in a real
/// pause: after 15 s once 400 ms have been quiet, or regardless at 30 s.
const ELEVENLABS_COMMIT_AFTER: usize = 15 * 16_000;
const ELEVENLABS_FORCE_COMMIT: usize = 30 * 16_000;
const ELEVENLABS_PAUSE: usize = 6_400;
/// About -38 dBFS for 16-bit PCM: a pause rather than speech.
const ELEVENLABS_QUIET_RMS: f64 = 400.0;

fn elevenlabs_commit_due(pending_frames: usize, quiet_frames: usize) -> bool {
    pending_frames >= ELEVENLABS_FORCE_COMMIT
        || (pending_frames >= ELEVENLABS_COMMIT_AFTER && quiet_frames >= ELEVENLABS_PAUSE)
}

/// A completed clip goes out in one-second messages: ElevenLabs closes the
/// session (`queue_overflow`) when 100 ms chunks arrive faster than real time.
const ELEVENLABS_COMPLETED_BLOCK: usize = 16_000;

/// Commit points for a completed ElevenLabs clip: the middle of the first
/// quietest 500 ms between 15 and 30 s after the previous commit, so each
/// commit falls in the clearest pause rather than inside a phrase.
fn elevenlabs_commit_points(samples: &[f32]) -> Vec<usize> {
    const WINDOW: usize = 5 * BLOCK;
    let energy = |start: usize| -> f64 {
        samples[start..start + WINDOW]
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum()
    };
    let mut points = Vec::new();
    let mut start = 0;
    while samples.len() - start > ELEVENLABS_FORCE_COMMIT {
        let quietest = (start + ELEVENLABS_COMMIT_AFTER..=start + ELEVENLABS_FORCE_COMMIT - WINDOW)
            .step_by(BLOCK)
            .min_by(|&a, &b| energy(a).total_cmp(&energy(b)))
            .unwrap_or(start + ELEVENLABS_FORCE_COMMIT - WINDOW);
        start = quietest + WINDOW / 2;
        points.push(start);
    }
    points
}

fn pcm16_rms(pcm: &[u8]) -> f64 {
    let samples = pcm.len() / 2;
    if samples == 0 {
        return 0.0;
    }
    let energy: f64 = pcm
        .chunks_exact(2)
        .map(|pair| f64::from(i16::from_le_bytes([pair[0], pair[1]])).powi(2))
        .sum();
    (energy / samples as f64).sqrt()
}
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
    pub attempted: bool,
    pub error_kind: ErrorKind,
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
    failure_kind(message, ErrorKind::Network)
}
fn failure_kind(message: &'static str, error_kind: ErrorKind) -> color_eyre::Report {
    LiveError {
        status: None,
        keyword_count: 0,
        keywords_rejected: false,
        attempted: false,
        error_kind,
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
        attempted: true,
        error_kind: status.map_or(ErrorKind::Network, ErrorKind::from_status),
        message: "Synthetic streaming failure.",
    }
    .into()
}

/// Replays the real Grok protocol offline through its capture/Finish boundary.
#[cfg(test)]
pub(crate) fn fixture_grok_live_result(
    before_finish: &[Value],
    after_finish: &[Value],
) -> Result<LiveResult> {
    let mut protocol = Protocol::new(Provider::Grok);
    protocol.event(&json!({"type":"transcript.created"}))?;
    for event in before_finish {
        if protocol.event(event)?.is_some() {
            return Err(fixture_error(None, false));
        }
    }
    protocol.finishing = true;
    for event in after_finish {
        if let Some(text) = protocol.event(event)? {
            return Ok(LiveResult {
                text,
                model: "grok::grok-voice-transcribe-2.0".into(),
                keyword_count: 2,
                latency_ms: 42,
            });
        }
    }
    Err(LiveError {
        status: None,
        keyword_count: 2,
        keywords_rejected: false,
        attempted: true,
        error_kind: ErrorKind::InvalidResponse,
        message: "Synthetic Grok stream ended without transcript.done.",
    }
    .into())
}

fn mark_attempt(mut error: color_eyre::Report, control: &SessionControl) -> color_eyre::Report {
    if let Some(error) = error.downcast_mut::<LiveError>() {
        error.attempted |= control.attempt_started.load(Ordering::Acquire);
        error.keyword_count = error
            .keyword_count
            .max(control.sent_keyword_count.load(Ordering::Acquire));
    }
    error
}

fn connect_error_kind(error: &io::Error) -> ErrorKind {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        _ => ErrorKind::Network,
    }
}

fn websocket_error_kind(error: &tungstenite::Error) -> ErrorKind {
    match error {
        tungstenite::Error::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            ErrorKind::Timeout
        }
        tungstenite::Error::Http(response) => ErrorKind::from_status(response.status().as_u16()),
        tungstenite::Error::Utf8(_)
        | tungstenite::Error::Capacity(_)
        | tungstenite::Error::Protocol(_) => ErrorKind::InvalidResponse,
        _ => ErrorKind::Network,
    }
}
fn websocket_failure(message: &'static str, error: &tungstenite::Error) -> color_eyre::Report {
    failure_kind(message, websocket_error_kind(error))
}
fn query_keywords(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Deepgram | Provider::Grok | Provider::ElevenLabs
    )
}
fn handshake_failure<S: Read + Write>(
    error: tungstenite::HandshakeError<tungstenite::handshake::client::ClientHandshake<S>>,
    query_keyword_count: usize,
) -> color_eyre::Report {
    let (status, keywords_rejected, keyword_count, error_kind) = match &error {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response)) => {
            let status = response.status().as_u16();
            let rejected = matches!(status, 400 | 422)
                && response
                    .body()
                    .as_ref()
                    .is_some_and(|body| rejected_keywords(&String::from_utf8_lossy(body)));
            // Every HTTP response confirms receipt of the request query, even
            // when upgrade is rejected for auth, rate limits or a redirect.
            // DNS/TCP/TLS and local credential failures do not prove delivery.
            (
                Some(status),
                rejected,
                query_keyword_count,
                ErrorKind::from_status(status),
            )
        }
        tungstenite::HandshakeError::Failure(error) => {
            (None, false, 0, websocket_error_kind(error))
        }
        // Our socket is blocking; Interrupted here is its bounded I/O wait.
        tungstenite::HandshakeError::Interrupted(_) => (None, false, 0, ErrorKind::Timeout),
    };
    LiveError {
        status,
        keyword_count,
        keywords_rejected,
        attempted: true,
        error_kind,
        message: "Streaming handshake failed.",
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
    attempt_started: AtomicBool,
    sent_keyword_count: AtomicUsize,
}
impl SessionControl {
    fn new(permit: Option<SessionPermit>, cancellation: Option<Arc<AtomicBool>>) -> Arc<Self> {
        Arc::new(Self {
            stopped: AtomicBool::new(false),
            cancelled: cancellation.unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            _permit: permit,
            sent_samples: Arc::new(AtomicUsize::new(0)),
            attempt_started: AtomicBool::new(false),
            sent_keyword_count: AtomicUsize::new(0),
        })
    }
    fn check(&self, deadline: Instant) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) {
            return Err(failure("Streaming transcription was cancelled."));
        }
        if Instant::now() >= deadline {
            return Err(failure_kind(
                "Streaming transcription timed out.",
                ErrorKind::Timeout,
            ));
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
    /// The worker already returned (for example without a key). Its own error
    /// is the result; audio is no longer sent and no other reason is invented.
    worker_ended: bool,
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
        let options = super::options(&config, id);
        if !options.streaming || !ModelRef::parse(id).can_stream_language(&options.language) {
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
                    let _ = send_result
                        .try_send(outcome.map_err(|error| mark_attempt(error, &worker_control)));
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
            worker_ended: false,
            invalid: (!admitted)
                .then_some("Streaming session limit reached; using the complete recording."),
        }
    }

    /// `prefix` is the exact immutable prefix of the eventual untrimmed clip.
    pub fn push_prefix(&mut self, prefix: &[f32]) {
        if self.invalid.is_some() || self.worker_ended {
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
            match self.audio.try_send(AudioBlock {
                start: self.queued,
                samples: samples.to_vec(),
            }) {
                Ok(()) => self.queued += samples.len(),
                Err(mpsc::TrySendError::Full(_)) => {
                    self.invalidate("Streaming audio fell behind; using the complete recording.");
                    break;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.worker_ended = true;
                    break;
                }
            }
        }
    }

    fn invalidate(&mut self, reason: &'static str) {
        self.invalid = Some(reason);
        self.control.stopped.store(true, Ordering::Release);
    }

    pub fn finish(mut self, samples: &[f32]) -> PendingLive {
        if self.worker_ended {
            // Keep the worker's own result rather than a generic sealing error.
        } else if samples.len() < self.queued {
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
        let attempted = result.as_ref().map_or_else(
            |error| {
                error
                    .downcast_ref::<LiveError>()
                    .is_none_or(|error| error.attempted)
            },
            |_| true,
        );
        sender.send(result).expect("fixture receiver is alive");
        let control = SessionControl::new(None, None);
        control.attempt_started.store(attempted, Ordering::Release);
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
        self.resolve_inner(timeout)
            .map_err(|error| mark_attempt(error, &self.control))
    }
    fn resolve_inner(&self, timeout: Duration) -> Result<LiveResult> {
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
    let result = (|| {
        let started = Instant::now();
        let provider = ModelRef::parse(model_id).provider;
        validate_audio_length(provider, samples.len())?;
        let deadline = (started + timeout).min(started + session_lifetime(provider));
        control.check(deadline)?;
        let mut connection = Connection::open(config, model_id, vocabulary, deadline, &control)?;
        connection.ready(deadline, &control)?;
        if provider == Provider::ElevenLabs {
            connection.send_completed_elevenlabs(samples, deadline, &control)?;
        } else {
            for chunk in samples.chunks(BLOCK) {
                control.check(deadline)?;
                connection.audio(chunk, deadline)?;
                connection.poll(deadline, &control)?;
            }
        }
        connection.finish(deadline)?;
        let text = connection.complete(deadline, &control)?;
        Ok(connection.result(text, started))
    })();
    result.map_err(|error| mark_attempt(error, &control))
}

fn run_live(
    config: &Config,
    vocabulary: &Snapshot,
    audio: mpsc::Receiver<AudioBlock>,
    seal: mpsc::Receiver<Seal>,
    control: &Arc<SessionControl>,
) -> Result<LiveResult> {
    let started = Instant::now();

    let attempt = Duration::from_secs(config.transcription.attempt_timeout_seconds.clamp(1, 600));
    let id = config
        .transcription
        .models
        .first()
        .ok_or_else(|| failure("No streaming model selected."))?;
    let lifetime = started + session_lifetime(ModelRef::parse(id).provider);
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
    if model.provider == Provider::Meta {
        // Validate the model/language before DNS or credential resolution.
        super::meta::settings(model.model, options, "PCM_16KHZ", keywords)
            .map_err(|_| failure("Invalid Muse Voice model or language hint."))?;
    }
    let base = match model.provider {
        Provider::OpenAi => "wss://api.openai.com/v1/realtime?intent=transcription",
        Provider::ElevenLabs => "wss://api.elevenlabs.io/v1/speech-to-text/realtime",
        Provider::Deepgram => "wss://api.deepgram.com/v1/listen",
        Provider::Grok => "wss://api.x.ai/v1/stt",
        Provider::Meta => "wss://api.meta.ai/v1/asr/realtime",
        Provider::Google => {
            "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent"
        }
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
        Provider::Grok => {
            let format =
                options.smart_format && super::batch::grok_format_supported(&options.language);
            let mut query = url.query_pairs_mut();
            query
                .append_pair("model", model.model)
                .append_pair("encoding", "pcm")
                .append_pair("sample_rate", "16000")
                .append_pair("interim_results", "false")
                .append_pair("format", if format { "true" } else { "false" })
                .append_pair(
                    "filler_words",
                    if options.no_verbatim { "false" } else { "true" },
                );
            if !auto {
                query.append_pair("language", &options.language);
            }
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
    [
        "keyword",
        "keyterm",
        "prompt",
        "customvocabulary",
        "custom_vocabulary",
    ]
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

fn streaming_keywords(model: ModelRef<'_>, vocabulary: &Snapshot) -> Vec<String> {
    super::batch::keywords(model, vocabulary)
}

fn google_setup(model: &str, options: &ModelOptions, keywords: &[String]) -> Value {
    let language = if options.language == "zh" {
        Some("cmn-Hans-CN")
    } else {
        super::bcp47_language(&options.language)
    };
    let languages: Vec<_> = language.into_iter().collect();
    let mut transcription = json!({"languageCodes":languages,"mode":if options.smart_format {"SMART"} else {"VERBATIM"}});
    if !keywords.is_empty() {
        transcription["customVocabulary"] = json!(keywords);
    }
    json!({"setup":{"model":format!("models/{model}"),
        "generationConfig":{"responseModalities":["TEXT"]},
        "inputAudioTranscription":transcription,
        "realtimeInputConfig":{"automaticActivityDetection":{"disabled":true}}
    }})
}

fn authentication(
    provider: Provider,
    key: &str,
) -> Result<(&'static str, tungstenite::http::HeaderValue)> {
    if provider == Provider::Meta {
        return Err(failure("Meta authenticates in its first session frame."));
    }
    let (header, value) = match provider {
        Provider::Google => ("x-goog-api-key", key.to_owned()),
        Provider::ElevenLabs => ("xi-api-key", key.to_owned()),
        Provider::Deepgram => ("authorization", format!("Token {key}")),
        _ => ("authorization", format!("Bearer {key}")),
    };
    let mut value = tungstenite::http::HeaderValue::from_str(&value)
        .map_err(|_| failure("Invalid streaming credential."))?;
    value.set_sensitive(true);
    Ok((header, value))
}

fn finish_message(provider: Provider) -> Result<Value> {
    match provider {
        Provider::OpenAi => Ok(json!({"type":"input_audio_buffer.commit"})),
        Provider::ElevenLabs => Ok(
            json!({"message_type":"input_audio_chunk","audio_base_64":"","commit":true,"sample_rate":16000}),
        ),
        Provider::Deepgram => Ok(json!({"type":"CloseStream"})),
        Provider::Grok => Ok(json!({"type":"audio.done"})),
        Provider::Google => Ok(json!({"realtimeInput":{"activityEnd":{}}})),
        Provider::Meta => Ok(json!({"type":"endStream"})),
        Provider::OpenRouter => Err(failure("Unsupported streaming provider.")),
    }
}

const MAX_GROK_FINALS: usize = 10_000;

struct GrokFinal {
    text: String,
    utterance: bool,
}

/// Only provider-final text is retained. Timing identifies replays/corrections;
/// equal words at different times are legitimate speech, never text duplicates.
#[derive(Default)]
struct GrokFinals {
    timed: Option<bool>,
    spans: BTreeMap<(u64, u64), GrokFinal>,
    closed: Vec<String>,
    open: Vec<String>,
    bytes: usize,
}
impl GrokFinals {
    fn retain(&mut self, event: &Value) -> Result<()> {
        if event["is_final"].as_bool() != Some(true) {
            return Ok(());
        }
        let text = event
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| grok_invalid("Grok returned a final without text."))?
            .trim();
        let utterance = match event.get("speech_final") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| grok_invalid("Grok returned an invalid final marker."))?,
        };
        if text.is_empty() {
            // An empty utterance boundary closes existing untimed chunks; the
            // next utterance's stitched text must not replace the previous one.
            if utterance && self.timed == Some(false) {
                self.closed.append(&mut self.open);
            }
            return Ok(());
        }
        let span = grok_span(event)?;
        let timed = span.is_some();
        if self.timed.is_some_and(|previous| previous != timed) {
            return Err(grok_invalid(
                "Grok final events changed their timing representation.",
            ));
        }
        self.timed = Some(timed);
        if let Some(span) = span {
            self.retain_timed(span, text, utterance)
        } else {
            self.retain_untimed(text, utterance)
        }
    }

    fn retain_timed(&mut self, span: (u64, u64), text: &str, utterance: bool) -> Result<()> {
        // A same-start final of the same class is a revision even when its
        // duration is adjusted. It may not absorb any neighboring interval.
        let revising = self
            .spans
            .iter()
            .any(|(old_span, old)| old_span.0 == span.0 && old.utterance == utterance);
        let mut replaced = Vec::new();
        for (&old_span, old) in &self.spans {
            if span.0 >= old_span.1 || old_span.0 >= span.1 {
                continue;
            }
            if old.utterance && !utterance && old_span.0 <= span.0 && old_span.1 >= span.1 {
                // A late chunk cannot overwrite its already stitched utterance.
                return Ok(());
            }
            if (old_span.0 == span.0 && old.utterance == utterance)
                || old_span == span
                || (!revising
                    && utterance
                    && !old.utterance
                    && span.0 <= old_span.0
                    && span.1 >= old_span.1)
            {
                replaced.push(old_span);
            } else {
                return Err(grok_invalid(
                    "Grok final transcript intervals overlap ambiguously.",
                ));
            }
        }
        let removed: usize = replaced.iter().map(|key| self.spans[key].text.len()).sum();
        let bytes = self.bytes - removed + text.len();
        let count = self.spans.len() - replaced.len() + 1;
        grok_bound(bytes, count)?;
        for key in replaced {
            self.spans.remove(&key);
        }
        self.spans.insert(
            span,
            GrokFinal {
                text: text.to_owned(),
                utterance,
            },
        );
        self.bytes = bytes;
        Ok(())
    }

    fn retain_untimed(&mut self, text: &str, utterance: bool) -> Result<()> {
        // Without timestamps the WebSocket arrival order is the only identity.
        // Never deduplicate equal strings: the speaker may have repeated them.
        let removed = if utterance {
            self.open.iter().map(String::len).sum()
        } else {
            0
        };
        let bytes = self.bytes - removed + text.len();
        let count = self.closed.len() + if utterance { 1 } else { self.open.len() + 1 };
        grok_bound(bytes, count)?;
        if utterance {
            self.open.clear();
            self.closed.push(text.to_owned());
        } else {
            self.open.push(text.to_owned());
        }
        self.bytes = bytes;
        Ok(())
    }

    fn finish(&self, event: &Value) -> Result<String> {
        match event.get("text") {
            Some(Value::String(text)) if !text.trim().is_empty() => {
                // xAI documents a nonempty done transcript as the full result.
                // Do not guess tail/full semantics from textual prefixes.
                return final_text(event.get("text"));
            }
            None | Some(Value::Null) | Some(Value::String(_)) => {}
            Some(_) => {
                return Err(grok_invalid(
                    "Grok returned an invalid terminal transcript.",
                ));
            }
        }
        if self.timed == Some(true) {
            Ok(self
                .spans
                .values()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join(" "))
        } else {
            Ok(self
                .closed
                .iter()
                .chain(self.open.iter())
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" "))
        }
    }
}

fn grok_invalid(message: &'static str) -> color_eyre::Report {
    failure_kind(message, ErrorKind::InvalidResponse)
}
fn grok_bound(bytes: usize, parts: usize) -> Result<()> {
    if parts > MAX_GROK_FINALS || bytes.saturating_add(parts.saturating_sub(1)) > MAX_TEXT {
        return Err(grok_invalid(
            "Grok final transcript exceeded its storage limit.",
        ));
    }
    Ok(())
}
fn grok_span(event: &Value) -> Result<Option<(u64, u64)>> {
    let (start, duration) = match (event.get("start"), event.get("duration")) {
        (None, None) => return Ok(None),
        (Some(start), Some(duration)) => (start.as_f64(), duration.as_f64()),
        _ => return Err(grok_invalid("Grok final transcript timing is incomplete.")),
    };
    let start = start
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| grok_invalid("Grok final transcript has invalid start timing."))?;
    let duration = duration
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| grok_invalid("Grok final transcript has invalid duration."))?;
    let first = (start * 16_000.0).round();
    let last = ((start + duration) * 16_000.0).round();
    if !last.is_finite() || last >= u64::MAX as f64 || last <= first {
        return Err(grok_invalid(
            "Grok final transcript timing is out of range.",
        ));
    }
    Ok(Some((first as u64, last as u64)))
}

struct Protocol {
    provider: Provider,
    ready: bool,
    finishing: bool,
    google_text: String,
    google_finished: bool,
    google_turn_complete: bool,
    meta_final: Option<String>,
    commit_item: Option<String>,
    completed_items: BTreeMap<String, String>,
    segments: BTreeMap<(u64, u64), String>,
    grok: GrokFinals,
    /// ElevenLabs: commits sent before Finish, audio since the last one,
    /// whether Finish needed no commit of its own, and segments received.
    elevenlabs_commits: usize,
    elevenlabs_pending: usize,
    /// Consecutive quiet audio at the end of what was sent.
    elevenlabs_quiet: usize,
    elevenlabs_skip_final: bool,
    elevenlabs_segments: Vec<String>,
}
impl Protocol {
    fn new(provider: Provider) -> Self {
        Self {
            provider,
            ready: provider == Provider::Deepgram,
            finishing: false,
            google_text: String::new(),
            google_finished: false,
            google_turn_complete: false,
            meta_final: None,
            commit_item: None,
            completed_items: BTreeMap::new(),
            segments: BTreeMap::new(),
            grok: GrokFinals::default(),
            elevenlabs_commits: 0,
            elevenlabs_pending: 0,
            elevenlabs_quiet: 0,
            elevenlabs_skip_final: false,
            elevenlabs_segments: Vec::new(),
        }
    }
    /// The joined transcript once every requested commit has answered after
    /// Finish. A segment nobody asked for is an automatic commit we did not
    /// anticipate, so the live result cannot be trusted.
    fn elevenlabs_result(&self) -> Result<Option<String>> {
        let expected =
            self.elevenlabs_commits + usize::from(self.finishing && !self.elevenlabs_skip_final);
        if self.elevenlabs_segments.len() > expected {
            return Err(failure("The streaming provider committed before Finish."));
        }
        if !self.finishing || self.elevenlabs_segments.len() < expected {
            return Ok(None);
        }
        Ok(Some(
            self.elevenlabs_segments
                .iter()
                .map(|segment| segment.trim())
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        ))
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
                .or_else(|| event.pointer("/error/code"))
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .filter(|status| (100..=599).contains(status))
                .or_else(|| {
                    if self.provider != Provider::Meta {
                        return None;
                    }
                    // Voice errorType uses the documented Model API taxonomy.
                    match event["errorType"].as_str() {
                        Some("authentication_error") => Some(401),
                        Some("rate_limit_error") => Some(429),
                        Some("server_error") => Some(500),
                        _ => None,
                    }
                });
            return Err(LiveError {
                status,
                keyword_count: 0,
                keywords_rejected: (self.provider != Provider::Meta
                    || status.is_none_or(|status| matches!(status, 400 | 422)))
                    && rejected_keywords(&event.to_string()),
                attempted: true,
                error_kind: if self.provider == Provider::Meta
                    && event["errorCode"].as_str() == Some("gateway_timeout")
                {
                    ErrorKind::Timeout
                } else {
                    status.map_or(ErrorKind::Rejected, ErrorKind::from_status)
                },
                message: "The streaming provider rejected the request.",
            }
            .into());
        }
        match self.provider {
            Provider::Meta => {
                if !self.ready {
                    if event.get("type").is_none()
                        && event
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .is_some_and(|id| !id.is_empty())
                    {
                        self.ready = true;
                        return Ok(None);
                    }
                    return Err(failure(
                        "Meta sent an event before acknowledging its session.",
                    ));
                }
                if kind == "transcript" {
                    if self.meta_final.is_some() {
                        return Err(failure("Meta sent a transcript after its final result."));
                    }
                    if event["final"].as_bool() == Some(true) {
                        if !self.finishing {
                            return Err(failure("Meta finalized transcription before Finish."));
                        }
                        // Cumulative final replaces every partial. Wait for the
                        // server's normal close before accepting it as complete.
                        self.meta_final = Some(final_text(event.get("transcript"))?);
                    }
                }
            }
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
                    let text = event["text"]
                        .as_str()
                        .ok_or_else(|| failure("Streaming response had no final text."))?;
                    let retained: usize = self.elevenlabs_segments.iter().map(String::len).sum();
                    if retained.saturating_add(text.len()) > MAX_TEXT {
                        return Err(failure("Streaming response exceeded its limit."));
                    }
                    self.elevenlabs_segments.push(text.to_owned());
                    return self.elevenlabs_result();
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
            Provider::Grok => {
                if kind == "transcript.created" {
                    self.ready = true;
                }
                if kind == "transcript.partial" {
                    self.grok.retain(event)?;
                }
                if kind == "transcript.done" {
                    if !self.finishing {
                        return Err(failure("The streaming provider committed before Finish."));
                    }
                    // Finals may already have been delivered in partial events.
                    // Only this terminal event authorizes releasing their text.
                    return self.grok.finish(event).map(Some);
                }
            }
            Provider::Google => {
                if event.get("setupComplete").is_some_and(Value::is_object) {
                    self.ready = true;
                }
                if event.get("goAway").is_some() {
                    return Err(failure(
                        "Google live session is ending; using the complete recording.",
                    ));
                }
                if let Some(content) = event.get("serverContent") {
                    if content["interrupted"].as_bool() == Some(true) {
                        return Err(failure("Google live transcription was interrupted."));
                    }
                    if let Some(transcript) = content.get("inputTranscription") {
                        if self.google_finished {
                            return Err(failure(
                                "Google sent audio transcription after its final marker.",
                            ));
                        }
                        if let Some(text) = transcript.get("text") {
                            let text = text.as_str().ok_or_else(|| {
                                failure("Google returned invalid transcription text.")
                            })?;
                            if self.google_text.len().saturating_add(text.len()) > MAX_TEXT {
                                return Err(failure("Streaming response exceeded its limit."));
                            }
                            self.google_text.push_str(text);
                        }
                        if transcript["finished"].as_bool() == Some(true) {
                            if !self.finishing {
                                return Err(failure(
                                    "Google finalized transcription before Finish.",
                                ));
                            }
                            self.google_finished = true;
                        }
                    }
                    if content["turnComplete"].as_bool() == Some(true) {
                        if !self.finishing {
                            return Err(failure("Google completed its turn before Finish."));
                        }
                        self.google_turn_complete = true;
                    }
                    // The input transcription is independent of turnComplete.
                    // The SDK's explicit finished flag avoids accepting the first
                    // late fragment as a full result when turnComplete arrives first.
                    if self.finishing && self.google_finished && self.google_turn_complete {
                        return Ok(Some(std::mem::take(&mut self.google_text)));
                    }
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
    fn meta_closed(&self, code: Option<u16>) -> Result<Option<String>> {
        match code {
            Some(1000) if self.ready && self.finishing && self.meta_final.is_some() => {
                Ok(self.meta_final.clone())
            }
            Some(1013) => Err(failure_kind(
                "Meta rate limited the live session.",
                ErrorKind::RateLimited,
            )),
            Some(1011) => Err(failure_kind(
                "Meta could not complete the live session.",
                ErrorKind::Server,
            )),
            Some(1008) => Err(failure_kind(
                "Meta rejected the live session or its audio pacing.",
                ErrorKind::Rejected,
            )),
            _ => self.closed(),
        }
    }
}

fn final_text(value: Option<&Value>) -> Result<String> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| failure("Streaming response had no final text."))?;
    if text.len() > MAX_TEXT {
        return Err(failure("Streaming response exceeded its limit."));
    }
    Ok(text.to_owned())
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
    activity_started: bool,
    pacing_started: Option<Instant>,
    /// ElevenLabs: whether the next chunk commits, when the caller planned
    /// the commits (completed clips) instead of detecting pauses (live).
    planned_commit: Option<bool>,
    control: Arc<SessionControl>,
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
        let keywords = streaming_keywords(model, vocabulary);
        let url = endpoint(model, &options, &keywords)?;
        // Validation above is preflight. From credential resolution onward this
        // is an attempted provider request, even when authentication fails before
        // bytes reach the wire (the UI deliberately calls these attempts).
        control.attempt_started.store(true, Ordering::Release);
        let key = super::keys::api_key(model.provider, config).map_err(|_| {
            failure_kind(
                "The streaming provider has no available API key.",
                ErrorKind::Auth,
            )
        })?;
        control.check(deadline)?;
        control.check(deadline)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| failure("Could not create streaming request."))?;
        if model.provider != Provider::Meta {
            let (header, value) = authentication(model.provider, &key)?;
            request.headers_mut().insert(header, value);
        }
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
            .map_err(|_| failure_kind("Streaming DNS lookup timed out.", ErrorKind::Timeout))?
            .map_err(|_| failure("Streaming DNS lookup failed."))?;
        let mut connected = None;
        let mut last_connect_error = ErrorKind::Network;
        for address in addresses {
            control.check(deadline)?;
            match TcpStream::connect_timeout(
                &address,
                Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())),
            ) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last_connect_error = connect_error_kind(&error),
            }
        }
        control.check(deadline)?;
        let stream = connected.ok_or_else(|| {
            failure_kind(
                "Could not connect to streaming provider.",
                last_connect_error,
            )
        })?;
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
            handshake_failure(
                error,
                if query_keywords(model.provider) {
                    keywords.len()
                } else {
                    0
                },
            )
        })?;
        if query_keywords(model.provider) {
            control
                .sent_keyword_count
                .store(keywords.len(), Ordering::Release);
        }
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
            activity_started: false,
            pacing_started: None,
            planned_commit: None,
            control: control.clone(),
        };
        match model.provider {
            Provider::Meta => {
                let setup = super::meta::handshake(model.model, &options, &keywords, &key)
                    .map_err(|_| failure("Invalid Muse Voice model or language hint."))?;
                connection.json(setup)?;
                control
                    .sent_keyword_count
                    .store(keywords.len(), Ordering::Release);
            }
            Provider::OpenAi => {
                connection.json(session_update(model.model, &options, &keywords))?;
                control
                    .sent_keyword_count
                    .store(keywords.len(), Ordering::Release);
            }
            Provider::Google => {
                connection.json(google_setup(model.model, &options, &keywords))?;
                control
                    .sent_keyword_count
                    .store(keywords.len(), Ordering::Release);
            }
            _ => {}
        }
        Ok(connection)
    }
    fn json(&mut self, value: Value) -> Result<()> {
        self.wire(Message::Text(value.to_string().into()))
    }
    fn wire(&mut self, message: Message) -> Result<()> {
        self.socket
            .send(message)
            .map_err(|error| websocket_failure("Streaming write failed.", &error))?;
        self.last_send = Instant::now();
        Ok(())
    }
    fn ready(&mut self, deadline: Instant, control: &SessionControl) -> Result<()> {
        while !self.protocol.ready {
            self.poll(deadline, control)?;
        }
        if self.protocol.provider == Provider::Google && !self.activity_started {
            self.set_deadline(deadline);
            self.json(json!({"realtimeInput":{"activityStart":{}}}))?;
            self.activity_started = true;
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
        validate_audio_length(
            self.protocol.provider,
            self.encoder.source.saturating_add(samples.len()),
        )?;
        if matches!(self.protocol.provider, Provider::Grok | Provider::Meta) {
            // Grok/Meta run at real time. Pace only on this worker; capture
            // remains nonblocking.
            let target = *self.pacing_started.get_or_insert_with(Instant::now)
                + Duration::from_secs_f64(self.wire_frames as f64 / 16_000.0);
            let control = self.control.clone();
            while Instant::now() < target {
                self.poll(deadline, &control)?;
                std::thread::sleep(POLL.min(target.saturating_duration_since(Instant::now())));
            }
        }
        let pcm = self.encoder.push(samples, false)?;
        self.send_pcm(pcm)
    }
    fn send_pcm(&mut self, pcm: Vec<u8>) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let frames = pcm.len() / 2;
        {
            match self.protocol.provider {
            Provider::OpenAi => self.json(json!({"type":"input_audio_buffer.append","audio":crate::openrouter::transcribe::encode_base64(&pcm)})),
            Provider::ElevenLabs => {
                let pending = self.protocol.elevenlabs_pending + frames;
                let quiet = if pcm16_rms(&pcm) < ELEVENLABS_QUIET_RMS {
                    self.protocol.elevenlabs_quiet + frames
                } else {
                    0
                };
                let commit = match self.planned_commit.as_mut() {
                    Some(planned) => std::mem::take(planned),
                    None => elevenlabs_commit_due(pending, quiet),
                };
                self.protocol.elevenlabs_quiet = if commit { 0 } else { quiet };
                self.json(json!({"message_type":"input_audio_chunk","audio_base_64":crate::openrouter::transcribe::encode_base64(&pcm),"commit":commit,"sample_rate":16000}))?;
                if commit {
                    self.protocol.elevenlabs_commits += 1;
                    self.protocol.elevenlabs_pending = 0;
                } else {
                    self.protocol.elevenlabs_pending = pending;
                }
                Ok(())
            }
            Provider::Google => self.json(json!({"realtimeInput":{"audio":{"data":crate::openrouter::transcribe::encode_base64(&pcm),"mimeType":"audio/pcm;rate=16000"}}})),
            Provider::Deepgram | Provider::Grok | Provider::Meta => {
                self.socket.send(Message::Binary(pcm.into())).map_err(|error| websocket_failure("Streaming write failed.", &error))?;
                self.last_send = Instant::now();
                Ok(())
            }
            _ => Err(failure("Unsupported streaming provider.")),
        }?;
        }
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
        if self.protocol.provider == Provider::ElevenLabs
            && self.protocol.elevenlabs_pending == 0
            && self.protocol.elevenlabs_commits > 0
        {
            // The last audio already went out with a commit; an empty one
            // would only ask for an extra, empty segment.
            self.protocol.elevenlabs_skip_final = true;
            if let Some(text) = self.protocol.elevenlabs_result()? {
                self.completed = Some(text);
            }
            return Ok(());
        }
        self.json(finish_message(self.protocol.provider)?)
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
            Ok(Message::Binary(bytes)) if self.protocol.provider == Provider::Google => {
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| failure("Invalid Google streaming response."))?;
                self.protocol.event(&value)
            }
            Ok(Message::Close(frame)) => {
                if self.protocol.provider == Provider::Meta {
                    self.protocol
                        .meta_closed(frame.map(|frame| frame.code.into()))
                } else {
                    if frame.is_some_and(|frame| {
                        frame.code != tungstenite::protocol::frame::coding::CloseCode::Normal
                    }) {
                        return Err(failure(
                            "Streaming provider closed the connection with an error.",
                        ));
                    }
                    self.protocol.closed()
                }
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
            Err(error) => Err(websocket_failure("Streaming read failed.", &error)),
            _ => Ok(None),
        }?;
        if result.is_some() {
            self.completed = result;
        }
        Ok(())
    }
    /// Sends a completed clip segment by segment, committing in pauses and
    /// waiting for each committed transcript before the next segment, so no
    /// audio arrives while ElevenLabs finalizes the previous one.
    fn send_completed_elevenlabs(
        &mut self,
        samples: &[f32],
        deadline: Instant,
        control: &SessionControl,
    ) -> Result<()> {
        self.planned_commit = Some(false);
        let mut start = 0;
        for point in elevenlabs_commit_points(samples) {
            self.send_completed_segment(&samples[start..point], true, deadline, control)?;
            while self.protocol.elevenlabs_segments.len() < self.protocol.elevenlabs_commits {
                self.poll(deadline, control)?;
            }
            start = point;
        }
        self.send_completed_segment(&samples[start..], false, deadline, control)
    }
    fn send_completed_segment(
        &mut self,
        segment: &[f32],
        commit: bool,
        deadline: Instant,
        control: &SessionControl,
    ) -> Result<()> {
        let count = segment.len().div_ceil(ELEVENLABS_COMPLETED_BLOCK);
        for (index, chunk) in segment.chunks(ELEVENLABS_COMPLETED_BLOCK).enumerate() {
            control.check(deadline)?;
            self.planned_commit = Some(commit && index + 1 == count);
            self.audio(chunk, deadline)?;
            self.poll(deadline, control)?;
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

    #[test]
    fn meta_authenticates_only_in_its_handshake_and_preserves_pcm16_contract() {
        let options = ModelOptions {
            language: "pt".into(),
            ..Default::default()
        };
        let model = ModelRef::parse("meta::muse-voice-transcribe-1.0");
        let words = ["Nimbus Files".into()];
        let url = endpoint(model, &options, &words).unwrap();
        assert_eq!(url.as_str(), "wss://api.meta.ai/v1/asr/realtime");
        assert!(authentication(Provider::Meta, "fixture-only-key").is_err());
        assert!(!query_keywords(Provider::Meta));
        let setup =
            super::super::meta::handshake(model.model, &options, &words, "fixture-only-key")
                .unwrap();
        assert_eq!(
            setup["authorization"]["accessToken"],
            "Bearer fixture-only-key"
        );
        assert_eq!(setup["audioEncoding"], "PCM_16KHZ");
        assert_eq!(setup["languageBias"], json!(["Portuguese"]));
        assert_eq!(setup["keywords"], json!(["Nimbus Files"]));
        assert_eq!(
            finish_message(Provider::Meta).unwrap(),
            json!({"type":"endStream"})
        );
        let mut encoder = PcmEncoder::new(false).unwrap();
        let pcm = encoder.push(&[0.0; 1600], false).unwrap();
        assert_eq!(pcm.len(), 3200);
        assert!(encoder.push(&[], true).unwrap().is_empty());
        assert!(
            endpoint(
                model,
                &ModelOptions {
                    language: "ru".into(),
                    ..Default::default()
                },
                &words
            )
            .is_err()
        );
    }

    fn ready_meta() -> Protocol {
        let mut protocol = Protocol::new(Provider::Meta);
        assert!(!protocol.ready);
        protocol
            .event(&json!({"sessionId":"synthetic-session"}))
            .unwrap();
        assert!(protocol.ready);
        protocol
    }

    #[test]
    fn meta_waits_for_final_and_explicit_normal_close_never_pasting_partials() {
        let mut protocol = ready_meta();
        for text in ["Nimb", "Nimbus wrong", "Nimbus Files"] {
            assert!(
                protocol
                    .event(&json!({"type":"transcript", "transcript":text, "final":false}))
                    .unwrap()
                    .is_none()
            );
        }
        protocol.finishing = true;
        // End-of-speech/segment events are not full PUSH_TO_TALK completion.
        for event in [
            json!({"type":"speechEnd"}),
            json!({"type":"speechComplete","transcript":"partial"}),
            json!({"type":"futureEvent"}),
        ] {
            assert!(protocol.event(&event).unwrap().is_none());
        }
        assert!(protocol.meta_closed(Some(1000)).is_err());
        assert!(
            protocol
                .event(
                    &json!({"type":"transcript","transcript":"Nimbus Files is ready.","final":true})
                )
                .unwrap()
                .is_none()
        );
        // A TCP close or any abnormal close discards the speculative result.
        assert!(protocol.closed().is_err());
        for code in [None, Some(1006), Some(1008), Some(1011), Some(1013)] {
            assert!(protocol.meta_closed(code).is_err());
        }
        assert_eq!(
            protocol.meta_closed(Some(1000)).unwrap(),
            Some("Nimbus Files is ready.".into())
        );
    }

    #[test]
    fn meta_rejects_early_malformed_oversized_or_conflicting_finals() {
        for ack in [
            json!({}),
            json!({"sessionId":""}),
            json!({"type":"transcript","sessionId":"fixture"}),
        ] {
            assert!(Protocol::new(Provider::Meta).event(&ack).is_err());
        }
        assert!(
            ready_meta()
                .event(&json!({"type":"transcript","transcript":"early","final":true}))
                .is_err()
        );
        for text in [Value::Null, json!(123), json!("x".repeat(MAX_TEXT + 1))] {
            let mut protocol = ready_meta();
            protocol.finishing = true;
            assert!(
                protocol
                    .event(&json!({"type":"transcript","transcript":text,"final":true}))
                    .is_err()
            );
        }
        let mut protocol = ready_meta();
        protocol.finishing = true;
        protocol
            .event(&json!({"type":"transcript","transcript":"full","final":true}))
            .unwrap();
        assert!(
            protocol
                .event(&json!({"type":"transcript","transcript":"late","final":false}))
                .is_err()
        );
        for (code, kind) in [
            (1008, ErrorKind::Rejected),
            (1011, ErrorKind::Server),
            (1013, ErrorKind::RateLimited),
        ] {
            let error = protocol.meta_closed(Some(code)).unwrap_err();
            assert_eq!(error.downcast_ref::<LiveError>().unwrap().error_kind, kind);
        }
        let error = ready_meta().event(&json!({"type":"error","message":"keywords unsupported PRIVATE_MARKER","errorParam":"keywords"})).unwrap_err();
        assert!(error.downcast_ref::<LiveError>().unwrap().keywords_rejected);
        assert!(!error.to_string().contains("PRIVATE_MARKER"));
        for (error_type, status, kind) in [
            ("authentication_error", 401, ErrorKind::Auth),
            ("rate_limit_error", 429, ErrorKind::RateLimited),
            ("server_error", 500, ErrorKind::Server),
        ] {
            let error = ready_meta()
                .event(&json!({
                    "type":"error", "errorType":error_type,
                    "message":"PRIVATE_MARKER keywords are unavailable"
                }))
                .unwrap_err();
            let typed = error.downcast_ref::<LiveError>().unwrap();
            assert_eq!(typed.status, Some(status));
            assert_eq!(typed.error_kind, kind);
            assert!(!typed.keywords_rejected);
            assert!(!error.to_string().contains("PRIVATE_MARKER"));
        }
    }

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
            worker_ended: false,
            invalid: None,
        };
        (live, audio_rx, seal_rx, send_result)
    }

    #[test]
    fn grok_format_and_keywords_are_encoded_only_for_supported_options() {
        let model = ModelRef::parse("grok::grok-voice-transcribe-2.0");
        for (language, expected) in [("pt", "true"), ("auto", "false"), ("ko", "false")] {
            let options = ModelOptions {
                language: language.into(),
                smart_format: true,
                no_verbatim: true,
                ..Default::default()
            };
            let url = endpoint(model, &options, &["Name & Sons".into(), "Other".into()]).unwrap();
            assert_eq!(url.host_str(), Some("api.x.ai"));
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "format" && v == expected)
            );
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "encoding" && v == "pcm")
            );
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "sample_rate" && v == "16000")
            );
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "filler_words" && v == "false")
            );
            assert_eq!(url.query_pairs().filter(|(k, _)| k == "keyterm").count(), 2);
            assert_eq!(
                url.query_pairs().any(|(k, _)| k == "language"),
                language != "auto"
            );
        }
        let (name, value) = authentication(Provider::Grok, "fixture-xai-key").unwrap();
        assert_eq!(name, "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer fixture-xai-key");
        assert!(value.is_sensitive());
    }

    #[test]
    fn grok_final_partial_survives_an_empty_done() {
        let before = [
            json!({"type":"transcript.partial","is_final":true,"speech_final":true,"start":0.0,"duration":1.0,"text":"Confirmed words"}),
        ];
        let after = [json!({"type":"transcript.done","duration":1.0,"text":""})];
        assert_eq!(
            fixture_grok_live_result(&before, &after).unwrap().text,
            "Confirmed words"
        );
    }

    fn grok_segment(start: f64, duration: f64, text: &str, utterance: bool) -> Value {
        json!({"type":"transcript.partial","is_final":true,"speech_final":utterance,"start":start,"duration":duration,"text":text})
    }
    fn empty_grok_done() -> Value {
        json!({"type":"transcript.done","duration":20.0,"text":""})
    }

    #[test]
    fn grok_stitched_utterance_replaces_only_its_covered_chunks() {
        let before = [
            grok_segment(0.0, 2.0, "First utterance.", true),
            grok_segment(3.0, 3.0, "Old chunk", false),
            grok_segment(6.0, 3.0, "second chunk", false),
            grok_segment(3.0, 6.0, "Corrected second utterance.", true),
            // A delayed already-covered chunk cannot undo the stitched result.
            grok_segment(6.0, 3.0, "second chunk", false),
        ];
        assert_eq!(
            fixture_grok_live_result(&before, &[empty_grok_done()])
                .unwrap()
                .text,
            "First utterance. Corrected second utterance."
        );
    }

    #[test]
    fn grok_timing_orders_finals_and_preserves_legitimate_repeated_speech() {
        let before = [
            grok_segment(2.0, 1.0, "sim", true),
            grok_segment(0.0, 1.0, "sim", true),
            grok_segment(2.0, 1.0, "sim", true), // replay, not a third occurrence
        ];
        assert_eq!(
            fixture_grok_live_result(&before, &[empty_grok_done()])
                .unwrap()
                .text,
            "sim sim"
        );
        let corrected = [
            grok_segment(0.0, 1.0, "old", false),
            grok_segment(0.0, 1.0, "corrected", false),
        ];
        assert_eq!(
            fixture_grok_live_result(&corrected, &[empty_grok_done()])
                .unwrap()
                .text,
            "corrected"
        );
    }

    #[test]
    fn grok_same_start_same_class_revisions_can_adjust_duration_without_duplicates() {
        for utterance in [false, true] {
            let before = [
                grok_segment(0.0, 1.0, "original", utterance),
                grok_segment(0.0, 1.01, "expanded", utterance),
                grok_segment(0.0, 0.99, "corrected", utterance),
                grok_segment(2.0, 1.0, "next", utterance),
            ];
            assert_eq!(
                fixture_grok_live_result(&before, &[empty_grok_done()])
                    .unwrap()
                    .text,
                "corrected next"
            );
        }
        for neighbor_utterance in [false, true] {
            let mut finals = GrokFinals::default();
            finals.retain(&grok_segment(0.0, 1.0, "A", true)).unwrap();
            finals
                .retain(&grok_segment(1.5, 1.0, "B", neighbor_utterance))
                .unwrap();
            assert!(
                finals
                    .retain(&grok_segment(0.0, 3.0, "revision crossing neighbor", true))
                    .is_err()
            );
            assert_eq!(finals.finish(&empty_grok_done()).unwrap(), "A B");
        }
    }

    #[test]
    fn grok_interim_text_never_replaces_finals_and_done_full_is_authoritative() {
        let before = [
            grok_segment(0.0, 1.0, "Confirmed", true),
            json!({"type":"transcript.partial","is_final":false,"speech_final":false,"start":0.0,"duration":2.0,"text":"PREVIEW must not paste"}),
        ];
        assert_eq!(
            fixture_grok_live_result(&before, &[empty_grok_done()])
                .unwrap()
                .text,
            "Confirmed"
        );
        let done =
            json!({"type":"transcript.done","duration":2.0,"text":"Canonical full transcript"});
        assert_eq!(
            fixture_grok_live_result(&before, &[done]).unwrap().text,
            "Canonical full transcript"
        );
        assert_eq!(
            fixture_grok_live_result(&before, &[json!({"type":"transcript.done","duration":2.0})])
                .unwrap()
                .text,
            "Confirmed"
        );
    }

    #[test]
    fn grok_untimed_finals_keep_arrival_order_without_text_deduplication() {
        let partial = |text, utterance| json!({"type":"transcript.partial","is_final":true,"speech_final":utterance,"text":text});
        let before = [
            partial("wrong", false),
            partial("chunk", false),
            partial("sim", true),
            partial("sim", true),
        ];
        assert_eq!(
            fixture_grok_live_result(&before, &[empty_grok_done()])
                .unwrap()
                .text,
            "sim sim"
        );
        let boundaries = [
            partial("first", false),
            partial("", true),
            partial("second", false),
            partial("corrected second", true),
        ];
        assert_eq!(
            fixture_grok_live_result(&boundaries, &[empty_grok_done()])
                .unwrap()
                .text,
            "first corrected second"
        );
    }

    #[test]
    fn grok_ambiguous_timing_fails_instead_of_silently_dropping_or_duplicating_text() {
        for before in [
            vec![
                grok_segment(0.0, 3.0, "A", false),
                grok_segment(2.0, 3.0, "B", false),
            ],
            vec![
                grok_segment(0.0, 1.0, "A", true),
                grok_segment(1.5, 1.0, "B", true),
                grok_segment(0.0, 3.0, "colliding revision", true),
            ],
            vec![
                grok_segment(0.0, 3.0, "A", false),
                json!({"type":"transcript.partial","is_final":true,"text":"untimed"}),
            ],
            vec![grok_segment(-1.0, 3.0, "negative", true)],
            vec![grok_segment(0.0, 0.0, "zero duration", true)],
            vec![
                json!({"type":"transcript.partial","is_final":true,"start":0.0,"text":"missing duration"}),
            ],
        ] {
            assert!(fixture_grok_live_result(&before, &[empty_grok_done()]).is_err());
        }
        // An extending stitched final may replace covered chunks safely.
        let valid = [
            grok_segment(0.0, 1.0, "old chunk", false),
            grok_segment(0.0, 2.0, "whole utterance", true),
        ];
        assert_eq!(
            fixture_grok_live_result(&valid, &[empty_grok_done()])
                .unwrap()
                .text,
            "whole utterance"
        );
    }

    #[test]
    fn grok_retained_finals_require_both_finish_and_terminal_confirmation() {
        let final_event = grok_segment(0.0, 1.0, "must wait", true);
        assert!(fixture_grok_live_result(std::slice::from_ref(&final_event), &[]).is_err());
        assert!(fixture_grok_live_result(&[final_event.clone(), empty_grok_done()], &[]).is_err());
        let mut protocol = Protocol::new(Provider::Grok);
        assert!(protocol.event(&final_event).unwrap().is_none());
        protocol.finishing = true;
        assert!(protocol.closed().is_err());
        assert_eq!(
            protocol.event(&empty_grok_done()).unwrap(),
            Some("must wait".into())
        );
    }

    #[test]
    fn grok_final_storage_is_bounded_before_done_without_losing_retained_entries() {
        let mut finals = GrokFinals::default();
        let text = "x".repeat(MAX_TEXT);
        finals.retain(&grok_segment(0.0, 1.0, &text, true)).unwrap();
        assert_eq!(finals.bytes, MAX_TEXT);
        assert!(
            finals
                .retain(&grok_segment(2.0, 1.0, "overflow", true))
                .is_err()
        );
        assert_eq!(finals.spans.len(), 1);
        assert_eq!(finals.bytes, MAX_TEXT);
        let mut finals = GrokFinals::default();
        let event =
            json!({"type":"transcript.partial","is_final":true,"speech_final":true,"text":"x"});
        for _ in 0..MAX_GROK_FINALS {
            finals.retain(&event).unwrap();
        }
        assert!(finals.retain(&event).is_err());
        assert_eq!(finals.closed.len(), MAX_GROK_FINALS);
        assert_eq!(finals.bytes, MAX_GROK_FINALS);
    }

    #[test]
    fn grok_only_audio_done_acknowledgement_can_finish_the_capture() {
        let mut protocol = Protocol::new(Provider::Grok);
        assert!(!protocol.ready);
        protocol
            .event(&json!({"type":"transcript.created"}))
            .unwrap();
        assert!(protocol.ready);
        for finality in [false, true] {
            assert!(protocol.event(&json!({"type":"transcript.partial","is_final":finality,"speech_final":true,"text":"segment"})).unwrap().is_none());
        }
        assert!(
            protocol
                .event(&json!({"type":"transcript.done","text":"early"}))
                .is_err()
        );
        protocol.finishing = true;
        assert!(
            protocol
                .event(
                    &json!({"type":"transcript.partial","is_final":true,"text":"still a segment"})
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(protocol.event(&json!({"type":"transcript.done","text":"Complete final transcript","duration":1.0})).unwrap(), Some("Complete final transcript".into()));
        assert_eq!(
            finish_message(Provider::Grok).unwrap(),
            json!({"type":"audio.done"})
        );
    }

    #[test]
    fn google_native_transcription_setup_has_no_assistant_prompt_or_key_in_url() {
        let model = ModelRef::parse("google::gemini-3.5-transcribe-live");
        let options = ModelOptions {
            language: "pt".into(),
            smart_format: true,
            prompt: "must not become system instruction".into(),
            ..Default::default()
        };
        let url = endpoint(model, &options, &[]).unwrap();
        assert_eq!(url.host_str(), Some("generativelanguage.googleapis.com"));
        assert!(url.query().is_none());
        let setup = google_setup(model.model, &options, &["Nimbus Files".into()]);
        assert_eq!(setup["setup"]["model"], "models/gemini-3.5-transcribe-live");
        assert_eq!(
            setup["setup"]["generationConfig"]["responseModalities"],
            json!(["TEXT"])
        );
        assert_eq!(
            setup["setup"]["inputAudioTranscription"],
            json!({"languageCodes":["pt-BR"],"customVocabulary":["Nimbus Files"],"mode":"SMART"})
        );
        assert_eq!(
            setup["setup"]["realtimeInputConfig"]["automaticActivityDetection"]["disabled"],
            true
        );
        assert!(setup["setup"].get("systemInstruction").is_none());
        assert!(setup["setup"].get("outputAudioTranscription").is_none());
        let auto = google_setup(
            model.model,
            &ModelOptions {
                smart_format: false,
                ..ModelOptions::default()
            },
            &[],
        );
        assert_eq!(
            auto["setup"]["inputAudioTranscription"],
            json!({"languageCodes":[],"mode":"VERBATIM"})
        );
        let zh = google_setup(
            model.model,
            &ModelOptions {
                language: "zh".into(),
                ..Default::default()
            },
            &[],
        );
        assert_eq!(
            zh["setup"]["inputAudioTranscription"]["languageCodes"],
            json!(["cmn-Hans-CN"])
        );
        let (header, value) = authentication(Provider::Google, "fixture-google-key").unwrap();
        assert_eq!(header, "x-goog-api-key");
        assert!(value.is_sensitive());
        assert!(!format!("{value:?}").contains("fixture-google-key"));
        assert_eq!(
            finish_message(Provider::Google).unwrap(),
            json!({"realtimeInput":{"activityEnd":{}}})
        );
    }

    #[test]
    fn google_waits_for_last_transcription_fragment_in_either_terminal_event_order() {
        for turn_first in [false, true] {
            let mut protocol = Protocol::new(Provider::Google);
            protocol.event(&json!({"setupComplete":{}})).unwrap();
            assert!(protocol.ready);
            assert!(protocol.event(&json!({"serverContent":{"inputTranscription":{"text":"Hello", "finished":false},"interimInputTranscription":{"text":"preview"},"modelTurn":{"parts":[{"text":"assistant must not paste"}]}}})).unwrap().is_none());
            protocol.finishing = true;
            if turn_first {
                assert!(
                    protocol
                        .event(&json!({"serverContent":{"turnComplete":true}}))
                        .unwrap()
                        .is_none()
                );
            }
            assert!(protocol.event(&json!({"serverContent":{"inputTranscription":{"text":" world", "finished":false}}})).unwrap().is_none());
            let result = protocol
                .event(
                    &json!({"serverContent":{"inputTranscription":{"text":"!", "finished":true}}}),
                )
                .unwrap();
            if turn_first {
                assert_eq!(result, Some("Hello world!".into()));
            } else {
                assert!(result.is_none());
                assert_eq!(
                    protocol
                        .event(&json!({"serverContent":{"turnComplete":true}}))
                        .unwrap(),
                    Some("Hello world!".into())
                );
            }
        }
    }

    #[test]
    fn google_rejects_early_finish_and_missing_terminal_marker_never_returns_partial_text() {
        let mut protocol = Protocol::new(Provider::Google);
        assert!(protocol.event(&json!({"serverContent":{"inputTranscription":{"text":"early","finished":true}}})).is_err());
        let mut protocol = Protocol::new(Provider::Google);
        assert!(
            protocol
                .event(&json!({"serverContent":{"turnComplete":true}}))
                .is_err()
        );
        for marker in ["finished", "turnComplete"] {
            let mut protocol = Protocol::new(Provider::Google);
            protocol.finishing = true;
            let text = json!({"serverContent":{"inputTranscription":{"text":"must not paste","finished": marker == "finished"},"turnComplete":marker == "turnComplete"}});
            assert!(protocol.event(&text).unwrap().is_none());
            assert!(
                protocol.closed().is_err(),
                "a missing marker must fail into whole-clip fallback"
            );
        }
        let mut protocol = Protocol::new(Provider::Google);
        assert!(
            protocol
                .event(&json!({"goAway":{"timeLeft":"2s"}}))
                .is_err()
        );
        assert!(
            protocol
                .event(&json!({"serverContent":{"interrupted":true}}))
                .is_err()
        );
    }

    #[test]
    fn google_audio_and_wall_time_are_bounded_and_errors_stay_sanitized() {
        assert_eq!(session_lifetime(Provider::Google), Duration::from_secs(600));
        assert!(validate_audio_length(Provider::Google, 600 * 16_000).is_ok());
        assert!(validate_audio_length(Provider::Google, 600 * 16_000 + 1).is_err());
        let mut protocol = Protocol::new(Provider::Google);
        let error = protocol.event(&json!({"error":{"code":400,"message":"invalid customVocabulary: PRIVATE_PAYLOAD"}})).unwrap_err();
        let typed = error.downcast_ref::<LiveError>().unwrap();
        assert_eq!(typed.status, Some(400));
        assert!(typed.keywords_rejected);
        assert!(!format!("{error:#}").contains("PRIVATE_PAYLOAD"));
        protocol.finishing = true;
        assert!(
            protocol
                .event(
                    &json!({"serverContent":{"inputTranscription":{"text":"x".repeat(MAX_TEXT)}}})
                )
                .unwrap()
                .is_none()
        );
        assert!(
            protocol
                .event(&json!({"serverContent":{"inputTranscription":{"text":"overflow"}}}))
                .is_err()
        );
    }

    #[test]
    fn tcp_connection_timeout_is_preserved_without_assuming_query_delivery() {
        for (kind, expected) in [
            (io::ErrorKind::TimedOut, ErrorKind::Timeout),
            (io::ErrorKind::WouldBlock, ErrorKind::Timeout),
            (io::ErrorKind::ConnectionRefused, ErrorKind::Network),
            (io::ErrorKind::NetworkUnreachable, ErrorKind::Network),
        ] {
            let classified = connect_error_kind(&io::Error::new(kind, "PRIVATE_CONNECT"));
            assert_eq!(classified, expected);
            let control = SessionControl::new(None, None);
            control.attempt_started.store(true, Ordering::Release);
            let report = mark_attempt(
                failure_kind("Could not connect to streaming provider.", classified),
                &control,
            );
            let typed = report.downcast_ref::<LiveError>().unwrap();
            assert_eq!(typed.error_kind, expected);
            assert_eq!(typed.keyword_count, 0);
            assert!(!format!("{report:#}").contains("PRIVATE"));
        }
    }

    #[test]
    fn socket_send_read_and_handshake_timeouts_keep_their_category() {
        for kind in [io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock] {
            let error = tungstenite::Error::Io(io::Error::new(kind, "PRIVATE_TRANSPORT"));
            assert_eq!(websocket_error_kind(&error), ErrorKind::Timeout);
            for operation in ["Streaming write failed.", "Streaming read failed."] {
                let safe = websocket_failure(operation, &error);
                assert_eq!(
                    safe.downcast_ref::<LiveError>().unwrap().error_kind,
                    ErrorKind::Timeout
                );
                assert!(!format!("{safe:#}").contains("PRIVATE"));
            }
            let safe = handshake_failure::<std::io::Cursor<Vec<u8>>>(
                tungstenite::HandshakeError::Failure(error),
                7,
            );
            assert_eq!(
                safe.downcast_ref::<LiveError>().unwrap().error_kind,
                ErrorKind::Timeout
            );
            assert_eq!(safe.downcast_ref::<LiveError>().unwrap().keyword_count, 0);
        }
        #[derive(Debug)]
        struct WaitingSocket;
        impl Read for WaitingSocket {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
        impl Write for WaitingSocket {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        // A pure in-memory I/O fixture reaches tungstenite's Interrupted path;
        // no connection, DNS, credentials, or server is involved.
        let error = tungstenite::client("ws://fixture.invalid/", WaitingSocket).unwrap_err();
        assert!(matches!(
            &error,
            tungstenite::HandshakeError::Interrupted(_)
        ));
        let safe = handshake_failure(error, 0);
        assert_eq!(
            safe.downcast_ref::<LiveError>().unwrap().error_kind,
            ErrorKind::Timeout
        );
    }

    #[test]
    fn failed_attempt_reports_only_keywords_confirmed_sent_in_query_or_setup() {
        let control = SessionControl::new(None, None);
        control.attempt_started.store(true, Ordering::Release);
        let before = mark_attempt(
            failure_kind("credential missing", ErrorKind::Auth),
            &control,
        );
        assert_eq!(before.downcast_ref::<LiveError>().unwrap().keyword_count, 0);
        control.sent_keyword_count.store(7, Ordering::Release);
        let mut protocol = Protocol::new(Provider::Google);
        let after = mark_attempt(protocol.event(&json!({"error":{"code":400,"message":"customVocabulary rejected PRIVATE_TERMS"}})).unwrap_err(),&control);
        let typed = after.downcast_ref::<LiveError>().unwrap();
        assert_eq!(typed.keyword_count, 7);
        assert!(typed.keywords_rejected);
        assert!(!format!("{after:#}").contains("PRIVATE"));
        for status in [400, 422, 401, 429, 503, 302] {
            for (query_count, expected) in [(3, 3), (0, 0)] {
                let response = tungstenite::http::Response::builder()
                    .status(status)
                    .body(Some(b"keyterms rejected PRIVATE_QUERY".to_vec()))
                    .unwrap();
                let safe = handshake_failure::<std::io::Cursor<Vec<u8>>>(
                    tungstenite::HandshakeError::Failure(tungstenite::Error::Http(Box::new(
                        response,
                    ))),
                    query_count,
                );
                let typed = safe.downcast_ref::<LiveError>().unwrap();
                assert_eq!(typed.keyword_count, expected);
                assert_eq!(typed.error_kind, ErrorKind::from_status(status));
                assert_eq!(typed.keywords_rejected, matches!(status, 400 | 422));
                assert!(!format!("{safe:#}").contains("PRIVATE"));
            }
        }
        assert!(query_keywords(Provider::Deepgram));
        assert!(query_keywords(Provider::ElevenLabs));
        assert!(query_keywords(Provider::Grok));
        assert!(!query_keywords(Provider::Google));
        assert!(!query_keywords(Provider::OpenAi));
    }

    #[test]
    fn telemetry_marks_credentials_as_attempts_but_not_local_preflight() {
        let control = SessionControl::new(None, None);
        let error = mark_attempt(failure("preflight rejected"), &control);
        assert!(!error.downcast_ref::<LiveError>().unwrap().attempted);
        control.attempt_started.store(true, Ordering::Release);
        let error = mark_attempt(
            failure_kind("credential unavailable", ErrorKind::Auth),
            &control,
        );
        let typed = error.downcast_ref::<LiveError>().unwrap();
        assert!(typed.attempted);
        assert_eq!(typed.error_kind, ErrorKind::Auth);
        let error = mark_attempt(control.check(Instant::now()).unwrap_err(), &control);
        assert_eq!(
            error.downcast_ref::<LiveError>().unwrap().error_kind,
            ErrorKind::Timeout
        );
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
        // An unrequested commit discards the live session; a new one finishes.
        let mut protocol = Protocol::new(Provider::ElevenLabs);
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
    fn scribe_joins_every_requested_segment_in_order() {
        let mut protocol = Protocol::new(Provider::ElevenLabs);
        protocol.elevenlabs_commits = 2;
        for text in ["first part.", "  second part. "] {
            assert!(
                protocol
                    .event(&json!({"message_type":"committed_transcript","text":text}))
                    .unwrap()
                    .is_none()
            );
        }
        protocol.finishing = true;
        assert_eq!(
            protocol
                .event(&json!({"message_type":"committed_transcript","text":"end"}))
                .unwrap(),
            Some("first part. second part. end".into())
        );
    }

    #[test]
    fn scribe_late_segments_wait_for_the_final_commit() {
        let mut protocol = Protocol::new(Provider::ElevenLabs);
        protocol.elevenlabs_commits = 1;
        protocol.finishing = true;
        assert!(
            protocol
                .event(&json!({"message_type":"committed_transcript","text":"one"}))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            protocol
                .event(&json!({"message_type":"committed_transcript","text":"two"}))
                .unwrap(),
            Some("one two".into())
        );
    }

    #[test]
    fn scribe_unrequested_commits_and_skipped_finals_are_exact() {
        let mut protocol = Protocol::new(Provider::ElevenLabs);
        protocol.elevenlabs_commits = 1;
        protocol
            .event(&json!({"message_type":"committed_transcript","text":"asked"}))
            .unwrap();
        assert!(
            protocol
                .event(&json!({"message_type":"committed_transcript","text":"surprise"}))
                .is_err()
        );

        let mut skipped = Protocol::new(Provider::ElevenLabs);
        skipped.elevenlabs_commits = 1;
        skipped.elevenlabs_skip_final = true;
        skipped.finishing = true;
        assert_eq!(
            skipped
                .event(&json!({"message_type":"committed_transcript","text":"only"}))
                .unwrap(),
            Some("only".into())
        );
    }

    #[test]
    fn scribe_commits_in_a_real_pause_after_fifteen_seconds_and_always_by_thirty() {
        assert!(!elevenlabs_commit_due(14 * 16_000, ELEVENLABS_PAUSE));
        assert!(elevenlabs_commit_due(15 * 16_000, ELEVENLABS_PAUSE));
        // A short gap inside a phrase is not a pause.
        assert!(!elevenlabs_commit_due(20 * 16_000, ELEVENLABS_PAUSE - 1));
        assert!(!elevenlabs_commit_due(29 * 16_000, 0));
        assert!(elevenlabs_commit_due(30 * 16_000, 0));
        let silence = vec![0_u8; 3_200];
        assert!(pcm16_rms(&silence) < ELEVENLABS_QUIET_RMS);
        let speech: Vec<u8> = (0..1_600_i32)
            .flat_map(|n| (((n as f64 * 0.2).sin() * 8_000.0) as i16).to_le_bytes())
            .collect();
        assert!(pcm16_rms(&speech) > ELEVENLABS_QUIET_RMS);
        assert_eq!(pcm16_rms(&[]), 0.0);
    }

    #[test]
    fn completed_scribe_clips_commit_in_their_pauses() {
        let speech = |seconds: usize| -> Vec<f32> {
            (0..seconds * 16_000)
                .map(|n| (n as f32 * 0.05).sin() * 0.3)
                .collect()
        };
        assert!(elevenlabs_commit_points(&speech(30)).is_empty());

        let mut clip = speech(24);
        clip.extend(vec![0.0; 16_000]);
        clip.extend(speech(22));
        clip.extend(vec![0.0; 16_000]);
        clip.extend(speech(20));
        let points = elevenlabs_commit_points(&clip);
        assert_eq!(points.len(), 2);
        assert!((24 * 16_000..25 * 16_000).contains(&points[0]));
        assert!((47 * 16_000..48 * 16_000).contains(&points[1]));

        // Without a pause, each segment still ends within the 30 s window.
        let points = elevenlabs_commit_points(&speech(95));
        assert!(points.len() >= 3);
        let mut start = 0;
        for point in points {
            assert!(
                (start + ELEVENLABS_COMMIT_AFTER..start + ELEVENLABS_FORCE_COMMIT).contains(&point)
            );
            start = point;
        }
        assert!(95 * 16_000 - start <= ELEVENLABS_FORCE_COMMIT);
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
