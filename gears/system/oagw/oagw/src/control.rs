// Created: 2026-09-03 by Constructor Tech
//! Control-plane services: CRUD orchestration for upstreams, routes and
//! plugins.
//!
//! Create-time validation enforces the endpoint scheme allowlist, hostname
//! syntax, pool homogeneity, alias derivation, plugin-reference
//! resolvability and the rate-limit / CORS invariants documented in
//! `DESIGN.md` §3.3.

use std::sync::Arc;

use uuid::Uuid;

use crate::error::{ErrorKind, OagwError};
use crate::gts;
use crate::model::{AuthConfig, CorsConfig, PluginsConfig, RateLimitConfig, Route, RouteInput, Upstream, UpstreamInput};
use crate::state::OagwState;

/// Creates an upstream.
///
/// # Errors
/// Returns 400 `ValidationError` for any malformed input and 409
/// `AliasConflict` when the alias is taken.
pub async fn create_upstream(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    input: UpstreamInput,
) -> Result<Upstream, OagwError> {
    let tenant = sec.subject_tenant_id();
    let endpoints = crate::model::validate_endpoints(&input.server.endpoints, state.validation_context())?;
    validate_protocol(&input.protocol)?;
    validate_tags(input.tags.as_ref())?;
    validate_sections(state, sec, input.auth.as_ref(), input.plugins.as_ref(), input.rate_limit.as_ref(), input.cors.as_ref())?;
    let alias = crate::alias::enforce_alias_create(input.alias.as_deref(), &endpoints)?;
    let now = crate::model::now_millis();
    let upstream = Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias,
        enabled: input.enabled,
        tags: input.tags.unwrap_or_default(),
        server: crate::model::ServerConfig { endpoints },
        protocol: canonical_protocol(&input.protocol),
        auth: input.auth,
        headers: input.headers,
        plugins: input.plugins,
        rate_limit: input.rate_limit,
        cors: input.cors,
        created_at: now,
        updated_at: now,
    };
    let stored = state.store.insert_upstream(upstream)?;
    Ok((*stored).clone())
}

/// Replaces an upstream.
///
/// # Errors
/// Returns 400 `ValidationError` for malformed input or an alias override,
/// and 404 when the upstream does not exist.
pub async fn update_upstream(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    id: Uuid,
    input: UpstreamInput,
) -> Result<Upstream, OagwError> {
    let tenant = sec.subject_tenant_id();
    let existing = state
        .store
        .get_upstream(tenant, id)
        .ok_or_else(|| OagwError::new(ErrorKind::ResourceNotFound, "upstream not found"))?;
    let endpoints = crate::model::validate_endpoints(&input.server.endpoints, state.validation_context())?;
    validate_protocol(&input.protocol)?;
    validate_tags(input.tags.as_ref())?;
    validate_sections(state, sec, input.auth.as_ref(), input.plugins.as_ref(), input.rate_limit.as_ref(), input.cors.as_ref())?;
    crate::alias::enforce_alias_update(
        &existing.alias,
        input.alias.as_deref(),
        &existing.server.endpoints,
        &endpoints,
    )?;
    let upstream = Upstream {
        id,
        tenant_id: tenant,
        alias: existing.alias.clone(),
        enabled: input.enabled,
        tags: input.tags.unwrap_or_default(),
        server: crate::model::ServerConfig { endpoints },
        protocol: canonical_protocol(&input.protocol),
        auth: input.auth,
        headers: input.headers,
        plugins: input.plugins,
        rate_limit: input.rate_limit,
        cors: input.cors,
        created_at: existing.created_at,
        updated_at: crate::model::now_millis(),
    };
    let stored = state.store.replace_upstream(upstream)?;
    Ok((*stored).clone())
}

/// Fetches an upstream.
///
/// # Errors
/// Returns 404 when the upstream does not exist for the tenant.
pub fn get_upstream(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    id: Uuid,
) -> Result<Upstream, OagwError> {
    state
        .store
        .get_upstream(sec.subject_tenant_id(), id)
        .map(|record| (*record).clone())
        .ok_or_else(|| OagwError::new(ErrorKind::ResourceNotFound, "upstream not found"))
}

/// Lists the upstreams of the calling tenant.
#[must_use]
pub fn list_upstreams(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
) -> Vec<Upstream> {
    state
        .store
        .list_upstreams(sec.subject_tenant_id())
        .into_iter()
        .map(|record| (*record).clone())
        .collect()
}

/// Deletes an upstream.
///
/// # Errors
/// Returns 404 when the upstream does not exist and 409 when routes still
/// reference it.
pub fn delete_upstream(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    id: Uuid,
) -> Result<Upstream, OagwError> {
    state
        .store
        .delete_upstream(sec.subject_tenant_id(), id)
        .map(|record| (*record).clone())
}

/// Creates a route.
///
/// # Errors
/// Returns 400 `ValidationError` for malformed input and 404 when the
/// referenced upstream does not exist.
pub async fn create_route(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    input: RouteInput,
) -> Result<Route, OagwError> {
    let tenant = sec.subject_tenant_id();
    let upstream_id = input.upstream_id.ok_or_else(|| {
        OagwError::new(ErrorKind::Validation, "upstream_id is required to create a route")
    })?;
    let route_match = input.r#match.ok_or_else(|| {
        OagwError::new(ErrorKind::Validation, "match is required to create a route")
    })?;
    validate_route_match(&route_match)?;
    let upstream = state
        .store
        .get_upstream(tenant, upstream_id)
        .ok_or_else(|| OagwError::new(ErrorKind::UpstreamNotFound, "upstream not found"))?;
    validate_chain(state, sec, input.plugins.as_ref())?;
    validate_limit_and_cors(input.rate_limit.as_ref(), input.cors.as_ref())?;
    validate_tags(input.tags.as_ref())?;
    let now = crate::model::now_millis();
    let route = Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id: upstream.id,
        enabled: input.enabled,
        tags: input.tags.unwrap_or_default(),
        r#match: route_match,
        plugins: input.plugins,
        rate_limit: input.rate_limit,
        cors: input.cors,
        created_at: now,
        updated_at: now,
    };
    let stored = state.store.insert_route(route)?;
    Ok((*stored).clone())
}

/// Replaces a route.
///
/// # Errors
/// Returns 400 `ValidationError` for malformed input and 404 when the route
/// does not exist.
pub async fn update_route(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    id: Uuid,
    input: RouteInput,
) -> Result<Route, OagwError> {
    let tenant = sec.subject_tenant_id();
    let existing = state
        .store
        .get_route(tenant, id)
        .ok_or_else(|| OagwError::new(ErrorKind::ResourceNotFound, "route not found"))?;
    let route_match = input
        .r#match
        .ok_or_else(|| OagwError::new(ErrorKind::Validation, "match is required to replace a route"))?;
    if let Some(repointed) = input.upstream_id
        && repointed != existing.upstream_id {
            return Err(OagwError::new(
                ErrorKind::RouteConflict,
                "upstream_id is immutable on a route; delete and re-create the route",
            ));
        }
    validate_route_match(&route_match)?;
    validate_chain(state, sec, input.plugins.as_ref())?;
    validate_limit_and_cors(input.rate_limit.as_ref(), input.cors.as_ref())?;
    validate_tags(input.tags.as_ref())?;
    let route = Route {
        id,
        tenant_id: tenant,
        upstream_id: existing.upstream_id,
        enabled: input.enabled,
        tags: input.tags.unwrap_or_default(),
        r#match: route_match,
        plugins: input.plugins,
        rate_limit: input.rate_limit,
        cors: input.cors,
        created_at: existing.created_at,
        updated_at: crate::model::now_millis(),
    };
    let stored = state.store.replace_route(route)?;
    Ok((*stored).clone())
}

/// Fetches a route.
///
/// # Errors
/// Returns 404 when the route does not exist for the tenant.
pub fn get_route(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    id: Uuid,
) -> Result<Route, OagwError> {
    state
        .store
        .get_route(sec.subject_tenant_id(), id)
        .map(|record| (*record).clone())
        .ok_or_else(|| OagwError::new(ErrorKind::ResourceNotFound, "route not found"))
}

/// Lists the routes of the calling tenant.
#[must_use]
pub fn list_routes(state: &Arc<OagwState>, sec: &toolkit_security::SecurityContext) -> Vec<Route> {
    state
        .store
        .list_routes(sec.subject_tenant_id())
        .into_iter()
        .map(|record| (*record).clone())
        .collect()
}

/// Deletes a route.
///
/// # Errors
/// Returns 404 when the route does not exist.
pub fn delete_route(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    id: Uuid,
) -> Result<Route, OagwError> {
    state
        .store
        .delete_route(sec.subject_tenant_id(), id)
        .map(|record| (*record).clone())
}

/// Creates a custom plugin definition.
///
/// # Errors
/// Returns 409 when the name is already registered for the tenant.
pub async fn create_plugin(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    name: String,
    plugin_type: crate::model::PluginType,
    config: Option<serde_json::Value>,
) -> Result<crate::model::PluginRecord, OagwError> {
    let now = crate::model::now_millis();
    let gc_eligible_at = i64::try_from(state.config.plugin_gc_ttl_secs.saturating_mul(1000))
        .ok()
        .map(|offset| now + offset);
    let record = crate::model::PluginRecord {
        id: Uuid::new_v4(),
        tenant_id: sec.subject_tenant_id(),
        name,
        plugin_type,
        config,
        created_at: now,
        last_used_at: None,
        gc_eligible_at,
    };
    let stored = state.store.insert_plugin(record)?;
    Ok((*stored).clone())
}

/// Fetches a custom plugin.
///
/// # Errors
/// Returns 404 when the plugin does not exist.
pub fn get_plugin(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    id: Uuid,
) -> Result<crate::model::PluginRecord, OagwError> {
    state
        .store
        .get_plugin(sec.subject_tenant_id(), id)
        .map(|record| (*record).clone())
        .ok_or_else(|| OagwError::new(ErrorKind::PluginNotFound, "plugin not found"))
}

/// Lists the custom plugins of the calling tenant.
#[must_use]
pub fn list_plugins(state: &Arc<OagwState>, sec: &toolkit_security::SecurityContext) -> Vec<crate::model::PluginRecord> {
    state
        .store
        .list_plugins(sec.subject_tenant_id())
        .into_iter()
        .map(|record| (*record).clone())
        .collect()
}

/// Deletes a custom plugin.
///
/// # Errors
/// Returns 404 when the plugin does not exist and 409 `PluginInUse` when it
/// is still referenced.
pub fn delete_plugin(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    id: Uuid,
) -> Result<crate::model::PluginRecord, OagwError> {
    state
        .store
        .delete_plugin(sec.subject_tenant_id(), id)
        .map(|record| (*record).clone())
}

fn validate_protocol(protocol: &str) -> Result<(), OagwError> {
    let normalized = canonical_protocol(protocol);
    if normalized == gts::PROTOCOL_HTTP || normalized == gts::PROTOCOL_GRPC {
        Ok(())
    } else {
        Err(OagwError::new(
            ErrorKind::Validation,
            format!(
                "protocol must be '{http}' or '{grpc}' (the short names 'http' and 'grpc' are accepted)",
                http = gts::PROTOCOL_HTTP,
                grpc = gts::PROTOCOL_GRPC
            ),
        ))
    }
}

/// Canonicalises a protocol selector to its GTS identifier.
///
/// The schema declares the GTS form; the bare short names are tolerated as an
/// ergonomic alias so callers never have to spell the identifier out.
#[must_use]
fn canonical_protocol(protocol: &str) -> String {
    let trimmed = protocol.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "http" | "https" => gts::PROTOCOL_HTTP.to_owned(),
        "grpc" | "grpc-web" => gts::PROTOCOL_GRPC.to_owned(),
        _ => trimmed.to_owned(),
    }
}

fn validate_tags(tags: Option<&Vec<String>>) -> Result<(), OagwError> {
    match tags {
        Some(tags) => crate::model::validate_tags(tags),
        None => Ok(()),
    }
}

fn validate_sections(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    auth: Option<&AuthConfig>,
    plugins: Option<&PluginsConfig>,
    rate_limit: Option<&RateLimitConfig>,
    cors: Option<&CorsConfig>,
) -> Result<(), OagwError> {
    if let Some(auth) = auth {
        crate::plugins::validate_auth(auth)?;
    }
    validate_chain(state, sec, plugins)?;
    validate_limit_and_cors(rate_limit, cors)
}

fn validate_chain(
    state: &Arc<OagwState>,
    sec: &toolkit_security::SecurityContext,
    plugins: Option<&PluginsConfig>,
) -> Result<(), OagwError> {
    let Some(config) = plugins else {
        return Ok(());
    };
    for item in &config.items {
        let reference = item.plugin_ref();
        let custom = crate::plugins::custom_reference(reference)
            .and_then(|id| state.store.get_plugin(sec.subject_tenant_id(), id));
        crate::plugins::validate_reference_any(reference, custom.as_deref())?;
    }
    Ok(())
}

fn validate_limit_and_cors(
    rate_limit: Option<&RateLimitConfig>,
    cors: Option<&CorsConfig>,
) -> Result<(), OagwError> {
    if let Some(limit) = rate_limit {
        crate::model::validate_rate_limit(limit)?;
    }
    if let Some(cors) = cors {
        crate::model::validate_cors(cors)?;
    }
    Ok(())
}

fn validate_route_match(route_match: &crate::model::RouteMatch) -> Result<(), OagwError> {
    let http_present = route_match.http.is_some();
    let grpc_present = route_match.grpc.is_some();
    if http_present == grpc_present {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "match must declare exactly one of http or grpc",
        ));
    }
    if let Some(http) = &route_match.http {
        if http.methods.is_empty() {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "match.http.methods must not be empty",
            ));
        }
        if http.path.is_empty() || !http.path.starts_with('/') {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "match.http.path must be an absolute path",
            ));
        }
    }
    if let Some(grpc) = &route_match.grpc
        && (grpc.service.is_empty() || grpc.method.is_empty()) {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "match.grpc.service and match.grpc.method are required",
            ));
        }
    Ok(())
}
