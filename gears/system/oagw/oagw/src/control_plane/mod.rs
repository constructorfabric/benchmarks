//! Control-plane business logic of the management half.
//!
//! One module per CDSL routine, plus the service facade and the two seams the
//! data-plane features consume. No `axum`/`http` type appears here: the layer
//! takes and returns domain types, `serde_json::Value`, and `Uuid`.
//!
//! ## Layering
//!
//! - [`validation`] — `cpt-cf-oagw-algo-request-validate`
//! - [`alias_derive`] — `cpt-cf-oagw-algo-alias-derive`
//! - [`bind`] — `cpt-cf-oagw-algo-bind-create-tags`
//! - [`binding`] — `cpt-cf-oagw-algo-plugin-ref-resolve` and
//!   `cpt-cf-oagw-algo-binding-validate`
//! - [`scoping`] — `cpt-cf-oagw-algo-tenant-scope`
//! - [`odata`] — `cpt-cf-oagw-algo-odata-list`
//! - [`plugin_def`] — the plugin create body's validation and the wire phase
//!   vocabulary the three contracts declare
//! - [`replace`] — `cpt-cf-oagw-algo-put-replace-diff`
//! - [`match_uniqueness`] — `cpt-cf-oagw-algo-match-uniqueness`
//! - [`chain`] — `cpt-cf-oagw-algo-tenant-chain-walk`
//! - [`shadow`] — `cpt-cf-oagw-algo-alias-shadow-resolve`
//! - [`effective`] — `cpt-cf-oagw-algo-field-family-merge` and the
//!   `cpt-cf-oagw-flow-resolve-effective-config` entry point
//! - [`sharing`] — `cpt-cf-oagw-algo-sharing-mode-decision`
//! - [`cache`] — the Control Plane L1 configuration cache and the rate-limit
//!   deletion notification seam
//! - [`service`] — `ManagementService`, the only caller of the store

pub mod alias_derive;
pub mod bind;
pub mod binding;
pub mod cache;
pub mod chain;
pub mod effective;
pub mod match_uniqueness;
pub mod odata;
pub mod plugin_def;
pub mod replace;
pub mod scoping;
pub mod service;
pub mod shadow;
pub mod sharing;
pub mod validation;
