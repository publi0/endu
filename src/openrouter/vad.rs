//! Energy-based speech detection that trims silence before audio is sent to
//! OpenRouter: less audio billed, a smaller upload, and no silent clips for
//! the provider to hallucinate on.
//!
//! The threshold adapts to each recording's noise floor. Margins and the
//! minimum kept pause err toward keeping audio, so speech is not clipped.

use std::ops::Range;

use super::transcribe::SAMPLE_RATE;

const FRAME: usize = SAMPLE_RATE as usize / 50; // 20 ms
/// Audio kept before and after every speech region.
const MARGIN_FRAMES: usize = 10; // 200 ms
/// A silent gap longer than this (beyond the margins) is shortened to it.
const KEPT_GAP_FRAMES: usize = 15; // 300 ms
/// Louder-than-threshold runs shorter than this are clicks, not speech.
const MIN_SPEECH_RUN_FRAMES: usize = 3; // 60 ms
/// Less detected speech than this in total means "nothing was said".
const MIN_TOTAL_SPEECH_FRAMES: usize = 8; // 160 ms
/// About -54 dBFS: below this nothing counts as speech, however quiet the room.
const ABSOLUTE_FLOOR: f32 = 0.002;
/// Speech must be this far above the noise floor (10 dB).
const ABOVE_NOISE: f32 = 3.16;
/// But never require more than this fraction of the loudest frame (-14 dB),
/// so a recording that is speech throughout keeps its quieter syllables.
const BELOW_PEAK: f32 = 0.2;

#[derive(Debug, PartialEq)]
pub enum Trimmed {
    /// No speech was detected; nothing should be sent.
    Silent,
    Speech(Vec<f32>),
}

pub fn trim(samples: &[f32]) -> Trimmed {
    let energies: Vec<f32> = samples.chunks(FRAME).map(rms).collect();
    if energies.is_empty() {
        return Trimmed::Silent;
    }
    let threshold = threshold(&energies);
    let speech = speech_frames(&energies, threshold);
    if speech.iter().filter(|frame| **frame).count() < MIN_TOTAL_SPEECH_FRAMES {
        return Trimmed::Silent;
    }
    let regions = regions(&speech);
    let mut output = Vec::with_capacity(samples.len());
    let to_samples = |frames: Range<usize>| {
        (frames.start * FRAME).min(samples.len())..(frames.end * FRAME).min(samples.len())
    };
    for (index, region) in regions.iter().enumerate() {
        if index > 0 {
            // Keep a short slice of each long pause: half from each side, so
            // the model still hears a boundary it can punctuate.
            let gap = regions[index - 1].end..region.start;
            if gap.len() <= KEPT_GAP_FRAMES {
                output.extend_from_slice(&samples[to_samples(gap)]);
            } else {
                let half = KEPT_GAP_FRAMES / 2;
                output.extend_from_slice(&samples[to_samples(gap.start..gap.start + half)]);
                output.extend_from_slice(
                    &samples[to_samples(gap.end - (KEPT_GAP_FRAMES - half)..gap.end)],
                );
            }
        }
        output.extend_from_slice(&samples[to_samples(region.clone())]);
    }
    Trimmed::Speech(output)
}

fn rms(frame: &[f32]) -> f32 {
    let sum: f32 = frame
        .iter()
        .map(|sample| {
            if sample.is_finite() {
                sample * sample
            } else {
                0.0
            }
        })
        .sum();
    (sum / frame.len().max(1) as f32).sqrt()
}

fn threshold(energies: &[f32]) -> f32 {
    let mut sorted = energies.to_vec();
    sorted.sort_by(f32::total_cmp);
    let noise_floor = sorted[sorted.len() / 10];
    let peak = sorted[sorted.len() - 1];
    (noise_floor * ABOVE_NOISE)
        .min(peak * BELOW_PEAK)
        .max(ABSOLUTE_FLOOR)
}

/// Frames above the threshold, ignoring runs too short to be speech.
fn speech_frames(energies: &[f32], threshold: f32) -> Vec<bool> {
    let mut speech: Vec<bool> = energies.iter().map(|energy| *energy > threshold).collect();
    let mut start = 0;
    while start < speech.len() {
        if !speech[start] {
            start += 1;
            continue;
        }
        let end = (start..speech.len())
            .find(|index| !speech[*index])
            .unwrap_or(speech.len());
        if end - start < MIN_SPEECH_RUN_FRAMES {
            speech[start..end].fill(false);
        }
        start = end;
    }
    speech
}

/// Speech regions widened by the margins and merged where they touch.
fn regions(speech: &[bool]) -> Vec<Range<usize>> {
    let mut regions: Vec<Range<usize>> = Vec::new();
    for (index, _) in speech.iter().enumerate().filter(|(_, speech)| **speech) {
        let start = index.saturating_sub(MARGIN_FRAMES);
        let end = (index + 1 + MARGIN_FRAMES).min(speech.len());
        match regions.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => regions.push(start..end),
        }
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: usize = SAMPLE_RATE as usize;

    /// A 220 Hz tone standing in for speech.
    fn tone(seconds: f32, amplitude: f32) -> Vec<f32> {
        (0..(seconds * SECOND as f32) as usize)
            .map(|index| {
                (index as f32 * 220.0 * std::f32::consts::TAU / SECOND as f32).sin() * amplitude
            })
            .collect()
    }

    /// Deterministic low-level noise standing in for a quiet room.
    fn noise(seconds: f32, amplitude: f32) -> Vec<f32> {
        let mut state = 0x2545_f491_u32;
        (0..(seconds * SECOND as f32) as usize)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    fn concat(parts: &[Vec<f32>]) -> Vec<f32> {
        parts.concat()
    }

    fn seconds(samples: &[f32]) -> f32 {
        samples.len() as f32 / SECOND as f32
    }

    #[test]
    fn silence_and_room_noise_are_silent() {
        assert_eq!(trim(&[]), Trimmed::Silent);
        assert_eq!(trim(&vec![0.0; 3 * SECOND]), Trimmed::Silent);
        assert_eq!(trim(&noise(3.0, 0.001)), Trimmed::Silent);
    }

    #[test]
    fn a_key_click_alone_is_not_speech() {
        let mut samples = noise(2.0, 0.001);
        samples[SECOND..SECOND + 200].fill(0.8); // 12 ms click
        assert_eq!(trim(&samples), Trimmed::Silent);
    }

    #[test]
    fn leading_and_trailing_silence_is_cut_with_margins() {
        let samples = concat(&[noise(2.0, 0.001), tone(1.0, 0.1), noise(3.0, 0.001)]);
        let Trimmed::Speech(trimmed) = trim(&samples) else {
            panic!("speech expected");
        };
        // 1 s of speech plus a 200 ms margin on each side.
        assert!(
            (seconds(&trimmed) - 1.4).abs() < 0.05,
            "{}",
            seconds(&trimmed)
        );
    }

    #[test]
    fn long_pauses_shrink_and_short_ones_stay() {
        let long = concat(&[tone(1.0, 0.1), noise(4.0, 0.001), tone(1.0, 0.1)]);
        let Trimmed::Speech(trimmed) = trim(&long) else {
            panic!("speech expected");
        };
        // 2 s of speech (it starts and ends the clip, so no outer margins)
        // plus a 4 s pause shortened to 200 + 300 + 200 ms.
        assert!(
            (seconds(&trimmed) - 2.7).abs() < 0.05,
            "{}",
            seconds(&trimmed)
        );

        let short = concat(&[tone(1.0, 0.1), noise(0.5, 0.001), tone(1.0, 0.1)]);
        let Trimmed::Speech(trimmed) = trim(&short) else {
            panic!("speech expected");
        };
        assert_eq!(trimmed.len(), short.len());
    }

    #[test]
    fn quiet_speech_in_a_quiet_room_is_kept() {
        let samples = concat(&[noise(1.0, 0.0005), tone(1.0, 0.008), noise(1.0, 0.0005)]);
        let Trimmed::Speech(trimmed) = trim(&samples) else {
            panic!("quiet speech expected");
        };
        assert!(seconds(&trimmed) > 1.3, "{}", seconds(&trimmed));
    }

    #[test]
    fn continuous_speech_with_quieter_parts_is_untouched() {
        let samples = concat(&[tone(1.0, 0.2), tone(1.0, 0.05), tone(1.0, 0.2)]);
        let Trimmed::Speech(trimmed) = trim(&samples) else {
            panic!("speech expected");
        };
        assert_eq!(trimmed.len(), samples.len());
    }

    #[test]
    fn speech_in_a_noisy_room_keeps_the_speech() {
        let samples = concat(&[noise(2.0, 0.02), tone(1.0, 0.3), noise(2.0, 0.02)]);
        let Trimmed::Speech(trimmed) = trim(&samples) else {
            panic!("speech expected");
        };
        assert!(
            seconds(&trimmed) >= 1.0 && seconds(&trimmed) < 2.0,
            "{}",
            seconds(&trimmed)
        );
    }
}
