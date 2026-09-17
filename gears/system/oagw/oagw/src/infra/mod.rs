//! Infrastructure layer — storage, error mapping, the RFC 9457 envelope,
//! metrics registry, and audit logging.
//!
//! `infra` holds adapters and platform integrations (DoD
//! `cpt-cf-oagw-dod-gear-foundation-skeleton`; feature
//! `cpt-cf-oagw-feature-error-semantics`):
//!
//! - [`storage`] — in-memory DashMap repository implementations mirroring the
//!   §3.7 table shapes (constraint
//!   `cpt-cf-oagw-constraint-in-memory-storage`, DoD
//!   `cpt-cf-oagw-dod-domain-model-repositories-repo-traits`);
//! - [`canonical_mapping`] — `From<DomainError> for CanonicalError`;
//! - [`error_envelope`] — the OAGW RFC 9457 `GatewayError` envelope with
//!   `X-OAGW-Error-Source` (DoD `cpt-cf-oagw-dod-error-semantics-envelope`);
//! - [`metrics`] — the DESIGN §4.2 metrics registry (feature
//!   `cpt-cf-oagw-feature-observability-audit`);
//! - [`audit`] — DESIGN §4.3 structured audit logging via `tracing`.

pub mod audit;
pub mod canonical_mapping;
pub mod error_envelope;
pub mod metrics;
pub mod plugin;
pub mod storage;

pub use audit::{AUDIT_TARGET, AuditEntry, config_change};
pub use error_envelope::{ERROR_SOURCE_HEADER, ErrorRequestContext, GatewayError};
pub use metrics::{MetricsRegistry, normalize_method};
pub use plugin::builtin_registries;
pub use storage::{InMemoryStore, UpstreamRepoOptions};
