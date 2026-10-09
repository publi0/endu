//! Selectable recording start cues. Each synthesized cue is rendered once,
//! deterministically, from a few oscillators and filtered noise bursts; Classic
//! keeps the original bundled recording.

use crate::i18n::t;
use serde::{Deserialize, Serialize};

pub const SAMPLE_RATE: u32 = 48_000;

/// Every synthesized cue is normalized to this peak so the start volume keeps
/// the same meaning, close to the original bundled cue, whichever cue is chosen.
const PEAK: f32 = 0.85;
const FLOOR: f32 = 0.0001;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartCue {
    #[default]
    Breath,
    Drop,
    TwoNotes,
    Pen,
    Glass,
    Wood,
    Classic,
}

impl StartCue {
    pub const ALL: [Self; 7] = [
        Self::Breath,
        Self::Drop,
        Self::TwoNotes,
        Self::Pen,
        Self::Glass,
        Self::Wood,
        Self::Classic,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Breath => t("Breath"),
            Self::Drop => t("Drop"),
            Self::TwoNotes => t("Two notes"),
            Self::Pen => t("Pen click"),
            Self::Glass => t("Glass"),
            Self::Wood => t("Wood"),
            Self::Classic => t("Classic"),
        }
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|cue| *cue == self)
            .unwrap_or_default()
    }

    /// Mono samples at [`SAMPLE_RATE`], or `None` for the bundled Classic cue.
    pub fn samples(self) -> Option<Vec<f32>> {
        let mut cue = Synth::default();
        match self {
            Self::Breath => {
                cue.noise(Noise {
                    at: 0.0,
                    filter: Filter::BandPass,
                    frequency: (900.0, 3200.0),
                    q: 3.0,
                    envelope: Envelope::new(0.07, 0.1, 0.9),
                });
                cue.tone(Tone::sine(1320.0, 0.09, Envelope::new(0.004, 0.12, 0.25)));
            }
            Self::Drop => {
                cue.tone(
                    Tone::sine(520.0, 0.0, Envelope::new(0.003, 0.13, 0.9)).bend(1250.0, 0.06),
                );
                cue.tone(
                    Tone::sine(1040.0, 0.0, Envelope::new(0.003, 0.08, 0.15)).bend(2500.0, 0.06),
                );
            }
            Self::TwoNotes => {
                for (frequency, at) in [(659.25, 0.0), (987.77, 0.075)] {
                    cue.tone(Tone::sine(frequency, at, Envelope::new(0.004, 0.2, 0.7)));
                    cue.tone(Tone::sine(
                        frequency * 4.0,
                        at,
                        Envelope::new(0.002, 0.05, 0.08),
                    ));
                }
            }
            Self::Pen => {
                for (at, level, frequency) in [(0.0, 1.0, 3400.0), (0.045, 0.75, 2900.0)] {
                    cue.noise(Noise {
                        at,
                        filter: Filter::HighPass,
                        frequency: (2500.0, 2500.0),
                        q: std::f32::consts::FRAC_1_SQRT_2,
                        envelope: Envelope::new(0.001, 0.012, 0.9 * level),
                    });
                    cue.tone(Tone::sine(
                        frequency,
                        at,
                        Envelope::new(0.001, 0.018, 0.35 * level),
                    ));
                }
            }
            Self::Glass => {
                cue.tone(Tone::sine(1568.0, 0.0, Envelope::new(0.003, 0.4, 0.6)));
                cue.tone(Tone::sine(
                    1568.0 * 2.76,
                    0.0,
                    Envelope::new(0.002, 0.2, 0.18),
                ));
                cue.tone(Tone::sine(
                    1568.0 * 5.4,
                    0.0,
                    Envelope::new(0.001, 0.09, 0.06),
                ));
            }
            Self::Wood => {
                cue.tone(Tone::sine(900.0, 0.0, Envelope::new(0.002, 0.08, 0.9)).bend(760.0, 0.02));
                cue.tone(Tone {
                    triangle: true,
                    ..Tone::sine(1800.0, 0.0, Envelope::new(0.001, 0.03, 0.2))
                });
                cue.noise(Noise {
                    at: 0.0,
                    filter: Filter::BandPass,
                    frequency: (2200.0, 2200.0),
                    q: 2.0,
                    envelope: Envelope::new(0.001, 0.01, 0.4),
                });
            }
            Self::Classic => return None,
        }
        Some(cue.finish())
    }
}

/// An exponential attack to `peak` followed by an exponential decay, matching
/// the shape the cues were designed with.
#[derive(Clone, Copy)]
struct Envelope {
    attack: f32,
    decay: f32,
    peak: f32,
}

impl Envelope {
    fn new(attack: f32, decay: f32, peak: f32) -> Self {
        Self {
            attack,
            decay,
            peak,
        }
    }

    fn duration(self) -> f32 {
        self.attack + self.decay
    }

    fn gain(self, time: f32) -> f32 {
        if !(0.0..self.duration()).contains(&time) {
            0.0
        } else if time < self.attack {
            FLOOR * (self.peak / FLOOR).powf(time / self.attack)
        } else {
            self.peak * (FLOOR / self.peak).powf((time - self.attack) / self.decay)
        }
    }
}

#[derive(Clone, Copy)]
struct Tone {
    frequency: f32,
    bend: Option<(f32, f32)>,
    triangle: bool,
    at: f32,
    envelope: Envelope,
}

impl Tone {
    fn sine(frequency: f32, at: f32, envelope: Envelope) -> Self {
        Self {
            frequency,
            bend: None,
            triangle: false,
            at,
            envelope,
        }
    }

    /// Glides exponentially to `target` over `seconds`, then holds.
    fn bend(self, target: f32, seconds: f32) -> Self {
        Self {
            bend: Some((target, seconds)),
            ..self
        }
    }

    fn frequency_at(self, time: f32) -> f32 {
        match self.bend {
            Some((target, seconds)) => {
                self.frequency * (target / self.frequency).powf((time / seconds).min(1.0))
            }
            None => self.frequency,
        }
    }
}

#[derive(Clone, Copy)]
enum Filter {
    BandPass,
    HighPass,
}

#[derive(Clone, Copy)]
struct Noise {
    at: f32,
    filter: Filter,
    /// Cutoff or center, swept exponentially across the envelope.
    frequency: (f32, f32),
    q: f32,
    envelope: Envelope,
}

#[derive(Default)]
struct Synth {
    samples: Vec<f32>,
    /// A fixed seed keeps every rendering of a cue identical.
    seed: u32,
}

impl Synth {
    fn span(&mut self, at: f32, duration: f32) -> std::ops::Range<usize> {
        let start = (at * SAMPLE_RATE as f32).round() as usize;
        let end = ((at + duration) * SAMPLE_RATE as f32).ceil() as usize;
        if self.samples.len() < end {
            self.samples.resize(end, 0.0);
        }
        start..end
    }

    fn tone(&mut self, tone: Tone) {
        let mut phase = 0.0_f32;
        for (offset, index) in self.span(tone.at, tone.envelope.duration()).enumerate() {
            let time = offset as f32 / SAMPLE_RATE as f32;
            let wave = if tone.triangle {
                1.0 - 4.0 * (phase - 0.5).abs()
            } else {
                (phase * std::f32::consts::TAU).sin()
            };
            self.samples[index] += wave * tone.envelope.gain(time);
            phase = (phase + tone.frequency_at(time) / SAMPLE_RATE as f32).fract();
        }
    }

    fn noise(&mut self, noise: Noise) {
        let duration = noise.envelope.duration();
        let mut history = [0.0_f32; 4];
        for (offset, index) in self.span(noise.at, duration).enumerate() {
            let time = offset as f32 / SAMPLE_RATE as f32;
            let (from, to) = noise.frequency;
            let frequency = from * (to / from).powf(time / duration);
            let [b0, b1, b2, a1, a2] = biquad(noise.filter, frequency, noise.q);
            let input = self.white();
            let [x1, x2, y1, y2] = history;
            let output = b0 * input + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
            history = [input, x1, output, y1];
            self.samples[index] += output * noise.envelope.gain(time);
        }
    }

    /// Uniform white noise from a xorshift generator, in -1..1.
    fn white(&mut self) -> f32 {
        let mut state = if self.seed == 0 {
            0x9E37_79B9
        } else {
            self.seed
        };
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        self.seed = state;
        state as f32 / u32::MAX as f32 * 2.0 - 1.0
    }

    fn finish(mut self) -> Vec<f32> {
        let peak = self
            .samples
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0, f32::max);
        if peak > 0.0 {
            for sample in &mut self.samples {
                *sample *= PEAK / peak;
            }
        }
        self.samples
    }
}

/// Normalized RBJ cookbook coefficients: band-pass with 0 dB peak gain, or a
/// resonant high-pass.
fn biquad(filter: Filter, frequency: f32, q: f32) -> [f32; 5] {
    let omega = std::f32::consts::TAU * frequency / SAMPLE_RATE as f32;
    let (sin, cos) = omega.sin_cos();
    let alpha = sin / (2.0 * q);
    let a0 = 1.0 + alpha;
    let (b0, b1, b2) = match filter {
        Filter::BandPass => (alpha, 0.0, -alpha),
        Filter::HighPass => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
    };
    [
        b0 / a0,
        b1 / a0,
        b2 / a0,
        -2.0 * cos / a0,
        (1.0 - alpha) / a0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breath_is_the_default_and_every_cue_round_trips() {
        assert_eq!(StartCue::default(), StartCue::Breath);
        for (index, cue) in StartCue::ALL.into_iter().enumerate() {
            assert_eq!(cue.index(), index);
            let json = serde_json::to_string(&cue).unwrap();
            assert_eq!(serde_json::from_str::<StartCue>(&json).unwrap(), cue);
        }
        assert_eq!(
            serde_json::to_string(&StartCue::TwoNotes).unwrap(),
            "\"two_notes\""
        );
    }

    #[test]
    fn synthesized_cues_are_short_bounded_deterministic_and_end_silent() {
        for cue in StartCue::ALL {
            let Some(samples) = cue.samples() else {
                assert_eq!(cue, StartCue::Classic);
                continue;
            };
            let seconds = samples.len() as f32 / SAMPLE_RATE as f32;
            // The microphone is already open, so a cue must not linger into speech.
            assert!(
                seconds > 0.02 && seconds <= 0.45,
                "{cue:?} lasts {seconds}s"
            );
            assert!(samples.iter().all(|sample| sample.is_finite()));
            let peak = samples
                .iter()
                .map(|sample| sample.abs())
                .fold(0.0, f32::max);
            assert!((peak - PEAK).abs() < 1e-4, "{cue:?} peaks at {peak}");
            assert!(
                samples.last().unwrap().abs() < 0.01,
                "{cue:?} ends abruptly"
            );
            assert_eq!(cue.samples().unwrap(), samples);
        }
    }

    #[test]
    fn cues_are_distinct() {
        let rendered: Vec<_> = StartCue::ALL
            .into_iter()
            .filter_map(StartCue::samples)
            .collect();
        for (index, left) in rendered.iter().enumerate() {
            for right in &rendered[index + 1..] {
                assert_ne!(left, right);
            }
        }
    }
}
