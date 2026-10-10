//! Aggregate dictation statistics: words, audio, tokens, cost, fallbacks, and
//! the errors that caused them. Daily totals only, never text, so they are
//! kept regardless of the History retention setting.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;

use color_eyre::Result;
use serde::{Deserialize, Serialize};

const FILE: &str = "stats.json";
const VERSION: u32 = 3;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Daily buckets kept on disk; older saved buckets are dropped, including across gaps.
const MAX_DAYS: usize = 400;

static LOCK: Mutex<()> = Mutex::new(());

fn add_counter(total: &mut u64, count: u64) {
    *total = total.saturating_add(count);
}

/// Ignore invalid reported amounts and clamp an unrepresentable sum. Never
/// serialize infinity as null, which would make the next load lose the totals.
fn add_cost(total: &mut f64, cost: f64) -> bool {
    if !total.is_finite() || *total < 0.0 {
        *total = 0.0;
    }
    if !cost.is_finite() || cost < 0.0 {
        return false;
    }
    *total = (*total + cost).min(f64::MAX);
    true
}

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
        self.responses = self.responses.saturating_add(1);
        self.total_ms = self.total_ms.saturating_add(latency_ms);
    }

    #[cfg(test)]
    pub fn average_ms(&self) -> Option<u64> {
        self.total_ms.checked_div(self.responses)
    }

    pub fn merge(&mut self, other: &Self) {
        self.responses = self.responses.saturating_add(other.responses);
        self.total_ms = self.total_ms.saturating_add(other.total_ms);
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestMode {
    Live,
    #[default]
    Recorded,
}

/// Ephemeral measurements for one actual provider attempt. Never serialized.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AttemptSample {
    pub model: String,
    pub mode: RequestMode,
    pub success: bool,
    pub error: Option<ErrorKind>,
    pub latency_ms: Option<u64>,
    pub keyword_count: usize,
    pub cost_usd: Option<f64>,
    /// Published-price estimate for a successful request without a reported cost.
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DictationTelemetry {
    pub attempts: Vec<AttemptSample>,
    pub used_fallback: bool,
    pub retried: bool,
    pub live_recovered: bool,
}

// Stable bucket upper bounds in milliseconds; the final bucket is overflow.
const LATENCY_BOUNDS: [u64; 22] = [
    100, 200, 300, 400, 500, 750, 1_000, 1_500, 2_000, 2_500, 3_000, 4_000, 5_000, 7_500, 10_000,
    15_000, 20_000, 30_000, 45_000, 60_000, 90_000, 120_000,
];

/// Fixed-size distribution of successful measured requests, never raw samples.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct LatencyDistribution {
    pub count: u64,
    pub total_ms: u64,
    pub min_ms: Option<u64>,
    pub max_ms: Option<u64>,
    buckets: [u64; 23],
}

impl LatencyDistribution {
    pub fn record(&mut self, milliseconds: u64) {
        let bucket = LATENCY_BOUNDS.partition_point(|bound| *bound < milliseconds);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.total_ms = self.total_ms.saturating_add(milliseconds);
        self.min_ms = Some(
            self.min_ms
                .map_or(milliseconds, |old| old.min(milliseconds)),
        );
        self.max_ms = Some(
            self.max_ms
                .map_or(milliseconds, |old| old.max(milliseconds)),
        );
    }

    pub fn merge(&mut self, other: &Self) {
        self.count = self.count.saturating_add(other.count);
        self.total_ms = self.total_ms.saturating_add(other.total_ms);
        self.min_ms = match (self.min_ms, other.min_ms) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        self.max_ms = match (self.max_ms, other.max_ms) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        };
        for (target, source) in self.buckets.iter_mut().zip(other.buckets) {
            *target = target.saturating_add(source);
        }
    }

    pub fn average_ms(&self) -> Option<u64> {
        self.total_ms.checked_div(self.count)
    }

    /// Approximate nearest-rank percentile using bucket upper bounds. P95 and
    /// higher require at least twenty measured successes. Overflow uses max_ms.
    pub fn percentile_ms(&self, percent: u8) -> Option<u64> {
        if self.count == 0 || !(1..=100).contains(&percent) || (percent >= 95 && self.count < 20) {
            return None;
        }
        let rank = (u128::from(self.count) * u128::from(percent)).div_ceil(100);
        let mut cumulative = 0_u128;
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative += u128::from(*count);
            if cumulative >= rank {
                let maximum = self.max_ms?;
                return Some(
                    LATENCY_BOUNDS
                        .get(index)
                        .copied()
                        .unwrap_or(maximum)
                        .min(maximum),
                );
            }
        }
        None
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct RequestTotals {
    pub attempts: u64,
    pub successes: u64,
    pub errors: BTreeMap<String, u64>,
    pub latency: LatencyDistribution,
    pub keyword_requests: u64,
    pub keywords_sent: u64,
    pub reported_cost_usd: f64,
    pub cost_reports: u64,
    /// Estimates from published prices, kept apart from reported costs.
    pub estimated_cost_usd: f64,
    pub cost_estimates: u64,
}

impl RequestTotals {
    fn normalize_cost(&mut self) {
        if !self.reported_cost_usd.is_finite() || self.reported_cost_usd < 0.0 {
            self.reported_cost_usd = 0.0;
            self.cost_reports = 0;
        }
        if !self.estimated_cost_usd.is_finite() || self.estimated_cost_usd < 0.0 {
            self.estimated_cost_usd = 0.0;
            self.cost_estimates = 0;
        }
    }

    fn add_attempt(&mut self, attempt: &AttemptSample) {
        self.normalize_cost();
        self.attempts = self.attempts.saturating_add(1);
        if attempt.success {
            self.successes = self.successes.saturating_add(1);
            if let Some(milliseconds) = attempt.latency_ms {
                self.latency.record(milliseconds);
            }
        } else {
            add_counter(
                self.errors
                    .entry(attempt.error.map_or("unknown", ErrorKind::key).into())
                    .or_default(),
                1,
            );
        }
        if attempt.keyword_count > 0 {
            self.keyword_requests = self.keyword_requests.saturating_add(1);
            self.keywords_sent = self
                .keywords_sent
                .saturating_add(attempt.keyword_count as u64);
        }
        if let Some(cost) = attempt
            .cost_usd
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
        {
            add_cost(&mut self.reported_cost_usd, cost);
            self.cost_reports = self.cost_reports.saturating_add(1);
        } else if let Some(cost) = attempt
            .estimated_cost_usd
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
        {
            add_cost(&mut self.estimated_cost_usd, cost);
            self.cost_estimates = self.cost_estimates.saturating_add(1);
        }
    }

    pub fn merge(&mut self, other: &Self) {
        self.attempts = self.attempts.saturating_add(other.attempts);
        self.successes = self.successes.saturating_add(other.successes);
        self.latency.merge(&other.latency);
        self.keyword_requests = self.keyword_requests.saturating_add(other.keyword_requests);
        self.keywords_sent = self.keywords_sent.saturating_add(other.keywords_sent);
        self.normalize_cost();
        if other.cost_reports > 0 && add_cost(&mut self.reported_cost_usd, other.reported_cost_usd)
        {
            self.cost_reports = self.cost_reports.saturating_add(other.cost_reports);
        }
        if other.cost_estimates > 0
            && add_cost(&mut self.estimated_cost_usd, other.estimated_cost_usd)
        {
            self.cost_estimates = self.cost_estimates.saturating_add(other.cost_estimates);
        }
        for (kind, count) in &other.errors {
            add_counter(self.errors.entry(kind.clone()).or_default(), *count);
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelRequests {
    pub live: RequestTotals,
    pub recorded: RequestTotals,
}

impl ModelRequests {
    pub fn merge(&mut self, other: &Self) {
        self.live.merge(&other.live);
        self.recorded.merge(&other.recorded);
    }

    pub fn combined(&self) -> RequestTotals {
        let mut combined = self.live.clone();
        combined.merge(&self.recorded);
        combined
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct DetailedTotals {
    pub dictations: u64,
    pub successful_dictations: u64,
    pub fallback_dictations: u64,
    pub retried_dictations: u64,
    pub live_recoveries: u64,
    pub requests: BTreeMap<String, ModelRequests>,
}

impl DetailedTotals {
    fn add_dictation(&mut self, telemetry: &DictationTelemetry, success: bool) {
        self.dictations = self.dictations.saturating_add(1);
        self.successful_dictations = self
            .successful_dictations
            .saturating_add(u64::from(success));
        self.fallback_dictations = self
            .fallback_dictations
            .saturating_add(u64::from(success && telemetry.used_fallback));
        self.retried_dictations = self
            .retried_dictations
            .saturating_add(u64::from(telemetry.retried));
        self.live_recoveries = self
            .live_recoveries
            .saturating_add(u64::from(success && telemetry.live_recovered));
        for attempt in &telemetry.attempts {
            let requests = self.requests.entry(attempt.model.clone()).or_default();
            match attempt.mode {
                RequestMode::Live => &mut requests.live,
                RequestMode::Recorded => &mut requests.recorded,
            }
            .add_attempt(attempt);
        }
    }

    pub fn merge(&mut self, other: &Self) {
        self.dictations = self.dictations.saturating_add(other.dictations);
        self.successful_dictations = self
            .successful_dictations
            .saturating_add(other.successful_dictations);
        self.fallback_dictations = self
            .fallback_dictations
            .saturating_add(other.fallback_dictations);
        self.retried_dictations = self
            .retried_dictations
            .saturating_add(other.retried_dictations);
        self.live_recoveries = self.live_recoveries.saturating_add(other.live_recoveries);
        for (model, requests) in &other.requests {
            self.requests
                .entry(model.clone())
                .or_default()
                .merge(requests);
        }
    }
}

/// What one dictation contributed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    pub telemetry: Option<DictationTelemetry>,
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
    /// A live session that sent audio but ended without any dictation, for
    /// example cancelled while recording. It is not a dictation.
    pub discarded: Option<DiscardedStream>,
}

/// Audio streamed to a provider that no dictation accounts for.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DiscardedStream {
    pub sent_ms: u64,
    /// Published-price estimate; `None` when the model has no known price.
    pub estimated_cost_usd: Option<f64>,
}

/// Live sessions that were cancelled, discarded or interrupted after sending
/// audio. Absent (zero) for days recorded before this existed.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct DiscardedTotals {
    pub sessions: u64,
    pub sent_ms: u64,
    /// Sessions with a list-price estimate; the others have unknown cost.
    pub estimated_sessions: u64,
    pub estimated_cost_usd: f64,
}

impl DiscardedTotals {
    fn add(&mut self, stream: &DiscardedStream) {
        self.sessions = self.sessions.saturating_add(1);
        self.sent_ms = self.sent_ms.saturating_add(stream.sent_ms);
        if let Some(cost) = stream.estimated_cost_usd
            && add_cost(&mut self.estimated_cost_usd, cost)
        {
            self.estimated_sessions = self.estimated_sessions.saturating_add(1);
        }
    }

    fn merge(&mut self, other: &Self) {
        self.sessions = self.sessions.saturating_add(other.sessions);
        self.sent_ms = self.sent_ms.saturating_add(other.sent_ms);
        if add_cost(&mut self.estimated_cost_usd, other.estimated_cost_usd) {
            self.estimated_sessions = self
                .estimated_sessions
                .saturating_add(other.estimated_sessions);
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Totals {
    pub details: DetailedTotals,
    pub dictations: u64,
    pub failed_dictations: u64,
    pub skipped_silent: u64,
    pub words: u64,
    pub recorded_ms: u64,
    pub sent_ms: u64,
    pub latency_ms: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    /// Published-price estimates for successful requests without a reported
    /// cost; absent (zero) for days recorded before estimates existed.
    pub estimated_cost_usd: f64,
    /// Dictations that needed at least one fallback.
    pub fallbacks: u64,
    /// Successful dictations using each model; one dictation can use several.
    pub models: BTreeMap<String, u64>,
    /// Successful requests, not dictations; absent for data collected before 3.0.1.
    pub model_latency: BTreeMap<String, ModelLatency>,
    /// Failed attempts per error kind, then per model.
    pub errors: BTreeMap<String, BTreeMap<String, u64>>,
    pub discarded: DiscardedTotals,
}

impl Totals {
    pub(crate) fn add_sample(&mut self, sample: &Sample) {
        if let Some(stream) = &sample.discarded {
            self.discarded.add(stream);
            return;
        }
        self.recorded_ms = self.recorded_ms.saturating_add(sample.recorded_ms);
        self.sent_ms = self.sent_ms.saturating_add(sample.sent_ms);
        if sample.skipped_silent {
            self.skipped_silent = self.skipped_silent.saturating_add(1);
            return;
        }
        if let Some(telemetry) = &sample.telemetry {
            self.details
                .add_dictation(telemetry, sample.words.is_some());
            for attempt in &telemetry.attempts {
                if attempt.cost_usd.is_none()
                    && let Some(cost) = attempt.estimated_cost_usd
                {
                    add_cost(&mut self.estimated_cost_usd, cost);
                }
            }
        }
        match sample.words {
            Some(words) => {
                self.dictations = self.dictations.saturating_add(1);
                self.words = self.words.saturating_add(words);
                for model in sample.models.iter().collect::<BTreeSet<_>>() {
                    add_counter(self.models.entry(model.clone()).or_default(), 1);
                }
                if sample
                    .telemetry
                    .as_ref()
                    .map_or(!sample.failures.is_empty(), |telemetry| {
                        telemetry.used_fallback
                    })
                {
                    self.fallbacks = self.fallbacks.saturating_add(1);
                }
            }
            None => self.failed_dictations = self.failed_dictations.saturating_add(1),
        }
        self.latency_ms = self.latency_ms.saturating_add(sample.latency_ms);
        self.tokens = self.tokens.saturating_add(sample.tokens);
        add_cost(&mut self.cost_usd, sample.cost_usd);
        for (model, latency) in &sample.model_latency {
            self.model_latency
                .entry(model.clone())
                .or_default()
                .merge(latency);
        }
        for failure in &sample.failures {
            add_counter(
                self.errors
                    .entry(failure.kind.key().into())
                    .or_default()
                    .entry(failure.model.clone())
                    .or_default(),
                1,
            );
        }
    }

    pub fn merge(&mut self, other: &Self) {
        self.details.merge(&other.details);
        self.discarded.merge(&other.discarded);
        self.dictations = self.dictations.saturating_add(other.dictations);
        self.failed_dictations = self
            .failed_dictations
            .saturating_add(other.failed_dictations);
        self.skipped_silent = self.skipped_silent.saturating_add(other.skipped_silent);
        self.words = self.words.saturating_add(other.words);
        self.recorded_ms = self.recorded_ms.saturating_add(other.recorded_ms);
        self.sent_ms = self.sent_ms.saturating_add(other.sent_ms);
        self.latency_ms = self.latency_ms.saturating_add(other.latency_ms);
        self.tokens = self.tokens.saturating_add(other.tokens);
        add_cost(&mut self.cost_usd, other.cost_usd);
        add_cost(&mut self.estimated_cost_usd, other.estimated_cost_usd);
        self.fallbacks = self.fallbacks.saturating_add(other.fallbacks);
        for (model, count) in &other.models {
            add_counter(self.models.entry(model.clone()).or_default(), *count);
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
                add_counter(target.entry(model.clone()).or_default(), *count);
            }
        }
    }

    pub fn average_latency_ms(&self) -> Option<u64> {
        let attempts = self.dictations.saturating_add(self.failed_dictations);
        (attempts > 0).then(|| self.latency_ms / attempts)
    }

    #[cfg(test)]
    pub fn error_count(&self, kind: &str) -> u64 {
        self.errors.get(kind).map_or(0, |models| {
            models
                .values()
                .fold(0_u64, |total, count| total.saturating_add(*count))
        })
    }

    /// Error kinds by descending count.
    #[cfg(test)]
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

    pub fn label(self) -> &'static str {
        crate::i18n::t(match self {
            Self::Today => "Today",
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::AllTime => "All saved",
        })
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dashboard {
    pub totals: Totals,
    pub previous: Option<Totals>,
    /// One entry per local day, including gaps. AllTime charts only the last 30 days.
    pub daily: Vec<(String, Totals)>,
}

pub fn dashboard(period: Period) -> Result<Dashboard> {
    let path = crate::app_paths::support_dir()?.join(FILE);
    dashboard_at(&path, period, now_seconds())
}

/// Read one consistent snapshot without consulting the real application path.
pub fn dashboard_at(path: &Path, period: Period, now: i64) -> Result<Dashboard> {
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let file = load(path)?;
    let today = local_day(now);
    let first = period.days().map(|days| local_day_before(now, days - 1));
    let sum = |first: Option<&str>, last: &str| {
        let mut totals = Totals::default();
        for (day, day_totals) in &file.days {
            if day.as_str() <= last && first.is_none_or(|first| day.as_str() >= first) {
                totals.merge(day_totals);
            }
        }
        totals
    };
    Ok(Dashboard {
        totals: sum(first.as_deref(), &today),
        previous: period.days().map(|days| {
            sum(
                Some(&local_day_before(now, days * 2 - 1)),
                &local_day_before(now, days),
            )
        }),
        daily: (0..period.days().unwrap_or(30))
            .rev()
            .map(|offset| {
                let day = local_day_before(now, offset);
                let totals = file.days.get(&day).cloned().unwrap_or_default();
                (day, totals)
            })
            .collect(),
    })
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
    let mut file = load(path)?;
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

#[cfg(test)]
fn summary_at(path: &Path, period: Period, now: i64) -> Totals {
    dashboard_at(path, period, now).unwrap().totals
}

#[cfg(test)]
fn daily_words_at(path: &Path, period: Period, now: i64) -> Vec<(String, u64)> {
    dashboard_at(path, period, now)
        .unwrap()
        .daily
        .into_iter()
        .map(|(day, totals)| (day, totals.words))
        .collect()
}

fn load(path: &Path) -> Result<StatsFile> {
    let handle = match fs::File::open(path) {
        Ok(handle) => handle,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StatsFile::default());
        }
        Err(error) => return Err(error.into()),
    };
    let bytes = read_bounded(handle, MAX_FILE_BYTES)?;
    match serde_json::from_slice::<StatsFile>(&bytes) {
        Ok(mut file) if matches!(file.version, 1 | 2 | VERSION) => {
            if file.version == 1 {
                // Version 1 joined every model used by a dictation into one key.
                // Keep the original dictation unit while separating those labels.
                for totals in file.days.values_mut() {
                    let models = std::mem::take(&mut totals.models);
                    for (joined, count) in models {
                        for model in joined.split(", ").collect::<BTreeSet<_>>() {
                            add_counter(totals.models.entry(model.to_owned()).or_default(), count);
                        }
                    }
                }
            }
            file.version = VERSION;
            // Legacy error_examples are deliberately not deserialized. The next
            // save removes response bodies without discarding any daily totals.
            Ok(file)
        }
        Ok(_) => Err(color_eyre::eyre::eyre!(
            "Statistics use an unsupported version. The existing file was preserved."
        )),
        Err(_) => Err(color_eyre::eyre::eyre!(
            "Statistics contain invalid data. The existing file was preserved."
        )),
    }
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        color_eyre::eyre::bail!("Statistics file exceeds the size limit.");
    }
    Ok(bytes)
}

fn save(path: &Path, file: &StatsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = StatsFile {
        version: VERSION,
        days: file.days.clone(),
    };
    let bytes = serde_json::to_vec(&file)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        color_eyre::eyre::bail!(
            "Statistics file exceeds the size limit. The existing file was preserved."
        );
    }
    let temporary = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut handle = options.open(&temporary)?;
    handle.write_all(&bytes)?;
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

    fn local_noon(year: i32, month: i32, day: i32) -> i64 {
        // SAFETY: zero initializes every tm field before normalization by libc.
        let mut parts: libc::tm = unsafe { std::mem::zeroed() };
        parts.tm_year = year - 1900;
        parts.tm_mon = month - 1;
        parts.tm_mday = day;
        parts.tm_hour = 12;
        parts.tm_isdst = -1;
        // SAFETY: parts is a valid writable local calendar structure.
        unsafe { libc::mktime(&mut parts) as i64 }
    }

    fn attempt(
        model: &str,
        mode: RequestMode,
        latency: Option<u64>,
        cost: Option<f64>,
    ) -> AttemptSample {
        AttemptSample {
            model: model.into(),
            mode,
            success: true,
            latency_ms: latency,
            cost_usd: cost,
            ..Default::default()
        }
    }

    #[test]
    fn telemetry_separates_models_modes_retries_and_actual_fallbacks() {
        let mut sample = success(
            12,
            "openai::shared",
            vec![failure("openai::shared", ErrorKind::RateLimited)],
        );
        sample.telemetry = Some(DictationTelemetry {
            retried: true,
            attempts: vec![
                AttemptSample {
                    model: "openai::shared".into(),
                    mode: RequestMode::Live,
                    error: Some(ErrorKind::RateLimited),
                    latency_ms: Some(900),
                    keyword_count: 3,
                    ..Default::default()
                },
                attempt(
                    "openai::shared",
                    RequestMode::Recorded,
                    Some(200),
                    Some(0.0),
                ),
                attempt("openai/shared", RequestMode::Recorded, None, None),
            ],
            ..Default::default()
        });
        let mut totals = Totals::default();
        totals.add_sample(&sample);
        assert_eq!(
            totals.fallbacks, 0,
            "retrying the same model is not fallback"
        );
        assert_eq!(totals.error_count("rate_limited"), 1);
        assert_eq!(totals.details.dictations, 1);
        assert_eq!(totals.details.successful_dictations, 1);
        assert_eq!(totals.details.retried_dictations, 1);
        let requests = &totals.details.requests["openai::shared"];
        assert_eq!(requests.live.attempts, 1);
        assert_eq!(requests.live.successes, 0);
        assert_eq!(requests.live.errors["rate_limited"], 1);
        assert_eq!(
            requests.live.latency.count, 0,
            "failed latency cannot dilute success percentiles"
        );
        assert_eq!(requests.live.keyword_requests, 1);
        assert_eq!(requests.live.keywords_sent, 3);
        assert_eq!(requests.recorded.successes, 1);
        assert_eq!(requests.recorded.latency.average_ms(), Some(200));
        assert_eq!(requests.recorded.cost_reports, 1);
        assert_eq!(requests.recorded.reported_cost_usd, 0.0);
        assert_eq!(requests.combined().attempts, 2);
        assert_eq!(
            totals.details.requests["openai/shared"]
                .recorded
                .latency
                .count,
            0
        );
        assert_eq!(
            totals.details.requests["openai/shared"]
                .recorded
                .cost_reports,
            0
        );

        let telemetry = sample.telemetry.as_mut().unwrap();
        telemetry.used_fallback = true;
        telemetry.live_recovered = true;
        sample.words = None;
        totals.add_sample(&sample);
        assert_eq!(totals.details.dictations, 2);
        assert_eq!(totals.details.successful_dictations, 1);
        assert_eq!(totals.details.fallback_dictations, 0);
        assert_eq!(totals.details.live_recoveries, 0);
        sample.words = Some(3);
        totals.add_sample(&sample);
        assert_eq!(totals.fallbacks, 1);
        assert_eq!(totals.details.fallback_dictations, 1);
        assert_eq!(totals.details.live_recoveries, 1);
        sample.skipped_silent = true;
        totals.add_sample(&sample);
        assert_eq!(totals.details.dictations, 3);
    }

    #[test]
    fn histogram_is_fixed_weighted_and_withholds_small_sample_p95() {
        let mut slow = LatencyDistribution::default();
        for _ in 0..19 {
            slow.record(900);
        }
        assert_eq!(slow.percentile_ms(50), Some(900));
        assert_eq!(slow.percentile_ms(95), None);
        let mut fast = LatencyDistribution::default();
        fast.record(100);
        slow.merge(&fast);
        assert_eq!(slow.count, 20);
        assert_eq!(slow.average_ms(), Some(860));
        assert_eq!(slow.percentile_ms(95), Some(900));
        assert_eq!(slow.min_ms, Some(100));
        assert_eq!(slow.max_ms, Some(900));
        assert_eq!(slow.percentile_ms(0), None);
        assert_eq!(slow.percentile_ms(101), None);
        slow.record(200_000);
        assert_eq!(slow.percentile_ms(100), Some(200_000));
        let encoded = serde_json::to_value(&slow).unwrap();
        assert_eq!(encoded["buckets"].as_array().unwrap().len(), 23);
        let decoded: LatencyDistribution = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, slow);
        let mut empty = LatencyDistribution::default();
        empty.merge(&slow);
        assert_eq!(empty, slow);
    }

    #[test]
    fn detailed_merge_weights_latency_and_keeps_cost_coverage_distinct() {
        let mut totals = Totals::default();
        let mut daily = Totals::default();
        for (target, timings) in [(&mut totals, vec![100, 300]), (&mut daily, vec![1_400])] {
            let attempts = timings
                .into_iter()
                .map(|time| {
                    attempt(
                        "google::transcribe",
                        RequestMode::Recorded,
                        Some(time),
                        Some(0.25),
                    )
                })
                .collect();
            target.add_sample(&Sample {
                words: Some(1),
                telemetry: Some(DictationTelemetry {
                    attempts,
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        daily.add_sample(&Sample {
            telemetry: Some(DictationTelemetry {
                attempts: vec![
                    attempt("google::transcribe", RequestMode::Live, Some(9), None),
                    attempt(
                        "google::transcribe",
                        RequestMode::Live,
                        None,
                        Some(f64::NAN),
                    ),
                    attempt("google::transcribe", RequestMode::Live, None, Some(-1.0)),
                ],
                ..Default::default()
            }),
            ..Default::default()
        });
        totals.merge(&daily);
        assert_eq!(totals.details.dictations, 3);
        assert_eq!(totals.details.successful_dictations, 2);
        let requests = &totals.details.requests["google::transcribe"];
        assert_eq!(requests.recorded.latency.average_ms(), Some(600));
        assert_eq!(requests.recorded.cost_reports, 3);
        assert_eq!(requests.recorded.reported_cost_usd, 0.75);
        assert_eq!(requests.live.cost_reports, 0);
        assert_eq!(requests.combined().attempts, 6);
        assert_eq!(requests.combined().latency.count, 4);
    }

    #[test]
    fn counters_saturate_and_invalid_costs_do_not_corrupt_coverage_or_json() {
        let mut requests = RequestTotals {
            attempts: u64::MAX,
            successes: u64::MAX,
            keyword_requests: u64::MAX,
            keywords_sent: u64::MAX,
            reported_cost_usd: f64::MAX,
            cost_reports: u64::MAX,
            errors: BTreeMap::from([("network".into(), u64::MAX)]),
            ..Default::default()
        };
        requests.merge(&requests.clone());
        assert_eq!(requests.attempts, u64::MAX);
        assert_eq!(requests.errors["network"], u64::MAX);
        assert_eq!(requests.cost_reports, u64::MAX);
        assert_eq!(requests.reported_cost_usd, f64::MAX);
        let mut totals = Totals {
            dictations: u64::MAX,
            failed_dictations: u64::MAX,
            words: u64::MAX,
            tokens: u64::MAX,
            cost_usd: f64::MAX,
            models: BTreeMap::from([("fixture".into(), u64::MAX)]),
            details: DetailedTotals {
                dictations: u64::MAX,
                requests: BTreeMap::from([(
                    "fixture".into(),
                    ModelRequests {
                        recorded: requests,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
            ..Default::default()
        };
        totals.merge(&totals.clone());
        let mut sample = success(1, "fixture", vec![]);
        sample.cost_usd = f64::INFINITY;
        sample.telemetry = Some(DictationTelemetry {
            attempts: vec![attempt(
                "fixture",
                RequestMode::Recorded,
                Some(u64::MAX),
                Some(f64::NAN),
            )],
            ..Default::default()
        });
        totals.add_sample(&sample);
        assert_eq!(totals.dictations, u64::MAX);
        assert_eq!(totals.models["fixture"], u64::MAX);
        assert_eq!(totals.details.dictations, u64::MAX);
        assert!(totals.cost_usd.is_finite());
        assert_eq!(totals.average_latency_ms(), Some(0));
        let json = serde_json::to_vec(&totals).unwrap();
        assert!(serde_json::from_slice::<Totals>(&json).is_ok());
        let mut clean = RequestTotals::default();
        clean.merge(&RequestTotals {
            reported_cost_usd: f64::INFINITY,
            cost_reports: 10,
            ..Default::default()
        });
        assert_eq!(clean.cost_reports, 0);
        assert_eq!(clean.reported_cost_usd, 0.0);
        clean.add_attempt(&attempt("fixture", RequestMode::Recorded, None, Some(0.0)));
        assert_eq!(clean.cost_reports, 1);
    }

    #[test]
    fn dashboard_compares_equivalent_calendar_periods_and_excludes_future_days() {
        let path = temp_path("dashboard-periods");
        let now = local_noon(2024, 3, 1);
        for (offset, words) in [
            (0, 1),
            (1, 2),
            (6, 4),
            (7, 8),
            (13, 16),
            (29, 32),
            (30, 64),
            (59, 128),
            (-1, 1000),
        ] {
            record_at(
                &path,
                &success(words, "fixture", vec![]),
                &local_day_before(now, offset),
            )
            .unwrap();
        }
        for (period, current, previous) in [
            (Period::Today, 1, 2),
            (Period::Week, 7, 24),
            (Period::Month, 63, 192),
        ] {
            let view = dashboard_at(&path, period, now).unwrap();
            assert_eq!(view.totals.words, current);
            assert_eq!(view.previous.unwrap().words, previous);
            assert_eq!(view.daily.len(), period.days().unwrap() as usize);
            assert_eq!(view.daily.last().unwrap().0, "2024-03-01");
        }
        let week = dashboard_at(&path, Period::Week, now).unwrap();
        assert_eq!(week.daily[4], (local_day_before(now, 2), Totals::default()));
        assert_eq!(week.daily[5].0, "2024-02-29");
        let all = dashboard_at(&path, Period::AllTime, now).unwrap();
        assert_eq!(all.totals.words, 255);
        assert!(all.previous.is_none());
        assert_eq!(all.daily.len(), 30);
        assert_eq!(all.daily.iter().map(|(_, day)| day.words).sum::<u64>(), 63);
    }

    #[test]
    fn detailed_persistence_retains_aggregates_but_never_attempt_payloads() {
        let path = temp_path("detailed-privacy");
        let marker = "PRIVATE_PROVIDER_PAYLOAD";
        let mut sample = success(
            3,
            "grok::voice",
            vec![Failure {
                model: "grok::voice".into(),
                kind: ErrorKind::Network,
                detail: marker.into(),
            }],
        );
        sample.telemetry = Some(DictationTelemetry {
            attempts: vec![AttemptSample {
                model: "grok::voice".into(),
                error: Some(ErrorKind::Network),
                ..Default::default()
            }],
            ..Default::default()
        });
        record_at(&path, &sample, "2026-10-08").unwrap();
        let encoded = fs::read_to_string(&path).unwrap();
        assert!(!encoded.contains(marker));
        assert!(!encoded.contains("telemetry"));
        let file = load(&path).unwrap();
        assert_eq!(file.version, VERSION);
        assert_eq!(
            file.days["2026-10-08"].details.requests["grok::voice"]
                .recorded
                .attempts,
            1
        );
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert!(value["days"]["2026-10-08"]["details"]["requests"]["grok::voice"]["recorded"]["attempts"].is_number());
        assert_eq!(
            read_bounded(std::io::Cursor::new(b"1234"), 4).unwrap(),
            b"1234"
        );
        assert!(read_bounded(std::io::Cursor::new(b"12345"), 4).is_err());
    }

    #[test]
    fn dashboard_and_record_surface_io_errors_without_replacing_existing_data() {
        let path = temp_path("read-error");
        fs::create_dir(&path).unwrap();
        assert!(dashboard_at(&path, Period::Today, local_noon(2026, 10, 8)).is_err());
        assert!(record_at(&path, &Sample::default(), "2026-10-08").is_err());
        assert!(path.is_dir());
        assert!(!path.with_extension("json.corrupt").exists());
    }

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
            telemetry: None,
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
            discarded: None,
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

        let all = summary_at(&path, Period::AllTime, local_noon(2026, 10, 4));
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
        for version in [1, 2, VERSION] {
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
            let old = summary_at(&path, Period::AllTime, local_noon(2026, 10, 4));
            assert_eq!(old.models["a"], 100);
            assert!(old.model_latency.is_empty());
            assert_eq!(old.details, DetailedTotals::default());
            assert_eq!(ModelLatency::default().average_ms(), None);

            let mut sample = success(1, "a", Vec::new());
            sample
                .model_latency
                .entry("a".into())
                .or_default()
                .record(250);
            record_at(&path, &sample, "2026-10-04").unwrap();
            let updated = summary_at(&path, Period::AllTime, local_noon(2026, 10, 4));
            assert_eq!(updated.models["a"], 101);
            assert_eq!(updated.model_latency["a"].responses, 1);
            assert_eq!(updated.model_latency["a"].average_ms(), Some(250));
        }
    }

    #[test]
    fn version_two_migration_keeps_legacy_coverage_separate_from_new_requests() {
        let path = temp_path("version-two-details");
        fs::write(&path, br#"{"version":2,"days":{"2026-10-08":{"dictations":100,"failed_dictations":10,"words":500,"models":{"openai/model":100}}}}"#).unwrap();
        let mut sample = success(3, "openai::model", vec![]);
        sample.telemetry = Some(DictationTelemetry {
            attempts: vec![attempt(
                "openai::model",
                RequestMode::Recorded,
                Some(500),
                None,
            )],
            ..Default::default()
        });
        record_at(&path, &sample, "2026-10-08").unwrap();
        let file = load(&path).unwrap();
        assert_eq!(file.version, VERSION);
        let totals = &file.days["2026-10-08"];
        assert_eq!(totals.dictations, 101);
        assert_eq!(totals.failed_dictations, 10);
        assert_eq!(totals.words, 503);
        assert_eq!(totals.models["openai/model"], 100);
        assert_eq!(totals.details.dictations, 1);
        assert_eq!(totals.details.requests.len(), 1);
        assert!(!totals.details.requests.contains_key("openai/model"));
        assert_eq!(
            totals.details.requests["openai::model"]
                .recorded
                .latency
                .count,
            1
        );
    }

    #[test]
    fn unsupported_versions_and_invalid_json_are_read_only_errors() {
        let path = temp_path("unsupported-version");
        let marker = "PRIVATE_OLD_PAYLOAD";
        for bytes in [
            format!("{{invalid {marker}"),
            serde_json::json!({"version":VERSION+1,"days":{},"future":marker}).to_string(),
        ] {
            fs::write(&path, &bytes).unwrap();
            let error = dashboard_at(&path, Period::Today, local_noon(2026, 10, 8)).unwrap_err();
            assert!(!error.to_string().contains(marker));
            assert!(record_at(&path, &Sample::default(), "2026-10-08").is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
            assert!(!path.with_extension("json.corrupt").exists());
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

        let loaded = load(&path).unwrap();
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
        let loaded = load(&path).unwrap();
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
        assert_eq!(load(&path).unwrap().days.len(), MAX_DAYS);

        fs::write(&path, "{ nope").unwrap();
        let backup = path.with_extension("json.corrupt");
        fs::write(&backup, "earlier backup").unwrap();
        assert!(dashboard_at(&path, Period::AllTime, local_noon(2026, 10, 8)).is_err());
        assert!(record_at(&path, &Sample::default(), "2026-10-08").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ nope");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "earlier backup");
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

    #[test]
    fn discarded_streams_are_separate_from_dictations_and_keep_unknown_costs() {
        let mut day = Totals::default();
        day.add_sample(&Sample {
            discarded: Some(DiscardedStream {
                sent_ms: 4_000,
                estimated_cost_usd: Some(0.001),
            }),
            ..Sample::default()
        });
        day.add_sample(&Sample {
            discarded: Some(DiscardedStream {
                sent_ms: 2_000,
                estimated_cost_usd: None,
            }),
            ..Sample::default()
        });
        assert_eq!(
            day.discarded,
            DiscardedTotals {
                sessions: 2,
                sent_ms: 6_000,
                estimated_sessions: 1,
                estimated_cost_usd: 0.001,
            }
        );
        assert_eq!(
            day.dictations + day.failed_dictations + day.skipped_silent,
            0
        );
        assert_eq!((day.recorded_ms, day.sent_ms), (0, 0));
        assert_eq!((day.cost_usd, day.estimated_cost_usd), (0.0, 0.0));

        let mut period = Totals::default();
        period.merge(&day);
        period.merge(&day);
        assert_eq!(period.discarded.sessions, 4);
        assert_eq!(period.discarded.estimated_sessions, 2);
        assert!((period.discarded.estimated_cost_usd - 0.002).abs() < 1e-12);

        // Days saved before this existed load as nothing discarded.
        let old: Totals = serde_json::from_str(r#"{"dictations":3}"#).unwrap();
        assert_eq!(old.discarded, DiscardedTotals::default());
        let saved: Totals = serde_json::from_str(&serde_json::to_string(&period).unwrap()).unwrap();
        assert_eq!(saved.discarded, period.discarded);
    }

    #[test]
    fn discarded_streams_are_recorded_into_the_day_file() {
        let path = temp_path("discarded");
        let sample = Sample {
            discarded: Some(DiscardedStream {
                sent_ms: 1_500,
                estimated_cost_usd: Some(0.0005),
            }),
            ..Sample::default()
        };
        record_at(&path, &sample, "2026-10-10").unwrap();
        let totals = &load(&path).unwrap().days["2026-10-10"];
        assert_eq!(totals.discarded.sessions, 1);
        assert_eq!(totals.discarded.sent_ms, 1_500);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
