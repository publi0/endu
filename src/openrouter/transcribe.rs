//! Cloud transcription through OpenRouter's `/audio/transcriptions` with an
//! ordered fallback chain: any failure on one model moves on to the next.

use std::io::Cursor;
use std::ops::Range;
#[cfg(test)]
use std::time::{Duration, Instant};

use color_eyre::Result;
#[cfg(test)]
use color_eyre::eyre::{WrapErr, bail, eyre};
#[cfg(test)]
use serde_json::{Value, json};

use super::StepReport;
#[cfg(test)]
use super::http::Response;
#[cfg(test)]
use super::stats::ErrorKind;
use super::stats::Failure;
#[cfg(test)]
use super::{Config, excerpt};

/// HEX hands the transcriber normalized 16 kHz mono samples.
pub const SAMPLE_RATE: u32 = 16_000;
const QUIET_SEARCH_SECONDS: usize = 10;
const QUIET_FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 10;

/// A finished transcription. `report` is `None` when nothing was sent
/// because the clip was empty or had no speech.
pub struct Transcription {
    pub text: String,
    pub report: Option<StepReport>,
}

/// One model's successful answer in a fallback chain.
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct Success {
    pub text: String,
    pub model: String,
    pub usage: Usage,
    /// The successful request only; excludes earlier failures and retry waits.
    pub latency_ms: u64,
    /// Attempts that failed before `model` answered, in order.
    pub failures: Vec<Failure>,
}

/// Every model in the chain failed.
#[derive(Debug)]
pub(crate) struct ChainFailure {
    pub failures: Vec<Failure>,
}

impl std::fmt::Display for ChainFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.failures.is_empty() {
            return formatter.write_str("No transcription models are configured");
        }
        let failures: Vec<String> = self
            .failures
            .iter()
            .map(|failure| failure.detail.clone())
            .collect();
        write!(
            formatter,
            "Transcription failed on every model: {}",
            failures.join("; ")
        )
    }
}

impl std::error::Error for ChainFailure {}

/// Billing details OpenRouter returns with a transcription.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Usage {
    pub tokens: u64,
    pub cost_usd: f64,
}

/// Shared entrypoint for recovery and CLI callers without a live capture session.
pub fn transcribe_with_vocabulary(
    samples: &[f32],
    vocabulary: &crate::vocabulary::Snapshot,
) -> Result<Transcription> {
    crate::providers::batch::transcribe(samples, vocabulary, None)
}

#[cfg(test)]
fn models(config: &Config) -> impl Iterator<Item = &str> {
    config
        .transcription
        .models
        .iter()
        .map(|model| model.trim())
        .filter(|model| !model.is_empty())
}

/// Try each configured model in order and return the first transcript.
/// `send` performs one HTTP request; `sleep` waits before a 429 retry.
#[cfg(test)]
pub(crate) fn transcribe_with_fallback(
    config: &Config,
    audio_base64: &str,
    language: Option<&str>,
    mut send: impl FnMut(&str, Duration) -> Result<Response>,
    sleep: impl FnMut(Duration),
) -> std::result::Result<Success, ChainFailure> {
    transcribe_with_timed_requests(
        config,
        audio_base64,
        language,
        |body, timeout| {
            let started = Instant::now();
            let response = send(body, timeout);
            (response, started.elapsed())
        },
        sleep,
    )
}

#[cfg(test)]
fn transcribe_with_timed_requests(
    config: &Config,
    audio_base64: &str,
    language: Option<&str>,
    send: impl FnMut(&str, Duration) -> (Result<Response>, Duration),
    sleep: impl FnMut(Duration),
) -> std::result::Result<Success, ChainFailure> {
    chain_with_hints(
        config,
        audio_base64,
        language,
        &Default::default(),
        send,
        sleep,
        |_| {},
    )
}

#[cfg(test)]
fn chain_with_hints(
    config: &Config,
    audio_base64: &str,
    language: Option<&str>,
    hints: &super::vocabulary_support::HintPlan,
    mut send: impl FnMut(&str, Duration) -> (Result<Response>, Duration),
    mut sleep: impl FnMut(Duration),
    mut invalidate: impl FnMut(&str),
) -> std::result::Result<Success, ChainFailure> {
    let started = Instant::now();
    let total = config.total_timeout();
    let max_rate_limit_wait =
        Duration::from_millis(config.transcription.rate_limit_retry_max_wait_ms);
    let mut failures: Vec<Failure> = Vec::new();
    for model in models(config) {
        let mut retried = false;
        let mut without_hints = false;
        loop {
            let remaining = total.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                failures.push(Failure {
                    model: model.to_owned(),
                    kind: ErrorKind::Timeout,
                    detail: format!("{model}: not tried, chain deadline reached"),
                });
                break;
            }
            let mut body = request_body(
                model,
                audio_base64,
                language,
                config.transcription.temperature,
            );
            let provider_options = (!without_hints).then(|| hints.for_model(model)).flatten();
            if let Some(options) = provider_options {
                let mut value: Value = serde_json::from_str(&body).expect("request JSON");
                value["provider"] = options.clone();
                body = value.to_string();
            }
            let timeout = config.attempt_timeout().min(remaining);
            let (response, request_latency) = send(&body, timeout);
            let (kind, detail) = match response {
                Ok(response) if response.is_success() => match parse_transcript(&response.body) {
                    Ok((text, usage)) => {
                        if !failures.is_empty() {
                            tracing::warn!(
                                model,
                                failed = failures.len(),
                                "OpenRouter transcription succeeded on a fallback model"
                            );
                        }
                        return Ok(Success {
                            text,
                            model: model.to_owned(),
                            usage,
                            latency_ms: request_latency.as_millis() as u64,
                            failures,
                        });
                    }
                    Err(error) => (ErrorKind::InvalidResponse, format!("{model}: {error}")),
                },
                Ok(response) => {
                    if !without_hints && hints.rejected(model, &response) {
                        without_hints = true;
                        invalidate(model);
                        failures.push(Failure {
                            model: model.to_owned(),
                            kind: ErrorKind::from_status(response.status),
                            detail: format!(
                                "{model}: vocabulary parameter rejected (HTTP {})",
                                response.status
                            ),
                        });
                        tracing::warn!(
                            model,
                            status = response.status,
                            "vocabulary rejected; retrying without hints"
                        );
                        continue;
                    }
                    if response.status == 401 {
                        super::forget_cached_key();
                    }
                    if response.status == 429 && !retried {
                        let wait = response.retry_after.unwrap_or(Duration::from_secs(1));
                        if wait <= max_rate_limit_wait && wait < remaining {
                            tracing::info!(
                                model,
                                wait_ms = wait.as_millis(),
                                "OpenRouter rate limited; retrying"
                            );
                            sleep(wait);
                            retried = true;
                            continue;
                        }
                    }
                    (
                        ErrorKind::from_status(response.status),
                        format!(
                            "{model}: HTTP {}: {}",
                            response.status,
                            if provider_options.is_some() {
                                "Provider request failed; vocabulary details omitted".to_owned()
                            } else {
                                excerpt(&response.body)
                            }
                        ),
                    )
                }
                Err(error) => {
                    let message = error.to_string();
                    (
                        ErrorKind::from_transport(&message),
                        format!("{model}: {message}"),
                    )
                }
            };
            tracing::warn!(failure = detail, "OpenRouter transcription attempt failed");
            failures.push(Failure {
                model: model.to_owned(),
                kind,
                detail,
            });
            break;
        }
    }
    Err(ChainFailure { failures })
}

#[cfg(test)]
fn request_body(
    model: &str,
    audio_base64: &str,
    language: Option<&str>,
    temperature: Option<f32>,
) -> String {
    let mut body = json!({
        "model": model,
        "input_audio": { "data": audio_base64, "format": "wav" },
    });
    if let Some(language) = language {
        body["language"] = json!(language);
    }
    if let Some(temperature) = temperature {
        body["temperature"] = json!(temperature);
    }
    body.to_string()
}

#[cfg(test)]
fn parse_transcript(body: &[u8]) -> Result<(String, Usage)> {
    let value: Value = serde_json::from_slice(body)
        .wrap_err_with(|| format!("invalid JSON response: {}", excerpt(body)))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string());
        bail!("provider error: {message}");
    }
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_owned())
        .ok_or_else(|| eyre!("response has no text: {}", excerpt(body)))?;
    let usage = value.get("usage");
    let number = |key: &str| usage.and_then(|usage| usage.get(key));
    Ok((
        text,
        Usage {
            tokens: number("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            cost_usd: number("cost").and_then(Value::as_f64).unwrap_or_default(),
        },
    ))
}

pub(crate) fn encode_wav(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buffer = Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    {
        let mut writer = hound::WavWriter::new(&mut buffer, spec)?;
        for sample in samples {
            let sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            writer.write_sample((sample * f32::from(i16::MAX)).round() as i16)?;
        }
        writer.finalize()?;
    }
    Ok(buffer.into_inner())
}

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        output.push(ALPHABET[(n >> 18) as usize & 63] as char);
        output.push(ALPHABET[(n >> 12) as usize & 63] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    output
}

/// Split long audio into chunks of at most `max_len` samples, cutting at the
/// quietest 100 ms frame within the last few seconds before each boundary.
pub(crate) fn chunk_ranges(samples: &[f32], max_len: usize) -> Vec<Range<usize>> {
    let max_len = max_len.max(QUIET_FRAME_SAMPLES * 2);
    let search = (QUIET_SEARCH_SECONDS * SAMPLE_RATE as usize).min(max_len / 2);
    let mut ranges = Vec::new();
    let mut start = 0;
    while samples.len() - start > max_len {
        let window_end = start + max_len;
        let window_start = window_end - search;
        let mut best = (f32::INFINITY, window_end);
        let mut frame = window_start;
        while frame + QUIET_FRAME_SAMPLES <= window_end {
            let energy: f32 = samples[frame..frame + QUIET_FRAME_SAMPLES]
                .iter()
                .map(|sample| sample * sample)
                .sum();
            if energy < best.0 {
                best = (energy, frame + QUIET_FRAME_SAMPLES / 2);
            }
            frame += QUIET_FRAME_SAMPLES;
        }
        ranges.push(start..best.1);
        start = best.1;
    }
    ranges.push(start..samples.len());
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn ok(body: &str) -> Result<Response> {
        Ok(Response {
            status: 200,
            retry_after: None,
            body: body.as_bytes().to_vec(),
        })
    }

    fn status(status: u16, retry_after: Option<u64>) -> Result<Response> {
        Ok(Response {
            status,
            retry_after: retry_after.map(Duration::from_secs),
            body: br#"{"error":{"message":"nope"}}"#.to_vec(),
        })
    }

    fn config(models: &[&str]) -> Config {
        let mut config = Config::default();
        config.transcription.models = models.iter().map(|model| (*model).to_owned()).collect();
        config
    }

    fn model_of(body: &str) -> String {
        serde_json::from_str::<Value>(body).unwrap()["model"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn rejected_hints_retry_without_parameters_and_do_not_leak_names() {
        let config = config(&["hinted"]);
        let hints = super::super::vocabulary_support::HintPlan::fixture(
            "hinted",
            json!({"options":{"azure":{"phraseList":{"phrases":["PRIVATE_FIXTURE_NAME"]}}}}),
        );
        let mut requests = Vec::new();
        let mut invalidated = Vec::new();
        let success = chain_with_hints(
            &config,
            "AAAA",
            None,
            &hints,
            |body, _| {
                let value: Value = serde_json::from_str(body).unwrap();
                requests.push(value);
                let response = if requests.len() == 1 {
                    Response {
                        status: 400,
                        body: br#"{"error":"phraseList PRIVATE_FIXTURE_NAME invalid"}"#.to_vec(),
                        retry_after: None,
                    }
                } else {
                    Response {
                        status: 200,
                        body: br#"{"text":"ok"}"#.to_vec(),
                        retry_after: None,
                    }
                };
                (Ok(response), Duration::from_millis(1))
            },
            |_| {},
            |model| invalidated.push(model.to_owned()),
        )
        .unwrap();
        assert!(requests[0].get("provider").is_some());
        assert!(requests[1].get("provider").is_none());
        assert_eq!(requests.len(), 2);
        assert_eq!(invalidated, ["hinted"]);
        assert_eq!(success.text, "ok");
        assert!(!success.failures[0].detail.contains("PRIVATE_FIXTURE_NAME"));
    }

    #[test]
    fn vocabulary_retry_cannot_extend_the_original_deadline() {
        let mut config = config(&["hinted", "fallback"]);
        config.transcription.total_timeout_seconds = 1;
        let hints = super::super::vocabulary_support::HintPlan::fixture(
            "hinted",
            json!({"options":{"openai":{"keywords":["Nimbus"]}}}),
        );
        let mut calls = 0;
        let result = chain_with_hints(
            &config,
            "AAAA",
            None,
            &hints,
            |_, _| {
                calls += 1;
                std::thread::sleep(Duration::from_millis(1_010));
                (
                    Ok(Response {
                        status: 400,
                        body: b"invalid keywords".to_vec(),
                        retry_after: None,
                    }),
                    Duration::from_secs(1),
                )
            },
            |_| {},
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn each_fallback_gets_only_its_own_verified_options() {
        let config = config(&["unsupported", "hinted"]);
        let hints = super::super::vocabulary_support::HintPlan::fixture(
            "hinted",
            json!({"options":{"openai":{"keywords":["Nimbus"]}}}),
        );
        let mut calls = 0;
        let result = chain_with_hints(
            &config,
            "AAAA",
            None,
            &hints,
            |body, _| {
                let value: Value = serde_json::from_str(body).unwrap();
                calls += 1;
                let response = if calls == 1 {
                    assert!(value.get("provider").is_none());
                    Response {
                        status: 503,
                        body: Vec::new(),
                        retry_after: None,
                    }
                } else {
                    assert_eq!(
                        value["provider"]["options"]["openai"]["keywords"],
                        json!(["Nimbus"])
                    );
                    Response {
                        status: 200,
                        body: br#"{"text":"ready"}"#.to_vec(),
                        retry_after: None,
                    }
                };
                (Ok(response), Duration::ZERO)
            },
            |_| {},
            |_| panic!("no rejection"),
        )
        .unwrap();
        assert_eq!(result.model, "hinted");
        assert_eq!(calls, 2);
    }

    #[test]
    fn first_model_success_returns_trimmed_text() {
        let calls = RefCell::new(Vec::new());
        let success = transcribe_with_fallback(
            &config(&["a", "b"]),
            "AAAA",
            Some("pt"),
            |body, _| {
                calls.borrow_mut().push(model_of(body));
                ok(r#"{"text":"  olá mundo \n"}"#)
            },
            |_| panic!("no sleep expected"),
        )
        .unwrap();
        assert_eq!(success.text, "olá mundo");
        assert_eq!(success.model, "a");
        assert!(success.failures.is_empty());
        assert_eq!(*calls.borrow(), ["a"]);
    }

    #[test]
    fn every_kind_of_failure_falls_back_to_the_next_model() {
        let calls = RefCell::new(Vec::new());
        let success = transcribe_with_fallback(
            &config(&[
                "transport",
                "server",
                "garbage",
                "provider",
                "missing",
                "good",
            ]),
            "AAAA",
            None,
            |body, _| {
                let model = model_of(body);
                calls.borrow_mut().push(model.clone());
                match model.as_str() {
                    "transport" => Err(eyre!("connection reset")),
                    "server" => status(503, None),
                    "garbage" => ok("<html>"),
                    "provider" => ok(r#"{"error":{"message":"bad audio"}}"#),
                    "missing" => ok(r#"{"usage":{}}"#),
                    _ => ok(r#"{"text":"done"}"#),
                }
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(success.text, "done");
        assert_eq!(success.model, "good");
        let failed: Vec<(&str, ErrorKind)> = success
            .failures
            .iter()
            .map(|failure| (failure.model.as_str(), failure.kind))
            .collect();
        assert_eq!(
            failed,
            [
                ("transport", ErrorKind::Network),
                ("server", ErrorKind::Server),
                ("garbage", ErrorKind::InvalidResponse),
                ("provider", ErrorKind::InvalidResponse),
                ("missing", ErrorKind::InvalidResponse),
            ]
        );
        assert_eq!(calls.borrow().len(), 6);
    }

    #[test]
    fn short_rate_limit_retries_the_same_model_once() {
        let calls = RefCell::new(Vec::new());
        let slept = RefCell::new(Vec::new());
        let success = transcribe_with_fallback(
            &config(&["a", "b"]),
            "AAAA",
            None,
            |body, _| {
                let model = model_of(body);
                calls.borrow_mut().push(model.clone());
                if calls.borrow().len() == 1 {
                    status(429, Some(1))
                } else {
                    ok(r#"{"text":"ok"}"#)
                }
            },
            |wait| slept.borrow_mut().push(wait),
        )
        .unwrap();
        assert_eq!(success.text, "ok");
        assert_eq!(success.model, "a");
        assert!(success.failures.is_empty(), "a retry is not a fallback");
        assert_eq!(*calls.borrow(), ["a", "a"]);
        assert_eq!(*slept.borrow(), [Duration::from_secs(1)]);
    }

    #[test]
    fn model_latency_excludes_failed_attempts_and_retry_waits() {
        let mut calls = Vec::new();
        let mut waits = Vec::new();
        let success = transcribe_with_timed_requests(
            &config(&["a", "b"]),
            "AAAA",
            None,
            |body, _| {
                calls.push(model_of(body));
                match calls.len() {
                    1 => (status(503, None), Duration::from_secs(8)),
                    2 => (status(429, Some(1)), Duration::from_millis(100)),
                    _ => (ok(r#"{"text":"done"}"#), Duration::from_millis(450)),
                }
            },
            |wait| waits.push(wait),
        )
        .unwrap();
        assert_eq!(calls, ["a", "b", "b"]);
        assert_eq!(waits, [Duration::from_secs(1)]);
        assert_eq!(success.model, "b");
        assert_eq!(success.latency_ms, 450);
        assert_eq!(success.failures.len(), 1);
    }

    #[test]
    fn long_or_repeated_rate_limits_fall_back_instead_of_waiting() {
        let calls = RefCell::new(Vec::new());
        let error = transcribe_with_fallback(
            &config(&["slow", "twice"]),
            "AAAA",
            None,
            |body, _| {
                calls.borrow_mut().push(model_of(body));
                match calls.borrow().len() {
                    1 => status(429, Some(30)),
                    _ => status(429, Some(1)),
                }
            },
            |_| {},
        )
        .unwrap_err();
        assert!(
            error
                .failures
                .iter()
                .all(|failure| failure.kind == ErrorKind::RateLimited)
        );
        let error = error.to_string();
        assert_eq!(*calls.borrow(), ["slow", "twice", "twice"]);
        assert!(error.contains("slow: HTTP 429"), "{error}");
        assert!(error.contains("twice: HTTP 429"), "{error}");
    }

    #[test]
    fn total_failure_reports_every_model() {
        let error = transcribe_with_fallback(
            &config(&["a", " ", "b"]),
            "AAAA",
            None,
            |_, _| status(500, None),
            |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("a: HTTP 500"), "{error}");
        assert!(error.contains("b: HTTP 500"), "{error}");
        assert!(!error.contains(" : "), "blank models are skipped: {error}");
    }

    #[test]
    fn usage_is_read_when_present() {
        let (text, usage) = parse_transcript(
            br#"{"text":" oi ","usage":{"seconds":3,"total_tokens":42,"cost":0.0012}}"#,
        )
        .unwrap();
        assert_eq!(text, "oi");
        assert_eq!(usage.tokens, 42);
        assert!((usage.cost_usd - 0.0012).abs() < 1e-9);
        let (_, usage) = parse_transcript(br#"{"text":"oi"}"#).unwrap();
        assert_eq!(usage, Usage::default());
    }

    #[test]
    fn request_body_includes_language_only_when_set() {
        let with: Value =
            serde_json::from_str(&request_body("m", "QQ==", Some("pt"), Some(0.0))).unwrap();
        assert_eq!(with["language"], "pt");
        assert_eq!(with["input_audio"]["format"], "wav");
        assert_eq!(with["input_audio"]["data"], "QQ==");
        let without: Value = serde_json::from_str(&request_body("m", "QQ==", None, None)).unwrap();
        assert!(without.get("language").is_none());
        assert!(without.get("temperature").is_none());
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn wav_is_16_bit_mono_16k_and_clamps() {
        let wav = encode_wav(&[0.0, 1.0, -1.0, 2.0, f32::NAN]).unwrap();
        let reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.channels, spec.sample_rate, spec.bits_per_sample),
            (1, 16_000, 16)
        );
        let samples: Vec<i16> = reader
            .into_samples()
            .map(|sample| sample.unwrap())
            .collect();
        assert_eq!(samples, [0, i16::MAX, -i16::MAX, i16::MAX, 0]);
    }

    #[test]
    fn short_audio_is_one_chunk() {
        assert_eq!(
            chunk_ranges(&vec![0.1; 1_000], 16_000 * 120),
            vec![0..1_000]
        );
    }

    #[test]
    fn long_audio_splits_at_the_quietest_frame_and_covers_everything() {
        let rate = SAMPLE_RATE as usize;
        let mut samples = vec![0.5; rate * 50];
        // A silent gap 2 s before the 20 s boundary.
        let gap = rate * 18;
        samples[gap..gap + QUIET_FRAME_SAMPLES].fill(0.0);
        let ranges = chunk_ranges(&samples, rate * 20);
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, samples.len());
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        assert!(ranges.iter().all(|range| range.len() <= rate * 20));
        assert_eq!(ranges[0].end, gap + QUIET_FRAME_SAMPLES / 2);
    }
}
