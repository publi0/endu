//! The dictation pipeline: one transcription worker that sends each finished
//! clip to OpenRouter, and one output worker that pastes results in
//! submission order, records History, and answers "paste last".
//!
//! Capture never waits on this pipeline. Its queues are bounded, accepted
//! jobs keep their order, and a cancelled job never pastes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::Result;

use crate::context::ContextSnapshot;
use crate::dictation::DictationClip;
use crate::history::{History, HistoryDraft};
use crate::openrouter::StepReport;
use crate::paste::{PasteOptions, PasteOutcome, Paster};
use crate::suppression::InputActivity;

const MAX_PENDING_OUTPUTS: usize = 16;
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub enum WorkerEvent {
    ReadyToPaste {
        job_id: Option<DictationJobId>,
        copied_to_clipboard: bool,
    },
    Completed {
        job_id: DictationJobId,
        result: Result<String, String>,
    },
    /// The job left the queue and is being transcribed.
    Transcribing {
        job_id: DictationJobId,
    },
    Cancelled {
        job_id: DictationJobId,
    },
    Pasted {
        result: Result<String, String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DictationJobId(u64);

impl DictationJobId {
    pub const fn value(self) -> u64 {
        self.0
    }
}

struct TranscriptionJob {
    job_id: DictationJobId,
    control: Arc<JobControl>,
    submitted_at: Instant,
    clip: DictationClip,
    context: ContextSnapshot,
    options: PasteOptions,
}

/// Pipeline timings carried from transcription through output.
#[derive(Clone, Copy)]
struct JobTimings {
    total_started: Instant,
    queue_ms: u128,
    audio_ms: u64,
    inference_ms: u128,
}

struct CompletedTranscript {
    options: PasteOptions,
    text: String,
    context: ContextSnapshot,
    timings: JobTimings,
    report: Option<StepReport>,
}

struct LastTranscript {
    completed: CompletedTranscript,
    has_pasted: bool,
}

enum OutputJob {
    PreparePaste,
    Completed {
        job_id: DictationJobId,
        control: Arc<JobControl>,
        result: Box<Result<CompletedTranscript, String>>,
    },
    Cancelled {
        job_id: DictationJobId,
    },
    PasteLast {
        sequence: u64,
        target: ContextSnapshot,
    },
}

impl OutputJob {
    fn sequence(&self) -> u64 {
        match self {
            Self::PreparePaste => u64::MAX,
            Self::Completed { job_id, .. } | Self::Cancelled { job_id } => job_id.0,
            Self::PasteLast { sequence, .. } => *sequence,
        }
    }
}

#[derive(Default)]
struct JobControl {
    cancelled: AtomicBool,
    output_started: Mutex<bool>,
}

impl JobControl {
    /// Cancel unless output already began. Returns whether this call cancelled.
    fn cancel(&self) -> bool {
        let output_started = self
            .output_started
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *output_started {
            return false;
        }
        !self.cancelled.swap(true, Ordering::AcqRel)
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Commit to output; refused once cancelled.
    fn begin_output(&self) -> bool {
        let mut output_started = self
            .output_started
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_cancelled() {
            return false;
        }
        *output_started = true;
        true
    }
}

/// Releases outputs strictly in sequence order.
#[derive(Default)]
struct OrderedOutputs {
    waiting: BTreeMap<u64, OutputJob>,
    next_sequence: u64,
}

impl OrderedOutputs {
    fn push(&mut self, job: OutputJob) -> Vec<OutputJob> {
        if job.sequence() < self.next_sequence {
            return Vec::new();
        }
        self.waiting.insert(job.sequence(), job);
        let mut ready = Vec::new();
        while let Some(job) = self.waiting.remove(&self.next_sequence) {
            ready.push(job);
            self.next_sequence += 1;
        }
        ready
    }
}

pub struct DictationWorker {
    transcription_jobs: Option<SyncSender<TranscriptionJob>>,
    output_jobs: Option<SyncSender<OutputJob>>,
    events: Receiver<WorkerEvent>,
    state: Arc<Mutex<WorkerState>>,
    transcription_worker: Option<thread::JoinHandle<()>>,
    output_worker: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct WorkerState {
    next_sequence: u64,
    jobs: BTreeMap<DictationJobId, Arc<JobControl>>,
    pending_pastes: usize,
    shutting_down: bool,
}

impl WorkerState {
    fn next_output_sequence(&self) -> Result<u64, &'static str> {
        if self.shutting_down {
            return Err("dictation worker is unavailable");
        }
        // Count accepted work even after it leaves a channel for ordered buffering.
        if self.jobs.len() + self.pending_pastes >= MAX_PENDING_OUTPUTS {
            return Err("dictation queue is full");
        }
        Ok(self.next_sequence)
    }

    fn cancel_latest(&self) -> Option<DictationJobId> {
        self.jobs
            .iter()
            .rev()
            .find_map(|(&job_id, control)| control.cancel().then_some(job_id))
    }
}

impl DictationWorker {
    pub fn start(
        activity: InputActivity,
        history: Option<History>,
        recovery: crate::recording_recovery::RecordingRecovery,
    ) -> Self {
        Self::start_with(
            history,
            move |samples, context| {
                recovery.transcribe_original(
                    samples,
                    context.application.as_deref(),
                    crate::openrouter::transcribe::transcribe,
                )
            },
            move || {
                let mut paster = Paster::new(activity);
                Box::new(move |prepare_only, text, target, options, commit| {
                    if prepare_only {
                        paster.prepare();
                        Ok(PasteOutcome::Pasted)
                    } else {
                        paster.paste(text, target, options, commit)
                    }
                })
            },
        )
    }

    fn start_with(
        history: Option<History>,
        transcribe: impl FnMut(
            &[f32],
            &ContextSnapshot,
        ) -> Result<crate::openrouter::transcribe::Transcription>
        + Send
        + 'static,
        create_paste: impl FnOnce() -> Box<PasteFn<'static>> + Send + 'static,
    ) -> Self {
        let (transcription_jobs, transcription_receiver) =
            mpsc::sync_channel::<TranscriptionJob>(4);
        let (output_jobs, output_receiver) = mpsc::sync_channel::<OutputJob>(8);
        let (event_sender, events) = mpsc::channel();
        let state = Arc::new(Mutex::new(WorkerState::default()));

        let output_worker = thread::spawn({
            let state = state.clone();
            let events = event_sender.clone();
            move || {
                let mut paste = create_paste();
                run_output_worker(output_receiver, &mut *paste, history, &state, &events)
            }
        });

        let transcription_worker = thread::spawn({
            let output = output_jobs.clone();
            let state = state.clone();
            move || {
                run_transcription_worker(
                    transcription_receiver,
                    &output,
                    &event_sender,
                    &state,
                    transcribe,
                )
            }
        });

        Self {
            transcription_jobs: Some(transcription_jobs),
            output_jobs: Some(output_jobs),
            events,
            state,
            transcription_worker: Some(transcription_worker),
            output_worker: Some(output_worker),
        }
    }

    pub fn transcribe(
        &self,
        clip: DictationClip,
        context: ContextSnapshot,
        submit_after_paste: Option<u64>,
    ) -> Result<DictationJobId, &'static str> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let job_id = DictationJobId(state.next_output_sequence()?);
        let control = Arc::new(JobControl::default());
        self.transcription_jobs
            .as_ref()
            .ok_or("dictation worker is unavailable")?
            .try_send(TranscriptionJob {
                job_id,
                control: control.clone(),
                submitted_at: Instant::now(),
                options: PasteOptions {
                    submit_after_paste,
                    post_processing: crate::post_processing::Preferences::current(),
                },
                clip,
                context,
            })
            .map(|()| {
                state.next_sequence += 1;
                state.jobs.insert(job_id, control);
                job_id
            })
            .map_err(queue_error)
    }

    /// Capture the clipboard ahead of the first paste while nothing is queued.
    pub fn prepare_paste(&self) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.shutting_down || !state.jobs.is_empty() || state.pending_pastes > 0 {
            return;
        }
        drop(state);
        if let Some(output_jobs) = &self.output_jobs {
            let _ = output_jobs.try_send(OutputJob::PreparePaste);
        }
    }

    pub fn paste_last(&self, target: ContextSnapshot) -> Result<(), &'static str> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let sequence = state.next_output_sequence()?;
        self.output_jobs
            .as_ref()
            .ok_or("dictation worker is unavailable")?
            .try_send(OutputJob::PasteLast { sequence, target })
            .map(|()| {
                state.next_sequence += 1;
                state.pending_pastes += 1;
            })
            .map_err(queue_error)
    }

    pub fn try_recv(&self) -> Option<WorkerEvent> {
        self.events.try_recv().ok()
    }

    pub fn is_busy(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        !state.jobs.is_empty() || state.pending_pastes > 0
    }

    pub fn pending_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .jobs
            .len()
    }

    pub fn cancel_latest(&self) -> Option<DictationJobId> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let job_id = state.cancel_latest()?;
        drop(state);
        let _ = self
            .output_jobs
            .as_ref()?
            .try_send(OutputJob::Cancelled { job_id });
        Some(job_id)
    }

    fn shutdown(&mut self) {
        {
            // Output commits take this same lock. A pending clipboard write can
            // no longer begin once shutdown wins; an already committed paste finishes.
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.shutting_down = true;
            for control in state.jobs.values() {
                control.cancel();
            }
            state.jobs.clear();
            state.pending_pastes = 0;
        }
        self.transcription_jobs.take();
        self.output_jobs.take();
        join_worker(self.output_worker.take(), "dictation output");
        if let Some(worker) = self.transcription_worker.take()
            && worker.is_finished()
        {
            join_worker(Some(worker), "dictation transcription");
        }
        // Dropping an unfinished handle detaches the remote request. Its worker
        // checks shutdown before publishing the result or starting queued clips.
    }
}

impl Drop for DictationWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_transcription_worker(
    jobs: Receiver<TranscriptionJob>,
    output: &SyncSender<OutputJob>,
    events: &mpsc::Sender<WorkerEvent>,
    state: &Mutex<WorkerState>,
    mut transcribe: impl FnMut(
        &[f32],
        &ContextSnapshot,
    ) -> Result<crate::openrouter::transcribe::Transcription>,
) {
    prioritize_transcription_thread();
    while let Ok(job) = jobs.recv() {
        if is_shutting_down(state) {
            break;
        }
        if job.control.is_cancelled() {
            let _ = output.send(OutputJob::Cancelled { job_id: job.job_id });
            continue;
        }
        let _ = events.send(WorkerEvent::Transcribing { job_id: job.job_id });
        let queue_ms = job.submitted_at.elapsed().as_millis();
        let audio_ms = job.clip.duration_ms();
        let input_description = job.clip.input.clone();
        let samples = job.clip.into_transcription_samples();
        crate::microphone::record(&samples, input_description);
        let started = Instant::now();
        let result = transcribe(&samples, &job.context);
        if is_shutting_down(state) {
            break;
        }
        let timings = JobTimings {
            total_started: job.submitted_at,
            queue_ms,
            audio_ms,
            inference_ms: started.elapsed().as_millis(),
        };
        let result = result
            .map(|transcription| CompletedTranscript {
                options: job.options,
                text: job
                    .options
                    .post_processing
                    .process(transcription.text.trim())
                    .trim()
                    .to_owned(),
                context: job.context,
                timings,
                report: transcription.report,
            })
            .map_err(|error| format!("{error:#}"));
        if output
            .send(OutputJob::Completed {
                job_id: job.job_id,
                control: job.control,
                result: Box::new(result),
            })
            .is_err()
        {
            break;
        }
    }
}

/// `paste(prepare_only, text, commit)` either captures the clipboard
/// ahead of time or pastes `text` once `commit` accepts.
type PasteFn<'a> = dyn FnMut(bool, &str, &ContextSnapshot, PasteOptions, &dyn Fn() -> bool) -> Result<PasteOutcome>
    + 'a;

fn run_output_worker(
    jobs: Receiver<OutputJob>,
    paste: &mut PasteFn<'_>,
    history: Option<History>,
    state: &Mutex<WorkerState>,
    events: &mpsc::Sender<WorkerEvent>,
) {
    let mut last_transcript = None;
    let mut ordered = OrderedOutputs::default();
    while !is_shutting_down(state) {
        let job = match jobs.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if is_shutting_down(state) {
            break;
        }
        if matches!(job, OutputJob::PreparePaste) {
            let _ = paste(
                true,
                "",
                &ContextSnapshot::default(),
                PasteOptions::default(),
                &|| true,
            );
            continue;
        }
        for job in ordered.push(job) {
            if is_shutting_down(state) {
                return;
            }
            let event = finish_output(
                job,
                &mut |text, target, options, commit| paste(false, text, target, options, commit),
                &mut last_transcript,
                history.as_ref(),
                state,
            );
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            match &event {
                WorkerEvent::Completed { job_id, .. }
                | WorkerEvent::Cancelled { job_id }
                | WorkerEvent::ReadyToPaste {
                    job_id: Some(job_id),
                    ..
                } => {
                    state.jobs.remove(job_id);
                }
                WorkerEvent::Pasted { .. } | WorkerEvent::ReadyToPaste { job_id: None, .. } => {
                    state.pending_pastes = state.pending_pastes.saturating_sub(1);
                }
                WorkerEvent::Transcribing { .. } => {}
            }
            drop(state);
            if events.send(event).is_err() {
                return;
            }
        }
    }
}

fn is_shutting_down(state: &Mutex<WorkerState>) -> bool {
    state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .shutting_down
}

/// Serialize every clipboard commit, including Paste Last, against shutdown.
fn commit_output(state: &Mutex<WorkerState>, commit: &dyn Fn() -> bool) -> bool {
    let state = state.lock().unwrap_or_else(|error| error.into_inner());
    !state.shutting_down && commit()
}

/// `paste(text, commit)` pastes `text` once `commit` accepts.
type OutputPasteFn<'a> =
    dyn FnMut(&str, &ContextSnapshot, PasteOptions, &dyn Fn() -> bool) -> Result<PasteOutcome> + 'a;

fn finish_output(
    job: OutputJob,
    paste: &mut OutputPasteFn<'_>,
    last_transcript: &mut Option<LastTranscript>,
    history: Option<&History>,
    state: &Mutex<WorkerState>,
) -> WorkerEvent {
    match job {
        OutputJob::PreparePaste => unreachable!("paste preparation bypasses ordered output"),
        OutputJob::Completed {
            job_id,
            control,
            result,
        } if !control.is_cancelled() => {
            let completed = match *result {
                Ok(completed) => completed,
                Err(error) => {
                    if !commit_output(state, &|| control.begin_output()) {
                        return WorkerEvent::Cancelled { job_id };
                    }
                    return WorkerEvent::Completed {
                        job_id,
                        result: Err(error),
                    };
                }
            };
            if completed.text.is_empty() {
                if !commit_output(state, &|| control.begin_output()) {
                    return WorkerEvent::Cancelled { job_id };
                }
                return WorkerEvent::Completed {
                    job_id,
                    result: Ok(String::new()),
                };
            }
            let paste_started = Instant::now();
            let commit = || commit_output(state, &|| control.begin_output());
            let outcome = paste(
                &completed.text,
                &completed.context,
                completed.options,
                &commit,
            );
            match outcome {
                Ok(PasteOutcome::Pasted) => {
                    if let Some(history) = history {
                        record_history(history, &completed);
                    }
                    tracing::info!(
                        audio_ms = completed.timings.audio_ms,
                        queue_ms = completed.timings.queue_ms,
                        inference_ms = completed.timings.inference_ms,
                        paste_ms = paste_started.elapsed().as_millis(),
                        total_ms = completed.timings.total_started.elapsed().as_millis(),
                        "dictation pipeline completed"
                    );
                    let text = completed.text.clone();
                    *last_transcript = Some(LastTranscript {
                        completed,
                        has_pasted: true,
                    });
                    WorkerEvent::Completed {
                        job_id,
                        result: Ok(text),
                    }
                }
                Ok(PasteOutcome::CopiedToClipboard) => {
                    // The clipboard write already passed the output commit gate.
                    // A later shutdown must not relabel that completed copy as cancelled.
                    *last_transcript = Some(LastTranscript {
                        completed,
                        has_pasted: false,
                    });
                    WorkerEvent::ReadyToPaste {
                        job_id: Some(job_id),
                        copied_to_clipboard: true,
                    }
                }
                outcome => {
                    // Reserving the final outcome also serializes retaining text
                    // against Escape/shutdown when clipboard preparation failed.
                    if !commit() {
                        return WorkerEvent::Cancelled { job_id };
                    }
                    *last_transcript = Some(LastTranscript {
                        completed,
                        has_pasted: false,
                    });
                    match outcome {
                        Ok(PasteOutcome::Deferred) => WorkerEvent::ReadyToPaste {
                            job_id: Some(job_id),
                            copied_to_clipboard: false,
                        },
                        Err(error) => WorkerEvent::Completed {
                            job_id,
                            result: Err(error.to_string()),
                        },
                        Ok(PasteOutcome::Pasted | PasteOutcome::CopiedToClipboard) => {
                            unreachable!()
                        }
                    }
                }
            }
        }
        OutputJob::Completed { job_id, .. } | OutputJob::Cancelled { job_id } => {
            WorkerEvent::Cancelled { job_id }
        }
        OutputJob::PasteLast { target, .. } => {
            let Some(last) = last_transcript else {
                return WorkerEvent::Pasted {
                    result: Err("no previous transcript is available".into()),
                };
            };
            match paste(
                &last.completed.text,
                &target,
                PasteOptions {
                    submit_after_paste: None,
                    ..last.completed.options
                },
                &|| commit_output(state, &|| true),
            ) {
                Ok(result @ (PasteOutcome::Deferred | PasteOutcome::CopiedToClipboard)) => {
                    WorkerEvent::ReadyToPaste {
                        job_id: None,
                        copied_to_clipboard: result == PasteOutcome::CopiedToClipboard,
                    }
                }
                Ok(PasteOutcome::Pasted) => {
                    if !last.has_pasted {
                        last.completed.context = target;
                        if let Some(history) = history {
                            record_history(history, &last.completed);
                        }
                        last.has_pasted = true;
                    }
                    WorkerEvent::Pasted {
                        result: Ok(last.completed.text.clone()),
                    }
                }
                Err(error) => WorkerEvent::Pasted {
                    result: Err(error.to_string()),
                },
            }
        }
    }
}

/// Record one pasted result. History failures never fail the paste.
fn record_history(history: &History, completed: &CompletedTranscript) {
    let draft = HistoryDraft {
        text: completed.text.clone(),
        application: completed.context.application.clone(),
        audio_ms: completed.timings.audio_ms,
        inference_ms: completed.timings.inference_ms as u64,
        total_ms: completed.timings.total_started.elapsed().as_millis() as u64,
        transcription: completed.report.clone(),
    };
    if let Err(error) = history.record(draft) {
        tracing::warn!(%error, "could not record dictation history");
    }
}

fn join_worker(worker: Option<thread::JoinHandle<()>>, name: &str) {
    if let Some(worker) = worker
        && worker.join().is_err()
    {
        tracing::error!(worker = name, "worker panicked during shutdown");
    }
}

fn prioritize_transcription_thread() {
    const QOS_CLASS_USER_INITIATED: u32 = 0x19;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    // SAFETY: configures only the calling thread through the public macOS
    // pthread QoS API; relative priority zero is valid for this class.
    let status = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0) };
    if status != 0 {
        tracing::warn!(status, "could not raise dictation transcription QoS");
    }
}

fn queue_error(error: TrySendError<impl Sized>) -> &'static str {
    match error {
        TrySendError::Full(_) => "dictation queue is full",
        TrySendError::Disconnected(_) => "dictation worker is unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn completed(job_id: u64, text: &str) -> OutputJob {
        completed_for(job_id, text, ContextSnapshot::default())
    }

    fn completed_for(job_id: u64, text: &str, context: ContextSnapshot) -> OutputJob {
        OutputJob::Completed {
            job_id: DictationJobId(job_id),
            control: Arc::new(JobControl::default()),
            result: Box::new(Ok(CompletedTranscript {
                options: PasteOptions::default(),
                text: text.into(),
                context,
                timings: JobTimings {
                    total_started: Instant::now(),
                    queue_ms: 0,
                    audio_ms: 1_000,
                    inference_ms: 0,
                },
                report: None,
            })),
        }
    }

    fn target(name: &str, instance: u64) -> ContextSnapshot {
        ContextSnapshot {
            application: Some(name.into()),
            target: Some(crate::context::ForegroundApplication::Test(instance)),
        }
    }

    #[test]
    fn shutdown_after_a_committed_copy_does_not_report_it_as_cancelled() {
        let state = Mutex::new(WorkerState::default());
        let mut last = None;
        let event = finish_output(
            completed(0, "copied text"),
            &mut |_, _, _, commit| {
                assert!(commit());
                state.lock().unwrap().shutting_down = true;
                Ok(PasteOutcome::CopiedToClipboard)
            },
            &mut last,
            None,
            &state,
        );
        assert!(matches!(
            event,
            WorkerEvent::ReadyToPaste {
                copied_to_clipboard: true,
                ..
            }
        ));
        assert_eq!(last.unwrap().completed.text, "copied text");
    }

    #[test]
    fn copied_fallback_stays_available_without_becoming_a_history_entry() {
        let directory = std::env::temp_dir().join(format!(
            "hex-copied-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let history = History::new(crate::history::HistoryStore::open(
            directory.join("history.json"),
            crate::history::HistoryRetention::Week,
            crate::history::now_ms(),
        ));
        let state = Mutex::new(WorkerState::default());
        let mut last = None;
        let event = finish_output(
            completed(0, "fallback text"),
            &mut |_, _, _, commit| {
                assert!(commit());
                Ok(PasteOutcome::CopiedToClipboard)
            },
            &mut last,
            Some(&history),
            &state,
        );
        assert!(matches!(
            event,
            WorkerEvent::ReadyToPaste {
                copied_to_clipboard: true,
                ..
            }
        ));
        assert!(history.search("").is_empty());
        assert_eq!(last.as_ref().unwrap().completed.text, "fallback text");
        assert!(!last.as_ref().unwrap().has_pasted);
        let event = finish_output(
            OutputJob::PasteLast {
                sequence: 1,
                target: target("Notes", 1),
            },
            &mut |text, _, _, commit| {
                assert_eq!(text, "fallback text");
                assert!(commit());
                Ok(PasteOutcome::Pasted)
            },
            &mut last,
            Some(&history),
            &state,
        );
        assert!(matches!(event, WorkerEvent::Pasted { result: Ok(_) }));
        assert_eq!(history.search("").len(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deferred_results_are_kept_and_history_records_only_the_first_explicit_paste() {
        let directory = std::env::temp_dir().join(format!(
            "hex-paste-history-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let history = History::new(crate::history::HistoryStore::open(
            directory.join("history.json"),
            crate::history::HistoryRetention::Week,
            crate::history::now_ms(),
        ));
        let original = target("Editor", 1);
        // Same display name is deliberately insufficient to authorize a paste.
        let chosen = target("Editor", 2);
        let current = RefCell::new(chosen.clone());
        let writes = RefCell::new(Vec::new());
        let mut paste =
            |text: &str, expected: &ContextSnapshot, _submit, commit: &dyn Fn() -> bool| {
                assert!(commit());
                if expected.target.is_none() || expected.target != current.borrow().target {
                    return Ok(PasteOutcome::Deferred);
                }
                writes.borrow_mut().push(text.to_owned());
                Ok(PasteOutcome::Pasted)
            };
        let state = Mutex::new(WorkerState::default());
        let mut last = None;
        let event = finish_output(
            completed_for(0, "kept text", original),
            &mut paste,
            &mut last,
            Some(&history),
            &state,
        );
        assert!(matches!(
            event,
            WorkerEvent::ReadyToPaste {
                job_id: Some(_),
                ..
            }
        ));
        assert!(writes.borrow().is_empty());
        assert!(history.search("").is_empty());
        assert_eq!(last.as_ref().unwrap().completed.text, "kept text");

        // A second focus change while Paste Last waits also keeps the text.
        *current.borrow_mut() = target("Notes", 3);
        let event = finish_output(
            OutputJob::PasteLast {
                sequence: 1,
                target: chosen,
            },
            &mut paste,
            &mut last,
            Some(&history),
            &state,
        );
        assert!(matches!(
            event,
            WorkerEvent::ReadyToPaste { job_id: None, .. }
        ));
        assert!(history.search("").is_empty());
        for sequence in [2, 3] {
            let event = finish_output(
                OutputJob::PasteLast {
                    sequence,
                    target: target("Notes", 3),
                },
                &mut paste,
                &mut last,
                Some(&history),
                &state,
            );
            assert!(matches!(event, WorkerEvent::Pasted { result: Ok(_) }));
        }
        assert_eq!(*writes.borrow(), ["kept text", "kept text"]);
        let entries = history.search("");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].application.as_deref(), Some("Notes"));
        assert_eq!(entries[0].audio_ms, 1_000);
        drop(history);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancellation_or_shutdown_cannot_replace_last_with_a_deferred_result() {
        for shutdown in [false, true] {
            let state = Mutex::new(WorkerState::default());
            let mut last = None;
            finish_output(
                completed(0, "previous"),
                &mut |_, _, _submit, commit| {
                    assert!(commit());
                    Ok(PasteOutcome::Pasted)
                },
                &mut last,
                None,
                &state,
            );
            let job = completed(1, "cancelled text");
            let OutputJob::Completed { control, .. } = &job else {
                unreachable!()
            };
            let control = control.clone();
            let event = finish_output(
                job,
                &mut |_, _, _submit, commit| {
                    if shutdown {
                        state.lock().unwrap().shutting_down = true;
                    } else {
                        assert!(control.cancel());
                    }
                    assert!(!commit());
                    Ok(PasteOutcome::Deferred)
                },
                &mut last,
                None,
                &state,
            );
            assert!(matches!(event, WorkerEvent::Cancelled { .. }));
            assert_eq!(last.as_ref().unwrap().completed.text, "previous");
        }
    }

    #[test]
    fn a_deferred_job_releases_the_next_ordered_output() {
        let (pasted, pastes) = mpsc::channel();
        let mut count = 0;
        let worker = DictationWorker::start_with(
            None,
            move |_, _| {
                count += 1;
                Ok(test_transcription(if count == 1 { "held" } else { "next" }))
            },
            move || {
                Box::new(move |prepare_only, text, _, _submit, commit| {
                    if prepare_only {
                        return Ok(PasteOutcome::Pasted);
                    }
                    assert!(commit());
                    if text == "held" {
                        return Ok(PasteOutcome::Deferred);
                    }
                    pasted.send(text.to_owned()).unwrap();
                    Ok(PasteOutcome::Pasted)
                })
            },
        );
        submit_test_clip(&worker);
        submit_test_clip(&worker);
        assert_eq!(pastes.recv_timeout(TEST_TIMEOUT).unwrap(), "next");
        let mut ready = false;
        let deadline = Instant::now() + TEST_TIMEOUT;
        while worker.pending_count() > 0 && Instant::now() < deadline {
            if matches!(worker.try_recv(), Some(WorkerEvent::ReadyToPaste { .. })) {
                ready = true;
            }
            thread::yield_now();
        }
        while let Some(event) = worker.try_recv() {
            ready |= matches!(event, WorkerEvent::ReadyToPaste { .. });
        }
        assert!(ready);
        assert_eq!(worker.pending_count(), 0);
    }

    #[test]
    fn outputs_release_in_submission_order() {
        let mut ordered = OrderedOutputs::default();
        assert!(ordered.push(completed(1, "second")).is_empty());
        let ready = ordered.push(completed(0, "first"));
        let sequences: Vec<u64> = ready.iter().map(OutputJob::sequence).collect();
        assert_eq!(sequences, [0, 1]);
        assert!(ordered.push(completed(0, "stale")).is_empty());
    }

    #[test]
    fn cancellation_wins_only_before_output_begins() {
        let control = JobControl::default();
        assert!(control.cancel());
        assert!(!control.cancel(), "a second cancel is a no-op");
        assert!(!control.begin_output());

        let control = JobControl::default();
        assert!(control.begin_output());
        assert!(!control.cancel(), "started output cannot be cancelled");
    }

    #[test]
    fn completed_text_is_pasted_and_becomes_the_last_transcript() {
        let pasted = RefCell::new(Vec::new());
        let mut last = None;
        let state = Mutex::new(WorkerState::default());
        let event = finish_output(
            completed(0, "olá"),
            &mut |text, _, _submit, commit| {
                assert!(commit());
                pasted.borrow_mut().push(text.to_owned());
                Ok(PasteOutcome::Pasted)
            },
            &mut last,
            None,
            &state,
        );
        assert!(
            matches!(event, WorkerEvent::Completed { result: Ok(ref text), .. } if text == "olá")
        );
        assert_eq!(*pasted.borrow(), ["olá"]);

        let event = finish_output(
            OutputJob::PasteLast {
                sequence: 1,
                target: ContextSnapshot::default(),
            },
            &mut |text, _, _submit, commit| {
                assert!(commit());
                pasted.borrow_mut().push(text.to_owned());
                Ok(PasteOutcome::Pasted)
            },
            &mut last,
            None,
            &state,
        );
        assert!(matches!(event, WorkerEvent::Pasted { result: Ok(_) }));
        assert_eq!(*pasted.borrow(), ["olá", "olá"]);
    }

    #[test]
    fn submit_intent_belongs_to_one_output_and_is_not_replayed_by_paste_last() {
        let mut job = completed(0, "send this");
        if let OutputJob::Completed { result, .. } = &mut job {
            result.as_mut().as_mut().unwrap().options.submit_after_paste = Some(17);
        }
        let state = Mutex::new(WorkerState::default());
        let mut last = None;
        let intents = RefCell::new(Vec::new());
        let mut paste =
            |_: &str, _: &ContextSnapshot, options: PasteOptions, commit: &dyn Fn() -> bool| {
                assert!(commit());
                intents.borrow_mut().push(options.submit_after_paste);
                Ok(PasteOutcome::Deferred)
            };
        assert!(matches!(
            finish_output(job, &mut paste, &mut last, None, &state),
            WorkerEvent::ReadyToPaste { .. }
        ));
        finish_output(
            OutputJob::PasteLast {
                sequence: 1,
                target: ContextSnapshot::default(),
            },
            &mut paste,
            &mut last,
            None,
            &state,
        );
        assert_eq!(*intents.borrow(), [Some(17), None]);
    }

    #[test]
    fn cancelled_and_empty_results_never_paste() {
        let job = completed(0, "text");
        if let OutputJob::Completed { control, .. } = &job {
            control.cancel();
        }
        let mut last = None;
        let state = Mutex::new(WorkerState::default());
        let event = finish_output(
            job,
            &mut |_, _, _submit, _| panic!("no paste"),
            &mut last,
            None,
            &state,
        );
        assert!(matches!(event, WorkerEvent::Cancelled { .. }));

        let event = finish_output(
            completed(1, ""),
            &mut |_, _, _submit, _| panic!("no paste"),
            &mut last,
            None,
            &state,
        );
        assert!(
            matches!(event, WorkerEvent::Completed { result: Ok(ref text), .. } if text.is_empty())
        );
        assert!(last.is_none());
    }

    #[test]
    fn shutdown_suppresses_empty_and_failed_completion_feedback() {
        for fail in [false, true] {
            let mut job = completed(0, "");
            if fail && let OutputJob::Completed { result, .. } = &mut job {
                *result = Box::new(Err("transcription failed".into()));
            }
            let state = Mutex::new(WorkerState {
                shutting_down: true,
                ..WorkerState::default()
            });
            let mut last = None;
            let event = finish_output(
                job,
                &mut |_, _, _submit, _| panic!("no paste"),
                &mut last,
                None,
                &state,
            );
            assert!(matches!(event, WorkerEvent::Cancelled { .. }));
            assert!(last.is_none());
        }
    }

    #[test]
    fn post_processing_snapshot_reaches_history_and_paste_last_and_empty_output_never_pastes() {
        use crate::post_processing::Preferences;
        let formatted = Preferences {
            lowercase: true,
            remove_ellipses: true,
            remove_final_period: true,
            collapse_spaces: true,
            ..Preferences::default()
        };
        let (jobs, receiver) = mpsc::sync_channel(4);
        let (output, outputs) = mpsc::sync_channel(4);
        let (events, _) = mpsc::channel();
        for (index, preferences) in [
            formatted,
            Preferences::default(),
            Preferences {
                remove_ellipses: true,
                ..Preferences::default()
            },
        ]
        .into_iter()
        .enumerate()
        {
            jobs.send(TranscriptionJob {
                job_id: DictationJobId(index as u64),
                control: Arc::new(JobControl::default()),
                submitted_at: Instant::now(),
                clip: DictationClip::from_samples(vec![0.1; 1_600]),
                context: ContextSnapshot {
                    application: Some(index.to_string()),
                    ..Default::default()
                },
                options: PasteOptions {
                    post_processing: preferences,
                    submit_after_paste: Some(17),
                },
            })
            .unwrap();
        }
        drop(jobs);
        let state = Mutex::new(WorkerState::default());
        run_transcription_worker(receiver, &output, &events, &state, |_, context| {
            Ok(test_transcription(
                if context.application.as_deref() == Some("2") {
                    "..."
                } else {
                    "Olá...  JOÃO."
                },
            ))
        });
        let directory = std::env::temp_dir().join(format!(
            "hex-post-history-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let history = History::new(crate::history::HistoryStore::open(
            directory.join("history.json"),
            crate::history::HistoryRetention::Week,
            crate::history::now_ms(),
        ));
        let mut last = None;
        finish_output(
            outputs.recv().unwrap(),
            &mut |text, _, options, commit| {
                assert_eq!(text, "olá joão");
                assert_eq!(options.post_processing, formatted);
                assert_eq!(options.submit_after_paste, Some(17));
                assert!(commit());
                Ok(PasteOutcome::Pasted)
            },
            &mut last,
            Some(&history),
            &state,
        );
        assert_eq!(history.search("")[0].text, "olá joão");
        finish_output(
            OutputJob::PasteLast {
                sequence: 3,
                target: ContextSnapshot::default(),
            },
            &mut |text, _, options, commit| {
                assert_eq!(text, "olá joão");
                assert_eq!(options.post_processing, formatted);
                assert_eq!(options.submit_after_paste, None);
                assert!(commit());
                Ok(PasteOutcome::Pasted)
            },
            &mut last,
            Some(&history),
            &state,
        );
        assert_eq!(history.search("").len(), 1);
        let OutputJob::Completed { result, .. } = outputs.recv().unwrap() else {
            panic!("completed")
        };
        assert_eq!(result.unwrap().text, "Olá...  JOÃO.");
        let event = finish_output(
            outputs.recv().unwrap(),
            &mut |_, _, _, _| panic!("empty formatting must not paste or send Enter"),
            &mut last,
            Some(&history),
            &state,
        );
        assert!(
            matches!(event, WorkerEvent::Completed { result: Ok(text), .. } if text.is_empty())
        );
        assert_eq!(history.search("").len(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transcription_errors_reach_the_output_in_order() {
        let (jobs, receiver) = mpsc::sync_channel(4);
        let (output, outputs) = mpsc::sync_channel(4);
        let (events, _events) = mpsc::channel();
        let control = Arc::new(JobControl::default());
        jobs.send(TranscriptionJob {
            options: PasteOptions::default(),
            job_id: DictationJobId(0),
            control,
            submitted_at: Instant::now(),
            clip: DictationClip::from_samples(vec![0.1; 1_600]),
            context: ContextSnapshot {
                application: Some("Notes".into()),
                ..Default::default()
            },
        })
        .unwrap();
        drop(jobs);
        run_transcription_worker(
            receiver,
            &output,
            &events,
            &Mutex::new(WorkerState::default()),
            |_, context| {
                assert_eq!(context.application.as_deref(), Some("Notes"));
                Err(color_eyre::eyre::eyre!("offline"))
            },
        );
        match outputs.recv().unwrap() {
            OutputJob::Completed { result, .. } => match *result {
                Err(error) => assert_eq!(error, "offline"),
                Ok(_) => panic!("error expected"),
            },
            _ => panic!("completed job expected"),
        }
    }

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn test_transcription(text: &str) -> crate::openrouter::transcribe::Transcription {
        crate::openrouter::transcribe::Transcription {
            text: text.into(),
            report: None,
        }
    }

    fn submit_test_clip(worker: &DictationWorker) {
        worker
            .transcribe(
                DictationClip::from_samples(vec![0.1; 1_600]),
                ContextSnapshot::default(),
                None,
            )
            .unwrap();
    }

    fn wait_for_shutdown(state: &Mutex<WorkerState>) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !is_shutting_down(state) {
            assert!(Instant::now() < deadline, "shutdown did not begin");
            thread::yield_now();
        }
    }

    #[test]
    fn drop_does_not_wait_for_network_or_paste_queued_work() {
        use std::sync::atomic::AtomicUsize;

        let calls = Arc::new(AtomicUsize::new(0));
        let (started, network_started) = mpsc::channel();
        let (release, network_release) = mpsc::channel();
        let (network_alive, network_stopped) = mpsc::channel::<()>();
        let (pasted, pastes) = mpsc::channel();
        let worker = DictationWorker::start_with(
            None,
            {
                let calls = calls.clone();
                move |_, _| {
                    let _alive = &network_alive;
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Ok(test_transcription("previous"));
                    }
                    started.send(()).unwrap();
                    network_release.recv_timeout(TEST_TIMEOUT).unwrap();
                    Ok(test_transcription("must not paste"))
                }
            },
            move || {
                Box::new(move |prepare_only, text, _, _submit, commit| {
                    if !prepare_only {
                        assert!(commit());
                        pasted.send(text.to_owned()).unwrap();
                    }
                    Ok(PasteOutcome::Pasted)
                })
            },
        );
        submit_test_clip(&worker);
        assert_eq!(pastes.recv_timeout(TEST_TIMEOUT).unwrap(), "previous");
        submit_test_clip(&worker);
        network_started.recv_timeout(TEST_TIMEOUT).unwrap();
        submit_test_clip(&worker);
        worker.paste_last(ContextSnapshot::default()).unwrap();

        let (dropped, drop_finished) = mpsc::channel();
        let drop_worker = thread::spawn(move || {
            drop(worker);
            dropped.send(()).unwrap();
        });
        let shutdown_result = drop_finished.recv_timeout(Duration::from_secs(2));
        // Release even if shutdown regressed, so a failed test leaves no blocked worker.
        release.send(()).unwrap();
        drop_worker.join().unwrap();
        assert!(
            shutdown_result.is_ok(),
            "shutdown waited for the remote request"
        );
        assert_eq!(
            network_stopped.recv_timeout(TEST_TIMEOUT),
            Err(RecvTimeoutError::Disconnected)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "queued clip was transcribed"
        );
        assert_eq!(pastes.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn shutdown_rejects_prepared_paste_last_but_finishes_committed_output() {
        for committed_before_shutdown in [false, true] {
            let (prepared, paste_prepared) = mpsc::channel();
            let (release, paste_release) = mpsc::channel();
            let (pasted, pastes) = mpsc::channel();
            let worker = DictationWorker::start_with(
                None,
                |_, _| Ok(test_transcription("previous")),
                move || {
                    let mut first = true;
                    Box::new(move |prepare_only, text, _, _submit, commit| {
                        if prepare_only {
                            return Ok(PasteOutcome::Pasted);
                        }
                        let accepted = if first {
                            first = false;
                            commit()
                        } else {
                            let committed = committed_before_shutdown && commit();
                            prepared.send(()).unwrap();
                            paste_release.recv_timeout(TEST_TIMEOUT).unwrap();
                            committed || commit()
                        };
                        if !accepted {
                            return Err(color_eyre::eyre::eyre!("paste cancelled"));
                        }
                        pasted.send(text.to_owned()).unwrap();
                        Ok(PasteOutcome::Pasted)
                    })
                },
            );
            submit_test_clip(&worker);
            assert_eq!(pastes.recv_timeout(TEST_TIMEOUT).unwrap(), "previous");
            worker.paste_last(ContextSnapshot::default()).unwrap();
            paste_prepared.recv_timeout(TEST_TIMEOUT).unwrap();

            let state = worker.state.clone();
            let (dropped, drop_finished) = mpsc::channel();
            let drop_worker = thread::spawn(move || {
                drop(worker);
                dropped.send(()).unwrap();
            });
            wait_for_shutdown(&state);
            assert_eq!(drop_finished.try_recv(), Err(mpsc::TryRecvError::Empty));
            release.send(()).unwrap();
            drop_finished.recv_timeout(TEST_TIMEOUT).unwrap();
            drop_worker.join().unwrap();

            let remaining: Vec<_> = pastes.try_iter().collect();
            if committed_before_shutdown {
                assert_eq!(remaining, ["previous"]);
            } else {
                assert!(remaining.is_empty(), "Paste Last committed after shutdown");
            }
            assert_eq!(pastes.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        }
    }
}
