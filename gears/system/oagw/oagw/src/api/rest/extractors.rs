//! Extractors for the OAGW REST surface.
//!
//! The platform installs its bearer-token middleware above the gear, so a
//! handler reached through the host always finds a
//! [`SecurityContext`](toolkit_security::SecurityContext) in the request
//! extensions. Routes exercised below that middleware (a gear-level test harness,
//! for instance) do not, and [`Extension`] would answer a missing context with a
//! `500`. [`AuthenticatedSubject`] turns the absence into the documented `401`
//! instead, so the authentication requirement holds wherever the router is
//! mounted.
//!
//! Authentication and authorization are separate questions here. The extractor
//! answers the first: is there a principal at all. [`require_permission`]
//! answers the second: may that principal do *this* — reading the permission
//! the platform's bearer token carries, in the form
//! `gts.cf.core.oagw.<kind>.v1~:<action>`.

use axum::extract::FromRequest;
use axum::extract::FromRequestParts;
use axum::extract::rejection::JsonRejection;
use serde::de::DeserializeOwned;
use toolkit_security::SecurityContext;

use crate::api::rest::error::GatewayProblem;
use crate::domain::error::{DomainError, ErrorKind};
use crate::gts_helpers::{AUTH_PLUGIN_GTS_ID, PROXY_GTS_ID, ROUTE_GTS_ID, UPSTREAM_GTS_ID};

/// What a caller wants to do to a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Create or override a configuration object.
    Write,
    /// Read a configuration object.
    Read,
    /// Delete a configuration object.
    Delete,
    /// Send traffic through the gateway.
    Invoke,
}

impl Action {
    /// The action suffix the permission string carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            // `override` is the document's name for rewriting an object an
            // ancestor owns; it sits in the same grant as `create`.
            Self::Write => "create",
            Self::Read => "read",
            Self::Delete => "delete",
            Self::Invoke => "invoke",
        }
    }

    /// The permission this action needs on `kind`.
    ///
    /// # Examples
    /// ```
    /// # use oagw::api::rest::extractors::{Action, proxy_permission};
    /// assert_eq!(
    ///     proxy_permission(),
    ///     "gts.cf.core.oagw.proxy.v1~:invoke"
    /// );
    /// ```
    #[must_use]
    pub fn permission_on(self, kind: &str) -> String {
        format!("{kind}:{}", self.as_str())
    }
}

/// The permission `action` on the upstream resource needs.
#[must_use]
pub fn upstream_permission(action: Action) -> String {
    action.permission_on(UPSTREAM_GTS_ID)
}

/// The permission `action` on the route resource needs.
#[must_use]
pub fn route_permission(action: Action) -> String {
    action.permission_on(ROUTE_GTS_ID)
}

/// The permission `action` on a plugin resource needs.
#[must_use]
pub fn plugin_permission(action: Action) -> String {
    action.permission_on(AUTH_PLUGIN_GTS_ID)
}

/// The permission the proxy path needs.
#[must_use]
pub fn proxy_permission() -> String {
    Action::Invoke.permission_on(PROXY_GTS_ID)
}

/// Whether `scopes` grant `permission`.
///
/// A token that carries no scope at all is unrestricted: the platform
/// authenticated the principal and expressed no restriction over it, and the
/// gear would be unusable behind an authenticator that emits no claims. A token
/// that *does* carry scopes is governed by them — this is fail-closed, and
/// `*` is the platform's "first-party, unrestricted" scope.
#[must_use]
fn grants(scopes: &[String], permission: &str) -> bool {
    scopes.is_empty()
        || scopes
            .iter()
            .any(|scope| scope == "*" || scope == permission)
}

/// Check that the subject's token may do `action` on `kind`.
///
/// # Errors
/// Returns [`ErrorKind::AuthFailed`] — the documented answer for an
/// unauthorized call — with the missing permission in the detail, so an
/// operator can see what to grant.
pub fn require_permission(
    ctx: &SecurityContext,
    action: Action,
    kind: &str,
) -> Result<(), DomainError> {
    let permission = action.permission_on(kind);
    if grants(ctx.token_scopes(), &permission) {
        return Ok(());
    }
    Err(DomainError::new(
        ErrorKind::AuthFailed,
        format!("the token does not carry the {permission} permission"),
    ))
}

/// Check that the subject may proxy to an upstream.
///
/// # Errors
/// As [`require_permission`], for the proxy resource's `invoke` action.
pub fn require_proxy(ctx: &SecurityContext) -> Result<(), DomainError> {
    require_permission(ctx, Action::Invoke, PROXY_GTS_ID)
}

/// A JSON request body, rendered as the gear's own problem document when the
/// payload is not what the schema wants.
///
/// [`axum::Json`] answers a payload that fails to deserialize with `422` or
/// `400` of its own choosing and a plain-text body; the management contract
/// says a malformed payload is `400` `validation.error.v1` in
/// `application/problem+json`, so the handlers read through this extractor
/// instead.
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = GatewayProblem;

    async fn from_request(
        request: axum::extract::Request,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(GatewayProblem::from(DomainError::validation(
                malformed_body(&rejection),
            ))),
        }
    }
}

/// The message a body rejection carries, without the extractor's boilerplate.
fn malformed_body(rejection: &JsonRejection) -> String {
    let reason = rejection.body_text();
    let reason = reason
        .strip_prefix("Failed to deserialize the JSON body into the target type: ")
        .unwrap_or(&reason);
    reason.to_owned()
}

/// The authenticated caller of a management or proxy request.
#[derive(Debug, Clone)]
pub struct AuthenticatedSubject {
    ctx: SecurityContext,
}

impl AuthenticatedSubject {
    /// The security context the request was authenticated with.
    #[must_use]
    pub const fn context(&self) -> &SecurityContext {
        &self.ctx
    }

    /// The tenant the request acts in.
    #[must_use]
    pub fn tenant_id(&self) -> uuid::Uuid {
        self.ctx.subject_tenant_id()
    }

    /// Consume the extractor into the context it carries.
    #[must_use]
    pub fn into_context(self) -> SecurityContext {
        self.ctx
    }
}

impl From<SecurityContext> for AuthenticatedSubject {
    fn from(ctx: SecurityContext) -> Self {
        Self { ctx }
    }
}

impl<S> FromRequestParts<S> for AuthenticatedSubject
where
    S: Send + Sync,
{
    type Rejection = GatewayProblem;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<SecurityContext>() {
            Some(ctx) => Ok(Self { ctx: ctx.clone() }),
            None => Err(GatewayProblem::new(DomainError::new(
                ErrorKind::AuthFailed,
                "no authenticated subject; supply a bearer token".to_owned(),
            ))),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod extractor_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;

    fn router() -> axum::Router {
        axum::Router::new().route(
            "/who",
            axum::routing::get(|subject: AuthenticatedSubject| async move {
                subject.tenant_id().to_string()
            }),
        )
    }

    #[tokio::test]
    async fn a_missing_context_is_unauthorized() {
        let response = router()
            .oneshot(
                Request::get("/who")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(crate::api::rest::error::ERROR_SOURCE_HEADER)
                .unwrap(),
            crate::api::rest::error::ERROR_SOURCE_GATEWAY
        );
    }

    #[tokio::test]
    async fn an_inserted_context_is_accepted() {
        let tenant = uuid::Uuid::new_v4();
        let ctx = SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .expect("valid context");
        let request = Request::get("/who")
            .body(axum::body::Body::empty())
            .unwrap();
        let mut request = request;
        request.extensions_mut().insert(ctx);
        let response = router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
    }
}
