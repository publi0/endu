use std::collections::HashSet;
use std::ffi::c_void;
use std::mem::size_of;
use std::process::Command;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use objc2_app_kit::NSWorkspace;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    kAudioDevicePropertyMute, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
};
use objc2_core_foundation::CFString;

use crate::app_settings::{self, RecordingAudioBehavior};
use crate::volume_fade::{FADE_TICK, VolumeDevice, VolumeFade, VolumeState};

const VIRTUAL_MAIN_VOLUME: u32 = u32::from_be_bytes(*b"vmvc");
const POWER_ASSERTION_LEVEL_ON: u32 = 255;

/// A media player HEX may pause for dictation and resume afterwards.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum MediaPlayer {
    Music,
    Spotify,
    Vlc,
}

impl MediaPlayer {
    const ALL: [Self; 3] = [Self::Music, Self::Spotify, Self::Vlc];

    fn name(self) -> &'static str {
        match self {
            Self::Music => "Music",
            Self::Spotify => "Spotify",
            Self::Vlc => "VLC",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|player| player.name() == name)
    }

    fn from_bundle_id(bundle_id: &str) -> Option<Self> {
        match bundle_id {
            "com.apple.Music" => Some(Self::Music),
            "com.spotify.client" => Some(Self::Spotify),
            "org.videolan.vlc" => Some(Self::Vlc),
            _ => None,
        }
    }

    /// The AppleScript condition that is true while the player is playing.
    fn playing_clause(self) -> &'static str {
        match self {
            Self::Music | Self::Spotify => "player state is playing",
            Self::Vlc => "playing",
        }
    }

    fn pause_fragment(self) -> String {
        let name = self.name();
        let playing = self.playing_clause();
        format!(
            "\ntry\n  if application \"{name}\" is running then\n    tell application \"{name}\"\n      if {playing} then\n        pause\n        set end of pausedPlayers to \"{name}\"\n      end if\n    end tell\n  end if\nend try\n"
        )
    }

    fn resume_fragment(self) -> String {
        let name = self.name();
        format!(
            "try\n  if application \"{name}\" is running then tell application \"{name}\" to play\nend try"
        )
    }
}

struct RecordingEnvironment {
    sleep: Option<PreventSleep>,
    audio: AudioBehaviorGuard,
    behavior: RecordingAudioBehavior,
    lower_volume_percent: u8,
}

impl RecordingEnvironment {
    pub fn start() -> Self {
        let behavior = app_settings::recording_audio_behavior();
        let lower_volume_percent = app_settings::lower_volume_percent();
        Self {
            sleep: prevent_sleep(),
            audio: AudioBehaviorGuard::start(behavior, lower_volume_percent),
            behavior,
            lower_volume_percent,
        }
    }
}

trait EnvironmentState {
    fn set_active(&mut self, _active: bool, _now: Instant) {}
    fn tick(&mut self, _now: Instant) {}
    fn needs_tick(&self) -> bool {
        false
    }
    fn restoring(&self) -> bool {
        false
    }
    fn can_reactivate(&mut self) -> bool {
        true
    }
}

impl EnvironmentState for RecordingEnvironment {
    fn set_active(&mut self, active: bool, now: Instant) {
        if active && self.sleep.is_none() {
            self.sleep = prevent_sleep();
        }
        if !active {
            self.sleep = None;
        }
        self.audio.set_active(active, now);
    }
    fn tick(&mut self, now: Instant) {
        if let AudioBehaviorGuard::Faded(fade) = &mut self.audio {
            fade.tick(now);
        }
    }
    fn needs_tick(&self) -> bool {
        matches!(&self.audio, AudioBehaviorGuard::Faded(fade) if fade.is_animating())
    }
    fn restoring(&self) -> bool {
        matches!(&self.audio, AudioBehaviorGuard::Faded(fade) if fade.is_restoring())
    }
    fn can_reactivate(&mut self) -> bool {
        self.behavior == app_settings::recording_audio_behavior()
            && (self.behavior != RecordingAudioBehavior::LowerVolume
                || self.lower_volume_percent == app_settings::lower_volume_percent())
            && match &mut self.audio {
                AudioBehaviorGuard::Faded(fade) => fade.can_reactivate(),
                _ => true,
            }
    }
}

#[cfg(test)]
impl EnvironmentState for () {}

enum EnvironmentCommand {
    Start,
    Stop,
    #[cfg(test)]
    Barrier(Sender<()>),
}

#[derive(Clone)]
pub struct RecordingEnvironmentController {
    // Senders drop before the last worker owner joins, closing its receiver.
    commands: Sender<EnvironmentCommand>,
    worker: Arc<EnvironmentWorker>,
}

struct EnvironmentWorker(Option<thread::JoinHandle<()>>);
impl Drop for EnvironmentWorker {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            // Volume restoration normally finishes as soon as the channel closes.
            // A stalled media-player Apple event must not trap application shutdown.
            let deadline = Instant::now() + Duration::from_millis(250);
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if worker.is_finished() && worker.join().is_err() {
                tracing::error!("recording environment worker panicked");
            }
        }
    }
}

impl RecordingEnvironmentController {
    pub fn start() -> Self {
        Self::with_environment(RecordingEnvironment::start)
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::with_environment(|| ())
    }

    fn with_environment<E: EnvironmentState>(start: impl Fn() -> E + Send + 'static) -> Self {
        let (commands, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut sessions = 0_u32;
            let mut environment: Option<E> = None;
            loop {
                let command = if environment
                    .as_ref()
                    .is_some_and(EnvironmentState::needs_tick)
                {
                    receiver.recv_timeout(FADE_TICK)
                } else {
                    receiver
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                let now = Instant::now();
                match command {
                    Ok(EnvironmentCommand::Start) => {
                        sessions = sessions.saturating_add(1);
                        if sessions == 1 {
                            if environment
                                .as_mut()
                                .is_some_and(|value| !value.can_reactivate())
                            {
                                environment = None;
                            }
                            if let Some(value) = &mut environment {
                                value.set_active(true, now);
                            } else {
                                environment = Some(start());
                            }
                        }
                    }
                    Ok(EnvironmentCommand::Stop) => {
                        sessions = sessions.saturating_sub(1);
                        if sessions == 0
                            && let Some(value) = &mut environment
                        {
                            value.set_active(false, now);
                        }
                    }
                    #[cfg(test)]
                    Ok(EnvironmentCommand::Barrier(reply)) => {
                        let _ = reply.send(());
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if let Some(value) = &mut environment {
                    value.tick(now);
                    if sessions == 0 && !value.restoring() {
                        environment = None;
                    }
                }
            }
        });
        Self {
            commands,
            worker: Arc::new(EnvironmentWorker(Some(worker))),
        }
    }

    pub fn begin(&self) -> RecordingEnvironmentSession {
        let _ = self.commands.send(EnvironmentCommand::Start);
        RecordingEnvironmentSession {
            commands: self.commands.clone(),
            _worker: self.worker.clone(),
        }
    }
}

pub struct RecordingEnvironmentSession {
    commands: Sender<EnvironmentCommand>,
    _worker: Arc<EnvironmentWorker>,
}

impl Drop for RecordingEnvironmentSession {
    fn drop(&mut self) {
        let _ = self.commands.send(EnvironmentCommand::Stop);
    }
}

pub fn prevent_sleep() -> Option<PreventSleep> {
    match PreventSleep::start() {
        Ok(prevention) => Some(prevention),
        Err(error) => {
            tracing::warn!(%error, "could not prevent idle system sleep");
            None
        }
    }
}

pub struct PreventSleep {
    assertion_id: u32,
}

impl PreventSleep {
    fn start() -> std::io::Result<Self> {
        let assertion_type = CFString::from_static_str("NoIdleSleepAssertion");
        let assertion_name = CFString::from_static_str("Endu intentional recording");
        let mut assertion_id = 0;
        // SAFETY: Both Core Foundation strings remain alive for the call and
        // assertion_id points to writable, correctly sized storage. IOKit
        // retains the assertion independently until IOPMAssertionRelease.
        let status = unsafe {
            IOPMAssertionCreateWithName(
                (&*assertion_type as *const CFString).cast(),
                POWER_ASSERTION_LEVEL_ON,
                (&*assertion_name as *const CFString).cast(),
                &mut assertion_id,
            )
        };
        if status != 0 {
            return Err(std::io::Error::other(format!(
                "IOPMAssertionCreateWithName failed with IOReturn 0x{:08x}",
                status as u32
            )));
        }
        Ok(Self { assertion_id })
    }
}

impl Drop for PreventSleep {
    fn drop(&mut self) {
        // SAFETY: assertion_id was returned by a successful create call and
        // this guard is its sole owner, so release occurs exactly once.
        let status = unsafe { IOPMAssertionRelease(self.assertion_id) };
        if status != 0 {
            tracing::warn!(
                status = format_args!("0x{:08x}", status as u32),
                "could not release idle-sleep assertion"
            );
        }
    }
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: *const c_void,
        assertion_level: u32,
        assertion_name: *const c_void,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

enum AudioBehaviorGuard {
    Faded(VolumeFade<CoreAudioVolume>),
    Paused { players: Vec<MediaPlayer> },
    None,
}

impl AudioBehaviorGuard {
    fn start(behavior: RecordingAudioBehavior, lower_volume_percent: u8) -> Self {
        match behavior {
            RecordingAudioBehavior::Mute => default_output_device()
                .and_then(|device| VolumeFade::new(CoreAudioVolume(device), Instant::now()))
                .map_or(Self::None, Self::Faded),
            RecordingAudioBehavior::LowerVolume => default_output_device()
                .and_then(|device| {
                    VolumeFade::with_remaining_volume(
                        CoreAudioVolume(device),
                        f32::from(lower_volume_percent.min(100)) / 100.0,
                        Instant::now(),
                    )
                })
                .map_or(Self::None, Self::Faded),
            RecordingAudioBehavior::PauseMedia => {
                let players = pause_media();
                if players.is_empty() {
                    Self::None
                } else {
                    tracing::info!(?players, "paused media for dictation");
                    Self::Paused { players }
                }
            }
            RecordingAudioBehavior::DoNothing => Self::None,
        }
    }

    fn set_active(&mut self, active: bool, now: Instant) {
        match self {
            Self::Faded(fade) => fade.set_active(active, now),
            Self::Paused { players } if !active => {
                resume_media(players);
                players.clear();
            }
            _ => {}
        }
    }
}

impl Drop for AudioBehaviorGuard {
    fn drop(&mut self) {
        match self {
            Self::Paused { players } => resume_media(players),
            Self::Faded(_) | Self::None => {}
        }
    }
}

struct CoreAudioVolume(u32);

impl VolumeDevice for CoreAudioVolume {
    fn is_current_output(&mut self) -> bool {
        default_output_device() == Some(self.0)
    }
    fn read(&mut self) -> Option<VolumeState> {
        Some(VolumeState {
            volume: output_volume(self.0)?,
            muted: output_muted(self.0),
        })
    }
    fn write(&mut self, volume: f32) -> bool {
        set_output_volume(self.0, volume)
    }
}

fn output_muted(device: u32) -> Option<bool> {
    let mut address = volume_address();
    address.mSelector = kAudioDevicePropertyMute;
    let mut muted = 0_u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: each pointer refers to correctly sized, initialized stack storage.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut muted).cast::<c_void>(),
        )
    };
    (status == 0 && size == size_of::<u32>() as u32).then_some(muted != 0)
}

fn default_output_device() -> Option<u32> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = size_of::<u32>() as u32;
    let mut device = 0_u32;
    // SAFETY: All pointers reference initialized, correctly sized stack values.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as u32,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut device).cast::<c_void>(),
        )
    };
    (status == 0 && device != 0).then_some(device)
}

fn output_volume(device: u32) -> Option<f32> {
    let mut address = volume_address();
    let mut size = size_of::<f32>() as u32;
    let mut volume = 0.0_f32;
    // SAFETY: All pointers reference initialized, correctly sized stack values.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut volume).cast::<c_void>(),
        )
    };
    (status == 0).then_some(volume)
}

fn set_output_volume(device: u32, volume: f32) -> bool {
    let mut address = volume_address();
    let mut volume = volume;
    // SAFETY: All pointers reference initialized, correctly sized stack values.
    unsafe {
        AudioObjectSetPropertyData(
            device,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            size_of::<f32>() as u32,
            NonNull::from(&mut volume).cast::<c_void>(),
        ) == 0
    }
}

fn volume_address() -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: VIRTUAL_MAIN_VOLUME,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn pause_media() -> Vec<MediaPlayer> {
    let running = running_media_players();
    if running.is_empty() {
        return Vec::new();
    }
    let script = pause_media_script(&running);
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        tracing::warn!(
            error = %String::from_utf8_lossy(&output.stderr).trim(),
            "could not pause media for dictation"
        );
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .split(',')
        .map(str::trim)
        .filter_map(MediaPlayer::from_name)
        .collect()
}

fn resume_media(players: &[MediaPlayer]) {
    let running = running_media_players();
    let script = players
        .iter()
        .filter(|player| running.contains(player))
        .map(|player| player.resume_fragment())
        .collect::<Vec<_>>()
        .join("\n");
    if script.is_empty() {
        return;
    }
    match Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
    {
        Ok(output) if output.status.success() => {
            tracing::info!(?players, "resumed media after dictation")
        }
        Ok(output) => tracing::warn!(
            error = %String::from_utf8_lossy(&output.stderr).trim(),
            "could not resume media after dictation"
        ),
        Err(error) => tracing::warn!(%error, "could not resume media after dictation"),
    }
}

fn running_media_players() -> HashSet<MediaPlayer> {
    objc2::rc::autoreleasepool(|_| {
        NSWorkspace::sharedWorkspace()
            .runningApplications()
            .iter()
            .filter_map(|application| {
                MediaPlayer::from_bundle_id(&application.bundleIdentifier()?.to_string())
            })
            .collect()
    })
}

fn pause_media_script(players: &HashSet<MediaPlayer>) -> String {
    let mut script = String::from("set pausedPlayers to {}\n");
    for player in MediaPlayer::ALL {
        if players.contains(&player) {
            script.push_str(&player.pause_fragment());
        }
    }
    script.push_str("return pausedPlayers\n");
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};

    #[derive(Debug, PartialEq)]
    enum Event {
        Started,
        Restored,
    }

    struct ObservedEnvironment(Sender<Event>);

    impl EnvironmentState for ObservedEnvironment {}

    impl Drop for ObservedEnvironment {
        fn drop(&mut self) {
            let _ = self.0.send(Event::Restored);
        }
    }

    fn observed_controller() -> (RecordingEnvironmentController, Receiver<Event>) {
        let (events, receiver) = mpsc::channel();
        let controller = RecordingEnvironmentController::with_environment(move || {
            let _ = events.send(Event::Started);
            ObservedEnvironment(events.clone())
        });
        (controller, receiver)
    }

    fn assert_events(
        commands: &Sender<EnvironmentCommand>,
        events: &Receiver<Event>,
        expected: &[Event],
    ) {
        let (reply, response) = mpsc::channel();
        commands.send(EnvironmentCommand::Barrier(reply)).unwrap();
        response.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(events.try_iter().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn sessions_are_harmless_after_the_environment_worker_disconnects() {
        let (commands, receiver) = mpsc::channel();
        drop(receiver);
        let controller = RecordingEnvironmentController {
            commands,
            worker: Arc::new(EnvironmentWorker(None)),
        };
        drop(controller.begin());
    }

    #[test]
    #[ignore = "exercises the native macOS idle-sleep assertion"]
    fn native_idle_sleep_assertion_acquires_and_releases() {
        drop(PreventSleep::start().unwrap());
    }

    #[test]
    fn overlapping_sessions_restore_only_after_the_last_session() {
        let (controller, events) = observed_controller();
        assert_events(&controller.commands, &events, &[]);

        let first = controller.begin();
        let second = controller.begin();
        assert_events(&controller.commands, &events, &[Event::Started]);

        drop(first);
        assert_events(&controller.commands, &events, &[]);

        let third = controller.begin();
        drop(second);
        assert_events(&controller.commands, &events, &[]);

        drop(third);
        assert_events(&controller.commands, &events, &[Event::Restored]);
    }

    #[test]
    fn a_new_session_reacquires_the_environment_after_restoration() {
        let (controller, events) = observed_controller();
        for _ in 0..2 {
            let session = controller.begin();
            assert_events(&controller.commands, &events, &[Event::Started]);
            drop(session);
            assert_events(&controller.commands, &events, &[Event::Restored]);
        }
    }

    #[test]
    fn quick_restart_reuses_a_fade_only_while_it_owns_the_output() {
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Debug, PartialEq)]
        enum Transition {
            Started,
            Active(bool),
            Dropped,
        }
        struct FadingEnvironment {
            events: Sender<Transition>,
            owned: Arc<AtomicBool>,
            restoring: bool,
        }
        impl EnvironmentState for FadingEnvironment {
            fn set_active(&mut self, active: bool, _: Instant) {
                self.restoring = !active;
                self.events.send(Transition::Active(active)).unwrap();
            }
            fn restoring(&self) -> bool {
                self.restoring
            }
            fn can_reactivate(&mut self) -> bool {
                self.owned.load(Ordering::SeqCst)
            }
        }
        impl Drop for FadingEnvironment {
            fn drop(&mut self) {
                self.events.send(Transition::Dropped).unwrap();
            }
        }

        let (events, observed) = mpsc::channel();
        let owned = Arc::new(AtomicBool::new(true));
        let worker_owned = owned.clone();
        let controller = RecordingEnvironmentController::with_environment(move || {
            events.send(Transition::Started).unwrap();
            FadingEnvironment {
                events: events.clone(),
                owned: worker_owned.clone(),
                restoring: false,
            }
        });
        let check = |expected: &[Transition]| {
            let (reply, response) = mpsc::channel();
            controller
                .commands
                .send(EnvironmentCommand::Barrier(reply))
                .unwrap();
            response.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(observed.try_iter().collect::<Vec<_>>(), expected);
        };
        let first = controller.begin();
        drop(first);
        check(&[Transition::Started, Transition::Active(false)]);
        let second = controller.begin();
        check(&[Transition::Active(true)]);
        drop(second);
        check(&[Transition::Active(false)]);
        owned.store(false, Ordering::SeqCst);
        let third = controller.begin();
        check(&[Transition::Dropped, Transition::Started]);
        drop(third);
        check(&[Transition::Active(false)]);
        drop(controller);
        // The last worker owner waits for guarded restoration on shutdown.
        assert_eq!(
            observed.try_iter().collect::<Vec<_>>(),
            [Transition::Dropped]
        );
    }

    #[test]
    fn controller_moves_and_clones_do_not_restore_a_live_session() {
        let (controller, events) = observed_controller();
        let session = controller.begin();
        assert_events(&controller.commands, &events, &[Event::Started]);

        let cloned = controller.clone();
        let moved = controller;
        drop(moved);
        assert_events(&cloned.commands, &events, &[]);

        let overlapping = cloned.begin();
        drop(cloned);
        assert_events(&session.commands, &events, &[]);

        drop(session);
        assert_events(&overlapping.commands, &events, &[]);

        drop(overlapping);
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Restored
        );
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn pause_script_never_resolves_players_that_are_not_running() {
        let players = HashSet::from([MediaPlayer::Music, MediaPlayer::Spotify]);

        let script = pause_media_script(&players);

        assert!(script.contains("application \"Music\""));
        assert!(script.contains("application \"Spotify\""));
        assert!(!script.contains("application \"VLC\""));

        let script = pause_media_script(&HashSet::from([MediaPlayer::Vlc]));

        assert_eq!(
            script,
            r#"set pausedPlayers to {}

try
  if application "VLC" is running then
    tell application "VLC"
      if playing then
        pause
        set end of pausedPlayers to "VLC"
      end if
    end tell
  end if
end try
return pausedPlayers
"#
        );
        assert!(!script.contains("player state is playing"));
        assert!(
            pause_media_script(&HashSet::from([MediaPlayer::Music]))
                .contains("if player state is playing then")
        );
    }
}
