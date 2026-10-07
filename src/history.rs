//! Retained dictation history: an owner-only, bounded, crash-safe store of
//! successful dictations.
//!
//! History is a product record, deliberately separate from the diagnostic
//! event stream. Entries hold text and bounded metadata only; audio is never
//! retained here. Every retention choice remains subject to hard
//! entry and byte caps, writes are atomic, and files are owner-only.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
const MAX_ENTRIES: usize = 2_000;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_LABEL_BYTES: usize = 256;
const MAX_TOTAL_TEXT_BYTES: usize = 4 * 1024 * 1024;
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);
pub const MAX_SEARCH_RESULTS: usize = 200;

/// How long successful results remain in retained history.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRetention {
    Off,
    Day,
    #[default]
    Week,
    Month,
    Forever,
}

impl HistoryRetention {
    pub const ALL: [Self; 5] = [Self::Off, Self::Day, Self::Week, Self::Month, Self::Forever];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Day => "24 hours",
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::Forever => "Forever",
        }
    }

    pub const fn is_off(self) -> bool {
        matches!(self, Self::Off)
    }

    const fn max_age_ms(self) -> Option<u64> {
        const HOUR_MS: u64 = 60 * 60 * 1_000;
        match self {
            Self::Off | Self::Forever => None,
            Self::Day => Some(24 * HOUR_MS),
            Self::Week => Some(7 * 24 * HOUR_MS),
            Self::Month => Some(30 * 24 * HOUR_MS),
        }
    }
}

/// One retained successful dictation. Files from older builds keep their text
/// and OpenRouter transcription report; removed features are ignored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(from = "LoadedHistoryEntry")]
pub struct HistoryEntry {
    pub id: u64,
    pub timestamp_ms: u64,
    /// Text that was actually inserted.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    pub audio_ms: u64,
    pub inference_ms: u64,
    pub total_ms: u64,
    /// The OpenRouter model, latency, fallbacks, and trimming.
    #[serde(
        rename = "openrouter_transcription",
        skip_serializing_if = "Option::is_none"
    )]
    pub transcription: Option<crate::openrouter::StepReport>,
}

#[derive(Deserialize)]
struct LoadedHistoryEntry {
    id: u64,
    timestamp_ms: u64,
    #[serde(alias = "final_text")]
    text: String,
    #[serde(default)]
    application: Option<String>,
    #[serde(default)]
    audio_ms: u64,
    #[serde(default)]
    inference_ms: u64,
    #[serde(default)]
    total_ms: u64,
    #[serde(default, deserialize_with = "deserialize_present_transcription")]
    openrouter_transcription: Option<Option<crate::openrouter::StepReport>>,
    #[serde(default)]
    openrouter: Option<LegacyOpenRouterReport>,
}

#[derive(Deserialize)]
struct LegacyOpenRouterReport {
    #[serde(default)]
    transcription: Option<crate::openrouter::StepReport>,
}

// Missing means migrate the legacy report. Explicit null is a current value
// and must take precedence over a legacy report in the same entry.
fn deserialize_present_transcription<'de, D>(
    deserializer: D,
) -> Result<Option<Option<crate::openrouter::StepReport>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<crate::openrouter::StepReport>::deserialize(deserializer).map(Some)
}

impl From<LoadedHistoryEntry> for HistoryEntry {
    fn from(entry: LoadedHistoryEntry) -> Self {
        Self {
            id: entry.id,
            timestamp_ms: entry.timestamp_ms,
            text: entry.text,
            application: entry.application,
            audio_ms: entry.audio_ms,
            inference_ms: entry.inference_ms,
            total_ms: entry.total_ms,
            transcription: entry
                .openrouter_transcription
                .unwrap_or_else(|| entry.openrouter.and_then(|report| report.transcription)),
        }
    }
}

impl HistoryEntry {
    fn text_bytes(&self) -> usize {
        self.text.len()
    }

    fn matches(&self, needle: &str) -> bool {
        let matches_field = |field: &str| field.to_lowercase().contains(needle);
        matches_field(&self.text)
            || self.application.as_deref().is_some_and(matches_field)
            || self
                .transcription
                .as_ref()
                .and_then(|report| report.model.as_deref())
                .is_some_and(matches_field)
    }
}

/// A successful result awaiting a history identity.
#[derive(Clone, Debug)]
pub struct HistoryDraft {
    pub text: String,
    pub application: Option<String>,
    pub audio_ms: u64,
    pub inference_ms: u64,
    pub total_ms: u64,
    pub transcription: Option<crate::openrouter::StepReport>,
}

#[derive(Serialize)]
struct SavedHistory<'a> {
    version: u32,
    next_id: u64,
    entries: &'a [HistoryEntry],
}

#[derive(Deserialize)]
struct LoadedHistory {
    version: u32,
    next_id: u64,
    entries: Vec<HistoryEntry>,
}

/// Owner-only bounded history store. Entries are held oldest-first.
pub struct HistoryStore {
    path: PathBuf,
    entries: Vec<HistoryEntry>,
    next_id: u64,
    retention: HistoryRetention,
    prune_pending: bool,
    prune_error_reported: bool,
}

impl HistoryStore {
    /// Open the store, recover from unreadable content, and prune expired
    /// entries. A malformed file is preserved beside the store instead of
    /// being silently destroyed.
    pub fn open(path: PathBuf, retention: HistoryRetention, now_ms: u64) -> Self {
        let mut store = Self {
            path,
            entries: Vec::new(),
            next_id: 1,
            retention,
            prune_pending: false,
            prune_error_reported: false,
        };
        match fs::read(&store.path) {
            Ok(bytes) => match serde_json::from_slice::<LoadedHistory>(&bytes) {
                Ok(loaded) if loaded.version == VERSION => {
                    let max_entry_id = loaded.entries.iter().map(|entry| entry.id).max();
                    store.next_id = loaded
                        .next_id
                        .max(max_entry_id.map_or(1, |id| id.saturating_add(1)));
                    store.entries = loaded.entries;
                    store.entries.sort_by_key(|entry| entry.id);
                }
                Ok(loaded) => {
                    tracing::warn!(version = loaded.version, "unsupported history version");
                    store.preserve_corrupt();
                }
                Err(error) => {
                    tracing::warn!(%error, path = %store.path.display(), "history file is malformed");
                    store.preserve_corrupt();
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %store.path.display(), "could not read history");
            }
        }
        store.expire(now_ms);
        store
    }

    /// Record one successful result. Returns the stable entry ID, or `None`
    /// while retention is off.
    pub fn record(&mut self, draft: HistoryDraft, now_ms: u64) -> io::Result<Option<u64>> {
        if self.retention.is_off() {
            return Ok(None);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push(HistoryEntry {
            id,
            timestamp_ms: now_ms,
            text: truncated(&draft.text, MAX_TEXT_BYTES),
            application: draft
                .application
                .map(|application| truncated(&application, MAX_LABEL_BYTES)),
            audio_ms: draft.audio_ms,
            inference_ms: draft.inference_ms,
            total_ms: draft.total_ms,
            transcription: draft
                .transcription
                .map(crate::openrouter::StepReport::bounded),
        });
        self.prune(now_ms);
        self.persist()?;
        Ok(Some(id))
    }

    /// Delete one entry. Returns whether it existed.
    pub fn delete(&mut self, id: u64) -> io::Result<bool> {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        if self.entries.len() == before {
            return Ok(false);
        }
        self.persist()?;
        Ok(true)
    }

    /// Delete every entry.
    pub fn clear(&mut self) -> io::Result<()> {
        // A corrupt-file backup predating the current store is stale once the
        // user clears history deliberately.
        let _ = fs::remove_file(self.path.with_extension("json.corrupt"));
        if self.entries.is_empty() {
            return Ok(());
        }
        self.entries.clear();
        self.persist()
    }

    /// Change retention and immediately prune to the new window. Turning
    /// history off stops recording but keeps existing entries until they are
    /// deleted explicitly.
    pub fn set_retention(&mut self, retention: HistoryRetention, now_ms: u64) -> io::Result<()> {
        if self.retention == retention {
            return Ok(());
        }
        self.retention = retention;
        if self.prune(now_ms) {
            self.persist()?;
        }
        Ok(())
    }

    /// All entries, newest first.
    pub fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter().rev()
    }

    #[cfg(test)]
    pub fn entry(&self, id: u64) -> Option<&HistoryEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Case-insensitive substring search over text, application, and model.
    /// Newest first and bounded.
    pub fn search(&self, query: &str, now_ms: u64) -> Vec<&HistoryEntry> {
        let needle = query.trim().to_lowercase();
        let cutoff = self
            .retention
            .max_age_ms()
            .map(|max_age| now_ms.saturating_sub(max_age));
        self.entries()
            .filter(|entry| cutoff.is_none_or(|cutoff| entry.timestamp_ms >= cutoff))
            .filter(|entry| needle.is_empty() || entry.matches(&needle))
            .take(MAX_SEARCH_RESULTS)
            .collect()
    }

    fn expire(&mut self, now_ms: u64) {
        self.prune(now_ms);
        if self.prune_pending
            && let Err(error) = self.persist()
        {
            if !self.prune_error_reported {
                tracing::warn!(%error, "could not persist pruned history; will retry");
            }
            self.prune_error_reported = true;
        }
    }

    fn prune(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        if let Some(max_age) = self.retention.max_age_ms() {
            let cutoff = now_ms.saturating_sub(max_age);
            let before = self.entries.len();
            self.entries.retain(|entry| entry.timestamp_ms >= cutoff);
            changed |= self.entries.len() != before;
        }
        while self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
            changed = true;
        }
        let mut total: usize = self.entries.iter().map(HistoryEntry::text_bytes).sum();
        while total > MAX_TOTAL_TEXT_BYTES && self.entries.len() > 1 {
            total -= self.entries.remove(0).text_bytes();
            changed = true;
        }
        self.prune_pending |= changed;
        changed
    }

    fn persist(&mut self) -> io::Result<()> {
        let saved = SavedHistory {
            version: VERSION,
            next_id: self.next_id,
            entries: &self.entries,
        };
        let json = serde_json::to_vec(&saved).map_err(io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&temporary)?;
            restrict_to_owner(&file)?;
            file.write_all(&json)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &self.path)?;
        self.prune_pending = false;
        self.prune_error_reported = false;
        Ok(())
    }

    fn preserve_corrupt(&self) {
        let backup = self.path.with_extension("json.corrupt");
        if let Err(error) = fs::rename(&self.path, &backup) {
            tracing::warn!(%error, "could not preserve the malformed history file");
        }
    }
}

/// Thread-safe shared handle over one history store.
#[derive(Clone)]
pub struct History {
    store: Arc<Mutex<HistoryStore>>,
    // Only handles own senders: dropping the last clone wakes and stops the worker.
    _shutdown: mpsc::Sender<()>,
}

impl History {
    pub fn new(store: HistoryStore) -> Self {
        Self::with_clock(store, PRUNE_INTERVAL, now_ms)
    }

    fn with_clock(
        store: HistoryStore,
        interval: Duration,
        clock: impl Fn() -> u64 + Send + 'static,
    ) -> Self {
        let store = Arc::new(Mutex::new(store));
        let (shutdown, stopped) = mpsc::channel();
        let worker_store = Arc::clone(&store);
        if let Err(error) = thread::Builder::new()
            .name("history-retention".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    let mut store = match worker_store.try_lock() {
                        Ok(store) => store,
                        Err(TryLockError::WouldBlock) => continue,
                        Err(TryLockError::Poisoned(error)) => error.into_inner(),
                    };
                    store.expire(clock());
                }
            })
        {
            tracing::warn!(%error, "could not start history retention worker");
        }
        Self {
            store,
            _shutdown: shutdown,
        }
    }

    /// Open the default store in Application Support.
    pub fn open_default(retention: HistoryRetention) -> color_eyre::Result<Self> {
        let path = crate::app_paths::support_dir()?.join("history.json");
        Ok(Self::new(HistoryStore::open(path, retention, now_ms())))
    }

    pub fn record(&self, draft: HistoryDraft) -> io::Result<Option<u64>> {
        self.locked().record(draft, now_ms())
    }

    pub fn delete(&self, id: u64) -> io::Result<bool> {
        self.locked().delete(id)
    }

    pub fn clear(&self) -> io::Result<()> {
        self.locked().clear()
    }

    pub fn set_retention(&self, retention: HistoryRetention) -> io::Result<()> {
        self.locked().set_retention(retention, now_ms())
    }

    /// Bounded snapshot of matching entries, newest first.
    pub fn search(&self, query: &str) -> Vec<HistoryEntry> {
        self.locked()
            .search(query, now_ms())
            .into_iter()
            .cloned()
            .collect()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, HistoryStore> {
        self.store.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn truncated(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(unix)]
fn restrict_to_owner(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_to_owner(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("hex-history-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory.join("history.json")
    }

    fn draft(text: &str) -> HistoryDraft {
        HistoryDraft {
            text: text.to_string(),
            application: Some("Zed".into()),
            audio_ms: 900,
            inference_ms: 80,
            total_ms: 1_100,
            transcription: Some(crate::openrouter::StepReport {
                executions: Vec::new(),
                model: Some("openai/whisper-large-v3-turbo".into()),
                latency_ms: 120,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn records_survive_reopen_with_monotonic_ids() {
        let path = temp_path("reopen");
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Week, 1_000);
        let first = store.record(draft("first"), 1_000).unwrap().unwrap();
        let second = store.record(draft("second"), 2_000).unwrap().unwrap();
        assert!(second > first);
        store.delete(second).unwrap();

        let mut reopened = HistoryStore::open(path, HistoryRetention::Week, 2_500);
        assert_eq!(reopened.len(), 1);
        let third = reopened.record(draft("third"), 3_000).unwrap().unwrap();
        assert!(third > second, "deleted IDs are never reused");
        let texts: Vec<_> = reopened
            .entries()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(texts, ["third", "first"]);
    }

    #[test]
    fn retention_prunes_expired_entries_on_open_and_write() {
        let path = temp_path("retention");
        let day_ms = 24 * 60 * 60 * 1_000;
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Day, 0);
        store.record(draft("old"), 1_000).unwrap();
        store.record(draft("fresh"), day_ms).unwrap();

        // A write one day later prunes the first entry exactly at the cutoff.
        store.record(draft("newest"), day_ms + 1_001).unwrap();
        let texts: Vec<_> = store.entries().map(|entry| entry.text.as_str()).collect();
        assert_eq!(texts, ["newest", "fresh"]);

        let reopened = HistoryStore::open(path, HistoryRetention::Day, 2 * day_ms + 500);
        let texts: Vec<_> = reopened
            .entries()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(texts, ["newest"]);
    }

    #[test]
    fn search_hides_expired_entries_before_the_next_idle_prune() {
        let path = temp_path("search-expiry");
        let day_ms = HistoryRetention::Day.max_age_ms().unwrap();
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Day, 0);
        store.record(draft("old"), 1_000).unwrap();
        store.record(draft("fresh"), 2_000).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();

        assert_eq!(store.search("", day_ms + 1_000).len(), 2);
        assert!(store.search("old", day_ms + 1_001).is_empty());
        let matches = store.search("", day_ms + 1_001);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "fresh");
        assert_eq!(
            store.len(),
            2,
            "reads filter without writing on the UI thread"
        );
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), UNIX_EPOCH);
    }

    #[test]
    fn idle_worker_expires_entries_on_disk_without_recording_or_reading() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let path = temp_path("idle-expiry");
        let day_ms = HistoryRetention::Day.max_age_ms().unwrap();
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Day, 0);
        store.record(draft("old"), 1_000).unwrap();
        store.record(draft("fresh"), 2_000).unwrap();
        let clock = Arc::new(AtomicU64::new(2_000));
        let worker_clock = Arc::clone(&clock);
        let (ticks, ticked) = mpsc::channel();
        let history = History::with_clock(store, Duration::from_millis(10), move || {
            let now = worker_clock.load(Ordering::SeqCst);
            let _ = ticks.send(now);
            now
        });
        let tick = |now| {
            clock.store(now, Ordering::SeqCst);
            while ticked.recv_timeout(Duration::from_secs(2)).unwrap() != now {}
            // The clock is sampled under the lock; acquiring it waits for that prune.
            drop(history.locked());
        };
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();

        tick(day_ms + 1_000);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), UNIX_EPOCH);

        tick(day_ms + 1_001);
        let saved: LoadedHistory = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.entries.len(), 1);
        assert_eq!(saved.entries[0].text, "fresh");

        tick(day_ms + 2_001);
        let saved: LoadedHistory = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.entries.is_empty());
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();
        tick(2 * day_ms);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), UNIX_EPOCH);
    }

    #[test]
    fn failed_expiry_persistence_retries_without_new_removals() {
        let path = temp_path("expiry-retry");
        let day_ms = HistoryRetention::Day.max_age_ms().unwrap();
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Day, 0);
        store.record(draft("old"), 1_000).unwrap();
        let temporary = path.with_extension("json.tmp");
        fs::create_dir(&temporary).unwrap();

        store.expire(day_ms + 1_001);
        assert!(store.entries.is_empty());
        assert!(store.prune_pending);
        assert!(store.prune_error_reported);
        let saved: LoadedHistory = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.entries.len(), 1);
        store.expire(day_ms + 1_002);
        assert!(store.prune_pending);
        assert!(store.prune_error_reported);

        fs::remove_dir(temporary).unwrap();
        store.expire(day_ms + 1_003);
        assert!(!store.prune_pending);
        assert!(!store.prune_error_reported);
        let saved: LoadedHistory = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.entries.is_empty());
    }

    #[test]
    fn idle_worker_stops_when_the_last_history_handle_is_dropped() {
        let path = temp_path("worker-lifecycle");
        let store = HistoryStore::open(path, HistoryRetention::Day, 0);
        let (alive, stopped) = mpsc::channel::<()>();
        let history = History::with_clock(store, Duration::from_secs(24 * 60 * 60), move || {
            let _alive = &alive;
            0
        });
        let clone = history.clone();
        drop(history);
        assert_eq!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty));

        drop(clone);
        assert_eq!(
            stopped.recv_timeout(Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Disconnected),
            "shutdown must wake the worker rather than wait for its timer"
        );
    }

    #[test]
    fn choosing_week_retention_from_month_keeps_entries_newer_than_a_week() {
        let day_ms = 24 * 60 * 60 * 1_000;
        let now = 30 * day_ms;
        let path = temp_path("month-to-week");
        let mut store = HistoryStore::open(path, HistoryRetention::Month, 0);
        store
            .record(draft("twenty days old"), now - 20 * day_ms)
            .unwrap();
        store
            .record(draft("two days old"), now - 2 * day_ms)
            .unwrap();

        store.set_retention(HistoryRetention::Week, now).unwrap();

        let texts: Vec<_> = store.entries().map(|entry| entry.text.as_str()).collect();
        assert_eq!(texts, ["two days old"]);
    }

    #[test]
    fn off_records_nothing_but_preserves_existing_entries() {
        let path = temp_path("off");
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Week, 1_000);
        store.record(draft("kept"), 1_000).unwrap();
        store.set_retention(HistoryRetention::Off, 1_500).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();

        assert_eq!(store.record(draft("dropped"), 2_000).unwrap(), None);
        let later = HistoryRetention::Month.max_age_ms().unwrap() + 2_000;
        store.expire(later);
        assert_eq!(store.len(), 1);
        assert_eq!(store.search("kept", later).len(), 1);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), UNIX_EPOCH);
    }

    #[test]
    fn hard_caps_bound_entries_and_text_regardless_of_retention() {
        let path = temp_path("caps");
        let mut store = HistoryStore::open(path, HistoryRetention::Forever, 0);
        for index in 0..(MAX_ENTRIES + 5) {
            store
                .record(draft(&format!("entry {index}")), index as u64)
                .unwrap();
        }
        assert_eq!(store.len(), MAX_ENTRIES);

        let oversized = "x".repeat(MAX_TEXT_BYTES + 100);
        let id = store.record(draft(&oversized), 9_999_999).unwrap().unwrap();
        assert_eq!(store.entry(id).unwrap().text.len(), MAX_TEXT_BYTES);
    }

    #[test]
    fn total_text_bytes_evict_oldest_entries() {
        let path = temp_path("total-bytes");
        let mut store = HistoryStore::open(path, HistoryRetention::Forever, 0);
        let big = "y".repeat(MAX_TEXT_BYTES - 10);
        let per_entry = big.len();
        let fits = MAX_TOTAL_TEXT_BYTES / per_entry;
        for index in 0..(fits + 3) {
            store.record(draft(&big), index as u64).unwrap();
        }
        let total: usize = store.entries().map(|entry| entry.text.len()).sum();
        assert!(total <= MAX_TOTAL_TEXT_BYTES);
        assert!(store.len() < fits + 3);
    }

    #[test]
    fn search_is_case_insensitive_across_text_application_and_model() {
        let path = temp_path("search");
        let mut store = HistoryStore::open(path, HistoryRetention::Week, 0);
        store.record(draft("Hello World"), 1_000).unwrap();
        store.record(draft("other text"), 2_000).unwrap();

        assert_eq!(store.search("hello world", 2_000).len(), 1);
        assert_eq!(store.search("zed", 2_000).len(), 2);
        assert_eq!(store.search("whisper", 2_000).len(), 2);
        assert_eq!(store.search("absent", 2_000).len(), 0);
        assert_eq!(
            store.search("  ", 2_000).len(),
            2,
            "blank queries list everything"
        );
    }

    #[test]
    fn clear_removes_everything_durably() {
        let path = temp_path("clear");
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Week, 0);
        store.record(draft("one"), 1_000).unwrap();
        store.record(draft("two"), 2_000).unwrap();
        store.clear().unwrap();

        assert_eq!(store.len(), 0);
        let reopened = HistoryStore::open(path, HistoryRetention::Week, 3_000);
        assert_eq!(reopened.len(), 0);
    }

    #[test]
    fn malformed_files_are_preserved_and_recovered_from() {
        let path = temp_path("malformed");
        fs::write(&path, b"{ not json").unwrap();
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Week, 0);
        assert_eq!(store.len(), 0);
        assert!(path.with_extension("json.corrupt").exists());

        store.record(draft("fresh"), 1_000).unwrap();
        let reopened = HistoryStore::open(path, HistoryRetention::Week, 2_000);
        assert_eq!(reopened.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn history_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("permissions");
        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Week, 0);
        store.record(draft("private"), 1_000).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn legacy_history_keeps_text_and_transcription_after_persist_and_reopen() {
        let path = temp_path("legacy-migration");
        let legacy = serde_json::json!({
            "version": 1,
            "next_id": 8,
            "entries": [{
                "id": 7,
                "timestamp_ms": 1_000,
                "kind": "voice_action",
                "raw_text": "raw text",
                "final_text": "Final text.",
                "application": "Zed",
                "audio_ms": 9_400,
                "inference_ms": 820,
                "total_ms": 1_050,
                "processing": {"profile": "Messages", "latency_ms": 100},
                "openrouter": {
                    "transcription": {
                        "model": "openai/gpt-4o-mini-transcribe",
                        "latency_ms": 820,
                        "failed": ["openai/whisper-large-v3-turbo"],
                        "audio": {"recorded_ms": 9_400, "sent_ms": 6_100}
                    },
                    "cleanup": {"model": "old-cleanup-model", "latency_ms": 100}
                }
            }]
        });
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let expected = HistoryEntry {
            id: 7,
            timestamp_ms: 1_000,
            text: "Final text.".into(),
            application: Some("Zed".into()),
            audio_ms: 9_400,
            inference_ms: 820,
            total_ms: 1_050,
            transcription: Some(crate::openrouter::StepReport {
                executions: Vec::new(),
                model: Some("openai/gpt-4o-mini-transcribe".into()),
                latency_ms: 820,
                failed: vec!["openai/whisper-large-v3-turbo".into()],
                audio: Some(crate::openrouter::AudioTrim {
                    recorded_ms: 9_400,
                    sent_ms: 6_100,
                }),
            }),
        };

        let mut store = HistoryStore::open(path.clone(), HistoryRetention::Week, 2_000);
        assert_eq!(store.entry(7), Some(&expected));
        assert_eq!(store.search("gpt-4o-mini-transcribe", 2_000), [&expected]);
        assert_eq!(store.record(draft("new entry"), 2_000).unwrap(), Some(8));

        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let saved_entry = &saved["entries"][0];
        assert_eq!(saved_entry["text"], "Final text.");
        assert_eq!(
            saved_entry["openrouter_transcription"],
            legacy["entries"][0]["openrouter"]["transcription"]
        );
        for removed in ["final_text", "raw_text", "kind", "processing", "openrouter"] {
            assert!(saved_entry.get(removed).is_none());
        }

        let reopened = HistoryStore::open(path, HistoryRetention::Week, 3_000);
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.entry(7), Some(&expected));
        assert_eq!(
            reopened.search("gpt-4o-mini-transcribe", 3_000),
            [&expected]
        );
    }

    #[test]
    fn current_transcription_takes_precedence_over_legacy_including_null() {
        for current in [
            serde_json::json!({"model": "current/model", "latency_ms": 10}),
            serde_json::Value::Null,
        ] {
            let expected: Option<crate::openrouter::StepReport> =
                serde_json::from_value(current.clone()).unwrap();
            let entry: HistoryEntry = serde_json::from_value(serde_json::json!({
                "id": 1,
                "timestamp_ms": 2,
                "text": "final",
                "openrouter_transcription": current,
                "openrouter": {
                    "transcription": {"model": "legacy/model", "latency_ms": 4}
                }
            }))
            .unwrap();
            assert_eq!(entry.transcription, expected);

            let roundtrip: HistoryEntry =
                serde_json::from_value(serde_json::to_value(&entry).unwrap()).unwrap();
            assert_eq!(roundtrip, entry);
        }
    }

    #[test]
    fn legacy_entries_without_transcription_still_load() {
        for legacy_report in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({"cleanup": {"model": "old/model", "latency_ms": 4}}),
        ] {
            let entry: HistoryEntry = serde_json::from_value(serde_json::json!({
                "id": 1,
                "timestamp_ms": 2,
                "final_text": "final",
                "openrouter": legacy_report
            }))
            .unwrap();
            assert_eq!(entry.text, "final");
            assert!(entry.transcription.is_none());
        }
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        let text = "é".repeat(MAX_TEXT_BYTES);
        let truncated = truncated(&text, MAX_TEXT_BYTES);
        assert!(truncated.len() <= MAX_TEXT_BYTES);
        assert!(text.starts_with(&truncated));
    }
}
