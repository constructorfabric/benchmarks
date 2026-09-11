// @cpt-begin:cpt-cf-oagw-dod-upstream-api-create:p1:inst-upstream-handlers
//! Upstream management handlers.

use crate::api::rest::dto::{ListQuery, UpstreamDto, UpstreamListDto, UpstreamWriteDto};
use crate::api::rest::error::set_error_source;
use crate::api::rest::state::OagwState;
use crate::domain::alias::{enforce_alias_update, resolve_create_alias};
use crate::domain::error::{DomainError, DomainResult, ErrorSource};
use crate::domain::model::{Upstream, gts_resource_id};
use crate::domain::validate::validate_upstream;
use axum::extract::{Extension, Path, Query};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use std::sync::Arc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Render a stored upstream for the wire.
fn to_dto(upstream: Upstream) -> UpstreamDto {
    UpstreamDto {
        id: gts_resource_id("upstream", upstream.id),
        uuid: upstream.id,
        alias: upstream.alias,
        enabled: upstream.enabled,
        server: upstream.server,
        protocol: upstream.protocol,
        tags: upstream.tags,
        auth: upstream.auth,
        headers: upstream.headers,
        plugins: upstream.plugins,
        rate_limit: upstream.rate_limit,
        cors: upstream.cors,
    }
}

/// Build a JSON response carrying the gateway error-source header.
fn json_ok<T: serde::Serialize>(status: StatusCode, body: &T) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    set_error_source(&mut response, ErrorSource::Gateway);
    response
}

/// Create an upstream.
///
/// # Errors
/// Returns a validation error on a malformed body and a conflict when the
/// tenant already owns that alias.
pub async fn create(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    axum::Json(body): axum::Json<UpstreamWriteDto>,
) -> Result<Response, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let alias = resolve_create_alias(&body.server.endpoints, body.alias.as_deref())?;
    let upstream = Upstream {
        id: Uuid::new_v4(),
        tenant_id,
        alias,
        enabled: body.enabled,
        server: body.server,
        protocol: body.protocol,
        tags: body.tags,
        auth: body.auth,
        headers: body.headers,
        plugins: body.plugins,
        rate_limit: body.rate_limit,
        cors: body.cors,
    };
    validate_upstream(&upstream)?;
    let created = state.store.create_upstream(upstream)?;
    Ok(json_ok(StatusCode::CREATED, &to_dto(created)))
}

/// List the tenant's upstreams.
///
/// # Errors
/// Returns a validation error when a query parameter is malformed.
pub async fn list(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, DomainError> {
    validate_list_query(&query)?;
    let mut items: Vec<UpstreamDto> = state
        .store
        .list_upstreams(ctx.subject_tenant_id())
        .into_iter()
        .map(to_dto)
        .collect();
    items.sort_by(|a, b| a.alias.cmp(&b.alias));
    let page: Vec<UpstreamDto> = items
        .into_iter()
        .skip(query.effective_skip())
        .take(query.effective_top())
        .collect();
    let count = page.len();
    Ok(json_ok(
        StatusCode::OK,
        &UpstreamListDto { items: page, count },
    ))
}

/// Reject a malformed list query.
///
/// # Errors
/// Returns a validation error when the filter expression cannot be parsed.
pub fn validate_list_query(query: &ListQuery) -> DomainResult<()> {
    // A filter is an OData boolean expression; an unparseable one is a
    // client error rather than a silently ignored parameter.
    if let Some(filter) = &query.filter
        && !filter.contains(' ')
    {
        return Err(DomainError::validation(format!(
            "$filter `{filter}` is not a valid OData expression"
        )));
    }
    Ok(())
}

/// Fetch one upstream.
///
/// # Errors
/// Returns not-found when the tenant does not own an upstream with that id.
pub async fn get(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    let found = state
        .store
        .get_upstream(ctx.subject_tenant_id(), id)
        .ok_or_else(|| DomainError::not_found("upstream not found"))?;
    Ok(json_ok(StatusCode::OK, &to_dto(found)))
}

/// Replace an upstream.
///
/// # Errors
/// Returns not-found when the upstream does not exist for the tenant, and a
/// validation error when the replacement would change the alias.
pub async fn replace(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<UpstreamWriteDto>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    let tenant_id = ctx.subject_tenant_id();
    let existing = state
        .store
        .get_upstream(tenant_id, id)
        .ok_or_else(|| DomainError::not_found("upstream not found"))?;
    let alias = enforce_alias_update(
        &existing.alias,
        &body.server.endpoints,
        body.alias.as_deref(),
    )?;
    let replacement = Upstream {
        id,
        tenant_id,
        alias,
        // A full replacement overwrites every field, so an omitted `enabled`
        // returns to the schema default rather than keeping the stored value.
        enabled: body.enabled,
        server: body.server,
        protocol: body.protocol,
        tags: body.tags,
        auth: body.auth,
        headers: body.headers,
        plugins: body.plugins,
        rate_limit: body.rate_limit,
        cors: body.cors,
    };
    validate_upstream(&replacement)?;
    let stored = state.store.replace_upstream(replacement)?;
    Ok(json_ok(StatusCode::OK, &to_dto(stored)))
}

/// Delete an upstream and its routes.
///
/// # Errors
/// Returns not-found when the tenant does not own an upstream with that id.
pub async fn delete(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    if state.store.delete_upstream(ctx.subject_tenant_id(), id) {
        let mut response = StatusCode::NO_CONTENT.into_response();
        set_error_source(&mut response, ErrorSource::Gateway);
        Ok(response)
    } else {
        Err(DomainError::not_found("upstream not found"))
    }
}

/// Parse a path identifier, accepting a bare identifier or the anonymous
/// global type system form.
///
/// # Errors
/// Returns not-found when the value is not an identifier, so a malformed path
/// is indistinguishable from an absent resource and cannot be used to probe.
pub fn parse_id(raw: &str) -> DomainResult<Uuid> {
    let instance = crate::domain::model::gts_instance(raw);
    Uuid::parse_str(instance).map_err(|_| DomainError::not_found("resource not found"))
}
// @cpt-end:cpt-cf-oagw-dod-upstream-api-create:p1:inst-upstream-handlers

#[cfg(test)]
mod tests {
    use super::{parse_id, validate_list_query};
    use crate::api::rest::dto::ListQuery;
    use uuid::Uuid;

    #[test]
    fn identifiers_parse_in_bare_and_prefixed_form() {
        let id = Uuid::new_v4();
        assert_eq!(parse_id(&id.to_string()).expect("bare"), id);
        let prefixed = format!("gts.cf.core.oagw.upstream.v1~{id}");
        assert_eq!(parse_id(&prefixed).expect("prefixed"), id);
    }

    #[test]
    fn a_malformed_identifier_reads_as_not_found() {
        let err = parse_id("not-a-uuid").expect_err("rejected");
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn an_unparseable_filter_is_rejected() {
        let query = ListQuery {
            filter: Some("garbage".to_owned()),
            ..ListQuery::default()
        };
        assert!(validate_list_query(&query).is_err());

        let ok = ListQuery {
            filter: Some("alias eq 'x'".to_owned()),
            ..ListQuery::default()
        };
        assert!(validate_list_query(&ok).is_ok());
    }
}
