//! Stable GTS identifiers the data plane resolves plugins under.
//!
//! The registries of `infra/plugin` key their instances by these ids; they are
//! constants rather than call-site literals so a spelling drift here is a
//! compile error and a drift against `ADR`-0008/`ADR`-0009 is caught by the
//! plugin tests.

use toolkit_gts::gts_id;

/// `NoopAuthPlugin` (`DESIGN` §3.2, built-in auth plugins).
pub const NOOP_AUTH_PLUGIN_ID: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");

/// `ApiKeyAuthPlugin`.
pub const API_KEY_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");

/// `OAuth2ClientCredAuthPlugin` with the `Form` client-auth method (`ADR`-0008).
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");

/// `OAuth2ClientCredAuthPlugin` with the `Basic` client-auth method
/// (`ADR`-0008).
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");

/// `RequiredHeadersGuardPlugin` (`ADR`-0009).
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");

/// `RequestIdTransformPlugin` (`DESIGN` §3.2, built-in transform plugins).
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");

/// Header the `RequestIdTransformPlugin` propagates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gts_helpers_tests.rs"]
mod tests;
