//! Axum extractors for the OAGW management API.
//!
//! * [`Tenant`] — the calling tenant, taken from the authenticated
//!   [`SecurityContext`] the platform's auth middleware installs.
//! * [`ListQueryParams`] — the OData query-parameter family
//!   (`$filter`, `$select`, `$orderby`, `$top`, `$skip`).
//! * [`require_path_id`] — parses an `{id}` path segment that may arrive as a
//!   bare UUID or as a full GTS instance id
//!   (`gts.cf.core.oagw.upstream.v1~{uuid}`).
//!
//! DESIGN.md "List Query Parameters" explicitly supports `$skip` (offset
//! pagination), so the management API binds its own extractor instead of the
//! platform's `OData` extractor, which pages by cursor only.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use toolkit_security::SecurityContext;

use super::dto::parse_resource_id;
use super::error::OagwProblem;

/// The calling tenant, from the authenticated security context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tenant(pub uuid::Uuid);

impl Tenant {
    /// The tenant id.
    #[must_use]
    pub const fn id(self) -> uuid::Uuid {
        self.0
    }
}

impl<S> FromRequestParts<S> for Tenant
where
    S: Send + Sync,
{
    type Rejection = OagwProblem;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<SecurityContext>() {
            Some(ctx) => Ok(Self(ctx.subject_tenant_id())),
            // No security context in this process (e.g. a bootstrap or
            // test deployment): the caller acts in the root tenant.
            None => Ok(Self(uuid::Uuid::nil())),
        }
    }
}

/// Raw management-API list parameters (`$filter`, `$select`, `$orderby`,
/// `$top`, `$skip`).
///
/// Bound off the query string verbatim; clamping and evaluation happen in
/// [`crate::domain::dto::ListQuery`].
#[derive(Debug, Clone, Default)]
pub struct ListQueryParams(pub crate::domain::dto::ListParams);

impl<S> FromRequestParts<S> for ListQueryParams
where
    S: Send + Sync,
{
    type Rejection = OagwProblem;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let raw = parts.uri.query().unwrap_or_default();
        Ok(Self(parse_query(raw)))
    }
}

/// Parse a raw query string into [`crate::domain::dto::ListParams`].
///
/// Percent-decoding uses a minimal `+`/`%XX` decoder so the gear needs no
/// extra dependency; the values that matter here (field names, filter
/// expressions, ids) are ASCII.
#[must_use]
pub fn parse_query(raw: &str) -> crate::domain::dto::ListParams {
    let mut params = crate::domain::dto::ListParams::default();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        };
        match key.as_str() {
            "$filter" | "filter" => params.filter = Some(value),
            "$select" | "select" => params.select = Some(value),
            "$orderby" | "orderby" => params.orderby = Some(value),
            "$top" | "top" | "limit" => params.top = value.parse().ok(),
            "$skip" | "skip" | "offset" => params.skip = value.parse().ok(),
            _ => {}
        }
    }
    params
}

/// Minimal percent-decoder (`%XX` plus `+` as space).
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                match (high, low) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        index += 3;
                    }
                    _ => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a `{id}` path segment into a resource UUID.
///
/// # Errors
/// [`DomainError::Validation`] when the segment is neither a bare UUID nor a
/// GTS instance id.
#[allow(clippy::result_large_err)] // the platform's own convention for DTO errors
pub fn require_path_id(raw: &str) -> Result<uuid::Uuid, OagwProblem> {
    super::dto::require_resource_id(raw, "id").map_err(OagwProblem::from)
}

/// Parse an optional `{id}` path segment, treating `null` / `none` as absent.
#[must_use]
pub fn parse_resource_ref(raw: &str) -> Option<uuid::Uuid> {
    parse_resource_id(raw).filter(|id| !id.is_nil())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_spellings() {
        let params = parse_query("%24filter=alias%20eq%20%27a.com%27&$top=5&$skip=10");
        assert_eq!(params.filter.as_deref(), Some("alias eq 'a.com'"));
        assert_eq!(params.top, Some(5));
        assert_eq!(params.skip, Some(10));
    }

    #[test]
    fn ignores_unknown_keys() {
        let params = parse_query("$orderby=alias%20desc&bogus=1");
        assert_eq!(params.orderby.as_deref(), Some("alias desc"));
        assert_eq!(params.top, None);
    }

    #[test]
    fn empty_query_yields_defaults() {
        let params = parse_query("");
        assert_eq!(params, crate::domain::dto::ListParams::default());
    }

    #[test]
    fn gts_instance_ids_are_accepted() {
        let uuid = uuid::Uuid::new_v4();
        let raw = format!("gts.cf.core.oagw.upstream.v1~{uuid}");
        assert_eq!(require_path_id(&raw).unwrap(), uuid);
        assert_eq!(require_path_id(&uuid.to_string()).unwrap(), uuid);
        assert!(require_path_id("not-an-id").is_err());
    }
}
