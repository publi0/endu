//! The hotkey dictation listener: the shortcut machine, the authoritative
//! microphone timeline, and the transcription pipeline, wired together on one
//! control loop. Capture never waits on transcription or paste.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::Duration;

use color_eyre::Result;

use crate::audio::CaptureInstant;
use crate::context::{ContextMonitor, ContextSnapshot};
use crate::dictation::Finish;
use crate::dictation_audio::{CaptureStart, DictationAudio, DictationAudioEvent};
use crate::dictation_indicator::{DictationIndicatorEvent, DictationIndicatorSender};
use crate::doorbell::Doorbell;
use crate::events::{DictationPhase, EventLog, VoiceEvent, VoiceState, now_ms};
use crate::feedback::{self, Tone};
use crate::pipeline::{DictationWorker, WorkerEvent};
use crate::recording_environment::RecordingEnvironmentController;
use crate::suppression::{DictationHotkey, HotkeyAction, InputMonitor};

fn capture_start_event(start: CaptureStart) -> Option<DictationIndicatorEvent> {
    match start {
        CaptureStart::Rejected => None,
        CaptureStart::Opening => Some(DictationIndicatorEvent::Preparing),
        CaptureStart::Ready => Some(DictationIndicatorEvent::Started),
    }
}

/// While recording, pending or recovering, the loop turns every 20 ms to
/// drive the HUD meter and time-based gesture repair.
const ACTIVE_TURN: Duration = Duration::from_millis(20);

/// An idle loop blocks until input, a control or this fallback. Only
/// bookkeeping that needs no prompt reaction waits for it: settings and
/// microphone selection (applied between clips), foreground context, and
/// audio notifications while no capture exists. Every input edge first drains
/// all of those, exactly as an active turn does.
const IDLE_TURN: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum ListenerControl {
    PasteLast,
}

/// The sending half of the listener's control queue. It also wakes an idle
/// listener so a control never waits for the idle fallback.
#[derive(Clone)]
pub struct ListenerControls {
    sender: SyncSender<ListenerControl>,
    wake: Arc<Doorbell>,
}

impl ListenerControls {
    pub fn try_send(&self, control: ListenerControl) -> Result<(), TrySendError<ListenerControl>> {
        self.sender.try_send(control)?;
        self.wake.ring();
        Ok(())
    }

    /// Wakes an idle listener, so it observes shutdown at once.
    pub fn wake(&self) {
        self.wake.ring();
    }
}

pub struct ListenerControlReceiver {
    receiver: Receiver<ListenerControl>,
    wake: Arc<Doorbell>,
}

pub fn control_channel(capacity: usize) -> (ListenerControls, ListenerControlReceiver) {
    let (sender, receiver) = mpsc::sync_channel(capacity);
    let wake = Arc::new(Doorbell::new());
    (
        ListenerControls {
            sender,
            wake: wake.clone(),
        },
        ListenerControlReceiver { receiver, wake },
    )
}

/// Everything one control-loop iteration needs to report a capture edge.
struct Session<'a> {
    input: &'a DictationAudio,
    worker: &'a DictationWorker,
    events: &'a EventLog,
    indicator: Option<&'a DictationIndicatorSender>,
    recording_context: RefCell<Option<ContextSnapshot>>,
}

impl Session<'_> {
    fn device(&self) -> String {
        self.input.device_name()
    }

    fn indicate(&self, event: DictationIndicatorEvent) {
        if let Some(indicator) = self.indicator {
            indicator.send(event);
        }
    }

    fn emit_state(&self, capturing: bool) -> Result<()> {
        self.events.emit(&VoiceEvent::State {
            timestamp_ms: now_ms(),
            state: if capturing {
                VoiceState::Dictating
            } else if self.worker.is_busy() {
                VoiceState::Transcribing
            } else {
                VoiceState::Listening
            },
            device: self.device(),
        })?;
        Ok(())
    }

    /// Applies one shortcut action. Returns `false` when a start was refused
    /// so the caller can suspend the shortcut machine.
    fn handle_hotkey(
        &self,
        action: HotkeyAction,
        at: CaptureInstant,
        submit: Option<u64>,
    ) -> Result<bool> {
        match action {
            HotkeyAction::Start => {
                let target = ContextSnapshot::capture().unwrap_or_default();
                let start = self.input.start(at)?;
                let Some(indicator_event) = capture_start_event(start) else {
                    return Ok(false);
                };
                *self.recording_context.borrow_mut() = Some(target);
                self.input.invalidate_recognition()?;
                self.worker.prepare_paste();
                self.events.dictation(DictationPhase::Started, "")?;
                self.indicate(indicator_event);
                self.emit_state(true)?;
            }
            HotkeyAction::Finish => {
                if self.input.become_intentional(at)? {
                    feedback::play(Tone::DictationStart);
                }
                self.finish(at, None)?;
            }
            HotkeyAction::FinishAndSubmit => {
                if self.input.become_intentional(at)? {
                    feedback::play(Tone::DictationStart);
                }
                self.finish(at, submit)?;
            }
            HotkeyAction::Discard => self.end(CaptureEnd::Discarded)?,
            HotkeyAction::Cancel => self.end(CaptureEnd::Cancelled)?,
            HotkeyAction::PasteLast => self.paste_last()?,
        }
        Ok(true)
    }

    fn paste_last(&self) -> Result<()> {
        let target = ContextSnapshot::capture().unwrap_or_default();
        // A repaste ends any capture, including a double-tap lock. Keep the
        // HUD in sync with the audio owner before queueing the previous text.
        self.end(CaptureEnd::Discarded)?;
        if let Err(error) = self.worker.paste_last(target) {
            feedback::play(Tone::Error);
            self.events
                .dictation(DictationPhase::Failed(error.into()), "")?;
        }
        self.emit_state(false)
    }

    fn finish(&self, at: CaptureInstant, submit: Option<u64>) -> Result<()> {
        let context = self
            .recording_context
            .borrow_mut()
            .take()
            .unwrap_or_default();
        let Finish::Transcribe(clip) = self.input.finish(at)? else {
            self.events.dictation(DictationPhase::Discarded, "")?;
            self.indicate(DictationIndicatorEvent::Discarded);
            return self.emit_state(false);
        };
        feedback::play(Tone::DictationStop);
        self.events.dictation(DictationPhase::Transcribing, "")?;
        match self.worker.transcribe(clip, context, submit) {
            Ok(job_id) => self.indicate(DictationIndicatorEvent::Submitted {
                job_id: job_id.value(),
            }),
            Err(error) => {
                feedback::play(Tone::Error);
                self.events
                    .dictation(DictationPhase::Failed(error.into()), "")?;
                self.indicate(DictationIndicatorEvent::Failed);
            }
        }
        self.emit_state(false)
    }

    fn end(&self, end: CaptureEnd) -> Result<()> {
        self.input.cancel()?;
        self.recording_context.borrow_mut().take();
        let (phase, indicator_event) = match end {
            CaptureEnd::Discarded => (
                DictationPhase::Discarded,
                DictationIndicatorEvent::Discarded,
            ),
            CaptureEnd::Cancelled => {
                feedback::play(Tone::Cancel);
                (
                    DictationPhase::Cancelled,
                    DictationIndicatorEvent::Cancelled,
                )
            }
        };
        self.events.dictation(phase, "")?;
        self.indicate(indicator_event);
        self.emit_state(false)
    }

    fn fail(&self, message: String) -> Result<()> {
        self.recording_context.borrow_mut().take();
        feedback::play(Tone::Error);
        self.events.dictation(DictationPhase::Failed(message), "")?;
        self.indicate(DictationIndicatorEvent::Failed);
        self.emit_state(false)
    }

    fn worker_event(&self, event: WorkerEvent) -> Result<()> {
        match event {
            WorkerEvent::ReadyToPaste {
                job_id,
                copied_to_clipboard,
            } => {
                self.events.dictation(
                    if copied_to_clipboard {
                        DictationPhase::CopiedToClipboard
                    } else {
                        DictationPhase::ReadyToPaste
                    },
                    "",
                )?;
                self.indicate(match job_id {
                    Some(job_id) => DictationIndicatorEvent::JobReadyToPaste {
                        job_id: job_id.value(),
                        copied_to_clipboard,
                    },
                    None => DictationIndicatorEvent::ReadyToPaste {
                        copied_to_clipboard,
                    },
                });
            }
            WorkerEvent::Completed {
                job_id,
                result: Ok(text),
            } => {
                // An empty result means the clip held no speech: the HUD says
                // so instead of showing a check over nothing.
                if text.trim().is_empty() {
                    self.events.dictation(DictationPhase::Discarded, text)?;
                    self.indicate(DictationIndicatorEvent::JobNoAudio {
                        job_id: job_id.value(),
                    });
                } else {
                    self.indicate(DictationIndicatorEvent::PasteCommitted);
                    self.events.dictation(DictationPhase::Pasted, text)?;
                    self.indicate(DictationIndicatorEvent::JobCompleted {
                        job_id: job_id.value(),
                    });
                }
            }
            WorkerEvent::Completed {
                job_id,
                result: Err(error),
            } => {
                tracing::error!(%error, "dictation failed");
                feedback::play(Tone::Error);
                let no_speech = error == crate::openrouter::transcribe::NO_SPEECH;
                self.events.dictation(DictationPhase::Failed(error), "")?;
                self.indicate(if no_speech {
                    // Streamed silence fails, and the HUD names the reason.
                    DictationIndicatorEvent::JobNoAudio {
                        job_id: job_id.value(),
                    }
                } else {
                    DictationIndicatorEvent::JobFailed {
                        job_id: job_id.value(),
                    }
                });
            }
            WorkerEvent::Quiet { job_id } => {
                self.indicate(DictationIndicatorEvent::JobQuiet {
                    job_id: job_id.value(),
                });
            }
            WorkerEvent::Transcribing { job_id } => {
                self.indicate(DictationIndicatorEvent::Transcribing {
                    job_id: job_id.value(),
                });
            }
            WorkerEvent::Cancelled { .. } => {}
            WorkerEvent::Pasted { result: Ok(text) } => {
                self.indicate(DictationIndicatorEvent::PasteCommitted);
                self.events.dictation(DictationPhase::Repasted, text)?;
            }
            WorkerEvent::Pasted { result: Err(error) } => {
                feedback::play(Tone::Error);
                self.events.dictation(DictationPhase::Failed(error), "")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum CaptureEnd {
    /// The capture never became a dictation; no feedback tone.
    Discarded,
    /// The user cancelled an intentional capture; plays the cancel tone.
    Cancelled,
}

/// The instant a shortcut edge applies to. A released microphone has no
/// authoritative timeline yet, so the edge uses wall-clock time instead.
fn capture_boundary(input: &DictationAudio, release_while_idle: bool) -> CaptureInstant {
    if release_while_idle {
        CaptureInstant::now()
    } else {
        input.captured_through()
    }
}

pub fn listen(
    events: EventLog,
    device_override: Option<&str>,
    shutdown: &AtomicBool,
    indicator: Option<DictationIndicatorSender>,
    history: Option<crate::history::History>,
    controls: Option<ListenerControlReceiver>,
    recovery: crate::recording_recovery::RecordingRecovery,
) -> Result<()> {
    shutdown.store(false, Ordering::Relaxed);
    // Feedback admission is advisory: a cold audio stack can exceed the
    // preload timeout, and missing tones must not stop dictation.
    if let Err(error) = feedback::preload() {
        tracing::warn!(%error, "recording sounds are unavailable; continuing without feedback");
    }
    let mut release_while_idle = crate::app_settings::release_microphone_while_idle();
    let wake = controls
        .as_ref()
        .map_or_else(Arc::default, |controls| controls.wake.clone());
    let input_monitor = InputMonitor::start(wake.clone())?;
    let context_monitor = ContextMonitor::start();
    let mut context = ContextSnapshot::default();
    let mut hotkey = DictationHotkey::new(
        CaptureInstant::now(),
        crate::app_settings::dictation_mode(),
        crate::app_settings::dictation_hotkey(),
    );
    hotkey.set_double_tap_only(crate::app_settings::double_tap_only());
    hotkey.set_double_tap_sensitivity(crate::app_settings::double_tap_sensitivity());
    let recording_environment = RecordingEnvironmentController::start();
    let (mut microphone_revision, microphone, priority) =
        crate::app_settings::microphone_selection();
    let input = DictationAudio::open(
        device_override,
        microphone_revision,
        microphone.as_deref(),
        &priority,
        recording_environment,
        input_monitor.pending_events(),
        release_while_idle,
    )?;
    let worker = DictationWorker::start(
        input_monitor.activity.clone(),
        history,
        recovery,
        wake.clone(),
    );
    let session = Session {
        input: &input,
        worker: &worker,
        events: &events,
        indicator: indicator.as_ref(),
        recording_context: RefCell::new(None),
    };

    events.emit(&VoiceEvent::SessionStarted {
        timestamp_ms: now_ms(),
    })?;
    if input.is_recovering() {
        hotkey.suspend();
        input_monitor.reset_submit_guard(false);
    }
    if hotkey.is_recording() {
        let target = ContextSnapshot::capture().unwrap_or_default();
        if let Some(indicator_event) =
            capture_start_event(input.start(capture_boundary(&input, release_while_idle))?)
        {
            *session.recording_context.borrow_mut() = Some(target);
            events.dictation(DictationPhase::Started, "")?;
            session.indicate(indicator_event);
        } else {
            hotkey.suspend();
            input_monitor.reset_submit_guard(false);
        }
    }
    session.emit_state(hotkey.is_recording())?;
    tracing::info!(device = %input.device_name(), "dictation listener started");

    while !shutdown.load(Ordering::Relaxed) {
        if let Some(controls) = &controls {
            while let Ok(control) = controls.receiver.try_recv() {
                match control {
                    ListenerControl::PasteLast => {
                        hotkey.suspend();
                        input_monitor.reset_submit_guard(false);
                        session.paste_last()?;
                    }
                }
            }
        }
        while let Ok(next_context) = context_monitor.updates.try_recv() {
            if context != next_context {
                input_monitor.activity.invalidate();
            }
            context = next_context;
            events.emit(&VoiceEvent::Context {
                timestamp_ms: now_ms(),
                application: context.application.clone(),
            })?;
        }

        let next_release_while_idle = crate::app_settings::release_microphone_while_idle();
        if next_release_while_idle != release_while_idle {
            input.set_release_while_idle(next_release_while_idle);
            release_while_idle = next_release_while_idle;
        }
        refresh_hotkey_settings(&mut hotkey);
        let (next_microphone_revision, microphone, priority) =
            crate::app_settings::microphone_selection();
        if next_microphone_revision != microphone_revision {
            input.request_selection(next_microphone_revision, microphone.as_deref(), &priority);
            microphone_revision = next_microphone_revision;
        }

        let suspended = crate::app_settings::hotkey_capture_active() || input.is_recovering();
        if suspended && hotkey.suspend().is_some() {
            input_monitor.reset_submit_guard(false);
            session.end(CaptureEnd::Cancelled)?;
        }
        while let Ok(observed) = input_monitor.events.try_recv() {
            let _acknowledge = input_monitor.acknowledge_after(observed);
            if suspended {
                hotkey.track_key_state(observed.event, observed.capture_at);
                continue;
            }
            // A prior edge in this batch may have finished a capture whose old
            // mode/binding was intentionally retained. Match the predictor before
            // interpreting the next edge under the newly saved preferences.
            refresh_hotkey_settings(&mut hotkey);
            let input_event = observed.event;
            if !hotkey.is_recording()
                && input_event.is_escape_down()
                && let Some(job_id) = worker.cancel_latest()
            {
                feedback::play(Tone::Cancel);
                events.dictation(DictationPhase::Cancelled, "")?;
                session.indicate(DictationIndicatorEvent::JobCancelled {
                    job_id: job_id.value(),
                });
                continue;
            }
            let mut action = hotkey.process(input_event, observed.capture_at);
            if observed.submit_epoch == Some(input_monitor.submit_epoch()) && hotkey.finish_locked()
            {
                action = Some(HotkeyAction::FinishAndSubmit);
            }
            if let Some(action) = action
                && !session.handle_hotkey(
                    action,
                    observed.capture_at,
                    observed.submit_epoch.map(|_| observed.interaction_revision),
                )?
            {
                hotkey.suspend();
                input_monitor.reset_submit_guard(false);
            }
        }
        if !suspended
            && input_monitor.pending_events().oldest().is_none()
            && hotkey.recover_stale_keys()
        {
            input_monitor.reset_submit_guard(hotkey.is_locked());
        }
        if hotkey.is_recording()
            && input.become_intentional(capture_boundary(&input, release_while_idle))?
        {
            feedback::play(Tone::DictationStart);
        }

        let mut received_worker_event = false;
        while let Some(event) = worker.try_recv() {
            received_worker_event = true;
            session.worker_event(event)?;
        }
        input_monitor.set_escape_cancels(hotkey.is_recording() || worker.pending_count() > 0);
        if received_worker_event {
            session.emit_state(hotkey.is_recording())?;
        }

        while let Some(audio_event) = input.try_recv_event() {
            match audio_event {
                DictationAudioEvent::CaptureReady { .. } => {
                    if input.is_recording() {
                        session.indicate(DictationIndicatorEvent::Started);
                    }
                }
                DictationAudioEvent::ReadyIntentional { .. } => {
                    if input.is_recording() {
                        feedback::play(Tone::DictationStart);
                    }
                }
                DictationAudioEvent::OpenFailed { error, .. } => {
                    hotkey.suspend();
                    input_monitor.reset_submit_guard(false);
                    session.fail(format!("Could not open microphone: {error}"))?;
                }
                DictationAudioEvent::RecognitionDiscontinuity { dropped_frames } => {
                    tracing::debug!(dropped_frames, "HUD meter audio fell behind");
                }
                DictationAudioEvent::CaptureDiscontinuity {
                    was_recording,
                    gap_ms,
                    ..
                } => {
                    input.discard_recognition_backlog();
                    if was_recording {
                        hotkey.suspend();
                        input_monitor.reset_submit_guard(false);
                        session
                            .fail(format!("Microphone audio was interrupted for {gap_ms} ms."))?;
                    }
                }
                DictationAudioEvent::Reopened => {
                    input.discard_recognition_backlog();
                    if !input.is_recording() {
                        session.emit_state(false)?;
                    }
                }
                DictationAudioEvent::Interrupted { was_recording, .. } => {
                    hotkey.suspend();
                    input_monitor.reset_submit_guard(false);
                    input.discard_recognition_backlog();
                    if was_recording {
                        session.fail("Microphone capture was interrupted; reconnecting.".into())?;
                    }
                }
            }
        }

        // Nothing is recording, pending or recovering, and no input awaits
        // processing: sleep until the next edge, control or fallback instead
        // of polling. Capture boundaries come from event timestamps, so the
        // wait never moves them.
        let idle = hotkey.is_quiescent()
            && !input.is_recording()
            && !input.is_recovering()
            && !worker.is_busy()
            && input_monitor.pending_events().oldest().is_none();
        if idle {
            input.discard_recognition_backlog();
            wake.wait_timeout(IDLE_TURN);
            continue;
        }

        // The audio projection only drives the HUD meter while recording. Take
        // the newest chunk: a cold-start boundary drain slows this loop, and
        // reading one chunk per turn would let stale audio crowd out every
        // current level.
        let audio = input.recv_timeout(ACTIVE_TURN)?;
        if let Some(audio) = input.latest_recognition(audio)
            && audio.is_current(input.recognition_generation())
            && hotkey.is_recording()
            && let Some(indicator) = &indicator
        {
            indicator.meter(&audio.samples);
        }
    }

    events.emit(&VoiceEvent::State {
        timestamp_ms: now_ms(),
        state: VoiceState::Stopping,
        device: input.device_name(),
    })?;
    events.flush()?;
    session.indicate(DictationIndicatorEvent::Discarded);
    Ok(())
}

fn refresh_hotkey_settings(hotkey: &mut DictationHotkey) {
    hotkey.set_mode(crate::app_settings::dictation_mode());
    hotkey.set_binding(crate::app_settings::dictation_hotkey());
    hotkey.set_double_tap_only(crate::app_settings::double_tap_only());
    hotkey.set_double_tap_sensitivity(crate::app_settings::double_tap_sensitivity());
}
