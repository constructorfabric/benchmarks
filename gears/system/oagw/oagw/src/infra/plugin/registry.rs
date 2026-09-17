//! Plugin registries (DESIGN.md §3.1 “Plugin Identification Model”).
//!
//! Plugins are identified by GTS identifiers in the API layer. Two families
//! exist:
//!
//! * **named** plugins — `gts.cf.core.oagw.{type}_plugin.v1~cf.core.oagw.{name}.v1`
//!   — resolved by these in-process registries and never stored in
//!   `oagw_plugin`;
//! * **custom** plugins — `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` — stored
//!   by the control plane and resolved through [`PluginRepository`].
//!
//! Catalog-only identifiers (`basic`/`bearer`, `timeout`/`cors`,
//! `logging`/`metrics`) are known to the types-registry but have no backing
//! implementation here, so the registries reject them.

use std::collections::BTreeSet;
use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::repo::PluginRepository;
use crate::infra::storage::InMemoryPluginRepo;

const AUTH_PLUGIN_TYPE: &str = "auth_plugin";
const GUARD_PLUGIN_TYPE: &str = "guard_plugin";
const TRANSFORM_PLUGIN_TYPE: &str = "transform_plugin";

/// Built-in named auth plugins.
const AUTH_BUILTINS: [&str; 4] = [
    "noop.v1",
    "apikey.v1",
    "oauth2_client_cred.v1",
    "oauth2_client_cred_basic.v1",
];
/// Built-in named guard plugins.
const GUARD_BUILTINS: [&str; 1] = ["required_headers.v1"];
/// Built-in named transform plugins.
const TRANSFORM_BUILTINS: [&str; 1] = ["request_id.v1"];

/// Catalog-only identifiers: registered with the types-registry, never
/// resolvable by a plugin registry.
const CATALOG_ONLY: [&str; 6] = [
    "basic.v1",
    "bearer.v1",
    "timeout.v1",
    "cors.v1",
    "logging.v1",
    "metrics.v1",
];

/// The kind of plugin a registry resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PluginKind {
    /// Credential injection; one per upstream.
    Auth,
    /// Request policy enforcement; many per upstream/route.
    Guard,
    /// Request/response mutation; many per upstream/route.
    Transform,
}

impl PluginKind {
    /// `{type}_plugin` segment of the GTS identifier.
    #[must_use]
    pub const fn type_segment(self) -> &'static str {
        match self {
            Self::Auth => AUTH_PLUGIN_TYPE,
            Self::Guard => GUARD_PLUGIN_TYPE,
            Self::Transform => TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// The GTS base type of the plugin schema (trailing `~` included).
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::Auth => "gts.cf.core.oagw.auth_plugin.v1~",
            Self::Guard => "gts.cf.core.oagw.guard_plugin.v1~",
            Self::Transform => "gts.cf.core.oagw.transform_plugin.v1~",
        }
    }
}

/// A plugin reference after resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPluginRef {
    /// A named built-in plugin, resolvable in-process.
    Named {
        /// Full GTS identifier of the plugin.
        plugin_ref: String,
    },
    /// A tenant-defined custom plugin stored in `oagw_plugin`.
    Custom {
        /// Full GTS identifier of the plugin.
        plugin_ref: String,
        /// The UUID carried in the identifier.
        uuid: Uuid,
    },
}

impl ResolvedPluginRef {
    /// The canonical identifier string persisted as `plugin_ref`.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Named { plugin_ref } | Self::Custom { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The UUID of a custom plugin, when the reference is UUID-backed.
    #[must_use]
    pub const fn uuid(&self) -> Option<Uuid> {
        match self {
            Self::Named { .. } => None,
            Self::Custom { uuid, .. } => Some(*uuid),
        }
    }
}

/// Registry of the named plugins of one [`PluginKind`].
///
/// Aliased below as `AuthPluginRegistry`, `GuardPluginRegistry` and
/// `TransformPluginRegistry` so each plugin family keeps its own type name.
#[derive(Debug, Clone)]
pub struct PluginRegistry {
    kind: PluginKind,
    builtins: Arc<BTreeSet<String>>,
}

/// Registry of named auth plugins.
pub type AuthPluginRegistry = PluginRegistry;
/// Registry of named guard plugins.
pub type GuardPluginRegistry = PluginRegistry;
/// Registry of named transform plugins.
pub type TransformPluginRegistry = PluginRegistry;

impl PluginRegistry {
    /// Creates an empty registry for `kind`.
    #[must_use]
    pub fn new(kind: PluginKind) -> Self {
        Self {
            kind,
            builtins: Arc::new(BTreeSet::new()),
        }
    }

    /// Creates a registry pre-populated with the built-in plugins of `kind`.
    #[must_use]
    pub fn with_builtins(kind: PluginKind) -> Self {
        let names: &[&str] = match kind {
            PluginKind::Auth => &AUTH_BUILTINS,
            PluginKind::Guard => &GUARD_BUILTINS,
            PluginKind::Transform => &TRANSFORM_BUILTINS,
        };
        let builtins = names
            .iter()
            .map(|name| format!("{}cf.core.oagw.{name}", kind.gts_type()))
            .collect();
        Self {
            kind,
            builtins: Arc::new(builtins),
        }
    }

    /// Registers an extra named plugin; used by gears that contribute plugins.
    pub fn register(&mut self, plugin_ref: String) {
        Arc::make_mut(&mut self.builtins).insert(plugin_ref);
    }

    /// The plugin family this registry resolves.
    #[must_use]
    pub const fn kind(&self) -> PluginKind {
        self.kind
    }

    /// Canonical GTS form of a binding item.
    ///
    /// Bare UUIDs are expanded into this registry's family; full GTS ids are
    /// passed through.
    #[must_use]
    pub fn canonical_ref(&self, item: &str) -> String {
        if item.starts_with(self.kind.gts_type()) {
            item.to_owned()
        } else if let Ok(uuid) = Uuid::parse_str(item) {
            format!("{}{uuid}", self.kind.gts_type())
        } else {
            item.to_owned()
        }
    }

    /// Whether `plugin_ref` is a named plugin of this registry.
    #[must_use]
    pub fn is_named(&self, plugin_ref: &str) -> bool {
        self.builtins.contains(plugin_ref)
    }

    /// Every named plugin of this registry, sorted.
    #[must_use]
    pub fn named(&self) -> Vec<String> {
        self.builtins.iter().cloned().collect()
    }

    /// Resolves a plugin reference against the named built-ins and, for
    /// UUID-backed references, the stored custom plugins.
    ///
    /// # Errors
    /// Returns [`ErrorKind::PluginNotFound`] when the reference is unknown,
    /// not of this registry's type, or catalog-only, and
    /// [`ErrorKind::ResourceNotFound`] when a UUID-backed reference has no
    /// stored plugin row.
    pub fn resolve(
        &self,
        item: &str,
        custom: &dyn PluginRepository,
        tenant_id: Uuid,
    ) -> Result<ResolvedPluginRef, DomainError> {
        let plugin_ref = self.canonical_ref(item);
        if let Some(uuid) = plugin_ref
            .strip_prefix(self.kind.gts_type())
            .and_then(|instance| Uuid::parse_str(instance).ok())
        {
            let stored = custom.find(tenant_id, uuid)?;
            return stored
                .filter(|plugin| plugin.spec.plugin_type == self.kind.type_segment())
                .map(|_| ResolvedPluginRef::Custom {
                    plugin_ref: plugin_ref.clone(),
                    uuid,
                })
                .ok_or_else(|| {
                    DomainError::new(
                        ErrorKind::ResourceNotFound,
                        format!("custom plugin {plugin_ref} does not exist"),
                    )
                });
        }
        if self.is_named(&plugin_ref) {
            return Ok(ResolvedPluginRef::Named { plugin_ref });
        }
        Err(self.unknown(&plugin_ref))
    }

    /// Whether the reference would be rejected by [`Self::resolve`].
    #[must_use]
    pub fn is_resolvable(&self, plugin_ref: &str) -> bool {
        self.resolve(plugin_ref, &InMemoryPluginRepo::new(), Uuid::nil())
            .is_ok()
    }

    fn unknown(&self, plugin_ref: &str) -> DomainError {
        if CATALOG_ONLY
            .iter()
            .any(|name| plugin_ref == format!("{}cf.core.oagw.{name}", self.kind.gts_type()))
        {
            return DomainError::new(
                ErrorKind::PluginNotFound,
                format!("`{plugin_ref}` is a catalog-only identifier with no backing plugin"),
            );
        }
        DomainError::new(
            ErrorKind::PluginNotFound,
            format!("unknown {} plugin `{plugin_ref}`", self.kind.type_segment()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> InMemoryPluginRepo {
        InMemoryPluginRepo::new()
    }

    #[test]
    fn builtins_are_resolvable_per_family() {
        let auth = AuthPluginRegistry::with_builtins(PluginKind::Auth);
        assert!(auth.is_named("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"));
        assert!(auth.is_named("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"));
        assert!(
            auth.is_named(
                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"
            )
        );
        let guard = GuardPluginRegistry::with_builtins(PluginKind::Guard);
        assert!(
            guard.is_named("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
        );
        let transform = TransformPluginRegistry::with_builtins(PluginKind::Transform);
        assert!(
            transform.is_named("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1")
        );
    }

    #[test]
    fn catalog_only_identifiers_are_rejected() {
        let auth = AuthPluginRegistry::with_builtins(PluginKind::Auth);
        let error = auth
            .resolve(
                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
                &repo(),
                Uuid::nil(),
            )
            .expect_err("basic.v1 is not backed");
        assert_eq!(error.kind, ErrorKind::PluginNotFound);
        assert!(error.detail.contains("catalog-only"), "{}", error.detail);

        let guard = GuardPluginRegistry::with_builtins(PluginKind::Guard);
        assert!(
            guard
                .resolve(
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
                    &repo(),
                    Uuid::nil()
                )
                .is_err()
        );
        let transform = TransformPluginRegistry::with_builtins(PluginKind::Transform);
        assert!(
            transform
                .resolve(
                    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
                    &repo(),
                    Uuid::nil()
                )
                .is_err()
        );
    }

    #[test]
    fn identifiers_of_the_wrong_family_are_rejected() {
        let auth = AuthPluginRegistry::with_builtins(PluginKind::Auth);
        let error = auth
            .resolve(
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                &repo(),
                Uuid::nil(),
            )
            .expect_err("a guard id is not an auth plugin");
        assert_eq!(error.kind, ErrorKind::PluginNotFound);
        assert!(
            error.detail.contains("unknown auth_plugin plugin"),
            "{}",
            error.detail
        );
    }

    #[test]
    fn uuid_backed_references_require_a_stored_plugin() {
        let auth = AuthPluginRegistry::with_builtins(PluginKind::Auth);
        let repo = repo();
        let id = Uuid::new_v4();
        let reference = format!("gts.cf.core.oagw.auth_plugin.v1~{id}");
        assert!(auth.resolve(&reference, &repo, Uuid::nil()).is_err());

        let tenant = Uuid::new_v4();
        let plugin = crate::domain::model::Plugin {
            id,
            tenant_id: tenant,
            spec: crate::domain::model::PluginSpec {
                plugin_type: "auth_plugin".to_owned(),
                ..crate::domain::model::PluginSpec::default()
            },
        };
        repo.insert(&plugin).expect("insert");
        let resolved = auth
            .resolve(&reference, &repo, tenant)
            .expect("custom plugin");
        assert_eq!(
            resolved,
            ResolvedPluginRef::Custom {
                plugin_ref: reference,
                uuid: id
            }
        );
    }

    #[test]
    fn extra_plugins_can_be_registered() {
        let mut transform = TransformPluginRegistry::with_builtins(PluginKind::Transform);
        transform
            .register("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1".to_owned());
        assert!(
            transform.is_resolvable("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1")
        );
        assert_eq!(transform.named().len(), 2);
        assert_eq!(transform.kind(), PluginKind::Transform);
    }
}
