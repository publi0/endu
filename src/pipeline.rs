//! The dictation pipeline: one transcription worker that sends each finished
//! clip to OpenRouter, and one output worker that pastes results in
//! submission order, records History, and answers "paste last".
//!
//! Capture never waits on this pipeline. Its queues are bounded, accepted
//! jobs keep their order, and a cancelled job never pastes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use color_eyre::eyre::Result;

use crate::context::ContextSnapshot;
use crate::dictation::DictationClip;
use crate::history::{History, HistoryDraft};
use crate::openrouter::StepReport;
use crate::paste::Paster;
use crate::suppression::InputActivity;

const MAX_PENDING_OUTPUTS: usize = 16;

pub enum WorkerEvent {
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
    text: String,
    application: Option<String>,
    timings: JobTimings,
    report: Option<StepReport>,
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
    },
}

impl OutputJob {
    fn sequence(&self) -> u64 {
        match self {
            Self::PreparePaste => u64::MAX,
            Self::Completed { job_id, .. } | Self::Cancelled { job_id } => job_id.0,
            Self::PasteLast { sequence } => *sequence,
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

struct WorkerState {
    next_sequence: u64,
    jobs: BTreeMap<DictationJobId, Arc<JobControl>>,
    pending_pastes: usize,
}

impl WorkerState {
    fn next_output_sequence(&self) -> Result<u64, &'static str> {
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
    pub fn start(activity: InputActivity, history: Option<History>) -> Self {
        let (transcription_jobs, transcription_receiver) =
            mpsc::sync_channel::<TranscriptionJob>(4);
        let (output_jobs, output_receiver) = mpsc::sync_channel::<OutputJob>(8);
        let (event_sender, events) = mpsc::channel();
        let state = Arc::new(Mutex::new(WorkerState {
            next_sequence: 0,
            jobs: BTreeMap::new(),
            pending_pastes: 0,
        }));

        let output_worker = thread::spawn({
            let state = state.clone();
            let events = event_sender.clone();
            move || {
                let mut paster = Paster::new(activity);
                run_output_worker(
                    output_receiver,
                    &mut |prepare_only, text, commit| {
                        if prepare_only {
                            paster.prepare();
                            Ok(())
                        } else {
                            paster.paste(text, commit)
                        }
                    },
                    history,
                    &state,
                    &events,
                )
            }
        });

        let transcription_worker = thread::spawn({
            let output = output_jobs.clone();
            move || {
                run_transcription_worker(
                    transcription_receiver,
                    &output,
                    &event_sender,
                    crate::openrouter::transcribe::transcribe,
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
        if !state.jobs.is_empty() || state.pending_pastes > 0 {
            return;
        }
        drop(state);
        if let Some(output_jobs) = &self.output_jobs {
            let _ = output_jobs.try_send(OutputJob::PreparePaste);
        }
    }

    pub fn paste_last(&self) -> Result<(), &'static str> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let sequence = state.next_output_sequence()?;
        self.output_jobs
            .as_ref()
            .ok_or("dictation worker is unavailable")?
            .try_send(OutputJob::PasteLast { sequence })
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
        self.transcription_jobs.take();
        join_worker(self.transcription_worker.take(), "dictation transcription");
        self.output_jobs.take();
        join_worker(self.output_worker.take(), "dictation output");
    }
}

impl Drop for DictationWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

type Transcribe = fn(&[f32]) -> Result<crate::openrouter::transcribe::Transcription>;

fn run_transcription_worker(
    jobs: Receiver<TranscriptionJob>,
    output: &SyncSender<OutputJob>,
    events: &mpsc::Sender<WorkerEvent>,
    transcribe: Transcribe,
) {
    prioritize_transcription_thread();
    while let Ok(job) = jobs.recv() {
        if job.control.is_cancelled() {
            let _ = output.send(OutputJob::Cancelled { job_id: job.job_id });
            continue;
        }
        let _ = events.send(WorkerEvent::Transcribing { job_id: job.job_id });
        let queue_ms = job.submitted_at.elapsed().as_millis();
        let audio_ms = job.clip.duration_ms();
        let samples = job.clip.into_transcription_samples();
        let started = Instant::now();
        let result = transcribe(&samples);
        let timings = JobTimings {
            total_started: job.submitted_at,
            queue_ms,
            audio_ms,
            inference_ms: started.elapsed().as_millis(),
        };
        let application = job.context.application;
        let result = result
            .map(|transcription| CompletedTranscript {
                text: transcription.text.trim().to_owned(),
                application,
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
type PasteFn<'a> = dyn FnMut(bool, &str, &dyn Fn() -> bool) -> Result<()> + 'a;

fn run_output_worker(
    jobs: Receiver<OutputJob>,
    paste: &mut PasteFn<'_>,
    history: Option<History>,
    state: &Mutex<WorkerState>,
    events: &mpsc::Sender<WorkerEvent>,
) {
    let mut last_transcript = None;
    let mut ordered = OrderedOutputs::default();
    while let Ok(job) = jobs.recv() {
        if matches!(job, OutputJob::PreparePaste) {
            let _ = paste(true, "", &|| true);
            continue;
        }
        for job in ordered.push(job) {
            let event = finish_output(
                job,
                &mut |text, commit| paste(false, text, commit),
                &mut last_transcript,
                history.as_ref(),
            );
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            match &event {
                WorkerEvent::Completed { job_id, .. } | WorkerEvent::Cancelled { job_id } => {
                    state.jobs.remove(job_id);
                }
                WorkerEvent::Pasted { .. } => {
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

fn finish_output(
    job: OutputJob,
    paste: &mut dyn FnMut(&str, &dyn Fn() -> bool) -> Result<()>,
    last_transcript: &mut Option<String>,
    history: Option<&History>,
) -> WorkerEvent {
    match job {
        OutputJob::PreparePaste => unreachable!("paste preparation bypasses ordered output"),
        OutputJob::Completed {
            job_id,
            control,
            result,
        } if !control.is_cancelled() => {
            let result = (*result).and_then(|completed| {
                let paste_started = Instant::now();
                if !completed.text.is_empty() {
                    let commit = || control.begin_output();
                    paste(&completed.text, &commit).map_err(|error| error.to_string())?;
                    *last_transcript = Some(completed.text.clone());
                    if let Some(history) = history {
                        record_history(history, &completed);
                    }
                }
                tracing::info!(
                    audio_ms = completed.timings.audio_ms,
                    queue_ms = completed.timings.queue_ms,
                    inference_ms = completed.timings.inference_ms,
                    paste_ms = paste_started.elapsed().as_millis(),
                    total_ms = completed.timings.total_started.elapsed().as_millis(),
                    "dictation pipeline completed"
                );
                Ok(completed.text)
            });
            if control.is_cancelled() {
                WorkerEvent::Cancelled { job_id }
            } else {
                WorkerEvent::Completed { job_id, result }
            }
        }
        OutputJob::Completed { job_id, .. } | OutputJob::Cancelled { job_id } => {
            WorkerEvent::Cancelled { job_id }
        }
        OutputJob::PasteLast { .. } => {
            let result = last_transcript
                .as_deref()
                .ok_or_else(|| "no previous transcript is available".to_string())
                .and_then(|text| {
                    paste(text, &|| true).map_err(|error| error.to_string())?;
                    Ok(text.to_string())
                });
            WorkerEvent::Pasted { result }
        }
    }
}

/// Record one pasted result. History failures never fail the paste.
fn record_history(history: &History, completed: &CompletedTranscript) {
    let draft = HistoryDraft {
        text: completed.text.clone(),
        application: completed.application.clone(),
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
        OutputJob::Completed {
            job_id: DictationJobId(job_id),
            control: Arc::new(JobControl::default()),
            result: Box::new(Ok(CompletedTranscript {
                text: text.into(),
                application: None,
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
        let event = finish_output(
            completed(0, "olá"),
            &mut |text, commit| {
                assert!(commit());
                pasted.borrow_mut().push(text.to_owned());
                Ok(())
            },
            &mut last,
            None,
        );
        assert!(
            matches!(event, WorkerEvent::Completed { result: Ok(ref text), .. } if text == "olá")
        );
        assert_eq!(*pasted.borrow(), ["olá"]);

        let event = finish_output(
            OutputJob::PasteLast { sequence: 1 },
            &mut |text, _| {
                pasted.borrow_mut().push(text.to_owned());
                Ok(())
            },
            &mut last,
            None,
        );
        assert!(matches!(event, WorkerEvent::Pasted { result: Ok(_) }));
        assert_eq!(*pasted.borrow(), ["olá", "olá"]);
    }

    #[test]
    fn cancelled_and_empty_results_never_paste() {
        let job = completed(0, "text");
        if let OutputJob::Completed { control, .. } = &job {
            control.cancel();
        }
        let mut last = None;
        let event = finish_output(job, &mut |_, _| panic!("no paste"), &mut last, None);
        assert!(matches!(event, WorkerEvent::Cancelled { .. }));

        let event = finish_output(
            completed(1, ""),
            &mut |_, _| panic!("no paste"),
            &mut last,
            None,
        );
        assert!(
            matches!(event, WorkerEvent::Completed { result: Ok(ref text), .. } if text.is_empty())
        );
        assert!(last.is_none());
    }

    #[test]
    fn transcription_errors_reach_the_output_in_order() {
        let (jobs, receiver) = mpsc::sync_channel(4);
        let (output, outputs) = mpsc::sync_channel(4);
        let (events, _events) = mpsc::channel();
        let control = Arc::new(JobControl::default());
        jobs.send(TranscriptionJob {
            job_id: DictationJobId(0),
            control,
            submitted_at: Instant::now(),
            clip: DictationClip::from_samples(vec![0.1; 1_600]),
            context: ContextSnapshot::default(),
        })
        .unwrap();
        drop(jobs);
        run_transcription_worker(receiver, &output, &events, |_| {
            Err(color_eyre::eyre::eyre!("offline"))
        });
        match outputs.recv().unwrap() {
            OutputJob::Completed { result, .. } => match *result {
                Err(error) => assert_eq!(error, "offline"),
                Ok(_) => panic!("error expected"),
            },
            _ => panic!("completed job expected"),
        }
    }
}
