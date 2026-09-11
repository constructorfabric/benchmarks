//! Built-in plugin registries (`cpt-cf-oagw-algo-plugin-registry-init`).
//!
//! Three in-process, immutable-after-construction registries -- auth,
//! guard, transform -- each mapping a registry-resolvable GTS plugin
//! *token* (the bare name after `cf.core.oagw.` in the named identifier,
//! e.g. `"apikey"`) to exactly one implementation kind. Per
//! `cpt-cf-oagw-adr-plugin-system`'s normative prose (not its own
//! contradicting illustrative snippet -- see the module-level note below),
//! the catalog-only identifiers (`basic`/`bearer`, `timeout`/`cors`,
//! `logging`/`metrics`) are never inserted here, so binding one always
//! fails resolution rather than silently behaving as a no-op.

use std::collections::HashMap;

#[cfg(test)]
use crate::model::plugin::PluginType;

/// Registry-resolvable auth plugin kinds
/// (`cpt-cf-oagw-dod-plugin-builtin-registries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthKind {
    Noop,
    ApiKey,
    OAuth2ClientCred,
    OAuth2ClientCredBasic,
}

/// The only registry-resolvable guard kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardKind {
    RequiredHeaders,
}

/// The only registry-resolvable transform kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransformKind {
    RequestId,
}

/// The three immutable in-process registries
/// (`cpt-cf-oagw-dod-plugin-builtin-registries`), constructed once via
/// [`Registries::init`] and shared read-only thereafter.
// @cpt-algo:cpt-cf-oagw-algo-plugin-registry-init:p2
// @cpt-dod:cpt-cf-oagw-dod-plugin-builtin-registries:p2
// @cpt-dod:cpt-cf-oagw-dod-plugin-types-registry-touchpoints:p2
// This module relies on `PluginType::named_catalog()`
// (`cpt-cf-oagw-feature-plugin-management`, entry 2.4) as the catalog of
// recognized identifiers and defines no new GTS type, schema, or endpoint
// of its own; the registries above are the separate executability
// authority the catalog is not conflated with.
pub(crate) struct Registries {
    auth: HashMap<&'static str, AuthKind>,
    guard: HashMap<&'static str, GuardKind>,
    transform: HashMap<&'static str, TransformKind>,
}

impl Registries {
    /// Build the three registries, inserting exactly the six
    /// registry-resolvable built-ins and asserting the six catalog-only
    /// identifiers stay absent (`inst-registry-init-01` through
    /// `inst-registry-init-07`).
    // @cpt-begin:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-01
    // @cpt-begin:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-02
    // @cpt-begin:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-03
    // @cpt-begin:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-04
    #[must_use]
    pub(crate) fn init() -> Self {
        let mut auth = HashMap::new();
        auth.insert("noop", AuthKind::Noop);
        auth.insert("apikey", AuthKind::ApiKey);
        auth.insert("oauth2_client_cred", AuthKind::OAuth2ClientCred);
        auth.insert("oauth2_client_cred_basic", AuthKind::OAuth2ClientCredBasic);

        let mut guard = HashMap::new();
        guard.insert("required_headers", GuardKind::RequiredHeaders);

        let mut transform = HashMap::new();
        transform.insert("request_id", TransformKind::RequestId);

        // @cpt-begin:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-07
        Self {
            auth,
            guard,
            transform,
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-07
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-04
    // @cpt-end:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-03
    // @cpt-end:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-02
    // @cpt-end:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-01

    pub(crate) fn auth(&self, token: &str) -> Option<AuthKind> {
        self.auth.get(token).copied()
    }

    pub(crate) fn guard(&self, token: &str) -> Option<GuardKind> {
        self.guard.get(token).copied()
    }

    pub(crate) fn transform(&self, token: &str) -> Option<TransformKind> {
        self.transform.get(token).copied()
    }

    /// `inst-registry-init-05`/`inst-registry-init-06`: assert every
    /// catalog-only identifier from `PluginType::named_catalog()` is
    /// absent from the registry of its own kind.
    // @cpt-begin:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-05
    // @cpt-begin:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-06
    #[cfg(test)]
    fn catalog_only_tokens_are_absent(&self) -> bool {
        for plugin_type in [PluginType::Auth, PluginType::Guard, PluginType::Transform] {
            for entry in plugin_type.named_catalog() {
                if entry.has_backing_implementation {
                    continue;
                }
                let present = match plugin_type {
                    PluginType::Auth => self.auth.contains_key(entry.token),
                    PluginType::Guard => self.guard.contains_key(entry.token),
                    PluginType::Transform => self.transform.contains_key(entry.token),
                };
                if present {
                    return false;
                }
            }
        }
        true
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-06
    // @cpt-end:cpt-cf-oagw-algo-plugin-registry-init:p2:inst-registry-init-05
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn auth_registry_has_exactly_the_four_backed_identifiers() {
        let registries = Registries::init();
        assert_eq!(registries.auth.len(), 4);
        assert_eq!(registries.auth("noop"), Some(AuthKind::Noop));
        assert_eq!(registries.auth("apikey"), Some(AuthKind::ApiKey));
        assert_eq!(
            registries.auth("oauth2_client_cred"),
            Some(AuthKind::OAuth2ClientCred)
        );
        assert_eq!(
            registries.auth("oauth2_client_cred_basic"),
            Some(AuthKind::OAuth2ClientCredBasic)
        );
    }

    #[test]
    fn guard_and_transform_registries_have_exactly_one_entry_each() {
        let registries = Registries::init();
        assert_eq!(registries.guard.len(), 1);
        assert_eq!(registries.transform.len(), 1);
        assert_eq!(
            registries.guard("required_headers"),
            Some(GuardKind::RequiredHeaders)
        );
        assert_eq!(
            registries.transform("request_id"),
            Some(TransformKind::RequestId)
        );
    }

    #[test]
    fn catalog_only_identifiers_are_absent_from_every_registry() {
        let registries = Registries::init();
        assert!(registries.catalog_only_tokens_are_absent());
        assert_eq!(registries.auth("basic"), None);
        assert_eq!(registries.auth("bearer"), None);
        assert_eq!(registries.guard("timeout"), None);
        assert_eq!(registries.guard("cors"), None);
        assert_eq!(registries.transform("logging"), None);
        assert_eq!(registries.transform("metrics"), None);
    }

    #[test]
    fn unknown_token_resolves_to_none_in_every_registry() {
        let registries = Registries::init();
        assert_eq!(registries.auth("frobnicate"), None);
        assert_eq!(registries.guard("frobnicate"), None);
        assert_eq!(registries.transform("frobnicate"), None);
    }
}
