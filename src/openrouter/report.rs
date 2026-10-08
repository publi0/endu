//! What the OpenRouter transcription did for one dictation, kept in History.

use serde::{Deserialize, Serialize};

/// Longest model label stored; model ids are short, this bounds bad config.
const MAX_MODEL_CHARS: usize = 120;
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
    /// Actual USD cost returned by this request, never inferred from a rate card.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_cost"
    )]
    pub cost_usd: Option<f64>,
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
            execution.keyword_count = execution.keyword_count.min(2000);
            execution.cost_usd = valid_cost(execution.cost_usd);
        }
        self.model = self.model.take().map(|model| bound(&model));
        self.failed.truncate(MAX_FAILED_MODELS);
        self.failed = self.failed.iter().map(|model| bound(model)).collect();
        self
    }

    /// Sum only reported costs, identifying incomplete coverage explicitly.
    pub fn cost_summary(&self) -> String {
        let costs: Vec<_> = self
            .executions
            .iter()
            .filter_map(|execution| valid_cost(execution.cost_usd))
            .collect();
        let total = self
            .executions
            .len()
            .saturating_add(self.omitted_executions);
        if total == 0 {
            return "Not recorded".into();
        }
        if costs.is_empty() {
            return "Not reported".into();
        }
        let sum: f64 = costs.iter().sum();
        if !sum.is_finite() {
            return "Exceeds display range; see individual attempts".into();
        }
        let amount = format_cost(sum);
        if costs.len() == total {
            amount
        } else {
            format!("{amount} · partial ({} of {total} attempts)", costs.len())
        }
    }

    /// `(label, value)` rows for the History detail view.
    pub fn history_rows(&self) -> Vec<(&'static str, String)> {
        let mut rows = vec![
            ("Reported cost", self.cost_summary()),
            (
                "Model",
                format!(
                    "{} · {} ms",
                    self.model.as_deref().unwrap_or("unknown"),
                    self.latency_ms
                ),
            ),
        ];
        for execution in &self.executions {
            let mode = if execution.streaming {
                "Live streaming"
            } else {
                "After recording"
            };
            let cost = if self.executions.len() > 1 || self.omitted_executions > 0 {
                format!(
                    " · {}",
                    valid_cost(execution.cost_usd)
                        .map(format_cost)
                        .unwrap_or_else(|| "Cost not reported".into())
                )
            } else {
                String::new()
            };
            let keywords = if execution.keyword_count == 0 {
                "No keywords sent".to_owned()
            } else {
                format!("{} keywords sent", execution.keyword_count)
            };
            rows.push((
                "Attempt",
                format!(
                    "{} · {} · {mode} · {keywords} · {}{cost}",
                    execution.provider, execution.model, execution.outcome
                ),
            ));
        }
        if self.omitted_executions > 0 {
            rows.push(("Attempts omitted", self.omitted_executions.to_string()));
        }
        if !self.failed.is_empty() {
            rows.push(("Fell back from", self.failed.join(", ")));
        }
        if let Some(audio) = self.audio {
            let mut value = format!("{} sent", seconds(audio.sent_ms));
            if audio.sent_ms < audio.recorded_ms {
                value.push_str(&format!(
                    " of {} recorded (silence trimmed)",
                    seconds(audio.recorded_ms)
                ));
            }
            rows.push(("Audio", value));
        }
        rows
    }
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
    if cost == 0.0 {
        return "$0.00 USD".into();
    }
    if !(0.000_000_000_001..1_000_000_000.0).contains(&cost) {
        return format!("${cost:.6e} USD");
    }
    let precision = if cost < 0.000_001 { 12 } else { 6 };
    let mut amount = format!("{cost:.precision$}");
    while amount.ends_with('0') && amount.len() - amount.find('.').unwrap_or(0) > 3 {
        amount.pop();
    }
    format!("${amount} USD")
}

fn seconds(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1_000.0)
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
                    cost_usd: None,
                },
                ExecutionReport {
                    provider: "OpenAI".into(),
                    model: "gpt-transcribe".into(),
                    streaming: false,
                    keyword_count: 2,
                    outcome: "success".into(),
                    cost_usd: Some(0.000_123),
                },
            ],
            ..StepReport::default()
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("terms"));
        let loaded: StepReport = serde_json::from_str(&json).unwrap();
        let rows = loaded.history_rows();
        assert_eq!(rows[0].1, "$0.000123 USD · partial (1 of 2 attempts)");
        assert!(rows[2].1.contains("Live streaming"));
        assert!(rows[2].1.contains("failed"));
        assert!(rows[2].1.contains("Cost not reported"));
        assert!(rows[3].1.contains("After recording"));
        assert!(rows[3].1.contains("2 keywords sent"));
        assert!(rows[3].1.contains("$0.000123 USD"));
        assert_eq!(
            serde_json::from_str::<StepReport>(r#"{"latency_ms":123}"#)
                .unwrap()
                .executions,
            Vec::new()
        );
    }

    #[test]
    fn rows_show_model_latency_fallbacks_and_trimming() {
        assert_eq!(
            preview().history_rows(),
            [
                ("Reported cost", "Not recorded".to_owned()),
                ("Model", "openai/gpt-4o-mini-transcribe · 820 ms".to_owned()),
                ("Fell back from", "openai/whisper-large-v3-turbo".to_owned()),
                (
                    "Audio",
                    "6.1 s sent of 9.4 s recorded (silence trimmed)".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn untrimmed_audio_and_no_fallback_read_plainly() {
        let report = StepReport {
            model: Some("a/b".into()),
            latency_ms: 500,
            failed: Vec::new(),
            executions: Vec::new(),
            omitted_executions: 0,
            audio: Some(AudioTrim {
                recorded_ms: 3_000,
                sent_ms: 3_000,
            }),
        };
        assert_eq!(
            report.history_rows(),
            [
                ("Reported cost", "Not recorded".to_owned()),
                ("Model", "a/b · 500 ms".to_owned()),
                ("Audio", "3.0 s sent".to_owned()),
            ]
        );
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
}
