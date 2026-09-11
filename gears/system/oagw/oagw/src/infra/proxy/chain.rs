//! Plugin chain resolution and execution.
//!
//! Resolution follows `docs/DESIGN.md` §"Resolution Algorithm": a binding
//! whose instance part parses as a UUID is a tenant-defined plugin looked up
//! in `oagw_plugin`; anything else is a named built-in from the in-process
//! registry.
//!
//! Execution order is fixed — Auth, then Guards, then Transform(on_request) —
//! with upstream bindings always ahead of route bindings, which the Control
//! Plane has already arranged in the merged chain.

use std::sync::Arc;

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::gts_helpers;
use crate::domain::model::{PluginBinding, PluginKind, PluginsConfig};
use crate::domain::plugin::{
    ErrorContext, GuardDecision, GuardPlugin, RequestContext, ResponseContext, TransformPlugin,
};

use crate::infra::plugin::PluginRegistries;

/// A binding resolved to something executable.
#[derive(Clone)]
pub enum ResolvedPlugin {
    /// A registered guard.
    Guard(Arc<dyn GuardPlugin>),
    /// A registered transform.
    Transform(Arc<dyn TransformPlugin>),
    /// A stored custom (Starlark) definition.
    ///
    /// There is no Starlark runtime in this build, so the body is inert. The
    /// binding still resolves — the definition exists and is owned by the
    /// tenant — which keeps `PluginNotFound` meaning "no such plugin" rather
    /// than "no interpreter".
    Custom {
        /// Plugin identifier, for logging.
        plugin_ref: String,
        /// Which trait the definition claims to implement.
        kind: PluginKind,
    },
}

/// One entry of an executable chain.
pub struct ChainEntry {
    /// The binding this came from.
    pub binding: PluginBinding,
    /// What it resolved to.
    pub resolved: ResolvedPlugin,
}

impl std::fmt::Debug for ChainEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.resolved {
            ResolvedPlugin::Guard(_) => "guard",
            ResolvedPlugin::Transform(_) => "transform",
            ResolvedPlugin::Custom { .. } => "custom",
        };
        f.debug_struct("ChainEntry")
            .field("plugin_ref", &self.binding.plugin_ref)
            .field("kind", &kind)
            .finish()
    }
}

/// Resolve a merged chain into executable entries.
///
/// # Errors
///
/// `503 PluginNotFound` when a binding names nothing the gateway can resolve.
pub async fn resolve_chain(
    registries: &PluginRegistries,
    custom_lookup: &dyn CustomPluginLookup,
    chain: &PluginsConfig,
) -> DomainResult<Vec<ChainEntry>> {
    let mut entries = Vec::with_capacity(chain.items.len());
    for binding in &chain.items {
        let resolved = if let Some(uuid) = binding.plugin_uuid() {
            let plugin = custom_lookup.lookup(uuid).await?.ok_or_else(|| {
                DomainError::plugin_not_found(format!(
                    "plugin '{}' is not registered",
                    binding.plugin_ref
                ))
            })?;
            ResolvedPlugin::Custom {
                plugin_ref: binding.plugin_ref.clone(),
                kind: plugin.plugin_type,
            }
        } else if let Some(guard) = registries.guard.get(&binding.plugin_ref) {
            ResolvedPlugin::Guard(guard)
        } else if let Some(transform) = registries.transform.get(&binding.plugin_ref) {
            ResolvedPlugin::Transform(transform)
        } else {
            return Err(DomainError::plugin_not_found(format!(
                "no plugin is registered for '{}'",
                binding.plugin_ref
            )));
        };
        entries.push(ChainEntry {
            binding: binding.clone(),
            resolved,
        });
    }
    Ok(entries)
}

/// Look up a custom plugin definition by id. Implemented by the Control Plane.
#[async_trait::async_trait]
pub trait CustomPluginLookup: Send + Sync {
    /// Fetch the definition, if it exists.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn lookup(&self, id: uuid::Uuid) -> DomainResult<Option<crate::domain::model::Plugin>>;
}

/// Run the auth plugin named by `plugin_ref`.
///
/// # Errors
///
/// `503 PluginNotFound` for an unknown identifier — including the reserved
/// `basic.v1` / `bearer.v1` catalog entries, which have no implementation —
/// and whatever the plugin itself rejects with.
pub async fn run_auth(
    registries: &PluginRegistries,
    plugin_ref: &str,
    ctx: &mut RequestContext,
) -> DomainResult<()> {
    if gts_helpers::plugin_ref_uuid(plugin_ref).is_some() {
        tracing::warn!(
            target: "oagw.plugin",
            plugin_ref,
            "custom auth plugins are stored but not executed in this build"
        );
        return Ok(());
    }
    let plugin = registries.auth.get(plugin_ref).ok_or_else(|| {
        DomainError::plugin_not_found(format!("unknown auth plugin '{plugin_ref}'"))
    })?;
    plugin
        .authenticate(ctx)
        .await
        .map_err(|err| err.into_domain(plugin_ref))
}

/// Run every guard's request phase, then every transform's request phase.
///
/// # Errors
///
/// The first guard rejection, or a plugin's internal failure.
pub async fn run_request_phase(entries: &[ChainEntry], ctx: &mut RequestContext) -> DomainResult<()> {
    for entry in entries {
        if let ResolvedPlugin::Guard(guard) = &entry.resolved {
            ctx.config = entry.binding.config.clone();
            match guard
                .guard_request(ctx)
                .await
                .map_err(|err| err.into_domain(&entry.binding.plugin_ref))?
            {
                GuardDecision::Allow => {}
                GuardDecision::Reject(err) => return Err(err),
            }
        }
    }
    for entry in entries {
        match &entry.resolved {
            ResolvedPlugin::Transform(transform) => {
                ctx.config = entry.binding.config.clone();
                transform
                    .transform_request(ctx)
                    .await
                    .map_err(|err| err.into_domain(&entry.binding.plugin_ref))?;
            }
            ResolvedPlugin::Custom { plugin_ref, kind } if *kind == PluginKind::Transform => {
                tracing::debug!(
                    target: "oagw.plugin",
                    plugin_ref = %plugin_ref,
                    "custom transform plugin is stored but not executed in this build"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// Run every guard's response phase, then every transform's response phase.
///
/// # Errors
///
/// The first guard rejection, or a plugin's internal failure.
pub async fn run_response_phase(
    entries: &[ChainEntry],
    ctx: &mut ResponseContext,
) -> DomainResult<()> {
    for entry in entries {
        if let ResolvedPlugin::Guard(guard) = &entry.resolved {
            ctx.config = entry.binding.config.clone();
            match guard
                .guard_response(ctx)
                .await
                .map_err(|err| err.into_domain(&entry.binding.plugin_ref))?
            {
                GuardDecision::Allow => {}
                GuardDecision::Reject(err) => return Err(err),
            }
        }
    }
    for entry in entries {
        if let ResolvedPlugin::Transform(transform) = &entry.resolved {
            ctx.config = entry.binding.config.clone();
            transform
                .transform_response(ctx)
                .await
                .map_err(|err| err.into_domain(&entry.binding.plugin_ref))?;
        }
    }
    Ok(())
}

/// Give transforms a chance to rewrite a gateway error before it is rendered.
///
/// A transform that fails here is logged and skipped: the original error is
/// more useful to the caller than a failure to decorate it.
pub async fn run_error_phase(entries: &[ChainEntry], error: DomainError) -> DomainError {
    let mut ctx = ErrorContext {
        config: serde_json::Map::new(),
        error,
    };
    for entry in entries {
        if let ResolvedPlugin::Transform(transform) = &entry.resolved {
            ctx.config = entry.binding.config.clone();
            if let Err(err) = transform.transform_error(&mut ctx).await {
                tracing::warn!(
                    target: "oagw.plugin",
                    plugin_ref = %entry.binding.plugin_ref,
                    error = ?err,
                    "transform_error failed; keeping the original error"
                );
            }
        }
    }
    ctx.error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Plugin, PluginPhase};
    use crate::infra::plugin::TokenCacheConfig;
    use crate::infra::plugin::test_support::{mock_credstore, request_context, response_context};

    struct NoCustomPlugins;

    #[async_trait::async_trait]
    impl CustomPluginLookup for NoCustomPlugins {
        async fn lookup(&self, _id: uuid::Uuid) -> DomainResult<Option<Plugin>> {
            Ok(None)
        }
    }

    struct OneCustomPlugin(Plugin);

    #[async_trait::async_trait]
    impl CustomPluginLookup for OneCustomPlugin {
        async fn lookup(&self, id: uuid::Uuid) -> DomainResult<Option<Plugin>> {
            Ok((id == self.0.id).then(|| self.0.clone()))
        }
    }

    fn registries() -> PluginRegistries {
        PluginRegistries::with_builtins(mock_credstore(Vec::new()), TokenCacheConfig::default())
    }

    fn chain(refs: &[&str]) -> PluginsConfig {
        PluginsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            items: refs.iter().map(|r| PluginBinding::named(*r)).collect(),
        }
    }

    #[tokio::test]
    async fn named_bindings_resolve_to_their_registry_entries() {
        let entries = resolve_chain(
            &registries(),
            &NoCustomPlugins,
            &chain(&[
                gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
                gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
            ]),
        )
        .await
        .expect("resolves");
        assert!(matches!(entries[0].resolved, ResolvedPlugin::Guard(_)));
        assert!(matches!(entries[1].resolved, ResolvedPlugin::Transform(_)));
    }

    #[tokio::test]
    async fn an_unknown_binding_is_a_503() {
        let err = resolve_chain(
            &registries(),
            &NoCustomPlugins,
            &chain(&["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.nope.v1"]),
        )
        .await
        .expect_err("unknown");
        assert_eq!(err.status(), 503);
        assert_eq!(err.gts_type(), gts_helpers::errors::PLUGIN_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_uuid_binding_resolves_against_the_store() {
        let plugin = Plugin {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            name: "validator".to_owned(),
            description: None,
            plugin_type: PluginKind::Guard,
            phases: vec![PluginPhase::OnRequest],
            config_schema: serde_json::json!({}),
            source_code: "def on_request(ctx): return ctx.next()".to_owned(),
            last_used_at: None,
            gc_eligible_at: None,
        };
        let plugin_ref =
            gts_helpers::anonymous_id(gts_helpers::GUARD_PLUGIN_TYPE, plugin.id);
        let entries = resolve_chain(
            &registries(),
            &OneCustomPlugin(plugin),
            &chain(&[&plugin_ref]),
        )
        .await
        .expect("resolves");
        assert!(matches!(entries[0].resolved, ResolvedPlugin::Custom { .. }));
    }

    #[tokio::test]
    async fn a_dangling_uuid_binding_is_a_503() {
        let plugin_ref =
            gts_helpers::anonymous_id(gts_helpers::GUARD_PLUGIN_TYPE, uuid::Uuid::new_v4());
        let err = resolve_chain(&registries(), &NoCustomPlugins, &chain(&[&plugin_ref]))
            .await
            .expect_err("dangling");
        assert_eq!(err.status(), 503);
    }

    #[tokio::test]
    async fn reserved_auth_identifiers_have_no_implementation() {
        let mut ctx = request_context(serde_json::Map::new());
        for reserved in [
            gts_helpers::BASIC_AUTH_PLUGIN_ID,
            gts_helpers::BEARER_AUTH_PLUGIN_ID,
        ] {
            let err = run_auth(&registries(), reserved, &mut ctx)
                .await
                .expect_err("no implementation");
            assert_eq!(err.status(), 503);
            assert!(err.detail().contains("unknown auth plugin"));
        }
    }

    #[tokio::test]
    async fn guards_run_before_transforms_on_the_request_leg() {
        let entries = resolve_chain(
            &registries(),
            &NoCustomPlugins,
            &PluginsConfig {
                sharing: crate::domain::model::SharingMode::Private,
                items: vec![
                    PluginBinding::named(gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID),
                    PluginBinding {
                        plugin_ref: gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
                        config: serde_json::json!({ "required_request_headers": "x-request-id" })
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    },
                ],
            },
        )
        .await
        .expect("resolves");

        // The guard demands a header the transform would have minted — since
        // guards run first, the request is rejected.
        let mut ctx = request_context(serde_json::Map::new());
        let err = run_request_phase(&entries, &mut ctx)
            .await
            .expect_err("guard runs first");
        assert_eq!(err.status(), 400);
    }

    #[tokio::test]
    async fn response_guards_can_reject_the_upstream_reply() {
        let entries = resolve_chain(
            &registries(),
            &NoCustomPlugins,
            &PluginsConfig {
                sharing: crate::domain::model::SharingMode::Private,
                items: vec![PluginBinding {
                    plugin_ref: gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
                    config: serde_json::json!({ "required_response_headers": "content-type" })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                }],
            },
        )
        .await
        .expect("resolves");
        let mut ctx = response_context(serde_json::Map::new());
        let err = run_response_phase(&entries, &mut ctx)
            .await
            .expect_err("missing content-type");
        assert_eq!(err.status(), 502);
    }
}
