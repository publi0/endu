//! What the OpenRouter transcription did for one dictation, kept in History.

use serde::{Deserialize, Serialize};

use crate::i18n::t;

/// Longest model label stored; model ids are short, this bounds bad config.
const MAX_MODEL_CHARS: usize = 120;
/// Longest per-request safe error summary stored in History/Recovery.
const MAX_ERROR_CHARS: usize = 240;
const MAX_FAILED_MODELS: usize = 8;
const MAX_EXECUTIONS: usize = 64;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct StepReport {
    /// Actual requests; absent in History saved by earlier versions. Never terms or prompts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executions: Vec<ExecutionReport>,
    /// Keep cost coverage honest when the persistence limit removes attempts.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_executions: usize,
    /// The model that answered; several when long audio was split into chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub latency_ms: u64,
    /// Models that failed before the one that answered, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// How much audio was recorded and how much was sent after trimming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioTrim>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExecutionReport {
    pub provider: String,
    pub model: String,
    pub streaming: bool,
    pub keyword_count: usize,
    pub outcome: String,
    /// Safe, bounded summary of why a failed request did not succeed.
    /// Never stores provider payloads, URLs, headers or credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Actual USD cost returned by this request, never inferred from a rate card.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_cost"
    )]
    pub cost_usd: Option<f64>,
    /// Hex's estimate from the provider's published price, only for a
    /// successful request whose provider reported no cost. Never mixed into
    /// `cost_usd` and never added to entries recorded before estimates existed.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_cost"
    )]
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioTrim {
    pub recorded_ms: u64,
    pub sent_ms: u64,
}

impl StepReport {
    /// The same report with every label bounded, for persistence.
    pub fn bounded(mut self) -> Self {
        self.omitted_executions = self
            .omitted_executions
            .saturating_add(self.executions.len().saturating_sub(MAX_EXECUTIONS));
        self.executions.truncate(MAX_EXECUTIONS);
        for execution in &mut self.executions {
            execution.provider = bound(&execution.provider);
            execution.model = bound(&execution.model);
            execution.outcome = execution.outcome.chars().take(80).collect();
            execution.error = execution.error.take().and_then(|error| {
                let clean: String = error
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(MAX_ERROR_CHARS)
                    .collect();
                let trimmed = clean.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_owned())
            });
            execution.keyword_count = execution.keyword_count.min(2000);
            execution.cost_usd = valid_cost(execution.cost_usd);
            execution.estimated_cost_usd = valid_cost(execution.estimated_cost_usd);
        }
        self.model = self.model.take().map(|model| bound(&model));
        self.failed.truncate(MAX_FAILED_MODELS);
        self.failed = self.failed.iter().map(|model| bound(model)).collect();
        self
    }

    /// Sum reported costs, then estimates for requests that reported none,
    /// marking estimates and incomplete coverage explicitly.
    #[cfg(test)]
    pub fn cost_summary(&self) -> String {
        match self.cost() {
            Err(message) => message.into(),
            Ok(cost) => {
                let amount = if cost.estimated {
                    tf!("≈ {cost} (estimated)", cost = format_cost(cost.sum))
                } else {
                    format_cost(cost.sum)
                };
                if cost.counted == cost.total {
                    amount
                } else {
                    tf!(
                        "{amount} · partial ({count} of {total} attempts)",
                        amount = amount,
                        count = cost.counted,
                        total = cost.total
                    )
                }
            }
        }
    }

    /// The History cost tile: a short amount (its label names the currency)
    /// and what qualifies it, so the estimate mark never truncates away.
    pub fn cost_tile(&self) -> (String, Option<String>) {
        match self.cost() {
            Err(message) => ("—".into(), Some(message.into())),
            Ok(cost) => {
                let amount = crate::i18n::decimal(format_usd(cost.sum));
                let partial = (cost.counted != cost.total).then(|| {
                    tf!(
                        "{count} of {total} attempts",
                        count = cost.counted,
                        total = cost.total
                    )
                });
                if cost.estimated {
                    let estimated = t("Estimated from list prices").to_owned();
                    (
                        format!("≈ {amount}"),
                        Some(match partial {
                            Some(partial) => format!("{estimated} · {partial}"),
                            None => estimated,
                        }),
                    )
                } else {
                    (
                        amount,
                        partial.map(|partial| tf!("Partial: {partial}", partial = partial)),
                    )
                }
            }
        }
    }

    fn cost(&self) -> Result<CostSum, &'static str> {
        let mut estimated = false;
        let costs: Vec<_> = self
            .executions
            .iter()
            .filter_map(|execution| {
                valid_cost(execution.cost_usd).or_else(|| {
                    let estimate = valid_cost(execution.estimated_cost_usd);
                    estimated |= estimate.is_some();
                    estimate
                })
            })
            .collect();
        let total = self
            .executions
            .len()
            .saturating_add(self.omitted_executions);
        if total == 0 {
            return Err(t("Not recorded"));
        }
        if costs.is_empty() {
            return Err(t("Not reported"));
        }
        let sum: f64 = costs.iter().sum();
        if !sum.is_finite() {
            return Err(t("Exceeds display range; see individual attempts"));
        }
        Ok(CostSum {
            sum,
            estimated,
            counted: costs.len(),
            total,
        })
    }

    /// Every request in order, labelled by why it was sent, for the History detail view.
    pub fn attempts(&self) -> Vec<AttemptView> {
        let show_cost = self.executions.len() > 1 || self.omitted_executions > 0;
        let views: Vec<AttemptView> = if self.executions.is_empty() {
            // Older History kept only the answering model and the failures before it.
            self.legacy_requests()
                .map(|request| {
                    let model = crate::providers::ModelRef::parse(request.model);
                    AttemptView {
                        provider: model.provider.label().into(),
                        model: model_name(model.provider.id(), model.model),
                        streaming: None,
                        keyword_count: None,
                        succeeded: request.succeeded,
                        error: None,
                        step: AttemptStep::First,
                        cost: None,
                    }
                })
                .collect()
        } else {
            self.executions
                .iter()
                .map(|execution| AttemptView {
                    provider: provider_label(&execution.provider),
                    model: model_name(&execution.provider, &execution.model),
                    streaming: Some(execution.streaming),
                    keyword_count: Some(execution.keyword_count),
                    succeeded: execution.outcome == "success",
                    error: execution.error.clone(),
                    step: AttemptStep::First,
                    cost: show_cost.then(|| {
                        valid_cost(execution.cost_usd).map_or_else(
                            || {
                                valid_cost(execution.estimated_cost_usd).map_or_else(
                                    || t("Cost not reported").into(),
                                    |cost| tf!("≈ {cost} (estimated)", cost = format_cost(cost)),
                                )
                            },
                            format_cost,
                        )
                    }),
                })
                .collect()
        };
        views
            .into_iter()
            .zip(self.steps())
            .map(|(view, step)| AttemptView { step, ..view })
            .collect()
    }

    /// One word for the History list when the answer needed more than one request.
    /// Runs for every visible row on every frame, so it borrows instead of building views.
    pub fn recovery_badge(&self) -> Option<&'static str> {
        let mut retried = false;
        for step in self.steps() {
            match step {
                AttemptStep::Fallback => return Some("Fallback"),
                AttemptStep::Retry | AttemptStep::Recovery => retried = true,
                AttemptStep::First | AttemptStep::Continued => {}
            }
        }
        retried.then_some("Retried")
    }

    /// Why each request was sent, judged against the one before it.
    fn steps(&self) -> impl Iterator<Item = AttemptStep> + '_ {
        let requests: Box<dyn Iterator<Item = Request<'_>>> = if self.executions.is_empty() {
            Box::new(self.legacy_requests())
        } else {
            Box::new(self.executions.iter().map(|execution| Request {
                provider: &execution.provider,
                model: &execution.model,
                streaming: Some(execution.streaming),
                succeeded: execution.outcome == "success",
            }))
        };
        let mut previous: Option<Request<'_>> = None;
        requests.map(move |current| {
            let step = match previous {
                None => AttemptStep::First,
                Some(previous) if previous.succeeded => AttemptStep::Continued,
                Some(previous)
                    if (previous.provider, previous.model) != (current.provider, current.model) =>
                {
                    AttemptStep::Fallback
                }
                Some(previous)
                    if previous.streaming == Some(true) && current.streaming == Some(false) =>
                {
                    AttemptStep::Recovery
                }
                Some(_) => AttemptStep::Retry,
            };
            previous = Some(current);
            step
        })
    }

    /// Legacy model ids already carry their provider prefix.
    fn legacy_requests(&self) -> impl Iterator<Item = Request<'_>> {
        let request = |model, succeeded| Request {
            provider: "",
            model,
            streaming: None,
            succeeded,
        };
        self.failed
            .iter()
            .map(move |model| request(model.as_str(), false))
            .chain(self.model.as_deref().map(move |model| request(model, true)))
    }

    /// `(value, detail)` for the audio summary, mentioning trimming only when it happened.
    pub fn audio_summary(&self) -> Option<(String, Option<String>)> {
        self.audio.map(|audio| {
            let trimmed = (audio.sent_ms < audio.recorded_ms).then(|| {
                tf!(
                    "of {seconds} recorded",
                    seconds = seconds(audio.recorded_ms)
                )
            });
            (seconds(audio.sent_ms), trimmed)
        })
    }

    /// Short model name(s) for a History list row. Uses the answering model
    /// when available, otherwise the attempted models.
    pub fn row_model_label(&self) -> Option<String> {
        if let Some(model) = self.model.as_deref() {
            let labels = short_model_labels(model.split(',').map(str::trim));
            if !labels.is_empty() {
                return Some(labels.join(", "));
            }
        }
        if !self.failed.is_empty() {
            let labels = short_model_labels(self.failed.iter().map(String::as_str));
            if !labels.is_empty() {
                return Some(labels.join(", "));
            }
        }
        let labels = short_model_labels(self.executions.iter().map(|e| e.model.as_str()));
        (!labels.is_empty()).then(|| labels.join(", "))
    }

    /// Human-readable model name(s) for a History detail view, matching the
    /// names shown in [`Self::attempts`].
    pub fn detail_model_label(&self) -> Option<String> {
        let mut names = Vec::new();
        if let Some(model) = self.model.as_deref() {
            for part in model.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let parsed = crate::providers::ModelRef::parse(part);
                push_unique(&mut names, model_name(parsed.provider.id(), parsed.model));
            }
        }
        if names.is_empty() {
            for part in &self.failed {
                let part = part.trim();
                if !part.is_empty() {
                    let parsed = crate::providers::ModelRef::parse(part);
                    push_unique(&mut names, model_name(parsed.provider.id(), parsed.model));
                }
            }
        }
        if names.is_empty() {
            for execution in &self.executions {
                push_unique(
                    &mut names,
                    model_name(&execution.provider, &execution.model),
                );
            }
        }
        (!names.is_empty()).then(|| names.join(", "))
    }

    /// Whether at least one failed request in this report carries its own
    /// per-request error reason.
    pub fn has_failed_attempt_reasons(&self) -> bool {
        self.executions
            .iter()
            .any(|execution| execution.outcome != "success" && execution.error.is_some())
    }

    /// Whether any model or provider recorded in this report matches `needle`
    /// (already lowercased).
    pub fn matches_model(&self, needle: &str) -> bool {
        let contains = |value: &str| value.to_lowercase().contains(needle);
        self.model.as_deref().is_some_and(contains)
            || self.failed.iter().any(|model| contains(model))
            || self
                .executions
                .iter()
                .any(|execution| contains(&execution.model) || contains(&execution.provider))
            || self.detail_model_label().as_deref().is_some_and(contains)
    }
}

fn push_unique(names: &mut Vec<String>, name: String) {
    if !name.is_empty() && !names.iter().any(|existing| existing == &name) {
        names.push(name);
    }
}

fn short_model_labels<'a>(models: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut labels = Vec::new();
    for raw in models {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let parsed = crate::providers::ModelRef::parse(raw).model;
        let short = parsed.rsplit('/').next().unwrap_or(parsed).trim();
        if !short.is_empty() && !labels.iter().any(|existing| existing == short) {
            labels.push(short.to_owned());
        }
    }
    labels
}

#[derive(Clone, Copy)]
struct Request<'a> {
    provider: &'a str,
    model: &'a str,
    streaming: Option<bool>,
    succeeded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptStep {
    /// The first request for this dictation.
    First,
    /// The same model again after it failed.
    Retry,
    /// The recorded clip after the live session on the same model failed.
    Recovery,
    /// The next model in the chain after the previous one failed.
    Fallback,
    /// A following chunk after a successful one.
    Continued,
}

impl AttemptStep {
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::First | Self::Continued => None,
            Self::Retry => Some("Retry"),
            Self::Recovery => Some("Recorded retry"),
            Self::Fallback => Some("Fallback"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct CostSum {
    sum: f64,
    estimated: bool,
    counted: usize,
    total: usize,
}

pub struct AttemptView {
    pub provider: String,
    pub model: String,
    /// Unknown in History saved before requests were recorded individually.
    pub streaming: Option<bool>,
    pub keyword_count: Option<usize>,
    pub succeeded: bool,
    /// Safe summary of why this request failed, when known.
    pub error: Option<String>,
    pub step: AttemptStep,
    /// Shown per request only when the dictation made several.
    pub cost: Option<String>,
}

impl AttemptView {
    /// Secondary line: provider, transport and keyword use.
    pub fn details(&self) -> String {
        let mut parts = vec![self.provider.clone()];
        match self.streaming {
            Some(true) => parts.push("Live streaming".into()),
            Some(false) => parts.push(t("After recording").into()),
            None => {}
        }
        match self.keyword_count {
            Some(0) | None => {}
            Some(1) => parts.push("1 keyword".into()),
            Some(count) => parts.push(format!("{count} keywords")),
        }
        parts.join(" · ")
    }
}

/// Short human duration: milliseconds below a second, seconds above.
pub fn duration_label(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms} ms"),
        1_000..=9_999 => crate::i18n::decimal(format!("{:.2} s", ms as f64 / 1_000.0)),
        _ => crate::i18n::decimal(format!("{:.1} s", ms as f64 / 1_000.0)),
    }
}

/// A native model reads with its own name; routes and unknown models keep their ID.
fn model_name(provider: &str, model: &str) -> String {
    crate::providers::native_models()
        .iter()
        .find(|native| native.provider.id() == provider && native.id == model)
        .map_or_else(|| model.to_owned(), |native| native.name.to_owned())
}

fn provider_label(id: &str) -> String {
    if id == "microsoft" {
        // Older History entries name the removed native Microsoft provider.
        return "Microsoft (removed)".into();
    }
    crate::providers::Provider::ALL
        .into_iter()
        .find(|provider| provider.id() == id)
        .map_or_else(|| id.to_owned(), |provider| provider.label().to_owned())
}

/// Representative report for tests.
#[cfg(test)]
pub fn preview() -> StepReport {
    StepReport {
        model: Some("openai/gpt-4o-mini-transcribe".into()),
        latency_ms: 820,
        failed: vec!["openai/whisper-large-v3-turbo".into()],
        executions: Vec::new(),
        omitted_executions: 0,
        audio: Some(AudioTrim {
            recorded_ms: 9_400,
            sent_ms: 6_100,
        }),
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn valid_cost(cost: Option<f64>) -> Option<f64> {
    cost.filter(|value| value.is_finite() && *value >= 0.0)
}

fn deserialize_cost<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    // A malformed optional cost must not discard the user's retained transcript.
    Ok(valid_cost(value.as_f64()))
}

fn format_cost(cost: f64) -> String {
    crate::i18n::decimal(format!("{} USD", format_usd(cost)))
}

/// A dollar amount that never rounds a positive cost to zero. Shared with Statistics.
pub fn format_usd(cost: f64) -> String {
    if cost == 0.0 {
        return "$0.00".into();
    }
    if !(0.000_000_000_001..1_000_000_000.0).contains(&cost) {
        return format!("${cost:.6e}");
    }
    let precision = if cost < 0.000_001 { 12 } else { 6 };
    let mut amount = format!("{cost:.precision$}");
    while amount.ends_with('0') && amount.len() - amount.find('.').unwrap_or(0) > 3 {
        amount.pop();
    }
    format!("${amount}")
}

pub fn seconds(ms: u64) -> String {
    crate::i18n::decimal(format!("{:.1} s", ms as f64 / 1_000.0))
}

fn bound(model: &str) -> String {
    model.chars().take(MAX_MODEL_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_records_actual_features_without_keyword_contents() {
        let report = StepReport {
            executions: vec![
                ExecutionReport {
                    provider: "Deepgram".into(),
                    model: "nova-3".into(),
                    streaming: true,
                    keyword_count: 2,
                    outcome: "failed".into(),
                    error: None,
                    cost_usd: None,
                    estimated_cost_usd: None,
                },
                ExecutionReport {
                    provider: "OpenAI".into(),
                    model: "gpt-transcribe".into(),
                    streaming: false,
                    keyword_count: 2,
                    outcome: "success".into(),
                    error: None,
                    cost_usd: Some(0.000_123),
                    estimated_cost_usd: None,
                },
            ],
            ..StepReport::default()
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("terms"));
        let loaded: StepReport = serde_json::from_str(&json).unwrap();
        assert_eq!(
            loaded.cost_summary(),
            "$0.000123 USD · partial (1 of 2 attempts)"
        );
        let attempts = loaded.attempts();
        assert_eq!(
            attempts[0].details(),
            "Deepgram · Live streaming · 2 keywords"
        );
        assert!(!attempts[0].succeeded);
        assert_eq!(attempts[0].cost.as_deref(), Some("Cost not reported"));
        assert_eq!(
            attempts[1].details(),
            "OpenAI · After recording · 2 keywords"
        );
        assert_eq!(attempts[1].step, AttemptStep::Fallback);
        assert_eq!(attempts[1].cost.as_deref(), Some("$0.000123 USD"));
        assert_eq!(loaded.recovery_badge(), Some("Fallback"));
        assert_eq!(
            serde_json::from_str::<StepReport>(r#"{"latency_ms":123}"#)
                .unwrap()
                .executions,
            Vec::new()
        );
    }

    #[test]
    fn legacy_reports_list_fallbacks_and_trimming_without_inventing_modes() {
        let report = preview();
        let attempts = report.attempts();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].model, "openai/whisper-large-v3-turbo");
        assert_eq!(attempts[0].details(), "OpenRouter");
        assert!(!attempts[0].succeeded);
        assert_eq!(attempts[1].step, AttemptStep::Fallback);
        assert!(attempts[1].succeeded);
        assert_eq!(attempts[1].cost, None);
        assert_eq!(
            report.audio_summary(),
            Some(("6.1 s".to_owned(), Some("of 9.4 s recorded".to_owned())))
        );
    }

    #[test]
    fn untrimmed_audio_and_no_fallback_read_plainly() {
        let report = StepReport {
            model: Some("a/b".into()),
            latency_ms: 500,
            audio: Some(AudioTrim {
                recorded_ms: 3_000,
                sent_ms: 3_000,
            }),
            ..StepReport::default()
        };
        assert_eq!(report.audio_summary(), Some(("3.0 s".to_owned(), None)));
        assert_eq!(report.attempts().len(), 1);
        assert_eq!(report.recovery_badge(), None);
    }

    fn execution(model: &str, streaming: bool, outcome: &str) -> ExecutionReport {
        ExecutionReport {
            provider: "deepgram".into(),
            model: model.into(),
            streaming,
            outcome: outcome.into(),
            ..ExecutionReport::default()
        }
    }

    #[test]
    fn attempts_distinguish_retry_recorded_retry_fallback_and_chunks() {
        let report = StepReport {
            executions: vec![
                execution("nova-3", true, "failed"),
                execution("nova-3", false, "failed"),
                execution("nova-3", false, "failed"),
                execution("nova-2", false, "success"),
                execution("nova-2", false, "success"),
            ],
            ..StepReport::default()
        };
        let steps: Vec<_> = report.attempts().iter().map(|a| a.step).collect();
        assert_eq!(
            steps,
            [
                AttemptStep::First,
                AttemptStep::Recovery,
                AttemptStep::Retry,
                AttemptStep::Fallback,
                AttemptStep::Continued,
            ]
        );
        assert_eq!(report.attempts()[0].provider, "Deepgram");
        let retried = StepReport {
            executions: vec![
                execution("nova-3", false, "failed"),
                execution("nova-3", false, "success"),
            ],
            ..StepReport::default()
        };
        assert_eq!(retried.recovery_badge(), Some("Retried"));
        let chunks = StepReport {
            executions: vec![
                execution("nova-3", false, "success"),
                execution("nova-3", false, "success"),
            ],
            ..StepReport::default()
        };
        assert_eq!(chunks.recovery_badge(), None);
    }

    #[test]
    fn durations_read_in_the_natural_unit() {
        assert_eq!(duration_label(820), "820 ms");
        assert_eq!(duration_label(1_300), "1.30 s");
        assert_eq!(duration_label(18_400), "18.4 s");
    }

    #[test]
    fn serialization_skips_empty_fields() {
        let json = serde_json::to_string(&StepReport {
            model: Some("m".into()),
            latency_ms: 1,
            ..StepReport::default()
        })
        .unwrap();
        assert_eq!(json, r#"{"model":"m","latency_ms":1}"#);
    }

    #[test]
    fn bounded_caps_labels() {
        let long = "m".repeat(500);
        let report = StepReport {
            model: Some(long.clone()),
            latency_ms: 1,
            failed: vec![long; 20],
            audio: None,
            executions: Vec::new(),
            omitted_executions: 0,
        }
        .bounded();
        assert_eq!(report.model.unwrap().len(), MAX_MODEL_CHARS);
        assert_eq!(report.failed.len(), MAX_FAILED_MODELS);
        assert!(
            report
                .failed
                .iter()
                .all(|model| model.len() == MAX_MODEL_CHARS)
        );
    }

    fn costs(values: &[Option<f64>]) -> StepReport {
        StepReport {
            executions: values
                .iter()
                .map(|cost_usd| ExecutionReport {
                    cost_usd: *cost_usd,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn estimates_fill_unreported_costs_and_are_always_labelled() {
        let mut report = costs(&[None, None, Some(0.002)]);
        report.executions[1].estimated_cost_usd = Some(0.003);
        assert_eq!(
            report.cost_summary(),
            "≈ $0.005 USD (estimated) · partial (2 of 3 attempts)"
        );
        // A reported amount always wins over an estimate for the same request.
        report.executions[2].estimated_cost_usd = Some(9.0);
        assert!(report.cost_summary().starts_with("≈ $0.005 USD"));
        let attempts = report.attempts();
        assert_eq!(attempts[0].cost.as_deref(), Some("Cost not reported"));
        assert_eq!(
            attempts[1].cost.as_deref(),
            Some("≈ $0.003 USD (estimated)")
        );
        assert_eq!(attempts[2].cost.as_deref(), Some("$0.002 USD"));
        let restored: StepReport =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(restored.executions[1].estimated_cost_usd, Some(0.003));
        let older: ExecutionReport = serde_json::from_str(
            r#"{"provider":"openai","model":"gpt-transcribe","streaming":false,"keyword_count":0,"outcome":"success"}"#,
        )
        .unwrap();
        assert_eq!(
            older.estimated_cost_usd, None,
            "old entries are never estimated"
        );
    }

    #[test]
    fn history_cost_tile_keeps_the_amount_short_and_qualifies_it_below() {
        // The estimate mark used to sit after the amount and was truncated away.
        let mut live = costs(&[None]);
        live.executions[0].estimated_cost_usd = Some(0.000_675);
        assert_eq!(
            live.cost_tile(),
            (
                "≈ $0.000675".into(),
                Some("Estimated from list prices".into())
            )
        );
        let mut mixed = costs(&[None, None, Some(0.002)]);
        mixed.executions[1].estimated_cost_usd = Some(0.003);
        assert_eq!(
            mixed.cost_tile(),
            (
                "≈ $0.005".into(),
                Some("Estimated from list prices · 2 of 3 attempts".into())
            )
        );
        assert_eq!(
            costs(&[Some(0.0), None]).cost_tile(),
            ("$0.00".into(), Some("Partial: 1 of 2 attempts".into()))
        );
        assert_eq!(costs(&[Some(0.002)]).cost_tile(), ("$0.002".into(), None));
        assert_eq!(
            costs(&[None]).cost_tile(),
            ("—".into(), Some("Not reported".into()))
        );
        assert_eq!(
            costs(&[]).cost_tile(),
            ("—".into(), Some("Not recorded".into()))
        );
    }

    #[test]
    fn attempts_name_native_models_and_keep_route_ids() {
        let report = StepReport {
            executions: vec![
                ExecutionReport {
                    provider: "elevenlabs".into(),
                    model: "scribe_v2_realtime".into(),
                    ..Default::default()
                },
                ExecutionReport {
                    provider: "openrouter".into(),
                    model: "microsoft/mai-transcribe-2".into(),
                    ..Default::default()
                },
                ExecutionReport {
                    provider: "elevenlabs".into(),
                    model: "retired-model".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let models: Vec<_> = report
            .attempts()
            .into_iter()
            .map(|attempt| attempt.model)
            .collect();
        assert_eq!(
            models,
            [
                "Scribe v2 Realtime",
                "microsoft/mai-transcribe-2",
                "retired-model"
            ]
        );
    }

    #[test]
    fn costs_distinguish_unknown_zero_partial_and_complete_coverage() {
        assert_eq!(costs(&[]).cost_summary(), "Not recorded");
        assert_eq!(costs(&[None]).cost_summary(), "Not reported");
        assert_eq!(costs(&[Some(0.0)]).cost_summary(), "$0.00 USD");
        assert_eq!(costs(&[Some(-0.0)]).cost_summary(), "$0.00 USD");
        assert_eq!(
            costs(&[Some(0.0), None]).cost_summary(),
            "$0.00 USD · partial (1 of 2 attempts)"
        );
        assert_eq!(
            costs(&[Some(0.002), Some(0.003)]).cost_summary(),
            "$0.005 USD"
        );
        let invalid = costs(&[Some(f64::NAN), Some(f64::INFINITY), Some(-1.0)]).bounded();
        assert!(
            invalid
                .executions
                .iter()
                .all(|request| request.cost_usd.is_none())
        );
        assert_eq!(invalid.cost_summary(), "Not reported");
    }

    #[test]
    fn small_costs_never_round_to_zero_and_extreme_costs_stay_bounded() {
        assert_eq!(format_cost(0.000_001), "$0.000001 USD");
        assert_eq!(format_cost(0.000_000_12), "$0.00000012 USD");
        assert_ne!(format_cost(f64::MIN_POSITIVE), "$0.00 USD");
        assert!(format_cost(f64::MAX).len() < 30);
        assert_eq!(
            costs(&[Some(f64::MAX), Some(f64::MAX)]).cost_summary(),
            "Exceeds display range; see individual attempts"
        );
    }

    #[test]
    fn truncated_request_costs_remain_partial_after_repeated_bounds_and_roundtrip() {
        let bounded = costs(&[Some(0.001); MAX_EXECUTIONS + 1])
            .bounded()
            .bounded();
        assert_eq!(bounded.omitted_executions, 1);
        assert_eq!(bounded.executions.len(), MAX_EXECUTIONS);
        let restored: StepReport =
            serde_json::from_str(&serde_json::to_string(&bounded).unwrap()).unwrap();
        assert_eq!(restored, bounded);
        assert_eq!(
            restored.cost_summary(),
            "$0.064 USD · partial (64 of 65 attempts)"
        );
    }

    #[test]
    fn optional_malformed_or_legacy_cost_never_drops_the_report() {
        for cost in ["null", "-1", "\"unknown\"", "true", "{}", "[]"] {
            let json = format!(
                r#"{{"provider":"openrouter","model":"fixture/model","streaming":false,"keyword_count":0,"outcome":"success","cost_usd":{cost}}}"#
            );
            let loaded: ExecutionReport = serde_json::from_str(&json).unwrap();
            assert_eq!(loaded.cost_usd, None);
        }
        let legacy: ExecutionReport = serde_json::from_str(r#"{"provider":"openrouter","model":"fixture/model","streaming":false,"keyword_count":0,"outcome":"success"}"#).unwrap();
        assert_eq!(legacy.cost_usd, None);
        let zero = costs(&[Some(0.0)]);
        let json = serde_json::to_string(&zero).unwrap();
        assert!(json.contains("\"cost_usd\":0.0"));
        assert_eq!(serde_json::from_str::<StepReport>(&json).unwrap(), zero);
    }

    #[test]
    fn model_labels_cover_succeeded_and_failed_reports() {
        let native = StepReport {
            model: Some("deepgram::nova-3".into()),
            ..StepReport::default()
        };
        assert_eq!(native.row_model_label().as_deref(), Some("nova-3"));
        assert_eq!(native.detail_model_label().as_deref(), Some("Nova-3"));
        assert!(native.matches_model("nova-3"));
        assert!(native.matches_model("deepgram"));

        let failed_only = StepReport {
            failed: vec![
                "google::gemini-3.5-transcribe-live".into(),
                "openai/whisper-large-v3-turbo".into(),
                "google::gemini-3.5-transcribe-live".into(),
            ],
            ..StepReport::default()
        };
        assert_eq!(
            failed_only.row_model_label().as_deref(),
            Some("gemini-3.5-transcribe-live, whisper-large-v3-turbo")
        );
        assert_eq!(
            failed_only.detail_model_label().as_deref(),
            Some("Gemini 3.5 Transcribe Live, openai/whisper-large-v3-turbo")
        );
        assert!(failed_only.matches_model("gemini 3.5 transcribe live"));
        assert!(failed_only.matches_model("whisper-large-v3-turbo"));
        assert!(!failed_only.matches_model("deepgram"));

        assert_eq!(StepReport::default().row_model_label(), None);
        assert_eq!(StepReport::default().detail_model_label(), None);
    }

    #[test]
    fn per_request_failure_reasons_roundtrip_bound_and_expose_on_attempts() {
        let noisy = format!("  HTTP 429\n\t{}  ", "x".repeat(400));
        let report = StepReport {
            executions: vec![
                ExecutionReport {
                    provider: "google".into(),
                    model: "gemini-3.5-transcribe-live".into(),
                    streaming: true,
                    outcome: "failed".into(),
                    error: Some(noisy),
                    ..Default::default()
                },
                ExecutionReport {
                    provider: "openrouter".into(),
                    model: "openai/whisper-large-v3-turbo".into(),
                    streaming: false,
                    outcome: "success".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
        .bounded();
        assert!(report.has_failed_attempt_reasons());
        let attempts = report.attempts();
        let error = attempts[0].error.as_deref().unwrap();
        assert!(error.starts_with("HTTP 429"));
        assert!(!error.chars().any(char::is_control));
        assert!(error.chars().count() <= MAX_ERROR_CHARS);
        assert_eq!(attempts[1].error, None);

        let restored: StepReport =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(restored, report);
    }
}
