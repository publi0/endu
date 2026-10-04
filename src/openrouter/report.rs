//! What the OpenRouter steps did for one dictation, kept in History.

use serde::{Deserialize, Serialize};

/// Longest model label stored; model ids are short, this bounds bad config.
const MAX_MODEL_CHARS: usize = 120;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription: Option<StepReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<StepReport>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StepReport {
    /// The model that produced the result; `None` when every model failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub latency_ms: u64,
    /// Models that failed before the one that answered, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// Transcription only: how much audio was recorded and how much was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioTrim>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioTrim {
    pub recorded_ms: u64,
    pub sent_ms: u64,
}

impl RunReport {
    pub fn is_empty(&self) -> bool {
        self.transcription.is_none() && self.cleanup.is_none()
    }

    /// The same report with every label bounded, for persistence.
    pub fn bounded(mut self) -> Self {
        for step in [&mut self.transcription, &mut self.cleanup]
            .into_iter()
            .flatten()
        {
            step.model = step.model.take().map(|model| bound(&model));
            step.failed.truncate(8);
            step.failed = step.failed.iter().map(|model| bound(model)).collect();
        }
        self
    }

    /// `(label, value)` rows for the History detail view.
    pub fn history_rows(&self) -> Vec<(&'static str, String)> {
        let mut rows = Vec::new();
        if let Some(step) = &self.transcription {
            let mut value = step_summary(step);
            if let Some(audio) = step.audio
                && audio.sent_ms < audio.recorded_ms
            {
                value.push_str(&format!(
                    " · sent {} of {} recorded",
                    seconds(audio.sent_ms),
                    seconds(audio.recorded_ms)
                ));
            }
            rows.push(("OpenRouter", value));
        }
        if let Some(step) = &self.cleanup {
            rows.push(("Cleanup", step_summary(step)));
        }
        rows
    }
}

/// Representative report for the History preview, in the fork build only.
pub fn preview() -> Option<RunReport> {
    super::ENABLED.then(|| RunReport {
        transcription: Some(StepReport {
            model: Some("openai/gpt-4o-mini-transcribe".into()),
            latency_ms: 820,
            failed: vec!["openai/whisper-large-v3-turbo".into()],
            audio: Some(AudioTrim {
                recorded_ms: 9_400,
                sent_ms: 6_100,
            }),
        }),
        cleanup: Some(StepReport {
            model: Some("openai/gpt-4o-mini".into()),
            latency_ms: 410,
            failed: Vec::new(),
            audio: None,
        }),
    })
}

fn step_summary(step: &StepReport) -> String {
    let mut summary = match &step.model {
        Some(model) => format!("{model} · {} ms", step.latency_ms),
        None => format!(
            "failed after {} ms; kept the previous text",
            step.latency_ms
        ),
    };
    if !step.failed.is_empty() {
        let verb = if step.model.is_some() {
            "fell back from"
        } else {
            "tried"
        };
        summary.push_str(&format!(" · {verb} {}", step.failed.join(", ")));
    }
    summary
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

    fn step(model: Option<&str>, latency_ms: u64, failed: &[&str]) -> StepReport {
        StepReport {
            model: model.map(Into::into),
            latency_ms,
            failed: failed.iter().map(|model| (*model).to_owned()).collect(),
            audio: None,
        }
    }

    #[test]
    fn rows_show_model_latency_fallbacks_and_trimming() {
        let report = RunReport {
            transcription: Some(StepReport {
                audio: Some(AudioTrim {
                    recorded_ms: 9_400,
                    sent_ms: 6_100,
                }),
                ..step(
                    Some("openai/gpt-4o-mini-transcribe"),
                    820,
                    &["openai/whisper-large-v3-turbo"],
                )
            }),
            cleanup: Some(step(Some("openai/gpt-4o-mini"), 410, &[])),
        };
        assert_eq!(
            report.history_rows(),
            [
                (
                    "OpenRouter",
                    "openai/gpt-4o-mini-transcribe · 820 ms · fell back from openai/whisper-large-v3-turbo · sent 6.1 s of 9.4 s recorded".to_owned()
                ),
                ("Cleanup", "openai/gpt-4o-mini · 410 ms".to_owned()),
            ]
        );
    }

    #[test]
    fn failed_cleanup_and_untrimmed_audio_read_plainly() {
        let report = RunReport {
            transcription: Some(StepReport {
                audio: Some(AudioTrim {
                    recorded_ms: 3_000,
                    sent_ms: 3_000,
                }),
                ..step(Some("a/b"), 500, &[])
            }),
            cleanup: Some(step(None, 15_000, &["x/y", "z/w"])),
        };
        let rows = report.history_rows();
        assert_eq!(rows[0].1, "a/b · 500 ms");
        assert_eq!(
            rows[1].1,
            "failed after 15000 ms; kept the previous text · tried x/y, z/w"
        );
    }

    #[test]
    fn empty_reports_have_no_rows_and_old_history_parses() {
        assert!(RunReport::default().is_empty());
        assert!(RunReport::default().history_rows().is_empty());
        let parsed: RunReport = serde_json::from_str("{}").unwrap();
        assert!(parsed.is_empty());
        let json = serde_json::to_string(&RunReport {
            transcription: Some(step(Some("m"), 1, &[])),
            cleanup: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"transcription":{"model":"m","latency_ms":1}}"#);
    }

    #[test]
    fn bounded_caps_labels() {
        let long = "m".repeat(500);
        let report = RunReport {
            transcription: Some(step(Some(&long), 1, &[long.as_str(); 20])),
            cleanup: None,
        }
        .bounded();
        let step = report.transcription.unwrap();
        assert_eq!(step.model.unwrap().len(), MAX_MODEL_CHARS);
        assert_eq!(step.failed.len(), 8);
        assert!(
            step.failed
                .iter()
                .all(|model| model.len() == MAX_MODEL_CHARS)
        );
    }
}
