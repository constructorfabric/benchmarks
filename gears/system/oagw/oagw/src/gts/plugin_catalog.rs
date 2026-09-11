//! The twelve plugin instance identifiers of the built-in and catalog-only
//! catalogue.
//!
//! Realizes `cpt-cf-oagw-dod-builtin-catalogue`: the six identifiers a
//! registry backs, the six identifiers the types-registry reserves and no
//! registry backs, and the two questions any resolution asks of the table —
//! is this identifier catalog-only, and is it known at all. The table is the
//! one place the twelve identifiers are written, so a registry, a binding
//! validation, and a types-registry registration cannot disagree about what
//! the catalogue holds.

use toolkit_gts::gts_id;

use crate::gts::{AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE};

/// The four backed auth identifiers.
pub const AUTH_NOOP: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
/// The API-key auth plugin, backed.
pub const AUTH_APIKEY: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
/// The Form-authenticated OAuth2 Client Credentials plugin, backed.
pub const AUTH_OAUTH2_CLIENT_CRED: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
/// The Basic-authenticated OAuth2 Client Credentials plugin, backed.
pub const AUTH_OAUTH2_CLIENT_CRED_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");

/// The one backed guard identifier: the required-headers guard.
pub const GUARD_REQUIRED_HEADERS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");

/// The one backed transform identifier: the request-identifier transform.
pub const TRANSFORM_REQUEST_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");

/// The six backed identifiers, paired with the family each belongs to.
pub const BACKED: [(&str, crate::domain::plugin_contract::PluginFamily); 6] = [
    (AUTH_NOOP, crate::domain::plugin_contract::PluginFamily::Auth),
    (AUTH_APIKEY, crate::domain::plugin_contract::PluginFamily::Auth),
    (
        AUTH_OAUTH2_CLIENT_CRED,
        crate::domain::plugin_contract::PluginFamily::Auth,
    ),
    (
        AUTH_OAUTH2_CLIENT_CRED_BASIC,
        crate::domain::plugin_contract::PluginFamily::Auth,
    ),
    (
        GUARD_REQUIRED_HEADERS,
        crate::domain::plugin_contract::PluginFamily::Guard,
    ),
    (
        TRANSFORM_REQUEST_ID,
        crate::domain::plugin_contract::PluginFamily::Transform,
    ),
];

/// The two catalog-only auth identifiers: reserved GTS identifiers with no
/// backing `AuthPlugin` implementation anywhere.
pub const CATALOG_ONLY_AUTH_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
/// `bearer`, catalog-only.
pub const CATALOG_ONLY_AUTH_BEARER: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");
/// `timeout`, catalog-only: a core data-plane behaviour, not a guard.
pub const CATALOG_ONLY_GUARD_TIMEOUT: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");
/// `cors`, catalog-only: a dedicated field on the aggregates, not a guard.
pub const CATALOG_ONLY_GUARD_CORS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");
/// `logging`, catalog-only: core data-plane instrumentation, not a transform.
pub const CATALOG_ONLY_TRANSFORM_LOGGING: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// `metrics`, catalog-only: core data-plane instrumentation, not a transform.
pub const CATALOG_ONLY_TRANSFORM_METRICS: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

/// The six catalog-only identifiers.
pub const CATALOG_ONLY: [&str; 6] = [
    CATALOG_ONLY_AUTH_BASIC,
    CATALOG_ONLY_AUTH_BEARER,
    CATALOG_ONLY_GUARD_TIMEOUT,
    CATALOG_ONLY_GUARD_CORS,
    CATALOG_ONLY_TRANSFORM_LOGGING,
    CATALOG_ONLY_TRANSFORM_METRICS,
];

/// All twelve identifiers of the catalogue: the six backed ones and the six
/// catalog-only ones.
#[must_use]
pub fn all() -> Vec<&'static str> {
    BACKED
        .iter()
        .map(|(identifier, _)| *identifier)
        .chain(CATALOG_ONLY)
        .collect()
}

/// Whether one identifier is one of the six catalog-only identifiers.
#[must_use]
pub fn is_catalog_only(identifier: &str) -> bool {
    CATALOG_ONLY.contains(&identifier)
}

/// Whether the catalogue names one identifier at all, backed or not.
#[must_use]
pub fn is_known_identifier(identifier: &str) -> bool {
    BACKED.iter().any(|(known, _)| *known == identifier) || is_catalog_only(identifier)
}

/// The family one known identifier belongs to, or `None` for an identifier the
/// catalogue does not name.
#[must_use]
pub fn family_of(identifier: &str) -> Option<crate::domain::plugin_contract::PluginFamily> {
    BACKED
        .iter()
        .find(|(known, _)| *known == identifier)
        .map(|(_, family)| *family)
}

/// Whether one base type schema prefix names one of the three plugin base
/// types, and which.
#[must_use]
pub fn family_of_prefix(prefix: &str) -> Option<crate::domain::plugin_contract::PluginFamily> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-map
    // The family names exactly one of the three registries, so mapping the
    // base type schema to its family is what keeps an auth identifier out of
    // the guard and transform registries.
    match prefix {
        AUTH_PLUGIN_TYPE => Some(crate::domain::plugin_contract::PluginFamily::Auth),
        GUARD_PLUGIN_TYPE => Some(crate::domain::plugin_contract::PluginFamily::Guard),
        TRANSFORM_PLUGIN_TYPE => Some(crate::domain::plugin_contract::PluginFamily::Transform),
        _ => None,
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-map
}
