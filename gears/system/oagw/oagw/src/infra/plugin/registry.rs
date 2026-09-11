//! The builtin plugin registries (`cpt-cf-oagw-algo-plugin-catalog-register`).
//!
//! Each registry holds the builtin implementations of one plugin type, keyed by
//! the named instance the full GTS identifier maps to (`inst-pcrg-08`). A
//! registry is built once at gear init and read-only for the lifetime of the
//! process: no management operation mutates it, because builtin plugins are not
//! stored, not addressable and not subject to deletion (`inst-pcrg-10`).
//!
//! The implementations are typed *declarations* (`cpt-cf-oagw-dod-builtin-registries`,
//! `inst-full`): identity, type and declared phases. Entry 2.5 owns the
//! resolution of a registry entry into an executable instance and the execution
//! itself, so nothing here interprets a script, performs network I/O or builds a
//! sandbox.

// @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-09
// The registries live in `infra/plugin/` and the plugin trait definitions in
// `domain/plugin/`, per the layering of `cpt-cf-oagw-component-model`: the
// domain layer holds no registry dependency and names only the identifier sets
// the registries are built from.
// @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-09

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::domain::plugin::{
    AuthPlugin, GuardPlugin, Phase, PluginDeclaration, PluginType, TransformPlugin,
};
use crate::infra::plugin::PluginCatalogError;

// @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-07
// @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-08
/// Key a registry entry by: the named instance `cf.core.oagw.{name}.v1`, so the
/// mapping from the full GTS identifier
/// `gts.cf.core.oagw.{type}_plugin.v1~cf.core.oagw.{name}.v1` to the registry key
/// is the same one classification walks.
// @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-08
// @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-07
fn named_instance(name: &str) -> String {
    format!("cf.core.oagw.{name}.v1")
}

macro_rules! declare_plugin {
    ($(#[$meta:meta])* $struct_name:ident, $trait_name:ident, $base:expr, $name:expr, $ty:expr, $phases:expr) => {
        $(#[$meta])*
        #[derive(Debug)]
        struct $struct_name;

        impl PluginDeclaration for $struct_name {
            fn identifier(&self) -> &str {
                concat!($base, "~", "cf.core.oagw.", $name, ".v1")
            }

            fn plugin_type(&self) -> PluginType {
                $ty
            }

            fn name(&self) -> &str {
                $name
            }

            fn phases(&self) -> &'static [Phase] {
                $phases
            }
        }

        impl $trait_name for $struct_name {}
    };
}

declare_plugin!(
    /// Credential passthrough: injects no header and rejects nothing.
    NoopAuthPlugin,
    AuthPlugin,
    "gts.cf.core.oagw.auth_plugin.v1",
    "noop",
    PluginType::Auth,
    &[Phase::OnRequest]
);

declare_plugin!(
    /// Static API-key credential injection from the upstream `auth.config`.
    ApiKeyAuthPlugin,
    AuthPlugin,
    "gts.cf.core.oagw.auth_plugin.v1",
    "apikey",
    PluginType::Auth,
    &[Phase::OnRequest]
);

declare_plugin!(
    /// OAuth2 client-credentials grant with the `client_secret_post` form body.
    OAuth2ClientCredAuthPlugin,
    AuthPlugin,
    "gts.cf.core.oagw.auth_plugin.v1",
    "oauth2_client_cred",
    PluginType::Auth,
    &[Phase::OnRequest]
);

declare_plugin!(
    /// OAuth2 client-credentials grant with the `client_secret_basic` header.
    OAuth2ClientCredBasicAuthPlugin,
    AuthPlugin,
    "gts.cf.core.oagw.auth_plugin.v1",
    "oauth2_client_cred_basic",
    PluginType::Auth,
    &[Phase::OnRequest]
);

declare_plugin!(
    /// Required header enforcement on the request and the response.
    RequiredHeadersGuardPlugin,
    GuardPlugin,
    "gts.cf.core.oagw.guard_plugin.v1",
    "required_headers",
    PluginType::Guard,
    &[Phase::OnRequest, Phase::OnResponse]
);

declare_plugin!(
    /// `X-Request-ID` injection and propagation.
    RequestIdTransformPlugin,
    TransformPlugin,
    "gts.cf.core.oagw.transform_plugin.v1",
    "request_id",
    PluginType::Transform,
    &[Phase::OnRequest, Phase::OnResponse]
);

/// Registry of the builtin implementations of one plugin type.
///
/// Read-only for the lifetime of the process (`inst-pcrg-10`); construction
/// fails instead of silently keeping a duplicate, so host startup aborts on an
/// identifier collision (`inst-pcrg-11`, `inst-pcrg-12`).
pub struct PluginRegistry<P: ?Sized> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-10
    // Read-only for the lifetime of the process: built once at gear init, then
    // only read. No management operation mutates a registry, because builtin
    // plugins are not stored, not addressable and not subject to deletion.
    /// Named instance to implementation.
    entries: BTreeMap<String, Arc<P>>,
    // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-10
}

impl<P: ?Sized> PluginRegistry<P> {
    /// Build a registry over the given implementations, rejecting a duplicate
    /// identifier.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCatalogError::DuplicateIdentifier`] naming the offending
    /// identifier when two entries carry the same identifier.
    pub fn new(entries: Vec<Arc<P>>) -> Result<Self, PluginCatalogError>
    where
        P: PluginDeclaration,
    {
        let mut registry = Self {
            entries: BTreeMap::new(),
        };
        for entry in entries {
            let key = named_instance(entry.name());
            // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-12
            // A registry construction failure names the offending identifier, so
            // the gear init step that surfaces it aborts host startup instead of
            // serving a plugin contract whose catalog is incomplete.
            if registry.entries.contains_key(&key) {
                return Err(PluginCatalogError::DuplicateIdentifier {
                    identifier: entry.identifier().to_owned(),
                });
            }
            // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-12
            registry.entries.insert(key, entry);
        }
        Ok(registry)
    }

    /// The implementation registered under `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<P>> {
        self.entries.get(&named_instance(name)).map(Arc::clone)
    }

    /// The full GTS identifiers of the resolvable plugins, in name order.
    #[must_use]
    pub fn identifiers(&self) -> Vec<String>
    where
        P: PluginDeclaration,
    {
        self.entries
            .values()
            .map(|entry| entry.identifier().to_owned())
            .collect()
    }

    /// Whether the registry holds an entry named `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(&named_instance(name))
    }
}

/// The builtin auth plugin registry: `noop`, `apikey`, `oauth2_client_cred` and
/// the `oauth2_client_cred_basic` variant (`inst-pcrg-02`).
pub type AuthPluginRegistry = PluginRegistry<dyn AuthPlugin>;

/// The builtin guard plugin registry: `required_headers`, the only guard
/// identifier bindable through `plugins.items[].plugin_ref` (`inst-pcrg-03`).
pub type GuardPluginRegistry = PluginRegistry<dyn GuardPlugin>;

/// The builtin transform plugin registry: `request_id` (`inst-pcrg-04`).
pub type TransformPluginRegistry = PluginRegistry<dyn TransformPlugin>;

/// The builtin auth plugin set, in declaration order.
fn auth_builtins() -> Vec<Arc<dyn AuthPlugin>> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-02
    // The four resolvable auth plugins, the last two being the Form and Basic
    // client-auth-method variants of `OAuth2ClientCredAuthPlugin` that ADR 0008
    // registers in the same constructor.
    vec![
        Arc::new(NoopAuthPlugin),
        Arc::new(ApiKeyAuthPlugin),
        Arc::new(OAuth2ClientCredAuthPlugin),
        Arc::new(OAuth2ClientCredBasicAuthPlugin),
    ]
    // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-02
}

impl AuthPluginRegistry {
    /// The builtin auth plugins, `inst-pcrg-02`.
    ///
    /// `oauth2_client_cred` and `oauth2_client_cred_basic` are the Form and
    /// Basic client-auth-method variants of the same plugin, which ADR 0008
    /// registers in this constructor.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCatalogError::DuplicateIdentifier`] when a builtin
    /// identifier collides with one already present.
    pub fn with_builtins() -> Result<Self, PluginCatalogError> {
        Self::new(auth_builtins())
    }
}

impl GuardPluginRegistry {
    /// The builtin guard plugins, `inst-pcrg-03`: `required_headers`, the only
    /// guard identifier bindable through `plugins.items[].plugin_ref`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCatalogError::DuplicateIdentifier`] when a builtin
    /// identifier collides with one already present.
    pub fn with_builtins() -> Result<Self, PluginCatalogError> {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-03
        // `required_headers`, the only guard identifier bindable through
        // `plugins.items[].plugin_ref`.
        Self::new(vec![Arc::new(RequiredHeadersGuardPlugin)])
        // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-03
    }
}

impl TransformPluginRegistry {
    /// The builtin transform plugins, `inst-pcrg-04`: `request_id`, the
    /// `X-Request-ID` injection and propagation plugin.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCatalogError::DuplicateIdentifier`] when a builtin
    /// identifier collides with one already present.
    pub fn with_builtins() -> Result<Self, PluginCatalogError> {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-04
        // `request_id`, the `X-Request-ID` injection and propagation plugin.
        Self::new(vec![Arc::new(RequestIdTransformPlugin)])
        // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-04
    }
}
