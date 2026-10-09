use std::io::Cursor;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use rodio::buffer::SamplesBuffer;
use rodio::{Decoder, DeviceSinkBuilder, Source};

use crate::interaction_settings::SoundVolumes;
use crate::start_cue::StartCue;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tone {
    Error,
    DictationStart,
    DictationStop,
    Cancel,
    /// The selected start cue, played on request from Settings. It stays
    /// audible while sounds are off so the choice can still be heard.
    StartPreview,
}

/// The preview level when the start sound is off: the default start volume.
const PREVIEW_FALLBACK_VOLUME: f32 = 0.75;

static DICTATION_PLAYER: OnceLock<SyncSender<Tone>> = OnceLock::new();
static LOADER_STARTED: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(true);
static START_VOLUME: AtomicU32 = AtomicU32::new(0.75_f32.to_bits());
static STOP_VOLUME: AtomicU32 = AtomicU32::new(0.5_f32.to_bits());
static ERROR_CANCEL_VOLUME: AtomicU32 = AtomicU32::new(0.5_f32.to_bits());
static START_CUE: AtomicUsize = AtomicUsize::new(0);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn set_volumes(volumes: SoundVolumes) {
    let volumes = volumes.normalized();
    START_VOLUME.store(volumes.start.to_bits(), Ordering::Relaxed);
    STOP_VOLUME.store(volumes.stop.to_bits(), Ordering::Relaxed);
    ERROR_CANCEL_VOLUME.store(volumes.error_cancel.to_bits(), Ordering::Relaxed);
}

pub fn set_start_cue(cue: StartCue) {
    START_CUE.store(cue.index(), Ordering::Relaxed);
}

fn start_cue() -> StartCue {
    StartCue::ALL
        .get(START_CUE.load(Ordering::Relaxed))
        .copied()
        .unwrap_or_default()
}

fn volumes() -> SoundVolumes {
    SoundVolumes {
        start: f32::from_bits(START_VOLUME.load(Ordering::Relaxed)),
        stop: f32::from_bits(STOP_VOLUME.load(Ordering::Relaxed)),
        error_cancel: f32::from_bits(ERROR_CANCEL_VOLUME.load(Ordering::Relaxed)),
    }
}

fn playback_volume(tone: Tone, volumes: SoundVolumes) -> f32 {
    match tone {
        Tone::DictationStart => volumes.start,
        Tone::StartPreview if volumes.start > 0.0 => volumes.start,
        Tone::StartPreview => PREVIEW_FALLBACK_VOLUME,
        Tone::DictationStop => volumes.stop,
        Tone::Error | Tone::Cancel => volumes.error_cancel,
    }
}

fn sounds_enabled() -> bool {
    let volumes = volumes();
    ENABLED.load(Ordering::Relaxed)
        && (volumes.start > 0.0 || volumes.stop > 0.0 || volumes.error_cancel > 0.0)
}

fn tone_volume(tone: Tone) -> f32 {
    if tone == Tone::StartPreview {
        let volumes = if ENABLED.load(Ordering::Relaxed) {
            volumes()
        } else {
            SoundVolumes::from_legacy(0.0)
        };
        playback_volume(tone, volumes)
    } else if ENABLED.load(Ordering::Relaxed) {
        playback_volume(tone, volumes())
    } else {
        0.0
    }
}

/// How long the output device stays open after the last tone finishes. Holding
/// the default output open keeps macOS from idle-sleeping, so an idle app must
/// release it. A tone implies recent user activity, so a grace shorter than any
/// idle-sleep timer costs nothing while sparing bursts of dictation the cold
/// device reopen.
const IDLE_RELEASE_GRACE: Duration = Duration::from_secs(5 * 60);
const OPEN_RETRY_BACKOFF: Duration = Duration::from_secs(2);
/// How soon an open device notices that sounds turned off or the grace ended.
const OUTPUT_OBSERVATION_INTERVAL: Duration = Duration::from_millis(250);

// Only the playback worker owns a device. It opens lazily for a tone, stays
// open through the idle grace, and drops within one observation interval once
// sounds turn off or the grace expires. Opening never blocks capture or UI.
struct FeedbackOutput<S> {
    sink: Option<S>,
    retry_at: Instant,
    playing_until: Option<Instant>,
}

impl<S> FeedbackOutput<S> {
    fn new() -> Self {
        Self {
            sink: None,
            retry_at: Instant::now(),
            playing_until: None,
        }
    }

    /// Opens the device for a tone unless a recent failure is still backing off.
    fn open_for_tone(&mut self, now: Instant, open: impl FnOnce() -> Result<S>) -> Result<()> {
        if self.sink.is_none() && now >= self.retry_at {
            self.retry_at = now + OPEN_RETRY_BACKOFF;
            self.sink = Some(open()?);
        }
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.sink.is_some()
    }

    fn mark_playing(&mut self, now: Instant, duration: Duration) {
        let until = now + duration;
        self.playing_until = Some(self.playing_until.unwrap_or(until).max(until));
    }

    /// Releases the device when sounds are off or nothing has played within the
    /// idle grace. A released device reopens immediately for the next tone.
    fn release_when_idle(&mut self, enabled: bool, now: Instant) {
        let idle = self
            .playing_until
            .is_none_or(|until| now >= until + IDLE_RELEASE_GRACE);
        if !enabled || idle {
            self.playing_until = None;
            if self.sink.take().is_some() {
                self.retry_at = now;
            }
        }
    }
}

/// Maps the admission wait outcome to an error. A timeout does not stop the
/// loader thread: the audio stack may simply be cold, and the loader keeps
/// running so tones recover once the default output finally opens.
fn admission_error(outcome: Result<Result<()>, mpsc::RecvTimeoutError>) -> Result<()> {
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(eyre!("timed out preloading feedback audio")),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(eyre!("feedback audio loader exited before initializing"))
        }
    }
}

fn publish_player(
    player: &OnceLock<SyncSender<Tone>>,
    sender: SyncSender<Tone>,
    ready: SyncSender<Result<()>>,
    outcome: Result<()>,
) {
    let _ = player.set(sender);
    let _ = ready.send(outcome);
}

pub fn preload() -> Result<()> {
    if DICTATION_PLAYER.get().is_some() {
        return Ok(());
    }
    if LOADER_STARTED.swap(true, Ordering::AcqRel) {
        // A loader is already running from an earlier admission; it registers
        // the player itself, so report the same slow-audio outcome.
        return Err(eyre!("feedback audio is still initializing"));
    }
    let (sender, receiver) = mpsc::sync_channel(8);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = (|| -> Result<_> {
            let classic = decode(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/audio/startRecording.mp3"
            )))?;
            let start = start_cues(classic);
            let stop = decode(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/audio/stopRecording.mp3"
            )))?;
            let cancel = decode(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/audio/cancel.mp3"
            )))?;
            Ok((start, stop, cancel))
        })();
        let Ok((start, stop, cancel)) = result else {
            let _ = ready_sender.send(result.map(|_| ()));
            LOADER_STARTED.store(false, Ordering::Release);
            return;
        };
        let mut output = FeedbackOutput::new();
        let open = || {
            let mut sink = DeviceSinkBuilder::open_default_sink()
                .wrap_err("could not open the audio output")?;
            sink.log_on_drop(false);
            Ok(sink)
        };
        // The device opens for the first tone, not at startup, so an idle app
        // holds no output stream. Publish before any tone arrives.
        publish_player(&DICTATION_PLAYER, sender, ready_sender, Ok(()));
        // A preview may play while sounds are off; keep the output until it ends.
        let mut previewing_until: Option<Instant> = None;
        loop {
            // Only a held device needs the periodic release check. Without
            // one, sleep until the next tone instead of waking the process.
            let tone = if output.is_open() {
                match receiver.recv_timeout(OUTPUT_OBSERVATION_INTERVAL) {
                    Ok(tone) => Some(tone),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match receiver.recv() {
                    Ok(tone) => Some(tone),
                    Err(mpsc::RecvError) => break,
                }
            };
            let now = Instant::now();
            let enabled = sounds_enabled();
            let previewing = previewing_until.is_some_and(|until| now < until);
            output.release_when_idle(enabled || previewing, now);
            let audible = |tone: Tone| enabled || tone == Tone::StartPreview;
            let Some(tone) = tone.filter(|tone| audible(*tone) && tone_volume(*tone) > 0.0) else {
                continue;
            };
            let sound = match tone {
                Tone::DictationStart | Tone::StartPreview => &start[start_cue().index()],
                Tone::DictationStop => &stop,
                Tone::Cancel => &cancel,
                Tone::Error => continue,
            };
            if let Err(error) = output.open_for_tone(Instant::now(), open) {
                tracing::warn!(%error, "recording audio output unavailable; retrying");
            }
            // Opening may have been slow; recheck the latest preference before
            // retaining the output or playing the queued sound.
            if tone != Tone::StartPreview && !sounds_enabled() {
                output.release_when_idle(false, Instant::now());
                continue;
            }
            let volume = tone_volume(tone);
            if volume <= 0.0 {
                continue;
            }
            let Some(sink) = output.sink.as_ref() else {
                continue;
            };
            sink.mixer().add(sound.clone().amplify(volume));
            let duration = sound.total_duration().unwrap_or(Duration::ZERO);
            output.mark_playing(Instant::now(), duration);
            if tone == Tone::StartPreview {
                previewing_until = Some(Instant::now() + duration);
            }
        }
    });
    let outcome = ready_receiver.recv_timeout(Duration::from_secs(2));
    admission_error(outcome)?;
    Ok(())
}

pub fn play(tone: Tone) {
    if tone_volume(tone) <= 0.0 {
        return;
    }
    match tone {
        Tone::DictationStart | Tone::DictationStop | Tone::Cancel | Tone::StartPreview => {
            enqueue(DICTATION_PLAYER.get(), tone);
        }
        Tone::Error => play_system_sound(tone),
    }
}

/// Plays the selected start cue for Settings, loading the player off the UI
/// thread first when dictation has not (previews never start a listener).
pub fn preview_start_cue() {
    thread::spawn(|| {
        let _ = preload();
        play(Tone::StartPreview);
    });
}

fn enqueue(player: Option<&SyncSender<Tone>>, tone: Tone) {
    if let Some(player) = player {
        let _ = player.try_send(tone);
    }
}

fn play_system_sound(tone: Tone) {
    let sound = match tone {
        Tone::Error => "Basso",
        Tone::DictationStart | Tone::DictationStop | Tone::Cancel | Tone::StartPreview => return,
    };
    let volume = tone_volume(tone);
    if volume <= 0.0 {
        return;
    }
    let volume = volume.to_string();
    let child = Command::new("/usr/bin/afplay")
        .args([
            "-v",
            &volume,
            &format!("/System/Library/Sounds/{sound}.aiff"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Renders every start cue once, in [`StartCue::ALL`] order, so switching the
/// preference never synthesizes on the playback path.
fn start_cues(classic: SamplesBuffer) -> Vec<SamplesBuffer> {
    StartCue::ALL
        .map(|cue| match cue.samples() {
            Some(samples) => SamplesBuffer::new(
                rodio::ChannelCount::MIN,
                rodio::SampleRate::new(crate::start_cue::SAMPLE_RATE)
                    .expect("the cue sample rate is nonzero"),
                samples,
            ),
            None => classic.clone(),
        })
        .into()
}

fn decode(bytes: &'static [u8]) -> Result<SamplesBuffer> {
    let decoder = Decoder::new(Cursor::new(bytes)).wrap_err("could not decode feedback audio")?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    Ok(SamplesBuffer::new(
        channels,
        sample_rate,
        decoder.collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_cue_is_emphasized_while_mute_and_other_tones_keep_their_levels() {
        for tone in [Tone::DictationStart, Tone::DictationStop, Tone::Cancel] {
            assert_eq!(playback_volume(tone, SoundVolumes::from_legacy(0.0)), 0.0);
            assert!(playback_volume(tone, SoundVolumes::from_legacy(1.0)) <= 1.0);
        }
        assert_eq!(
            playback_volume(Tone::DictationStart, SoundVolumes::default()),
            0.75
        );
        assert_eq!(
            playback_volume(Tone::DictationStop, SoundVolumes::default()),
            0.5
        );
        assert_eq!(playback_volume(Tone::Cancel, SoundVolumes::default()), 0.5);
    }

    #[test]
    fn each_volume_controls_only_its_tones_without_an_extra_start_multiplier() {
        let volumes = SoundVolumes {
            start: 0.2,
            stop: 0.6,
            error_cancel: 0.0,
        };
        assert_eq!(playback_volume(Tone::DictationStart, volumes), 0.2);
        assert_eq!(playback_volume(Tone::DictationStop, volumes), 0.6);
        assert_eq!(playback_volume(Tone::Cancel, volumes), 0.0);
        assert_eq!(playback_volume(Tone::Error, volumes), 0.0);

        let volumes = SoundVolumes {
            start: 0.0,
            error_cancel: 0.4,
            ..volumes
        };
        assert_eq!(playback_volume(Tone::DictationStart, volumes), 0.0);
        assert_eq!(playback_volume(Tone::DictationStop, volumes), 0.6);
        assert_eq!(playback_volume(Tone::Cancel, volumes), 0.4);
        assert_eq!(playback_volume(Tone::Error, volumes), 0.4);
    }

    #[test]
    fn louder_bundled_start_cue_stays_below_full_scale() {
        let sound = decode(include_bytes!("../resources/audio/startRecording.mp3")).unwrap();
        let peak = |samples: Vec<f32>| samples.into_iter().map(f32::abs).fold(0.0, f32::max);
        let before = peak(sound.clone().amplify(0.5).collect());
        let after = peak(
            sound
                .clone()
                .amplify(playback_volume(
                    Tone::DictationStart,
                    SoundVolumes::default(),
                ))
                .collect(),
        );
        let maximum = peak(
            sound
                .amplify(playback_volume(
                    Tone::DictationStart,
                    SoundVolumes::from_legacy(1.0),
                ))
                .collect(),
        );
        assert!(before > 0.0);
        assert!(after > before * 1.4);
        assert!(after <= 1.0);
        assert!(maximum <= 1.0);
    }

    #[test]
    fn preview_plays_the_chosen_level_or_the_default_when_start_is_off() {
        let volumes = SoundVolumes {
            start: 0.25,
            stop: 0.5,
            error_cancel: 0.5,
        };
        assert_eq!(playback_volume(Tone::StartPreview, volumes), 0.25);
        let silent = SoundVolumes::from_legacy(0.0);
        assert_eq!(playback_volume(Tone::DictationStart, silent), 0.0);
        assert_eq!(
            playback_volume(Tone::StartPreview, silent),
            PREVIEW_FALLBACK_VOLUME
        );
    }

    #[test]
    fn every_start_cue_loads_in_order_and_stays_below_full_scale() {
        let classic = decode(include_bytes!("../resources/audio/startRecording.mp3")).unwrap();
        let cues = start_cues(classic.clone());
        assert_eq!(cues.len(), StartCue::ALL.len());
        let loudest = playback_volume(Tone::DictationStart, SoundVolumes::from_legacy(1.0));
        for (cue, sound) in StartCue::ALL.into_iter().zip(&cues) {
            let peak = sound
                .clone()
                .amplify(loudest)
                .map(f32::abs)
                .fold(0.0, f32::max);
            assert!(peak > 0.0 && peak <= 1.0, "{cue:?} peaks at {peak}");
        }
        assert!(cues[StartCue::Classic.index()].clone().eq(classic));
    }

    #[test]
    fn timed_out_admission_still_publishes_a_usable_player() {
        let player = OnceLock::new();
        let (sender, receiver) = mpsc::sync_channel(8);
        let (ready, waiting) = mpsc::sync_channel(1);
        assert!(admission_error(waiting.recv_timeout(Duration::ZERO)).is_err());
        drop(waiting);
        publish_player(&player, sender, ready, Ok(()));
        enqueue(player.get(), Tone::DictationStart);
        assert_eq!(receiver.try_recv().unwrap(), Tone::DictationStart);
    }

    #[test]
    fn output_opens_for_a_tone_and_releases_after_the_idle_grace() {
        use std::cell::Cell;
        struct Sink<'a>(&'a Cell<usize>);
        impl Drop for Sink<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        let live = Cell::new(0);
        let opens = Cell::new(0);
        let open = || {
            live.set(live.get() + 1);
            opens.set(opens.get() + 1);
            Ok(Sink(&live))
        };
        let mut output = FeedbackOutput::new();
        let now = Instant::now();
        let tone = Duration::from_secs(1);

        // Idle observation without a tone never opens the device.
        output.release_when_idle(true, now);
        assert_eq!(opens.get(), 0);

        output.open_for_tone(now, open).unwrap();
        output.mark_playing(now, tone);
        output.open_for_tone(now, open).unwrap();
        assert_eq!((opens.get(), live.get()), (1, 1));

        // Retained while playing and through the grace, released afterwards.
        output.release_when_idle(true, now + tone);
        output.release_when_idle(true, now + tone + IDLE_RELEASE_GRACE / 2);
        assert!(output.is_open());
        output.release_when_idle(true, now + tone + IDLE_RELEASE_GRACE);
        assert!(!output.is_open());
        assert_eq!(live.get(), 0);

        // The next tone reopens immediately, without a failure backoff.
        let later = now + tone + IDLE_RELEASE_GRACE;
        output.open_for_tone(later, open).unwrap();
        assert_eq!((opens.get(), live.get()), (2, 1));

        // Turning sounds off releases even while a tone is nominally playing.
        output.mark_playing(later, tone);
        output.release_when_idle(false, later);
        assert!(!output.is_open());
        drop(output);
        assert_eq!(live.get(), 0);
    }

    #[test]
    fn overlapping_tones_extend_playback_instead_of_truncating_it() {
        let mut output = FeedbackOutput::new();
        let now = Instant::now();
        output.open_for_tone(now, || Ok(())).unwrap();
        output.mark_playing(now, Duration::from_secs(2));
        output.mark_playing(now + Duration::from_millis(300), Duration::from_secs(1));
        assert_eq!(output.playing_until, Some(now + Duration::from_secs(2)));

        output.release_when_idle(true, now + Duration::from_secs(1) + IDLE_RELEASE_GRACE);
        assert!(output.is_open());
        output.release_when_idle(true, now + Duration::from_secs(2) + IDLE_RELEASE_GRACE);
        assert!(!output.is_open());
    }

    #[test]
    fn failed_output_initialization_retries_with_a_bounded_backoff() {
        let mut output = FeedbackOutput::new();
        let now = Instant::now();
        assert!(
            output
                .open_for_tone(now, || Err(eyre!("device unavailable")))
                .is_err()
        );
        output
            .open_for_tone(now + Duration::from_secs(1), || -> Result<()> {
                panic!("must not spin on device failure")
            })
            .unwrap();
        assert!(!output.is_open());
        output
            .open_for_tone(now + OPEN_RETRY_BACKOFF, || Ok(()))
            .unwrap();
        assert!(output.is_open());
    }

    #[test]
    #[ignore = "opens the native default output and inspects pmset assertions"]
    #[cfg(target_os = "macos")]
    fn native_output_release_clears_the_idle_sleep_assertion() {
        fn assertion_for_this_process() -> bool {
            let output = Command::new("/usr/bin/pmset")
                .args(["-g", "assertions"])
                .output()
                .unwrap();
            let report = String::from_utf8_lossy(&output.stdout);
            let marker = format!("Created for PID: {}", std::process::id());
            report
                .split("PreventUserIdleSystemSleep")
                .skip(1)
                .any(|section| section.lines().take(3).any(|line| line.contains(&marker)))
        }

        let mut sink = DeviceSinkBuilder::open_default_sink().unwrap();
        sink.log_on_drop(false);
        thread::sleep(Duration::from_millis(500));
        assert!(
            assertion_for_this_process(),
            "an open output should hold the coreaudiod sleep assertion"
        );
        drop(sink);
        let released = (0..20).any(|_| {
            thread::sleep(Duration::from_millis(250));
            !assertion_for_this_process()
        });
        assert!(released, "releasing the output should clear the assertion");
    }

    #[test]
    fn bundled_recording_sounds_decode_without_an_audio_device() {
        for bytes in [
            include_bytes!("../resources/audio/startRecording.mp3").as_slice(),
            include_bytes!("../resources/audio/stopRecording.mp3").as_slice(),
            include_bytes!("../resources/audio/cancel.mp3").as_slice(),
        ] {
            let mut sound = decode(bytes).unwrap();
            assert!(sound.total_duration().unwrap() > Duration::ZERO);
            assert!(sound.any(|sample| sample.abs() > 0.0));
        }
    }

    #[test]
    fn feedback_admission_never_waits_for_playback() {
        let (sender, receiver) = mpsc::sync_channel(2);
        enqueue(Some(&sender), Tone::DictationStart);
        enqueue(Some(&sender), Tone::DictationStop);
        enqueue(Some(&sender), Tone::Cancel);
        assert_eq!(receiver.try_recv().unwrap(), Tone::DictationStart);
        assert_eq!(receiver.try_recv().unwrap(), Tone::DictationStop);
        assert!(receiver.try_recv().is_err());
        drop(receiver);
        enqueue(Some(&sender), Tone::Cancel);
        enqueue(None, Tone::DictationStart);
    }

    #[test]
    fn timeout_and_loader_exit_report_distinct_errors() {
        // A silent channel models a loader still warming the audio stack; a
        // dropped one models a loader that exited before loading.
        assert_eq!(
            admission_error(Err(mpsc::RecvTimeoutError::Timeout))
                .unwrap_err()
                .to_string(),
            "timed out preloading feedback audio"
        );
        assert_eq!(
            admission_error(Err(mpsc::RecvTimeoutError::Disconnected))
                .unwrap_err()
                .to_string(),
            "feedback audio loader exited before initializing"
        );

        // Real channels exercise the same admission wait the loader drives.
        let (slow_sender, slow_receiver) = mpsc::sync_channel::<Result<()>>(1);
        assert!(admission_error(slow_receiver.recv_timeout(Duration::ZERO)).is_err());
        drop(slow_sender);

        let (done_sender, done_receiver) = mpsc::sync_channel::<Result<()>>(1);
        done_sender.send(Ok(())).unwrap();
        assert!(admission_error(done_receiver.recv_timeout(Duration::ZERO)).is_ok());

        let (failed_sender, failed_receiver) = mpsc::sync_channel::<Result<()>>(1);
        failed_sender
            .send(Err(eyre!("could not open the audio output")))
            .unwrap();
        assert_eq!(
            admission_error(failed_receiver.recv_timeout(Duration::ZERO))
                .unwrap_err()
                .to_string(),
            "could not open the audio output"
        );
    }
}
