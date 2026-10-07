//! Local recovery for completed recordings whose transcription did not finish.
//! Audio is written before a remote attempt. Recovery is independent of normal
//! History retention; only an explicit delete removes an unrecovered recording.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::openrouter::transcribe::Transcription;

const SAMPLE_RATE: u32 = 16_000;
const VERSION: u32 = 1;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
const REQUEST_FAILED: &str = "Transcription could not finish; no detailed cause is available.";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStatus {
    Pending,
    Failed,
    Recovered,
    Resolved,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryEntry {
    version: u32,
    pub id: String,
    pub timestamp_ms: u64,
    pub audio_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    pub status: RecoveryStatus,
    pub message: Option<String>,
    pub text: Option<String>,
    #[serde(skip)]
    pub busy: bool,
    #[serde(skip)]
    pub volatile: bool,
}

impl RecoveryEntry {
    pub fn application_label(&self) -> &str {
        self.application
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("Unknown app")
    }

    pub fn title(&self) -> &'static str {
        if self.busy {
            "Transcribing saved audio"
        } else {
            match self.status {
                RecoveryStatus::Pending => "Interrupted transcription",
                RecoveryStatus::Failed => "Transcription failed",
                RecoveryStatus::Recovered => "Recovered dictation",
                RecoveryStatus::Resolved => "Dictation completed",
            }
        }
    }
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, RecoveryEntry>,
    active: HashSet<String>,
    retrying: bool,
    // If the disk cannot accept a recording, keep its samples for this session
    // and explicitly warn the user. No implementation can survive a crash
    // without writable storage, so never label this fallback as durable.
    volatile: BTreeMap<String, Arc<Vec<f32>>>,
}

#[derive(Clone)]
pub struct RecordingRecovery {
    directory: Arc<PathBuf>,
    state: Arc<Mutex<State>>,
    load_warning: Option<Arc<str>>,
}

impl gpui::Global for RecordingRecovery {}

impl RecordingRecovery {
    pub fn open_default() -> color_eyre::Result<Self> {
        let directory = crate::app_paths::support_dir()?.join("recording-recovery");
        Ok(Self::open(directory.clone()).unwrap_or_else(|error| {
            tracing::warn!(%error, "recording recovery storage could not be opened");
            Self { directory: Arc::new(directory), state: Arc::new(Mutex::new(State::default())),
                load_warning: Some("Saved audio could not be loaded. New recordings may remain only in memory; keep Hex open and check disk space and permissions.".into()) }
        }))
    }

    pub fn open(directory: PathBuf) -> io::Result<Self> {
        private_directory(&directory)?;
        let mut entries = BTreeMap::new();
        let mut audio = Vec::new();
        let mut resolved = Vec::new();
        for item in fs::read_dir(&directory)? {
            let item = item?;
            if !item.file_type()?.is_file() {
                continue;
            }
            let path = item.path();
            let Some(id) = path
                .file_stem()
                .and_then(|v| v.to_str())
                .filter(|id| valid_id(id))
            else {
                continue;
            };
            if path.extension().is_some_and(|e| e == "wav") {
                audio.push((id.to_owned(), path));
            } else if path.extension().is_some_and(|e| e == "json") {
                let Ok(entry) = fs::read(&path).and_then(|data| {
                    serde_json::from_slice::<RecoveryEntry>(&data).map_err(io::Error::other)
                }) else {
                    continue;
                };
                if entry.version == VERSION && entry.id == id {
                    if entry.status == RecoveryStatus::Resolved {
                        resolved.push(id.to_owned());
                    } else {
                        entries.insert(id.to_owned(), entry);
                    }
                }
            }
        }
        for id in resolved {
            let _ = remove_recording_files(&directory, &id);
        }
        // An audio rename may have committed just before metadata failed or the
        // process stopped. Discover the WAV independently; never discard it.
        for (id, path) in audio {
            if entries.contains_key(&id) {
                continue;
            }
            let metadata = directory.join(format!("{id}.json"));
            let resolved = fs::read(&metadata)
                .ok()
                .and_then(|b| serde_json::from_slice::<RecoveryEntry>(&b).ok())
                .is_some_and(|entry| {
                    entry.id == id
                        && entry.version == VERSION
                        && entry.status == RecoveryStatus::Resolved
                });
            if resolved {
                continue;
            }
            if let Ok(reader) = hound::WavReader::open(&path) {
                entries.insert(
                    id.clone(),
                    RecoveryEntry {
                        version: VERSION,
                        id,
                        timestamp_ms: crate::history::now_ms(),
                        audio_ms: u64::from(reader.duration()) * 1000
                            / u64::from(reader.spec().sample_rate.max(1)),
                        application: None,
                        status: RecoveryStatus::Pending,
                        message: Some(
                            "Transcription was interrupted. The saved audio is ready to retry."
                                .into(),
                        ),
                        text: None,
                        busy: false,
                        volatile: false,
                    },
                );
            }
        }
        Ok(Self {
            directory: Arc::new(directory),
            state: Arc::new(Mutex::new(State {
                entries,
                ..State::default()
            })),
            load_warning: None,
        })
    }

    pub fn load_warning(&self) -> Option<&str> {
        self.load_warning.as_deref()
    }

    pub fn entries(&self, query: &str) -> Vec<RecoveryEntry> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let needle = query.trim().to_lowercase();
        let mut entries: Vec<_> = state
            .entries
            .values()
            .filter(|entry| {
                entry.title().to_lowercase().contains(&needle)
                    || entry.application_label().to_lowercase().contains(&needle)
                    || entry
                        .text
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&needle)
                    || entry
                        .message
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&needle)
            })
            .cloned()
            .map(|mut entry| {
                entry.busy = state.active.contains(&entry.id);
                entry
            })
            .collect();
        entries.sort_by(|a, b| {
            b.timestamp_ms
                .cmp(&a.timestamp_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        entries
    }

    pub fn retry_in_progress(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retrying
    }

    fn audio_path(&self, id: &str) -> io::Result<PathBuf> {
        if !valid_id(id) {
            return Err(io::Error::other("Invalid recording identity"));
        }
        Ok(self.directory.join(format!("{id}.wav")))
    }

    fn save_metadata(&self, entry: &RecoveryEntry) -> io::Result<()> {
        if !valid_id(&entry.id) {
            return Err(io::Error::other("Invalid recording identity"));
        }
        let path = self.directory.join(format!("{}.json", entry.id));
        let temporary = path.with_extension("json.tmp");
        let mut file = private_file(&temporary)?;
        file.write_all(&serde_json::to_vec(entry).map_err(io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(temporary, path)?;
        File::open(self.directory.as_ref())?.sync_all()
    }

    fn save_audio(&self, id: &str, samples: &[f32]) -> io::Result<()> {
        private_directory(&self.directory)?;
        let path = self.audio_path(id)?;
        let temporary = path.with_extension("wav.tmp");
        let file = private_file(&temporary)?;
        let mut writer = hound::WavWriter::new(
            file,
            hound::WavSpec {
                channels: 1,
                sample_rate: SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .map_err(io::Error::other)?;
        for sample in samples {
            writer.write_sample(*sample).map_err(io::Error::other)?;
        }
        writer.finalize().map_err(io::Error::other)?;
        File::open(&temporary)?.sync_all()?;
        fs::rename(temporary, path)?;
        File::open(self.directory.as_ref())?.sync_all()
    }

    fn begin(&self, samples: &[f32], application: Option<&str>) -> RecoveryEntry {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!(
            "{:032x}",
            (nanos << 16) | u128::from(SEQUENCE.fetch_add(1, Ordering::Relaxed) & 0xffff)
        );
        let mut entry = RecoveryEntry {
            version: VERSION,
            id,
            timestamp_ms: crate::history::now_ms(),
            audio_ms: samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE),
            application: application
                .map(|name| {
                    name.chars()
                        .filter(|ch| !ch.is_control())
                        .take(128)
                        .collect::<String>()
                })
                .filter(|name| !name.trim().is_empty()),
            status: RecoveryStatus::Pending,
            message: None,
            text: None,
            busy: false,
            volatile: false,
        };
        if let Err(error) = self.save_audio(&entry.id, samples) {
            tracing::warn!(%error,"could not persist recording recovery audio");
            entry.volatile = true;
            entry.message = Some("Audio is only in memory because it could not be saved. Keep Hex open and free disk space before retrying.".into());
        } else if let Err(error) = self.save_metadata(&entry) {
            tracing::warn!(%error,"could not persist recording recovery metadata; WAV remains recoverable");
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if entry.volatile {
            state
                .volatile
                .insert(entry.id.clone(), Arc::new(samples.to_vec()));
        }
        state.active.insert(entry.id.clone());
        state.entries.insert(entry.id.clone(), entry.clone());
        entry
    }

    fn finish_failure(&self, mut entry: RecoveryEntry, message: &str) {
        entry.status = RecoveryStatus::Failed;
        entry.message = Some(if entry.volatile {
            format!(
                "{message} Audio could not be saved to disk and is only in memory. Keep Hex open until recovery."
            )
        } else {
            message.to_owned()
        });
        if !entry.volatile
            && let Err(error) = self.save_metadata(&entry)
        {
            tracing::warn!(%error,"could not update saved recording status");
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // ActiveAttempt alone releases ownership. Releasing here would allow
        // Retry to start before the original guard drops and clears the new owner.
        state.entries.insert(entry.id.clone(), entry);
    }

    /// Run on the transcription worker, never on the capture callback. A failed
    /// remote attempt leaves both the original samples and its recovery entry.
    pub fn transcribe_original(
        &self,
        samples: &[f32],
        application: Option<&str>,
        transcribe: impl FnOnce(&[f32]) -> color_eyre::Result<Transcription>,
    ) -> color_eyre::Result<Transcription> {
        let mut entry = self.begin(samples, application);
        let _active = ActiveAttempt {
            store: self.clone(),
            id: entry.id.clone(),
            retry: false,
        };
        let result = transcribe(samples);
        match &result {
            Ok(_) => {
                // A terminal marker prevents an orphan WAV from being presented
                // again if cleanup stops between its two removals.
                entry.status = RecoveryStatus::Resolved;
                if self.save_metadata(&entry).is_ok() {
                    let _ = remove_recording_files(&self.directory, &entry.id);
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.entries.remove(&entry.id);
                    state.volatile.remove(&entry.id);
                } else {
                    self.finish_failure(
                        entry,
                        "Transcription completed, but cleanup failed. Audio retained.",
                    );
                }
            }
            Err(error) => self.finish_failure(entry, &transcription_failure_reason(error)),
        }
        result
    }

    pub fn retry(&self, id: &str) -> io::Result<()> {
        let vocabulary = crate::vocabulary::Snapshot::current();
        let remote = vocabulary.clone();
        self.retry_with_processing(
            id,
            crate::post_processing::Preferences::current(),
            vocabulary,
            move |samples| {
                crate::openrouter::transcribe::transcribe_with_vocabulary(samples, &remote)
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn retry_with(
        &self,
        id: &str,
        transcribe: impl FnOnce(&[f32]) -> color_eyre::Result<Transcription> + Send + 'static,
    ) -> io::Result<()> {
        self.retry_with_preferences(
            id,
            crate::post_processing::Preferences::default(),
            transcribe,
        )
    }

    #[cfg(test)]
    pub(crate) fn retry_with_preferences(
        &self,
        id: &str,
        preferences: crate::post_processing::Preferences,
        transcribe: impl FnOnce(&[f32]) -> color_eyre::Result<Transcription> + Send + 'static,
    ) -> io::Result<()> {
        self.retry_with_processing(
            id,
            preferences,
            crate::vocabulary::Snapshot::default(),
            transcribe,
        )
    }

    pub(crate) fn retry_with_processing(
        &self,
        id: &str,
        preferences: crate::post_processing::Preferences,
        vocabulary: crate::vocabulary::Snapshot,
        transcribe: impl FnOnce(&[f32]) -> color_eyre::Result<Transcription> + Send + 'static,
    ) -> io::Result<()> {
        self.audio_path(id)?;
        let entry = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.retrying || state.active.contains(id) {
                return Err(io::Error::other(
                    "A transcription is already running. Wait for it to finish.",
                ));
            }
            let entry = state
                .entries
                .get(id)
                .filter(|e| e.status != RecoveryStatus::Recovered)
                .cloned()
                .ok_or_else(|| io::Error::other("This recording is not available to retry"))?;
            state.active.insert(id.to_owned());
            state.retrying = true;
            entry
        };
        let store = self.clone();
        let id = entry.id.clone();
        let spawned = thread::Builder::new()
            .name("recording-retry".into())
            .spawn(move || {
                let _active = ActiveAttempt {
                    store: store.clone(),
                    id: entry.id.clone(),
                    retry: true,
                };
                store.run_retry(entry, preferences, vocabulary, transcribe);
            });
        if let Err(error) = spawned {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.active.remove(&id);
            state.retrying = false;
            return Err(error);
        }
        Ok(())
    }

    fn read_audio(&self, id: &str) -> io::Result<Arc<Vec<f32>>> {
        if let Some(samples) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .volatile
            .get(id)
            .cloned()
        {
            return Ok(samples);
        }
        let mut reader = hound::WavReader::open(self.audio_path(id)?).map_err(io::Error::other)?;
        let spec = reader.spec();
        if spec.channels != 1
            || spec.sample_rate != SAMPLE_RATE
            || spec.bits_per_sample != 32
            || spec.sample_format != hound::SampleFormat::Float
        {
            return Err(io::Error::other("Saved audio has an unsupported format"));
        }
        Ok(Arc::new(
            reader
                .samples::<f32>()
                .collect::<Result<Vec<_>, _>>()
                .map_err(io::Error::other)?,
        ))
    }

    fn run_retry(
        &self,
        mut entry: RecoveryEntry,
        preferences: crate::post_processing::Preferences,
        vocabulary: crate::vocabulary::Snapshot,
        transcribe: impl FnOnce(&[f32]) -> color_eyre::Result<Transcription>,
    ) {
        let samples = match self.read_audio(&entry.id) {
            Ok(samples) => samples,
            Err(_) => {
                self.finish_failure(
                    entry,
                    "Could not read the saved audio. The recovery files were preserved.",
                );
                return;
            }
        };
        if entry.volatile && self.save_audio(&entry.id, &samples).is_ok() {
            entry.volatile = false;
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .volatile
                .remove(&entry.id);
        }
        let result = transcribe(&samples);
        match result {
            Ok(transcription) if !transcription.text.trim().is_empty() => {
                let formatted = preferences.process(transcription.text.trim());
                let text = vocabulary.restore(formatted.trim()).text;
                if text.is_empty() {
                    self.finish_failure(entry, "Post-processing removed all text. Change its settings before Retry; the saved audio was kept.");
                    return;
                }
                entry.status = RecoveryStatus::Recovered;
                entry.text = Some(text);
                entry.message = None;
                // Commit recovered text before deleting audio, even with History
                // disabled. It remains available here until explicitly deleted.
                if self.save_metadata(&entry).is_err() {
                    self.finish_failure(
                        entry,
                        "Could not save the recovered text. Audio retained; keep Hex open.",
                    );
                    return;
                }
                entry.volatile = false;
                let _ =
                    fs::remove_file(self.audio_path(&entry.id).expect("validated recording id"));
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.volatile.remove(&entry.id);
                state.entries.insert(entry.id.clone(), entry);
            }
            Ok(_) => self.finish_failure(
                entry,
                "The transcription returned no text. The saved audio has been kept.",
            ),
            Err(error) => self.finish_failure(entry, &transcription_failure_reason(&error)),
        }
    }

    pub fn delete(&self, id: &str) -> io::Result<()> {
        self.audio_path(id)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.contains(id) {
            return Err(io::Error::other(
                "Wait for the transcription to finish before deleting it",
            ));
        }
        if !state.entries.contains_key(id) {
            return Err(io::Error::other("Recording no longer exists"));
        }
        remove_recording_files(&self.directory, id)?;
        state.entries.remove(id);
        state.volatile.remove(id);
        Ok(())
    }
}

/// Describe typed failures without persisting provider bodies, URLs, headers,
/// credentials, or arbitrary strings from the transport. Only known categories
/// and validated numeric status/timeout values reach the recovery metadata.
fn transcription_failure_reason(error: &color_eyre::Report) -> String {
    use crate::openrouter::stats::ErrorKind;
    use crate::openrouter::transcribe::ChainFailure;
    if let Some(chain) = error.downcast_ref::<ChainFailure>() {
        if chain.failures.is_empty() {
            return "No transcription models are configured. Choose a model in Models.".into();
        }
        let mut reasons = Vec::new();
        for failure in &chain.failures {
            let status = failure
                .detail
                .strip_prefix(&format!("{}: HTTP ", failure.model))
                .and_then(|rest| rest.split(':').next())
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|value| (100..=599).contains(value));
            let reason = attempt_reason(failure.kind, status, &failure.detail);
            if !reasons.contains(&reason) && reasons.len() < 8 {
                reasons.push(reason);
            }
        }
        let details = reasons.join("\n");
        return if chain.failures.len() > 1 {
            format!("All configured models failed:\n{details}")
        } else {
            details
        };
    }
    let detail = format!("{error:#}").to_lowercase();
    if detail.contains("api key not found") {
        return "The OpenRouter API key is missing. Add it in Models before retrying.".into();
    }
    if detail.contains("no openrouter transcription models") {
        return "No transcription models are configured. Choose a model in Models.".into();
    }
    if error.downcast_ref::<serde_json::Error>().is_some() {
        return "The local transcription configuration is invalid. Check Models settings.".into();
    }
    if detail.contains("could not start curl") || detail.contains("curl stdin unavailable") {
        return "The network client could not be started.".into();
    }
    if let Some(reason) = network_reason(&detail) {
        return reason;
    }
    if let Some(error) = error.downcast_ref::<io::Error>() {
        return match error.kind() {
            io::ErrorKind::PermissionDenied => {
                "Permission denied while reading local transcription settings."
            }
            io::ErrorKind::NotFound => {
                "A local file required for transcription could not be found."
            }
            io::ErrorKind::TimedOut => return attempt_reason(ErrorKind::Timeout, None, ""),
            _ => "A local I/O error prevented transcription.",
        }
        .into();
    }
    REQUEST_FAILED.into()
}

fn attempt_reason(
    kind: crate::openrouter::stats::ErrorKind,
    status: Option<u16>,
    detail: &str,
) -> String {
    use crate::openrouter::stats::ErrorKind;
    let http = status
        .map(|status| format!(" (HTTP {status})"))
        .unwrap_or_default();
    match kind {
        ErrorKind::RateLimited => {
            "The API rate limit was reached (HTTP 429). Wait before retrying.".into()
        }
        ErrorKind::Auth => format!(
            "The API rejected the key or its access permissions{http}. Check the key in Models."
        ),
        ErrorKind::Server => {
            format!("The API returned a server error{http}. Retry when the service is available.")
        }
        ErrorKind::Rejected => match status {
            Some(402) => "The API requires payment or additional credits (HTTP 402).".into(),
            Some(404) => "The requested model or API endpoint was not found (HTTP 404).".into(),
            Some(413) => {
                "The API rejected the audio because the request was too large (HTTP 413).".into()
            }
            _ => {
                format!("The API rejected the transcription request{http}. Check Models settings.")
            }
        },
        ErrorKind::Timeout => {
            let lower = detail.to_lowercase();
            if lower.ends_with(": not tried, chain deadline reached") {
                return "A fallback could not be attempted because the transcription time limit was reached.".into();
            }
            let milliseconds = lower.split("timed out after ").nth(1).and_then(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                rest.get(digits.len()..)
                    .filter(|tail| tail.starts_with(" milliseconds"))?;
                digits
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0 && *value <= 604_800_000)
            });
            if let Some(ms) = milliseconds {
                let seconds = if ms % 1000 == 0 {
                    (ms / 1000).to_string()
                } else {
                    format!("{:.1}", ms as f64 / 1000.0)
                };
                format!(
                    "The API request timed out after {seconds} seconds{http}. No transcription was returned."
                )
            } else {
                format!("The API request timed out{http}. No transcription was returned.")
            }
        }
        ErrorKind::Network => network_reason(&detail.to_lowercase()).unwrap_or_else(|| {
            "The network connection to the API failed. Check your internet connection.".into()
        }),
        ErrorKind::InvalidResponse => {
            "The API returned an invalid or empty transcription response.".into()
        }
    }
}

fn network_reason(detail: &str) -> Option<String> {
    if detail.contains("timed out")
        || detail.contains("timeout")
        || detail.contains("exit status: 28")
    {
        Some(attempt_reason(
            crate::openrouter::stats::ErrorKind::Timeout,
            None,
            detail,
        ))
    } else if detail.contains("could not resolve")
        || detail.contains("couldn't resolve")
        || detail.contains("curl: (6)")
    {
        Some(
            "The API server address could not be resolved (DNS error). Check your connection."
                .into(),
        )
    } else if detail.contains("certificate")
        || detail.contains("ssl connect error")
        || detail.contains("tls handshake")
    {
        Some(
            "A secure connection to the API could not be established (TLS/certificate error)."
                .into(),
        )
    } else if detail.contains("connection reset") {
        Some("The connection to the API was reset before transcription completed.".into())
    } else if detail.contains("failed to connect")
        || detail.contains("couldn't connect")
        || detail.contains("connection refused")
    {
        Some(
            "The API server could not be reached. Check your connection or try again later.".into(),
        )
    } else if detail.contains("network") || detail.contains("offline") || detail.contains("curl") {
        Some("The network connection to the API failed. Check your internet connection.".into())
    } else {
        None
    }
}

struct ActiveAttempt {
    store: RecordingRecovery,
    id: String,
    retry: bool,
}
impl Drop for ActiveAttempt {
    fn drop(&mut self) {
        let mut state = self.store.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active.remove(&self.id);
        if self.retry {
            state.retrying = false;
        }
    }
}

fn remove_recording_files(directory: &Path, id: &str) -> io::Result<()> {
    if !valid_id(id) {
        return Err(io::Error::other("Invalid recording identity"));
    }
    // Metadata is last: a resolved marker survives interrupted cleanup.
    for suffix in ["wav", "wav.tmp", "json.tmp", "json"] {
        match fs::remove_file(directory.join(format!("{id}.{suffix}"))) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())
}

fn private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                    "hex-recovery-test-{}-{}-{}",
                    std::process::id(),
                    SEQUENCE.fetch_add(1, Ordering::Relaxed),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )))
        }
        fn store(&self) -> RecordingRecovery {
            RecordingRecovery::open(self.0.clone()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn failed(store: &RecordingRecovery) -> String {
        let samples = vec![0.125, -0.25, 0.5, 0.0];
        assert!(
            store
                .transcribe_original(&samples, Some("Notes"), |_| Err(color_eyre::eyre::eyre!(
                    "simulated network failure with SECRET_DO_NOT_PERSIST"
                )))
                .is_err()
        );
        store.entries("")[0].id.clone()
    }
    fn wait(store: &RecordingRecovery) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while store.retry_in_progress() {
            assert!(Instant::now() < deadline, "retry worker did not finish");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn retry_formats_text_but_keeps_audio_when_formatting_removes_everything() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        let preferences = crate::post_processing::Preferences {
            lowercase: true,
            remove_punctuation: true,
            ..Default::default()
        };
        store
            .retry_with_preferences(&id, preferences, |_| {
                Ok(Transcription {
                    text: "...".into(),
                    report: None,
                })
            })
            .unwrap();
        wait(&store);
        let entry = &store.entries("")[0];
        assert_eq!(entry.status, RecoveryStatus::Failed);
        assert!(
            entry
                .message
                .as_deref()
                .unwrap()
                .contains("Post-processing")
        );
        assert!(store.audio_path(&id).unwrap().exists());
        store
            .retry_with_preferences(&id, preferences, |_| {
                Ok(Transcription {
                    text: "Olá, JOÃO!".into(),
                    report: None,
                })
            })
            .unwrap();
        wait(&store);
        assert_eq!(store.entries("")[0].text.as_deref(), Some("olá joão"));
        assert!(!store.audio_path(&id).unwrap().exists());
    }

    #[test]
    fn retry_restores_the_snapshotted_vocabulary_after_formatting() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        let preferences = crate::post_processing::Preferences {
            lowercase: true,
            remove_punctuation: true,
            ..Default::default()
        };
        let vocabulary = crate::vocabulary::Snapshot::new(crate::vocabulary::Vocabulary {
            terms: vec!["Nimbus-Files".into()],
            approximate: true,
            ..Default::default()
        });
        store
            .retry_with_processing(&id, preferences, vocabulary, |_| {
                Ok(Transcription {
                    text: "MIMBUSFILES!".into(),
                    report: None,
                })
            })
            .unwrap();
        wait(&store);
        assert_eq!(store.entries("")[0].text.as_deref(), Some("Nimbus-Files"));
        assert!(!store.audio_path(&id).unwrap().exists());
        assert_eq!(
            fixture.store().entries("")[0].text.as_deref(),
            Some("Nimbus-Files")
        );
    }

    #[test]
    fn failed_audio_survives_restart_with_exact_samples_and_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        assert_eq!(store.entries("")[0].status, RecoveryStatus::Failed);
        let loaded = fixture.store();
        assert_eq!(
            *loaded.read_audio(&id).unwrap(),
            vec![0.125, -0.25, 0.5, 0.0]
        );
        for path in [
            &fixture.0,
            &loaded.audio_path(&id).unwrap(),
            &fixture.0.join(format!("{id}.json")),
        ] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
        let metadata = fs::read_to_string(fixture.0.join(format!("{id}.json"))).unwrap();
        assert!(!metadata.contains("SECRET_DO_NOT_PERSIST"));
        assert!(!loaded.entries("")[0].busy);
        assert_eq!(loaded.entries("")[0].application_label(), "Notes");
        assert_eq!(loaded.entries("notes").len(), 1);
    }

    #[test]
    fn retry_is_single_flight_and_commits_text_before_discarding_audio() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        let (finish, release) = mpsc::channel();
        store
            .retry_with(&id, move |samples| {
                assert_eq!(samples, [0.125, -0.25, 0.5, 0.0]);
                release.recv().unwrap();
                Ok(Transcription {
                    text: "Recovered words".into(),
                    report: None,
                })
            })
            .unwrap();
        assert!(store.entries("")[0].busy);
        assert!(
            store
                .retry_with(&id, |_| panic!("duplicate retry"))
                .is_err()
        );
        assert!(store.delete(&id).is_err());
        assert!(store.audio_path(&id).unwrap().exists());
        finish.send(()).unwrap();
        wait(&store);
        let loaded = fixture.store();
        let entry = loaded.entries("").remove(0);
        assert_eq!(entry.status, RecoveryStatus::Recovered);
        assert_eq!(entry.text.as_deref(), Some("Recovered words"));
        assert_eq!(entry.application.as_deref(), Some("Notes"));
        assert!(entry.message.is_none());
        assert!(!loaded.audio_path(&id).unwrap().exists());
        assert!(
            store
                .retry_with(&id, |_| panic!("recovered text is not retried"))
                .is_err()
        );
    }

    #[test]
    fn terminal_status_does_not_release_attempt_ownership_before_its_guard() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let entry = store.begin(&[0.25; 160], Some("Fixture app"));
        let id = entry.id.clone();
        let active = ActiveAttempt {
            store: store.clone(),
            id: id.clone(),
            retry: false,
        };
        store.finish_failure(entry, REQUEST_FAILED);
        assert!(store.entries("")[0].busy);
        assert!(store.delete(&id).is_err());
        assert!(
            store
                .retry_with(&id, |_| panic!("the previous attempt still owns the entry"))
                .is_err()
        );
        drop(active);
        assert!(!store.entries("")[0].busy);
        store
            .retry_with(&id, |_| {
                Ok(Transcription {
                    text: "Recovered fixture".into(),
                    report: None,
                })
            })
            .unwrap();
        wait(&store);
        assert_eq!(store.entries("")[0].status, RecoveryStatus::Recovered);
    }

    #[test]
    fn failed_retry_preserves_original_audio() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        let original = fs::read(store.audio_path(&id).unwrap()).unwrap();
        store
            .retry_with(&id, |_| {
                Err(color_eyre::eyre::eyre!("API still unavailable"))
            })
            .unwrap();
        wait(&store);
        assert_eq!(fs::read(store.audio_path(&id).unwrap()).unwrap(), original);
        assert_eq!(
            fixture.store().entries("")[0].status,
            RecoveryStatus::Failed
        );
    }

    #[test]
    fn original_success_does_not_retain_audio() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let result = store
            .transcribe_original(&[0.25; 32], Some("Notes"), |_| {
                Ok(Transcription {
                    text: "done".into(),
                    report: None,
                })
            })
            .unwrap();
        assert_eq!(result.text, "done");
        assert!(store.entries("").is_empty());
        assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
    }

    #[test]
    fn interrupted_attempt_and_orphan_wav_are_recoverable_without_automatic_requests() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let entry = store.begin(&[0.5; 160], None);
        let loaded = fixture.store();
        assert_eq!(loaded.entries("")[0].status, RecoveryStatus::Pending);
        assert!(!loaded.entries("")[0].busy);
        fs::write(
            fixture.0.join(format!("{}.json", entry.id)),
            "broken metadata",
        )
        .unwrap();
        let loaded = fixture.store();
        assert_eq!(loaded.entries("")[0].id, entry.id);
        assert_eq!(loaded.read_audio(&entry.id).unwrap().len(), 160);
    }

    #[test]
    fn storage_failure_keeps_session_audio_and_retry_can_recover_when_disk_returns() {
        let fixture = Fixture::new();
        let store = fixture.store();
        fs::remove_dir(&fixture.0).unwrap();
        fs::write(&fixture.0, "block directory creation").unwrap();
        let id = failed(&store);
        assert!(store.entries("")[0].volatile);
        assert_eq!(store.read_audio(&id).unwrap().len(), 4);
        fs::remove_file(&fixture.0).unwrap();
        store
            .retry_with(&id, |_| {
                Ok(Transcription {
                    text: "recovered after disk failure".into(),
                    report: None,
                })
            })
            .unwrap();
        wait(&store);
        assert_eq!(
            fixture.store().entries("")[0].text.as_deref(),
            Some("recovered after disk failure")
        );
        assert!(!store.entries("")[0].volatile);
    }

    #[test]
    fn failed_metadata_update_never_removes_the_source_audio() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        let metadata = fixture.0.join(format!("{id}.json"));
        fs::remove_file(&metadata).unwrap();
        fs::create_dir(&metadata).unwrap();
        store
            .retry_with(&id, |_| {
                Ok(Transcription {
                    text: "text cannot be saved yet".into(),
                    report: None,
                })
            })
            .unwrap();
        wait(&store);
        assert!(store.audio_path(&id).unwrap().exists());
        assert_eq!(store.entries("")[0].status, RecoveryStatus::Failed);
        assert_eq!(fixture.store().entries("")[0].id, id);
    }

    fn api_error(kind: crate::openrouter::stats::ErrorKind, detail: &str) -> color_eyre::Report {
        crate::openrouter::transcribe::ChainFailure {
            failures: vec![crate::openrouter::stats::Failure {
                model: "test/model".into(),
                kind,
                detail: format!("test/model: {detail}"),
            }],
        }
        .into()
    }

    #[test]
    fn reasons_show_status_timeout_and_connection_cause_without_provider_payloads() {
        use crate::openrouter::stats::ErrorKind;
        for (kind, detail, expected) in [
            (
                ErrorKind::Timeout,
                "network error: curl: (28) Operation timed out after 30000 milliseconds with SECRET_DO_NOT_PERSIST",
                "30 seconds",
            ),
            (
                ErrorKind::Timeout,
                "not tried, chain deadline reached",
                "time limit was reached",
            ),
            (
                ErrorKind::RateLimited,
                "HTTP 429: SECRET_DO_NOT_PERSIST",
                "HTTP 429",
            ),
            (
                ErrorKind::Auth,
                "HTTP 401: SECRET_DO_NOT_PERSIST",
                "HTTP 401",
            ),
            (
                ErrorKind::Server,
                "HTTP 503: SECRET_DO_NOT_PERSIST",
                "HTTP 503",
            ),
            (
                ErrorKind::Rejected,
                "HTTP 402: SECRET_DO_NOT_PERSIST",
                "credits",
            ),
            (
                ErrorKind::Network,
                "Could not resolve host: SECRET_DO_NOT_PERSIST",
                "DNS error",
            ),
            (
                ErrorKind::Network,
                "Connection reset by peer: SECRET_DO_NOT_PERSIST",
                "was reset",
            ),
            (
                ErrorKind::InvalidResponse,
                "SECRET_DO_NOT_PERSIST",
                "invalid or empty",
            ),
        ] {
            let reason = transcription_failure_reason(&api_error(kind, detail));
            assert!(reason.contains(expected), "{reason}");
            assert!(!reason.contains("SECRET_DO_NOT_PERSIST"));
            assert!(!reason.contains("test/model"));
        }
        let missing_key = color_eyre::eyre::eyre!("OpenRouter API key not found. PRIVATE_COMMAND");
        assert!(transcription_failure_reason(&missing_key).contains("API key is missing"));
        assert!(!transcription_failure_reason(&missing_key).contains("PRIVATE_COMMAND"));
        assert_eq!(
            transcription_failure_reason(&color_eyre::eyre::eyre!("SECRET_DO_NOT_PERSIST")),
            REQUEST_FAILED
        );
    }

    #[test]
    fn latest_failure_reason_and_original_application_survive_retry_and_reload() {
        use crate::openrouter::stats::ErrorKind;
        let fixture = Fixture::new();
        let store = fixture.store();
        assert!(
            store
                .transcribe_original(&[0.1; 160], Some("Codex"), |_| {
                    Err(api_error(
                        ErrorKind::Timeout,
                        "Operation timed out after 30000 milliseconds",
                    ))
                })
                .is_err()
        );
        let entry = store.entries("").remove(0);
        assert!(entry.message.unwrap().contains("30 seconds"));
        store
            .retry_with(&entry.id, |_| {
                Err(api_error(ErrorKind::RateLimited, "HTTP 429: PRIVATE_BODY"))
            })
            .unwrap();
        wait(&store);
        let reloaded = fixture.store().entries("").remove(0);
        assert_eq!(reloaded.application.as_deref(), Some("Codex"));
        let reason = reloaded.message.unwrap();
        assert!(reason.contains("HTTP 429"));
        assert!(!reason.contains("30 seconds"));
        assert!(!reason.contains("PRIVATE_BODY"));
    }

    #[test]
    fn old_recovery_entries_load_without_inventing_an_application() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        let path = fixture.0.join(format!("{id}.json"));
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        metadata.as_object_mut().unwrap().remove("application");
        fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let entry = fixture.store().entries("").remove(0);
        assert_eq!(entry.application, None);
        assert_eq!(entry.application_label(), "Unknown app");
    }

    #[test]
    fn delete_requires_an_existing_safe_identity_and_removes_the_audio() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        assert!(store.delete("../outside").is_err());
        fs::write(fixture.0.join(format!("{id}.wav.tmp")), "partial audio").unwrap();
        fs::write(fixture.0.join(format!("{id}.json.tmp")), "partial metadata").unwrap();
        store.delete(&id).unwrap();
        assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
        assert!(store.entries("").is_empty());
        assert!(!store.audio_path(&id).unwrap().exists());
        assert!(fixture.store().entries("").is_empty());
    }
}
