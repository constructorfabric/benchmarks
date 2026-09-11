//! Plugin registries and the executable built-in plugins
//! (`cpt-cf-oagw-dod-plugin-registries`,
//! `cpt-cf-oagw-dod-builtin-plugin-behaviors`).
//!
//! `registry` holds the three registries and the full named GTS identifiers the
//! built-ins are registered under, `plan` resolves a binding set into the
//! deterministic execution plan, and the remaining modules are the six built-in
//! implementations — auth `noop`, `apikey` and the two OAuth2 client-credentials
//! variants, guard `required_headers` and transform `request_id`.

pub mod auth;
pub mod guard;
pub mod oauth2_client_cred_auth;
pub mod plan;
pub mod registry;
pub mod token_cache;
pub mod transform;
