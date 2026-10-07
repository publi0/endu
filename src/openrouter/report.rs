//! What the OpenRouter transcription did for one dictation, kept in History.

use serde::{Deserialize, Serialize};

/// Longest model label stored; model ids are short, this bounds bad config.
const MAX_MODEL_CHARS: usize = 120;
const MAX_FAILED_MODELS: usize = 8;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StepReport {
    /// Actual requests; absent in History saved by earlier versions. Never terms or prompts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executions: Vec<ExecutionReport>,
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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionReport {
    pub provider: String,
    pub model: String,
    pub streaming: bool,
    pub keyword_count: usize,
    pub outcome: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioTrim {
    pub recorded_ms: u64,
    pub sent_ms: u64,
}

impl StepReport {
    /// The same report with every label bounded, for persistence.
    pub fn bounded(mut self) -> Self {
        self.executions.truncate(64);
        for execution in &mut self.executions {
            execution.provider = bound(&execution.provider);
            execution.model = bound(&execution.model);
            execution.outcome = execution.outcome.chars().take(80).collect();
            execution.keyword_count = execution.keyword_count.min(2000);
        }
        self.model = self.model.take().map(|model| bound(&model));
        self.failed.truncate(MAX_FAILED_MODELS);
        self.failed = self.failed.iter().map(|model| bound(model)).collect();
        self
    }

    /// `(label, value)` rows for the History detail view.
    pub fn history_rows(&self) -> Vec<(&'static str, String)> {
        let mut rows = vec![(
            "Model",
            format!(
                "{} · {} ms",
                self.model.as_deref().unwrap_or("unknown"),
                self.latency_ms
            ),
        )];
        for execution in &self.executions {
            let mode = if execution.streaming {
                "Live streaming"
            } else {
                "After recording"
            };
            let keywords = if execution.keyword_count == 0 {
                "No keywords sent".to_owned()
            } else {
                format!("{} keywords sent", execution.keyword_count)
            };
            rows.push((
                "Request",
                format!(
                    "{} · {} · {mode} · {keywords} · {}",
                    execution.provider, execution.model, execution.outcome
                ),
            ));
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
        audio: Some(AudioTrim {
            recorded_ms: 9_400,
            sent_ms: 6_100,
        }),
    }
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
                },
                ExecutionReport {
                    provider: "OpenAI".into(),
                    model: "gpt-transcribe".into(),
                    streaming: false,
                    keyword_count: 2,
                    outcome: "success".into(),
                },
            ],
            ..StepReport::default()
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("terms"));
        let loaded: StepReport = serde_json::from_str(&json).unwrap();
        let rows = loaded.history_rows();
        assert!(rows[1].1.contains("Live streaming"));
        assert!(rows[1].1.contains("failed"));
        assert!(rows[2].1.contains("After recording"));
        assert!(rows[2].1.contains("2 keywords sent"));
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
            audio: Some(AudioTrim {
                recorded_ms: 3_000,
                sent_ms: 3_000,
            }),
        };
        assert_eq!(
            report.history_rows(),
            [
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
}
