//! Plugin execution chain: wiring the Auth/Guard/Transform plugin chain
//! into the Data-Plane request path, plus the built-in plugins
//! (`DESIGN.md` §3.4).
//!
//! Implemented by DECOMPOSITION entry 2.9 (plugin-execution). This module
//! is distinct from `crate::api::rest::plugins`, which is the
//! Control-Plane Plugin Management REST API (entry 2.4).
//!
//! RF-001: `crate::proxy::engine` calls [`execute::execute_pre_call`]/
//! [`execute::execute_post_call`] directly, sharing the process-lifetime
//! [`runtime::chain_runtime`] handle across every request. Every other
//! submodule here (registries, binding resolution, chain assembly,
//! credential resolution, and the client-credentials token cache) is this
//! feature's directly-tested implementation that `execute` draws on.

/// Registry-resolvable vs. catalog-only built-in plugin registries
/// (`cpt-cf-oagw-algo-plugin-registry-init`).
pub(crate) mod registry;

/// Plugin binding resolution (`cpt-cf-oagw-algo-plugin-binding-resolve`).
pub(crate) mod binding;

/// Chain assembly and ordering (`cpt-cf-oagw-algo-plugin-chain-assemble`).
pub(crate) mod plan;

/// The `required_headers` guard (`cpt-cf-oagw-algo-plugin-guard-evaluate`).
pub(crate) mod guard;

/// The `request_id` transform (`cpt-cf-oagw-algo-plugin-transform-apply`).
pub(crate) mod transform;

/// Credential resolution via `cred_store`
/// (`cpt-cf-oagw-algo-plugin-cred-resolve`).
pub(crate) mod credential;

/// The client-credentials token cache
/// (`cpt-cf-oagw-state-plugin-token-cache-entry`).
pub(crate) mod token_cache;

/// Client-credentials token acquisition
/// (`cpt-cf-oagw-algo-plugin-token-acquire`).
pub(crate) mod oauth2;

/// Auth plugin invocation and credential injection
/// (`cpt-cf-oagw-algo-plugin-auth-invoke`).
pub(crate) mod auth;

/// Chain execution and short-circuit mapping
/// (`cpt-cf-oagw-algo-plugin-chain-execute`). RF-001: `crate::proxy::engine`
/// calls [`execute::execute_pre_call`]/[`execute::execute_post_call`]
/// directly -- the narrow `plugins::chain` adapters these superseded (which
/// only ever resolved identifiers and ran the zero-config `request_id`
/// transform, discarding every binding's `config` and never receiving an
/// auth binding at all) have been retired.
pub(crate) mod execute;

/// Process-lifetime plugin-chain runtime (RF-001): the shared registries,
/// client-credentials token cache, and `cred_store` client
/// `crate::proxy::engine`'s real call sites use, replacing the old narrow
/// adapters' per-call `Registries::init()`.
///
/// `pub` (not `pub(crate)`) *only* so this module's `test_support`
/// submodule -- itself entirely `#[cfg(feature = "test-utils")]`-gated, so
/// it does not exist in a real production build -- is reachable from an
/// external `tests/*.rs` integration-test crate, exactly the seam
/// `tests/plugin_execution.rs` uses to seed a real credential the
/// production router can resolve end to end. Every other item in this
/// module stays `pub(crate)`.
pub mod runtime;
