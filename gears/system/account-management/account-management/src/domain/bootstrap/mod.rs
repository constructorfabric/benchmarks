//! Platform-bootstrap saga domain layer.
//!
//! Implements FEATURE `platform-bootstrap` (see
//! `gears/system/account-management/docs/features/feature-platform-bootstrap.md`).
//!
//! The bootstrap saga is invoked exactly once per platform start from the
//! gear lifecycle entry path and **MUST** complete before the runtime
//! starts serving so that the retention + reaper background loops never
//! observe the platform without a root tenant.

pub mod config;
pub mod service;

pub use config::BootstrapConfig;
pub use service::BootstrapService;
