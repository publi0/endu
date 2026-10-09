//! A level-triggered wake-up for loops that sleep while idle. Producers ring
//! after queueing work; the loop drains its queues on every return, so a ring
//! that arrives while it is busy is consumed by the next wait at no cost.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[derive(Default)]
struct State {
    rung: bool,
    waker: Option<Waker>,
}

#[derive(Default)]
pub struct Doorbell {
    state: Mutex<State>,
    rung: Condvar,
}

impl Doorbell {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                rung: false,
                waker: None,
            }),
            rung: Condvar::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Wakes the waiting thread or task. Safe from any thread and cheap
    /// enough for an event-tap callback.
    pub fn ring(&self) {
        let waker = {
            let mut state = self.state();
            state.rung = true;
            state.waker.take()
        };
        self.rung.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Blocks until rung or `timeout` elapses. Returns whether it was rung,
    /// consuming the ring.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let state = self.state();
        let (mut state, _) = self
            .rung
            .wait_timeout_while(state, timeout, |state| !state.rung)
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut state.rung)
    }

    /// Completes when rung or when `timer` completes, consuming any ring.
    pub async fn ring_or<F: Future>(&self, timer: F) {
        RingOr {
            doorbell: self,
            timer: std::pin::pin!(timer),
        }
        .await;
    }
}

struct RingOr<'a, F> {
    doorbell: &'a Doorbell,
    timer: Pin<&'a mut F>,
}

impl<F: Future> Future for RingOr<'_, F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        {
            let mut state = self.doorbell.state();
            if std::mem::take(&mut state.rung) {
                return Poll::Ready(());
            }
            // Registered under the lock, so a ring cannot slip between the
            // check above and this registration.
            state.waker = Some(cx.waker().clone());
        }
        self.timer.as_mut().poll(cx).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn a_ring_before_the_wait_is_not_lost_and_is_consumed_once() {
        let doorbell = Doorbell::new();
        doorbell.ring();
        doorbell.ring();
        assert!(doorbell.wait_timeout(Duration::from_secs(5)));
        assert!(!doorbell.wait_timeout(Duration::ZERO));
    }

    #[test]
    fn a_ring_from_another_thread_ends_a_long_wait() {
        let doorbell = Arc::new(Doorbell::new());
        let ringer = doorbell.clone();
        let started = Instant::now();
        let ring = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            ringer.ring();
        });
        assert!(doorbell.wait_timeout(Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_secs(10));
        ring.join().unwrap();
    }

    #[test]
    fn an_unrung_wait_times_out() {
        assert!(!Doorbell::new().wait_timeout(Duration::from_millis(1)));
    }

    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn the_async_wait_completes_on_a_ring_or_its_timer() {
        let doorbell = Doorbell::new();
        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);

        let mut waiting = std::pin::pin!(doorbell.ring_or(std::future::pending::<()>()));
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        doorbell.ring();
        assert_eq!(counter.0.load(Ordering::Acquire), 1);
        assert!(waiting.as_mut().poll(&mut cx).is_ready());

        let mut timed = std::pin::pin!(doorbell.ring_or(std::future::ready(())));
        assert!(timed.as_mut().poll(&mut cx).is_ready());
        // The ring was consumed by the first wait.
        assert!(!doorbell.wait_timeout(Duration::ZERO));
    }
}
