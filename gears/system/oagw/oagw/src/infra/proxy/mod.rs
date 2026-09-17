//! Data-plane proxy implementation (DESIGN §3.5 "Proxy Request Flow").
//!
//! [`DataPlaneServiceImpl`] orchestrates the full proxy flow for an inbound
//! request: CORS (actual) validation → rate limiting (`min()` across the
//! hierarchy) → auth plugin → guards → transform(request) → outbound HTTP
//! call → transform(response/error), with ADR-0001 `X-OAGW-Target-Host`
//! endpoint selection, hop-by-hop header stripping and
//! `X-OAGW-Error-Source: upstream` on every forwarded response.

mod service;

pub use service::DataPlaneServiceImpl;
