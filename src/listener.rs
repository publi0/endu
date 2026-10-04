//! The hotkey dictation listener: the shortcut machine, the authoritative
//! microphone timeline, and the transcription pipeline, wired together on one
//! control loop. Capture never waits on transcription or paste.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use color_eyre::Result;

use crate::audio::CaptureInstant;
use crate::context::{ContextMonitor, ContextSnapshot};
use crate::dictation::Finish;
use crate::dictation_audio::{DictationAudio, DictationAudioEvent};
use crate::dictation_indicator::{DictationIndicatorEvent, DictationIndicatorSender};
use crate::events::{DictationPhase, EventLog, VoiceEvent, VoiceState, now_ms};
use crate::feedback::{self, Tone};
use crate::pipeline::{DictationWorker, WorkerEvent};
use crate::recording_environment::RecordingEnvironmentController;
use crate::suppression::{DictationHotkey, HotkeyAction, InputMonitor};

#[derive(Debug)]
pub enum ListenerControl {
    PasteLast,
}

/// Everything one control-loop iteration needs to report a capture edge.
struct Session<'a> {
    input: &'a DictationAudio,
    worker: &'a DictationWorker,
    events: &'a EventLog,
    indicator: Option<&'a DictationIndicatorSender>,
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
        context: &ContextSnapshot,
    ) -> Result<bool> {
        match action {
            HotkeyAction::Start => {
                if !self.input.start(at)? {
                    return Ok(false);
                }
                self.input.invalidate_recognition()?;
                self.worker.prepare_paste();
                self.events.dictation(DictationPhase::Started, "")?;
                self.indicate(DictationIndicatorEvent::Started);
                self.emit_state(true)?;
            }
            HotkeyAction::Finish => {
                if self.input.become_intentional(at)? {
                    feedback::play(Tone::DictationStart);
                }
                self.finish(at, context)?;
            }
            HotkeyAction::Discard => self.end(CaptureEnd::Discarded)?,
            HotkeyAction::Cancel => self.end(CaptureEnd::Cancelled)?,
            HotkeyAction::PasteLast => self.paste_last()?,
        }
        Ok(true)
    }

    fn paste_last(&self) -> Result<()> {
        self.input.cancel()?;
        if let Err(error) = self.worker.paste_last() {
            feedback::play(Tone::Error);
            self.events
                .dictation(DictationPhase::Failed(error.into()), "")?;
        }
        self.emit_state(false)
    }

    fn finish(&self, at: CaptureInstant, context: &ContextSnapshot) -> Result<()> {
        let Finish::Transcribe(clip) = self.input.finish(at)? else {
            self.events.dictation(DictationPhase::Discarded, "")?;
            self.indicate(DictationIndicatorEvent::Discarded);
            return self.emit_state(false);
        };
        feedback::play(Tone::DictationStop);
        self.events.dictation(DictationPhase::Transcribing, "")?;
        match self.worker.transcribe(clip, context.clone()) {
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
        feedback::play(Tone::Error);
        self.events.dictation(DictationPhase::Failed(message), "")?;
        self.indicate(DictationIndicatorEvent::Failed);
        self.emit_state(false)
    }

    fn worker_event(&self, event: WorkerEvent) -> Result<()> {
        match event {
            WorkerEvent::Completed {
                job_id,
                result: Ok(text),
            } => {
                let phase = if text.trim().is_empty() {
                    DictationPhase::Discarded
                } else {
                    DictationPhase::Pasted
                };
                self.events.dictation(phase, text)?;
                self.indicate(DictationIndicatorEvent::JobCompleted {
                    job_id: job_id.value(),
                });
            }
            WorkerEvent::Completed {
                job_id,
                result: Err(error),
            } => {
                tracing::error!(%error, "dictation failed");
                feedback::play(Tone::Error);
                self.events.dictation(DictationPhase::Failed(error), "")?;
                self.indicate(DictationIndicatorEvent::JobFailed {
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
    controls: Option<Receiver<ListenerControl>>,
) -> Result<()> {
    shutdown.store(false, Ordering::Relaxed);
    // Feedback admission is advisory: a cold audio stack can exceed the
    // preload timeout, and missing tones must not stop dictation.
    if let Err(error) = feedback::preload() {
        tracing::warn!(%error, "recording sounds are unavailable; continuing without feedback");
    }
    let mut release_while_idle = crate::app_settings::release_microphone_while_idle();
    let input_monitor = InputMonitor::start()?;
    let context_monitor = ContextMonitor::start();
    let mut context = ContextSnapshot::default();
    let mut hotkey = DictationHotkey::new(
        CaptureInstant::now(),
        crate::app_settings::double_tap_lock(),
        crate::app_settings::dictation_hotkey(),
    );
    hotkey.set_double_tap_only(crate::app_settings::double_tap_only());
    let recording_environment = RecordingEnvironmentController::start();
    let (mut microphone_revision, microphone) = crate::app_settings::microphone_selection();
    let input = DictationAudio::open(
        device_override,
        microphone_revision,
        microphone.as_deref(),
        recording_environment,
        input_monitor.pending_events(),
        release_while_idle,
    )?;
    let worker = DictationWorker::start(input_monitor.activity.clone(), history);
    let session = Session {
        input: &input,
        worker: &worker,
        events: &events,
        indicator: indicator.as_ref(),
    };

    events.emit(&VoiceEvent::SessionStarted {
        timestamp_ms: now_ms(),
    })?;
    if input.is_recovering() {
        hotkey.suspend();
    }
    if hotkey.is_recording() {
        if input.start(capture_boundary(&input, release_while_idle))? {
            events.dictation(DictationPhase::Started, "")?;
            session.indicate(DictationIndicatorEvent::Started);
        } else {
            hotkey.suspend();
        }
    }
    session.emit_state(hotkey.is_recording())?;
    tracing::info!(device = %input.device_name(), "dictation listener started");

    while !shutdown.load(Ordering::Relaxed) {
        if let Some(controls) = &controls {
            while let Ok(control) = controls.try_recv() {
                match control {
                    ListenerControl::PasteLast => session.paste_last()?,
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
        hotkey.set_double_tap_enabled(crate::app_settings::double_tap_lock());
        hotkey.set_double_tap_only(crate::app_settings::double_tap_only());
        hotkey.set_binding(crate::app_settings::dictation_hotkey());
        let (next_microphone_revision, microphone) = crate::app_settings::microphone_selection();
        if next_microphone_revision != microphone_revision {
            input.request_selection(next_microphone_revision, microphone.as_deref());
            microphone_revision = next_microphone_revision;
        }

        let suspended = crate::app_settings::hotkey_capture_active() || input.is_recovering();
        if suspended && hotkey.suspend().is_some() {
            session.end(CaptureEnd::Cancelled)?;
        }
        while let Ok(observed) = input_monitor.events.try_recv() {
            let _acknowledge = input_monitor.acknowledge_after(observed);
            if suspended {
                continue;
            }
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
            if let Some(action) = hotkey.process(input_event, observed.capture_at)
                && !session.handle_hotkey(action, observed.capture_at, &context)?
            {
                hotkey.suspend();
            }
        }
        if !suspended && input_monitor.pending_events().oldest().is_none() {
            hotkey.recover_stale_keys();
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
                DictationAudioEvent::ReadyIntentional { .. } => {
                    if input.is_recording() {
                        feedback::play(Tone::DictationStart);
                    }
                }
                DictationAudioEvent::OpenFailed { error, .. } => {
                    hotkey.suspend();
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
                    input.discard_recognition_backlog();
                    if was_recording {
                        session.fail("Microphone capture was interrupted; reconnecting.".into())?;
                    }
                }
            }
        }

        // The audio projection only drives the HUD meter while recording.
        if let Some(audio) = input.recv_timeout(Duration::from_millis(20))?
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
