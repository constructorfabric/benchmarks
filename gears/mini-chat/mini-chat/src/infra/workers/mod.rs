//! Leader-only background workers (no-op elector: every instance runs; CAS guards keep correctness).

pub mod orphan_watchdog;
pub mod upload_reaper;
