use std::ops::Add;
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use crate::microphone::{
    DevicePreference, InputDescription, preferred_input_indices, resolve_channel,
};
use color_eyre::eyre::{Result, WrapErr, eyre};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SizedSample, Stream, StreamConfig};

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

pub struct AudioInput {
    // Tests can deliver PCM through channels without opening a native microphone.
    _stream: Option<Stream>,
    chunks: Receiver<(Vec<f32>, CaptureInstant)>,
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
}

pub enum RecoveringAudioInputEvent {
    Chunk {
        samples: Vec<f32>,
        captured_through: CaptureInstant,
    },
    Timeout,
    Interrupted,
    Reopened,
    OpenFailed(String),
}

const MICROPHONE_RETRY_INITIAL: Duration = Duration::from_millis(250);
const MICROPHONE_RETRY_MAX: Duration = Duration::from_secs(5);
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
    reason: Option<MicrophoneRecoveryReason>,
    target_revision: Option<u64>,
}

impl Default for MicrophoneRecovery {
    fn default() -> Self {
        Self {
            next_attempt: None,
            delay: MICROPHONE_RETRY_INITIAL,
            reason: None,
            target_revision: None,
        }
    }
}

impl MicrophoneRecovery {
    fn request_selection_change(&mut self, revision: u64, now: Instant) {
        if self.target_revision != Some(revision) {
            self.next_attempt = Some(now);
            self.delay = MICROPHONE_RETRY_INITIAL;
            self.target_revision = Some(revision);
            if self.reason != Some(MicrophoneRecoveryReason::StreamFailed) {
                self.reason = Some(MicrophoneRecoveryReason::SelectionChanged);
            }
        }
    }

    fn request_stream_recovery(&mut self, now: Instant) {
        if self.reason != Some(MicrophoneRecoveryReason::StreamFailed) {
            self.next_attempt = Some(now);
            self.delay = MICROPHONE_RETRY_INITIAL;
            self.reason = Some(MicrophoneRecoveryReason::StreamFailed);
        }
    }

    fn request_routing_change(&mut self, revision: u64, now: Instant) {
        self.request_selection_change(revision, now);
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
        self.delay = self.delay.saturating_mul(2).min(MICROPHONE_RETRY_MAX);
    }

    fn recovered(&mut self) {
        self.next_attempt = None;
        self.delay = MICROPHONE_RETRY_INITIAL;
        self.reason = None;
        self.target_revision = None;
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
                },
            },
            sender,
        )
    }

    fn open_automatic(preferences: &[DevicePreference]) -> Result<Self> {
        try_input_candidates(automatic_devices(preferences)?, |device| {
            let name = device.to_string();
            Self::open_device(device).wrap_err_with(|| format!("could not open microphone {name}"))
        })
    }

    pub fn open_named(name: &str) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .input_devices()
            .wrap_err("could not enumerate input devices")?
            .find(|device| device.to_string() == name)
            .ok_or_else(|| eyre!("microphone is unavailable: {name}"))?;
        Self::open_device(device)
    }

    fn open_device(device: Device) -> Result<Self> {
        let device_name = device.to_string();
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
        let (sender, chunks) = mpsc::channel();
        let (error_sender, stream_errors) = mpsc::sync_channel(1);

        let stream = match sample_format {
            SampleFormat::F32 => build_stream(
                &device,
                &config,
                channels,
                selected_channel,
                sender,
                error_sender,
                |sample: f32| sample,
            )?,
            SampleFormat::I16 => build_stream(
                &device,
                &config,
                channels,
                selected_channel,
                sender,
                error_sender,
                |sample: i16| sample as f32 / i16::MAX as f32,
            )?,
            SampleFormat::U16 => build_stream(
                &device,
                &config,
                channels,
                selected_channel,
                sender,
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
            sample_rate,
            device_name,
            description,
            stream_errors,
        })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> AudioInputEvent {
        // A stream failure reported before or during the wait outranks any
        // chunk delivered alongside it.
        if let Ok(error) = self.stream_errors.try_recv() {
            return AudioInputEvent::StreamFailed(error);
        }
        let received = self.chunks.recv_timeout(timeout);
        if let Ok(error) = self.stream_errors.try_recv() {
            return AudioInputEvent::StreamFailed(error);
        }
        match received {
            Ok((samples, captured_through)) => AudioInputEvent::Chunk {
                samples,
                captured_through,
            },
            Err(RecvTimeoutError::Timeout) => AudioInputEvent::Timeout,
            Err(RecvTimeoutError::Disconnected) => {
                AudioInputEvent::StreamFailed("microphone audio channel disconnected".into())
            }
        }
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
            Ok(opened) => self.input = Some(opened),
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
        }
    }

    #[cfg(test)]
    pub fn pending_for_test() -> (Self, Sender<InputOpenResult>) {
        let mut input = Self::closed(
            Some("HEX nonexistent microphone for pending open test"),
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
        self.maintain_routing(now, capture_idle);
        if let Some((opened_revision, result)) = self.poll_replacement() {
            let target_revision = self
                .recovery
                .target_revision
                .unwrap_or(self.active_revision);
            if opened_revision != target_revision {
                self.recovery.next_attempt = Some(now);
                return RecoveringAudioInputEvent::Timeout;
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
                        return RecoveringAudioInputEvent::Timeout;
                    }
                    self.input = Some(replacement);
                    self.active_revision = opened_revision;
                    self.recovery.recovered();
                    tracing::info!(device = %self.device_name(), "dictation microphone reopened");
                    return RecoveringAudioInputEvent::Reopened;
                }
                Ok(_) => {
                    self.recovery.failed(now);
                }
                Err(error) => {
                    if self.input.is_none() && !self.is_recovering() {
                        self.recovery.recovered();
                        return RecoveringAudioInputEvent::OpenFailed(error);
                    }
                    self.recovery.failed(now);
                    tracing::warn!(
                        %error,
                        "could not reopen dictation microphone; retrying"
                    );
                }
            }
        }
        if (capture_idle || self.input.is_none())
            && self.recovery.should_attempt(now)
            && self.replacement.is_none()
        {
            self.start_replacement();
        }
        if self.recovery.blocks_audio() {
            std::thread::sleep(timeout);
            return RecoveringAudioInputEvent::Timeout;
        }
        let Some(input) = self.input.as_ref() else {
            std::thread::sleep(timeout);
            return RecoveringAudioInputEvent::Timeout;
        };
        match input.recv_timeout(timeout) {
            AudioInputEvent::Chunk {
                samples,
                captured_through,
            } => RecoveringAudioInputEvent::Chunk {
                samples,
                captured_through,
            },
            AudioInputEvent::Timeout => RecoveringAudioInputEvent::Timeout,
            AudioInputEvent::StreamFailed(error) => {
                tracing::warn!(%error, device = %input.device_name, "microphone stream stopped; reopening it");
                self.recovery.request_stream_recovery(Instant::now());
                RecoveringAudioInputEvent::Interrupted
            }
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.input
            .as_ref()
            .expect("microphone sample rate requested while closed")
            .sample_rate
    }

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
    let (mut input, fallback_from) = open_with_selection(
        device_override,
        selected_device,
        fallback_from_selected,
        AudioInput::open_named,
        || AudioInput::open_automatic(preferences),
    )?;
    input.description.fallback_from = fallback_from;
    Ok(input)
}

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
        return named(device).map(|input| (input, None)).or_else(|error| {
            if !fallback_from_selected {
                return Err(error);
            }
            tracing::warn!(%error, device, "selected microphone is unavailable; using automatic selection");
            automatic().map(|input| (input, Some(device.to_owned())))
        });
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

fn device_identity(device: &Device) -> DevicePreference {
    DevicePreference {
        id: device
            .id()
            .ok()
            .map(|id| id.id().to_owned())
            .filter(|id| !id.is_empty()),
        name: device.to_string(),
    }
}

/// Available inputs, retaining distinct devices even when names collide.
pub fn input_device_catalog() -> Result<Vec<DevicePreference>> {
    let mut devices = cpal::default_host()
        .input_devices()
        .wrap_err("could not enumerate input devices")?
        .map(|device| device_identity(&device))
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
        .map(|device| device.to_string())
        .collect::<Vec<_>>();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    Ok(names)
}

fn describe_device(device: &Device, channels: u16) -> InputDescription {
    let device_id = device_identity(device).id;
    let requested_channel = crate::app_settings::microphone_channel(device_id.as_deref());
    InputDescription {
        device_id,
        name: device.to_string(),
        channels,
        channel: resolve_channel(requested_channel, channels),
        requested_channel,
        fallback_from: None,
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
    let (mut description, fallback_from) = open_with_selection(
        None,
        selected_device,
        true,
        |name| {
            let device = cpal::default_host()
                .input_devices()?
                .find(|device| device.to_string() == name)
                .ok_or_else(|| eyre!("microphone is unavailable: {name}"))?;
            describe(device)
        },
        || try_input_candidates(automatic_devices(preferences)?, describe),
    )?;
    description.fallback_from = fallback_from;
    Ok(description)
}

fn automatic_device_indices(
    available: &[DevicePreference],
    preferences: &[DevicePreference],
    fallback: Option<usize>,
) -> Vec<usize> {
    let legacy;
    let preferences = if preferences.is_empty() {
        legacy = AUTOMATIC_INPUT_DEVICE_PREFERENCES
            .iter()
            .filter_map(|query| {
                available
                    .iter()
                    .find(|device| device.name.to_lowercase().contains(&query.to_lowercase()))
                    .cloned()
            })
            .collect::<Vec<_>>();
        &legacy
    } else {
        preferences
    };
    preferred_input_indices(available, preferences, fallback)
}

fn automatic_devices(preferences: &[DevicePreference]) -> Result<Vec<Device>> {
    let host = cpal::default_host();
    let mut devices: Vec<_> = host
        .input_devices()
        .wrap_err("could not enumerate input devices")?
        .collect();
    let fallback = host.default_input_device().map(|default| {
        let default = avoid_bluetooth_input(default, &devices);
        let identity = device_identity(&default);
        devices
            .iter()
            .position(|device| identity.matches(&device_identity(device)))
            .unwrap_or_else(|| {
                devices.push(default);
                devices.len() - 1
            })
    });
    let identities = devices.iter().map(device_identity).collect::<Vec<_>>();
    Ok(automatic_device_indices(&identities, preferences, fallback)
        .into_iter()
        .map(|index| devices[index].clone())
        .collect())
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputTransport {
    BuiltIn,
    Bluetooth,
    Other,
}

/// Opening a Bluetooth headset's microphone switches it into a low-quality
/// call profile for all audio, so automatic selection prefers a built-in mic.
#[cfg(target_os = "macos")]
fn automatic_input_index(default: usize, transports: &[InputTransport]) -> usize {
    if transports.get(default) != Some(&InputTransport::Bluetooth) {
        return default;
    }
    transports
        .iter()
        .position(|transport| *transport == InputTransport::BuiltIn)
        .unwrap_or(default)
}

#[cfg(target_os = "macos")]
fn avoid_bluetooth_input(default: Device, devices: &[Device]) -> Device {
    let identity = device_identity(&default);
    let Some(default_index) = devices
        .iter()
        .position(|device| identity.matches(&device_identity(device)))
    else {
        return default;
    };
    let transports = devices.iter().map(input_transport).collect::<Vec<_>>();
    let index = automatic_input_index(default_index, &transports);
    if index != default_index {
        tracing::info!(
            bluetooth = %devices[default_index],
            selected = %devices[index],
            "automatic microphone selection skipped a Bluetooth headset"
        );
    }
    devices[index].clone()
}

#[cfg(target_os = "macos")]
fn input_transport(device: &Device) -> InputTransport {
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use objc2_core_audio::{
        AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectPropertySelector, kAudioDevicePropertyTransportType,
        kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
        kAudioDeviceTransportTypeBuiltIn, kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    };
    use objc2_core_foundation::CFString;

    fn read_u32(
        object: AudioObjectID,
        selector: AudioObjectPropertySelector,
        qualifier: Option<NonNull<c_void>>,
        qualifier_size: u32,
    ) -> Option<u32> {
        let mut address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
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
        Some(NonNull::from(&uid_ref).cast()),
        std::mem::size_of::<*const CFString>() as u32,
    )
    .filter(|device_id| *device_id != 0) else {
        return InputTransport::Other;
    };
    match read_u32(device_id, kAudioDevicePropertyTransportType, None, 0) {
        Some(transport) if transport == kAudioDeviceTransportTypeBuiltIn => InputTransport::BuiltIn,
        Some(transport)
            if transport == kAudioDeviceTransportTypeBluetooth
                || transport == kAudioDeviceTransportTypeBluetoothLE =>
        {
            InputTransport::Bluetooth
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

fn send(
    sender: &Sender<(Vec<f32>, CaptureInstant)>,
    chunk: Vec<f32>,
    captured_through: CaptureInstant,
) {
    let _ = sender.send((chunk, captured_through));
}

fn stream_error(sender: &SyncSender<String>, error: cpal::Error) {
    report_stream_error(sender, error.to_string());
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
    sender: Sender<(Vec<f32>, CaptureInstant)>,
    error_sender: SyncSender<String>,
    convert: impl Fn(T) -> f32 + Send + 'static,
) -> Result<Stream> {
    let sample_rate = config.sample_rate;
    device
        .build_input_stream(
            *config,
            move |data: &[T], info| {
                let captured_through = captured_through(info, data.len() / channels, sample_rate);
                send(
                    &sender,
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

    #[cfg(target_os = "macos")]
    #[test]
    fn automatic_input_prefers_built_in_over_a_bluetooth_default() {
        use InputTransport::{Bluetooth, BuiltIn, Other};

        assert_eq!(automatic_input_index(1, &[BuiltIn, Bluetooth]), 0);
        assert_eq!(automatic_input_index(0, &[Bluetooth, Other, BuiltIn]), 2);
        // Non-Bluetooth defaults, such as USB microphones, are kept.
        assert_eq!(automatic_input_index(1, &[BuiltIn, Other]), 1);
        // Without a built-in microphone the Bluetooth default is still used.
        assert_eq!(automatic_input_index(0, &[Bluetooth, Other]), 0);
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
            Some("HEX nonexistent microphone for pending recovery test"),
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
