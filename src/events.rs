use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VoiceEvent {
    SessionStarted {
        timestamp_ms: u64,
    },
    State {
        timestamp_ms: u64,
        state: VoiceState,
        device: String,
    },
    Dictation {
        timestamp_ms: u64,
        phase: DictationPhase,
        #[serde(default)]
        text: String,
    },
    Context {
        timestamp_ms: u64,
        application: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    Listening,
    Dictating,
    Transcribing,
    Stopping,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DictationPhase {
    Started,
    Discarded,
    Cancelled,
    Transcribing,
    Pasted,
    Repasted,
    ReadyToPaste,
    CopiedToClipboard,
    Failed(String),
}

#[derive(Clone)]
pub struct EventLog {
    inner: Arc<EventLogInner>,
}

struct EventLogInner {
    sender: Option<SyncSender<WriterMessage>>,
    error: Arc<Mutex<Option<(io::ErrorKind, String)>>>,
    worker: Option<JoinHandle<()>>,
}

enum WriterMessage {
    Event(VoiceEvent),
    Flush(mpsc::SyncSender<Option<(io::ErrorKind, String)>>),
}

const EVENT_WRITER_CAPACITY: usize = 1_024;

impl EventLog {
    pub fn create(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let (sender, receiver) = mpsc::sync_channel(EVENT_WRITER_CAPACITY);
        let error = Arc::new(Mutex::new(None));
        let worker_error = error.clone();
        let worker = thread::Builder::new()
            .name("event-writer".into())
            .spawn(move || run_event_writer(BufWriter::new(file), receiver, worker_error))?;
        Ok(Self {
            inner: Arc::new(EventLogInner {
                sender: Some(sender),
                error,
                worker: Some(worker),
            }),
        })
    }

    pub fn emit(&self, event: &VoiceEvent) -> io::Result<()> {
        self.check_error()?;
        let sender = self
            .inner
            .sender
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "event writer stopped"))?;
        match sender.try_send(WriterMessage::Event(event.clone())) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(WriterMessage::Event(event))) if event.is_replaceable() => {
                Ok(())
            }
            Err(TrySendError::Full(message)) => sender
                .send(message)
                .map_err(|_| self.writer_stopped_error()),
            Err(TrySendError::Disconnected(_)) => Err(self.writer_stopped_error()),
        }
    }

    pub fn flush(&self) -> io::Result<()> {
        self.check_error()?;
        let (reply, flushed) = mpsc::sync_channel(0);
        self.inner
            .sender
            .as_ref()
            .ok_or_else(|| self.writer_stopped_error())?
            .send(WriterMessage::Flush(reply))
            .map_err(|_| self.writer_stopped_error())?;
        match flushed.recv().map_err(|_| self.writer_stopped_error())? {
            Some((kind, message)) => Err(io::Error::new(kind, message)),
            None => Ok(()),
        }
    }

    fn check_error(&self) -> io::Result<()> {
        match self
            .inner
            .error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            Some((kind, message)) => Err(io::Error::new(*kind, message.clone())),
            None => Ok(()),
        }
    }

    fn writer_stopped_error(&self) -> io::Error {
        self.check_error()
            .err()
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "event writer stopped"))
    }

    pub fn dictation(&self, phase: DictationPhase, text: impl Into<String>) -> io::Result<()> {
        self.emit(&VoiceEvent::Dictation {
            timestamp_ms: now_ms(),
            phase,
            text: text.into(),
        })
    }
}

impl VoiceEvent {
    fn is_replaceable(&self) -> bool {
        matches!(self, Self::Context { .. })
    }
}

impl Drop for EventLogInner {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_event_writer(
    mut writer: BufWriter<File>,
    receiver: mpsc::Receiver<WriterMessage>,
    error: Arc<Mutex<Option<(io::ErrorKind, String)>>>,
) {
    while let Ok(message) = receiver.recv() {
        match message {
            WriterMessage::Event(event) => {
                let result = serde_json::to_writer(&mut writer, &event)
                    .map_err(io::Error::other)
                    .and_then(|()| writer.write_all(b"\n"))
                    .and_then(|()| writer.flush());
                if let Err(write_error) = result {
                    *error.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some((write_error.kind(), write_error.to_string()));
                    break;
                }
            }
            WriterMessage::Flush(reply) => {
                let failure = writer
                    .flush()
                    .err()
                    .map(|write_error| (write_error.kind(), write_error.to_string()));
                if let Some(failure) = &failure {
                    *error.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(failure.clone());
                }
                let failed = failure.is_some();
                let _ = reply.send(failure);
                if failed {
                    break;
                }
            }
        }
    }
    let _ = writer.flush();
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_event_equality_preserves_serialized_identity() {
        let fixtures = serde_json::json!([
            {"kind": "session_started", "timestamp_ms": 42},
            {"kind": "state", "timestamp_ms": 42, "state": "listening", "device": "Test"},
            {"kind": "dictation", "timestamp_ms": 42, "phase": "pasted", "text": "olá"},
            {"kind": "dictation", "timestamp_ms": 42, "phase": "pasted", "text": "other"},
            {"kind": "dictation", "timestamp_ms": 42, "phase": {"failed": "timeout"}},
            {"kind": "context", "timestamp_ms": 42, "application": null},
            {"kind": "context", "timestamp_ms": 42, "application": "Test"}
        ]);
        let events: Vec<VoiceEvent> = serde_json::from_value(fixtures).unwrap();
        for left in &events {
            let serialized = serde_json::to_string(left).unwrap();
            let decoded: VoiceEvent = serde_json::from_str(&serialized).unwrap();
            assert_eq!(left, &decoded);
            for right in &events {
                assert_eq!(
                    left == right,
                    serialized == serde_json::to_string(right).unwrap(),
                    "{left:?} vs {right:?}",
                );
            }
        }
    }

    #[test]
    fn dictation_events_round_trip_through_ndjson() {
        let event = VoiceEvent::Dictation {
            timestamp_ms: 42,
            phase: DictationPhase::Failed("timeout".into()),
            text: String::new(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<VoiceEvent>(&json).unwrap(), event);
    }

    #[test]
    fn records_from_removed_features_are_rejected_not_misread() {
        assert!(
            serde_json::from_str::<VoiceEvent>(
                r#"{"kind":"command","timestamp_ms":1,"heard":"x","command":null,"outcome":"ignored"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn reopening_log_preserves_existing_events() {
        let path = std::env::temp_dir().join(format!(
            "voice-control-events-{}-{}.ndjson",
            std::process::id(),
            now_ms()
        ));
        let event = VoiceEvent::State {
            timestamp_ms: 1,
            state: VoiceState::Listening,
            device: "test".into(),
        };
        EventLog::create(&path).unwrap().emit(&event).unwrap();
        EventLog::create(&path).unwrap().emit(&event).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
        fs::remove_file(path).unwrap();
    }
}
