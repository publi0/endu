//! The hotkey dictation listener: the shortcut machine, the authoritative
//! microphone timeline, and the transcription pipeline, wired together on one
//! control loop. Capture never waits on transcription or paste.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::Duration;

use color_eyre::Result;

use crate::audio::CaptureInstant;
use crate::context::{ContextMonitor, ContextSnapshot};
use crate::dictation::{Finish, Release};
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

/// A stalled microphone must not hold a released clip forever. Past this
/// margin the clip ends at the tail with whatever audio the timeline reached
/// by then: everything captured up to the stall is kept, and only audio
/// arriving later than the tail plus this margin is left out.
const RELEASE_TAIL_STALL: Duration = Duration::from_millis(200);

/// A released capture that keeps recording until its tail is covered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReleaseTail {
    until: CaptureInstant,
    submit: Option<u64>,
}

impl ReleaseTail {
    /// The tail is complete once the microphone timeline covers it, or once
    /// the wall clock shows the device has stalled past it.
    fn is_covered(self, captured_through: CaptureInstant, now: CaptureInstant) -> bool {
        captured_through >= self.until
            || now
                >= self
                    .until
                    .checked_add(RELEASE_TAIL_STALL)
                    .unwrap_or(self.until)
    }

    /// A later shortcut edge ends the tail early, at that edge.
    fn end_before(self, edge: Option<CaptureInstant>) -> CaptureInstant {
        edge.map_or(self.until, |edge| edge.min(self.until))
    }
}

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
    tail: Cell<Option<ReleaseTail>>,
    /// `Keep recording after release`, refreshed every turn and snapshotted
    /// at each finishing edge.
    release_tail: Cell<Duration>,
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
            // A released clip still recording its tail is still dictating.
            state: if capturing || self.has_tail() {
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
        // Any later edge ends a released clip there, before it can start,
        // cancel or repaste anything. Accepted work is never cancelled.
        self.seal_tail(Some(at), action == HotkeyAction::Start)?;
        match action {
            HotkeyAction::Start => {
                // Ask for the microphone first: a cold open runs on its own
                // thread while the destination is captured here.
                let start = self.input.start(at)?;
                let Some(indicator_event) = capture_start_event(start) else {
                    return Ok(false);
                };
                let target = ContextSnapshot::capture().unwrap_or_default();
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
                self.release(at, None)?;
            }
            HotkeyAction::FinishAndSubmit => {
                if self.input.become_intentional(at)? {
                    feedback::play(Tone::DictationStart);
                }
                self.release(at, submit)?;
            }
            HotkeyAction::Discard => self.end(CaptureEnd::Discarded)?,
            HotkeyAction::Cancel => self.end(CaptureEnd::Cancelled)?,
            HotkeyAction::PasteLast => self.paste_last()?,
        }
        Ok(true)
    }

    fn paste_last(&self) -> Result<()> {
        // Menu repastes have no event timestamp; the tail ends now instead.
        self.seal_tail(Some(CaptureInstant::now()), false)?;
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

    fn has_tail(&self) -> bool {
        self.tail.get().is_some()
    }

    /// The user's finishing edge. The HUD keeps showing Recording and the
    /// stop cue waits for the tail, so the cue is never recorded into it.
    /// With the tail Off the clip finishes at the edge, as it always did.
    fn release(&self, at: CaptureInstant, submit: Option<u64>) -> Result<()> {
        let tail = self.release_tail.get();
        if tail.is_zero() {
            return self.finish(at, submit);
        }
        match self.input.release(at, tail)? {
            Release::Discard => self.finish(at, submit),
            Release::Tail { until } => {
                self.tail.set(Some(ReleaseTail { until, submit }));
                Ok(())
            }
        }
    }

    /// Finishes a released clip at the end of its tail or at `edge`. Before
    /// a new press the microphone stays open for the capture that follows.
    fn seal_tail(&self, edge: Option<CaptureInstant>, then_start: bool) -> Result<()> {
        let Some(tail) = self.tail.take() else {
            return Ok(());
        };
        let at = tail.end_before(edge);
        let finish = if then_start {
            self.input.finish_before_start(at)?
        } else {
            self.input.finish(at)?
        };
        self.complete(finish, tail.submit)
    }

    fn seal_covered_tail(&self, now: CaptureInstant) -> Result<()> {
        if self
            .tail
            .get()
            .is_some_and(|tail| tail.is_covered(self.input.captured_through(), now))
        {
            self.seal_tail(None, false)?;
        }
        Ok(())
    }

    /// Whether the event tap should consume Escape for this listener.
    fn escape_cancels(&self, hotkey_recording: bool) -> bool {
        hotkey_recording || self.has_tail() || self.worker.pending_count() > 0
    }

    /// Nothing is recording, sealing, pending or recovering.
    fn is_idle(&self, hotkey_quiescent: bool, input_pending: bool) -> bool {
        hotkey_quiescent
            && !input_pending
            && !self.has_tail()
            && !self.input.is_recording()
            && !self.input.is_recovering()
            && !self.worker.is_busy()
    }

    /// Applies one audio-owner notification. Returns whether the shortcut
    /// machine must be suspended.
    fn audio_event(&self, event: DictationAudioEvent) -> Result<bool> {
        match event {
            DictationAudioEvent::CaptureReady { .. } => {
                if self.input.is_recording() {
                    self.indicate(DictationIndicatorEvent::Started);
                }
                Ok(false)
            }
            DictationAudioEvent::ReadyIntentional { .. } => {
                if self.input.is_recording() {
                    feedback::play(Tone::DictationStart);
                }
                Ok(false)
            }
            DictationAudioEvent::OpenFailed { error, .. } => {
                self.fail(format!("Could not open microphone: {error}"))?;
                Ok(true)
            }
            DictationAudioEvent::RecognitionDiscontinuity { dropped_frames } => {
                tracing::debug!(dropped_frames, "HUD meter audio fell behind");
                Ok(false)
            }
            DictationAudioEvent::CaptureDiscontinuity {
                was_recording,
                gap_ms,
                ..
            } => {
                self.input.discard_recognition_backlog();
                if was_recording {
                    self.fail(format!("Microphone audio was interrupted for {gap_ms} ms."))?;
                    return Ok(true);
                }
                // A gap during a tail set the released clip aside: it ends
                // at the release edge or its last audio, never failing.
                self.seal_tail(None, false)?;
                Ok(false)
            }
            DictationAudioEvent::Reopened => {
                self.input.discard_recognition_backlog();
                if !self.input.is_recording() {
                    self.emit_state(false)?;
                }
                Ok(false)
            }
            DictationAudioEvent::Interrupted { was_recording, .. } => {
                self.input.discard_recognition_backlog();
                if was_recording {
                    self.fail("Microphone capture was interrupted; reconnecting.".into())?;
                } else {
                    // As with a gap, a disconnect during a tail keeps the clip.
                    self.seal_tail(None, false)?;
                }
                Ok(true)
            }
        }
    }

    /// Quitting cannot transcribe a released clip still recording its tail:
    /// the worker drops queued work on shutdown. The clip is accepted work,
    /// not cancelled output, so it ends now and goes to recording recovery
    /// for a manual Retry instead of vanishing. It is never pasted.
    fn shutdown(&self, recovery: &crate::recording_recovery::RecordingRecovery) -> Result<()> {
        let Some(tail) = self.tail.take() else {
            return Ok(());
        };
        let context = self
            .recording_context
            .borrow_mut()
            .take()
            .unwrap_or_default();
        if let Finish::Transcribe(mut clip) = self
            .input
            .finish(tail.end_before(Some(CaptureInstant::now())))?
        {
            drop(clip.live.take());
            recovery.save_untranscribed(
                &clip.into_transcription_samples(),
                context.application.as_deref(),
            );
        }
        Ok(())
    }

    /// Escape during a tail cancels that clip like any capture.
    fn cancel_tail(&self) -> Result<bool> {
        if self.tail.take().is_none() {
            return Ok(false);
        }
        self.end(CaptureEnd::Cancelled)?;
        Ok(true)
    }

    fn finish(&self, at: CaptureInstant, submit: Option<u64>) -> Result<()> {
        let finish = self.input.finish(at)?;
        self.complete(finish, submit)
    }

    fn complete(&self, finish: Finish, submit: Option<u64>) -> Result<()> {
        let context = self
            .recording_context
            .borrow_mut()
            .take()
            .unwrap_or_default();
        let Finish::Transcribe(clip) = finish else {
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
        self.tail.take();
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
        self.tail.take();
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
    let shutdown_recovery = recovery.clone();
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
        tail: Cell::new(None),
        release_tail: Cell::new(crate::app_settings::release_tail()),
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
        session
            .release_tail
            .set(crate::app_settings::release_tail());
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
            if !hotkey.is_recording() && input_event.is_escape_down() {
                // A released clip still recording its tail is the newest work.
                if session.cancel_tail()? {
                    continue;
                }
                if let Some(job_id) = worker.cancel_latest() {
                    feedback::play(Tone::Cancel);
                    events.dictation(DictationPhase::Cancelled, "")?;
                    session.indicate(DictationIndicatorEvent::JobCancelled {
                        job_id: job_id.value(),
                    });
                    continue;
                }
            }
            // Enter-to-submit: the token is the interaction revision at the
            // Enter press and belongs to this one job, through any release
            // tail. Return is never sent on failure, empty text, cancellation,
            // a changed foreground or intervening input, and Paste Last never
            // replays the token. Out-of-band suspend, recovery and cancellation
            // invalidate the event tap's Enter prediction through the reset
            // epoch (`reset_submit_guard`).
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
        if received_worker_event {
            session.emit_state(hotkey.is_recording())?;
        }

        while let Some(audio_event) = input.try_recv_event() {
            if session.audio_event(audio_event)? {
                hotkey.suspend();
                input_monitor.reset_submit_guard(false);
            }
        }

        // After audio events: a gap or disconnect during a tail has already
        // finished the released clip at its last audio instead of failing it.
        session.seal_covered_tail(CaptureInstant::now())?;
        // Only now, after any seal, cancel or failure this turn, does the
        // event tap learn whether Escape still belongs to Endu. Otherwise an
        // idle listener could keep swallowing Escape system-wide.
        input_monitor.set_escape_cancels(session.escape_cancels(hotkey.is_recording()));

        // Nothing is recording, pending or recovering, and no input awaits
        // processing: sleep until the next edge, control or fallback instead
        // of polling. Capture boundaries come from event timestamps, so the
        // wait never moves them.
        let idle = session.is_idle(
            hotkey.is_quiescent(),
            input_monitor.pending_events().oldest().is_some(),
        );
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
            && (hotkey.is_recording() || session.has_tail())
            && let Some(indicator) = &indicator
        {
            indicator.meter(&audio.samples);
        }
    }

    if let Err(error) = session.shutdown(&shutdown_recovery) {
        tracing::warn!(%error, "could not keep the released clip at shutdown");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paste::{PasteOptions, PasteOutcome};
    use std::sync::mpsc::{Receiver, Sender};

    const TAIL: Duration = Duration::from_millis(300);
    type Microphone = Sender<(Vec<f32>, CaptureInstant)>;
    /// 16 kHz samples in `ms` of transcribed audio.
    fn samples_ms(ms: usize) -> usize {
        ms * 16
    }

    /// A real audio owner over a fixture microphone and a real pipeline with
    /// injected transcription and paste; nothing touches a device, the
    /// network, the clipboard or the user's settings.
    struct Harness {
        input: DictationAudio,
        microphone: RefCell<Option<Microphone>>,
        worker: DictationWorker,
        wake: Arc<Doorbell>,
        events: EventLog,
        transcribed: Receiver<usize>,
        pasted: Receiver<bool>,
        activity: crate::suppression::InputActivity,
        directory: std::path::PathBuf,
    }

    impl Harness {
        fn new() -> Self {
            static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "endu-listener-tail-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let (input, microphone) = DictationAudio::for_test();
            let wake = Arc::new(Doorbell::new());
            let (transcribed_sender, transcribed) = mpsc::channel();
            let (pasted_sender, pasted) = mpsc::channel();
            let activity = crate::suppression::InputActivity::default();
            let paste_activity = activity.clone();
            let worker = DictationWorker::for_test(
                wake.clone(),
                move |samples| {
                    transcribed_sender.send(samples.len()).unwrap();
                    Ok(crate::openrouter::transcribe::Transcription {
                        text: "fixture text".into(),
                        report: None,
                    })
                },
                move |_, options: PasteOptions| {
                    // The paster's rule: Return only for an unchanged token.
                    let submits = options
                        .submit_after_paste
                        .is_some_and(|token| token == paste_activity.interaction_revision());
                    pasted_sender.send(submits).unwrap();
                    Ok(PasteOutcome::Pasted)
                },
            );
            let events = EventLog::create(&directory.join("live.ndjson")).unwrap();
            Self {
                input,
                microphone: RefCell::new(Some(microphone)),
                worker,
                wake,
                events,
                transcribed,
                pasted,
                activity,
                directory,
            }
        }

        fn session(&self) -> Session<'_> {
            Session {
                input: &self.input,
                worker: &self.worker,
                events: &self.events,
                indicator: None,
                recording_context: RefCell::new(None),
                tail: Cell::new(None),
                release_tail: Cell::new(TAIL),
            }
        }

        /// Sends contiguous 10 ms chunks of 48 kHz audio covering the span.
        fn feed(&self, from_ms: u64, to_ms: u64, level: f32) {
            for end in (from_ms + 10..=to_ms).step_by(10) {
                self.microphone
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .send((vec![level; 480], at_ms(end)))
                    .unwrap();
            }
        }

        /// Warm audio, a press at 500 ms and an intentional hold to 1.5 s,
        /// released with `submit`: the clip now records its tail.
        fn released(&self, session: &Session<'_>, submit: Option<u64>) {
            self.feed(0, 500, 0.25);
            assert!(
                session
                    .handle_hotkey(HotkeyAction::Start, at_ms(500), None)
                    .unwrap()
            );
            self.feed(500, 1_500, 0.5);
            session
                .handle_hotkey(HotkeyAction::FinishAndSubmit, at_ms(1_500), submit)
                .unwrap();
            assert_eq!(
                session.tail.get(),
                Some(ReleaseTail {
                    until: at_ms(1_800),
                    submit
                })
            );
            assert!(!session.is_idle(true, false));
            assert!(session.escape_cancels(false));
        }

        fn next_clip(&self) -> usize {
            self.transcribed
                .recv_timeout(Duration::from_secs(5))
                .expect("the released clip is transcribed")
        }

        /// Lets the pipeline finish, as the loop would, without tones.
        fn drain_worker(&self) {
            for _ in 0..500 {
                while self.worker.try_recv().is_some() {}
                if !self.worker.is_busy() {
                    return;
                }
                self.wake.wait_timeout(Duration::from_millis(10));
            }
            panic!("the pipeline did not finish");
        }

        fn next_capture_event(&self) -> DictationAudioEvent {
            for _ in 0..250 {
                if let Some(event) =
                    std::iter::from_fn(|| self.input.try_recv_event()).find(|event| {
                        !matches!(event, DictationAudioEvent::RecognitionDiscontinuity { .. })
                    })
                {
                    return event;
                }
                let _ = self.input.recv_timeout(Duration::from_millis(20));
            }
            panic!("the audio owner reported nothing");
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    /// 450 ms pre-roll, the 1 s hold and `tail_ms` of tail audio.
    fn clip_with_tail(tail_ms: usize) -> usize {
        samples_ms(450 + 1_000 + tail_ms)
    }

    #[test]
    fn a_covered_tail_seals_and_the_listener_returns_to_idle() {
        let harness = Harness::new();
        let session = harness.session();
        harness.released(&session, None);
        harness.feed(1_500, 1_790, 0.75);
        session.seal_covered_tail(at_ms(1_790)).unwrap();
        harness.feed(1_790, 1_900, 0.75);
        // Timestamps end the tail; audio past it never enters the clip.
        while session.has_tail() {
            let _ = harness.input.recv_timeout(Duration::from_millis(20));
            session.seal_covered_tail(at_ms(1_800)).unwrap();
        }
        assert_eq!(harness.next_clip(), clip_with_tail(300));
        harness.drain_worker();
        assert!(!harness.pasted.recv().unwrap());
        // The escape flag is recomputed after the seal and the loop sleeps.
        assert!(!session.escape_cancels(false));
        assert!(session.is_idle(true, false));
    }

    #[test]
    fn a_new_press_or_paste_last_seals_the_tail_at_that_edge() {
        for release_while_idle in [false, true] {
            let harness = Harness::new();
            let session = harness.session();
            harness.released(&session, None);
            harness.input.set_release_while_idle(release_while_idle);
            harness.feed(1_500, 1_600, 0.75);
            assert!(
                session
                    .handle_hotkey(HotkeyAction::Start, at_ms(1_600), None)
                    .unwrap()
            );
            assert!(!session.has_tail());
            assert_eq!(harness.next_clip(), clip_with_tail(100));
            // The second capture starts warm, without a cold open. Its
            // pre-roll stops where the first clip ended: no audio repeats.
            assert!(harness.input.is_recording());
            harness.feed(1_600, 2_000, 0.75);
            session
                .handle_hotkey(HotkeyAction::Finish, at_ms(2_000), None)
                .unwrap();
            harness.feed(2_000, 2_300, 0.75);
            session.seal_tail(None, false).unwrap();
            assert_eq!(harness.next_clip(), samples_ms(400 + 300));
        }

        let harness = Harness::new();
        let session = harness.session();
        harness.released(&session, None);
        harness.feed(1_500, 1_800, 0.75);
        session.paste_last().unwrap();
        assert!(!session.has_tail());
        assert_eq!(harness.next_clip(), clip_with_tail(300));
    }

    #[test]
    fn escape_cancels_the_tail_and_releases_escape() {
        let harness = Harness::new();
        let session = harness.session();
        harness.released(&session, None);
        assert!(session.cancel_tail().unwrap());
        assert!(!session.cancel_tail().unwrap());
        assert!(!session.escape_cancels(false));
        assert!(session.is_idle(true, false));
        assert!(harness.transcribed.try_recv().is_err());
    }

    #[test]
    fn a_stalled_microphone_seals_with_the_audio_it_captured() {
        let harness = Harness::new();
        let session = harness.session();
        harness.released(&session, None);
        harness.feed(1_500, 1_600, 0.75);
        session.seal_covered_tail(at_ms(1_999)).unwrap();
        assert!(session.has_tail());
        session.seal_covered_tail(at_ms(2_000)).unwrap();
        assert!(!session.has_tail());
        assert_eq!(harness.next_clip(), clip_with_tail(100));
        harness.drain_worker();
        assert!(!session.escape_cancels(false));
    }

    #[test]
    fn interruptions_during_the_tail_keep_the_clip() {
        // A capture gap read by the listener.
        let harness = Harness::new();
        let session = harness.session();
        harness.released(&session, None);
        harness.feed(1_500, 1_600, 0.75);
        harness.feed(1_700, 1_750, 0.9);
        let event = harness.next_capture_event();
        assert!(!session.is_idle(true, false));
        assert!(!session.audio_event(event).unwrap());
        assert!(!session.has_tail());
        assert_eq!(harness.next_clip(), clip_with_tail(100));

        // A device disconnect.
        let harness = Harness::new();
        let session = harness.session();
        harness.released(&session, None);
        harness.input.set_release_while_idle(true);
        harness.feed(1_500, 1_600, 0.75);
        // The stream stops: its channel disconnects.
        drop(harness.microphone.borrow_mut().take());
        let event = harness.next_capture_event();
        assert!(matches!(
            event,
            DictationAudioEvent::Interrupted {
                was_recording: false,
                ..
            }
        ));
        assert!(session.audio_event(event).unwrap());
        assert_eq!(harness.next_clip(), clip_with_tail(100));

        // A new press before the listener reads the gap.
        let harness = Harness::new();
        let session = harness.session();
        harness.released(&session, None);
        harness.feed(1_500, 1_600, 0.75);
        harness.feed(1_700, 1_750, 0.9);
        assert!(
            session
                .handle_hotkey(HotkeyAction::Start, at_ms(1_800), None)
                .unwrap()
        );
        assert_eq!(harness.next_clip(), clip_with_tail(100));
        assert!(harness.input.is_recording());
        // The stale gap notice never fails the new capture.
        while let Some(event) = harness.input.try_recv_event() {
            assert!(!matches!(
                event,
                DictationAudioEvent::CaptureDiscontinuity { .. }
                    | DictationAudioEvent::Interrupted { .. }
            ));
        }
    }

    #[test]
    fn enter_to_submit_survives_the_tail_unless_the_user_types() {
        for typed in [false, true] {
            let harness = Harness::new();
            let session = harness.session();
            let token = harness.activity.interaction_revision();
            harness.released(&session, Some(token));
            if typed {
                harness.activity.invalidate();
            }
            harness.feed(1_500, 1_800, 0.75);
            while session.has_tail() {
                let _ = harness.input.recv_timeout(Duration::from_millis(20));
                session.seal_covered_tail(at_ms(1_800)).unwrap();
            }
            harness.next_clip();
            harness.drain_worker();
            assert_eq!(
                harness.pasted.recv_timeout(Duration::from_secs(5)).unwrap(),
                !typed
            );
        }
    }

    #[test]
    fn off_finishes_at_the_release_edge() {
        let harness = Harness::new();
        let session = harness.session();
        session.release_tail.set(Duration::ZERO);
        harness.feed(0, 500, 0.25);
        session
            .handle_hotkey(HotkeyAction::Start, at_ms(500), None)
            .unwrap();
        harness.feed(500, 1_800, 0.5);
        session
            .handle_hotkey(HotkeyAction::Finish, at_ms(1_500), None)
            .unwrap();
        assert!(!session.has_tail());
        assert_eq!(harness.next_clip(), clip_with_tail(0));
    }

    #[test]
    fn quitting_during_the_tail_keeps_the_clip_for_recovery() {
        let harness = Harness::new();
        let session = harness.session();
        harness.released(&session, None);
        harness.feed(1_500, 1_650, 0.75);
        let recovery =
            crate::recording_recovery::RecordingRecovery::open(harness.directory.join("recovery"))
                .unwrap();
        session.shutdown(&recovery).unwrap();
        assert!(!session.has_tail());
        assert!(harness.transcribed.try_recv().is_err());
        let entries = recovery.entries("");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].status,
            crate::recording_recovery::RecoveryStatus::Failed
        );
        assert_eq!(entries[0].audio_ms, 1_600);
    }

    fn at_ms(ms: u64) -> CaptureInstant {
        CaptureInstant::from_nanos(60_000_000_000 + ms * 1_000_000)
    }

    #[test]
    fn release_tail_waits_for_the_timeline_or_a_stalled_device() {
        let tail = ReleaseTail {
            until: at_ms(1_300),
            submit: Some(7),
        };
        assert!(!tail.is_covered(at_ms(1_299), at_ms(1_310)));
        assert!(tail.is_covered(at_ms(1_300), at_ms(1_300)));
        // Timestamps, not the listener's wake time, end the tail.
        assert!(tail.is_covered(at_ms(1_320), at_ms(1_301)));
        assert!(!tail.is_covered(at_ms(1_000), at_ms(1_499)));
        assert!(tail.is_covered(at_ms(1_000), at_ms(1_500)));
    }

    #[test]
    fn a_later_edge_ends_the_tail_at_that_edge() {
        let tail = ReleaseTail {
            until: at_ms(1_300),
            submit: None,
        };
        assert_eq!(tail.end_before(None), at_ms(1_300));
        assert_eq!(tail.end_before(Some(at_ms(1_120))), at_ms(1_120));
        assert_eq!(tail.end_before(Some(at_ms(1_900))), at_ms(1_300));
    }
}
