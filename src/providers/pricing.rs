//! Estimated request cost from published list prices, for providers whose
//! APIs do not report what a request cost. OpenRouter reports its own cost, so
//! it never gets an estimate here. Estimates are labelled as such everywhere
//! and stay separate from reported costs.
//!
//! Prices in USD per minute of audio sent, checked on 2026-10-08:
//! - OpenAI: https://developers.openai.com/api/docs/pricing (per-minute estimates).
//! - Deepgram: https://deepgram.com/pricing (Pay As You Go; streaming uses the
//!   current promotional rate; keyterm prompting is a separate add-on).
//! - ElevenLabs: https://elevenlabs.io/pricing/api (per hour; keyterm add-on).
//! - xAI: https://docs.x.ai/developers/models ($0.10/h REST, $0.20/h streaming).
//! - Google: https://ai.google.dev/gemini-api/docs/pricing (Google's blended
//!   per-minute estimates for audio input plus text output).
//! - Meta: $0.18 per hour of audio for streaming and files, as reported from
//!   Meta's model page; its own pricing page was not reachable.
//!
//! Update this table when a provider changes its prices.

use super::{ModelRef, Provider};
use crate::openrouter::stats::RequestMode;

const PER_HOUR: f64 = 1.0 / 60.0;

/// What one successful request is billed on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Billing {
    pub audio_ms: u64,
    /// Auto-detect selects multilingual rates where a provider charges more.
    pub auto_language: bool,
}

/// List price per minute, plus the vocabulary add-on per minute when it is
/// billed separately. `None` when Hex has no published price for the model.
fn price_per_minute(
    model: ModelRef<'_>,
    mode: RequestMode,
    auto_language: bool,
) -> Option<(f64, f64)> {
    let live = mode == RequestMode::Live;
    Some(match (model.provider, model.model) {
        (Provider::OpenAi, "gpt-transcribe") => (0.0045, 0.0),
        (Provider::OpenAi, "gpt-live-transcribe") => (0.017, 0.0),
        (Provider::OpenAi, "gpt-4o-transcribe" | "whisper-1") => (0.006, 0.0),
        (Provider::OpenAi, "gpt-4o-mini-transcribe") => (0.003, 0.0),
        (Provider::Deepgram, "nova-3") => (
            match (live, auto_language) {
                (true, true) => 0.0058,
                (true, false) => 0.0048,
                (false, true) => 0.0052,
                (false, false) => 0.0043,
            },
            0.0013,
        ),
        (Provider::ElevenLabs, "scribe_v2") => (0.22 * PER_HOUR, 0.05 * PER_HOUR),
        (Provider::ElevenLabs, "scribe_v2_realtime") => (0.39 * PER_HOUR, 0.08 * PER_HOUR),
        (Provider::Grok, "grok-voice-transcribe-2.0") => {
            (if live { 0.20 } else { 0.10 } * PER_HOUR, 0.0)
        }
        (Provider::Google, "gemini-3.5-transcribe") => (0.005, 0.0),
        (Provider::Google, "gemini-3.5-transcribe-live") => (0.009, 0.0),
        (Provider::Meta, super::meta::MODEL) => (0.18 * PER_HOUR, 0.0),
        _ => return None,
    })
}

/// Estimated USD for one successful request, or `None` without a published
/// price or billed audio. Never used for OpenRouter, which reports its cost.
pub fn estimate(
    model: ModelRef<'_>,
    mode: RequestMode,
    billing: Billing,
    keyword_count: usize,
) -> Option<f64> {
    if model.provider == Provider::OpenRouter || billing.audio_ms == 0 {
        return None;
    }
    let (base, vocabulary) = price_per_minute(model, mode, billing.auto_language)?;
    let rate = base + if keyword_count > 0 { vocabulary } else { 0.0 };
    Some(rate * billing.audio_ms as f64 / 60_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn billing(audio_ms: u64) -> Billing {
        Billing {
            audio_ms,
            auto_language: false,
        }
    }

    #[test]
    fn estimates_follow_mode_language_and_vocabulary_add_ons() {
        let minute = billing(60_000);
        let close = |left: Option<f64>, right: f64| {
            assert!((left.unwrap() - right).abs() < 1e-12, "{left:?} != {right}");
        };
        let live = ModelRef::parse("openai::gpt-live-transcribe");
        close(estimate(live, RequestMode::Live, minute, 0), 0.017);
        let nova = ModelRef::parse("deepgram::nova-3");
        close(estimate(nova, RequestMode::Recorded, minute, 0), 0.0043);
        close(
            estimate(nova, RequestMode::Live, minute, 2),
            0.0048 + 0.0013,
        );
        let multilingual = Billing {
            auto_language: true,
            ..minute
        };
        close(estimate(nova, RequestMode::Live, multilingual, 0), 0.0058);
        let grok = ModelRef::parse("grok::grok-voice-transcribe-2.0");
        close(
            estimate(grok, RequestMode::Recorded, billing(3_600_000), 5),
            0.10,
        );
        close(
            estimate(grok, RequestMode::Live, billing(3_600_000), 5),
            0.20,
        );
        let meta = ModelRef::parse("meta::muse-voice-transcribe-1.0");
        close(
            estimate(meta, RequestMode::Live, billing(30_000), 0),
            0.0015,
        );
    }

    #[test]
    fn reported_routes_unknown_models_and_silent_requests_get_no_estimate() {
        let minute = billing(60_000);
        for id in [
            "openai/whisper-1",
            "microsoft/mai-transcribe-2",
            "deepgram::nova-2",
        ] {
            assert_eq!(
                estimate(ModelRef::parse(id), RequestMode::Recorded, minute, 0),
                None,
                "{id}"
            );
        }
        assert_eq!(
            estimate(
                ModelRef::parse("openai::gpt-transcribe"),
                RequestMode::Recorded,
                billing(0),
                0
            ),
            None
        );
    }
}
