use std::collections::VecDeque;
use std::time::Duration;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler, WindowFunction};

// CoreAudio capture timestamps and annotated-session CGEvent timestamps share
// the boot-time nanosecond clock, so shortcut edges and audio compare directly;
// never apply the Mach timebase to either. RMS/peak diagnostics measure clips
// downstream and never modify or retain audio.
use crate::audio::CaptureInstant;

#[cfg(target_os = "macos")]
use crate::recording_environment::{RecordingEnvironmentController, RecordingEnvironmentSession};

/// Extra audio kept beyond the oldest pending input while the coordinator catches up.
const PENDING_RETENTION_MARGIN: Duration = Duration::from_secs(1);
const TIMELINE_BUFFER_DURATION: Duration = Duration::from_secs(10);
const PRE_ROLL_DURATION: Duration = Duration::from_millis(450);
pub const MINIMUM_HOLD_DURATION: Duration = Duration::from_millis(300);
/// The default `Keep recording after release` tail, used by tests.
#[cfg(test)]
pub const RELEASE_TAIL: Duration = Duration::from_millis(300);
const INITIAL_RECORDING_CAPACITY: Duration = Duration::from_secs(10);
const TRANSCRIPTION_SAMPLE_RATE: u32 = 16_000;
// Covers ordinary event-tap scheduling latency. Finish still detects older edges.
const LIVE_BOUNDARY_GUARD: Duration = Duration::from_millis(150);

pub enum Finish {
    Discard,
    Transcribe(DictationClip),
}

/// The answer to a finishing shortcut edge, decided at the user's edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Release {
    Discard,
    /// Keep recording until the timeline reaches `until`, then finish there.
    Tail {
        until: CaptureInstant,
    },
}

pub struct DictationClip {
    samples: Vec<f32>,
    pub input: Option<crate::microphone::InputDescription>,
    pub live: Option<Box<crate::providers::streaming::PendingLive>>,
}

impl DictationClip {
    pub fn duration_ms(&self) -> u64 {
        self.samples.len() as u64 * 1_000 / u64::from(TRANSCRIPTION_SAMPLE_RATE)
    }

    pub fn into_transcription_samples(self) -> Vec<f32> {
        self.samples
    }

    #[cfg(test)]
    pub fn from_samples(samples: Vec<f32>) -> Self {
        Self {
            samples,
            input: None,
            live: None,
        }
    }
}

pub struct DictationCapture {
    sample_rate: u32,
    ring: VecDeque<f32>,
    ring_captured_through: Option<CaptureInstant>,
    /// Where the last transcribed clip ended. The ring keeps that clip's
    /// audio, so the next capture's pre-roll never reaches before this
    /// instant: a new clip never repeats audio already in the previous one.
    previous_clip_end: Option<CaptureInstant>,
    recording: Option<Recording>,
    #[cfg(target_os = "macos")]
    recording_environment: Option<RecordingEnvironmentController>,
}

struct Recording {
    started_at: CaptureInstant,
    intentional: bool,
    samples: Vec<f32>,
    source_samples: usize,
    source_rate: u32,
    recorded_through: Option<CaptureInstant>,
    input_buffer: Vec<f32>,
    resampler: Option<Fft<f32>>,
    live_decided: bool,
    live: Option<crate::providers::streaming::LiveCapture>,
    /// The user's finishing edge; the discard rule uses it, never the tail.
    released_at: Option<CaptureInstant>,
    /// The end of the release tail. Live audio never passes it.
    seal_at: Option<CaptureInstant>,
    #[cfg(target_os = "macos")]
    environment: RecordingEnvironmentState,
}

#[cfg(target_os = "macos")]
enum RecordingEnvironmentState {
    Disabled,
    Pending(RecordingEnvironmentController),
    Active {
        _session: RecordingEnvironmentSession,
    },
}

#[cfg(target_os = "macos")]
impl RecordingEnvironmentState {
    fn activate(&mut self) {
        if let Self::Pending(controller) = self {
            *self = Self::Active {
                _session: controller.begin(),
            };
        }
    }
}

impl Recording {
    const CHUNK_SIZE: usize = 1024;

    fn new(started_at: CaptureInstant, source_rate: u32) -> Self {
        Self::try_new(started_at, source_rate).expect("microphone sample rate must be resampleable")
    }

    fn try_new(started_at: CaptureInstant, source_rate: u32) -> Result<Self, String> {
        let resampler = (source_rate != TRANSCRIPTION_SAMPLE_RATE)
            .then(|| {
                Fft::new_custom(
                    source_rate as usize,
                    TRANSCRIPTION_SAMPLE_RATE as usize,
                    Self::CHUNK_SIZE,
                    1,
                    1,
                    WindowFunction::BlackmanHarris2,
                    FixedSync::Input,
                )
                .map_err(|error| error.to_string())
            })
            .transpose()?;
        Ok(Self {
            started_at,
            intentional: false,
            samples: Vec::with_capacity(samples_for(
                INITIAL_RECORDING_CAPACITY,
                TRANSCRIPTION_SAMPLE_RATE,
            )),
            source_samples: 0,
            source_rate,
            recorded_through: None,
            input_buffer: Vec::with_capacity(Self::CHUNK_SIZE),
            resampler,
            live_decided: false,
            live: None,
            released_at: None,
            seal_at: None,
            #[cfg(target_os = "macos")]
            environment: RecordingEnvironmentState::Disabled,
        })
    }

    #[cfg(target_os = "macos")]
    fn with_environment(mut self, environment: RecordingEnvironmentState) -> Self {
        self.environment = environment;
        self
    }

    fn push(&mut self, mut samples: &[f32]) {
        self.source_samples += samples.len();
        let Some(resampler) = &mut self.resampler else {
            self.samples.extend_from_slice(samples);
            return;
        };

        while !samples.is_empty() {
            let take = samples
                .len()
                .min(Self::CHUNK_SIZE - self.input_buffer.len());
            self.input_buffer.extend_from_slice(&samples[..take]);
            samples = &samples[take..];
            if self.input_buffer.len() == Self::CHUNK_SIZE {
                self.samples
                    .extend(resample_chunk(resampler, &self.input_buffer, false));
                self.input_buffer.clear();
            }
        }
    }

    fn push_through(&mut self, samples: &[f32], captured_through: CaptureInstant) {
        self.push(samples);
        self.recorded_through = Some(captured_through);
    }

    fn safe_live_len(&self, oldest_pending: Option<CaptureInstant>) -> usize {
        let Some(through) = self.recorded_through else {
            return 0;
        };
        let cutoff = through
            .checked_sub(LIVE_BOUNDARY_GUARD)
            .unwrap_or(CaptureInstant::ZERO);
        let cutoff = oldest_pending.map_or(cutoff, |pending| pending.min(cutoff));
        // The tail is deliberate audio, not a delayed release, but nothing
        // after its end may reach the live session.
        let cutoff = self.seal_at.map_or(cutoff, |seal| seal.min(cutoff));
        // Same source-frame rounding as Finish, including actual pre-roll and
        // delayed-start reconstruction rather than time since the shortcut.
        let kept = self.source_samples.saturating_sub(samples_for(
            through.saturating_duration_since(cutoff),
            self.source_rate,
        ));
        let delay = self.resampler.as_ref().map_or(0, Resampler::output_delay);
        (kept * TRANSCRIPTION_SAMPLE_RATE as usize / self.source_rate as usize)
            .min(self.samples.len().saturating_sub(delay))
    }

    fn finish(mut self, ended_at: Option<CaptureInstant>) -> Vec<f32> {
        if let (Some(ended_at), Some(recorded_through)) = (ended_at, self.recorded_through)
            && let Some(post_release) = recorded_through.checked_duration_since(ended_at)
        {
            self.source_samples = self
                .source_samples
                .saturating_sub(samples_for(post_release, self.source_rate));
        }
        if let Some(resampler) = &mut self.resampler {
            let expected_samples = self.source_samples * TRANSCRIPTION_SAMPLE_RATE as usize
                / self.source_rate as usize;
            if !self.input_buffer.is_empty() {
                self.samples
                    .extend(resample_chunk(resampler, &self.input_buffer, true));
            }
            let output_delay = resampler.output_delay();
            for _ in 0..8 {
                if self.samples.len() >= expected_samples + output_delay {
                    break;
                }
                self.samples.extend(resample_chunk(resampler, &[], true));
            }
            self.samples.drain(..output_delay.min(self.samples.len()));
            self.samples.truncate(expected_samples);
        } else {
            self.samples.truncate(self.source_samples);
        }
        self.samples
    }
}

/// Resamples one mono chunk. A partial chunk is padded with silence, so an
/// empty partial chunk flushes the resampler's delay.
fn resample_chunk(resampler: &mut Fft<f32>, samples: &[f32], partial: bool) -> Vec<f32> {
    let input =
        InterleavedSlice::new(samples, 1, samples.len()).expect("mono chunk fits its slice");
    let indexing = partial.then(|| Indexing {
        partial_len: Some(samples.len()),
        ..Indexing::default()
    });
    resampler
        .process(&input, indexing.as_ref())
        .expect("fixed-rate resampling must succeed")
        .take_data()
}

impl DictationCapture {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            ring: VecDeque::with_capacity(samples_for(TIMELINE_BUFFER_DURATION, sample_rate)),
            ring_captured_through: None,
            previous_clip_end: None,
            recording: None,
            #[cfg(target_os = "macos")]
            recording_environment: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn enable_recording_environment(&mut self, controller: RecordingEnvironmentController) {
        self.recording_environment = Some(controller);
    }

    #[cfg(test)]
    #[cfg(test)]
    pub fn keep_warm(&mut self, samples: &[f32]) {
        self.keep_warm_for(samples, TIMELINE_BUFFER_DURATION);
    }

    fn keep_warm_for(&mut self, samples: &[f32], retention: Duration) {
        let capacity = samples_for(retention, self.sample_rate);
        if samples.len() >= capacity {
            self.ring.clear();
            self.ring.extend(&samples[samples.len() - capacity..]);
            return;
        }
        self.ring.drain(
            ..self
                .ring
                .len()
                .saturating_add(samples.len())
                .saturating_sub(capacity),
        );
        self.ring.extend(samples);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// A released clip recording its tail: accepted work, not a live capture.
    pub fn is_released(&self) -> bool {
        self.recording
            .as_ref()
            .is_some_and(|recording| recording.seal_at.is_some())
    }

    #[cfg(test)]
    pub fn start(&mut self, now: CaptureInstant) {
        self.start_with_pre_roll(now, PRE_ROLL_DURATION, false);
    }

    pub fn start_at(&mut self, occurred_at: CaptureInstant) {
        self.start_from_timeline(occurred_at, PRE_ROLL_DURATION, false)
    }

    fn start_from_timeline(
        &mut self,
        occurred_at: CaptureInstant,
        pre_roll: Duration,
        intentional: bool,
    ) {
        let delayed_audio = self
            .ring_captured_through
            .and_then(|through| through.checked_duration_since(occurred_at))
            .unwrap_or_default();
        let mut from_ring = pre_roll.saturating_add(delayed_audio);
        // A press during (or just after) the previous clip's end would
        // otherwise take that clip's last audio, its tail included, again.
        if let (Some(through), Some(previous_end)) =
            (self.ring_captured_through, self.previous_clip_end)
        {
            from_ring = from_ring.min(through.saturating_duration_since(previous_end));
        }
        self.start_with_pre_roll(occurred_at, from_ring, intentional);
        if let (Some(recording), Some(through)) = (&mut self.recording, self.ring_captured_through)
        {
            recording.recorded_through = Some(through);
        }
    }

    fn start_with_pre_roll(&mut self, now: CaptureInstant, duration: Duration, intentional: bool) {
        #[cfg(target_os = "macos")]
        let environment = match &self.recording_environment {
            Some(controller) if intentional => RecordingEnvironmentState::Active {
                _session: controller.begin(),
            },
            Some(controller) => RecordingEnvironmentState::Pending(controller.clone()),
            None => RecordingEnvironmentState::Disabled,
        };
        #[cfg(not(target_os = "macos"))]
        let _ = intentional;
        let pre_roll = samples_for(duration, self.sample_rate).min(self.ring.len());
        let recording = Recording::new(now, self.sample_rate);
        #[cfg(target_os = "macos")]
        let mut recording = recording.with_environment(environment);
        #[cfg(not(target_os = "macos"))]
        let mut recording = recording;
        recording.intentional = intentional;
        let skip = self.ring.len() - pre_roll;
        let (front, back) = self.ring.as_slices();
        if skip < front.len() {
            recording.push(&front[skip..]);
            recording.push(back);
        } else {
            recording.push(&back[skip - front.len()..]);
        }
        self.recording = Some(recording);
    }

    #[cfg(test)]
    pub fn push(&mut self, samples: &[f32], now: CaptureInstant) -> bool {
        let became_intentional = self.become_intentional(now);
        if let Some(recording) = &mut self.recording {
            recording.push(samples);
        }
        became_intentional
    }

    #[cfg(test)]
    pub fn ingest(&mut self, samples: &[f32], captured_through: CaptureInstant) {
        self.ingest_with_pending(samples, captured_through, None)
    }

    pub fn ingest_with_pending(
        &mut self,
        samples: &[f32],
        captured_through: CaptureInstant,
        oldest_pending: Option<CaptureInstant>,
    ) {
        if let Some(recording) = &mut self.recording {
            recording.push_through(samples, captured_through);
        }
        self.publish_live_prefix(oldest_pending);
        let retention = oldest_pending
            .map(|pending| {
                captured_through
                    .saturating_duration_since(pending)
                    .saturating_add(PENDING_RETENTION_MARGIN)
                    .max(TIMELINE_BUFFER_DURATION)
            })
            .unwrap_or(TIMELINE_BUFFER_DURATION);
        self.keep_warm_for(samples, retention);
        self.ring_captured_through = Some(captured_through);
    }

    /// Called by the audio owner with the currently observed input barrier.
    pub fn publish_live_prefix(&mut self, oldest_pending: Option<CaptureInstant>) {
        let Some(recording) = &mut self.recording else {
            return;
        };
        if !recording.intentional {
            return;
        }
        if !recording.live_decided {
            recording.live_decided = true;
            recording.live = crate::providers::streaming::LiveCapture::start();
            if recording.live.is_none() {
                crate::providers::batch::prewarm();
            }
        }
        let length = recording.safe_live_len(oldest_pending);
        let delay = recording
            .resampler
            .as_ref()
            .map_or(0, Resampler::output_delay);
        if let Some(live) = &mut recording.live {
            let start = delay.min(recording.samples.len());
            live.push_prefix(&recording.samples[start..start + length]);
        }
    }

    pub fn become_intentional(&mut self, now: CaptureInstant) -> bool {
        let Some(recording) = &mut self.recording else {
            return false;
        };
        let became_intentional = !recording.intentional
            && now.duration_since(recording.started_at) >= MINIMUM_HOLD_DURATION;
        recording.intentional |= became_intentional;
        #[cfg(target_os = "macos")]
        if became_intentional {
            recording.environment.activate();
        }
        became_intentional
    }

    pub fn cancel(&mut self) {
        self.recording = None;
    }

    /// Applies the user's finishing edge. A short, unintentional capture is
    /// discarded at once; otherwise recording continues for `tail`, the
    /// user's `Keep recording after release` setting snapshotted at this edge.
    /// Audio after the edge keeps the last syllable that overlaps the key
    /// release; it never counts toward the hold.
    pub fn release(&mut self, released_at: CaptureInstant, tail: Duration) -> Release {
        let Some(recording) = &mut self.recording else {
            return Release::Discard;
        };
        if let Some(until) = recording.seal_at {
            return Release::Tail { until };
        }
        if !recording.intentional
            && released_at.duration_since(recording.started_at) < MINIMUM_HOLD_DURATION
        {
            self.recording = None;
            return Release::Discard;
        }
        let until = released_at.checked_add(tail).unwrap_or(released_at);
        recording.released_at = Some(released_at);
        recording.seal_at = Some(until);
        Release::Tail { until }
    }

    /// Ends the clip at `now`, or at the end of a release tail if earlier.
    pub fn finish(&mut self, now: CaptureInstant) -> Finish {
        let Some(mut recording) = self.recording.take() else {
            return Finish::Discard;
        };
        let released_at = recording.released_at.unwrap_or(now);
        if !recording.intentional
            && released_at.duration_since(recording.started_at) < MINIMUM_HOLD_DURATION
        {
            return Finish::Discard;
        }

        let ended_at = recording
            .seal_at
            .map_or(now, |seal| seal.min(now).max(released_at));
        let live = recording.live.take();
        let samples = recording.finish(Some(ended_at));
        self.previous_clip_end = Some(ended_at);
        let live = live.map(|live| Box::new(live.finish(&samples)));
        Finish::Transcribe(DictationClip {
            samples,
            input: None,
            live,
        })
    }
}

fn samples_for(duration: Duration, sample_rate: u32) -> usize {
    (duration.as_secs_f64() * f64::from(sample_rate)).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_time() -> CaptureInstant {
        CaptureInstant::from_nanos(60_000_000_000)
    }

    #[test]
    fn quick_option_tap_discards() {
        let mut capture = DictationCapture::new(16_000);
        let start = capture_time();
        capture.start(start);
        let _ = capture.push(&vec![0.5; 1_600], start + Duration::from_millis(100));
        assert!(matches!(
            capture.finish(start + Duration::from_millis(100)),
            Finish::Discard
        ));
    }

    #[test]
    fn modifier_capture_becomes_intentional_only_after_the_hold_threshold() {
        let mut capture = DictationCapture::new(16_000);
        let start = capture_time();
        capture.start(start);

        assert!(!capture.push(&[0.5; 80], start + Duration::from_millis(299)));
        assert!(capture.push(&[0.5; 80], start + MINIMUM_HOLD_DURATION));
        assert!(!capture.push(&[0.5; 80], start + Duration::from_millis(301)));
    }

    #[test]
    fn release_can_commit_the_intentional_transition_between_audio_chunks() {
        let mut capture = DictationCapture::new(16_000);
        let start = capture_time();
        capture.start(start);

        assert!(!capture.push(&[0.5; 80], start + Duration::from_millis(299)));
        assert!(capture.become_intentional(start + MINIMUM_HOLD_DURATION));
        assert!(!capture.become_intentional(start + Duration::from_millis(301)));
    }

    #[test]
    fn delayed_press_reconstructs_audio_from_the_original_boundary() {
        let mut capture = DictationCapture::new(16_000);
        let origin = capture_time();
        for index in 1..=16 {
            capture.ingest(
                &vec![index as f32; 1_600],
                origin + Duration::from_millis(index * 100),
            );
        }

        capture.start_at(origin + Duration::from_secs(1));
        for index in 17..=20 {
            capture.ingest(
                &vec![index as f32; 1_600],
                origin + Duration::from_millis(index * 100),
            );
        }

        let Finish::Transcribe(clip) = capture.finish(origin + Duration::from_millis(1_800)) else {
            panic!("expected transcription")
        };
        assert_eq!(clip.duration_ms(), 1_250);
        assert_eq!(clip.samples.first(), Some(&6.0));
    }

    #[test]
    fn delayed_release_removes_audio_captured_after_the_physical_release() {
        let mut capture = DictationCapture::new(16_000);
        let origin = capture_time();
        capture.ingest(&vec![0.25; 16_000], origin);
        capture.start_at(origin);
        for index in 1..=12 {
            capture.ingest(
                &vec![index as f32; 1_600],
                origin + Duration::from_millis(index * 100),
            );
        }

        let Finish::Transcribe(clip) = capture.finish(origin + Duration::from_secs(1)) else {
            panic!("expected transcription")
        };
        assert_eq!(clip.duration_ms(), 1_450);
        assert!(!clip.samples.contains(&11.0));
        assert!(!clip.samples.contains(&12.0));
    }

    #[test]
    fn minimum_hold_uses_source_event_times_after_a_processing_stall() {
        let origin = capture_time();
        let mut short = DictationCapture::new(16_000);
        short.start_at(origin);
        short.ingest(&vec![0.5; 16_000], origin + Duration::from_secs(2));
        assert!(matches!(
            short.finish(origin + Duration::from_millis(299)),
            Finish::Discard
        ));

        let mut accepted = DictationCapture::new(16_000);
        accepted.start_at(origin);
        accepted.ingest(&vec![0.5; 16_000], origin + Duration::from_secs(2));
        assert!(matches!(
            accepted.finish(origin + MINIMUM_HOLD_DURATION),
            Finish::Transcribe(_)
        ));
    }

    #[test]
    fn pending_input_extends_history_for_an_arbitrary_coordinator_stall() {
        let mut capture = DictationCapture::new(16_000);
        let origin = capture_time();
        capture.ingest(&vec![0.25; 16_000], origin + Duration::from_secs(1));
        let pressed_at = origin + Duration::from_secs(1);
        for second in 2..=31 {
            capture.ingest_with_pending(
                &vec![second as f32; 16_000],
                origin + Duration::from_secs(second),
                Some(pressed_at),
            );
        }

        capture.start_at(pressed_at);
        capture.ingest(&vec![32.0; 16_000], origin + Duration::from_secs(32));
        assert_eq!(capture.ring.len(), 160_000);
        assert_eq!(capture.ring.front(), Some(&23.0));
        assert_eq!(capture.ring.back(), Some(&32.0));

        let Finish::Transcribe(clip) = capture.finish(origin + Duration::from_secs(31)) else {
            panic!("expected transcription")
        };
        assert_eq!(clip.duration_ms(), 30_450);
        assert_eq!(clip.samples.first(), Some(&0.25));
    }

    #[test]
    fn warm_buffer_accepts_chunks_before_reaching_capacity() {
        let mut capture = DictationCapture::new(16_000);
        capture.keep_warm(&[0.25; 480]);
        capture.keep_warm(&[0.5; 480]);
        assert_eq!(capture.ring.len(), 960);
    }

    #[test]
    fn intentional_hold_includes_pre_roll() {
        let mut capture = DictationCapture::new(16_000);
        capture.keep_warm(&vec![0.25; 16_000]);
        let start = capture_time();
        capture.start(start);
        let _ = capture.push(&vec![0.5; 8_000], start + Duration::from_millis(500));
        let Finish::Transcribe(clip) = capture.finish(start + Duration::from_millis(500)) else {
            panic!("expected transcription")
        };
        assert_eq!(clip.duration_ms(), 950);
        let samples = clip.into_transcription_samples();
        assert_eq!(samples.len(), 15_200);
        assert_eq!(samples[0], 0.25);
        assert!(samples.contains(&0.5));
    }

    #[test]
    fn capture_continues_past_sixty_seconds_until_explicitly_finished() {
        let mut capture = DictationCapture::new(16_000);
        let start = capture_time();
        capture.start(start);
        let _ = capture.push(&vec![0.5; 61 * 16_000], start + Duration::from_secs(61));

        let Finish::Transcribe(clip) = capture.finish(start + Duration::from_secs(61)) else {
            panic!("expected transcription")
        };
        assert_eq!(clip.duration_ms(), 61_000);
    }

    #[test]
    fn immutable_live_prefixes_equal_the_finished_clip_at_every_source_rate() {
        for rate in [16_000, 44_100, 48_000] {
            let origin = capture_time();
            let mut recording = Recording::new(origin, rate);
            let samples: Vec<_> = (0..rate as usize * 2)
                .map(|n| (n as f32 * 0.017).sin() * 0.4)
                .collect();
            let mut prefix = Vec::new();
            let release = origin + Duration::from_millis(1_803);
            let mut received = 0;
            for chunk in samples.chunks(317) {
                received += chunk.len();
                let through = origin
                    + Duration::from_nanos(received as u64 * 1_000_000_000 / u64::from(rate));
                recording.push_through(chunk, through);
                let pending = (through >= release).then_some(release);
                let end = recording.safe_live_len(pending);
                let delay = recording
                    .resampler
                    .as_ref()
                    .map_or(0, Resampler::output_delay);
                assert!(end >= prefix.len());
                let begin = delay.min(recording.samples.len());
                let current = &recording.samples[begin..begin + end];
                assert_eq!(&current[..prefix.len()], &prefix);
                prefix.extend_from_slice(&current[prefix.len()..]);
            }
            let final_clip = recording.finish(Some(release));
            assert!(prefix.len() <= final_clip.len());
            assert_eq!(&prefix, &final_clip[..prefix.len()]);
        }
    }

    #[test]
    fn live_boundaries_include_actual_pre_roll_and_detect_late_edges() {
        let at = capture_time();
        let mut capture = DictationCapture::new(16_000);
        capture.ingest(&vec![0.25; 16_000], at);
        capture.start_at(at);
        capture.ingest(&vec![0.5; 16_000], at + Duration::from_secs(1));
        let recording = capture.recording.as_ref().unwrap();
        assert_eq!(recording.safe_live_len(None), 20_800); // 450ms + 1s - 150ms.
        assert_eq!(
            recording.safe_live_len(Some(at + Duration::from_millis(500))),
            15_200
        );
        assert_eq!(
            recording.safe_live_len(Some(at.checked_sub(Duration::from_secs(1)).unwrap())),
            0
        );
        // Merely extracting does not start network, mutate pre-roll, or flush FFT state.
        assert!(!recording.live_decided);
        assert_eq!(recording.source_samples, 23_200);
    }

    /// Ingests `duration` of constant-valued audio after `from` in 10 ms
    /// chunks; returns the timestamp of the last chunk.
    fn ingest_span(
        capture: &mut DictationCapture,
        rate: u32,
        from: CaptureInstant,
        duration: Duration,
        value: f32,
    ) -> CaptureInstant {
        let chunk = samples_for(Duration::from_millis(10), rate);
        let mut through = from;
        for _ in 0..duration.as_millis() / 10 {
            through = through + Duration::from_millis(10);
            capture.ingest(&vec![value; chunk], through);
        }
        through
    }

    #[test]
    fn back_to_back_clips_never_repeat_audio_from_the_previous_clip() {
        let mut capture = DictationCapture::new(16_000);
        let origin = capture_time();
        let pressed = ingest_span(&mut capture, 16_000, origin, Duration::from_secs(1), 0.1);
        capture.start_at(pressed);
        let finished = ingest_span(&mut capture, 16_000, pressed, Duration::from_secs(1), 0.5);
        // Tail Off: the clip ends at the finishing edge.
        let Finish::Transcribe(first) = capture.finish(finished) else {
            panic!("expected the first clip");
        };
        assert_eq!(first.duration_ms(), 1_450);

        // A press 100 ms later takes only the audio after the first clip.
        let again = ingest_span(
            &mut capture,
            16_000,
            finished,
            Duration::from_millis(100),
            0.25,
        );
        capture.start_at(again);
        let end = ingest_span(&mut capture, 16_000, again, Duration::from_secs(1), 0.75);
        let Finish::Transcribe(second) = capture.finish(end) else {
            panic!("expected the second clip");
        };
        assert_eq!(second.duration_ms(), 1_100);
        let samples = second.into_transcription_samples();
        assert!(samples.iter().all(|sample| *sample != 0.5));
        assert_eq!(samples[0], 0.25);

        // Once the pre-roll window has passed, the full pre-roll returns.
        let later = ingest_span(&mut capture, 16_000, end, Duration::from_secs(1), 0.25);
        capture.start_at(later);
        let last = ingest_span(&mut capture, 16_000, later, Duration::from_secs(1), 0.75);
        let Finish::Transcribe(third) = capture.finish(last) else {
            panic!("expected the third clip");
        };
        assert_eq!(third.duration_ms(), 1_450);
    }

    #[test]
    fn release_tail_adds_exactly_three_hundred_ms_at_every_source_rate() {
        for rate in [16_000, 44_100, 48_000] {
            let origin = capture_time();
            let mut capture = DictationCapture::new(rate);
            let pressed = ingest_span(&mut capture, rate, origin, Duration::from_secs(1), 0.25);
            capture.start_at(pressed);
            let released = pressed + Duration::from_secs(1);
            ingest_span(&mut capture, rate, pressed, Duration::from_secs(1), 0.5);
            assert_eq!(
                capture.release(released, RELEASE_TAIL),
                Release::Tail {
                    until: released + RELEASE_TAIL
                }
            );
            // A delayed seal still trims audio after the tail, through the
            // resampler's delay, to the exact source-frame boundary.
            ingest_span(
                &mut capture,
                rate,
                released,
                Duration::from_millis(700),
                0.75,
            );
            let Finish::Transcribe(clip) = capture.finish(released + Duration::from_millis(700))
            else {
                panic!("a released intentional hold must transcribe");
            };
            // 450 ms pre-roll + 1 s hold + 300 ms tail.
            assert_eq!(clip.samples.len(), 28_000, "{rate} Hz");
            assert_eq!(clip.duration_ms(), 1_750);
            if rate == TRANSCRIPTION_SAMPLE_RATE {
                assert_eq!(clip.samples[..7_200], [0.25; 7_200]);
                assert_eq!(clip.samples[7_200..23_200], [0.5; 16_000]);
                assert_eq!(clip.samples[23_200..], [0.75; 4_800]);
            }
        }
    }

    #[test]
    fn release_discard_uses_the_hold_and_never_the_tail() {
        let origin = capture_time();
        let mut short = DictationCapture::new(16_000);
        short.start_at(origin);
        ingest_span(&mut short, 16_000, origin, Duration::from_secs(1), 0.5);
        assert_eq!(
            short.release(origin + Duration::from_millis(299), RELEASE_TAIL),
            Release::Discard
        );
        assert!(!short.is_recording());

        let mut held = DictationCapture::new(16_000);
        held.start_at(origin);
        let released = origin + MINIMUM_HOLD_DURATION;
        assert_eq!(
            held.release(released, RELEASE_TAIL),
            Release::Tail {
                until: released + RELEASE_TAIL
            }
        );
        // A repeated edge keeps the first tail instead of extending it.
        assert_eq!(
            held.release(released + Duration::from_millis(100), RELEASE_TAIL),
            Release::Tail {
                until: released + RELEASE_TAIL
            }
        );
        ingest_span(&mut held, 16_000, origin, Duration::from_secs(1), 0.5);
        let Finish::Transcribe(clip) = held.finish(released + RELEASE_TAIL) else {
            panic!("a 300 ms hold transcribes even though the tail ends later");
        };
        assert_eq!(clip.duration_ms(), 600);

        // An unreleased finish keeps the original rule at its own edge.
        let mut direct = DictationCapture::new(16_000);
        direct.start_at(origin);
        assert!(matches!(
            direct.finish(origin + Duration::from_millis(299)),
            Finish::Discard
        ));
    }

    #[test]
    fn a_new_press_during_the_tail_ends_the_clip_at_that_press() {
        let origin = capture_time();
        let mut capture = DictationCapture::new(16_000);
        capture.start_at(origin);
        let released = origin + Duration::from_secs(1);
        assert!(matches!(
            capture.release(released, RELEASE_TAIL),
            Release::Tail { .. }
        ));
        ingest_span(&mut capture, 16_000, origin, Duration::from_secs(2), 0.5);
        let Finish::Transcribe(clip) = capture.finish(released + Duration::from_millis(120)) else {
            panic!("expected transcription")
        };
        assert_eq!(clip.duration_ms(), 1_120);
        assert!(!capture.is_recording());
    }

    #[test]
    fn live_prefix_streams_the_tail_and_never_passes_its_end() {
        for rate in [16_000, 44_100, 48_000] {
            let origin = capture_time();
            let mut recording = Recording::new(origin, rate);
            let samples: Vec<_> = (0..rate as usize * 3)
                .map(|n| (n as f32 * 0.013).sin() * 0.4)
                .collect();
            let released = origin + Duration::from_millis(1_503);
            let until = released + RELEASE_TAIL;
            let mut prefix = Vec::new();
            let mut received = 0;
            for chunk in samples.chunks(331) {
                received += chunk.len();
                let through = origin
                    + Duration::from_nanos(received as u64 * 1_000_000_000 / u64::from(rate));
                recording.push_through(chunk, through);
                if through >= released && recording.seal_at.is_none() {
                    recording.released_at = Some(released);
                    recording.seal_at = Some(until);
                }
                // The release edge has been acknowledged; nothing is pending.
                let end = recording.safe_live_len(None);
                let delay = recording
                    .resampler
                    .as_ref()
                    .map_or(0, Resampler::output_delay);
                assert!(end >= prefix.len());
                let begin = delay.min(recording.samples.len());
                let current = &recording.samples[begin..begin + end];
                assert_eq!(&current[..prefix.len()], &prefix);
                prefix.extend_from_slice(&current[prefix.len()..]);
            }
            let final_clip = recording.finish(Some(until));
            // Past the release edge but within the sealed clip,
            // so finishing never invalidates the live transcript.
            let released_len =
                samples_for(Duration::from_millis(1_503), rate) * 16_000 / rate as usize;
            assert!(prefix.len() > released_len, "{rate} Hz");
            assert!(prefix.len() <= final_clip.len(), "{rate} Hz");
            assert_eq!(&prefix, &final_clip[..prefix.len()]);
            if rate == TRANSCRIPTION_SAMPLE_RATE {
                assert_eq!(prefix.len(), final_clip.len());
            }
        }
    }

    #[test]
    fn resampling_flushes_the_end_of_the_recording() {
        for (sample_rate, input_len) in [
            (44_100, 1_337),
            (44_100, 4_410),
            (48_000, 1_024),
            (48_000, 4_800),
        ] {
            let mut recording = Recording::new(capture_time(), sample_rate);
            recording.push(&vec![0.5; input_len]);
            let samples = recording.finish(None);
            let expected = input_len * TRANSCRIPTION_SAMPLE_RATE as usize / sample_rate as usize;

            assert_eq!(samples.len(), expected);
            assert!(samples.last().is_some_and(|sample| *sample > 0.25));
        }
    }
}
