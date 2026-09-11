//! Plugin reference resolution and in-use detection
//! (`cpt-cf-oagw-algo-plugin-ref-resolution`,
//! `cpt-cf-oagw-algo-plugin-in-use-detection`,
//! `cpt-cf-oagw-dod-plugin-identification`).
//!
//! [`resolve_plugin_ref`] distinguishes a UUID-backed custom plugin
//! identifier from a named built-in identifier and resolves each against the
//! correct store: the tenant's in-process [`TenantState::plugins`] map for
//! the former, an in-process [`NamedPluginRegistry`] for the latter.
//! `cpt-cf-oagw-feature-plugin-runtime` (feature 8) is the seam's only other
//! consumer and the only feature that installs a *populated*
//! `NamedPluginRegistry` (the built-in Auth/Guard/Transform plugins); this
//! feature ships [`EmptyNamedPluginRegistry`], which matches no name, since
//! no built-in plugin behavior is implemented here.

use uuid::Uuid;

use super::model::{Plugin, PluginType, Route, Upstream, parse_gts_plugin_ref};
use crate::state::TenantState;

/// A source of named (built-in) plugin identities, keyed by plugin type and
/// name (`cpt-cf-oagw-dod-plugin-identification`).
///
/// This is the seam `cpt-cf-oagw-feature-plugin-runtime` reuses: that
/// feature installs the concrete registry backed by the real built-in
/// `AuthPlugin`/`GuardPlugin`/`TransformPlugin` implementations
/// (`cpt-cf-oagw-adr-plugin-system`). This feature only defines the trait
/// boundary and calls it with [`EmptyNamedPluginRegistry`], since
/// implementing built-in plugin behavior is out of this feature's scope.
pub trait NamedPluginRegistry {
    /// `true` when `name` is a registered named plugin of `plugin_type`.
    fn is_registered(&self, plugin_type: PluginType, name: &str) -> bool;
}

/// The empty named-plugin registry: matches no name
/// (`cpt-cf-oagw-dod-plugin-identification`). Used by this feature's own
/// handlers until `cpt-cf-oagw-feature-plugin-runtime` installs the real
/// built-in registry.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyNamedPluginRegistry;

impl NamedPluginRegistry for EmptyNamedPluginRegistry {
    fn is_registered(&self, _plugin_type: PluginType, _name: &str) -> bool {
        false
    }
}

/// A resolved plugin reference: either a stored custom plugin or a named
/// built-in plugin (`cpt-cf-oagw-algo-plugin-ref-resolution`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPlugin {
    /// A UUID-backed custom plugin stored in the tenant's control-plane
    /// state.
    Custom { id: Uuid, plugin_type: PluginType },
    /// A named built-in plugin, resolved via the in-process registry.
    Named {
        plugin_type: PluginType,
        name: String,
    },
}

/// Resolves a GTS plugin identifier
/// (`gts.cf.core.oagw.{type}_plugin.v1~{instance}`) against `tenant`'s
/// stored plugins and `registry`'s named plugins
/// (`cpt-cf-oagw-algo-plugin-ref-resolution`,
/// `cpt-cf-oagw-dod-plugin-identification`).
///
/// Returns `None` (a not-found result) when: the identifier fails to parse
/// or its base type is not one of `auth`, `guard`, `transform`; the instance
/// part parses as a UUID but no stored plugin has that id, or the stored
/// plugin's type does not match; or the instance part is not a UUID and is
/// not registered in `registry` for the parsed type.
// @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolution:p2:inst-plugin-refres-fn-01
#[must_use]
pub fn resolve_plugin_ref(
    gts_ref: &str,
    tenant: &TenantState,
    registry: &dyn NamedPluginRegistry,
) -> Option<ResolvedPlugin> {
    let (plugin_type, instance) = parse_gts_plugin_ref(gts_ref)?;

    if let Ok(id) = Uuid::parse_str(instance) {
        let stored = tenant.plugins.get(&id)?;
        if stored.plugin_type != plugin_type {
            return None;
        }
        return Some(ResolvedPlugin::Custom { id, plugin_type });
    }

    if registry.is_registered(plugin_type, instance) {
        Some(ResolvedPlugin::Named {
            plugin_type,
            name: instance.to_owned(),
        })
    } else {
        None
    }
}
// @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolution:p2:inst-plugin-refres-fn-01

/// Every upstream and route id in `tenant` whose plugin bindings reference
/// `plugin_gts_ref`: the upstream's `auth` reference and both entities'
/// `plugins.items` bindings (`cpt-cf-oagw-algo-plugin-in-use-detection`).
// @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-detection:p2:inst-plugin-inuse-fn-01
#[must_use]
pub fn plugin_references(tenant: &TenantState, plugin_gts_ref: &str) -> (Vec<Uuid>, Vec<Uuid>) {
    let upstream_ids = tenant
        .upstreams
        .iter()
        .filter(|entry| upstream_references_plugin(entry.value(), plugin_gts_ref))
        .map(|entry| *entry.key())
        .collect();
    let route_ids = tenant
        .routes
        .iter()
        .filter(|entry| route_references_plugin(entry.value(), plugin_gts_ref))
        .map(|entry| *entry.key())
        .collect();
    (upstream_ids, route_ids)
}
// @cpt-end:cpt-cf-oagw-algo-plugin-in-use-detection:p2:inst-plugin-inuse-fn-01

/// `true` when `upstream`'s `auth` plugin reference or `plugins.items`
/// bindings include `plugin_gts_ref`.
fn upstream_references_plugin(upstream: &Upstream, plugin_gts_ref: &str) -> bool {
    let auth_matches = upstream
        .auth
        .as_ref()
        .and_then(|auth| auth.auth_type.as_deref())
        .is_some_and(|auth_type| auth_type == plugin_gts_ref);
    let plugin_matches = upstream
        .plugins
        .as_ref()
        .is_some_and(|plugins| plugins.items.iter().any(|item| item == plugin_gts_ref));
    auth_matches || plugin_matches
}

/// `true` when `route`'s `plugins.items` bindings include `plugin_gts_ref`.
fn route_references_plugin(route: &Route, plugin_gts_ref: &str) -> bool {
    route
        .plugins
        .as_ref()
        .is_some_and(|plugins| plugins.items.iter().any(|item| item == plugin_gts_ref))
}

/// Projects a [`super::model::StoredPlugin`] to the API-facing [`Plugin`]
/// view, excluding `source_code`
/// (`cpt-cf-oagw-dod-plugin-get`, `cpt-cf-oagw-dod-plugin-get-source`).
#[must_use]
pub fn plugin_view(stored: &super::model::StoredPlugin) -> Plugin {
    Plugin {
        id: super::model::gts_plugin_id(stored.plugin_type, stored.id),
        kind: stored.plugin_type,
        name: stored.name.clone(),
        config_schema: stored.config_schema.clone(),
        phases: stored.phases.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{EmptyNamedPluginRegistry, ResolvedPlugin, plugin_references, resolve_plugin_ref};
    use crate::domain::model::{
        AuthConfig, Endpoint, PluginType, PluginsConfig, Protocol, Scheme, ServerConfig,
        StoredPlugin, Upstream, gts_plugin_id,
    };
    use crate::state::ControlPlaneState;
    use serde_json::json;
    use uuid::Uuid;

    fn sample_stored_plugin(plugin_type: PluginType) -> StoredPlugin {
        StoredPlugin {
            id: Uuid::new_v4(),
            plugin_type,
            name: "sample".to_owned(),
            config_schema: json!({"type": "object"}),
            phases: Vec::new(),
            source_code: "def on_request(ctx):\n    return ctx.next()".to_owned(),
        }
    }

    fn sample_upstream() -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            enabled: true,
            alias: "resolve.example.com".to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "resolve.example.com".to_owned(),
                    port: Some(443),
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolution:p2:inst-plugin-refres-uuid-test-01
    #[test]
    fn resolves_a_uuid_backed_custom_plugin_of_the_matching_type() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let tenant = state.tenant(tenant_id);
        let stored = sample_stored_plugin(PluginType::Guard);
        let id = stored.id;
        tenant.plugins.insert(id, stored);

        let gts_ref = gts_plugin_id(PluginType::Guard, id);
        let resolved =
            resolve_plugin_ref(&gts_ref, &tenant, &EmptyNamedPluginRegistry).expect("must resolve");
        assert_eq!(
            resolved,
            ResolvedPlugin::Custom {
                id,
                plugin_type: PluginType::Guard
            }
        );
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolution:p2:inst-plugin-refres-uuid-test-01

    #[test]
    fn fails_to_resolve_when_the_stored_plugin_type_does_not_match() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let tenant = state.tenant(tenant_id);
        let stored = sample_stored_plugin(PluginType::Guard);
        let id = stored.id;
        tenant.plugins.insert(id, stored);

        let mismatched_ref = gts_plugin_id(PluginType::Transform, id);
        assert!(resolve_plugin_ref(&mismatched_ref, &tenant, &EmptyNamedPluginRegistry).is_none());
    }

    #[test]
    fn fails_to_resolve_an_unknown_uuid() {
        let state = ControlPlaneState::new();
        let tenant = state.tenant(Uuid::new_v4());
        let gts_ref = gts_plugin_id(PluginType::Guard, Uuid::new_v4());
        assert!(resolve_plugin_ref(&gts_ref, &tenant, &EmptyNamedPluginRegistry).is_none());
    }

    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolution:p2:inst-plugin-refres-named-test-01
    #[test]
    fn fails_to_resolve_a_named_identifier_against_the_empty_registry() {
        let state = ControlPlaneState::new();
        let tenant = state.tenant(Uuid::new_v4());
        let gts_ref = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
        assert!(resolve_plugin_ref(gts_ref, &tenant, &EmptyNamedPluginRegistry).is_none());
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolution:p2:inst-plugin-refres-named-test-01

    #[test]
    fn fails_to_resolve_a_malformed_identifier() {
        let state = ControlPlaneState::new();
        let tenant = state.tenant(Uuid::new_v4());
        assert!(resolve_plugin_ref("not-a-gts-ref", &tenant, &EmptyNamedPluginRegistry).is_none());
    }

    // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-detection:p2:inst-plugin-inuse-auth-test-01
    #[test]
    fn plugin_references_finds_an_upstream_auth_binding() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let tenant = state.tenant(tenant_id);
        let stored = sample_stored_plugin(PluginType::Auth);
        let plugin_gts_ref = gts_plugin_id(stored.plugin_type, stored.id);
        tenant.plugins.insert(stored.id, stored);

        let mut upstream = sample_upstream();
        upstream.auth = Some(AuthConfig {
            auth_type: Some(plugin_gts_ref.clone()),
            sharing: crate::domain::model::Sharing::Private,
            config: None,
        });
        let upstream_id = upstream.id;
        tenant.upstreams.insert(upstream_id, upstream);

        let (upstream_ids, route_ids) = plugin_references(&tenant, &plugin_gts_ref);
        assert_eq!(upstream_ids, vec![upstream_id]);
        assert!(route_ids.is_empty());
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-detection:p2:inst-plugin-inuse-auth-test-01

    #[test]
    fn plugin_references_finds_an_upstream_guard_binding() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let tenant = state.tenant(tenant_id);
        let stored = sample_stored_plugin(PluginType::Guard);
        let plugin_gts_ref = gts_plugin_id(stored.plugin_type, stored.id);
        tenant.plugins.insert(stored.id, stored);

        let mut upstream = sample_upstream();
        upstream.plugins = Some(PluginsConfig {
            sharing: crate::domain::model::Sharing::Private,
            items: vec![plugin_gts_ref.clone().into()],
        });
        let upstream_id = upstream.id;
        tenant.upstreams.insert(upstream_id, upstream);

        let (upstream_ids, route_ids) = plugin_references(&tenant, &plugin_gts_ref);
        assert_eq!(upstream_ids, vec![upstream_id]);
        assert!(route_ids.is_empty());
    }

    #[test]
    fn plugin_references_returns_empty_lists_when_unreferenced() {
        let state = ControlPlaneState::new();
        let tenant = state.tenant(Uuid::new_v4());
        let (upstream_ids, route_ids) =
            plugin_references(&tenant, "gts.cf.core.oagw.guard_plugin.v1~unreferenced");
        assert!(upstream_ids.is_empty());
        assert!(route_ids.is_empty());
    }
}
