//! REST (management) API layer of the OAGW gear.
//!
//! The layer is split into three concerns, following the `types-registry`
//! gear: [`routes`] registers every operation on the shared router and its
//! `OpenAPI` documentation, [`handlers`] implement them, and [`dto`] declares
//! the wire documents they exchange.
//!
//! ## Tenancy
//!
//! Handlers scope every operation to the caller tenant taken from the
//! `SecurityContext` extension the gateway injects (`subject_tenant_id`,
//! DESIGN.md §3.3 "Tenant Scoping"); client-supplied tenant input is never
//! used for scoping. Ancestor-owned resources are invisible, which is why a
//! descendant addressing one gets `404` and never `403`.
//!
//! ## Module map
//!
//! - [`dto`] — wire documents, wire identifiers and the `OData` list parameters
//! - [`handlers`] — one function per operation
//! - [`routes`] — route registration, `OpenAPI` documentation and the
//!   `X-OAGW-Error-Source` response header
//! - [`ManagementService`] — the control-plane state the handlers read

// Validation failures are rich RFC 9457 problems (detail, field, extensions), so
// `GatewayError` is larger than clippy's `result_large_err` threshold and every
// API entry point that rejects input trips it. The error type is shared with the
// whole crate (see `crate::error`), so the transport layer accepts the size
// rather than boxing a problem it hands to the client whole.
#![allow(clippy::result_large_err)]

pub mod dto;
pub mod handlers;
pub mod routes;

use std::sync::Arc;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::ConfigService;
use crate::error::GatewayError;

// === RE-EXPORTS ===
pub use routes::register_routes;

/// The plugin families the gateway can execute (ADR 0002, "Plugin Types").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Injects outbound credentials: `gts.cf.core.oagw.auth_plugin.v1~*`.
    Auth,
    /// Validates requests and enforces policies: `…guard_plugin.v1~*`.
    Guard,
    /// Rewrites requests and responses: `…transform_plugin.v1~*`.
    Transform,
}

impl PluginKind {
    /// The wire spelling carried by the `plugin_type` member.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// The GTS base type of this plugin family (ADR 0002, "Plugin Types").
    #[must_use]
    pub const fn gts_base_type(self) -> &'static str {
        match self {
            Self::Auth => "gts.cf.core.oagw.auth_plugin.v1",
            Self::Guard => "gts.cf.core.oagw.guard_plugin.v1",
            Self::Transform => "gts.cf.core.oagw.transform_plugin.v1",
        }
    }

    /// Parses the `plugin_type` member of a plugin submission.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`GatewayError`] when `raw` is not one of the three
    /// plugin families.
    pub fn parse(raw: &str) -> Result<Self, GatewayError> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "auth" => Ok(Self::Auth),
            "guard" => Ok(Self::Guard),
            "transform" => Ok(Self::Transform),
            _ => Err(GatewayError::validation(
                format!(
                    "`{raw}` is not a valid plugin_type; expected one of `auth`, `guard`, \
                     `transform`"
                ),
                "plugin_type",
            )),
        }
    }
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A custom plugin stored by the management API (ADR 0002, Appendix A
/// "Definition").
///
/// Plugin definitions are immutable: the API offers no `PUT`, so an update is
/// performed by creating a new plugin and re-binding the references.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredPlugin {
    /// System-generated identifier; the instance part of the wire `id`.
    pub id: Uuid,

    /// Owning tenant.
    pub tenant_id: Uuid,

    /// Human-readable plugin name.
    pub name: String,

    /// Optional description.
    pub description: Option<String>,

    /// Plugin family (auth, guard or transform).
    pub kind: PluginKind,

    /// Lifecycle phases the plugin implements (`on_request`, `on_response`, …).
    pub phases: Vec<String>,

    /// JSON schema of the plugin configuration.
    pub config_schema: Option<Value>,

    /// Starlark source served by `GET /oagw/v1/plugins/{id}/source`.
    pub source_code: String,
}

impl StoredPlugin {
    /// The anonymous GTS identifier this plugin is exposed under
    /// (`gts.cf.core.oagw.<kind>_plugin.v1~{uuid}`, DESIGN.md §3.3).
    #[must_use]
    pub fn gts_id(&self) -> String {
        dto::gts_plugin_id(self.kind, self.id)
    }

    /// Whether this plugin is referenced by a submitted plugin chain or auth
    /// binding (`PluginRef` items carry either the GTS identifier or the bare
    /// UUID).
    #[must_use]
    pub fn is_referenced_by(&self, raw: &str) -> bool {
        raw == self.gts_id() || raw == self.id.to_string()
    }
}

/// In-memory registry of the custom plugins created through the management
/// API (ADR 0002, "External Plugins").
///
/// Upstream and route `plugins` chains reference these records by identifier,
/// so [`ConfigService`] keeps the configuration and this registry keeps the
/// definitions. Like [`OagwStore`](crate::domain::OagwStore) the registry is
/// interior-mutable and takes `&self`.
#[derive(Debug, Default)]
pub struct PluginRegistry {
    /// Stored plugins, keyed by id.
    plugins: DashMap<Uuid, StoredPlugin>,
}

impl PluginRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: DashMap::new(),
        }
    }

    /// Stores a plugin record under its own id.
    pub fn insert(&self, record: StoredPlugin) {
        self.plugins.insert(record.id, record);
    }

    /// Returns the plugin with `id`, whichever tenant owns it.
    #[must_use]
    pub fn get(&self, id: Uuid) -> Option<StoredPlugin> {
        self.plugins.get(&id).map(|entry| entry.clone())
    }

    /// Returns the plugin with `id` when `tenant_id` owns it.
    ///
    /// A plugin owned by another tenant (including an ancestor) is not
    /// visible, so the caller turns `None` into a `404`.
    #[must_use]
    pub fn find(&self, tenant_id: Uuid, id: Uuid) -> Option<StoredPlugin> {
        self.plugins
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .map(|entry| entry.clone())
    }

    /// Returns every plugin owned by `tenant_id`, ordered by name then id.
    #[must_use]
    pub fn list(&self, tenant_id: Uuid) -> Vec<StoredPlugin> {
        let mut records: Vec<StoredPlugin> = self
            .plugins
            .iter()
            .map(|entry| entry.value().clone())
            .filter(|record| record.tenant_id == tenant_id)
            .collect();
        records.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id))
        });

        records
    }

    /// Removes and returns the plugin with `id` when `tenant_id` owns it.
    ///
    /// The ownership check and the removal happen in one [`DashMap`] operation,
    /// so a concurrent submission cannot observe a half-deleted plugin.
    ///
    /// A `None` result means the plugin was absent or owned by another tenant,
    /// which the caller reports as a `404`.
    #[must_use]
    pub fn delete(&self, tenant_id: Uuid, id: Uuid) -> Option<StoredPlugin> {
        self.plugins
            .remove_if(&id, |_, record| record.tenant_id == tenant_id)
            .map(|(_, record)| record)
    }

    /// Number of stored plugins.
    #[must_use]
    pub fn count(&self) -> usize {
        self.plugins.len()
    }
}

/// The control-plane state the management REST API is served from.
///
/// It bundles the [`ConfigService`] — which validates and stores upstreams and
/// routes — with the custom-plugin registry this layer owns. A single
/// `Arc<ManagementService>` is shared by every handler, and the same
/// configuration service is what the data plane reads, so a configuration
/// change is visible to the proxy without a reload.
#[derive(Debug)]
pub struct ManagementService {
    /// Upstream and route configuration (validated and stored).
    config: Arc<ConfigService>,

    /// Custom plugin definitions.
    plugins: PluginRegistry,
}

impl ManagementService {
    /// Bundles a configuration service with an empty plugin registry.
    #[must_use]
    pub fn new(config: Arc<ConfigService>) -> Self {
        Self {
            config,
            plugins: PluginRegistry::new(),
        }
    }

    /// The upstream and route configuration service.
    #[must_use]
    pub fn config(&self) -> &ConfigService {
        &self.config
    }

    /// The custom-plugin registry.
    #[must_use]
    pub const fn plugins(&self) -> &PluginRegistry {
        &self.plugins
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::OagwConfig;

    const TENANT: Uuid = Uuid::from_u128(0x0A11);
    const OTHER: Uuid = Uuid::from_u128(0x0A12);

    fn record(id: Uuid, tenant_id: Uuid, name: &str, kind: PluginKind) -> StoredPlugin {
        StoredPlugin {
            id,
            tenant_id,
            name: name.to_owned(),
            description: None,
            kind,
            phases: Vec::new(),
            config_schema: None,
            source_code: "def on_request(ctx):\n    return ctx.next()\n".to_owned(),
        }
    }

    #[test]
    fn test_plugin_kind_wire_spelling_round_trips() {
        for kind in [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform] {
            assert_eq!(PluginKind::parse(kind.as_str()).unwrap(), kind);
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn test_plugin_kind_gts_base_types_match_adr_0002() {
        assert_eq!(
            PluginKind::Guard.gts_base_type(),
            "gts.cf.core.oagw.guard_plugin.v1"
        );
        assert_eq!(
            PluginKind::Auth.gts_base_type(),
            "gts.cf.core.oagw.auth_plugin.v1"
        );
        assert_eq!(
            PluginKind::Transform.gts_base_type(),
            "gts.cf.core.oagw.transform_plugin.v1"
        );
    }

    #[test]
    fn test_plugin_kind_parse_rejects_unknown_families() {
        let error = PluginKind::parse("middleware").unwrap_err();

        assert_eq!(error.status(), 400);
        assert_eq!(error.extensions().extra["field"], "plugin_type");
    }

    #[test]
    fn test_registry_is_tenant_scoped() {
        let registry = PluginRegistry::new();
        let id = Uuid::new_v4();
        registry.insert(record(id, TENANT, "redact_pii", PluginKind::Transform));

        assert!(registry.find(TENANT, id).is_some());
        assert!(
            registry.find(OTHER, id).is_none(),
            "foreign tenants see nothing"
        );
        assert_eq!(registry.list(TENANT).len(), 1);
        assert!(registry.list(OTHER).is_empty());
        assert_eq!(registry.count(), 1);
        assert!(registry.delete(OTHER, id).is_none());
        assert_eq!(registry.count(), 1);
        assert!(registry.delete(TENANT, id).is_some());
        assert_eq!(registry.count(), 0);
        assert!(registry.get(id).is_none());
    }

    #[test]
    fn test_registry_lists_plugins_ordered_by_name() {
        let registry = PluginRegistry::new();
        registry.insert(record(Uuid::new_v4(), TENANT, "zeta", PluginKind::Guard));
        registry.insert(record(Uuid::new_v4(), TENANT, "alpha", PluginKind::Guard));
        registry.insert(record(Uuid::new_v4(), OTHER, "alpha", PluginKind::Guard));

        let names: Vec<String> = registry
            .list(TENANT)
            .iter()
            .map(|plugin| plugin.name.clone())
            .collect();

        assert_eq!(names, ["alpha".to_owned(), "zeta".to_owned()]);
    }

    #[test]
    fn test_stored_plugin_matches_both_identifier_forms() {
        let plugin = record(Uuid::nil(), TENANT, "guard", PluginKind::Guard);
        let wire = plugin.gts_id();

        assert_eq!(
            wire,
            "gts.cf.core.oagw.guard_plugin.v1~00000000-0000-0000-0000-000000000000"
        );
        assert!(plugin.is_referenced_by(&wire));
        assert!(plugin.is_referenced_by(plugin.id.to_string().as_str()));
        assert!(!plugin.is_referenced_by("gts.cf.core.oagw.timeout.v1"));
    }

    #[test]
    fn test_management_service_exposes_its_state() {
        let service = ManagementService::new(Arc::new(ConfigService::new(OagwConfig::default())));

        assert_eq!(service.plugins().count(), 0);
        assert_eq!(service.config().store().upstream_count(), 0);
    }
}
