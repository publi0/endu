//! Microphone stream overloads: a glitch the host reports while the stream
//! keeps running, and the one coalesced line the audio owner writes about
//! them.
//!
//! CoreAudio reports a processor overload (`kAudioDeviceProcessorOverload`)
//! as `ErrorKind::Xrun` from its real-time thread, where a log line, a lock or
//! an allocation blocks an already-late cycle, so `record` only moves one
//! counter. `report` runs on the audio owner's thread, which drains the
//! stream, and `flush` on the one that drops it, where formatting and a
//! blocking write are harmless. Real sample gaps are not this module's
//! business: the owner's timeline still detects them from chunk timestamps.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Overloads reported since the last line.
static GLITCHES: AtomicU64 = AtomicU64::new(0);
/// When the last line was written. Only the owner side touches it.
static REPORTED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// The shortest gap between two overload lines. Overloads arrive in bursts
/// while a cold device settles or the machine is loaded, so one line per
/// burst keeps the log readable. A burst held back by this window is counted
/// by the next line, at most this much later.
const LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Records an overload from the thread that runs the stream: one relaxed
/// atomic add, no logging, allocation, clock read or lock.
pub(super) fn record() {
    GLITCHES.fetch_add(1, Ordering::Relaxed);
}

/// Whether the overloads a poll found are reported now, given how long ago the
/// last line was written: the first burst reports at once, and a continuing
/// one at most every [`LOG_INTERVAL`].
fn report_due(since_last_report: Option<Duration>) -> bool {
    since_last_report.is_none_or(|since| since >= LOG_INTERVAL)
}

/// Reports the overloads recorded since the last line, from the owner's
/// thread. The pending count is taken only when a line is due, so a burst the
/// rate limit holds back is reported late rather than lost.
pub(super) fn report() {
    if GLITCHES.load(Ordering::Relaxed) == 0 {
        return;
    }
    let now = Instant::now();
    let mut reported_at = REPORTED_AT
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !report_due(reported_at.map(|at| now.saturating_duration_since(at))) {
        return;
    }
    *reported_at = Some(now);
    drop(reported_at);
    write_pending();
}

/// Writes the count a closing stream leaves behind, without the rate limit:
/// its burst ended with the stream, so it does not belong beside the next
/// open's lines.
pub(super) fn flush() {
    if GLITCHES.load(Ordering::Relaxed) == 0 {
        return;
    }
    write_pending();
}

/// Writes the pending count, if there is one, exactly once.
fn write_pending() {
    let count = GLITCHES.swap(0, Ordering::AcqRel);
    if count == 0 {
        return;
    }
    tracing::warn!(count, "microphone stream overloaded; continuing");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line per burst: the first reports at once, and a continuing one
    /// waits out the window, so the count it holds is reported by the next
    /// poll rather than lost.
    #[test]
    fn a_burst_writes_one_line_and_holds_the_rest() {
        assert!(report_due(None));
        assert!(!report_due(Some(Duration::ZERO)));
        assert!(!report_due(Some(LOG_INTERVAL - Duration::from_millis(1))));
        assert!(report_due(Some(LOG_INTERVAL)));
        assert!(report_due(Some(LOG_INTERVAL * 3)));
    }
}
