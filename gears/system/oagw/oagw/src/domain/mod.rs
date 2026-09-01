//! Domain layer of the OAGW gear: wire model, GTS identifiers, the typed error
//! taxonomy, the ADR-0001 audit payload, the OData query engine, the
//! control-plane validation rules shared by the control plane (slice 2) and the
//! data plane (slice 4), plus the ADR-0002 plugin system, the ADR-0003 rate
//! limiter and the ADR-0004 CORS decision logic.
//!
//! Slice 4 adds [`metrics`], the in-process Prometheus-text registry the data
//! plane records into and `/oagw/v1/metrics` renders.

pub mod audit;
pub mod cors;
pub mod error;
pub mod metrics;
pub mod model;
pub mod odata;
pub mod plugin;
pub mod rate_limit;
pub mod services;
pub mod validation;
