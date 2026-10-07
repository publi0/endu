//! Aggregate dictation statistics: words, audio, tokens, cost, fallbacks, and
//! the errors that caused them. Daily totals only, never text, so they are
//! kept regardless of the History retention setting.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use color_eyre::Result;
use serde::{Deserialize, Serialize};

const FILE: &str = "stats.json";
const VERSION: u32 = 2;
/// Days kept on disk; older buckets are dropped.
const MAX_DAYS: usize = 400;

static LOCK: Mutex<()> = Mutex::new(());

/// Why one model attempt failed, grouped for display.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    RateLimited,
    Timeout,
    Server,
    Rejected,
    Auth,
    Network,
    InvalidResponse,
}

impl ErrorKind {
    pub const fn key(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::Server => "server",
            Self::Rejected => "rejected",
            Self::Auth => "auth",
            Self::Network => "network",
            Self::InvalidResponse => "invalid_response",
        }
    }

    pub fn label_for_key(key: &str) -> &'static str {
        match key {
            "rate_limited" => "Rate limited (HTTP 429)",
            "timeout" => "Timed out",
            "server" => "Provider error (HTTP 5xx)",
            "rejected" => "Request rejected (HTTP 4xx)",
            "auth" => "Key rejected (HTTP 401/403)",
            "network" => "Network error",
            "invalid_response" => "Invalid or empty response",
            _ => "Other",
        }
    }

    /// Classify an HTTP status from a failed attempt.
    pub fn from_status(status: u16) -> Self {
        match status {
            429 => Self::RateLimited,
            401 | 403 => Self::Auth,
            408 | 504 => Self::Timeout,
            500..=599 => Self::Server,
            _ => Self::Rejected,
        }
    }

    /// Classify a transport error message from curl.
    #[cfg(test)]
    pub fn from_transport(message: &str) -> Self {
        let lower = message.to_lowercase();
        if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("exit status: 28")
        {
            Self::Timeout
        } else {
            Self::Network
        }
    }
}

/// One failed model attempt.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub model: String,
    pub kind: ErrorKind,
    pub detail: String,
}

/// Timings of successful HTTP transcription requests, including network time.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelLatency {
    pub responses: u64,
    pub total_ms: u64,
}

impl ModelLatency {
    pub fn record(&mut self, latency_ms: u64) {
        self.responses += 1;
        self.total_ms += latency_ms;
    }

    pub fn average_ms(&self) -> Option<u64> {
        self.total_ms.checked_div(self.responses)
    }

    fn merge(&mut self, other: &Self) {
        self.responses += other.responses;
        self.total_ms += other.total_ms;
    }
}

/// What one dictation contributed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    /// `None` when nothing was transcribed (every model failed).
    pub words: Option<u64>,
    /// Models used by a successful dictation, counted once each.
    pub models: Vec<String>,
    pub model_latency: BTreeMap<String, ModelLatency>,
    pub recorded_ms: u64,
    pub sent_ms: u64,
    pub latency_ms: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    pub failures: Vec<Failure>,
    pub skipped_silent: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Totals {
    pub dictations: u64,
    pub failed_dictations: u64,
    pub skipped_silent: u64,
    pub words: u64,
    pub recorded_ms: u64,
    pub sent_ms: u64,
    pub latency_ms: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    /// Dictations that needed at least one fallback.
    pub fallbacks: u64,
    /// Successful dictations using each model; one dictation can use several.
    pub models: BTreeMap<String, u64>,
    /// Successful requests, not dictations; absent for data collected before 3.0.1.
    pub model_latency: BTreeMap<String, ModelLatency>,
    /// Failed attempts per error kind, then per model.
    pub errors: BTreeMap<String, BTreeMap<String, u64>>,
}

impl Totals {
    fn add_sample(&mut self, sample: &Sample) {
        self.recorded_ms += sample.recorded_ms;
        self.sent_ms += sample.sent_ms;
        if sample.skipped_silent {
            self.skipped_silent += 1;
            return;
        }
        match sample.words {
            Some(words) => {
                self.dictations += 1;
                self.words += words;
                for model in sample.models.iter().collect::<BTreeSet<_>>() {
                    *self.models.entry(model.clone()).or_default() += 1;
                }
                if !sample.failures.is_empty() {
                    self.fallbacks += 1;
                }
            }
            None => self.failed_dictations += 1,
        }
        self.latency_ms += sample.latency_ms;
        self.tokens += sample.tokens;
        self.cost_usd += sample.cost_usd;
        for (model, latency) in &sample.model_latency {
            self.model_latency
                .entry(model.clone())
                .or_default()
                .merge(latency);
        }
        for failure in &sample.failures {
            *self
                .errors
                .entry(failure.kind.key().into())
                .or_default()
                .entry(failure.model.clone())
                .or_default() += 1;
        }
    }

    fn merge(&mut self, other: &Self) {
        self.dictations += other.dictations;
        self.failed_dictations += other.failed_dictations;
        self.skipped_silent += other.skipped_silent;
        self.words += other.words;
        self.recorded_ms += other.recorded_ms;
        self.sent_ms += other.sent_ms;
        self.latency_ms += other.latency_ms;
        self.tokens += other.tokens;
        self.cost_usd += other.cost_usd;
        self.fallbacks += other.fallbacks;
        for (model, count) in &other.models {
            *self.models.entry(model.clone()).or_default() += count;
        }
        for (model, latency) in &other.model_latency {
            self.model_latency
                .entry(model.clone())
                .or_default()
                .merge(latency);
        }
        for (kind, models) in &other.errors {
            let target = self.errors.entry(kind.clone()).or_default();
            for (model, count) in models {
                *target.entry(model.clone()).or_default() += count;
            }
        }
    }

    pub fn average_latency_ms(&self) -> Option<u64> {
        let attempts = self.dictations + self.failed_dictations;
        (attempts > 0).then(|| self.latency_ms / attempts)
    }

    pub fn error_count(&self, kind: &str) -> u64 {
        self.errors
            .get(kind)
            .map_or(0, |models| models.values().sum())
    }

    /// Error kinds by descending count.
    pub fn errors_by_count(&self) -> Vec<(&str, u64)> {
        let mut errors: Vec<_> = self
            .errors
            .keys()
            .map(|kind| (kind.as_str(), self.error_count(kind)))
            .collect();
        errors.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
        errors
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct StatsFile {
    version: u32,
    /// Local calendar day (`YYYY-MM-DD`) to totals.
    days: BTreeMap<String, Totals>,
}

/// Which days a summary covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Period {
    Today,
    Week,
    Month,
    AllTime,
}

impl Period {
    pub const ALL: [Self; 4] = [Self::Today, Self::Week, Self::Month, Self::AllTime];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::AllTime => "All time",
        }
    }

    const fn days(self) -> Option<i64> {
        match self {
            Self::Today => Some(1),
            Self::Week => Some(7),
            Self::Month => Some(30),
            Self::AllTime => None,
        }
    }
}

/// Add one dictation to today's totals. Failures are logged, never raised:
/// statistics must not affect dictation.
#[cfg(not(test))]
pub fn record(sample: &Sample) {
    let result = (|| -> Result<()> {
        let path = crate::app_paths::support_dir()?.join(FILE);
        record_at(&path, sample, &local_day(now_seconds()))
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "could not record dictation statistics");
    }
}

/// Totals over `period`, ending today.
pub fn summary(period: Period) -> Result<Totals> {
    let path = crate::app_paths::support_dir()?.join(FILE);
    Ok(summary_at(&path, period, now_seconds()))
}

/// Words per local day over `period`, oldest first, including empty days.
/// All time is charted as the last 30 days.
pub fn daily_words(period: Period) -> Result<Vec<(String, u64)>> {
    let path = crate::app_paths::support_dir()?.join(FILE);
    Ok(daily_words_at(&path, period, now_seconds()))
}

pub fn clear() -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let path = crate::app_paths::support_dir()?.join(FILE);
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn record_at(path: &Path, sample: &Sample, day: &str) -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut file = load(path);
    file.days
        .entry(day.to_owned())
        .or_default()
        .add_sample(sample);
    while file.days.len() > MAX_DAYS {
        let oldest = file.days.keys().next().cloned();
        if let Some(oldest) = oldest {
            file.days.remove(&oldest);
        }
    }
    save(path, &file)
}

fn summary_at(path: &Path, period: Period, now: i64) -> Totals {
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let file = load(path);
    let first_day = period.days().map(|days| local_day_before(now, days - 1));
    let mut totals = Totals::default();
    for (day, day_totals) in &file.days {
        if first_day
            .as_deref()
            .is_none_or(|first| day.as_str() >= first)
        {
            totals.merge(day_totals);
        }
    }
    totals
}

fn daily_words_at(path: &Path, period: Period, now: i64) -> Vec<(String, u64)> {
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let file = load(path);
    let days = period.days().unwrap_or(30);
    (0..days)
        .rev()
        .map(|offset| {
            let day = local_day_before(now, offset);
            let words = file.days.get(&day).map_or(0, |totals| totals.words);
            (day, words)
        })
        .collect()
}

fn load(path: &Path) -> StatsFile {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<StatsFile>(&bytes) {
            Ok(mut file) if file.version == 1 || file.version == VERSION => {
                if file.version == 1 {
                    // Version 1 joined every model used by a dictation into one key.
                    // Keep the original dictation unit while separating those labels.
                    for totals in file.days.values_mut() {
                        let models = std::mem::take(&mut totals.models);
                        for (joined, count) in models {
                            for model in joined.split(", ").collect::<BTreeSet<_>>() {
                                *totals.models.entry(model.to_owned()).or_default() += count;
                            }
                        }
                    }
                    file.version = VERSION;
                }
                // Legacy error_examples are deliberately not deserialized. The next
                // save removes response bodies without discarding any daily totals.
                file
            }
            Ok(_) | Err(_) => {
                tracing::warn!(path = %path.display(), "unreadable statistics; starting over");
                let _ = fs::rename(path, path.with_extension("json.corrupt"));
                StatsFile::default()
            }
        },
        Err(_) => StatsFile::default(),
    }
}

fn save(path: &Path, file: &StatsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = StatsFile {
        version: VERSION,
        days: file.days.clone(),
    };
    let temporary = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut handle = options.open(&temporary)?;
    handle.write_all(&serde_json::to_vec(&file)?)?;
    handle.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// The local calendar day of a Unix time, as `YYYY-MM-DD`.
fn local_day(seconds: i64) -> String {
    local_day_before(seconds, 0)
}

/// Calendar arithmetic in the local timezone; a local day can have 23 or 25 hours.
fn local_day_before(seconds: i64, days: i64) -> String {
    let time = seconds as libc::time_t;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers reference valid stack values for the call.
    let converted = unsafe { !libc::localtime_r(&time, &mut parts).is_null() };
    if !converted {
        return format!("{}", seconds / 86_400 - days);
    }
    if days != 0 {
        parts.tm_mday -= days as i32;
        // Noon avoids clock changes around midnight. Ask libc to resolve DST for
        // the target date instead of retaining the offset of the current day.
        parts.tm_hour = 12;
        parts.tm_min = 0;
        parts.tm_sec = 0;
        parts.tm_isdst = -1;
        // SAFETY: mktime normalizes the valid, initialized local calendar fields.
        unsafe { libc::mktime(&mut parts) };
    }
    format!(
        "{:04}-{:02}-{:02}",
        parts.tm_year + 1900,
        parts.tm_mon + 1,
        parts.tm_mday
    )
}

/// Words in a transcript, as a person would count them.
pub fn word_count(text: &str) -> u64 {
    text.split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hex-stats-{name}-{}-{}",
            std::process::id(),
            now_seconds()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join(FILE)
    }

    fn success(words: u64, model: &str, failures: Vec<Failure>) -> Sample {
        Sample {
            words: Some(words),
            models: vec![model.into()],
            model_latency: BTreeMap::new(),
            recorded_ms: 10_000,
            sent_ms: 7_000,
            latency_ms: 800,
            tokens: 120,
            cost_usd: 0.001,
            failures,
            skipped_silent: false,
        }
    }

    fn failure(model: &str, kind: ErrorKind) -> Failure {
        Failure {
            model: model.into(),
            kind,
            detail: format!("{model} failed"),
        }
    }

    #[test]
    fn totals_count_words_fallbacks_and_errors() {
        let path = temp_path("record");
        record_at(&path, &success(10, "a", Vec::new()), "2026-10-01").unwrap();
        record_at(
            &path,
            &success(5, "b", vec![failure("a", ErrorKind::RateLimited)]),
            "2026-10-02",
        )
        .unwrap();
        record_at(
            &path,
            &Sample {
                failures: vec![
                    failure("a", ErrorKind::RateLimited),
                    failure("b", ErrorKind::Timeout),
                ],
                ..Sample::default()
            },
            "2026-10-02",
        )
        .unwrap();
        record_at(
            &path,
            &Sample {
                skipped_silent: true,
                ..Sample::default()
            },
            "2026-10-02",
        )
        .unwrap();

        let all = summary_at(&path, Period::AllTime, 0);
        assert_eq!(all.dictations, 2);
        assert_eq!(all.failed_dictations, 1);
        assert_eq!(all.skipped_silent, 1);
        assert_eq!(all.words, 15);
        assert_eq!(all.fallbacks, 1);
        assert_eq!(all.tokens, 240);
        assert_eq!(all.models["a"], 1);
        assert_eq!(all.models["b"], 1);
        assert_eq!(all.error_count("rate_limited"), 2);
        assert_eq!(all.errors["rate_limited"]["a"], 2);
        assert_eq!(all.errors_by_count()[0], ("rate_limited", 2));
        assert_eq!(all.average_latency_ms(), Some(1_600 / 3));
    }

    #[test]
    fn silent_batch_audio_counts_as_recorded_but_not_sent_or_failed() {
        let mut totals = Totals::default();
        totals.add_sample(&Sample {
            skipped_silent: true,
            recorded_ms: 10_000,
            ..Sample::default()
        });
        assert_eq!(totals.skipped_silent, 1);
        assert_eq!(totals.recorded_ms, 10_000);
        assert_eq!(totals.sent_ms, 0);
        assert_eq!(totals.dictations, 0);
        assert_eq!(totals.failed_dictations, 0);
        assert_eq!(totals.average_latency_ms(), None);
    }

    #[test]
    fn final_silence_check_retains_audio_already_sent_by_streaming() {
        let mut totals = Totals::default();
        totals.add_sample(&Sample {
            skipped_silent: true,
            recorded_ms: 10_000,
            sent_ms: 8_000,
            ..Sample::default()
        });
        assert_eq!(totals.sent_ms, 8_000);
        assert_eq!(totals.recorded_ms, 10_000);
        assert_eq!(totals.skipped_silent, 1);
        assert_eq!(totals.dictations, 0);
        assert_eq!(totals.failed_dictations, 0);
    }

    #[test]
    fn each_model_counts_once_per_successful_dictation() {
        let mut sample = success(10, "a", Vec::new());
        sample.models = vec!["a".into(), "b".into(), "a".into()];
        let mut totals = Totals::default();
        totals.add_sample(&sample);
        assert_eq!(totals.dictations, 1);
        assert_eq!(totals.models, [("a".into(), 1), ("b".into(), 1)].into());
    }

    #[test]
    fn model_latency_is_weighted_by_responses_and_filtered_by_period() {
        let path = temp_path("model-latency");
        let now = 1_791_100_000;
        let today = local_day(now);
        let yesterday = local_day_before(now, 1);
        let mut sample = success(10, "a", Vec::new());
        sample.models.push("b".into());
        sample
            .model_latency
            .entry("a".into())
            .or_default()
            .record(100);
        sample
            .model_latency
            .entry("a".into())
            .or_default()
            .record(300);
        sample
            .model_latency
            .entry("b".into())
            .or_default()
            .record(900);
        record_at(&path, &sample, &yesterday).unwrap();

        // An earlier chunk can succeed even when a later chunk fails the dictation.
        let mut partial = Sample::default();
        partial
            .model_latency
            .entry("a".into())
            .or_default()
            .record(1_600);
        record_at(&path, &partial, &today).unwrap();

        let all = summary_at(&path, Period::Week, now);
        assert_eq!(all.models["a"], 1);
        assert_eq!(all.model_latency["a"].responses, 3);
        assert_eq!(all.model_latency["a"].average_ms(), Some(2_000 / 3));
        assert_eq!(all.model_latency["b"].average_ms(), Some(900));
        let current = summary_at(&path, Period::Today, now);
        assert!(current.models.is_empty());
        assert_eq!(current.model_latency["a"].responses, 1);
        assert_eq!(current.model_latency["a"].average_ms(), Some(1_600));
        assert!(!current.model_latency.contains_key("b"));
    }

    #[test]
    fn old_statistics_do_not_invent_model_latency_measurements() {
        let path = temp_path("legacy-latency");
        for version in [1, VERSION] {
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "version": version,
                    "days": {"2026-10-04": {
                        "dictations": 100,
                        "latency_ms": 90_000,
                        "models": {"a": 100}
                    }}
                }))
                .unwrap(),
            )
            .unwrap();
            let old = summary_at(&path, Period::AllTime, 0);
            assert_eq!(old.models["a"], 100);
            assert!(old.model_latency.is_empty());
            assert_eq!(ModelLatency::default().average_ms(), None);

            let mut sample = success(1, "a", Vec::new());
            sample
                .model_latency
                .entry("a".into())
                .or_default()
                .record(250);
            record_at(&path, &sample, "2026-10-04").unwrap();
            let updated = summary_at(&path, Period::AllTime, 0);
            assert_eq!(updated.models["a"], 101);
            assert_eq!(updated.model_latency["a"].responses, 1);
            assert_eq!(updated.model_latency["a"].average_ms(), Some(250));
        }
    }

    #[test]
    fn legacy_totals_migrate_without_retaining_response_text() {
        let path = temp_path("legacy-privacy");
        let marker = "PRIVATE_TRANSCRIPT_MARKER";
        let legacy = serde_json::json!({
            "version": 1,
            "days": {
                "2026-10-04": {
                    "dictations": 3,
                    "words": 12,
                    "models": {"a": 1, "a, b": 2},
                    "errors": {"invalid_response": {"a": 1}},
                    "error_examples": {"invalid_response": marker}
                }
            }
        });
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let loaded = load(&path);
        let totals = &loaded.days["2026-10-04"];
        assert_eq!(loaded.version, VERSION);
        assert_eq!(totals.dictations, 3);
        assert_eq!(totals.words, 12);
        assert_eq!(totals.models, [("a".into(), 3), ("b".into(), 2)].into());
        assert_eq!(totals.error_count("invalid_response"), 1);
        assert!(!serde_json::to_string(&loaded).unwrap().contains(marker));

        record_at(
            &path,
            &Sample {
                failures: vec![Failure {
                    model: "b".into(),
                    kind: ErrorKind::InvalidResponse,
                    detail: format!("invalid JSON response: {{\"text\":\"{marker}"),
                }],
                ..Sample::default()
            },
            "2026-10-04",
        )
        .unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(!saved.contains(marker));
        assert!(!saved.contains("error_examples"));
        let loaded = load(&path);
        assert_eq!(loaded.days["2026-10-04"].dictations, 3);
        assert_eq!(loaded.days["2026-10-04"].failed_dictations, 1);
        assert_eq!(loaded.days["2026-10-04"].error_count("invalid_response"), 2);
    }

    #[test]
    fn periods_select_recent_local_days() {
        let path = temp_path("periods");
        let now = 1_791_100_000; // 2026-10-04
        let today = local_day(now);
        let five_days_ago = local_day(now - 5 * 86_400);
        let forty_days_ago = local_day(now - 40 * 86_400);
        for (day, words) in [(&today, 1), (&five_days_ago, 10), (&forty_days_ago, 100)] {
            record_at(&path, &success(words, "m", Vec::new()), day).unwrap();
        }
        assert_eq!(summary_at(&path, Period::Today, now).words, 1);
        assert_eq!(summary_at(&path, Period::Week, now).words, 11);
        assert_eq!(summary_at(&path, Period::Month, now).words, 11);
        assert_eq!(summary_at(&path, Period::AllTime, now).words, 111);
        let week = daily_words_at(&path, Period::Week, now);
        assert_eq!(week.len(), 7);
        assert_eq!(week.last().unwrap(), &(today.clone(), 1));
        assert_eq!(week[1], (five_days_ago.clone(), 10));
        assert_eq!(daily_words_at(&path, Period::AllTime, now).len(), 30);
        assert_eq!(daily_words_at(&path, Period::Today, now), [(today, 1)]);
    }

    #[test]
    fn periods_follow_calendar_days_across_dst() {
        const CHILD_ENV: &str = "HEX_STATS_DST_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            // Give this test its own timezone without changing the environment of
            // other tests running on concurrent threads.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("periods_follow_calendar_days_across_dst")
                .arg("--nocapture")
                .env(CHILD_ENV, "1")
                .env("TZ", "America/New_York")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        fn local_time(year: i32, month: i32, day: i32, hour: i32) -> i64 {
            // SAFETY: all fields are initialized before passing the local time to libc.
            let mut parts: libc::tm = unsafe { std::mem::zeroed() };
            parts.tm_year = year - 1900;
            parts.tm_mon = month - 1;
            parts.tm_mday = day;
            parts.tm_hour = hour;
            parts.tm_min = 30;
            parts.tm_isdst = -1;
            // SAFETY: parts points to an initialized, valid local calendar time.
            unsafe { libc::mktime(&mut parts) as i64 }
        }

        let now = local_time(2026, 11, 1, 23);
        let path = temp_path("dst");
        for day in [
            "2026-10-26",
            "2026-10-27",
            "2026-10-28",
            "2026-10-29",
            "2026-10-30",
            "2026-10-31",
            "2026-11-01",
        ] {
            record_at(&path, &success(1, "a", Vec::new()), day).unwrap();
        }
        let week = daily_words_at(&path, Period::Week, now);
        assert_eq!(week.first(), Some(&("2026-10-26".into(), 1)));
        assert_eq!(week.last(), Some(&("2026-11-01".into(), 1)));
        assert_eq!(week.iter().collect::<BTreeSet<_>>().len(), 7);
        assert_eq!(summary_at(&path, Period::Week, now).words, 7);
        assert_eq!(local_day_before(local_time(2026, 3, 9, 0), 1), "2026-03-08");
        assert_eq!(local_day_before(local_time(2024, 3, 1, 0), 1), "2024-02-29");
        assert_eq!(local_day_before(local_time(2026, 1, 1, 0), 1), "2025-12-31");
    }

    #[test]
    fn old_days_are_dropped_and_unreadable_files_are_preserved() {
        let path = temp_path("bounded");
        for day in 0..(MAX_DAYS + 3) {
            record_at(&path, &success(1, "m", Vec::new()), &format!("d{day:05}")).unwrap();
        }
        assert_eq!(load(&path).days.len(), MAX_DAYS);

        fs::write(&path, "{ nope").unwrap();
        assert_eq!(summary_at(&path, Period::AllTime, 0), Totals::default());
        assert!(path.with_extension("json.corrupt").exists());
    }

    #[test]
    fn errors_are_classified_for_display() {
        assert_eq!(ErrorKind::from_status(429), ErrorKind::RateLimited);
        assert_eq!(ErrorKind::from_status(503), ErrorKind::Server);
        assert_eq!(ErrorKind::from_status(401), ErrorKind::Auth);
        assert_eq!(ErrorKind::from_status(400), ErrorKind::Rejected);
        assert_eq!(
            ErrorKind::from_transport("network error (exit status: 28): Operation timed out"),
            ErrorKind::Timeout
        );
        assert_eq!(
            ErrorKind::from_transport("Could not resolve host"),
            ErrorKind::Network
        );
        assert_eq!(
            ErrorKind::label_for_key("rate_limited"),
            "Rate limited (HTTP 429)"
        );
    }

    #[test]
    fn words_ignore_punctuation_only_tokens() {
        assert_eq!(word_count("Olá, mundo — tudo bem?"), 4);
        assert_eq!(word_count("   "), 0);
    }
}
