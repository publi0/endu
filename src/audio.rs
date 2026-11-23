use std::ops::Add;
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc::Sender;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

use crate::microphone::{
    DevicePreference, InputDescription, preferred_input_indices, resolve_channel,
};
use color_eyre::eyre::{Result, WrapErr, eyre};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SizedSample, Stream, StreamConfig};

mod overloads;

#[cfg(target_os = "macos")]
/// Preferred inputs, tried in order before the macOS default when no microphone
/// is selected in Settings.
const AUTOMATIC_INPUT_DEVICE_PREFERENCES: &[&str] =
    &["Universal Audio Thunderbolt", "Studio Display Microphone"];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CaptureInstant(u64);

impl CaptureInstant {
    pub const ZERO: Self = Self(0);

    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    #[cfg(target_os = "macos")]
    pub fn now() -> Self {
        unsafe extern "C" {
            fn mach_absolute_time() -> u64;
        }
        Self::from_mach_ticks(unsafe { mach_absolute_time() })
    }

    #[cfg(target_os = "macos")]
    pub fn from_mach_ticks(ticks: u64) -> Self {
        let Some((numerator, denominator)) = mach_timebase() else {
            return Self::ZERO;
        };
        Self(scale_mach_ticks(ticks, numerator, denominator))
    }

    pub fn from_stream(instant: cpal::StreamInstant) -> Self {
        Self(u64::try_from(instant.as_nanos()).unwrap_or(u64::MAX))
    }

    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let nanos = u64::try_from(duration.as_nanos()).ok()?;
        self.0.checked_add(nanos).map(Self)
    }

    pub fn checked_sub(self, duration: Duration) -> Option<Self> {
        let nanos = u64::try_from(duration.as_nanos()).ok()?;
        self.0.checked_sub(nanos).map(Self)
    }

    pub fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_nanos)
    }

    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    pub fn duration_since(self, earlier: Self) -> Duration {
        self.saturating_duration_since(earlier)
    }
}

#[cfg(target_os = "macos")]
fn scale_mach_ticks(ticks: u64, numerator: u32, denominator: u32) -> u64 {
    let nanos = u128::from(ticks) * u128::from(numerator) / u128::from(denominator);
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

#[cfg(target_os = "macos")]
fn mach_timebase() -> Option<(u32, u32)> {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    static TIMEBASE: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        let status = unsafe { mach_timebase_info(&mut info) };
        (status == 0 && info.denom != 0).then_some((info.numer, info.denom))
    })
}

impl Add<Duration> for CaptureInstant {
    type Output = Self;

    fn add(self, duration: Duration) -> Self::Output {
        self.checked_add(duration)
            .expect("capture instant overflowed")
    }
}

/// Chunks the capture callback may queue for the audio owner. At CoreAudio's
/// usual 512-frame period at 48 kHz this is about five seconds; the owner
/// drains every few milliseconds, so only a stalled owner fills it. A chunk
/// that does not fit is dropped and reported as a gap, never waited for.
const CHUNK_QUEUE_CAPACITY: usize = 512;

/// A running stream delivers audio every few milliseconds, even silence. One
/// that has delivered nothing for this long has stopped without reporting an
/// error (seen after a burst of overloads) and is reopened. The owner checks
/// it on the turns it already takes while the stream is open.
const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(4);

pub struct AudioInput {
    // Tests can deliver PCM through channels without opening a native microphone.
    _stream: Option<Stream>,
    chunks: Receiver<(Vec<f32>, CaptureInstant)>,
    /// The end of the newest chunk the callback dropped on a full queue, in
    /// capture nanoseconds; zero means none is pending.
    dropped_through: Arc<AtomicU64>,
    /// The chunk right after a dropped one, held back for one turn so the
    /// owner hears of the hole before the audio that follows it.
    after_drop: Option<(Vec<f32>, CaptureInstant)>,
    /// How long the stream may go without delivering audio before it counts
    /// as failed; `None` for test inputs fed by hand.
    stall_after: Option<Duration>,
    /// When the stream opened or last delivered a chunk.
    last_delivery: Instant,
    pub sample_rate: u32,
    pub device_name: String,
    pub description: InputDescription,
    stream_errors: Receiver<String>,
}

pub enum AudioInputEvent {
    Chunk {
        samples: Vec<f32>,
        captured_through: CaptureInstant,
    },
    /// The capture queue was full and dropped audio; the next chunk follows
    /// a hole in the timeline.
    Dropped,
    Timeout,
    StreamFailed(String),
}

type InputOpenResult = (u64, Result<AudioInput, String>);

pub struct RecoveringAudioInput {
    input: Option<AudioInput>,
    device_override: Option<String>,
    selected_device: Option<String>,
    preferences: Vec<DevicePreference>,
    active_revision: u64,
    recovery: MicrophoneRecovery,
    replacement: Option<Receiver<InputOpenResult>>,
    routing_probe: Option<Receiver<Result<DevicePreference, String>>>,
    next_routing_probe: Instant,
    lid_generation: u64,
    /// When the installed stream first delivered audio.
    first_delivery: Option<Instant>,
    /// Whether the installed stream has delivered for
    /// [`STREAM_PROVEN_UPTIME`]. Only a proven stream reopens at once after
    /// it fails; an unproven one waits out the stall backoff.
    proven: bool,
    /// The reopen error last logged, so a microphone that stays gone (an
    /// unplugged headset, a closed lid) logs once, not on every retry.
    retry_warning: Option<String>,
}

pub enum RecoveringAudioInputEvent {
    Chunk {
        samples: Vec<f32>,
        captured_through: CaptureInstant,
    },
    /// The next chunk follows audio the capture queue had to drop.
    Dropped,
    Timeout,
    Interrupted,
    Reopened,
    OpenFailed(String),
}

/// The first wait after a failed open or a stream that stopped delivering.
const MICROPHONE_RETRY_INITIAL: Duration = Duration::from_millis(250);
/// A microphone that will not open waits at most this long between attempts.
const MICROPHONE_RETRY_MAX: Duration = Duration::from_secs(5);
/// A stream that keeps failing before it proves steady delivery waits this
/// long at most: a reopen every few seconds would only keep waking the
/// machine. A dictation press retries at once.
const MICROPHONE_STALL_RETRY_MAX: Duration = Duration::from_secs(60);
/// How long a stream must have been delivering before its failure counts as
/// an accident of a working microphone, reopened at once.
const STREAM_PROVEN_UPTIME: Duration = STREAM_STALL_TIMEOUT.saturating_mul(2);
const ROUTING_PROBE_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Eq, PartialEq)]
enum MicrophoneRecoveryReason {
    SelectionChanged,
    AvailabilityChanged,
    StreamFailed,
}

struct MicrophoneRecovery {
    next_attempt: Option<Instant>,
    delay: Duration,
    /// Whether `delay` is a failing stream's backoff. It grows to
    /// [`MICROPHONE_STALL_RETRY_MAX`] and survives the reopens it schedules,
    /// so a stream that never proves steady delivery cannot start the wait
    /// over on every reopen.
    stream_stall: bool,
    reason: Option<MicrophoneRecoveryReason>,
    target_revision: Option<u64>,
}

impl Default for MicrophoneRecovery {
    fn default() -> Self {
        Self {
            next_attempt: None,
            delay: MICROPHONE_RETRY_INITIAL,
            stream_stall: false,
            reason: None,
            target_revision: None,
        }
    }
}

impl MicrophoneRecovery {
    /// The person chose another microphone: try it now and start over.
    fn request_selection_change(&mut self, revision: u64, now: Instant) {
        if self.target_revision != Some(revision) {
            self.next_attempt = Some(now);
            self.delay = MICROPHONE_RETRY_INITIAL;
            self.stream_stall = false;
            self.target_revision = Some(revision);
            if self.reason != Some(MicrophoneRecoveryReason::StreamFailed) {
                self.reason = Some(MicrophoneRecoveryReason::SelectionChanged);
            }
        }
    }

    /// A proven stream failed, or startup could not open one: reopen at once
    /// and start over, as a working microphone does.
    fn request_stream_recovery(&mut self, now: Instant) {
        if self.reason != Some(MicrophoneRecoveryReason::StreamFailed) {
            self.next_attempt = Some(now);
            self.delay = MICROPHONE_RETRY_INITIAL;
            self.stream_stall = false;
            self.reason = Some(MicrophoneRecoveryReason::StreamFailed);
        }
    }

    /// A stream failed before it proved steady delivery: wait out the stall
    /// backoff, doubling it up to [`MICROPHONE_STALL_RETRY_MAX`].
    fn back_off_stream_recovery(&mut self, now: Instant) {
        self.reason = Some(MicrophoneRecoveryReason::StreamFailed);
        self.stream_stall = true;
        self.failed(now);
    }

    /// A routing change resolves the input again now, but an unproven
    /// stream keeps the backoff its failed predecessors grew, so device churn
    /// cannot restart the fast reopen loop.
    fn request_routing_change(&mut self, revision: u64, now: Instant) {
        let (stream_stall, delay) = (self.stream_stall, self.delay);
        self.request_selection_change(revision, now);
        if stream_stall {
            self.stream_stall = true;
            self.delay = delay;
        }
        if self.reason != Some(MicrophoneRecoveryReason::StreamFailed) {
            self.reason = Some(MicrophoneRecoveryReason::AvailabilityChanged);
        }
    }

    fn blocks_audio(&self) -> bool {
        self.reason == Some(MicrophoneRecoveryReason::StreamFailed)
    }

    fn should_attempt(&self, now: Instant) -> bool {
        self.next_attempt.is_some_and(|next| now >= next)
    }

    fn failed(&mut self, now: Instant) {
        self.next_attempt = Some(now + self.delay);
        let ceiling = if self.stream_stall {
            MICROPHONE_STALL_RETRY_MAX
        } else {
            MICROPHONE_RETRY_MAX
        };
        self.delay = self.delay.saturating_mul(2).min(ceiling);
    }

    /// A replacement stream is installed. A failing stream's backoff is kept
    /// until that stream delivers steadily; anything else starts over.
    fn reopened(&mut self) {
        let stream_stall = self.stream_stall;
        let delay = self.delay;
        self.recovered();
        if stream_stall {
            self.stream_stall = true;
            self.delay = delay;
        }
    }

    /// The installed stream has delivered steadily, so the backoff left by
    /// the failed streams before it is forgotten.
    fn stream_proved(&mut self) {
        if self.stream_stall {
            self.stream_stall = false;
            self.delay = MICROPHONE_RETRY_INITIAL;
        }
    }

    /// Forgets everything: the microphone is closed, or a pending open was
    /// cancelled.
    fn recovered(&mut self) {
        self.next_attempt = None;
        self.delay = MICROPHONE_RETRY_INITIAL;
        self.stream_stall = false;
        self.reason = None;
        self.target_revision = None;
    }

    /// A dictation press asks for a failed microphone now, keeping the
    /// backoff it has grown so repeated presses cannot restore the loop.
    fn retry_now(&mut self, now: Instant) {
        if self.reason == Some(MicrophoneRecoveryReason::StreamFailed) {
            self.next_attempt = Some(now);
        }
    }
}

impl AudioInput {
    #[cfg(test)]
    pub fn channel_for_test() -> (Self, Sender<(Vec<f32>, CaptureInstant)>) {
        let (sender, chunks) = mpsc::channel();
        let (_, stream_errors) = mpsc::channel();
        (
            Self {
                _stream: None,
                chunks,
                dropped_through: Arc::default(),
                after_drop: None,
                stall_after: None,
                last_delivery: Instant::now(),
                stream_errors,
                sample_rate: 48_000,
                device_name: "Test microphone".into(),
                description: InputDescription {
                    device_id: Some("test-input".into()),
                    name: "Test microphone".into(),
                    channels: 1,
                    channel: None,
                    requested_channel: None,
                    fallback_from: None,
                    fallback_lid_closed: false,
                },
            },
            sender,
        )
    }

    fn open_automatic(preferences: &[DevicePreference]) -> Result<Self> {
        try_input_candidates(automatic_devices(preferences)?, |device| {
            let name = device_name(&device).unwrap_or_default();
            Self::open_device(device).wrap_err_with(|| format!("could not open microphone {name}"))
        })
    }

    pub fn open_named(name: &str) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .input_devices()
            .wrap_err("could not enumerate input devices")?
            .find(|device| device_name(device).as_deref() == Some(name))
            .ok_or_else(|| eyre!("microphone is unavailable: {name}"))?;
        Self::open_device(device)
    }

    fn open_device(device: Device) -> Result<Self> {
        let device_name =
            device_name(&device).ok_or_else(|| eyre!("microphone name cannot be read"))?;
        let supported = device
            .default_input_config()
            .wrap_err("could not read the input device configuration")?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let sample_rate = config.sample_rate;
        let channels = usize::from(config.channels);
        if channels == 0 {
            return Err(eyre!("microphone reports no input channels"));
        }
        let description = describe_device(&device, config.channels);
        let selected_channel = description.channel;
        let (sender, chunks) = mpsc::sync_channel(CHUNK_QUEUE_CAPACITY);
        let dropped_through = Arc::new(AtomicU64::new(0));
        let sink = ChunkSink {
            sender,
            dropped_through: dropped_through.clone(),
        };
        let (error_sender, stream_errors) = mpsc::sync_channel(1);

        let stream = match sample_format {
            SampleFormat::F32 => build_stream(
                &device,
                &config,
                channels,
                selected_channel,
                sink,
                error_sender,
                |sample: f32| sample,
            )?,
            SampleFormat::I16 => build_stream(
                &device,
                &config,
                channels,
                selected_channel,
                sink,
                error_sender,
                |sample: i16| sample as f32 / i16::MAX as f32,
            )?,
            SampleFormat::U16 => build_stream(
                &device,
                &config,
                channels,
                selected_channel,
                sink,
                error_sender,
                |sample: u16| sample as f32 / 32768.0 - 1.0,
            )?,
            format => return Err(eyre!("unsupported microphone sample format: {format:?}")),
        };
        stream
            .play()
            .wrap_err("could not start microphone capture")?;

        Ok(Self {
            _stream: Some(stream),
            chunks,
            dropped_through,
            after_drop: None,
            stall_after: Some(STREAM_STALL_TIMEOUT),
            last_delivery: Instant::now(),
            sample_rate,
            device_name,
            description,
            stream_errors,
        })
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> AudioInputEvent {
        // The one place overloads reach the log: the error callback only
        // counts them.
        overloads::report();
        // A stream failure reported before or during the wait outranks any
        // chunk delivered alongside it.
        if let Ok(error) = self.stream_errors.try_recv() {
            return AudioInputEvent::StreamFailed(error);
        }
        if let Some((samples, captured_through)) = self.after_drop.take() {
            return AudioInputEvent::Chunk {
                samples,
                captured_through,
            };
        }
        let received = self.chunks.recv_timeout(timeout);
        if let Ok(error) = self.stream_errors.try_recv() {
            return AudioInputEvent::StreamFailed(error);
        }
        if received.is_ok() {
            self.last_delivery = Instant::now();
        }
        match received {
            Ok((samples, captured_through)) if self.follows_drop(captured_through) => {
                self.after_drop = Some((samples, captured_through));
                AudioInputEvent::Dropped
            }
            Ok((samples, captured_through)) => AudioInputEvent::Chunk {
                samples,
                captured_through,
            },
            Err(RecvTimeoutError::Timeout) if self.stalled(Instant::now()) => {
                AudioInputEvent::StreamFailed("microphone stopped delivering audio".into())
            }
            Err(RecvTimeoutError::Timeout) => AudioInputEvent::Timeout,
            Err(RecvTimeoutError::Disconnected) => {
                AudioInputEvent::StreamFailed("microphone audio channel disconnected".into())
            }
        }
    }

    fn stalled(&self, now: Instant) -> bool {
        self.stall_after
            .is_some_and(|after| now.saturating_duration_since(self.last_delivery) >= after)
    }

    #[cfg(test)]
    fn watch_stalls_for_test(&mut self, after: Duration) {
        self.stall_after = Some(after);
        self.last_delivery = Instant::now();
    }

    /// Whether a chunk ending at `captured_through` is newer than audio the
    /// callback dropped: chunks queued before the drop are older and arrive
    /// first, so the hole is reported exactly where it is in the timeline.
    fn follows_drop(&self, captured_through: CaptureInstant) -> bool {
        let dropped_through = self.dropped_through.load(Ordering::Acquire);
        if dropped_through == 0 || captured_through.as_nanos() <= dropped_through {
            return false;
        }
        // A newer drop since the load keeps its own mark for a later chunk.
        let _ = self.dropped_through.compare_exchange(
            dropped_through,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        true
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        // Stop the stream before the count is taken, so a closing stream's
        // last burst is written here instead of beside the next open's lines.
        self._stream = None;
        overloads::flush();
    }
}

impl RecoveringAudioInput {
    pub fn open(
        device_override: Option<&str>,
        selection_revision: u64,
        selected_device: Option<&str>,
        preferences: &[DevicePreference],
    ) -> Self {
        let mut input = Self::closed(
            device_override,
            selection_revision,
            selected_device,
            preferences,
        );
        let opened = open_configured_input(device_override, selected_device, preferences, true);
        input.finish_initial_open(opened, Instant::now());
        input
    }

    fn finish_initial_open(&mut self, opened: Result<AudioInput>, now: Instant) {
        match opened {
            Ok(opened) => self.install(opened),
            Err(error) => {
                tracing::warn!(%error, "could not open dictation microphone at startup; retrying");
                self.recovery.request_stream_recovery(now);
                self.recovery.failed(now);
            }
        }
    }

    pub fn closed(
        device_override: Option<&str>,
        selection_revision: u64,
        selected_device: Option<&str>,
        preferences: &[DevicePreference],
    ) -> Self {
        Self {
            input: None,
            device_override: device_override.map(str::to_owned),
            selected_device: selected_device.map(str::to_owned),
            preferences: preferences.to_vec(),
            active_revision: selection_revision,
            recovery: MicrophoneRecovery::default(),
            replacement: None,
            routing_probe: None,
            next_routing_probe: Instant::now() + ROUTING_PROBE_INTERVAL,
            lid_generation: crate::clamshell::generation(),
            first_delivery: None,
            proven: false,
            retry_warning: None,
        }
    }

    /// Makes `input` the microphone. It has to prove steady delivery before
    /// a failure of its own reopens at once.
    fn install(&mut self, input: AudioInput) {
        self.input = Some(input);
        self.first_delivery = None;
        self.proven = false;
        self.retry_warning = None;
    }

    fn note_delivery(&mut self, now: Instant) {
        if self.proven {
            return;
        }
        let first = *self.first_delivery.get_or_insert(now);
        if now.saturating_duration_since(first) >= STREAM_PROVEN_UPTIME {
            self.proven = true;
            self.recovery.stream_proved();
        }
    }

    /// A dictation press asks for a failed microphone now instead of
    /// waiting out the backoff; it keeps the backoff for the next failure.
    pub fn retry_now(&mut self) {
        if self.replacement.is_none() {
            self.recovery.retry_now(Instant::now());
        }
    }

    /// How long a failed microphone with nothing open or opening waits
    /// before its next attempt, so its owner can sleep until then instead of
    /// polling. `None` while a stream is open or an open is in flight.
    pub fn retry_wait(&self, now: Instant) -> Option<Duration> {
        if self.input.is_some() || self.replacement.is_some() || !self.recovery.blocks_audio() {
            return None;
        }
        self.recovery
            .next_attempt
            .map(|next| next.saturating_duration_since(now))
    }

    #[cfg(test)]
    pub fn pending_for_test() -> (Self, Sender<InputOpenResult>) {
        let mut input = Self::closed(
            Some("Endu nonexistent microphone for pending open test"),
            0,
            None,
            &[],
        );
        let (sender, receiver) = mpsc::channel();
        input.replacement = Some(receiver);
        (input, sender)
    }

    pub fn request_open(&mut self) {
        if self.input.is_none() && self.replacement.is_none() && !self.is_recovering() {
            self.start_replacement();
        }
    }

    pub fn request_recovery(&mut self) {
        self.recovery.request_stream_recovery(Instant::now());
    }

    pub fn is_recovering(&self) -> bool {
        self.recovery.blocks_audio()
    }

    pub fn close(&mut self) {
        self.input = None;
        self.retry_warning = None;
        self.replacement = None;
        self.routing_probe = None;
        self.next_routing_probe = Instant::now() + ROUTING_PROBE_INTERVAL;
        self.recovery.recovered();
    }

    pub fn is_open(&self) -> bool {
        self.input.is_some()
    }

    pub fn is_opening(&self) -> bool {
        self.replacement.is_some()
    }

    pub fn cancel_open(&mut self) {
        // Cancelling a capture must not cancel the warm microphone's recovery.
        if self.is_recovering() {
            return;
        }
        self.replacement = None;
        if self.input.is_none() {
            self.recovery.recovered();
        }
    }

    pub fn request_selection(
        &mut self,
        revision: u64,
        selected_device: Option<&str>,
        preferences: &[DevicePreference],
    ) {
        if self.device_override.is_some() || revision == self.active_revision {
            return;
        }
        let keeps_explicit_input = selected_device.is_some_and(|selected| {
            self.selected_device.as_deref() == Some(selected)
                && self.description().is_some_and(|current| {
                    current.name == selected
                        && current.fallback_from.is_none()
                        && current.requested_channel
                            == crate::app_settings::microphone_channel(current.device_id.as_deref())
                })
        });
        self.selected_device = selected_device.map(str::to_owned);
        self.preferences = preferences.to_vec();
        self.routing_probe = None;
        self.next_routing_probe = Instant::now() + ROUTING_PROBE_INTERVAL;
        // Automatic priorities do not affect an explicitly selected healthy
        // stream. A channel edit still takes the normal deferred-reopen path.
        if keeps_explicit_input && self.recovery.reason.is_none() && self.replacement.is_none() {
            self.active_revision = revision;
            return;
        }
        if self.input.is_none() && !self.is_recovering() {
            let was_opening = self.replacement.take().is_some();
            self.active_revision = revision;
            self.recovery.recovered();
            if was_opening {
                self.start_replacement();
            }
            return;
        }
        self.recovery
            .request_selection_change(revision, Instant::now());
    }

    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
        capture_idle: bool,
    ) -> RecoveringAudioInputEvent {
        let now = Instant::now();
        // A failed stream is never read again; let go of the device now
        // rather than holding it through a backoff of up to a minute.
        if self.recovery.blocks_audio() {
            self.input = None;
        }
        self.maintain_routing(now, capture_idle);
        if let Some(opened) = self.poll_replacement()
            && let Some(event) = self.apply_replacement(opened, now, capture_idle)
        {
            return event;
        }
        if (capture_idle || self.input.is_none())
            && self.recovery.should_attempt(now)
            && self.replacement.is_none()
        {
            self.start_replacement();
        }
        if self.recovery.blocks_audio() {
            return self.wait_for_replacement(timeout, capture_idle);
        }
        let Some(input) = self.input.as_mut() else {
            return self.wait_for_replacement(timeout, capture_idle);
        };
        match input.recv_timeout(timeout) {
            AudioInputEvent::Chunk {
                samples,
                captured_through,
            } => {
                self.note_delivery(Instant::now());
                RecoveringAudioInputEvent::Chunk {
                    samples,
                    captured_through,
                }
            }
            AudioInputEvent::Dropped => RecoveringAudioInputEvent::Dropped,
            AudioInputEvent::Timeout => RecoveringAudioInputEvent::Timeout,
            AudioInputEvent::StreamFailed(error) => {
                let failed_at = Instant::now();
                if self.proven {
                    tracing::warn!(%error, device = %input.device_name, "microphone stream stopped; reopening it");
                    self.recovery.request_stream_recovery(failed_at);
                } else {
                    // A stream that never delivered steadily is likely to fail
                    // again: back off instead of reopening every few seconds.
                    self.recovery.back_off_stream_recovery(failed_at);
                    tracing::warn!(
                        %error,
                        device = %input.device_name,
                        retry_in_ms = self.recovery.next_attempt.map_or(0, |next| {
                            next.saturating_duration_since(failed_at).as_millis() as u64
                        }),
                        "microphone stream stopped before delivering steadily; backing off"
                    );
                }
                RecoveringAudioInputEvent::Interrupted
            }
        }
    }

    /// Applies a finished open. `None` means nothing to report this turn.
    fn apply_replacement(
        &mut self,
        (opened_revision, result): InputOpenResult,
        now: Instant,
        capture_idle: bool,
    ) -> Option<RecoveringAudioInputEvent> {
        let target_revision = self
            .recovery
            .target_revision
            .unwrap_or(self.active_revision);
        if opened_revision != target_revision {
            self.recovery.next_attempt = Some(now);
            return Some(RecoveringAudioInputEvent::Timeout);
        }
        match result {
            Ok(replacement) if capture_idle || self.input.is_none() => {
                // An available preferred device may fail to open. If the
                // fallback is already active, keep its stream and pre-roll.
                if self.recovery.reason == Some(MicrophoneRecoveryReason::AvailabilityChanged)
                    && self.is_current_device(&DevicePreference {
                        id: replacement.description.device_id.clone(),
                        name: replacement.device_name.clone(),
                    })
                {
                    self.recovery.recovered();
                    return Some(RecoveringAudioInputEvent::Timeout);
                }
                self.install(replacement);
                self.active_revision = opened_revision;
                self.recovery.reopened();
                tracing::info!(device = %self.device_name(), "dictation microphone reopened");
                return Some(RecoveringAudioInputEvent::Reopened);
            }
            Ok(_) => {
                self.recovery.failed(now);
            }
            Err(error) => {
                if self.input.is_none() && !self.is_recovering() {
                    self.recovery.recovered();
                    return Some(RecoveringAudioInputEvent::OpenFailed(error));
                }
                self.recovery.failed(now);
                if self.retry_warning.as_ref() != Some(&error) {
                    tracing::warn!(%error, "could not reopen dictation microphone; retrying");
                    self.retry_warning = Some(error);
                }
            }
        }
        None
    }

    /// Without a stream there is nothing to read, so wait on a pending open
    /// instead of sleeping through it: a cold open reaches the capture as
    /// soon as the device starts, not at the end of the owner's turn.
    fn wait_for_replacement(
        &mut self,
        timeout: Duration,
        capture_idle: bool,
    ) -> RecoveringAudioInputEvent {
        let Some(replacement) = &self.replacement else {
            std::thread::sleep(timeout);
            return RecoveringAudioInputEvent::Timeout;
        };
        match replacement.recv_timeout(timeout) {
            Ok(opened) => {
                self.replacement = None;
                self.apply_replacement(opened, Instant::now(), capture_idle)
                    .unwrap_or(RecoveringAudioInputEvent::Timeout)
            }
            // A stopped worker is reported by the next poll.
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                RecoveringAudioInputEvent::Timeout
            }
        }
    }

    /// Only while [`Self::is_open`]: a failed stream is dropped before its
    /// replacement opens.
    pub fn sample_rate(&self) -> u32 {
        self.input
            .as_ref()
            .expect("microphone sample rate requested while closed")
            .sample_rate
    }

    /// Only while [`Self::is_open`], like [`Self::sample_rate`].
    pub fn device_name(&self) -> &str {
        &self
            .input
            .as_ref()
            .expect("microphone name requested while closed")
            .device_name
    }

    pub fn description(&self) -> Option<&InputDescription> {
        self.input.as_ref().map(|input| &input.description)
    }

    fn start_replacement(&mut self) {
        let revision = self
            .recovery
            .target_revision
            .unwrap_or(self.active_revision);
        let device_override = self.device_override.clone();
        let selected_device = self.selected_device.clone();
        let preferences = self.preferences.clone();
        let allow_fallback = self.input.is_none()
            || self.recovery.blocks_audio()
            || self.recovery.reason == Some(MicrophoneRecoveryReason::AvailabilityChanged);
        // A failed stream lets go of the device before its replacement opens,
        // so the replacement starts cold, as at launch, instead of opening
        // beside a dead stream on the same device. A live stream (a routing
        // or selection change) keeps running until its replacement installs.
        if self.recovery.blocks_audio() {
            self.input = None;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = open_configured_input(
                device_override.as_deref(),
                selected_device.as_deref(),
                &preferences,
                allow_fallback,
            )
            .map_err(|error| error.to_string());
            let _ = sender.send((revision, result));
        });
        self.replacement = Some(receiver);
    }

    fn poll_replacement(&mut self) -> Option<InputOpenResult> {
        let result = match self.replacement.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => (
                self.active_revision,
                Err("microphone replacement worker stopped".into()),
            ),
        };
        self.replacement = None;
        Some(result)
    }

    /// Enumeration runs outside the audio owner. A returned routing decision
    /// is only consumed between clips, and normal generation checks still
    /// govern the replacement stream. CLI overrides are never rerouted.
    fn maintain_routing(&mut self, now: Instant, capture_idle: bool) {
        if !capture_idle
            || self.device_override.is_some()
            || self.input.is_none()
            || self.replacement.is_some()
            || self.recovery.reason.is_some()
        {
            return;
        }
        // Opening or closing the lid turns the internal microphone on or
        // off without changing the device list: resolve again now.
        let lid_generation = crate::clamshell::generation();
        if lid_generation != self.lid_generation {
            self.lid_generation = lid_generation;
            self.routing_probe = None;
            self.next_routing_probe = now;
        }
        if let Some(probe) = &self.routing_probe {
            match probe.try_recv() {
                Ok(Ok(preferred)) => {
                    self.routing_probe = None;
                    if !self.is_current_device(&preferred) {
                        self.recovery
                            .request_routing_change(self.active_revision, now);
                    }
                    return;
                }
                Ok(Err(_)) | Err(TryRecvError::Disconnected) => self.routing_probe = None,
                Err(TryRecvError::Empty) => return,
            }
        }
        if now < self.next_routing_probe {
            return;
        }
        // Channel fixtures must never discover and open a developer's actual
        // microphone, even when a slow test exceeds the polling interval.
        #[cfg(test)]
        if self
            .input
            .as_ref()
            .is_some_and(|input| input._stream.is_none())
        {
            return;
        }
        self.next_routing_probe = now + ROUTING_PROBE_INTERVAL;
        let selected = self.selected_device.clone();
        let preferences = self.preferences.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = input_description(selected.as_deref(), &preferences)
                .map(|description| DevicePreference {
                    id: description.device_id,
                    name: description.name,
                })
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        self.routing_probe = Some(receiver);
    }

    fn is_current_device(&self, device: &DevicePreference) -> bool {
        self.description().is_some_and(|current| {
            device.matches(&DevicePreference {
                id: current.device_id.clone(),
                name: current.name.clone(),
            })
        })
    }
}

fn open_configured_input(
    device_override: Option<&str>,
    selected_device: Option<&str>,
    preferences: &[DevicePreference],
    fallback_from_selected: bool,
) -> Result<AudioInput> {
    let lid_closed =
        device_override.is_none() && selected_device.is_some_and(selected_off_with_lid_closed);
    let (mut input, fallback_from) = open_with_selection(
        device_override,
        selected_device,
        // A switched-off microphone is a known state, not a failed edit.
        fallback_from_selected || lid_closed,
        |name| {
            if lid_closed {
                return Err(eyre!(crate::clamshell::LID_CLOSED_ERROR));
            }
            AudioInput::open_named(name)
        },
        || AudioInput::open_automatic(preferences),
    )?;
    input.description.fallback_lid_closed = lid_closed && fallback_from.is_some();
    input.description.fallback_from = fallback_from;
    Ok(input)
}

/// A warning about one subject, written once until the subject changes or
/// the condition clears.
struct RepeatedWarning(std::sync::Mutex<Option<String>>);

impl RepeatedWarning {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    /// Whether `subject` was not the last one warned about; it is from now.
    fn is_new(&self, subject: &str) -> bool {
        let mut last = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if last.as_deref() == Some(subject) {
            return false;
        }
        *last = Some(subject.to_owned());
        true
    }

    fn clear(&self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
}

static UNAVAILABLE_SELECTION_WARNING: RepeatedWarning = RepeatedWarning::new();

fn open_with_selection<T>(
    device_override: Option<&str>,
    selected_device: Option<&str>,
    fallback_from_selected: bool,
    mut named: impl FnMut(&str) -> Result<T>,
    mut automatic: impl FnMut() -> Result<T>,
) -> Result<(T, Option<String>)> {
    if let Some(device) = device_override {
        return named(device).map(|input| (input, None));
    }
    if let Some(device) = selected_device {
        return match named(device) {
            Ok(input) => {
                UNAVAILABLE_SELECTION_WARNING.clear();
                Ok((input, None))
            }
            Err(error) if !fallback_from_selected => Err(error),
            Err(error) => {
                // Opens and the periodic routing probe both land here while
                // the saved microphone stays gone; say so once.
                if UNAVAILABLE_SELECTION_WARNING.is_new(device) {
                    tracing::warn!(%error, device, "selected microphone is unavailable; using automatic selection");
                }
                automatic().map(|input| (input, Some(device.to_owned())))
            }
        };
    }
    automatic().map(|input| (input, None))
}

fn try_input_candidates<D, T>(
    candidates: impl IntoIterator<Item = D>,
    mut open: impl FnMut(D) -> Result<T>,
) -> Result<T> {
    let mut errors = Vec::new();
    for candidate in candidates {
        match open(candidate) {
            Ok(input) => return Ok(input),
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    Err(eyre!(
        "no preferred or default microphone could be opened: {}",
        errors.join("; ")
    ))
}

/// The device's name, or `None` when it cannot be read. cpal's `Display` for
/// `Device` returns `fmt::Error` then (CoreAudio's placeholder when no input
/// exists, or a device that vanished mid-enumeration), which makes
/// `to_string` panic, so the description is read directly instead.
fn device_name(device: &Device) -> Option<String> {
    description_name(device.description())
}

fn description_name<E: std::fmt::Display>(
    description: std::result::Result<cpal::DeviceDescription, E>,
) -> Option<String> {
    match description {
        Ok(description) => Some(description.name().to_owned()),
        Err(error) => {
            tracing::debug!(%error, "skipping an input device whose name cannot be read");
            None
        }
    }
}

/// An unnamed device gets an empty name: it can be neither selected nor
/// opened, and enumeration leaves it out.
fn device_identity(device: &Device) -> DevicePreference {
    DevicePreference {
        id: device
            .id()
            .ok()
            .map(|id| id.id().to_owned())
            .filter(|id| !id.is_empty()),
        name: device_name(device).unwrap_or_default(),
    }
}

/// The input devices whose names can be read.
fn named_input_devices(host: &cpal::Host) -> Result<impl Iterator<Item = Device>> {
    Ok(host
        .input_devices()
        .wrap_err("could not enumerate input devices")?
        .filter(|device| device_name(device).is_some()))
}

/// Available inputs, retaining distinct devices even when names collide.
pub fn input_device_catalog() -> Result<Vec<DevicePreference>> {
    let mut devices = named_input_devices(&cpal::default_host())?
        .map(|device| device_identity(&device))
        .filter(|device| !device.name.is_empty())
        .collect::<Vec<_>>();
    devices.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then(left.id.cmp(&right.id))
    });
    devices.dedup();
    Ok(devices)
}

pub fn input_device_names() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut names = host
        .input_devices()
        .wrap_err("could not enumerate input devices")?
        .filter_map(|device| device_name(&device))
        .collect::<Vec<_>>();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    Ok(names)
}

fn describe_device(device: &Device, channels: u16) -> InputDescription {
    let DevicePreference {
        id: device_id,
        name,
    } = device_identity(device);
    let requested_channel = crate::app_settings::microphone_channel(device_id.as_deref());
    InputDescription {
        device_id,
        name,
        channels,
        channel: resolve_channel(requested_channel, channels),
        requested_channel,
        fallback_from: None,
        fallback_lid_closed: false,
    }
}

/// Read routing metadata without opening or recording the microphone.
pub fn input_description(
    selected_device: Option<&str>,
    preferences: &[DevicePreference],
) -> Result<InputDescription> {
    let describe = |device: Device| -> Result<InputDescription> {
        let channels = device.default_input_config()?.channels();
        if channels == 0 {
            return Err(eyre!("microphone reports no input channels"));
        }
        Ok(describe_device(&device, channels))
    };
    let lid_closed = selected_device.is_some_and(selected_off_with_lid_closed);
    let (mut description, fallback_from) = open_with_selection(
        None,
        selected_device,
        true,
        |name| {
            if lid_closed {
                return Err(eyre!(crate::clamshell::LID_CLOSED_ERROR));
            }
            let device = cpal::default_host()
                .input_devices()?
                .find(|device| device_name(device).as_deref() == Some(name))
                .ok_or_else(|| eyre!("microphone is unavailable: {name}"))?;
            describe(device)
        },
        || try_input_candidates(automatic_devices(preferences)?, describe),
    )?;
    description.fallback_lid_closed = lid_closed && fallback_from.is_some();
    description.fallback_from = fallback_from;
    Ok(description)
}

/// Connected inputs that the built-in automatic order prefers before the
/// macOS default when no explicit priority list exists.
pub fn legacy_automatic_preferences(available: &[DevicePreference]) -> Vec<DevicePreference> {
    AUTOMATIC_INPUT_DEVICE_PREFERENCES
        .iter()
        .filter_map(|query| {
            available
                .iter()
                .find(|device| device.name.to_lowercase().contains(&query.to_lowercase()))
                .cloned()
        })
        .collect()
}

fn automatic_device_indices(
    available: &[DevicePreference],
    preferences: &[DevicePreference],
    fallback: Option<usize>,
) -> Vec<usize> {
    let legacy;
    let preferences = if preferences.is_empty() {
        legacy = legacy_automatic_preferences(available);
        &legacy
    } else {
        preferences
    };
    preferred_input_indices(available, preferences, fallback)
}

fn automatic_devices(preferences: &[DevicePreference]) -> Result<Vec<Device>> {
    let host = cpal::default_host();
    let mut devices: Vec<_> = named_input_devices(&host)?.collect();
    let default = host
        .default_input_device()
        .filter(|default| device_name(default).is_some())
        .map(|default| {
            let identity = device_identity(&default);
            devices
                .iter()
                .position(|device| identity.matches(&device_identity(device)))
                .unwrap_or_else(|| {
                    devices.push(default);
                    devices.len() - 1
                })
        });
    let transports = devices.iter().map(input_transport).collect::<Vec<_>>();
    let identities = devices.iter().map(device_identity).collect::<Vec<_>>();
    let lid_closed = crate::clamshell::closed();
    Ok(
        automatic_input_order(&identities, &transports, default, preferences, lid_closed)
            .map_err(|error| eyre!(error))?
            .into_iter()
            .map(|index| devices[index].clone())
            .collect(),
    )
}

/// The automatic candidates in order: the priority list, then the system
/// default. A Bluetooth default stands aside for the internal microphone,
/// and with the lid closed the internal microphone, listed but switched off,
/// is passed over everywhere; with nothing else listed, say why instead of
/// recording silence.
fn automatic_input_order(
    identities: &[DevicePreference],
    transports: &[InputTransport],
    default: Option<usize>,
    preferences: &[DevicePreference],
    lid_closed: bool,
) -> std::result::Result<Vec<usize>, &'static str> {
    let off = |index: usize| lid_closed && transports.get(index) == Some(&InputTransport::BuiltIn);
    let fallback = default.map(|default| {
        let index = automatic_input_index(default, transports, lid_closed);
        if index != default {
            tracing::info!(
                default = %identities[default].name,
                selected = %identities[index].name,
                lid_closed,
                "automatic microphone selection passed over the system default"
            );
        }
        index
    });
    let indices = automatic_device_indices(identities, preferences, fallback)
        .into_iter()
        .filter(|&index| !off(index))
        .collect::<Vec<_>>();
    if indices.is_empty() && (0..identities.len()).any(off) {
        return Err(crate::clamshell::LID_CLOSED_ERROR);
    }
    Ok(indices)
}

/// Whether the saved microphone is the internal one while the lid is closed.
fn selected_off_with_lid_closed(name: &str) -> bool {
    if !crate::clamshell::closed() {
        return false;
    }
    cpal::default_host()
        .input_devices()
        .map(|mut devices| {
            devices.any(|device| {
                device_name(&device).as_deref() == Some(name)
                    && input_transport(&device) == InputTransport::BuiltIn
            })
        })
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputTransport {
    /// The internal microphone, switched off while a MacBook's lid is
    /// closed. A built-in headphone-jack input is `Other`.
    BuiltIn,
    Bluetooth,
    /// A virtual or aggregate device, which may carry no microphone at all.
    Virtual,
    Other,
}

/// Opening a Bluetooth headset's microphone switches it into a low-quality
/// call profile for all audio, so automatic selection prefers the internal
/// mic over a Bluetooth default. With the lid closed the internal mic is
/// off: a Bluetooth default stays, and an internal default gives way to the
/// first other input, preferring a physical one over Bluetooth, and
/// Bluetooth over a virtual device.
fn automatic_input_index(default: usize, transports: &[InputTransport], lid_closed: bool) -> usize {
    let first = |transport| transports.iter().position(|kind| *kind == transport);
    match transports.get(default) {
        Some(InputTransport::Bluetooth) if !lid_closed => {
            first(InputTransport::BuiltIn).unwrap_or(default)
        }
        Some(InputTransport::BuiltIn) if lid_closed => first(InputTransport::Other)
            .or_else(|| first(InputTransport::Bluetooth))
            .or_else(|| first(InputTransport::Virtual))
            .unwrap_or(default),
        _ => default,
    }
}

#[cfg(target_os = "macos")]
fn input_transport(device: &Device) -> InputTransport {
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use objc2_core_audio::{
        AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectPropertySelector, kAudioDevicePropertyTransportType,
        kAudioDeviceTransportTypeAggregate, kAudioDeviceTransportTypeBluetooth,
        kAudioDeviceTransportTypeBluetoothLE, kAudioDeviceTransportTypeBuiltIn,
        kAudioDeviceTransportTypeVirtual, kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    };
    use objc2_core_foundation::CFString;

    /// `kAudioDevicePropertyDataSource` and `kAudioObjectPropertyScopeInput`.
    const DATA_SOURCE_SELECTOR: u32 = u32::from_be_bytes(*b"ssrc");
    const INPUT_SCOPE: u32 = u32::from_be_bytes(*b"inpt");

    fn read_u32(
        object: AudioObjectID,
        selector: AudioObjectPropertySelector,
        scope: u32,
        qualifier: Option<NonNull<c_void>>,
        qualifier_size: u32,
    ) -> Option<u32> {
        let mut address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut value = 0_u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&mut address),
                qualifier_size,
                qualifier.map_or(std::ptr::null(), |qualifier| qualifier.as_ptr()),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            )
        };
        (status == 0 && size as usize == std::mem::size_of::<u32>()).then_some(value)
    }

    let Ok(id) = device.id() else {
        return InputTransport::Other;
    };
    let uid = CFString::from_str(id.id());
    let uid_ref: *const CFString = &*uid;
    let Some(device_id) = read_u32(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyScopeGlobal,
        Some(NonNull::from(&uid_ref).cast()),
        std::mem::size_of::<*const CFString>() as u32,
    )
    .filter(|device_id| *device_id != 0) else {
        return InputTransport::Other;
    };
    match read_u32(
        device_id,
        kAudioDevicePropertyTransportType,
        kAudioObjectPropertyScopeGlobal,
        None,
        0,
    ) {
        Some(transport) if transport == kAudioDeviceTransportTypeBuiltIn => {
            let data_source = read_u32(device_id, DATA_SOURCE_SELECTOR, INPUT_SCOPE, None, 0);
            // A headphone-jack microphone is built in but stays on with the
            // lid closed.
            if crate::clamshell::is_lid_switched_microphone(id.id(), data_source) {
                InputTransport::BuiltIn
            } else {
                InputTransport::Other
            }
        }
        Some(transport)
            if transport == kAudioDeviceTransportTypeBluetooth
                || transport == kAudioDeviceTransportTypeBluetoothLE =>
        {
            InputTransport::Bluetooth
        }
        Some(transport)
            if transport == kAudioDeviceTransportTypeVirtual
                || transport == kAudioDeviceTransportTypeAggregate =>
        {
            InputTransport::Virtual
        }
        _ => InputTransport::Other,
    }
}

fn mono<T>(
    samples: &[T],
    channels: usize,
    selected_channel: Option<u16>,
    convert: impl Fn(&T) -> f32,
) -> Vec<f32> {
    if channels == 0 {
        return Vec::new();
    }
    if channels == 1 {
        return samples.iter().map(convert).collect();
    }
    let selected =
        resolve_channel(selected_channel, channels as u16).map(|channel| usize::from(channel - 1));
    samples
        .chunks_exact(channels)
        .map(|frame| {
            selected.map_or_else(
                || frame.iter().map(&convert).sum::<f32>() / channels as f32,
                |channel| convert(&frame[channel]),
            )
        })
        .collect()
}

/// The capture callback's end of the bounded chunk queue.
struct ChunkSink {
    sender: SyncSender<(Vec<f32>, CaptureInstant)>,
    dropped_through: Arc<AtomicU64>,
}

impl ChunkSink {
    /// Runs on the capture callback: never blocks. A chunk the full queue
    /// cannot take is dropped and its end marked, so the owner reports the
    /// hole.
    fn send(&self, chunk: Vec<f32>, captured_through: CaptureInstant) {
        if let Err(TrySendError::Full(_)) = self.sender.try_send((chunk, captured_through)) {
            self.dropped_through
                .fetch_max(captured_through.as_nanos().max(1), Ordering::AcqRel);
        }
    }
}

/// Runs on the host's stream thread, CoreAudio's real-time thread included.
fn stream_error(sender: &SyncSender<String>, error: cpal::Error) {
    match error.kind() {
        // A processor overload or a refused real-time promotion is a glitch on
        // a stream that keeps running. Only a counter moves here; the owner
        // writes the burst. Lost samples still surface as a capture gap in
        // the owner's timeline.
        cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied => overloads::record(),
        _ => report_stream_error(sender, error.to_string()),
    }
}

fn report_stream_error(sender: &SyncSender<String>, error: String) {
    tracing::error!(%error, "microphone stream failed");
    let _ = sender.try_send(error);
}

fn build_stream<T: SizedSample>(
    device: &Device,
    config: &StreamConfig,
    channels: usize,
    selected_channel: Option<u16>,
    sink: ChunkSink,
    error_sender: SyncSender<String>,
    convert: impl Fn(T) -> f32 + Send + 'static,
) -> Result<Stream> {
    let sample_rate = config.sample_rate;
    device
        .build_input_stream(
            *config,
            move |data: &[T], info| {
                let captured_through = captured_through(info, data.len() / channels, sample_rate);
                sink.send(
                    mono(data, channels, selected_channel, |sample| convert(*sample)),
                    captured_through,
                )
            },
            move |error| stream_error(&error_sender, error),
            None,
        )
        .wrap_err_with(|| format!("could not open {} microphone stream", T::FORMAT))
}

fn captured_through(
    info: &cpal::InputCallbackInfo,
    frames: usize,
    sample_rate: u32,
) -> CaptureInstant {
    let captured_at = CaptureInstant::from_stream(info.timestamp().capture);
    let nanos = (frames as u128 * 1_000_000_000 / u128::from(sample_rate)) as u64;
    let duration = Duration::from_nanos(nanos);
    captured_at.checked_add(duration).unwrap_or(captured_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preference(id: &str, name: &str) -> DevicePreference {
        DevicePreference {
            id: Some(id.into()),
            name: name.into(),
        }
    }

    fn test_input(id: &str, name: &str) -> (AudioInput, Sender<(Vec<f32>, CaptureInstant)>) {
        let (mut input, samples) = AudioInput::channel_for_test();
        input.device_name = name.into();
        input.description.device_id = Some(id.into());
        input.description.name = name.into();
        (input, samples)
    }

    #[test]
    fn configured_priority_replaces_legacy_order_and_preserves_system_fallback() {
        let available = [
            preference("system", "Built-in Microphone"),
            preference("studio", "Studio Display Microphone"),
            preference("ua", "Universal Audio Thunderbolt"),
        ];
        assert_eq!(
            automatic_device_indices(&available, &[], Some(0)),
            [2, 1, 0]
        );
        assert_eq!(
            automatic_device_indices(&available, &[available[1].clone()], Some(0)),
            [1, 0]
        );
        assert_eq!(
            automatic_device_indices(&available, &[preference("missing", "Unavailable")], Some(0)),
            [0]
        );
        assert_eq!(
            automatic_device_indices(&available, &[available[1].clone()], None),
            [1]
        );
    }

    #[test]
    fn changing_automatic_priority_preserves_an_explicit_stream_and_its_revision() {
        let mut input = RecoveringAudioInput::closed(None, 3, Some("Explicit"), &[]);
        let (active, samples) = test_input("explicit-priority-test", "Explicit");
        input.finish_initial_open(Ok(active), Instant::now());
        let preferences = [preference("automatic", "Automatic preference")];
        input.request_selection(4, Some("Explicit"), &preferences);
        assert_eq!(input.active_revision, 4);
        assert_eq!(input.preferences, preferences);
        assert!(input.recovery.reason.is_none());
        assert!(input.replacement.is_none());
        samples
            .send((vec![0.25], CaptureInstant::from_nanos(1)))
            .unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, false),
            RecoveringAudioInputEvent::Chunk { .. }
        ));
    }

    #[test]
    fn explicit_channel_changes_still_use_the_deferred_reopen_path() {
        let mut input = RecoveringAudioInput::closed(None, 3, Some("Explicit"), &[]);
        let (mut active, _samples) = test_input("explicit-channel-test", "Explicit");
        // An input without a UID cannot match a saved channel preference, so
        // the current requested channel differs from the runtime mix default.
        active.description.device_id = None;
        active.description.channels = 2;
        active.description.requested_channel = Some(2);
        active.description.channel = Some(2);
        input.finish_initial_open(Ok(active), Instant::now());
        input.request_selection(4, Some("Explicit"), &[]);
        assert_eq!(input.active_revision, 3);
        assert_eq!(input.recovery.target_revision, Some(4));
        assert_eq!(input.description().unwrap().channel, Some(2));
    }

    #[test]
    fn automatic_open_tries_the_next_preference_and_then_the_system_device() {
        let mut attempted = Vec::new();
        let opened = try_input_candidates(["preferred", "second", "system"], |name| {
            attempted.push(name);
            if name == "system" {
                Ok(name)
            } else {
                Err(eyre!("unavailable"))
            }
        })
        .unwrap();
        assert_eq!(attempted, ["preferred", "second", "system"]);
        assert_eq!(opened, "system");
        attempted.clear();
        let opened = try_input_candidates(["preferred", "second", "system"], |name| {
            attempted.push(name);
            if name == "second" {
                Ok(name)
            } else {
                Err(eyre!("unavailable"))
            }
        })
        .unwrap();
        assert_eq!(attempted, ["preferred", "second"]);
        assert_eq!(opened, "second");
    }

    #[test]
    fn cli_and_explicit_selection_keep_precedence_over_automatic_preferences() {
        let (selected, fallback) = open_with_selection(
            Some("CLI"),
            Some("Settings"),
            true,
            |name| Ok(name.to_owned()),
            || panic!("CLI must not use automatic routing"),
        )
        .unwrap();
        assert_eq!(selected, "CLI");
        assert_eq!(fallback, None);
        let result = open_with_selection::<()>(
            Some("CLI"),
            Some("Settings"),
            true,
            |_| Err(eyre!("missing")),
            || panic!("missing CLI must not fall back"),
        );
        assert!(result.is_err());
        let (selected, fallback) = open_with_selection(
            None,
            Some("Settings"),
            true,
            |name| Ok(name.to_owned()),
            || panic!("explicit choice has precedence"),
        )
        .unwrap();
        assert_eq!(selected, "Settings");
        assert_eq!(fallback, None);
        let (selected, fallback) = open_with_selection(
            None,
            Some("Missing"),
            true,
            |_| Err(eyre!("missing")),
            || Ok("Automatic"),
        )
        .unwrap();
        assert_eq!(selected, "Automatic");
        assert_eq!(fallback.as_deref(), Some("Missing"));
        assert!(
            open_with_selection::<()>(
                None,
                Some("Missing"),
                false,
                |_| Err(eyre!("missing")),
                || panic!("keep current stream during a failed explicit edit")
            )
            .is_err()
        );
    }

    #[test]
    fn reconnected_preference_is_applied_only_after_the_active_capture() {
        let preferred = preference("preferred", "USB");
        let mut input =
            RecoveringAudioInput::closed(None, 3, None, std::slice::from_ref(&preferred));
        let (active, _samples) = test_input("fallback", "Built-in");
        input.finish_initial_open(Ok(active), Instant::now());
        let (sender, receiver) = mpsc::channel();
        input.routing_probe = Some(receiver);
        sender.send(Ok(preferred.clone())).unwrap();

        input.maintain_routing(Instant::now(), false);
        assert!(input.recovery.reason.is_none());
        assert!(input.routing_probe.is_some());
        assert_eq!(input.device_name(), "Built-in");
        input.maintain_routing(Instant::now(), true);
        assert!(input.recovery.reason == Some(MicrophoneRecoveryReason::AvailabilityChanged));
        assert_eq!(input.recovery.target_revision, Some(3));
        assert!(
            !input.is_recovering(),
            "an available input continues supplying audio"
        );

        let (sender, receiver) = mpsc::channel();
        let (replacement, _samples) = test_input("preferred", "USB renamed");
        input.replacement = Some(receiver);
        sender.send((3, Ok(replacement))).unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Reopened
        ));
        assert_eq!(input.device_name(), "USB renamed");
        assert_eq!(input.preferences, [preferred]);
    }

    #[test]
    fn preference_edits_discard_stale_workers_and_do_not_interrupt_a_clip() {
        let mut input = RecoveringAudioInput::closed(None, 3, None, &[]);
        let (mut active, _active_samples) = test_input("active", "Current");
        active.description.channels = 2;
        active.description.channel = Some(2);
        active.description.requested_channel = Some(2);
        input.finish_initial_open(Ok(active), Instant::now());
        let next = preference("next", "Next");
        input.request_selection(4, None, std::slice::from_ref(&next));
        let (sender, receiver) = mpsc::channel();
        let (replacement, _replacement_samples) = test_input("next", "Next");
        input.replacement = Some(receiver);
        sender.send((4, Ok(replacement))).unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, false),
            RecoveringAudioInputEvent::Timeout
        ));
        assert_eq!(input.device_name(), "Current");
        assert_eq!(input.active_revision, 3);
        assert_eq!(input.description().unwrap().channel, Some(2));

        input.request_selection(5, None, &[]);
        let (sender, receiver) = mpsc::channel();
        let (replacement, _replacement_samples) = test_input("next", "Next");
        input.replacement = Some(receiver);
        sender.send((4, Ok(replacement))).unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Timeout
        ));
        assert_eq!(input.device_name(), "Current");
        assert_eq!(input.recovery.target_revision, Some(5));
        assert!(input.preferences.is_empty());
    }

    #[test]
    fn failed_preferred_open_keeps_the_existing_fallback_stream() {
        let mut input = RecoveringAudioInput::closed(None, 3, None, &[]);
        let (active, samples) = test_input("same", "Current");
        input.finish_initial_open(Ok(active), Instant::now());
        input.recovery.request_routing_change(3, Instant::now());
        let (sender, receiver) = mpsc::channel();
        let (replacement, _replacement_samples) = test_input("same", "Current");
        input.replacement = Some(receiver);
        sender.send((3, Ok(replacement))).unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Timeout
        ));
        let at = CaptureInstant::from_nanos(1);
        samples.send((vec![0.25], at)).unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, false),
            RecoveringAudioInputEvent::Chunk { .. }
        ));
        assert!(input.recovery.reason.is_none());
    }

    #[test]
    fn explicit_channels_avoid_silent_channel_attenuation_and_phase_cancellation() {
        let signal = [0.25_f32, -0.5, 0.75];
        let left: Vec<_> = signal.iter().flat_map(|&sample| [sample, 0.0]).collect();
        let right: Vec<_> = signal.iter().flat_map(|&sample| [0.0, sample]).collect();
        let opposed: Vec<_> = signal
            .iter()
            .flat_map(|&sample| [sample, -sample])
            .collect();
        assert_eq!(mono(&left, 2, Some(1), |sample| *sample), signal);
        assert_eq!(mono(&right, 2, Some(2), |sample| *sample), signal);
        assert_eq!(mono(&opposed, 2, Some(1), |sample| *sample), signal);
        assert_eq!(
            mono(&opposed, 2, Some(2), |sample| *sample),
            signal.map(|sample| -sample)
        );
        // Mixing stays the default until the user makes an explicit choice.
        assert_eq!(
            mono(&left, 2, None, |sample| *sample),
            signal.map(|sample| sample / 2.0)
        );
        assert_eq!(mono(&opposed, 2, None, |sample| *sample), [0.0; 3]);
    }

    #[test]
    fn normal_stereo_and_integer_inputs_keep_their_expected_routing() {
        let stereo = [0.2_f32, 0.6, -0.2, -0.6];
        assert_eq!(mono(&stereo, 2, None, |sample| *sample), [0.4, -0.4]);
        assert_eq!(mono(&stereo, 2, Some(2), |sample| *sample), [0.6, -0.6]);
        assert_eq!(
            mono(
                &[0_i16, i16::MAX, 0, -i16::MAX],
                2,
                Some(2),
                |sample| *sample as f32 / i16::MAX as f32
            ),
            [1.0, -1.0]
        );
        assert_eq!(
            mono(
                &[32768_u16, 49152, 32768, 16384],
                2,
                Some(2),
                |sample| *sample as f32 / 32768.0 - 1.0
            ),
            [0.5, -0.5]
        );
    }

    #[test]
    fn mono_is_unchanged_and_invalid_routing_cannot_index_past_a_frame() {
        let samples = [0.0_f32, -0.0, 0.75, -0.25];
        for channel in [None, Some(1), Some(2)] {
            let routed = mono(&samples, 1, channel, |sample| *sample);
            assert_eq!(
                routed
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                samples.map(f32::to_bits)
            );
        }
        assert_eq!(
            mono(&[0.2_f32, 0.6, 0.9], 2, Some(8), |sample| *sample),
            [0.4]
        );
        assert!(mono(&samples, 0, Some(1), |sample| *sample).is_empty());
    }

    #[test]
    fn automatic_input_prefers_built_in_over_a_bluetooth_default() {
        use InputTransport::{Bluetooth, BuiltIn, Other};

        assert_eq!(automatic_input_index(1, &[BuiltIn, Bluetooth], false), 0);
        assert_eq!(
            automatic_input_index(0, &[Bluetooth, Other, BuiltIn], false),
            2
        );
        // Non-Bluetooth defaults, such as USB microphones, are kept.
        assert_eq!(automatic_input_index(1, &[BuiltIn, Other], false), 1);
        // Without a built-in microphone the Bluetooth default is still used.
        assert_eq!(automatic_input_index(0, &[Bluetooth, Other], false), 0);
    }

    #[test]
    fn a_closed_lid_never_trades_a_bluetooth_default_for_the_switched_off_microphone() {
        use InputTransport::{Bluetooth, BuiltIn, Other, Virtual};

        // Clamshell with AirPods: the headset stays, never the silent mic.
        assert_eq!(automatic_input_index(1, &[BuiltIn, Bluetooth], true), 1);
        // An internal default gives way to a physical input first, then
        // Bluetooth, then a virtual device.
        assert_eq!(
            automatic_input_index(0, &[BuiltIn, Virtual, Bluetooth, Other], true),
            3
        );
        assert_eq!(
            automatic_input_index(0, &[BuiltIn, Virtual, Bluetooth], true),
            2
        );
        assert_eq!(automatic_input_index(0, &[BuiltIn, Virtual], true), 1);
        // Other defaults are followed as before.
        assert_eq!(automatic_input_index(1, &[BuiltIn, Other], true), 1);
        // With nothing else listed the index is unchanged; the order rejects it.
        assert_eq!(automatic_input_index(0, &[BuiltIn], true), 0);
    }

    #[test]
    fn a_closed_lid_passes_over_the_built_in_microphone() {
        use InputTransport::{Bluetooth, BuiltIn, Other};

        let built_in = preference("built-in", "MacBook Pro Microphone");
        let airpods = preference("airpods", "AirPods Pro");
        let display = preference("usb", "USB Microphone");
        let devices = [built_in.clone(), airpods.clone(), display.clone()];
        let transports = [BuiltIn, Bluetooth, Other];
        let order = |default, preferences: &[DevicePreference], lid_closed| {
            automatic_input_order(&devices, &transports, default, preferences, lid_closed)
        };

        // Lid open: the built-in default and priority entries are used.
        assert_eq!(order(Some(0), &[], false), Ok(vec![0]));
        assert_eq!(
            order(Some(1), std::slice::from_ref(&built_in), false),
            Ok(vec![0])
        );
        // Lid closed: a built-in default stands aside for another input.
        assert_eq!(order(Some(0), &[], true), Ok(vec![2]));
        // A Bluetooth default is followed instead of the switched-off mic.
        assert_eq!(order(Some(1), &[], true), Ok(vec![1]));
        // A priority entry for the built-in microphone is skipped, keeping
        // the rest of the order and the system fallback.
        assert_eq!(
            order(Some(1), &[built_in.clone(), display.clone()], true),
            Ok(vec![2, 1])
        );
        // The headphone jack reads as Other, so it stays usable.
        assert_eq!(
            automatic_input_order(
                &[built_in.clone(), preference("jack", "External Microphone")],
                &[BuiltIn, Other],
                Some(1),
                &[],
                true
            ),
            Ok(vec![1])
        );
        // With nothing else listed, say why rather than record silence.
        assert_eq!(
            automatic_input_order(
                std::slice::from_ref(&built_in),
                &[BuiltIn],
                Some(0),
                std::slice::from_ref(&built_in),
                true
            ),
            Err(crate::clamshell::LID_CLOSED_ERROR)
        );
        assert_eq!(
            automatic_input_order(&[], &[], None, &[], true),
            Ok(Vec::new())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mach_event_ticks_are_converted_to_nanoseconds() {
        assert_eq!(scale_mach_ticks(72_000_000, 125, 3), 3_000_000_000);
        assert_eq!(scale_mach_ticks(3_000_000_000, 1, 1), 3_000_000_000);
    }

    #[test]
    fn stream_failures_are_visible_to_the_audio_owner() {
        let (sender, errors) = mpsc::sync_channel(1);

        report_stream_error(&sender, "Device sample rate changed".into());

        assert_eq!(errors.try_recv().unwrap(), "Device sample rate changed");
    }

    #[test]
    fn an_unreadable_device_name_is_skipped_instead_of_panicking() {
        let unreadable: std::result::Result<cpal::DeviceDescription, cpal::Error> =
            Err(cpal::ErrorKind::DeviceNotAvailable.into());
        assert_eq!(description_name(unreadable), None);
        let readable: std::result::Result<_, cpal::Error> = Ok(
            cpal::DeviceDescriptionBuilder::new("MacBook Pro Microphone")
                .manufacturer("Apple Inc.")
                .build(),
        );
        assert_eq!(
            description_name(readable).as_deref(),
            Some("MacBook Pro Microphone")
        );
    }

    #[test]
    fn a_dropped_chunk_is_reported_where_the_hole_is() {
        let (mut input, _samples) = AudioInput::channel_for_test();
        let (sender, chunks) = mpsc::sync_channel(2);
        input.chunks = chunks;
        let sink = ChunkSink {
            sender,
            dropped_through: input.dropped_through.clone(),
        };
        let at = |ms: u64| CaptureInstant::from_nanos(60_000_000_000 + ms * 1_000_000);
        // The queue holds two chunks; the third is dropped without blocking.
        for ms in [10, 20, 30] {
            sink.send(vec![0.25; 480], at(ms));
        }
        let chunk_at = |event: AudioInputEvent| match event {
            AudioInputEvent::Chunk {
                captured_through, ..
            } => Some(captured_through),
            _ => None,
        };
        // Audio queued before the drop arrives first, uninterrupted.
        assert_eq!(chunk_at(input.recv_timeout(Duration::ZERO)), Some(at(10)));
        assert_eq!(chunk_at(input.recv_timeout(Duration::ZERO)), Some(at(20)));
        sink.send(vec![0.25; 480], at(40));
        // The hole is reported right before the audio that follows it.
        assert!(matches!(
            input.recv_timeout(Duration::ZERO),
            AudioInputEvent::Dropped
        ));
        assert_eq!(chunk_at(input.recv_timeout(Duration::ZERO)), Some(at(40)));
        sink.send(vec![0.25; 480], at(50));
        assert_eq!(chunk_at(input.recv_timeout(Duration::ZERO)), Some(at(50)));
    }

    #[test]
    fn overloads_continue_the_stream_and_real_failures_stop_it() {
        let (sender, errors) = mpsc::sync_channel(1);

        stream_error(&sender, cpal::ErrorKind::Xrun.into());
        stream_error(&sender, cpal::ErrorKind::RealtimeDenied.into());
        assert!(errors.try_recv().is_err());

        stream_error(&sender, cpal::ErrorKind::DeviceNotAvailable.into());
        assert!(errors.try_recv().is_ok());
    }

    #[test]
    fn a_dead_stream_is_dropped_before_its_replacement_opens() {
        let mut input = RecoveringAudioInput::closed(
            Some("Endu nonexistent microphone for dead stream test"),
            3,
            None,
            &[],
        );
        let (failed, samples) = AudioInput::channel_for_test();
        input.finish_initial_open(Ok(failed), Instant::now());
        // A stream that delivered steadily reopens at once.
        input.proven = true;
        drop(samples);
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Interrupted
        ));
        // Metadata of the failed stream is still readable for the report.
        assert!(input.is_open());
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Timeout
        ));
        assert!(input.is_opening());
        assert!(!input.is_open(), "no dead stream stays on the device");
        assert!(input.description().is_none());

        // A routing change keeps the working stream until its replacement.
        let mut routed = RecoveringAudioInput::closed(
            Some("Endu nonexistent microphone for dead stream test"),
            3,
            None,
            &[],
        );
        let (live, _samples) = AudioInput::channel_for_test();
        routed.finish_initial_open(Ok(live), Instant::now());
        routed.recovery.request_routing_change(3, Instant::now());
        let _ = routed.recv_timeout(Duration::ZERO, true);
        assert!(routed.is_opening());
        assert!(routed.is_open());
    }

    #[test]
    fn a_stream_that_stops_delivering_audio_is_reopened() {
        let stall = Duration::from_secs(4);
        let mut input = RecoveringAudioInput::closed(
            Some("Endu nonexistent microphone for stall test"),
            1,
            None,
            &[],
        );
        let (mut opened, samples) = AudioInput::channel_for_test();
        opened.watch_stalls_for_test(stall);
        input.finish_initial_open(Ok(opened), Instant::now());

        // Delivering audio keeps the stream alive.
        samples
            .send((vec![0.5; 240], CaptureInstant::from_nanos(10_000_000_000)))
            .unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Chunk { .. }
        ));
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Timeout
        ));

        // A quiet stream with no error counts as failed and recovers.
        input.input.as_mut().unwrap().last_delivery = Instant::now() - stall;
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Interrupted
        ));
        assert!(input.is_recovering());
    }

    /// A test input whose stream has already stopped.
    fn stalling_input() -> AudioInput {
        let (mut input, samples) = AudioInput::channel_for_test();
        drop(samples);
        input.device_name = "Stalling microphone".into();
        input
    }

    /// Hands `input` a finished open of `replacement` and applies it.
    fn reopen_with(input: &mut RecoveringAudioInput, replacement: AudioInput) {
        let (sender, receiver) = mpsc::channel();
        input.replacement = Some(receiver);
        sender
            .send((input.active_revision, Ok(replacement)))
            .unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Reopened
        ));
    }

    fn stall_test_input() -> RecoveringAudioInput {
        RecoveringAudioInput::closed(
            Some("Endu nonexistent microphone for stall backoff test"),
            0,
            None,
            &[],
        )
    }

    #[test]
    fn a_stream_that_keeps_stalling_backs_off_to_a_minute() {
        let mut input = stall_test_input();
        input.finish_initial_open(Ok(stalling_input()), Instant::now());

        let mut delays = Vec::new();
        for _ in 0..10 {
            assert!(matches!(
                input.recv_timeout(Duration::ZERO, true),
                RecoveringAudioInputEvent::Interrupted
            ));
            delays.push(input.recovery.delay);
            // The next turn lets go of the stream, and the owner may sleep
            // until the retry.
            assert!(matches!(
                input.recv_timeout(Duration::ZERO, true),
                RecoveringAudioInputEvent::Timeout
            ));
            assert!(!input.is_open());
            assert!(input.retry_wait(Instant::now()).is_some());
            reopen_with(&mut input, stalling_input());
            assert!(input.recovery.next_attempt.is_none());
        }
        assert_eq!(
            delays,
            [
                500, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000, 60_000
            ]
            .map(Duration::from_millis)
        );
        let failed_at = Instant::now();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Interrupted
        ));
        assert!(input.recovery.next_attempt.unwrap() >= failed_at + Duration::from_secs(60));
        let _ = input.recv_timeout(Duration::ZERO, true);
        assert!(
            input
                .retry_wait(Instant::now())
                .is_some_and(|wait| wait > Duration::from_secs(59))
        );
        assert!(!input.is_open(), "the failed stream lets go of the device");

        // A press retries now but keeps the backoff for the next failure.
        input.retry_now();
        assert_eq!(input.retry_wait(Instant::now()), Some(Duration::ZERO));
        assert_eq!(input.recovery.delay, Duration::from_secs(60));
    }

    #[test]
    fn a_stream_that_delivers_steadily_reopens_at_once_and_forgets_the_backoff() {
        let mut input = stall_test_input();
        input.finish_initial_open(Ok(stalling_input()), Instant::now());
        for _ in 0..2 {
            assert!(matches!(
                input.recv_timeout(Duration::ZERO, true),
                RecoveringAudioInputEvent::Interrupted
            ));
            reopen_with(&mut input, stalling_input());
        }
        assert_eq!(input.recovery.delay, Duration::from_secs(1));
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Interrupted
        ));

        // A replacement delivering for long enough proves itself.
        let (replacement, samples) = AudioInput::channel_for_test();
        reopen_with(&mut input, replacement);
        assert_eq!(input.recovery.delay, Duration::from_secs(2));
        input.first_delivery = Some(Instant::now() - STREAM_PROVEN_UPTIME);
        samples
            .send((vec![0.5; 240], CaptureInstant::from_nanos(30_000_000_000)))
            .unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Chunk { .. }
        ));
        assert!(input.proven);
        assert_eq!(input.recovery.delay, MICROPHONE_RETRY_INITIAL);

        // Its failure reopens at once, as a working microphone always has.
        drop(samples);
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Interrupted
        ));
        assert!(input.recovery.should_attempt(Instant::now()));
        assert_eq!(input.recovery.delay, MICROPHONE_RETRY_INITIAL);
    }

    #[test]
    fn selection_changes_reset_the_stall_backoff_and_routing_keeps_it() {
        let now = Instant::now();
        let mut recovery = MicrophoneRecovery::default();
        for _ in 0..4 {
            recovery.back_off_stream_recovery(now);
        }
        assert_eq!(recovery.delay, Duration::from_secs(4));
        recovery.reopened();
        assert_eq!(recovery.delay, Duration::from_secs(4));
        recovery.request_routing_change(1, now);
        assert!(recovery.should_attempt(now));
        assert_eq!(recovery.delay, Duration::from_secs(4));
        recovery.request_selection_change(2, now);
        assert!(recovery.should_attempt(now));
        assert_eq!(recovery.delay, MICROPHONE_RETRY_INITIAL);
        // A press only advances a failed microphone's next attempt.
        let mut idle = MicrophoneRecovery::default();
        idle.retry_now(now);
        assert!(idle.next_attempt.is_none());
    }

    #[test]
    fn a_warning_about_the_same_subject_is_written_once() {
        let warning = RepeatedWarning::new();
        assert!(warning.is_new("USB microphone"));
        assert!(!warning.is_new("USB microphone"));
        assert!(warning.is_new("Headset"));
        warning.clear();
        assert!(warning.is_new("Headset"));
    }

    #[test]
    fn a_microphone_that_stays_gone_is_logged_once() {
        let mut input = stall_test_input();
        input.request_recovery();
        let fail = |input: &mut RecoveringAudioInput, error: &str| {
            let (sender, receiver) = mpsc::channel();
            input.replacement = Some(receiver);
            sender.send((0, Err(error.to_owned()))).unwrap();
            assert!(matches!(
                input.recv_timeout(Duration::ZERO, true),
                RecoveringAudioInputEvent::Timeout
            ));
        };
        fail(&mut input, "microphone is unavailable");
        assert_eq!(
            input.retry_warning.as_deref(),
            Some("microphone is unavailable")
        );
        fail(&mut input, "microphone is unavailable");
        fail(&mut input, "microphone is busy");
        assert_eq!(input.retry_warning.as_deref(), Some("microphone is busy"));
        reopen_with(&mut input, AudioInput::channel_for_test().0);
        assert!(input.retry_warning.is_none());
    }

    #[test]
    fn closed_input_applies_selection_without_opening() {
        let mut input = RecoveringAudioInput::closed(None, 3, Some("Old microphone"), &[]);

        input.request_selection(4, Some("Next microphone"), &[]);

        assert!(!input.is_open());
        assert_eq!(input.active_revision, 4);
        assert_eq!(input.selected_device.as_deref(), Some("Next microphone"));
        assert!(input.replacement.is_none());
    }

    #[test]
    fn failed_startup_retry_keeps_backoff_without_an_open_stream() {
        let mut input = RecoveringAudioInput::closed(None, 3, Some("Missing microphone"), &[]);
        let started = Instant::now();
        input.finish_initial_open(Err(eyre!("device unavailable")), started);
        assert!(input.is_recovering());
        assert!(
            !input
                .recovery
                .should_attempt(started + Duration::from_millis(249))
        );
        assert!(
            input
                .recovery
                .should_attempt(started + Duration::from_millis(250))
        );
        let (sender, receiver) = mpsc::channel();
        input.replacement = Some(receiver);
        sender.send((3, Err("device unavailable".into()))).unwrap();

        let before = Instant::now();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Timeout
        ));
        let after = Instant::now();
        assert!(!input.is_open());
        assert!(!input.is_opening());
        assert!(input.is_recovering());
        let next_attempt = input.recovery.next_attempt.unwrap();
        assert!(next_attempt >= before + Duration::from_millis(500));
        assert!(next_attempt <= after + Duration::from_millis(500));

        input.cancel_open();
        input.request_open();
        assert!(input.is_recovering());
        assert!(!input.is_opening());
        assert_eq!(input.recovery.next_attempt, Some(next_attempt));

        input.request_selection(4, Some("Next microphone"), &[]);
        assert!(input.is_recovering());
        assert_eq!(input.recovery.target_revision, Some(4));
        assert_eq!(input.active_revision, 3);
        assert_eq!(input.selected_device.as_deref(), Some("Next microphone"));
        assert!(input.recovery.should_attempt(Instant::now()));
    }

    #[test]
    fn startup_retry_recovers_metadata_and_audio_without_restarting() {
        let mut input = RecoveringAudioInput::closed(None, 3, Some("Test microphone"), &[]);
        input.finish_initial_open(Err(eyre!("device unavailable")), Instant::now());
        let (sender, receiver) = mpsc::channel();
        input.replacement = Some(receiver);
        let (opened, samples) = AudioInput::channel_for_test();
        sender.send((3, Ok(opened))).unwrap();

        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Reopened
        ));
        assert!(!input.is_recovering());
        assert!(!input.is_opening());
        assert!(input.recovery.next_attempt.is_none());
        assert_eq!(input.sample_rate(), 48_000);
        assert_eq!(input.device_name(), "Test microphone");
        let at = CaptureInstant::from_nanos(60_000_000_000);
        samples.send((vec![0.25; 480], at)).unwrap();
        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::Chunk { samples, captured_through }
                if samples == vec![0.25; 480] && captured_through == at
        ));
    }

    #[test]
    fn startup_recovery_opens_even_when_a_capture_is_pending() {
        let mut input = RecoveringAudioInput::closed(
            Some("Endu nonexistent microphone for pending recovery test"),
            3,
            None,
            &[],
        );
        input.request_recovery();
        input.request_selection(4, Some("Ignored selection under CLI override"), &[]);
        assert_eq!(input.active_revision, 3);
        assert!(input.selected_device.is_none());

        assert!(matches!(
            input.recv_timeout(Duration::ZERO, false),
            RecoveringAudioInputEvent::Timeout
        ));
        assert!(input.is_opening());
        input.cancel_open();
        assert!(input.is_opening());
        input.close();
        assert!(!input.is_opening());
        assert!(!input.is_recovering());
        assert!(input.recovery.next_attempt.is_none());
    }

    #[test]
    fn a_cold_open_is_delivered_when_it_completes_not_after_the_turn() {
        let (mut input, opened) = RecoveringAudioInput::pending_for_test();
        let opener = std::thread::spawn(move || {
            let (replacement, samples) = AudioInput::channel_for_test();
            opened.send((0, Ok(replacement))).unwrap();
            samples
        });
        let waited = Instant::now();
        // A closed input must not sleep through the whole turn before polling.
        assert!(matches!(
            input.recv_timeout(Duration::from_secs(60), false),
            RecoveringAudioInputEvent::Reopened
        ));
        assert!(waited.elapsed() < Duration::from_secs(30));
        assert!(input.is_open());
        assert!(!input.is_opening());
        let _samples = opener.join().unwrap();
    }

    #[test]
    fn failed_on_demand_open_does_not_retry_while_idle() {
        let mut input = RecoveringAudioInput::closed(None, 3, None, &[]);
        let (sender, receiver) = mpsc::channel();
        input.replacement = Some(receiver);
        sender.send((3, Err("device unavailable".into()))).unwrap();

        assert!(matches!(
            input.recv_timeout(Duration::ZERO, true),
            RecoveringAudioInputEvent::OpenFailed(error) if error == "device unavailable"
        ));
        assert!(!input.is_recovering());
        assert!(!input.is_opening());
        assert!(input.recovery.next_attempt.is_none());
    }

    #[test]
    fn microphone_recovery_retries_with_bounded_backoff_and_resets() {
        let started = Instant::now();
        let mut recovery = MicrophoneRecovery::default();

        recovery.request_selection_change(1, started);
        assert!(!recovery.blocks_audio());
        assert!(recovery.should_attempt(started));

        recovery.failed(started);
        recovery.request_selection_change(1, started + Duration::from_millis(100));
        assert!(!recovery.should_attempt(started + Duration::from_millis(249)));
        assert!(recovery.should_attempt(started + Duration::from_millis(250)));

        let mut now = started + Duration::from_millis(250);
        for expected_delay in [500, 1_000, 2_000, 4_000, 5_000, 5_000] {
            recovery.failed(now);
            assert!(!recovery.should_attempt(now + Duration::from_millis(expected_delay - 1)));
            now += Duration::from_millis(expected_delay);
            assert!(recovery.should_attempt(now));
        }

        recovery.recovered();
        assert!(!recovery.blocks_audio());
        assert!(!recovery.should_attempt(now));
        recovery.request_stream_recovery(now);
        assert!(recovery.blocks_audio());
        recovery.failed(now);
        recovery.request_stream_recovery(now + Duration::from_millis(100));
        recovery.request_selection_change(2, now + Duration::from_millis(100));
        assert!(recovery.blocks_audio());
        assert!(recovery.should_attempt(now + Duration::from_millis(100)));
    }
}
