//! Combined plugin registries and binding resolution (algorithm
//! `cpt-cf-oagw-algo-plugin-system-resolve-gts`).
//!
//! Resolution of a binding is deliberately split:
//!
//! 1. **Parse** the `plugin_ref` into type + instance segments (step
//!    `inst-ps-gts-parse`);
//! 2. **Catalog-only** identifiers resolve nowhere — `PluginNotFound` (step
//!    `inst-ps-gts-catalog-only`);
//! 3. **UUID-backed** bindings load the custom plugin from `oagw_plugin`
//!    (`plugin_uuid` must match the `plugin_ref` type segment) (step
//!    `inst-ps-gts-custom`);
//! 4. **Named** bindings resolve against the type-appropriate registry by
//!    `plugin_ref` (step `inst-ps-gts-registry`).
//!
//! Starlark execution of custom plugins is delivered with the sandbox runtime
//! (Starlark feature, p3); resolution/identification is fully performed here.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::entity::plugin::Plugin;
use crate::domain::error::DomainError;
use crate::domain::plugin::ids::{GtsPluginRef, INSTANCE_PREFIX};
use crate::domain::plugin::{
    AuthPlugin, AuthRegistry, GuardPlugin, GuardRegistry, TransformPlugin, TransformRegistry,
};
use crate::domain::repo::PluginRepository;

/// A resolved plugin instance for a binding.
#[derive(Debug)]
pub enum ResolvedPlugin {
    /// A registered built-in auth plugin.
    Auth(Arc<dyn AuthPlugin>),
    /// A registered built-in guard plugin.
    Guard(Arc<dyn GuardPlugin>),
    /// A registered built-in transform plugin.
    Transform(Arc<dyn TransformPlugin>),
    /// A UUID-backed custom plugin resolved from `oagw_plugin`; its Starlark
    /// execution is delivered with the sandbox runtime (p3).
    Custom { plugin_id: Uuid, name: String },
}

/// Failure resolving a plugin binding (steps
/// `inst-ps-gts-catalog-only`/`inst-ps-gts-custom`/`inst-ps-gts-notfound`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionError {
    /// The reference is not a valid GTS plugin identifier.
    #[error("invalid plugin reference: {detail}")]
    InvalidRef { plugin_ref: String, detail: String },
    /// The identifier is catalog-only or has no registered implementation.
    #[error("plugin not found: {plugin_ref}")]
    PluginNotFound { plugin_ref: String },
    /// A UUID-backed binding named a plugin whose type does not match the
    /// `plugin_ref` type segment (DoD
    /// `cpt-cf-oagw-dod-plugin-system-identification`).
    #[error("plugin type mismatch: {detail}")]
    TypeMismatch { plugin_ref: String, detail: String },
}

impl ResolutionError {
    /// Maps a resolution failure to the DESIGN error catalog (steps
    /// `inst-ps-gts-catalog-only`/`inst-ps-gts-notfound`): resolution failures
    /// are 503 `plugin.not_found`; malformed references and type mismatches
    /// are binding-validation 400s.
    #[must_use]
    pub fn to_domain_error(&self) -> DomainError {
        match self {
            Self::PluginNotFound { plugin_ref } => DomainError::PluginNotFound {
                detail: plugin_ref.clone(),
            },
            Self::InvalidRef { detail, .. } | Self::TypeMismatch { detail, .. } => {
                DomainError::validation(None, detail.clone())
            }
        }
    }
}

/// The three plugin registries, assembled.
///
/// - [`AuthRegistry`] — named auth plugins by GTS identifier;
/// - [`GuardRegistry`] — named guard plugins by GTS identifier;
/// - [`TransformRegistry`] — named transform plugins by GTS identifier.
///
/// Together they back algorithm `cpt-cf-oagw-algo-plugin-system-resolve-gts`.
#[derive(Debug, Default)]
pub struct PluginRegistries {
    pub auth: AuthRegistry,
    pub guards: GuardRegistry,
    pub transforms: TransformRegistry,
}

impl PluginRegistries {
    /// Creates an empty set of registries.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolves a complete binding — `plugin_ref` (+ optional `plugin_uuid`)
    /// to a concrete plugin instance.
    ///
    /// `custom` supplies UUID-backed custom-plugin lookup when the binding
    /// carries a `plugin_uuid`; when `None`, a UUID-backed binding fails with
    /// [`ResolutionError::PluginNotFound`].
    ///
    /// # Errors
    /// - [`ResolutionError::InvalidRef`] — malformed `plugin_ref`;
    /// - [`ResolutionError::PluginNotFound`] — catalog-only identifier, no
    ///   registered implementation, or an unresolvable `plugin_uuid`;
    /// - [`ResolutionError::TypeMismatch`] — `plugin_uuid` names a plugin
    ///   whose type does not match the `plugin_ref` type segment.
    pub async fn resolve_binding(
        &self,
        tenant_id: Uuid,
        plugin_ref: &str,
        plugin_uuid: Option<Uuid>,
        custom: Option<&dyn CustomPluginLookup>,
    ) -> Result<ResolvedPlugin, ResolutionError> {
        let parsed =
            GtsPluginRef::parse(plugin_ref).ok_or_else(|| ResolutionError::InvalidRef {
                plugin_ref: plugin_ref.to_owned(),
                detail: format!("'{plugin_ref}' is not a canonical GTS plugin identifier"),
            })?;

        // Catalog-only identifiers are never resolvable (step
        // `inst-ps-gts-catalog-only`).
        if parsed.is_catalog_only() {
            return Err(ResolutionError::PluginNotFound {
                plugin_ref: plugin_ref.to_owned(),
            });
        }

        // UUID-backed custom plugins: load from oagw_plugin and verify the
        // type segment matches (step `inst-ps-gts-custom`).
        if let Some(uuid) = plugin_uuid {
            return Self::resolve_custom(custom, tenant_id, uuid, &parsed).await;
        }

        // Named plugins resolve against the type-appropriate registry (step
        // `inst-ps-gts-registry`).
        match parsed.plugin_type {
            crate::domain::entity::PluginType::Auth => self
                .auth
                .resolve(plugin_ref)
                .map(ResolvedPlugin::Auth)
                .ok_or_else(|| ResolutionError::PluginNotFound {
                    plugin_ref: plugin_ref.to_owned(),
                }),
            crate::domain::entity::PluginType::Guard => self
                .guards
                .resolve(plugin_ref)
                .map(ResolvedPlugin::Guard)
                .ok_or_else(|| ResolutionError::PluginNotFound {
                    plugin_ref: plugin_ref.to_owned(),
                }),
            crate::domain::entity::PluginType::Transform => self
                .transforms
                .resolve(plugin_ref)
                .map(ResolvedPlugin::Transform)
                .ok_or_else(|| ResolutionError::PluginNotFound {
                    plugin_ref: plugin_ref.to_owned(),
                }),
        }
    }

    async fn resolve_custom(
        custom: Option<&dyn CustomPluginLookup>,
        tenant_id: Uuid,
        uuid: Uuid,
        parsed: &GtsPluginRef,
    ) -> Result<ResolvedPlugin, ResolutionError> {
        let repo = custom.ok_or_else(|| ResolutionError::PluginNotFound {
            plugin_ref: parsed.full.clone(),
        })?;
        let plugin = repo.get_custom(tenant_id, uuid).await.ok_or_else(|| {
            ResolutionError::PluginNotFound {
                plugin_ref: parsed.full.clone(),
            }
        })?;
        if plugin.plugin_type != parsed.plugin_type {
            return Err(ResolutionError::TypeMismatch {
                plugin_ref: parsed.full.clone(),
                detail: format!(
                    "plugin '{}' is a {:?} plugin but is bound as {}",
                    plugin.name, plugin.plugin_type, parsed.full
                ),
            });
        }
        // `plugin_ref` instance must be derived from the plugin's name —
        // `cf.core.oagw.<name>.v1` — mirroring the identification model.
        let expected_instance = format!("{INSTANCE_PREFIX}{}.v1", plugin.name);
        if parsed.instance != expected_instance {
            return Err(ResolutionError::TypeMismatch {
                plugin_ref: parsed.full.clone(),
                detail: format!(
                    "plugin_ref instance '{}' does not match custom plugin '{}' (expected '{}')",
                    parsed.instance, plugin.name, expected_instance
                ),
            });
        }
        Ok(ResolvedPlugin::Custom {
            plugin_id: plugin.id,
            name: plugin.name,
        })
    }
}

/// Narrow custom-plugin lookup used during binding resolution.
///
/// Blanket-implemented for every [`PluginRepository`], so the Data Plane can
/// pass its repository handle directly while unit tests supply a fake with a
/// single method.
#[async_trait]
pub trait CustomPluginLookup: Send + Sync {
    /// Loads a custom plugin by `(tenant_id, id)`.
    async fn get_custom(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin>;
}

#[async_trait]
impl<T: PluginRepository + ?Sized> CustomPluginLookup for T {
    async fn get_custom(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin> {
        PluginRepository::get(self, tenant_id, id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entity::plugin::PluginType;
    use crate::domain::plugin::ids::{BASIC_AUTH, NOOP_AUTH};
    use serde_json::json;

    #[derive(Default)]
    struct FakeCustomRepo {
        plugins: std::collections::HashMap<Uuid, Plugin>,
    }

    impl FakeCustomRepo {
        fn insert(&mut self, plugin: Plugin) {
            self.plugins.insert(plugin.id, plugin);
        }
    }

    #[async_trait]
    impl CustomPluginLookup for FakeCustomRepo {
        async fn get_custom(&self, _tenant_id: Uuid, id: Uuid) -> Option<Plugin> {
            self.plugins.get(&id).cloned()
        }
    }

    fn custom_plugin(id: Uuid, plugin_type: PluginType, name: &str) -> Plugin {
        Plugin {
            id,
            tenant_id: Uuid::from_u128(7),
            plugin_type,
            name: name.to_owned(),
            config_schema: json!({ "type": "object" }),
            source_code: "def on_request(ctx): return ctx.next()".to_owned(),
            ..Plugin::default()
        }
    }

    #[test]
    fn catalog_only_identifiers_resolve_nowhere() {
        let regs = PluginRegistries::new();
        // `basic` is catalog-only: nothing may resolve it (DoD
        // `cpt-cf-oagw-dod-plugin-system-catalog-only`).
        assert!(regs.auth.resolve(BASIC_AUTH).is_none());
        let parsed = GtsPluginRef::parse(BASIC_AUTH).unwrap();
        assert!(parsed.is_catalog_only());
    }

    #[tokio::test]
    async fn malformed_ref_is_invalid() {
        let regs = PluginRegistries::new();
        let err = regs
            .resolve_binding(Uuid::nil(), "not-a-gts-id", None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolutionError::InvalidRef { .. }));
        assert_eq!(err.to_domain_error().status(), 400);
    }

    #[tokio::test]
    async fn unresolvable_named_ref_is_plugin_not_found() {
        let regs = PluginRegistries::new();
        let err = regs
            .resolve_binding(Uuid::nil(), NOOP_AUTH, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolutionError::PluginNotFound { .. }));
        let de = err.to_domain_error();
        assert_eq!(de.status(), 503);
        assert_eq!(
            de.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
        );
    }

    #[tokio::test]
    async fn uuid_backed_custom_plugin_resolves_with_type_and_instance_check() {
        let mut repo = FakeCustomRepo::default();
        let id = Uuid::from_u128(99);
        repo.insert(custom_plugin(id, PluginType::Transform, "redact_pii"));

        // Wrong instance segment for the chosen name → type mismatch.
        let bad_ref = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.other_name.v1";
        let err = PluginRegistries::new()
            .resolve_binding(Uuid::from_u128(7), bad_ref, Some(id), Some(&repo))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ResolutionError::TypeMismatch { .. }),
            "{err:?}"
        );

        // Correct instance + matching type resolves as a custom plugin.
        let good_ref = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.redact_pii.v1";
        let r = PluginRegistries::new()
            .resolve_binding(Uuid::from_u128(7), good_ref, Some(id), Some(&repo))
            .await
            .unwrap();
        match r {
            ResolvedPlugin::Custom { plugin_id, name } => {
                assert_eq!(plugin_id, id);
                assert_eq!(name, "redact_pii");
            }
            other => panic!("expected custom, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn uuid_backed_binding_without_a_repo_is_not_found() {
        let err = PluginRegistries::new()
            .resolve_binding(
                Uuid::from_u128(7),
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.my_guard.v1",
                Some(Uuid::from_u128(1234)),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ResolutionError::PluginNotFound { .. }));
    }

    #[test]
    fn registries_default_to_empty() {
        let r = PluginRegistries::new();
        assert!(r.auth.is_empty());
        assert!(r.guards.is_empty());
        assert!(r.transforms.is_empty());
    }
}
