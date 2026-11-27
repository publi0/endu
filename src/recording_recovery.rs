//! Local recovery for completed recordings whose transcription did not finish.
//! Audio is written before a remote attempt. Recovery is independent of normal
//! History retention; only an explicit delete removes an unrecovered recording.

use crate::i18n::t;
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

#[derive(Debug)]
enum UnusableTranscript {
    EmptyResponse,
    EmptyAfterFormatting,
}

impl std::fmt::Display for UnusableTranscript {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EmptyResponse => t("The transcription returned no text. The saved audio has been kept."),
            Self::EmptyAfterFormatting => t("Post-processing removed all text. Change its settings before Retry; the saved audio was kept."),
        })
    }
}

impl std::error::Error for UnusableTranscript {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryCancel {
    None,
    /// Explicit Escape cancellation: discard temporary audio.
    Discard,
    /// User stopped processing from the HUD: keep saved audio for retry.
    KeepForRetry,
}

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
            .unwrap_or(t("Unknown app"))
    }

    pub fn title(&self) -> &'static str {
        if self.busy {
            t("Transcribing saved audio")
        } else {
            match self.status {
                RecoveryStatus::Pending => t("Interrupted transcription"),
                RecoveryStatus::Failed => t("Transcription failed"),
                RecoveryStatus::Recovered => t("Recovered dictation"),
                RecoveryStatus::Resolved => t("Dictation completed"),
            }
        }
    }
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, RecoveryEntry>,
    active: HashSet<String>,
    retrying: bool,
    retention: crate::history::HistoryRetention,
    // If the disk cannot accept a recording, keep its samples for this session
    // and explicitly warn the user. No implementation can survive a crash
    // without writable storage, so never label this fallback as durable.
    volatile: BTreeMap<String, Arc<Vec<f32>>>,
    /// Bumped whenever `entries` or `active` change, so the History poll can skip clones.
    revision: u64,
}

#[derive(Clone)]
pub struct RecordingRecovery {
    directory: Arc<PathBuf>,
    state: Arc<Mutex<State>>,
    load_warning: Option<Arc<str>>,
}

impl gpui::Global for RecordingRecovery {}

/// Completes post-transcription disk cleanup (`Resolved` metadata + WAV/JSON
/// removal) after the caller has already handed a successful transcript to the
/// output worker, keeping recovery `fsync` barriers off the paste critical path.
pub struct PendingResolution {
    store: RecordingRecovery,
    entry: Option<RecoveryEntry>,
    _active: ActiveAttempt,
}

impl PendingResolution {
    pub fn finish(mut self) {
        self.run();
    }

    /// Preserves an accepted recording during shutdown instead of deleting it
    /// when output could not be dispatched. If `text` is already available,
    /// saves it as `Recovered` before removing audio; otherwise keeps the WAV
    /// intact for later retry.
    pub fn preserve_for_shutdown(mut self, text: Option<String>) {
        let Some(entry) = self.entry.take() else {
            return;
        };
        if let Some(text) = text.filter(|value| !value.trim().is_empty())
            && !entry.volatile
            && self.store.keep_recovered(entry.clone(), text)
        {
            return;
        }
        self.store.finish_failure(
            entry,
            t("Endu closed before transcription finished. Audio was kept for retry."),
        );
    }

    /// Preserves an accepted recording whose transcription was stopped by the
    /// user from the HUD, keeping its audio ready for a manual Retry.
    pub fn preserve_stopped(mut self) {
        if let Some(entry) = self.entry.take() {
            self.store.finish_stopped(entry);
        }
    }

    /// Keeps a successful transcript that was not pasted (copied, deferred or
    /// failed to paste) as a `Recovered` entry, so it stays in History until
    /// deleted, expired, or pasted with Paste Last. Empty text resolves.
    pub fn keep_unpasted(mut self, text: &str) -> Option<RecoveredText> {
        let entry = self.entry.take()?;
        let text = text.trim();
        if text.is_empty() {
            self.store.resolve_entry(entry);
            return None;
        }
        let id = entry.id.clone();
        if self.store.keep_recovered(entry.clone(), text.to_owned()) {
            return Some(RecoveredText {
                store: self.store.clone(),
                id,
            });
        }
        self.store.finish_failure(
            entry,
            t("Could not save the recovered text. Audio retained; keep Endu open."),
        );
        None
    }

    fn run(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.store.resolve_entry(entry);
        }
    }
}

/// A transcript kept as `Recovered` because it was not pasted.
pub struct RecoveredText {
    store: RecordingRecovery,
    id: String,
}

impl RecoveredText {
    /// Paste Last delivered the text and History now records it: drop the
    /// recovery copy so it is not listed twice.
    pub fn discard(self) {
        let _ = self.store.delete(&self.id);
    }
}

impl Drop for PendingResolution {
    fn drop(&mut self) {
        self.run();
    }
}

static ACTIVE_PLAYBACK_STOP: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>> = Mutex::new(None);

/// Stops any local Recording Recovery playback currently in progress (for
/// example when a new dictation begins or the History pane closes).
pub fn stop_active_playback() {
    if let Some(flag) = ACTIVE_PLAYBACK_STOP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        flag.store(true, Ordering::Release);
    }
}

/// Handle to one local playback of a failed recording. Stopping or dropping
/// the handle immediately releases the audio output device.
pub struct RecoveryPlayback {
    id: String,
    duration: std::time::Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl RecoveryPlayback {
    fn start(id: String, samples: Arc<Vec<f32>>) -> io::Result<Self> {
        stop_active_playback();
        let duration =
            std::time::Duration::from_secs_f64(samples.len() as f64 / f64::from(SAMPLE_RATE));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *ACTIVE_PLAYBACK_STOP
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(stop.clone());

        if !cfg!(test) {
            let worker_stop = stop.clone();
            let worker_finished = finished.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<io::Result<()>>(1);
            thread::Builder::new()
                .name("recovery-playback".into())
                .spawn(move || {
                    use rodio::{DeviceSinkBuilder, buffer::SamplesBuffer};
                    let sink = match DeviceSinkBuilder::open_default_sink() {
                        Ok(sink) => {
                            let _ = ready_tx.send(Ok(()));
                            sink
                        }
                        Err(error) => {
                            let _ = ready_tx.send(Err(io::Error::other(format!(
                                "{}: {error}",
                                t("Audio output is unavailable.")
                            ))));
                            worker_finished.store(true, Ordering::Release);
                            return;
                        }
                    };
                    let buffer = SamplesBuffer::new(
                        std::num::NonZero::new(1).expect("mono channel count is non-zero"),
                        std::num::NonZero::new(SAMPLE_RATE).expect("sample rate is non-zero"),
                        (*samples).clone(),
                    );
                    sink.mixer().add(buffer);
                    let end =
                        std::time::Instant::now() + duration + std::time::Duration::from_millis(80);
                    while !worker_stop.load(Ordering::Acquire) && std::time::Instant::now() < end {
                        let remaining = end.saturating_duration_since(std::time::Instant::now());
                        thread::sleep(remaining.min(std::time::Duration::from_millis(40)));
                    }
                    drop(sink);
                    worker_finished.store(true, Ordering::Release);
                })?;
            match ready_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    stop.store(true, Ordering::Release);
                    return Err(error);
                }
                Err(_) => {
                    stop.store(true, Ordering::Release);
                    return Err(io::Error::other(t("Audio output is unavailable.")));
                }
            }
        }

        Ok(Self {
            id,
            duration,
            stop,
            finished,
        })
    }

    pub fn is_playing(&self, id: &str) -> bool {
        self.id == id
            && !self.stop.load(Ordering::Acquire)
            && !self.finished.load(Ordering::Acquire)
    }

    pub fn duration(&self) -> std::time::Duration {
        self.duration
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for RecoveryPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

impl RecordingRecovery {
    pub fn open_default(retention: crate::history::HistoryRetention) -> color_eyre::Result<Self> {
        let directory = crate::app_paths::support_dir()?.join("recording-recovery");
        Ok(Self::open_with_retention(directory.clone(), retention).unwrap_or_else(|error| {
            tracing::warn!(%error, "recording recovery storage could not be opened");
            Self {
                directory: Arc::new(directory),
                state: Arc::new(Mutex::new(State {
                    retention,
                    ..State::default()
                })),
                load_warning: Some(t("Saved audio could not be loaded. New recordings may remain only in memory; keep Endu open and check disk space and permissions.").into()),
            }
        }))
    }

    pub fn open(directory: PathBuf) -> io::Result<Self> {
        Self::open_with_retention(directory, crate::history::HistoryRetention::Forever)
    }

    pub fn open_with_retention(
        directory: PathBuf,
        retention: crate::history::HistoryRetention,
    ) -> io::Result<Self> {
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
                let timestamp_ms = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or_else(crate::history::now_ms);
                entries.insert(
                    id.clone(),
                    RecoveryEntry {
                        version: VERSION,
                        id,
                        timestamp_ms,
                        audio_ms: u64::from(reader.duration()) * 1000
                            / u64::from(reader.spec().sample_rate.max(1)),
                        application: None,
                        status: RecoveryStatus::Pending,
                        message: Some(
                            t("Transcription was interrupted. The saved audio is ready to retry.")
                                .into(),
                        ),
                        text: None,
                        busy: false,
                        volatile: false,
                    },
                );
            }
        }
        let store = Self {
            directory: Arc::new(directory),
            state: Arc::new(Mutex::new(State {
                entries,
                retention,
                ..State::default()
            })),
            load_warning: None,
        };
        store.prune_expired_at(crate::history::now_ms());
        Ok(store)
    }

    pub fn load_warning(&self) -> Option<&str> {
        self.load_warning.as_deref()
    }

    pub fn set_retention(&self, retention: crate::history::HistoryRetention) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.retention == retention {
                return;
            }
            state.retention = retention;
        }
        self.prune_expired_at(crate::history::now_ms());
    }

    /// Removes non-active failed and recovered entries older than the timed
    /// retention window (`Day`, `Week`, `Month`). `Off` and `Forever` keep
    /// recovery entries until manually handled.
    pub fn prune_expired_at(&self, now_ms: u64) -> usize {
        let expired: Vec<(String, bool)> = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(max_age) = state.retention.max_age_ms() else {
                return 0;
            };
            let cutoff = now_ms.saturating_sub(max_age);
            state
                .entries
                .values()
                .filter(|entry| entry.timestamp_ms < cutoff && !state.active.contains(&entry.id))
                .map(|entry| (entry.id.clone(), entry.volatile))
                .collect()
        };
        if expired.is_empty() {
            return 0;
        }
        let mut removed = Vec::with_capacity(expired.len());
        for (id, volatile) in expired {
            if !volatile && let Err(error) = remove_recording_files(&self.directory, &id) {
                tracing::warn!(%error, "could not remove expired recovery recording");
                continue;
            }
            removed.push(id);
        }
        if removed.is_empty() {
            return 0;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0;
        for id in removed {
            if !state.active.contains(&id) && state.entries.remove(&id).is_some() {
                state.volatile.remove(&id);
                count += 1;
            }
        }
        if count > 0 {
            state.revision += 1;
        }
        count
    }

    /// Changes whenever entries change or an entry crosses the retention window.
    pub fn view_key(&self) -> (u64, usize) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let visible = match state.retention.max_age_ms() {
            Some(max_age) => {
                let cutoff = crate::history::now_ms().saturating_sub(max_age);
                state
                    .entries
                    .values()
                    .filter(|entry| {
                        entry.timestamp_ms >= cutoff || state.active.contains(&entry.id)
                    })
                    .count()
            }
            None => state.entries.len(),
        };
        (state.revision, visible)
    }

    pub fn entries(&self, query: &str) -> Vec<RecoveryEntry> {
        self.prune_expired_at(crate::history::now_ms());
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let needle = query.trim().to_lowercase();
        let mut entries: Vec<_> = state
            .entries
            .values()
            .filter(|entry| {
                needle.is_empty()
                    || entry.title().to_lowercase().contains(&needle)
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
            return Err(io::Error::other(t("Invalid recording identity")));
        }
        Ok(self.directory.join(format!("{id}.wav")))
    }

    fn save_metadata(&self, entry: &RecoveryEntry) -> io::Result<()> {
        if !valid_id(&entry.id) {
            return Err(io::Error::other(t("Invalid recording identity")));
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
            entry.message = Some(t("Audio is only in memory because it could not be saved. Keep Endu open and free disk space before retrying.").into());
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
        state.revision += 1;
        entry
    }

    fn finish_failure(&self, entry: RecoveryEntry, message: &str) {
        let message = if entry.volatile {
            format!(
                "{message} Audio could not be saved to disk and is only in memory. Keep Endu open until recovery."
            )
        } else {
            message.to_owned()
        };
        self.record_failure(entry, message);
    }

    fn finish_stopped(&self, mut entry: RecoveryEntry) {
        let base = t("Transcription was stopped. The saved audio is ready to retry.");
        let message = if entry.volatile {
            format!(
                "{base} Audio could not be saved to disk and is only in memory. Keep Endu open until recovery."
            )
        } else {
            base.to_owned()
        };
        entry.status = RecoveryStatus::Pending;
        entry.message = Some(message);
        if !entry.volatile
            && let Err(error) = self.save_metadata(&entry)
        {
            tracing::warn!(%error, "could not update stopped recording status");
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.insert(entry.id.clone(), entry);
        state.revision += 1;
    }

    fn record_failure(&self, mut entry: RecoveryEntry, message: String) {
        entry.status = RecoveryStatus::Failed;
        entry.message = Some(message);
        if !entry.volatile
            && let Err(error) = self.save_metadata(&entry)
        {
            tracing::warn!(%error,"could not update saved recording status");
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // ActiveAttempt alone releases ownership. Releasing here would allow
        // Retry to start before the original guard drops and clears the new owner.
        state.entries.insert(entry.id.clone(), entry);
        state.revision += 1;
    }

    /// Commits `text` as `Recovered` before removing the audio. In-memory
    /// entries whose metadata cannot be written stay recovered in memory.
    /// Returns false, with nothing changed, when saved audio would lose its
    /// only durable companion.
    fn keep_recovered(&self, mut entry: RecoveryEntry, text: String) -> bool {
        entry.status = RecoveryStatus::Recovered;
        entry.message = None;
        entry.text = Some(text);
        if self.save_metadata(&entry).is_ok() {
            if !entry.volatile {
                let _ =
                    fs::remove_file(self.audio_path(&entry.id).expect("validated recording id"));
            }
            entry.volatile = false;
        } else if !entry.volatile {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.volatile.remove(&entry.id);
        state.entries.insert(entry.id.clone(), entry);
        state.revision += 1;
        true
    }

    fn resolve_entry(&self, mut entry: RecoveryEntry) {
        if entry.volatile {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.entries.remove(&entry.id);
            state.volatile.remove(&entry.id);
            state.revision += 1;
            return;
        }
        // A terminal marker prevents an orphan WAV from being presented
        // again if cleanup stops between its two removals.
        entry.status = RecoveryStatus::Resolved;
        if self.save_metadata(&entry).is_ok() {
            let _ = remove_recording_files(&self.directory, &entry.id);
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.entries.remove(&entry.id);
            state.volatile.remove(&entry.id);
            state.revision += 1;
        } else {
            self.finish_failure(
                entry,
                t("Transcription completed, but cleanup failed. Audio retained."),
            );
        }
    }

    /// Run on the transcription worker, never on the capture callback. A failed
    /// remote attempt leaves both the original samples and its recovery entry.
    /// Format with the accepted job's preferences before resolving recovery;
    /// an unusable result keeps its audio and returns a visible failure.
    pub fn transcribe_original(
        &self,
        samples: &[f32],
        application: Option<&str>,
        preferences: crate::post_processing::Preferences,
        transcribe: impl FnOnce(&[f32]) -> color_eyre::Result<Transcription>,
    ) -> color_eyre::Result<Transcription> {
        let (result, pending) = self.transcribe_original_cancellable(
            samples,
            application,
            preferences,
            None,
            transcribe,
        );
        if let Some(pending) = pending {
            pending.finish();
        }
        result
    }

    /// Like [`Self::transcribe_original`], but treats an explicit user
    /// cancellation as resolved (removing temporary audio instead of leaving a
    /// failed recovery entry) and returns non-volatile post-success disk
    /// cleanup as [`PendingResolution`] so the caller can hand the transcript
    /// to paste before flushing `Resolved` metadata to disk.
    pub fn transcribe_original_cancellable(
        &self,
        samples: &[f32],
        application: Option<&str>,
        preferences: crate::post_processing::Preferences,
        user_cancelled: Option<&std::sync::atomic::AtomicBool>,
        transcribe: impl FnOnce(&[f32]) -> color_eyre::Result<Transcription>,
    ) -> (color_eyre::Result<Transcription>, Option<PendingResolution>) {
        self.transcribe_original_with_cancel(
            samples,
            application,
            preferences,
            || {
                if user_cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    RecoveryCancel::Discard
                } else {
                    RecoveryCancel::None
                }
            },
            transcribe,
        )
    }

    /// Like [`Self::transcribe_original_cancellable`], distinguishing between
    /// discarding a cancelled recording (Escape) and stopping transcription
    /// while keeping the saved audio ready to retry in History.
    pub fn transcribe_original_with_cancel(
        &self,
        samples: &[f32],
        application: Option<&str>,
        preferences: crate::post_processing::Preferences,
        cancel_state: impl Fn() -> RecoveryCancel,
        transcribe: impl FnOnce(&[f32]) -> color_eyre::Result<Transcription>,
    ) -> (color_eyre::Result<Transcription>, Option<PendingResolution>) {
        let entry = self.begin(samples, application);
        let active = ActiveAttempt {
            store: self.clone(),
            id: entry.id.clone(),
            retry: false,
        };
        let result = transcribe(samples).and_then(|mut transcription| {
            // An empty local silence result is intentional. A remote result or
            // text removed by formatting must remain available for recovery.
            if transcription.text.trim().is_empty() {
                if transcription.report.is_some() {
                    return Err(UnusableTranscript::EmptyResponse.into());
                }
            } else {
                transcription.text = preferences
                    .process(transcription.text.trim())
                    .trim()
                    .to_owned();
                if transcription.text.is_empty() {
                    return Err(UnusableTranscript::EmptyAfterFormatting.into());
                }
            }
            Ok(transcription)
        });
        let no_speech = result.as_ref().is_err_and(|error| {
            error
                .downcast_ref::<crate::openrouter::transcribe::NoSpeech>()
                .is_some()
        });
        let cancel = cancel_state();
        if cancel == RecoveryCancel::KeepForRetry {
            self.finish_stopped(entry);
            drop(active);
            return (result, None);
        }
        match &result {
            Ok(_) => {
                if entry.volatile {
                    self.resolve_entry(entry);
                    drop(active);
                    (result, None)
                } else {
                    (
                        result,
                        Some(PendingResolution {
                            store: self.clone(),
                            entry: Some(entry),
                            _active: active,
                        }),
                    )
                }
            }
            // Silence and explicit user cancellation have nothing to recover.
            Err(_) if no_speech || cancel == RecoveryCancel::Discard => {
                self.resolve_entry(entry);
                drop(active);
                (result, None)
            }
            Err(error) => {
                self.finish_failure(entry, &transcription_failure_reason(error));
                drop(active);
                (result, None)
            }
        }
    }

    /// Keeps accepted audio whose transcription was stopped before it began,
    /// ready for a manual Retry in History.
    pub fn save_stopped(&self, samples: &[f32], application: Option<&str>) {
        let entry = self.begin(samples, application);
        let _active = ActiveAttempt {
            store: self.clone(),
            id: entry.id.clone(),
            retry: false,
        };
        self.finish_stopped(entry);
    }

    /// Keeps accepted audio that can no longer be transcribed, such as a
    /// released clip still recording its tail when Endu quits, as a failed
    /// entry for a manual Retry.
    pub fn save_untranscribed(&self, samples: &[f32], application: Option<&str>) {
        let entry = self.begin(samples, application);
        let _active = ActiveAttempt {
            store: self.clone(),
            id: entry.id.clone(),
            retry: false,
        };
        if entry.volatile {
            // Quitting ends the process that holds the in-memory copy, so
            // "keep Endu open" cannot apply: say the audio is lost instead.
            tracing::warn!("audio sealed at quit could not be saved for recovery");
            let message = t(
                "Endu quit before this recording was transcribed, and its audio could not be saved to disk.",
            );
            self.record_failure(entry, message.to_owned());
            return;
        }
        self.finish_failure(entry, t("Endu quit before this recording was transcribed."));
    }

    /// Retries a failed recording using either the current Models chain (`None`)
    /// or a one-off batch-capable model override (`Some(model_id)`) without
    /// modifying `openrouter.json`.
    pub fn retry_with_model(&self, id: &str, model_override: Option<String>) -> io::Result<()> {
        let model_override = model_override
            .map(|model| model.trim().to_owned())
            .filter(|model| !model.is_empty());
        if let Some(model) = model_override.as_deref()
            && crate::providers::is_realtime_only(model)
        {
            return Err(io::Error::other(t(
                "Only recorded-audio models can retry a saved recording.",
            )));
        }
        let vocabulary = crate::vocabulary::Snapshot::current();
        let remote = vocabulary.clone();
        self.retry_with_processing(
            id,
            crate::post_processing::Preferences::current(),
            vocabulary,
            move |samples| {
                crate::providers::batch::transcribe_with_model_override(
                    samples,
                    &remote,
                    model_override.as_deref(),
                )
            },
        )
    }

    /// Plays back a saved or volatile recovery recording locally over the
    /// default audio output. Stops any previously playing recovery clip first.
    pub fn play(&self, id: &str) -> io::Result<RecoveryPlayback> {
        let samples = self.read_audio(id)?;
        if samples.is_empty() {
            return Err(io::Error::other(t("This recording has no audio to play.")));
        }
        RecoveryPlayback::start(id.to_owned(), samples)
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
                return Err(io::Error::other(t(
                    "A transcription is already running. Wait for it to finish.",
                )));
            }
            let entry = state
                .entries
                .get(id)
                .filter(|e| e.status != RecoveryStatus::Recovered)
                .cloned()
                .ok_or_else(|| io::Error::other(t("This recording is not available to retry")))?;
            state.active.insert(id.to_owned());
            state.retrying = true;
            state.revision += 1;
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
            state.revision += 1;
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
            return Err(io::Error::other(t("Saved audio has an unsupported format")));
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
                    t("Could not read the saved audio. The recovery files were preserved."),
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
                    self.finish_failure(entry, t("Post-processing removed all text. Change its settings before Retry; the saved audio was kept."));
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
                        t("Could not save the recovered text. Audio retained; keep Endu open."),
                    );
                    return;
                }
                entry.volatile = false;
                let _ =
                    fs::remove_file(self.audio_path(&entry.id).expect("validated recording id"));
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.volatile.remove(&entry.id);
                state.entries.insert(entry.id.clone(), entry);
                state.revision += 1;
            }
            Ok(_) => self.finish_failure(
                entry,
                t("The transcription returned no text. The saved audio has been kept."),
            ),
            Err(error) => self.finish_failure(entry, &transcription_failure_reason(&error)),
        }
    }

    pub fn delete(&self, id: &str) -> io::Result<()> {
        self.audio_path(id)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.contains(id) {
            return Err(io::Error::other(t(
                "Wait for the transcription to finish before deleting it",
            )));
        }
        let Some(entry) = state.entries.get(id) else {
            return Err(io::Error::other(t("Recording no longer exists")));
        };
        if !entry.volatile {
            remove_recording_files(&self.directory, id)?;
        }
        state.entries.remove(id);
        state.volatile.remove(id);
        state.revision += 1;
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
            return t("No transcription models are configured. Choose a model in Models.").into();
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
    if let Some(reason) = error.downcast_ref::<UnusableTranscript>() {
        return reason.to_string();
    }
    let detail = format!("{error:#}").to_lowercase();
    if detail.contains("api key not found") {
        return t("The OpenRouter API key is missing. Add it in Models before retrying.").into();
    }
    if detail.contains("no openrouter transcription models") {
        return t("No transcription models are configured. Choose a model in Models.").into();
    }
    if error.downcast_ref::<serde_json::Error>().is_some() {
        return t("The local transcription configuration is invalid. Check Models settings.")
            .into();
    }
    if detail.contains("could not start curl") || detail.contains("curl stdin unavailable") {
        return t("The network client could not be started.").into();
    }
    if let Some(reason) = network_reason(&detail) {
        return reason;
    }
    if let Some(error) = error.downcast_ref::<io::Error>() {
        return match error.kind() {
            io::ErrorKind::PermissionDenied => {
                t("Permission denied while reading local transcription settings.")
            }
            io::ErrorKind::NotFound => {
                t("A local file required for transcription could not be found.")
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
            t("The API rate limit was reached (HTTP 429). Wait before retrying.").into()
        }
        ErrorKind::Auth => format!(
            "The API rejected the key or its access permissions{http}. Check the key in Models."
        ),
        ErrorKind::Server => {
            format!("The API returned a server error{http}. Retry when the service is available.")
        }
        ErrorKind::Rejected => match status {
            Some(402) => t("The API requires payment or additional credits (HTTP 402).").into(),
            Some(404) => t("The requested model or API endpoint was not found (HTTP 404).").into(),
            Some(413) => {
                t("The API rejected the audio because the request was too large (HTTP 413).").into()
            }
            _ => {
                format!("The API rejected the transcription request{http}. Check Models settings.")
            }
        },
        ErrorKind::Timeout => {
            let lower = detail.to_lowercase();
            if lower.ends_with(": not tried, chain deadline reached") {
                return t("A fallback could not be attempted because the transcription time limit was reached.").into();
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
                    crate::i18n::decimal(format!("{:.1}", ms as f64 / 1000.0))
                };
                format!(
                    "The API request timed out after {seconds} seconds{http}. No transcription was returned."
                )
            } else {
                format!("The API request timed out{http}. No transcription was returned.")
            }
        }
        ErrorKind::Network => network_reason(&detail.to_lowercase()).unwrap_or_else(|| {
            t("The network connection to the API failed. Check your internet connection.").into()
        }),
        ErrorKind::InvalidResponse => {
            t("The API returned an invalid or empty transcription response.").into()
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
            t("The API server address could not be resolved (DNS error). Check your connection.")
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
        Some(t("The connection to the API was reset before transcription completed.").into())
    } else if detail.contains("failed to connect")
        || detail.contains("couldn't connect")
        || detail.contains("connection refused")
    {
        Some(
            t("The API server could not be reached. Check your connection or try again later.")
                .into(),
        )
    } else if detail.contains("network") || detail.contains("offline") || detail.contains("curl") {
        Some(t("The network connection to the API failed. Check your internet connection.").into())
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
        state.revision += 1;
        if self.retry {
            state.retrying = false;
        }
    }
}

fn remove_recording_files(directory: &Path, id: &str) -> io::Result<()> {
    if !valid_id(id) {
        return Err(io::Error::other(t("Invalid recording identity")));
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
                .transcribe_original(
                    &samples,
                    Some("Notes"),
                    crate::post_processing::Preferences::default(),
                    |_| Err(color_eyre::eyre::eyre!(
                        "simulated network failure with SECRET_DO_NOT_PERSIST"
                    ))
                )
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
            .transcribe_original(
                &[0.25; 32],
                Some("Notes"),
                crate::post_processing::Preferences::default(),
                |_| {
                    Ok(Transcription {
                        text: "done".into(),
                        report: None,
                    })
                },
            )
            .unwrap();
        assert_eq!(result.text, "done");
        assert!(store.entries("").is_empty());
        assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
    }

    #[test]
    fn original_empty_remote_result_keeps_audio_and_can_be_retried_after_reload() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let samples = [0.25; 160];
        let error = store
            .transcribe_original(
                &samples,
                Some("Notes"),
                crate::post_processing::Preferences::default(),
                |_| {
                    Ok(Transcription {
                        text: " \n\t ".into(),
                        report: Some(crate::openrouter::StepReport::default()),
                    })
                },
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("returned no text"));
        let store = fixture.store();
        let entry = store.entries("").remove(0);
        assert_eq!(entry.status, RecoveryStatus::Failed);
        assert_eq!(entry.application.as_deref(), Some("Notes"));
        assert!(entry.message.unwrap().contains("returned no text"));
        assert_eq!(&**store.read_audio(&entry.id).unwrap(), &samples);
        store
            .retry_with(&entry.id, |_| {
                Ok(Transcription {
                    text: "recovered speech".into(),
                    report: None,
                })
            })
            .unwrap();
        wait(&store);
        let recovered = fixture.store().entries("").remove(0);
        assert_eq!(recovered.status, RecoveryStatus::Recovered);
        assert_eq!(recovered.text.as_deref(), Some("recovered speech"));
        assert!(!store.audio_path(&entry.id).unwrap().exists());
    }

    #[test]
    fn original_formatting_that_removes_all_text_keeps_audio() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let samples = [0.25; 160];
        let preferences = crate::post_processing::Preferences {
            remove_ellipses: true,
            ..Default::default()
        };
        let error = store
            .transcribe_original(&samples, Some("Notes"), preferences, |_| {
                Ok(Transcription {
                    text: "...".into(),
                    report: Some(crate::openrouter::StepReport::default()),
                })
            })
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("Post-processing removed all text")
        );
        let store = fixture.store();
        let entry = store.entries("").remove(0);
        assert_eq!(entry.status, RecoveryStatus::Failed);
        assert!(
            entry
                .message
                .unwrap()
                .contains("Post-processing removed all text")
        );
        assert_eq!(&**store.read_audio(&entry.id).unwrap(), &samples);
    }

    #[test]
    fn original_local_silence_does_not_create_a_recovery_failure() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let result = store
            .transcribe_original(
                &[0.0; 160],
                Some("Notes"),
                crate::post_processing::Preferences::default(),
                |_| {
                    Ok(Transcription {
                        text: String::new(),
                        report: None,
                    })
                },
            )
            .unwrap();
        assert!(result.text.is_empty());
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
                .transcribe_original(
                    &[0.1; 160],
                    Some("Codex"),
                    crate::post_processing::Preferences::default(),
                    |_| {
                        Err(api_error(
                            ErrorKind::Timeout,
                            "Operation timed out after 30000 milliseconds",
                        ))
                    }
                )
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

    #[test]
    fn streamed_silence_fails_without_keeping_audio_for_recovery() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let error = store
            .transcribe_original(
                &[0.0; 160],
                Some("Notes"),
                crate::post_processing::Preferences::default(),
                |_| Err(crate::openrouter::transcribe::NoSpeech.into()),
            )
            .err()
            .expect("streamed silence is a failure");
        assert!(
            error
                .downcast_ref::<crate::openrouter::transcribe::NoSpeech>()
                .is_some()
        );
        assert!(store.entries("").is_empty());
        assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
    }

    #[test]
    fn user_cancelled_transcription_is_not_kept_in_recovery() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let (result, pending) = store.transcribe_original_cancellable(
            &[0.1; 160],
            Some("Notes"),
            crate::post_processing::Preferences::default(),
            Some(&cancelled),
            |_| {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                Err(color_eyre::eyre::eyre!("Transcription was cancelled."))
            },
        );
        assert!(result.is_err());
        assert!(pending.is_none());
        assert!(store.entries("").is_empty());
        assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
    }

    #[test]
    fn stopped_transcription_is_kept_in_recovery_for_retry() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let (result, pending) = store.transcribe_original_with_cancel(
            &[0.1; 160],
            Some("Notes"),
            crate::post_processing::Preferences::default(),
            || {
                if stopped.load(Ordering::Acquire) {
                    RecoveryCancel::KeepForRetry
                } else {
                    RecoveryCancel::None
                }
            },
            |_| {
                stopped.store(true, Ordering::Release);
                Err(color_eyre::eyre::eyre!("Transcription was cancelled."))
            },
        );
        assert!(result.is_err());
        assert!(pending.is_none());
        let entries = store.entries("");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, RecoveryStatus::Pending);
        assert!(!entries[0].busy);
        assert_eq!(entries[0].application.as_deref(), Some("Notes"));
        assert_eq!(entries[0].title(), "Interrupted transcription");
        assert!(
            entries[0]
                .message
                .as_deref()
                .is_some_and(|message| message.contains("stopped"))
        );

        // Retrying the stopped entry recovers the transcript and deletes audio.
        let id = entries[0].id.clone();
        store
            .retry_with(&id, |_| {
                Ok(Transcription {
                    text: "retried after stop".into(),
                    report: None,
                })
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let entries = store.entries("");
            if entries[0].status == RecoveryStatus::Recovered && !entries[0].busy {
                assert_eq!(entries[0].text.as_deref(), Some("retried after stop"));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "retry did not finish");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // `save_stopped` and `PendingResolution::preserve_stopped` keep audio too.
        store.save_stopped(&[0.2; 320], Some("Slack"));
        let (ok, pending) = store.transcribe_original_with_cancel(
            &[0.3; 320],
            Some("Mail"),
            crate::post_processing::Preferences::default(),
            || RecoveryCancel::None,
            |_| {
                Ok(Transcription {
                    text: "late".into(),
                    report: None,
                })
            },
        );
        assert!(ok.is_ok());
        pending.unwrap().preserve_stopped();
        let entries = store.entries("");
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.status == RecoveryStatus::Pending && !entry.busy)
                .count(),
            2
        );
    }

    #[test]
    fn volatile_entry_cleans_up_in_memory_on_success_and_delete() {
        let fixture = Fixture::new();
        let store = fixture.store();
        fs::remove_dir(&fixture.0).unwrap();
        fs::write(&fixture.0, "block directory creation").unwrap();
        // Success cleans up volatile state without recording a false failure.
        let ok = store
            .transcribe_original(
                &[0.1; 160],
                Some("Notes"),
                crate::post_processing::Preferences::default(),
                |_| {
                    Ok(Transcription {
                        text: "hello".into(),
                        report: None,
                    })
                },
            )
            .unwrap();
        assert_eq!(ok.text, "hello");
        assert!(store.entries("").is_empty());
        assert!(store.state.lock().unwrap().volatile.is_empty());

        // Failed volatile entry can be deleted without failing on unwritable directory.
        let _ = store.transcribe_original(
            &[0.1; 160],
            Some("Notes"),
            crate::post_processing::Preferences::default(),
            |_| Err(color_eyre::eyre::eyre!("offline")),
        );
        let entries = store.entries("");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].volatile);
        store.delete(&entries[0].id).unwrap();
        assert!(store.entries("").is_empty());
        assert!(store.state.lock().unwrap().volatile.is_empty());
        let _ = fs::remove_file(&fixture.0);
    }

    #[test]
    fn alternate_model_retry_rejects_realtime_only_models_and_playback_stops_cleanly() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = failed(&store);
        let error = store
            .retry_with_model(&id, Some("elevenlabs::scribe_v2_realtime".into()))
            .unwrap_err();
        assert!(error.to_string().contains("recorded-audio"));
        assert!(!store.retry_in_progress());

        let playback = store.play(&id).unwrap();
        assert!(playback.is_playing(&id));
        stop_active_playback();
        assert!(!playback.is_playing(&id));
    }

    #[test]
    fn failed_recordings_respect_timed_history_retention_and_keep_active_or_off() {
        use crate::history::HistoryRetention;
        let fixture = Fixture::new();
        let store = fixture.store();
        let old_id = failed(&store);
        let fresh_id = failed(&store);
        let now = crate::history::now_ms();
        let week_ms = HistoryRetention::Week.max_age_ms().unwrap();

        // Backdate old_id to 10 days ago on disk and in memory.
        {
            let mut state = store.state.lock().unwrap();
            let entry = state.entries.get_mut(&old_id).unwrap();
            entry.timestamp_ms = now.saturating_sub(week_ms + 3 * 24 * 60 * 60 * 1_000);
            let clone = entry.clone();
            drop(state);
            store.save_metadata(&clone).unwrap();
        }

        // Off and Forever keep all failed entries.
        store.set_retention(HistoryRetention::Off);
        assert_eq!(store.entries("").len(), 2);
        store.set_retention(HistoryRetention::Forever);
        assert_eq!(store.entries("").len(), 2);

        // Active attempts are never pruned even past retention.
        store.state.lock().unwrap().active.insert(old_id.clone());
        store.set_retention(HistoryRetention::Week);
        assert_eq!(store.entries("").len(), 2);
        store.state.lock().unwrap().active.remove(&old_id);

        // Once inactive, Week retention removes the 10-day-old failure and its files,
        // while keeping the fresh failure intact.
        assert_eq!(store.prune_expired_at(now), 1);
        let remaining = store.entries("");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, fresh_id);
        assert!(!fixture.0.join(format!("{old_id}.wav")).exists());
        assert!(!fixture.0.join(format!("{old_id}.json")).exists());
        assert!(fixture.0.join(format!("{fresh_id}.wav")).exists());

        // Reopening with Day retention prunes entries older than 24 hours on startup.
        let reopened =
            RecordingRecovery::open_with_retention(fixture.0.clone(), HistoryRetention::Day)
                .unwrap();
        assert_eq!(reopened.entries("").len(), 1);
        assert_eq!(
            reopened.prune_expired_at(now + HistoryRetention::Day.max_age_ms().unwrap() + 1_000),
            1
        );
        assert!(reopened.entries("").is_empty());
    }
}
