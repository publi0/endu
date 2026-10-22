//! The shared HTTP agent for recorded uploads and their warm-up. Pooled
//! sockets that may have crossed a sleep or a long idle are never reused.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Pooled sockets retire before the shortest measured server idle close that
/// is worth keeping (Meta ~70 s; xAI ~200 s; OpenAI, OpenRouter, ElevenLabs
/// and Google longer). Deepgram's 5 s is `batch::closes_idle_quickly`.
pub(super) const POOL_IDLE: Duration = Duration::from_secs(55);
/// Wall time running this far ahead of `Instant`, which stops while the Mac
/// sleeps, means pooled sockets crossed a sleep and only look young.
const SLEEP_SLACK: Duration = Duration::from_secs(5);

/// One agent for every recorded upload, so retries, fallbacks to the same host
/// and consecutive dictations reuse a pooled TLS connection. Closed idle
/// connections are probed and replaced before use.
pub(super) static AGENT: LazyLock<HttpPool> = LazyLock::new(HttpPool::new);

pub(super) struct HttpPool {
    state: Mutex<PoolState>,
}

struct PoolState {
    agent: ureq::Agent,
    /// Bumped whenever `agent` (and with it every pooled socket) is replaced.
    generation: u64,
    /// Wall and monotonic time of the last request that returned a socket.
    last_use: Option<(SystemTime, Instant)>,
    /// Origins (scheme and authority) that returned a socket to this agent's
    /// pool. A request to any other origin necessarily opens a new connection.
    pooled: HashSet<String>,
}

/// The agent one request uses, and whether it may be handed a pooled socket.
pub(super) struct Checkout {
    pub(super) agent: ureq::Agent,
    generation: u64,
    pub(super) pooled: bool,
}

fn pool_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .max_idle_age(POOL_IDLE)
            .build(),
    )
}

impl HttpPool {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(PoolState {
                agent: pool_agent(),
                generation: 0,
                last_use: None,
                pooled: Default::default(),
            }),
        }
    }

    /// The agent for a request to `origin`. When the pool is suspect, the whole
    /// agent is replaced: pooled sockets are per host while the sleep evidence
    /// is global, so evicting only this origin would leave stale sockets to
    /// other hosts looking young. Requests in flight keep the old agent and
    /// return their sockets to its discarded pool.
    pub(super) fn checkout(&self, origin: Option<&str>) -> Checkout {
        self.checkout_at(origin, SystemTime::now(), Instant::now())
    }

    pub(super) fn checkout_at(
        &self,
        origin: Option<&str>,
        wall: SystemTime,
        now: Instant,
    ) -> Checkout {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if pool_suspect(state.last_use, wall, now) {
            state.agent = pool_agent();
            state.generation = state.generation.wrapping_add(1);
            state.last_use = None;
            state.pooled.clear();
        }
        Checkout {
            agent: state.agent.clone(),
            generation: state.generation,
            pooled: origin.is_some_and(|origin| state.pooled.contains(origin)),
        }
    }

    /// Records that a request through `checkout` returned its socket to the pool.
    pub(super) fn used(&self, checkout: &Checkout, origin: Option<&str>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != checkout.generation {
            return;
        }
        state.last_use = Some((SystemTime::now(), Instant::now()));
        if let Some(origin) = origin {
            state.pooled.insert(origin.to_owned());
        }
    }
}

/// The pool key ureq uses for `url`: scheme and authority.
pub(super) fn origin(url: &str) -> Option<String> {
    let uri = url.parse::<ureq::http::Uri>().ok()?;
    Some(format!("{}://{}", uri.scheme_str()?, uri.authority()?))
}

/// Whether every pooled socket may be dead although its `Instant` age is
/// young: the Mac slept since the last use, or the wall clock says it is old.
/// A clock set backwards is treated the same; a fresh socket only costs TLS.
fn pool_suspect(last: Option<(SystemTime, Instant)>, wall: SystemTime, now: Instant) -> bool {
    let Some((last_wall, last_now)) = last else {
        return false;
    };
    let Ok(wall_elapsed) = wall.duration_since(last_wall) else {
        return true;
    };
    wall_elapsed >= POOL_IDLE
        || wall_elapsed
            > now
                .saturating_duration_since(last_now)
                .saturating_add(SLEEP_SLACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pooled_sockets_are_bypassed_after_sleep_or_long_idle() {
        let wall = SystemTime::now();
        let now = Instant::now();
        assert!(!pool_suspect(None, wall, now));
        let recent = Some((wall, now));
        assert!(!pool_suspect(
            recent,
            wall + Duration::from_secs(20),
            now + Duration::from_secs(20)
        ));
        // Instant stood still across a 10-minute sleep.
        assert!(pool_suspect(
            recent,
            wall + Duration::from_secs(600),
            now + Duration::from_secs(1)
        ));
        assert!(pool_suspect(
            recent,
            wall + Duration::from_secs(20),
            now + Duration::from_secs(2)
        ));
        assert!(pool_suspect(recent, wall + POOL_IDLE, now + POOL_IDLE));
        // A clock set backwards cannot vouch for the socket either.
        assert!(pool_suspect(recent, wall - Duration::from_secs(1), now));
    }
}
