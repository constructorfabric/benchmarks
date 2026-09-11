//! Inbound extraction: the caller's tenant, the query string and the body.
//!
//! Everything the handlers need from the transport is normalised here, so the
//! handlers never look at raw query strings or raw JSON. The tenant always
//! comes from the security context the api-gateway injected; it is never taken
//! from a body, a query parameter or a header the caller could have written.

use crate::domain::error::OagwError;
use crate::domain::identifiers::new_uuid;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The resolved caller identity for a request.
#[derive(Debug, Clone)]
pub struct CallerContext {
    /// Tenant the gateway scoped the request to.
    pub tenant_id: Uuid,
    /// Subject identifier, when the caller authenticated.
    pub subject_id: Option<Uuid>,
    /// Whether the caller is the anonymous fallback.
    pub anonymous: bool,
}

impl CallerContext {
    /// The context of a caller the gateway could not identify.
    ///
    /// This is the fail-open posture used when the api-gateway is not in front
    /// of the gear (tests, local runs); the tenant is the nil tenant, which is
    /// the tenant the seeded fixtures use.
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            tenant_id: Uuid::nil(),
            subject_id: None,
            anonymous: true,
        }
    }
}

impl FromRequestParts<()> for CallerContext {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &()) -> Result<Self, Self::Rejection> {
        Ok(Self::from_parts(parts))
    }
}

impl CallerContext {
    /// Extracts the caller from request parts, falling back to anonymous.
    #[must_use]
    pub fn from_parts(parts: &Parts) -> Self {
        match parts.extensions.get::<SecurityContext>() {
            Some(context) => {
                let tenant_id = context.subject_tenant_id();
                let subject_id = context.subject_id();
                Self {
                    tenant_id,
                    subject_id: (subject_id != Uuid::nil()).then_some(subject_id),
                    anonymous: false,
                }
            }
            None => Self::anonymous(),
        }
    }
}

/// A parsed query string, with the OData-ish options the management API honours.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryOptions {
    /// `$filter` predicate as supplied.
    pub filter: Option<String>,
    /// `$select` field list.
    pub select: Option<Vec<String>>,
    /// `$orderby` clause.
    pub orderby: Option<String>,
    /// `$top` page size.
    pub top: Option<usize>,
    /// `$skip` offset.
    pub skip: Option<usize>,
    /// Whether the caller asked for a count alongside the page.
    pub count: bool,
}

impl QueryOptions {
    /// Parses a raw query string.
    #[must_use]
    pub fn parse(query: &str) -> Self {
        let mut options = Self::default();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            let key = key.into_owned();
            let value = value.into_owned();
            match key.as_str() {
                "$filter" => options.filter = Some(value),
                "$select" => {
                    options.select = Some(
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|entry| !entry.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    );
                }
                "$orderby" => options.orderby = Some(value),
                "$top" => {
                    if let Ok(parsed) = value.parse::<usize>() {
                        options.top =
                            Some(parsed.min(crate::domain::services::management::MAX_PAGE_SIZE));
                    }
                }
                "$skip" => {
                    if let Ok(parsed) = value.parse::<usize>() {
                        options.skip = Some(parsed);
                    }
                }
                "$count" => options.count = value.eq_ignore_ascii_case("true"),
                _ => {}
            }
        }
        options
    }

    /// The effective page size after the configured defaults.
    #[must_use]
    pub fn page_size(&self) -> usize {
        self.top
            .unwrap_or(crate::domain::services::management::DEFAULT_PAGE_SIZE)
            .clamp(1, crate::domain::services::management::MAX_PAGE_SIZE)
    }

    /// The effective offset.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.skip.unwrap_or(0)
    }
}

/// Reads a body as JSON, rejecting anything the model does not name.
///
/// The model types carry `deny_unknown_fields`, so serde does the rejecting;
/// this wrapper turns a parse failure into the documented
/// [`OagwError::ValidationError`].
///
/// # Errors
///
/// Returns [`OagwError::ValidationError`] for malformed or unknown-field
/// bodies and [`OagwError::PayloadTooLarge`] when the body is larger than the
/// limit.
pub fn parse_json<T: serde::de::DeserializeOwned>(
    body: &[u8],
    limit: usize,
) -> Result<T, OagwError> {
    if body.len() > limit {
        return Err(OagwError::PayloadTooLarge(format!(
            "request body of {} bytes exceeds the {} byte limit",
            body.len(),
            limit
        )));
    }
    serde_json::from_slice(body)
        .map_err(|error| OagwError::ValidationError(format!("request body is not valid: {error}")))
}

/// The correlation identifier a request carries, generated when absent.
#[must_use]
pub fn request_id_from(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| new_uuid().to_string(), str::to_owned)
}

/// Whether a request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &str, headers: &axum::http::HeaderMap) -> bool {
    method.eq_ignore_ascii_case("OPTIONS") && headers.contains_key("access-control-request-method")
}

/// Whether a request is a WebSocket upgrade.
#[must_use]
pub fn is_upgrade(method: &str, headers: &axum::http::HeaderMap) -> bool {
    let wants_upgrade = headers
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty());
    let announces = headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"));
    method.eq_ignore_ascii_case("GET") && wants_upgrade && announces
}

/// The `X-OAGW-Target-Host` pinning header, when present.
#[must_use]
pub fn target_host(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-oagw-target-host")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
