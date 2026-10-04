//! Cloud transcription through OpenRouter's `/audio/transcriptions` with an
//! ordered fallback chain: any failure on one model moves on to the next.

use std::io::Cursor;
use std::ops::Range;
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, bail, eyre};
use serde_json::{Value, json};

use super::http::{self, Response};
use super::{Config, excerpt};
use crate::transcription_models::{AUTO_LANGUAGE, TranscriptionSelection};

/// HEX hands transcribers normalized 16 kHz mono samples.
pub const SAMPLE_RATE: u32 = 16_000;
const QUIET_SEARCH_SECONDS: usize = 10;
const QUIET_FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 10;

pub struct OpenRouterTranscriber {
    selection: TranscriptionSelection,
}

impl OpenRouterTranscriber {
    /// Validate that a request could be sent: readable config, at least one
    /// model, and an API key. No network call is made.
    pub fn load(selection: &TranscriptionSelection) -> Result<Self> {
        let config = super::load_config()?;
        if models(&config).next().is_none() {
            bail!(
                "No OpenRouter transcription models are configured in {}",
                super::config_path()?.display()
            );
        }
        super::api_key(&config)?;
        Ok(Self {
            selection: selection.clone(),
        })
    }

    pub fn matches_selection(&self, selection: &TranscriptionSelection) -> bool {
        &self.selection == selection
    }

    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let config = super::load_config()?;
        let api_key = super::api_key(&config)?;
        let language =
            (self.selection.language != AUTO_LANGUAGE).then_some(self.selection.language.as_str());
        let url = config.endpoint("audio/transcriptions");
        // 200 s of 16 kHz 16-bit WAV is ~8.5 MB as base64, under the request cap.
        let chunk_seconds = config.transcription.chunk_seconds.clamp(10, 200);
        let chunk_samples = (chunk_seconds * u64::from(SAMPLE_RATE)) as usize;
        let mut texts = Vec::new();
        for range in chunk_ranges(samples, chunk_samples) {
            let started = Instant::now();
            let audio = encode_base64(&encode_wav(&samples[range])?);
            let text = transcribe_with_fallback(
                &config,
                &audio,
                language,
                |body, timeout| http::post_json(&url, &api_key, body, timeout),
                std::thread::sleep,
            )?;
            tracing::info!(
                latency_ms = started.elapsed().as_millis(),
                "OpenRouter transcribed audio chunk"
            );
            if !text.is_empty() {
                texts.push(text);
            }
        }
        Ok(texts.join(" "))
    }
}

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
pub(crate) fn transcribe_with_fallback(
    config: &Config,
    audio_base64: &str,
    language: Option<&str>,
    mut send: impl FnMut(&str, Duration) -> Result<Response>,
    mut sleep: impl FnMut(Duration),
) -> Result<String> {
    let started = Instant::now();
    let total = config.total_timeout();
    let max_rate_limit_wait =
        Duration::from_millis(config.transcription.rate_limit_retry_max_wait_ms);
    let mut failures = Vec::new();
    for model in models(config) {
        let mut retried = false;
        loop {
            let remaining = total.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                failures.push(format!("{model}: not tried, chain deadline reached"));
                break;
            }
            let body = request_body(
                model,
                audio_base64,
                language,
                config.transcription.temperature,
            );
            let timeout = config.attempt_timeout().min(remaining);
            let failure = match send(&body, timeout) {
                Ok(response) if response.is_success() => match parse_transcript(&response.body) {
                    Ok(text) => {
                        if !failures.is_empty() {
                            tracing::warn!(
                                model,
                                failures = failures.join("; "),
                                "OpenRouter transcription succeeded on a fallback model"
                            );
                        }
                        return Ok(text);
                    }
                    Err(error) => format!("{model}: {error}"),
                },
                Ok(response) => {
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
                    format!(
                        "{model}: HTTP {}: {}",
                        response.status,
                        excerpt(&response.body)
                    )
                }
                Err(error) => format!("{model}: {error}"),
            };
            tracing::warn!(failure, "OpenRouter transcription attempt failed");
            failures.push(failure);
            break;
        }
    }
    if failures.is_empty() {
        bail!("No OpenRouter transcription models are configured");
    }
    bail!(
        "OpenRouter transcription failed on every model: {}",
        failures.join("; ")
    )
}

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

fn parse_transcript(body: &[u8]) -> Result<String> {
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
    value
        .get("text")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_owned())
        .ok_or_else(|| eyre!("response has no text: {}", excerpt(body)))
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
    fn first_model_success_returns_trimmed_text() {
        let calls = RefCell::new(Vec::new());
        let text = transcribe_with_fallback(
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
        assert_eq!(text, "olá mundo");
        assert_eq!(*calls.borrow(), ["a"]);
    }

    #[test]
    fn every_kind_of_failure_falls_back_to_the_next_model() {
        let calls = RefCell::new(Vec::new());
        let text = transcribe_with_fallback(
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
        assert_eq!(text, "done");
        assert_eq!(calls.borrow().len(), 6);
    }

    #[test]
    fn short_rate_limit_retries_the_same_model_once() {
        let calls = RefCell::new(Vec::new());
        let slept = RefCell::new(Vec::new());
        let text = transcribe_with_fallback(
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
        assert_eq!(text, "ok");
        assert_eq!(*calls.borrow(), ["a", "a"]);
        assert_eq!(*slept.borrow(), [Duration::from_secs(1)]);
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
        .unwrap_err()
        .to_string();
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
