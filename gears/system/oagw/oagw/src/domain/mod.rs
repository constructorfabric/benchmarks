//! Domain layer ([DESIGN.md](../../docs/DESIGN.md) `cpt-cf-oagw-design-layers`).
//!
//! Pure business model and vocabulary: the schema-mirroring [`model`] types, the
//! [`alias`] enforcement rules, the [`error`] type that maps onto
//! `CanonicalError` / RFC 9457 `Problem`, the [`merger`] that layers the
//! traffic-policy configuration, and the [`service`] that owns the management
//! use cases. This layer depends on no transport type.

pub mod alias;
pub mod error;
pub mod merger;
pub mod model;
pub mod proxy;
pub mod service;
