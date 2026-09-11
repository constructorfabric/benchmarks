//! Management handlers — the ten endpoints of DECOMPOSITION §2.2.
//!
//! Every handler runs the same order: authenticate, enforce the operation's
//! permission, resolve the calling tenant, then — and only then — read the body
//! and reach the management service. The one address a path carries is
//! resolved between the permission and the tenant, and a selector that parses
//! to no identifier is answered only once both checks have answered, so a
//! request that cannot state who it is or what it may do writes nothing, reads
//! nothing, and is told nothing about the path it named.
//!
//! A handler never formats a problem body of its own: [`refused`] maps the
//! service outcome onto the module's problem builders, and a persistence
//! failure is logged with the reason it carries and answered with the platform's
//! 500 shape.

pub mod metrics;
pub mod plugins;
pub mod proxy;
pub mod routes;
pub mod upstreams;

use std::sync::Arc;

use authz_resolver_sdk::pep::ResourceType;
use axum::body::Bytes;
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use serde_json::Value;
use toolkit_security::pep_properties;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::problem;
use crate::api::rest::state::OagwState;
use crate::control_plane::scoping;
use crate::control_plane::service::ServiceError;
use crate::control_plane::validation::ResourceKind;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::plugin_contract::PluginFamily;

/// The `create` permission the family declares.
pub(crate) const CREATE: &str = "create";
/// The `read` permission the family declares.
pub(crate) const READ: &str = "read";
/// The `override` permission the family declares; a replacement and any
/// `enabled` change need it, because the ten paths hold no dedicated
/// enable/disable operation.
pub(crate) const OVERRIDE: &str = "override";
/// The `delete` permission the family declares.
pub(crate) const DELETE: &str = "delete";
/// The `invoke` permission the proxy API declares, which the Data Plane
/// enforces before any resolution or cache read runs.
pub(crate) const INVOKE: &str = "invoke";

/// The PEP properties the two resource types declare to the enforcer.
const SUPPORTED_PROPERTIES: &[&str] = &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID];

/// The enforcer's descriptor of one resource kind.
#[must_use]
pub fn resource_type(kind: ResourceKind) -> ResourceType {
    match kind {
        ResourceKind::Upstream => {
            ResourceType::from_static(crate::gts::UPSTREAM_TYPE, SUPPORTED_PROPERTIES)
        }
        ResourceKind::Route => {
            ResourceType::from_static(crate::gts::ROUTE_TYPE, SUPPORTED_PROPERTIES)
        }
    }
}

/// Authenticates the request: the 401 a subjectless request answers with.
///
/// A request that carries no `SecurityContext` extension was never
/// authenticated, so it is answered 401 and nothing else runs — no path
/// resolution, no validation, no store access.
///
/// # Errors
///
/// Returns the response the refusal answers with; the `Ok` arm carries the
/// authenticated context the caller enforces a permission against.
pub async fn authenticate<'a>(
    context: Option<&'a SecurityContext>,
    instance: &str,
) -> Result<&'a SecurityContext, Response> {
    // @cpt-begin:cpt-cf-oagw-dod-authz-permissions:p1:inst-authz-401
    if let Some(context) = context {
        return Ok(context);
    }
    tracing::warn!(instance, "management request reached no authenticated subject");
    Err(problem::problem_response(&unauthenticated(), instance))
    // @cpt-end:cpt-cf-oagw-dod-authz-permissions:p1:inst-authz-401
}

/// Enforces one operation's permission before any validation or store access.
///
/// An authenticated request whose token lacks the permission is answered 403. A
/// state resolved with no enforcer at all fails closed, because a permission
/// this process cannot check is a permission it must not grant.
///
/// # Errors
///
/// Returns the response the refusal answers with; the `Ok` arm answers nothing
/// and the caller proceeds.
pub async fn enforce(
    state: &OagwState,
    context: &SecurityContext,
    kind: ResourceKind,
    action: &str,
    id: Option<Uuid>,
    instance: &str,
) -> Result<(), Response> {
    // @cpt-begin:cpt-cf-oagw-dod-authz-permissions:p1:inst-authz-403
    let Some(enforcer) = state.enforcer() else {
        tracing::warn!(instance, "no AuthZ client resolved; the management surface fails closed");
        return Err(problem::forbidden_response(gts_type(kind), instance));
    };
    if let Err(error) = enforcer
        .access_scope(context, &resource_type(kind), action, id)
        .await
    {
        tracing::warn!(instance, action, error = %error, "management permission refused");
        return Err(problem::forbidden_response(gts_type(kind), instance));
    }
    // @cpt-end:cpt-cf-oagw-dod-authz-permissions:p1:inst-authz-403
    Ok(())
}

/// Authenticates the request and enforces one operation's permission, in that
/// order and before any validation or store access.
///
/// # Errors
///
/// Returns the response the refusal answers with; the `Ok` arm carries the
/// authenticated context the caller resolves the tenant from.
pub async fn authorize<'a>(
    state: &'a OagwState,
    context: Option<&'a SecurityContext>,
    kind: ResourceKind,
    action: &str,
    id: Option<Uuid>,
    instance: &str,
) -> Result<&'a SecurityContext, Response> {
    let context = authenticate(context, instance).await?;
    enforce(state, context, kind, action, id, instance).await?;
    Ok(context)
}

/// Resolves the calling tenant the request carries, answering 401 when the
/// authenticated subject carries none.
#[allow(clippy::result_large_err)]
pub fn tenant_of(context: &SecurityContext, instance: &str) -> Result<Uuid, Response> {
    scoping::calling_tenant(context).map_err(|error| problem::problem_response(&error, instance))
}

/// Enforces one operation's permission against the plugin arm one family
/// selects, before any validation or store access.
///
/// The three plugin families are three resource types to the enforcer, so the
/// family the request selects — from the body's `plugin_type` for a create and
/// from the path identifier for an addressed operation — is what names the arm.
///
/// # Errors
///
/// Returns the response the refusal answers with; the `Ok` arm answers nothing
/// and the caller proceeds.
pub async fn enforce_plugin(
    state: &OagwState,
    context: &SecurityContext,
    family: PluginFamily,
    action: &str,
    id: Option<Uuid>,
    instance: &str,
) -> Result<(), Response> {
    let Some(enforcer) = state.enforcer() else {
        tracing::warn!(instance, "no AuthZ client resolved; the management surface fails closed");
        return Err(problem::forbidden_response(family.base_type(), instance));
    };
    if let Err(error) = enforcer
        .access_scope(context, &plugin_resource_type(family), action, id)
        .await
    {
        tracing::warn!(instance, action, error = %error, "management permission refused");
        return Err(problem::forbidden_response(family.base_type(), instance));
    }
    Ok(())
}

/// Authenticates the request and enforces the permission of the plugin arm one
/// family selects, in that order and before any validation or store access.
///
/// # Errors
///
/// Returns the response the refusal answers with; the `Ok` arm carries the
/// authenticated context the caller resolves the tenant from.
pub async fn authorize_plugin<'a>(
    state: &'a OagwState,
    context: Option<&'a SecurityContext>,
    family: PluginFamily,
    action: &str,
    id: Option<Uuid>,
    instance: &str,
) -> Result<&'a SecurityContext, Response> {
    let context = authenticate(context, instance).await?;
    enforce_plugin(state, context, family, action, id, instance).await?;
    Ok(context)
}

/// Authenticates the request and enforces the permission of the arm one
/// plugin path selector names, in that order and before any validation or
/// store access.
///
/// A selector whose prefix names a family is enforced against that arm with
/// the identifier its full form carries; a selector that names no family at
/// all names no arm, and is admitted only when the token holds the permission
/// on every one — the same posture a plugin list takes.
///
/// # Errors
///
/// Returns the response the refusal answers with; the `Ok` arm carries the
/// authenticated context the caller resolves the tenant from.
pub async fn authorize_selector<'a>(
    state: &'a OagwState,
    context: Option<&'a SecurityContext>,
    selector: &crate::api::rest::params::PluginSelector,
    action: &str,
    instance: &str,
) -> Result<&'a SecurityContext, Response> {
    let Some(family) = selector.family else {
        return authorize_plugin_all(state, context, action, instance).await;
    };
    authorize_plugin(state, context, family, action, selector.id, instance).await
}

/// Enforces one operation's permission on every plugin arm, in family order.
///
/// A list carries no path identifier and no body, so no single arm names the
/// resource it reads: the operation reads the catalogue of all three families,
/// and is admitted only when the token holds the permission on each. One
/// refusal is one refusal, answered before any query is built.
///
/// # Errors
///
/// Returns the response the first refusal answers with; the `Ok` arm answers
/// nothing and the caller proceeds.
pub async fn authorize_plugin_all<'a>(
    state: &'a OagwState,
    context: Option<&'a SecurityContext>,
    action: &str,
    instance: &str,
) -> Result<&'a SecurityContext, Response> {
    const FAMILIES: [PluginFamily; 3] = [
        PluginFamily::Auth,
        PluginFamily::Guard,
        PluginFamily::Transform,
    ];
    let context = authenticate(context, instance).await?;
    for family in FAMILIES {
        enforce_plugin(state, context, family, action, None, instance).await?;
    }
    Ok(context)
}

/// Reads the request body as one JSON object.
///
/// A body that is not JSON at all is answered before the validators run; the
/// detail names the failure and copies nothing from the body.
#[allow(clippy::result_large_err)]
pub fn parse_body(body: &Bytes, instance: &str) -> Result<Value, Response> {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => Ok(value),
        Err(_) => {
            let error = DomainError::gateway(
                ErrorKind::ValidationError,
                "the request body is not a well-formed JSON document",
            );
            Err(problem::problem_response(&error, instance))
        }
    }
}

/// The JSON response one representation answers with.
#[must_use]
pub fn json_response(status: StatusCode, representation: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        )
        .body(axum::body::Body::from(representation.to_string()))
        .unwrap_or_else(|_| Response::new(axum::body::Body::empty()))
}

/// The response one refused or failed operation answers with.
///
/// A domain failure is logged with the failing property names and the
/// colliding identifier its detail carries; a persistence failure is logged
/// with the reason the store produced and answered with the platform's 500
/// problem shape, which carries that reason nowhere.
#[must_use]
pub fn refused(error: &ServiceError, instance: &str) -> Response {
    match error {
        ServiceError::Domain(failure) => {
            tracing::info!(
                instance,
                kind = failure.kind.title(),
                detail = %failure.detail,
                "management operation refused"
            );
            problem::problem_response(failure, instance)
        }
        ServiceError::Forbidden { resource, permission } => {
            tracing::warn!(
                instance,
                permission,
                "a descendant override permission was refused"
            );
            problem::forbidden_permission_response(gts_type(*resource), instance)
        }
        ServiceError::Storage { reason } => {
            tracing::error!(
                instance,
                reason,
                "the configuration store could not apply the management operation"
            );
            problem::storage_problem_response(instance)
        }
    }
}

/// The configuration-change record `cpt-cf-oagw-flow-config-change-logged`
/// writes when a management write completed.
///
/// The record is written at the in-process post-write seam the cache flush and
/// the cleanup notifications occupy, so it is produced when the write is
/// durable and before the response is returned. `host`, `duration_ms`,
/// `request_size`, and `response_size` are omitted, because no proxy exchange
/// happened, and the record is never sampled, because a configuration change
/// is by definition not a high-volume event. A write that was refused writes
/// no record at all.
pub fn record_config_change(
    state: &OagwState,
    event: &'static str,
    context: &SecurityContext,
    method: &str,
    instance: &str,
    status: u16,
    tenant: Uuid,
) {
    // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-emit
    // `cpt-cf-oagw-algo-audit-emit` builds the configuration-change
    // `AuditEvent` at the same in-process post-write seam the cache flush and
    // the cleanup notifications occupy: the event name carries the resource
    // kind and the operation, `tenant_id` and `principal_id` are the writer's,
    // `path` and `method` are the management path and method addressed, and
    // `status` is the status the handler answered.
    // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-level
    // The record is written to stdout at INFO and is not subject to the
    // high-volume sampling decision, because a configuration change is by
    // definition not a high-volume event.
    state.observability().config_change(
        event,
        Some(tenant),
        Some(context.subject_id().to_string()),
        method,
        instance,
        status,
    );
    // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-level
    // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-emit
}

/// The `204 No Content` response a successful deletion answers with.
#[must_use]
pub fn no_content() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| Response::new(axum::body::Body::empty()))
}

/// The `instance` a problem document names: the request path the operation was
/// issued against.
#[must_use]
pub fn instance_of(uri: &Uri) -> String {
    String::from(uri.path())
}

/// The 404 response a path-addressed identifier that addresses nothing answers
/// with.
#[must_use]
pub(crate) fn unaddressed(instance: &str) -> Response {
    problem::problem_response(&scoping::path_miss(), instance)
}

/// The GTS type of one resource kind.
fn gts_type(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Upstream => crate::gts::UPSTREAM_TYPE,
        ResourceKind::Route => crate::gts::ROUTE_TYPE,
    }
}

/// The enforcer's descriptor of one plugin family's base type.
#[must_use]
pub(crate) fn plugin_resource_type(family: PluginFamily) -> ResourceType {
    ResourceType::from_static(family.base_type(), SUPPORTED_PROPERTIES)
}

/// The 401 row a request with no authenticated subject answers with.
#[allow(clippy::result_large_err)]
fn unauthenticated() -> DomainError {
    DomainError::gateway(
        ErrorKind::AuthenticationFailed,
        "the request carries no authenticated subject",
    )
}

/// The `Arc` state extractor alias the handlers share.
pub type SharedState = Arc<OagwState>;
