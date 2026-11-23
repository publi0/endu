//! Energy-based speech detection that trims silence before audio is sent to
//! OpenRouter: less audio billed, a smaller upload, and no silent clips for
//! the provider to hallucinate on.
//!
//! The threshold adapts to each recording's noise floor. Margins and the
//! minimum kept pause err toward keeping audio, so speech is not clipped.
//!
//! A clip with nothing above [`ABSOLUTE_FLOOR`] still keeps its speech when it
//! holds one sustained run well above its own noise floor: a quiet built-in
//! microphone can record real speech below that floor while its room noise
//! sits far lower.

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
/// The quiet-microphone path: a frame must be this far above the clip's own
/// noise floor (12 dB) to count as speech there.
const QUIET_ABOVE_NOISE: f32 = 3.98;
/// About -66 dBFS: the quiet path never accepts anything quieter, however
/// quiet the floor, so dither and a barely moving microphone stay silent.
const QUIET_FLOOR: f32 = 0.0005;
/// The quiet path needs one contiguous run this long above its threshold, so
/// scattered clicks never add up to speech.
const QUIET_SUSTAINED_FRAMES: usize = 9; // 180 ms
/// About -120 dBFS: frames at or below this are digital silence or a constant
/// offset (a device warming up) and never define the noise floor.
const DIGITAL_SILENCE: f32 = 1e-6;

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
    let mut speech = speech_frames(&energies, threshold);
    if !enough_speech(&speech) {
        match quiet_threshold(&energies) {
            Some(quiet) if quiet < threshold => speech = speech_frames(&energies, quiet),
            _ => return Trimmed::Silent,
        }
        if !enough_speech(&speech) {
            return Trimmed::Silent;
        }
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

/// The frame's RMS around its own mean, so a microphone's DC offset never
/// reads as loudness and raises the noise floor above quiet speech. Only the
/// measurement removes the offset; the samples sent on stay untouched.
fn rms(frame: &[f32]) -> f32 {
    let finite = || {
        frame
            .iter()
            .map(|sample| if sample.is_finite() { *sample } else { 0.0 })
    };
    let count = frame.len().max(1) as f32;
    let mean = finite().sum::<f32>() / count;
    let sum: f32 = finite()
        .map(|sample| (sample - mean) * (sample - mean))
        .sum();
    (sum / count).sqrt()
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

fn enough_speech(speech: &[bool]) -> bool {
    speech.iter().filter(|frame| **frame).count() >= MIN_TOTAL_SPEECH_FRAMES
}

/// The quiet-microphone threshold: 12 dB over the clip's own noise floor (its
/// 10th-percentile frame, ignoring digital silence), never below
/// [`QUIET_FLOOR`]. `None` unless one sustained run clears it.
fn quiet_threshold(energies: &[f32]) -> Option<f32> {
    let mut audible: Vec<f32> = energies
        .iter()
        .copied()
        .filter(|energy| *energy > DIGITAL_SILENCE)
        .collect();
    if audible.is_empty() {
        return None;
    }
    audible.sort_by(f32::total_cmp);
    let noise_floor = audible[(audible.len() - 1) / 10];
    let threshold = (noise_floor * QUIET_ABOVE_NOISE).max(QUIET_FLOOR);
    (longest_run(energies, threshold) >= QUIET_SUSTAINED_FRAMES).then_some(threshold)
}

/// The most frames above `threshold` in one run. A single quieter frame
/// between loud ones is bridged but not counted; two in a row end the run.
fn longest_run(energies: &[f32], threshold: f32) -> usize {
    let (mut run, mut longest, mut bridged) = (0, 0, false);
    for energy in energies {
        if *energy > threshold {
            run += 1;
            longest = longest.max(run);
            bridged = false;
        } else if run > 0 && !bridged {
            bridged = true;
        } else {
            run = 0;
            bridged = false;
        }
    }
    longest
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

    fn amplitude(dbfs: f32) -> f32 {
        10.0_f32.powf(dbfs / 20.0)
    }

    /// DC-free noise whose every frame measures `dbfs` RMS.
    fn hum(seconds: f32, dbfs: f32) -> Vec<f32> {
        let level = amplitude(dbfs);
        (0..(seconds * SECOND as f32) as usize)
            .map(|index| if index % 2 == 0 { level } else { -level })
            .collect()
    }

    /// A tone whose RMS is `dbfs`.
    fn speech_at(seconds: f32, dbfs: f32) -> Vec<f32> {
        tone(seconds, 2.0_f32.sqrt() * amplitude(dbfs))
    }

    fn frames(count: usize) -> f32 {
        (count * FRAME) as f32 / SECOND as f32
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
    fn a_dc_offset_never_reads_as_speech_or_raises_the_noise_floor() {
        // A constant offset alone is silence.
        assert_eq!(trim(&vec![0.05; 3 * SECOND]), Trimmed::Silent);
        assert!(rms(&[0.2; FRAME]) < 1e-6);
        assert!((rms(&[0.7, -0.3]) - 0.5).abs() < 1e-6);

        // Quiet speech riding on an offset far louder than itself is kept,
        // and the trimmed audio is the original samples, offset included.
        let offset = 0.05;
        let samples: Vec<f32> = concat(&[noise(2.0, 0.0005), tone(1.0, 0.01), noise(2.0, 0.0005)])
            .into_iter()
            .map(|sample| sample + offset)
            .collect();
        let Trimmed::Speech(trimmed) = trim(&samples) else {
            panic!("speech over a DC offset expected");
        };
        assert!(
            (seconds(&trimmed) - 1.4).abs() < 0.05,
            "{}",
            seconds(&trimmed)
        );
        let start = samples
            .windows(trimmed.len())
            .position(|window| window == trimmed.as_slice());
        assert!(start.is_some(), "trimming never alters samples");
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

    #[test]
    fn quiet_microphone_speech_above_its_own_noise_floor_is_kept() {
        // A built-in microphone records speech at -58 dBFS, under the
        // absolute floor, over room noise at -76 dBFS.
        assert!(amplitude(-58.0) < ABSOLUTE_FLOOR);
        let samples = concat(&[hum(2.0, -76.0), speech_at(1.0, -58.0), hum(2.0, -76.0)]);
        let Trimmed::Speech(trimmed) = trim(&samples) else {
            panic!("quiet speech expected");
        };
        // The speech plus its margins; the surrounding noise is still cut.
        assert!(
            (seconds(&trimmed) - 1.4).abs() < 0.05,
            "{}",
            seconds(&trimmed)
        );
    }

    #[test]
    fn quiet_path_keeps_digital_silence_and_constant_floors_silent() {
        assert_eq!(trim(&vec![0.0; 3 * SECOND]), Trimmed::Silent);
        assert_eq!(trim(&hum(3.0, -60.0)), Trimmed::Silent);
        assert_eq!(trim(&noise(3.0, 0.0005)), Trimmed::Silent);
        // A device that warms up with exact zeros must not set a floor that
        // makes the room noise after it look like speech.
        let samples = concat(&[vec![0.0; SECOND / 2], hum(2.0, -62.0)]);
        assert_eq!(trim(&samples), Trimmed::Silent);
        let samples = concat(&[vec![0.0; SECOND / 2], noise(2.0, 0.0012)]);
        assert_eq!(trim(&samples), Trimmed::Silent);
    }

    #[test]
    fn quiet_path_never_accepts_anything_below_minus_66_dbfs() {
        // 12 dB over a -82 dBFS floor, but under the clamp.
        let too_quiet = concat(&[hum(1.0, -82.0), speech_at(1.0, -70.0), hum(1.0, -82.0)]);
        assert_eq!(trim(&too_quiet), Trimmed::Silent);
        // The same floor with speech at -62 dBFS clears it.
        let audible = concat(&[hum(1.0, -82.0), speech_at(1.0, -62.0), hum(1.0, -82.0)]);
        assert!(matches!(trim(&audible), Trimmed::Speech(_)));
    }

    #[test]
    fn quiet_path_needs_one_sustained_run() {
        // Bursts of 100 ms each, far apart, would add up to enough speech but
        // never form a 180 ms run.
        let mut parts = vec![hum(1.0, -76.0)];
        for _ in 0..4 {
            parts.push(speech_at(frames(5), -58.0));
            parts.push(hum(0.5, -76.0));
        }
        assert_eq!(trim(&concat(&parts)), Trimmed::Silent);

        for (count, kept) in [
            (QUIET_SUSTAINED_FRAMES - 1, false),
            (QUIET_SUSTAINED_FRAMES, true),
        ] {
            let samples = concat(&[
                hum(1.0, -76.0),
                speech_at(frames(count), -58.0),
                hum(1.0, -76.0),
            ]);
            assert_eq!(
                matches!(trim(&samples), Trimmed::Speech(_)),
                kept,
                "{count} frames"
            );
        }
    }

    #[test]
    fn quiet_path_bridges_one_quiet_frame_but_not_two() {
        let loud = [1.0; 5];
        let gap = |frames: usize| vec![0.0; frames];
        let one = [&loud[..], &gap(1), &loud[..]].concat();
        assert_eq!(longest_run(&one, 0.5), 10);
        let two = [&loud[..], &gap(2), &loud[..]].concat();
        assert_eq!(longest_run(&two, 0.5), 5);
        let leading = [&gap(1), &loud[..], &gap(1), &loud[..]].concat();
        assert_eq!(longest_run(&leading, 0.5), 10);
    }
}
