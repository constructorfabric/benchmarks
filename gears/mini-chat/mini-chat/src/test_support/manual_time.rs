//! A virtual clock and timer for the attachment tests: [`ManualTime`] is both the application
//! [`Clock`] and the injected [`Timer`] of the upload path, so waits and backoff run without real
//! sleeping and `updated_at` writes follow the virtual time.
//!
//! In auto mode every `sleep(d)` advances the virtual time by `d` at once; in manual mode a
//! sleeper parks until the test calls [`ManualTime::advance`]. Tokio paused time cannot be used
//! next to the in-memory `SQLite` pool (its timers fire while a query is in flight).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tokio::sync::{Notify, oneshot};

use crate::domain::attachments::timing::Timer;
use crate::domain::quota::Clock;

struct State {
    elapsed: Duration,
    auto: bool,
    sequence: u64,
    parked: BTreeMap<(Duration, u64), oneshot::Sender<()>>,
    requested: Vec<Duration>,
}

/// The virtual clock and timer.
pub struct ManualTime {
    base: OffsetDateTime,
    state: Mutex<State>,
    changed: Notify,
}

impl ManualTime {
    fn build(base: OffsetDateTime, auto: bool) -> Arc<Self> {
        Arc::new(Self {
            base,
            state: Mutex::new(State {
                elapsed: Duration::ZERO,
                auto,
                sequence: 0,
                parked: BTreeMap::new(),
                requested: Vec::new(),
            }),
            changed: Notify::new(),
        })
    }

    /// A timer whose sleeps advance the virtual time at once.
    pub fn auto(base: OffsetDateTime) -> Arc<Self> {
        Self::build(base, true)
    }

    /// A timer whose sleepers park until [`advance`](Self::advance).
    pub fn manual(base: OffsetDateTime) -> Arc<Self> {
        Self::build(base, false)
    }

    /// The virtual time elapsed since the start.
    pub fn elapsed(&self) -> Duration {
        self.state.lock().unwrap().elapsed
    }

    /// The durations passed to `sleep`, in call order.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.state.lock().unwrap().requested.clone()
    }

    /// The number of sleepers currently parked.
    pub fn parked(&self) -> usize {
        self.state.lock().unwrap().parked.len()
    }

    /// Wait (in real time, 5 s at most) until `count` sleepers are parked.
    pub async fn wait_for_parked(&self, count: usize) {
        let wait = async {
            loop {
                let notified = self.changed.notified();
                if self.parked() >= count {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the sleepers never parked");
    }

    /// Advance the virtual time by `by`, waking the parked sleepers that fall due in order.
    pub fn advance(&self, by: Duration) {
        let mut state = self.state.lock().unwrap();
        let target = state.elapsed + by;
        loop {
            let due = state
                .parked
                .keys()
                .next()
                .copied()
                .filter(|(wake_at, _)| *wake_at <= target);
            let Some(key) = due else { break };
            let sender = state.parked.remove(&key).expect("parked sleeper");
            state.elapsed = state.elapsed.max(key.0);
            if sender.send(()).is_err() {
                // the sleeper was dropped: nothing to wake
            }
        }
        state.elapsed = target;
    }
}

impl Clock for ManualTime {
    fn now(&self) -> OffsetDateTime {
        let elapsed = self.elapsed();
        let elapsed = time::Duration::try_from(elapsed).expect("elapsed fits");
        self.base + elapsed
    }
}

#[async_trait]
impl Timer for ManualTime {
    fn monotonic(&self) -> Duration {
        self.elapsed()
    }

    async fn sleep(&self, duration: Duration) {
        let parked = {
            let mut state = self.state.lock().unwrap();
            state.requested.push(duration);
            let started = state.elapsed;
            if state.auto || duration.is_zero() {
                state.elapsed = started + duration;
                None
            } else {
                state.sequence += 1;
                let key = (started + duration, state.sequence);
                let (sender, receiver) = oneshot::channel();
                state.parked.insert(key, sender);
                Some(receiver)
            }
        };
        match parked {
            Some(receiver) => {
                self.changed.notify_waiters();
                drop(receiver.await);
            }
            None => tokio::task::yield_now().await,
        }
    }
}
