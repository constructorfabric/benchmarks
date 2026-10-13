//! Mini-chat gear: multi-tenant AI chat (see `docs/DESIGN.md`).
//!
//! Module tree (DESIGN section 3.2 "Component Model"; modules marked
//! *planned* are added by later slices):
//!
//! ```text
//! lib.rs
//! gear.rs                      gear declaration, init/start/stop, capabilities
//! config.rs                    gear configuration (Appendix B)
//! domain/
//!   error.rs                   DomainError + canonical REST mapping (ADR-0004)
//!   quota.rs                   credit arithmetic, quota periods, estimation
//!   authz.rs                   PEP helpers (planned)
//!   models_service.rs          Models API service (planned)
//!   ...                        chat, stream, attachment, quota services (planned)
//! api/
//!   rest/                      routes, dto, handlers, error, sse (planned)
//! infra/
//!   plugins/
//!     static_audit.rs          static-mini-chat-audit-plugin gear
//!     static_model_policy.rs   static-mini-chat-model-policy-plugin gear
//!   db/                        migrations, entities, repositories (planned)
//!   policy_gateway.rs          model-policy plugin resolution (planned)
//!   audit_gateway.rs           audit plugin resolution (planned)
//!   llm/, oagw_provisioning.rs, outbox.rs, workers (planned)
//! ```

// Force-link the gears mini-chat depends on so their `inventory`
// registrations reach the final binary. The example server is built without
// its own `oagw` feature (`--features mini-chat,...`), so mini-chat is the
// only crate that pulls `oagw` (and with it the resolvers and the
// types-registry) into the link; an unused dependency would otherwise be
// dropped together with its gear registration.
use authn_resolver as _;
use authz_resolver as _;
use oagw as _;
use types_registry as _;

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;
