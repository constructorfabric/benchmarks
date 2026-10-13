//! The orphan turn watchdog (DESIGN "Orphan Turn Watchdog", B.9.1, ADR-0010).

mod elector;
mod model;
mod service;

pub use elector::{AlwaysLeader, LeaderElector};
pub use model::{OrphanCandidate, OrphanReserve};
pub use service::{OrphanRepository, OrphanWatchdog, OrphanWatchdogParts, ScanReport};

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
