//! Path extractors for OAGW resource identifiers.
//!
//! Resource ids on the wire may be either a plain UUID or the anonymous
//! GTS instance id (`gts.cf.core.oagw.upstream.v1~{uuid}`, etc.) echoed
//! back by the management API. [`IdPath`] accepts both and normalises to
//! the `Uuid`.

use axum::extract::FromRequestParts;
use axum::extract::Path;
use axum::http::request::Parts;
use uuid::Uuid;

use crate::domain::error::ProblemSpec;
use crate::domain::gts;

/// Base types whose instance ids end in a UUID.
const INSTANCE_ID_BASES: [&str; 5] = [
    gts::UPSTREAM_TYPE,
    gts::ROUTE_TYPE,
    gts::AUTH_PLUGIN_TYPE,
    gts::GUARD_PLUGIN_TYPE,
    gts::TRANSFORM_PLUGIN_TYPE,
];

/// Parse a resource id path segment: plain UUID or anonymous GTS instance id.
///
/// # Errors
///
/// An RFC 9457 `valid_target_host`-style validation problem (400) when the
/// value is neither a UUID nor a recognised OAGW instance id.
#[must_use]
pub fn parse_resource_id(raw: &str) -> Result<Uuid, ProblemSpec> {
    if let Ok(id) = Uuid::parse_str(raw) {
        return Ok(id);
    }
    for base in INSTANCE_ID_BASES {
        if let Some(id) = gts::parse_instance_uuid(base, raw) {
            return Ok(id);
        }
    }
    Err(ProblemSpec {
        gts_type: gts::ERR_VALIDATION,
        status: 400,
        title: "Validation Error",
        detail: format!("'{raw}' is not a valid OAGW resource id (expected a UUID or GTS instance id)"),
        context: vec![("id".to_owned(), raw.to_owned())],
        retry_after_seconds: None,
    })
}

/// Axum extractor: `{id}` path segment as a `Uuid`.
pub struct IdPath(pub Uuid);

impl<S> FromRequestParts<S> for IdPath
where
    S: Send + Sync,
{
    type Rejection = crate::api::rest::error::OagwError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let Path(raw) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| {
                crate::api::rest::error::OagwError::validation("missing {id} path parameter")
            })?;
        parse_resource_id(&raw)
            .map(IdPath)
            .map_err(crate::api::rest::error::OagwError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_uuid_parses() {
        let id = Uuid::new_v4();
        assert_eq!(parse_resource_id(&id.to_string()), Ok(id));
    }

    #[test]
    fn gts_instance_id_parses() {
        let id = Uuid::new_v4();
        let gid = gts::upstream_instance_id(id);
        assert_eq!(parse_resource_id(&gid), Ok(id));
    }

    #[test]
    fn cross_type_instance_id_parses() {
        let id = Uuid::new_v4();
        let gid = gts::plugin_instance_id(
            crate::domain::model::PluginKind::Guard,
            id,
        );
        assert_eq!(parse_resource_id(&gid), Ok(id));
    }

    #[test]
    fn garbage_rejected() {
        assert_eq!(
            parse_resource_id("not-an-id"),
            Err(ProblemSpec {
                gts_type: gts::ERR_VALIDATION,
                status: 400,
                title: "Validation Error",
                detail: "'not-an-id' is not a valid OAGW resource id (expected a UUID or GTS instance id)"
                    .to_owned(),
                context: vec![("id".to_owned(), "not-an-id".to_owned())],
                retry_after_seconds: None,
            })
        );
    }
}
