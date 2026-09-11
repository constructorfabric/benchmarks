//! `UpstreamManagement` — the upstream-management aggregate of entry 2.2
//! (FEATURE `upstream-management`, flows `create`/`list`/`get`/`replace`/
//! `delete`/`enable-disable`).
//!
//! The service is the *only* writer of upstream records on the management
//! surface. It is async, unlike the 2.1 [`crate::domain::services::ControlPlaneService`],
//! because three of its steps reach across the process: the authorization
//! decision, the ancestor chain and the post-write notification. Everything
//! that crosses the process boundary is a port defined here and implemented
//! by [`crate::infra`], so the domain stays free of every SDK client and of
//! `toolkit-security` (the domain layer may depend on nothing outside itself).
//!
//! # Ordering of one write
//!
//! Every mutating operation ends in the same order (`inst-um-cr-18`,
//! `inst-um-rp-14`, `inst-um-dl-8`, `inst-um-en-14`): **store write → Control
//! Plane L1 invalidation → Data Plane hot-config flush → success**. The
//! invalidation and the flush are *invoked*, not implemented, here: they are
//! owned by `cpt-cf-oagw-feature-observability-and-operability`, which entry
//! 2.9 delivers by installing a [`ConfigWriteHook`] on the service.
//!
//! # The record the caller hands over
//!
//! The management write receives a [`crate::domain::dto::Upstream`] with three
//! conventions, because the REST body of a create and of a full replacement is
//! the same shape:
//!
//! * `id` left at the nil UUID means *server-generated*;
//! * `tenant_id` left at the nil UUID means *server-assigned from the actor*;
//! * an empty `alias` means *not supplied*, which is the derivation case.
// @cpt-flow:cpt-cf-oagw-flow-upstream-management-enable-disable:p1
// @cpt-state:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::alias::{self, AliasDerivationError};
use crate::domain::dto::Upstream;
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{PERM_BIND, UPSTREAM_BASE_TYPE};
use crate::domain::list_query::ListQuery;
use crate::domain::services::plugin_management::PluginConfigValidator;
use crate::domain::services::route_management::PluginBindingResolver;
use crate::domain::repo::{PluginBinding, UpstreamRecord as StoredUpstream, UpstreamRepository};
use crate::domain::validation::{AliasMode, validate_upstream_record};

// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-1
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-10
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-11
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-12
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-13
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-14
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-2
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-3
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-4
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-5
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-6
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-7
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-8
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-9
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-1
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-10
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-2
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-3
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-4
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-5
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-6
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-7
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-8
// @cpt-begin:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-9
/// The caller identity the management surface works with.
///
/// The REST layer derives it from the platform security context; the domain
//
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-9
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-8
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-7
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-6
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-5
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-4
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-3
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-2
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-14
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-13
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-12
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-11
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-10
// @cpt-end:cpt-cf-oagw-flow-upstream-management-enable-disable:p1:inst-um-en-1
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-9
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-8
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-7
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-6
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-5
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-4
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-3
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-2
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-10
// @cpt-end:cpt-cf-oagw-state-upstream-management-upstream-lifecycle:p1:inst-um-st-1
//
/// never names that type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Actor {
    /// The tenant every record is scoped to.
    pub tenant_id: Uuid,
    /// The principal the authorization decisions are evaluated for.
    pub principal_id: Uuid,
}

/// The outcome of an authorization gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizeError {
    /// The decision was `deny`: the canonical permission-denied surface at
    /// `403` (`inst-um-cr-15`).
    Denied { permission: String, detail: String },
    /// The decision could not be obtained.
    Unavailable { detail: String },
}

/// The errors the management surface returns.
#[derive(Debug, Clone, PartialEq)]
pub enum ManagementError {
    /// A domain rejection: a validation failure, a not-found, a conflict.
    Domain(DomainError),
    /// An authorization gate that did not produce an allow.
    Authorization(AuthorizeError),
}

impl ManagementError {
    /// A validation rejection naming `field`.
    #[must_use]
    pub fn validation(field: &str, reason: &str) -> Self {
        Self::Domain(DomainError::field_rejection(field, reason))
    }

    /// A not-found that never discloses whether a foreign record exists.
    #[must_use]
    pub fn not_found() -> Self {
        Self::Domain(DomainError::NotFound { resource_type: "upstream" })
    }

    /// The route not-found that never discloses whether the identifier is
    /// missing, foreign, ancestor-owned or already removed.
    #[must_use]
    pub fn route_not_found() -> Self {
        Self::Domain(DomainError::NotFound { resource_type: "route" })
    }

    /// The plugin not-found that never discloses whether the identifier is
    /// missing, foreign, ancestor-owned or already removed, and that a named
    /// plugin — which the in-process registry resolves — also produces.
    #[must_use]
    pub fn plugin_not_found() -> Self {
        Self::Domain(DomainError::NotFound { resource_type: "plugin" })
    }

    /// The match-rule uniqueness conflict of a route write
    /// (`inst-rm-create-11`, `inst-rm-replace-9b`).
    #[must_use]
    pub fn route_match_conflict() -> Self {
        Self::Domain(DomainError::Conflict {
            detail: "another enabled route of this upstream already matches this method, \
                     path prefix and priority"
                .to_owned(),
            referenced_by: None,
        })
    }

    /// The `(tenant_id, alias)` uniqueness conflict (`inst-um-cr-12`).
    #[must_use]
    pub fn alias_conflict() -> Self {
        Self::Domain(DomainError::Conflict {
            detail: "an upstream with this alias already exists in this tenant".to_owned(),
            referenced_by: None,
        })
    }

    /// Whether the error is a not-found.
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::Domain(error) if error.is_not_found())
    }
}

impl std::fmt::Display for ManagementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Domain(error) => write!(f, "{error}"),
            Self::Authorization(AuthorizeError::Denied { permission, .. }) => {
                write!(f, "permission denied: {permission}")
            }
            Self::Authorization(AuthorizeError::Unavailable { detail }) => {
                write!(f, "authorization unavailable: {detail}")
            }
        }
    }
}

impl From<DomainError> for ManagementError {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

/// The authorization port (`inst-um-cr-15`).
///
/// The check is *behind* this trait so a unit test can stub it; it is never
/// skipped. The permission identifiers are the DESIGN §3.2 per-operation
/// permissions, and the resource identifier is the anonymous GTS identifier of
/// the record the operation addresses.
#[async_trait]
pub trait ManagementAuthorizer: Send + Sync {
    /// Evaluate `permission` for `actor` over `resource_id`.
    ///
    /// # Errors
    ///
    /// Returns [`AuthorizeError::Denied`] when the decision is `deny` and
    /// [`AuthorizeError::Unavailable`] when no decision can be obtained.
    async fn authorize(
        &self,
        actor: &Actor,
        permission: &str,
        resource_id: &str,
    ) -> Result<(), AuthorizeError>;
}

/// The tenant-hierarchy port: the ancestor chain of a tenant, direct parent
/// first.
#[async_trait]
pub trait AncestorResolver: Send + Sync {
    /// The ancestors of `tenant_id`, ordered direct parent first.
    ///
    /// # Errors
    ///
    /// Returns the domain error the resolver produces; an unresolvable chain
    /// is an internal failure, never a silent empty chain.
    async fn ancestors(&self, actor: &Actor, tenant_id: Uuid) -> Result<Vec<Uuid>, DomainError>;
}

/// The structured audit event of DESIGN §4.3 a management write supplies to
/// its emitting owner (`cpt-cf-oagw-feature-observability-and-operability`).
///
/// A management feature only *supplies the outcome*: it names the event, the
/// caller it is attributed to, the resource identifier and the outcome, and
/// never renders or serializes the audit record itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWriteNotification {
    /// The event name, e.g. `route.create` / `route.replace` / `route.delete`.
    pub event: &'static str,
    /// The tenant the write is attributed to.
    pub tenant_id: Uuid,
    /// The principal the write is attributed to.
    pub principal_id: Uuid,
    /// The anonymous GTS resource identifier of the written record,
    /// `gts.cf.core.oagw.route.v1~{uuid}` for a route.
    pub resource_id: String,
    /// The owning upstream when the written record is a route, so the flush
    /// can derive the `route:{upstream_id}:{method}:{path_prefix}` key set.
    pub upstream_id: Option<Uuid>,
    /// The routing alias of a written **upstream** record, so the invalidation
    /// hook can derive the `upstream:{owner_tenant_id}:{alias}` key
    /// (`inst-os-algo-key-1`). `None` for every other record kind.
    pub upstream_alias: Option<String>,
    /// The match block of a written **route** record, so the hook can derive
    /// the `route:{upstream_id}:{method}:{path_prefix}` key set
    /// (`inst-os-algo-key-2`). `None` for every other record kind.
    pub route: Option<RouteWriteKeys>,
    /// The identifier of a written **plugin** record, naming the reserved
    /// `plugin:{plugin_id}` family (`inst-os-algo-key-3`). `None` for every
    /// other record kind.
    pub plugin_id: Option<Uuid>,
    /// The response status the write produced, which the `config_change`
    /// audit record populates its `status` field from
    /// (`inst-os-algo-audit-2b`).
    pub status: u16,
    /// The outcome the write produced, e.g. `accepted`.
    pub outcome: &'static str,
}

/// The match block of a written route record, in the form the cache key
/// derivation consumes (`inst-os-algo-key-2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteWriteKeys {
    /// The upstream the route is bound to.
    pub upstream_id: Uuid,
    /// The normalized route match pattern.
    pub path_prefix: String,
    /// One affected key per method of the match allowlist.
    pub methods: Vec<String>,
}

impl RouteWriteKeys {
    /// The match block of a written route, in the form the cache-key
    /// derivation consumes (`inst-os-algo-key-2`).
    #[must_use]
    pub fn of(route: &crate::domain::dto::Route) -> Self {
        match &route.match_.http {
            Some(http) => Self {
                upstream_id: route.upstream_id,
                path_prefix: http.path.clone(),
                methods: http.methods.iter().map(|method| method.as_str().to_owned()).collect(),
            },
            // `inst-os-algo-key-2`: a `grpc` match block derives no key.
            None => Self {
                upstream_id: route.upstream_id,
                path_prefix: String::new(),
                methods: Vec::new(),
            },
        }
    }
}

/// The post-write notification port.
///
/// `cpt-cf-oagw-feature-observability-and-operability` (entry 2.9) owns the
/// mechanism; this entry only *invokes* it, after the store write and before
/// the response (`inst-um-cr-18`).
#[async_trait]
pub trait ConfigWriteHook: Send + Sync {
    /// The Control Plane L1 invalidation followed by the Data Plane flush for
    /// one written record.
    ///
    /// The default delegates to this method so a hook installed before entry
    /// 2.9 keeps observing the upstream writes it registered for.
    ///
    /// # Errors
    ///
    /// Returns the error the hook produces; a failed flush fails the
    /// operation, because the data plane must not serve a stale record.
    async fn on_config_written(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<(), DomainError>;

    /// The Control Plane L1 invalidation, the Data Plane flush and the
    /// structured audit event of DESIGN §4.3 for one written **upstream**
    /// record (`inst-um-cr-18`, `inst-um-rp-14`, `inst-um-dl-8`).
    ///
    /// The default carries the notification to the pre-2.9 callback, so a hook
    /// that overrides only [`Self::on_config_written`] keeps working.
    ///
    /// # Errors
    ///
    /// Returns the error the hook produces; a failed flush fails the
    /// operation, because the data plane must not serve a stale record.
    async fn on_upstream_written(
        &self,
        notification: ConfigWriteNotification,
    ) -> Result<(), DomainError> {
        let upstream_id = notification.upstream_id.unwrap_or_default();
        self.on_config_written(notification.tenant_id, upstream_id).await
    }

    /// The Control Plane L1 invalidation, the Data Plane flush and the
    /// structured audit event of DESIGN §4.3 for one written **route** record
    /// (`inst-rm-create-14`, `inst-rm-replace-13`, `inst-rm-del-7`).
    ///
    /// # Errors
    ///
    /// Returns the error the hook produces; a failed flush fails the
    /// operation, because the data plane must not serve a stale record.
    async fn on_route_written(&self, notification: ConfigWriteNotification) -> Result<(), DomainError> {
        tracing::debug!(
            event = notification.event,
            tenant_id = %notification.tenant_id,
            resource_id = %notification.resource_id,
            outcome = notification.outcome,
            "route configuration written; CP L1 invalidation, the DP flush and the audit emitter are not installed yet"
        );
        Ok(())
    }

    /// The Control Plane L1 invalidation, the Data Plane flush and the
    /// structured audit event of DESIGN §4.3 for one written **plugin**
    /// definition (`inst-ps-create-1`, `inst-ps-del-1`).
    ///
    /// # Errors
    ///
    /// Returns the error the hook produces.
    async fn on_plugin_written(
        &self,
        notification: ConfigWriteNotification,
    ) -> Result<(), DomainError> {
        tracing::debug!(
            event = notification.event,
            tenant_id = %notification.tenant_id,
            resource_id = %notification.resource_id,
            outcome = notification.outcome,
            "plugin configuration written; CP L1 invalidation, the DP flush and the audit emitter are not installed yet"
        );
        Ok(())
    }
}

/// The hook installed until entry 2.9 fills the real one: it observes and
/// logs the write and lets the operation succeed.
#[derive(Debug, Default)]
pub struct NoopConfigWriteHook;

#[async_trait]
impl ConfigWriteHook for NoopConfigWriteHook {
    async fn on_config_written(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<(), DomainError> {
        tracing::debug!(
            tenant_id = %tenant_id,
            upstream_id = %upstream_id,
            "configuration written; CP L1 invalidation and DP flush are not installed yet"
        );
        Ok(())
    }
}

/// The effective enablement a record presents to a requesting tenant
/// (`cpt-cf-oagw-algo-upstream-management-enabled-inheritance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveEnablement {
    /// `false` when the record presents as disabled.
    pub enabled: bool,
    /// The tenant holding the disablement when the effective state is
    /// disabled: the owning tenant, or the closest disabled ancestor record's
    /// tenant (`disabled-by-ancestor`).
    pub disabling_tenant_id: Option<Uuid>,
}

impl EffectiveEnablement {
    /// The record's own flag, with no ancestor disablement.
    #[must_use]
    pub const fn own(enabled: bool) -> Self {
        Self { enabled, disabling_tenant_id: None }
    }
}

/// One upstream record together with the enablement it presents to the actor.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamView {
    /// The stored record.
    pub record: Upstream,
    /// The effective enablement (`inst-um-en-9`).
    pub effective: EffectiveEnablement,
}

/// The upstream-management operations of entry 2.2.
#[async_trait]
pub trait UpstreamManagement: Send + Sync {
    /// `POST /oagw/v1/upstreams` (`inst-um-cr-1` .. `-18`).
    ///
    /// # Errors
    ///
    /// Returns the validation, conflict, bind or authorization failure.
    async fn create(&self, actor: Actor, record: Upstream) -> Result<UpstreamView, ManagementError>;

    /// `GET /oagw/v1/upstreams` (`inst-um-ls-1` .. `-8`).
    ///
    /// # Errors
    ///
    /// Returns the query-interpretation or authorization failure.
    async fn list(&self, actor: Actor, query: &ListQuery) -> Result<Vec<Upstream>, ManagementError>;

    /// `GET /oagw/v1/upstreams/{id}` (`inst-um-gt-1` .. `-6`).
    ///
    /// # Errors
    ///
    /// Returns not-found for a foreign or unknown identifier.
    async fn get(&self, actor: Actor, id: Uuid) -> Result<UpstreamView, ManagementError>;

    /// `PUT /oagw/v1/upstreams/{id}` — the replace and the enable/disable
    /// flow (`inst-um-rp-1` .. `-14`, `inst-um-en-1` .. `-14`).
    ///
    /// # Errors
    ///
    /// Returns the validation, immutability, bind or authorization failure.
    async fn replace(
        &self,
        actor: Actor,
        id: Uuid,
        record: Upstream,
    ) -> Result<UpstreamView, ManagementError>;

    /// `DELETE /oagw/v1/upstreams/{id}` (`inst-um-dl-1` .. `-8`).
    ///
    /// # Errors
    ///
    /// Returns not-found for a foreign or unknown identifier.
    async fn delete(&self, actor: Actor, id: Uuid) -> Result<(), ManagementError>;

    /// The effective enablement of one record for the actor
    /// (`inst-um-ei-1` .. `-6`).
    ///
    /// # Errors
    ///
    /// Returns not-found for a foreign or unknown identifier.
    async fn effective_enablement(
        &self,
        actor: Actor,
        id: Uuid,
    ) -> Result<EffectiveEnablement, ManagementError>;
}

/// The concrete implementation over the 2.1 upstream repository and the three
/// ports.
pub struct UpstreamManagementService {
    upstreams: Arc<dyn UpstreamRepository>,
    allow_http_upstream: bool,
    authorizer: Arc<dyn ManagementAuthorizer>,
    ancestors: Arc<dyn AncestorResolver>,
    hook: RwLock<Arc<dyn ConfigWriteHook>>,
    /// The binding-time `auth.config` validator (`inst-ps-bind-14`/`-15`).
    plugin_configs: RwLock<Arc<dyn PluginConfigValidator>>,
    /// The binding-time `plugins.items[]` resolver (`inst-ps-bind-2` .. `-8`).
    plugin_bindings: RwLock<Arc<dyn PluginBindingResolver>>,
}

/// The no-op default of the binding-time validator, replaced at init.
struct NoPluginConfigValidator;

impl PluginConfigValidator for NoPluginConfigValidator {
    fn validate_auth_config(
        &self,
        _tenant_id: Uuid,
        _auth_ref: Option<&str>,
        _config: Option<&serde_json::Value>,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

/// The no-op default of the binding-time resolver, replaced at init: it
/// numbers the entries from zero and records a UUID only for a UUID-backed
/// reference, which is the storage shape the write path established before
/// entry 2.6 installed the catalog boundary.
struct NoPluginBindingResolver;

impl PluginBindingResolver for NoPluginBindingResolver {
    fn resolve(&self, _tenant_id: Uuid, references: &[String]) -> Result<Vec<PluginBinding>, DomainError> {
        Ok(crate::domain::services::management::bindings_of_references(references))
    }
}

impl UpstreamManagementService {
    /// Build the service over the repository and the three ports.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        allow_http_upstream: bool,
        authorizer: Arc<dyn ManagementAuthorizer>,
        ancestors: Arc<dyn AncestorResolver>,
    ) -> Self {
        Self {
            upstreams,
            allow_http_upstream,
            authorizer,
            ancestors,
            hook: RwLock::new(Arc::new(NoopConfigWriteHook)),
            plugin_configs: RwLock::new(Arc::new(NoPluginConfigValidator)),
            plugin_bindings: RwLock::new(Arc::new(NoPluginBindingResolver)),
        }
    }

    /// Install the binding-time `auth.config` validator of entry 2.6
    /// (`inst-ps-bind-14`/`-15`).
    pub fn set_plugin_config_validator(&self, validator: Arc<dyn PluginConfigValidator>) {
        *self.plugin_configs.write() = validator;
    }

    /// Install the binding-time `plugins.items[]` resolver of entry 2.6
    /// (`inst-ps-bind-2` .. `-8`).
    pub fn set_plugin_binding_resolver(&self, resolver: Arc<dyn PluginBindingResolver>) {
        *self.plugin_bindings.write() = resolver;
    }

    /// The binding-time `plugins.items[]` resolver, for a test to assert the
    /// wiring with.
    #[must_use]
    pub fn plugin_binding_resolver(&self) -> Arc<dyn PluginBindingResolver> {
        Arc::clone(&self.plugin_bindings.read().clone())
    }

    /// The `auth.config` validator, for a test to assert the wiring with.
    #[must_use]
    pub fn plugin_config_validator(&self) -> Arc<dyn PluginConfigValidator> {
        Arc::clone(&self.plugin_configs.read().clone())
    }

    /// Install the post-write hook (entry 2.9).
    pub fn set_config_write_hook(&self, hook: Arc<dyn ConfigWriteHook>) {
        *self.hook.write() = hook;
    }

    /// The post-write hook, for a test to assert the ordering with.
    #[must_use]
    pub fn config_write_hook(&self) -> Arc<dyn ConfigWriteHook> {
        Arc::clone(&self.hook.read().clone())
    }

    /// The upstream repository, so a test can build a second service over the
    /// same store.
    #[must_use]
    pub fn upstream_repository(&self) -> Arc<dyn UpstreamRepository> {
        Arc::clone(&self.upstreams)
    }

    /// The write-ordering epilogue: store write already done, then CP L1
    /// invalidation, then the DP flush, then the audit event, then success.
    async fn notify_written(
        &self,
        actor: &Actor,
        event: &'static str,
        status: u16,
        upstream_id: Uuid,
        alias: &str,
    ) -> Result<(), ManagementError> {
        let hook = Arc::clone(&self.hook.read().clone());
        hook.on_upstream_written(ConfigWriteNotification {
            event,
            tenant_id: actor.tenant_id,
            principal_id: actor.principal_id,
            resource_id: format!("{UPSTREAM_BASE_TYPE}{upstream_id}"),
            upstream_id: Some(upstream_id),
            upstream_alias: Some(alias.to_owned()),
            route: None,
            plugin_id: None,
            status,
            outcome: "accepted",
        })
        .await
        .map_err(ManagementError::Domain)
    }

    /// The ancestor chain of the actor's tenant, direct parent first.
    async fn chain_of(&self, actor: &Actor) -> Result<Vec<Uuid>, ManagementError> {
        self.ancestors
            .ancestors(actor, actor.tenant_id)
            .await
            .map_err(ManagementError::Domain)
    }

    /// The ancestor record bound to `alias`, if one exists.
    async fn ancestor_record(
        &self,
        chain: &[Uuid],
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        for tenant in chain {
            if let Ok(record) = self.upstreams.get_by_alias(*tenant, alias) {
                return Ok(Some(record.upstream));
            }
        }
        Ok(None)
    }

    /// Whether the ancestor upstream blocks the bind by declaring only
    /// `private` sub-configurations, which makes it invisible to a descendant
    /// and leaves the alias available for a local create (`inst-um-cr-17`).
    ///
    /// An ancestor that declares no sub-configuration block at all does not
    /// block the bind: the alias match is still the documented bind case.
    #[must_use]
    fn ancestor_is_private(ancestor: &Upstream) -> bool {
        use crate::domain::dto::SharingMode;
        let declared = [
            ancestor.auth.as_ref().map(|a| a.sharing),
            ancestor.rate_limit.as_ref().map(|r| r.sharing),
            ancestor.cors.as_ref().map(|c| c.sharing),
            ancestor.plugins.as_ref().map(|p| p.sharing),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        !declared.is_empty() && declared.iter().all(|mode| *mode == SharingMode::Private)
    }

    /// The `sharing: enforce` blocks the ancestor owns and would not let a
    /// descendant override.
    fn enforce_ancestor_blocks<'a>(
        ancestor: &'a Upstream,
        replacement: &'a Upstream,
    ) -> Vec<&'static str> {
        let enforced = |mode: Option<crate::domain::dto::SharingMode>| {
            mode == Some(crate::domain::dto::SharingMode::Enforce)
        };
        let mut blocked: Vec<&'static str> = Vec::new();
        if enforced(ancestor.auth.as_ref().map(|a| a.sharing)) && replacement.auth.is_some() {
            blocked.push("auth");
        }
        if enforced(ancestor.rate_limit.as_ref().map(|r| r.sharing)) && replacement.rate_limit.is_some() {
            blocked.push("rate_limit");
        }
        if enforced(ancestor.cors.as_ref().map(|c| c.sharing)) && replacement.cors.is_some() {
            blocked.push("cors");
        }
        if enforced(ancestor.plugins.as_ref().map(|p| p.sharing)) && replacement.plugins.is_some() {
            blocked.push("plugins");
        }
        blocked
    }

    /// The ancestor-bind gate of DESIGN §3.3 (`inst-um-cr-16`/`-17`,
    /// `inst-um-rp-12`/`-13`).
    ///
    /// An alias that matches an ancestor record is a bind, not a conflict: it
    /// requires `oagw:upstream:bind`, and a `private` ancestor upstream blocks
    /// visibility so the alias stays available for a local create. A
    /// descendant override of an `enforce` ancestor-owned sub-configuration is
    /// rejected through the same permission-denied surface.
    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-16
    // `inst-um-cr-16`/`-17`: an alias that matches an ancestor-tenant upstream
    // is a bind — `oagw:upstream:bind` is required, a `private` ancestor
    // upstream blocks visibility and an `enforce` one blocks an override.
    async fn check_ancestor_bind(
        &self,
        actor: &Actor,
        chain: &[Uuid],
        alias: &str,
        replacement: &Upstream,
    ) -> Result<(), ManagementError> {
        let Some(ancestor) = self.ancestor_record(chain, alias).await? else {
            return Ok(());
        };
        if Self::ancestor_is_private(&ancestor) {
            // `private` blocks visibility: the alias stays available for a
            // local create and no bind is formed.
            return Ok(());
        }
        let blocked = Self::enforce_ancestor_blocks(&ancestor, replacement);
        self.authorize(actor, PERM_BIND).await?;
        if blocked.is_empty() {
            Ok(())
        } else {
            Err(ManagementError::Authorization(AuthorizeError::Denied {
                permission: PERM_BIND.to_owned(),
                detail: format!(
                    "the ancestor upstream declares {} with `sharing: enforce`, \
                     which a descendant cannot override",
                    blocked.join(", ")
                ),
            }))
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-16

    /// The per-operation permission gate, evaluated *before* the service call.
    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-8
    // `inst-um-cr-15`, `inst-um-ls-8`, `inst-um-gt-6`, `inst-um-rp-11`,
    // `inst-um-dl-7`, `inst-um-en-10`: the operation's permission is evaluated
    // through the `authz_resolver` before the service call, so an absent
    // security context is `401` and a valid one lacking the grant is `403`
    // through the shared canonical permission-denied surface.
    async fn authorize(&self, actor: &Actor, permission: &str) -> Result<(), ManagementError> {
        let resource = format!("{UPSTREAM_BASE_TYPE}:{}", permission.rsplit(':').next().unwrap_or(""));
        self.authorizer
            .authorize(actor, permission, &resource)
            .await
            .map_err(ManagementError::Authorization)
    }
    // @cpt-end:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-8

    /// Derive and reconcile the alias of a write (`inst-um-cr-6` .. `-11`,
    /// `inst-um-ad-1` .. `-11`).
    ///
    /// # Errors
    ///
    /// Returns the derivation, explicit-alias-required, or mismatched-alias
    /// rejection.
    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-6
    // `inst-um-cr-6` .. `-11`: the pool's alias is derived, reconciled with any
    // supplied alias, and normalized; a non-derivable pool requires an explicit
    // alias and a differing one is rejected naming the derived value.
    fn reconcile_alias(
        supplied: &str,
        server: &crate::domain::dto::ServerConfig,
    ) -> Result<String, ManagementError> {
        let derived = match alias::try_derive_alias(server) {
            Ok(Some(derived)) => Some(derived),
            Ok(None) | Err(AliasDerivationError::NoHostnameEndpoint)
            | Err(AliasDerivationError::BarePublicSuffix)
            | Err(AliasDerivationError::NoCommonSuffix) => None,
            Err(AliasDerivationError::NoEndpoints) => {
                return Err(ManagementError::validation("server.endpoints", "at least one endpoint is required"));
            }
        };
        let normalized = alias::normalize_alias(supplied);
        match (normalized.is_empty(), derived) {
            // No alias supplied: a derivable pool derives it, a non-derivable
            // pool requires an explicit alias (`inst-um-cr-7`/`-8`).
            (true, Some(derived)) => Ok(derived),
            (true, None) => Err(ManagementError::validation(
                "alias",
                "an explicit alias is required for an IP-based or non-derivable endpoint pool",
            )),
            // An alias supplied against a derivable pool must be the derived
            // value (`inst-um-cr-9`/`-10`).
            (false, Some(derived)) if normalized == derived => Ok(derived),
            (false, Some(derived)) => Err(ManagementError::validation(
                "alias",
                &format!(
                    "the derived alias for this endpoint pool is `{derived}`; \
                     to change it, delete and re-create the upstream"
                ),
            )),
            // A non-derivable pool keeps its explicit alias (DESIGN §3.2
            // supersedes §3.3).
            (false, None) if crate::domain::validation::alias_is_valid(&normalized) => Ok(normalized),
            (false, None) => Err(ManagementError::validation(
                "alias",
                "alias matches `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`",
            )),
        }
    }

    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-2
    // `inst-um-cr-2` .. `-13`: the write body is validated against the record
    // shapes (a failure names the offending field and stores nothing), the
    // alias is settled, and the server-assigned identifier and tenant are
    // applied.
    async fn prepare(
        &self,
        actor: &Actor,
        mut record: Upstream,
    ) -> Result<(Upstream, Vec<PluginBinding>), ManagementError> {
        let validated =
            validate_upstream_record(&record, self.allow_http_upstream, AliasMode::Deferred)
                .map_err(ManagementError::Domain)?;
        // `inst-um-cv-9`: the ordered plugin chain is checked on every write.
        if let Some(plugins) = &record.plugins {
            crate::domain::validation::validate_plugin_items(&plugins.items)
                .map_err(ManagementError::Domain)?;
        }
        let alias = Self::reconcile_alias(&validated.alias, &validated.server)?;
        record = validated;
        record.alias = alias;
        // `inst-ps-bind-14`/`-15`: the `auth.config` of the referenced auth
        // plugin is validated against the plugin's registered `config_schema`
        // before any binding row is stored, so an invalid configuration is a
        // `400` and not a stored binding.
        let (auth_ref, auth_config) = match &record.auth {
            Some(auth) => (auth.auth_type.as_deref(), auth.config.as_ref()),
            None => (None, None),
        };
        self.plugin_configs
            .read()
            .clone()
            .validate_auth_config(actor.tenant_id, auth_ref, auth_config)
            .map_err(ManagementError::Domain)?;
        // @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-2
        // `inst-ps-bind-2` .. `-8`: every entry of `plugins.items[]` resolves
        // at binding time through the plugin-catalog boundary, strictly
        // caller-scoped, before any binding row is stored — so a catalog-only
        // identifier and a reference the calling tenant does not hold are a
        // `400` and no interim unresolved binding is ever persisted. The
        // resolution returns the rows the write persists, so the stored
        // `plugin_ref`/`plugin_uuid` pair is the one the resolver agreed on.
        let references =
            record.plugins.as_ref().map_or(&[][..], |plugins| &plugins.items);
        let bindings = self
            .plugin_bindings
            .read()
            .clone()
            .resolve(actor.tenant_id, references)
            .map_err(ManagementError::Domain)?;
        // @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-2
        // `inst-um-cr-13`: the server-generated identifier and the
        // server-assigned tenant.
        if record.id.is_nil() {
            record.id = Uuid::new_v4();
        }
        record.tenant_id = actor.tenant_id;
        Ok((record, bindings))
    }
    // @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-2
    // @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-6

    /// The effective enablement of `record` for the actor
    /// (`inst-um-ei-1` .. `-6`).
    // @cpt-begin:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-4
    // `inst-um-ei-4` .. `-6`: a disabled ancestor record for the same alias
    // presents `disabled-by-ancestor` to the requesting tenant, naming the
    // ancestor as the disabling tenant.
    async fn effective_of(
        &self,
        actor: &Actor,
        record: &Upstream,
    ) -> Result<EffectiveEnablement, ManagementError> {
        // Step 1/-2: the record's own flag, inherited unchanged by every
        // descendant.
        if !record.enabled {
            return Ok(EffectiveEnablement {
                enabled: false,
                disabling_tenant_id: Some(record.tenant_id),
            });
        }
        // Step 4/-5: the closest resolvable ancestor record for the same alias
        // holds the disablement that governs the requesting tenant's view.
        let chain = self.chain_of(actor).await?;
        if let Some(ancestor) = self.ancestor_record(&chain, &record.alias).await? {
            if !ancestor.enabled {
                return Ok(EffectiveEnablement {
                    enabled: false,
                    disabling_tenant_id: Some(ancestor.tenant_id),
                });
            }
        }
        Ok(EffectiveEnablement::own(record.enabled))
    }
    // @cpt-end:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-4

    fn view(&self, record: Upstream, effective: EffectiveEnablement) -> UpstreamView {
        UpstreamView { record, effective }
    }
}

// @cpt-begin:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-2
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-3
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-5
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-1
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-10
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-11
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-12
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-13
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-15
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-3
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-4
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-5
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-7
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-8
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-9
#[async_trait]
impl UpstreamManagement for UpstreamManagementService {
    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-1
    // `inst-um-cr-1`: the create receives the body and the caller's tenant
    // context; every following step is the flow of the FEATURE document.
    async fn create(&self, actor: Actor, record: Upstream) -> Result<UpstreamView, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-15
        // `inst-um-cr-15`: the permission is checked before the service call.
        self.authorize(&actor, crate::domain::gts_helpers::PERM_UPSTREAM_CREATE).await?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-15
        // `inst-um-cr-2` .. `-5`: the body is validated and a failure stores
        // nothing.
        let (record, plugin_bindings) = self.prepare(&actor, record).await?;
        // `inst-um-cr-16`/`-17`: an ancestor alias match is a bind.
        let chain = self.chain_of(&actor).await?;
        self.check_ancestor_bind(&actor, &chain, &record.alias, &record).await?;
        // `inst-um-cr-12`: the `(tenant_id, alias)` lookup and the persist are
        // one critical section inside the repository write path.
        let stored = self
            .upstreams
            .create(actor.tenant_id, StoredUpstream {
                plugin_bindings,
                upstream: record,
            })
            .map_err(|error| {
                if error.is_conflict() {
                    ManagementError::alias_conflict()
                } else {
                    ManagementError::Domain(error)
                }
            })?
            .upstream;
        // `inst-um-cr-18`: store write, then CP L1 invalidation, then the DP
        // flush, then success.
        self.notify_written(&actor, "upstream.create", 201, stored.id, &stored.alias).await?;
        let effective = self.effective_of(&actor, &stored).await?;
        Ok(self.view(stored, effective))
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-1
    }

    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-1
    // `inst-um-ls-1`/`-2`: the query string and the caller's tenant context;
    // the OData interpretation is `domain::list_query`.
    async fn list(&self, actor: Actor, query: &ListQuery) -> Result<Vec<Upstream>, ManagementError> {
        self.authorize(&actor, crate::domain::gts_helpers::PERM_UPSTREAM_READ).await?;
        // `inst-um-ls-5`: only the records the caller's tenant owns are
        // candidates; `inst-um-ls-6`: filter, ordering, offset, limit.
        let records = self
            .upstreams
            .list(actor.tenant_id)
            .map_err(ManagementError::Domain)?
            .into_iter()
            .map(|record| record.upstream)
            .collect::<Vec<_>>();
        Ok(query.apply(&records).into_iter().cloned().collect())
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-1
    }

    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-1
    async fn get(&self, actor: Actor, id: Uuid) -> Result<UpstreamView, ManagementError> {
        self.authorize(&actor, crate::domain::gts_helpers::PERM_UPSTREAM_READ).await?;
        // `inst-um-gt-2` .. `-4`: the record is looked up under the caller's
        // tenant and a foreign key resolves as not-found.
        let stored = self
            .upstreams
            .get(actor.tenant_id, id)
            .map_err(ManagementError::Domain)?
            .upstream;
        let effective = self.effective_of(&actor, &stored).await?;
        Ok(self.view(stored, effective))
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-1
    }

    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-1
    async fn replace(&self, actor: Actor, id: Uuid, record: Upstream) -> Result<UpstreamView, ManagementError> {
        // `inst-um-rp-11`/`-10`/`-en-10`: the override permission gates the
        // replacement, which is also the enable and disable path.
        self.authorize(&actor, crate::domain::gts_helpers::PERM_UPSTREAM_OVERRIDE).await?;
        // `inst-um-rp-2`: the record is loaded first and a missing or foreign
        // identifier is not-found before any validation runs.
        let stored = self
            .upstreams
            .get(actor.tenant_id, id)
            .map_err(ManagementError::Domain)?
            .upstream;
        // `inst-um-rp-3` .. `-5`: the replacement body is validated and a
        // failure leaves the stored record unchanged.
        let (mut replacement, plugin_bindings) = self.prepare(&actor, record).await?;
        // The replacement never moves the identifier, the owner or the alias:
        // `inst-um-rp-6` .. `-8` enforce the immutability matrix.
        replacement.id = stored.id;
        replacement.tenant_id = stored.tenant_id;
        let alias = alias::validate_alias_replacement(&stored, &replacement).map_err(ManagementError::Domain)?;
        replacement.alias = alias;
        // `inst-um-rp-12`/`-13`: an ancestor bind is re-validated.
        let chain = self.chain_of(&actor).await?;
        self.check_ancestor_bind(&actor, &chain, &replacement.alias, &replacement).await?;
        // `inst-um-rp-9`: the full replacement, clearing every omitted
        // optional block to absent and re-persisting the tag rows and the
        // ordered binding rows, as one atomic record write.
        let replaced = self
            .upstreams
            .replace(actor.tenant_id, StoredUpstream {
                plugin_bindings,
                upstream: replacement,
            })
            .map_err(|error| {
                if error.is_conflict() {
                    ManagementError::alias_conflict()
                } else {
                    ManagementError::Domain(error)
                }
            })?
            .upstream;
        // `inst-um-rp-14`: store write, then CP L1 invalidation, then the DP
        // flush, then success.
        self.notify_written(&actor, "upstream.replace", 200, replaced.id, &replaced.alias).await?;
        let effective = self.effective_of(&actor, &replaced).await?;
        Ok(self.view(replaced, effective))
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-1
    }

    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-1
    async fn delete(&self, actor: Actor, id: Uuid) -> Result<(), ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-7
        self.authorize(&actor, crate::domain::gts_helpers::PERM_UPSTREAM_DELETE).await?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-7
        // @cpt-begin:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-2
        // @cpt-begin:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-3
        // `inst-um-dl-2` .. `-4`: a foreign or unknown identifier is
        // not-found and removes nothing.
        // @cpt-begin:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-4
        let stored = self
            .upstreams
            .get(actor.tenant_id, id)
            .map_err(ManagementError::Domain)?
            .upstream;
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-4
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-3
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-2
        // @cpt-begin:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-5
        // `inst-um-dl-5`: the record, its dependent routes, its tag rows and
        // its binding rows go in one atomic operation.
        self.upstreams.delete(actor.tenant_id, id).map_err(ManagementError::Domain)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-5
        // @cpt-begin:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-8
        // `inst-um-dl-8`: store write, then CP L1 invalidation, then the DP
        // flush, then success.
        self.notify_written(&actor, "upstream.delete", 204, stored.id, &stored.alias).await
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-8
        // @cpt-end:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-1
    }

    // @cpt-begin:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-1
    async fn effective_enablement(&self, actor: Actor, id: Uuid) -> Result<EffectiveEnablement, ManagementError> {
        self.authorize(&actor, crate::domain::gts_helpers::PERM_UPSTREAM_READ).await?;
        let stored = self
            .upstreams
            .get(actor.tenant_id, id)
            .map_err(ManagementError::Domain)?
            .upstream;
        self.effective_of(&actor, &stored).await
        // @cpt-end:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-1
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-6
// @cpt-end:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-5
// @cpt-end:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-3
// @cpt-end:cpt-cf-oagw-algo-upstream-management-enabled-inheritance:p1:inst-um-ei-2
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-9
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-8
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-7
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-6
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-5
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-4
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-3
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-15
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-13
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-12
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-11
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-10
// @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-1
//

// @cpt-begin:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-2
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-3
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-4
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-5
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-6
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-10
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-11
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-12
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-13
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-14
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-2
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-3
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-4
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-5
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-6
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-7
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-8
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-9
/// The ordered plugin bindings of a `plugins` block (`inst-um-cv-9`): the
/// positions are the list indices, contiguous from zero, the reference is
/// stored on every row and the UUID is recorded only when the reference is
/// UUID-backed.
#[must_use]
pub fn bindings_of(record: &Upstream) -> Vec<PluginBinding> {
    bindings_of_references(record.plugins.as_ref().map_or(&[], |plugins| &plugins.items))
}
//
// @cpt-end:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-6
// @cpt-end:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-5
// @cpt-end:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-4
// @cpt-end:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-3
// @cpt-end:cpt-cf-oagw-flow-upstream-management-get:p1:inst-um-gt-2
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-9
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-8
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-7
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-6
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-5
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-4
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-3
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-2
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-14
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-13
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-12
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-11
// @cpt-end:cpt-cf-oagw-flow-upstream-management-replace:p1:inst-um-rp-10
//

/// The ordered plugin bindings of a bare reference list.
#[must_use]
pub fn bindings_of_references(references: &[String]) -> Vec<PluginBinding> {
    references
        .iter()
        .enumerate()
        .map(|(position, reference)| PluginBinding {
            position: position as u32,
            plugin_ref: reference.clone(),
            plugin_uuid: Uuid::parse_str(reference).ok(),
        })
        .collect()
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod tests;
