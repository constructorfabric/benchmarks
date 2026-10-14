//! The fixed timing values of the upload path (not configurable, FEATURE 0005
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-configuration` step `inst-cfg-fixed`) and
//! the injected timer that lets the waits run without real sleeping.

use std::time::Duration;

use async_trait::async_trait;

// @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-fixed
// the fixed values stay out of the configuration: 25 s indexing deadline, 250 ms doubling to 2 s
// in-request polling, 20 s background rounds with 250 ms doubling to 5 s, the 10-minute
// background limit, 4 set-ready attempts 1, 2 and 4 s apart, the 120 s stale placeholder and the
// 5 loser polls

/// The in-request indexing deadline, counted from the request start.
pub const INDEXING_DEADLINE: Duration = Duration::from_secs(25);
/// The first in-request indexing poll wait; it doubles up to [`INDEX_WAIT_MAX`].
pub const INDEX_WAIT_INITIAL: Duration = Duration::from_millis(250);
pub const INDEX_WAIT_MAX: Duration = Duration::from_secs(2);
/// One background polling round (a heartbeat opens each round).
pub const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
/// The overall background limit.
pub const BACKGROUND_LIMIT: Duration = Duration::from_secs(600);
/// The first background poll wait; it doubles up to [`BACKGROUND_WAIT_MAX`].
pub const BACKGROUND_WAIT_INITIAL: Duration = Duration::from_millis(250);
pub const BACKGROUND_WAIT_MAX: Duration = Duration::from_secs(5);
/// The pauses between the 4 attempts of the guarded `ready` write.
pub const SET_READY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// A placeholder `chat_vector_stores` row older than this is reclaimed.
pub const STALE_PLACEHOLDER: Duration = Duration::from_secs(120);
/// The loser path of the vector store protocol: at most this many polls, the first after
/// [`LOSER_INITIAL_WAIT`], each next wait doubled.
pub const LOSER_POLLS: u32 = 5;
pub const LOSER_INITIAL_WAIT: Duration = Duration::from_millis(200);
/// The minimum `upload_reaper.stale_after_secs` the configuration accepts.
pub const MIN_STALE_AFTER_SECS: u64 = 60;
// @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-fixed

// @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-heartbeat-bound
// the heartbeat interval is at most half the minimum stale threshold (checked at compile time)
const _: () = assert!(BACKGROUND_ROUND.as_secs() * 2 <= MIN_STALE_AFTER_SECS);
// @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-heartbeat-bound

/// Monotonic time and sleeping, injected so tests run the waits without real sleeping (tokio
/// paused time cannot be combined with the in-memory `SQLite` pool of the REST tests).
#[async_trait]
pub trait Timer: Send + Sync {
    /// The monotonic time elapsed since the origin of this timer.
    fn monotonic(&self) -> Duration;

    /// Wait for `duration` of this timer's time.
    async fn sleep(&self, duration: Duration);
}

/// The production timer over `tokio::time`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioTimer;

#[async_trait]
impl Timer for TokioTimer {
    fn monotonic(&self) -> Duration {
        static ORIGIN: std::sync::OnceLock<tokio::time::Instant> = std::sync::OnceLock::new();
        ORIGIN.get_or_init(tokio::time::Instant::now).elapsed()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
#[path = "timing_tests.rs"]
mod timing_tests;
