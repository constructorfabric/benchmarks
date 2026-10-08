//! OAGW provisioning at gear start (D "OAGW provisioning", §3.5, ADR-0005).
//!
//! For every provider entry and tenant override one upstream is registered
//! under its alias (the alias `MiniChatConfig::validate` computed with OAGW's
//! own rule) with a chat route derived from `api_path` and best-effort RAG
//! routes. When OAGW reports the upstream or a route as existing, the
//! existing object is reused and brought in line with the configuration
//! (upstream endpoint and auth replaced; a route that matches less than
//! required is widened).
//! A deterministically misconfigured entry fails provisioning; an entry that
//! hit a transient failure (credstore secret not readable yet, OAGW
//! unavailable — also for a RAG route) is reported as deferred and retried
//! by [`reconcile_deferred`].

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::bail;
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PathSuffixMode, Route, Scheme, Server,
    ServiceGatewayClientV1, ServiceGatewayError, SharingMode, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderEntry, StorageKind};
use crate::infra::s2s::S2sContextProvider;

/// One route to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    pub method: HttpMethod,
    pub path: String,
    pub suffix: PathSuffixMode,
    pub query_allowlist: Vec<String>,
}

/// One upstream to register (a provider entry or a tenant override).
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamSpec {
    /// Provider id, or `"{provider_id}@{tenant}"` for a tenant override.
    pub label: String,
    pub alias: String,
    pub endpoint: Endpoint,
    pub auth: Option<AuthConfig>,
    pub chat_route: RouteSpec,
    /// Best effort: failures only degrade RAG.
    pub rag_routes: Vec<RouteSpec>,
}

/// Outcome of a provisioning pass (labels of [`UpstreamSpec`]s).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionReport {
    pub ok: Vec<String>,
    /// Entries with a transient failure (retry later). The chat route of an
    /// entry deferred only for a RAG route is already live.
    pub deferred: Vec<String>,
}

/// Upstream specs of all provider entries and tenant overrides.
///
/// Specs sharing an alias (D B.1: OAGW creates or reuses the upstream under
/// the alias) all carry the endpoint and auth of the first one in order
/// (provider ids ascending, each entry before its tenant overrides; see
/// `config::shared_alias_conflicts`), so every provisioning pass converges on
/// one upstream definition instead of rewriting it back and forth. Each spec
/// keeps its own routes.
#[must_use]
pub fn plan(providers: &BTreeMap<String, ProviderEntry>) -> Vec<UpstreamSpec> {
    let mut specs = Vec::new();
    for (id, p) in providers {
        let chat_route = chat_route(&p.api_path);
        let rag_routes = rag_routes(p.storage_kind);
        let spec = |label: String, alias: &str, host: &str, plugin: Option<&String>, config| {
            UpstreamSpec {
                label,
                alias: alias.to_owned(),
                endpoint: Endpoint {
                    scheme: if p.use_http {
                        Scheme::Http
                    } else {
                        Scheme::Https
                    },
                    host: host.to_owned(),
                    port: p.effective_port(),
                },
                auth: auth(plugin, config),
                chat_route: chat_route.clone(),
                rag_routes: rag_routes.clone(),
            }
        };
        let alias = p.upstream_alias.as_deref().unwrap_or(&p.host);
        specs.push(spec(
            id.clone(),
            alias,
            &p.host,
            p.auth_plugin_type.as_ref(),
            p.auth_config.as_ref(),
        ));
        for (tenant, o) in &p.tenant_overrides {
            let host = o.host.as_deref().unwrap_or(&p.host);
            specs.push(spec(
                format!("{id}@{tenant}"),
                o.upstream_alias.as_deref().unwrap_or(host),
                host,
                o.auth_plugin_type.as_ref().or(p.auth_plugin_type.as_ref()),
                o.auth_config.as_ref().or(p.auth_config.as_ref()),
            ));
        }
    }
    let mut owners: BTreeMap<String, (Endpoint, Option<AuthConfig>)> = BTreeMap::new();
    for spec in &mut specs {
        let (endpoint, auth) = owners
            .entry(spec.alias.to_ascii_lowercase())
            .or_insert_with(|| (spec.endpoint.clone(), spec.auth.clone()));
        spec.endpoint.clone_from(endpoint);
        spec.auth.clone_from(auth);
    }
    specs
}

fn auth(plugin: Option<&String>, config: Option<&BTreeMap<String, String>>) -> Option<AuthConfig> {
    plugin.map(|plugin_type| AuthConfig {
        plugin_type: plugin_type.clone(),
        sharing: SharingMode::Private,
        config: config.map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
    })
}

/// Chat route: `POST` on `api_path` without its query; a `{model}`
/// placeholder makes the part before it a prefix with the rest as path
/// suffix; the query parameter names of `api_path` are allowed.
fn chat_route(api_path: &str) -> RouteSpec {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let query_allowlist = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| kv.split_once('=').map_or(kv, |(k, _)| k).to_owned())
        .collect();
    let (path, suffix) = match path.find("{model}") {
        Some(idx) => (&path[..idx], PathSuffixMode::Append),
        None => (path, PathSuffixMode::Disabled),
    };
    RouteSpec {
        method: HttpMethod::Post,
        path: path.to_owned(),
        suffix,
        query_allowlist,
    }
}

/// File and vector-store routes (S§9.3): prefix `/v1` (`openai`) or
/// `/openai` with the `api-version` query parameter (`azure`).
fn rag_routes(kind: StorageKind) -> Vec<RouteSpec> {
    let (prefix, query): (&str, Vec<String>) = match kind {
        StorageKind::Openai => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    let route = |method, path: &str, suffix| RouteSpec {
        method,
        path: format!("{prefix}{path}"),
        suffix,
        query_allowlist: query.clone(),
    };
    vec![
        route(HttpMethod::Post, "/files", PathSuffixMode::Disabled),
        route(HttpMethod::Delete, "/files/", PathSuffixMode::Append),
        route(HttpMethod::Post, "/vector_stores", PathSuffixMode::Append),
        route(
            HttpMethod::Delete,
            "/vector_stores/",
            PathSuffixMode::Append,
        ),
        route(HttpMethod::Get, "/vector_stores/", PathSuffixMode::Append),
    ]
}

/// Provision every entry of `cfg.providers`.
///
/// # Errors
/// A deterministic misconfiguration of an entry.
pub async fn provision_all(
    gw: &dyn ServiceGatewayClientV1,
    s2s_ctx: &SecurityContext,
    cfg: &MiniChatConfig,
) -> anyhow::Result<ProvisionReport> {
    provision(gw, s2s_ctx, &plan(&cfg.providers)).await
}

/// Provision `specs`.
///
/// # Errors
/// A deterministic misconfiguration of an entry.
pub async fn provision(
    gw: &dyn ServiceGatewayClientV1,
    s2s_ctx: &SecurityContext,
    specs: &[UpstreamSpec],
) -> anyhow::Result<ProvisionReport> {
    let mut report = ProvisionReport::default();
    for spec in specs {
        match provision_one(gw, s2s_ctx, spec).await? {
            Outcome::Ok => report.ok.push(spec.label.clone()),
            Outcome::Deferred => report.deferred.push(spec.label.clone()),
        }
    }
    Ok(report)
}

enum Outcome {
    Ok,
    Deferred,
}

/// Errors that may clear without a config change (credstore secret not
/// readable yet, OAGW / credstore temporarily unavailable).
fn is_transient(err: &CanonicalError) -> bool {
    matches!(
        err,
        CanonicalError::FailedPrecondition { .. }
            | CanonicalError::ServiceUnavailable { .. }
            | CanonicalError::Internal { .. }
            | CanonicalError::DeadlineExceeded { .. }
            | CanonicalError::Aborted { .. }
            | CanonicalError::ResourceExhausted { .. }
            | CanonicalError::Unknown { .. }
    )
}

async fn provision_one(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    spec: &UpstreamSpec,
) -> anyhow::Result<Outcome> {
    let Some(upstream_id) = ensure_upstream(gw, ctx, spec).await? else {
        return Ok(Outcome::Deferred);
    };
    match ensure_route(gw, ctx, upstream_id, &spec.chat_route).await {
        Ok(()) => {}
        Err(e) if is_transient(&e) => return Ok(deferred(spec, &e)),
        Err(e) => bail!(
            "provider {}: OAGW rejected chat route {:?}: {}",
            spec.label,
            spec.chat_route.path,
            describe(&e)
        ),
    }
    let mut rag_pending = false;
    for route in &spec.rag_routes {
        if let Err(e) = ensure_route(gw, ctx, upstream_id, route).await {
            rag_pending |= is_transient(&e);
            warn!(
                provider = %spec.label,
                path = %route.path,
                retried = is_transient(&e),
                error = %describe(&e),
                "OAGW RAG route not provisioned; file and vector-store operations are degraded"
            );
        }
    }
    if rag_pending {
        // The chat route is live; the entry stays pending so the reconcile
        // loop retries the missing RAG routes.
        return Ok(Outcome::Deferred);
    }
    info!(provider = %spec.label, alias = %spec.alias, chat_path = %spec.chat_route.path, "OAGW routes provisioned");
    Ok(Outcome::Ok)
}

/// Create the upstream, or reuse the one OAGW already has under the alias
/// (updated to the spec's endpoint and credentials when they differ).
/// `Ok(None)` = deferred (transient failure).
async fn ensure_upstream(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    spec: &UpstreamSpec,
) -> anyhow::Result<Option<Uuid>> {
    let mut req = CreateUpstreamRequest::builder(spec_server(spec), HTTP_PROTOCOL_ID)
        .alias(spec.alias.clone());
    if let Some(auth) = &spec.auth {
        req = req.auth(auth.clone());
    }
    let err = match gw.create_upstream(ctx.clone(), req.build()).await {
        Ok(u) => {
            info!(provider = %spec.label, alias = %u.alias, "OAGW upstream created");
            return Ok(Some(u.id));
        }
        Err(CanonicalError::AlreadyExists { .. }) => match reuse_upstream(gw, ctx, spec).await {
            Ok(id) => return id.map(Some),
            Err(e) => e,
        },
        Err(e) => e,
    };
    if is_transient(&err) {
        deferred(spec, &err);
        return Ok(None);
    }
    bail!(
        "provider {}: OAGW rejected upstream {:?}: {}",
        spec.label,
        spec.alias,
        describe(&err)
    )
}

fn spec_server(spec: &UpstreamSpec) -> Server {
    Server {
        endpoints: vec![spec.endpoint.clone()],
    }
}

/// Find the existing upstream of `spec.alias` and bring its endpoint and
/// auth in line with the spec. The outer `Err` is an OAGW error to classify;
/// the inner one a deterministic failure.
async fn reuse_upstream(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    spec: &UpstreamSpec,
) -> Result<anyhow::Result<Uuid>, CanonicalError> {
    let Some(existing) = find_upstream(gw, ctx, &spec.alias).await? else {
        return Ok(Err(anyhow::anyhow!(
            "provider {}: OAGW reports upstream alias {:?} as existing but it is not visible",
            spec.label,
            spec.alias
        )));
    };
    let up_to_date = existing.server == spec_server(spec)
        && existing.auth == spec.auth
        && existing.protocol == HTTP_PROTOCOL_ID;
    if up_to_date {
        info!(provider = %spec.label, alias = %spec.alias, "OAGW upstream exists; reused");
        return Ok(Ok(existing.id));
    }
    gw.update_upstream(ctx.clone(), existing.id, update_request(spec, &existing))
        .await?;
    warn!(
        provider = %spec.label,
        alias = %spec.alias,
        "existing OAGW upstream had a different endpoint or credentials; updated to the configuration"
    );
    Ok(Ok(existing.id))
}

/// Full-replacement update to the spec's endpoint and auth, keeping what
/// mini-chat does not manage (headers, plugins, limits, CORS, tags, enabled).
fn update_request(spec: &UpstreamSpec, existing: &Upstream) -> UpdateUpstreamRequest {
    let mut req = UpdateUpstreamRequest::builder(spec_server(spec), HTTP_PROTOCOL_ID)
        .alias(existing.alias.clone())
        .tags(existing.tags.clone())
        .enabled(existing.enabled);
    if let Some(auth) = &spec.auth {
        req = req.auth(auth.clone());
    }
    if let Some(h) = &existing.headers {
        req = req.headers(h.clone());
    }
    if let Some(p) = &existing.plugins {
        req = req.plugins(p.clone());
    }
    if let Some(r) = &existing.rate_limit {
        req = req.rate_limit(r.clone());
    }
    if let Some(c) = &existing.cors {
        req = req.cors(c.clone());
    }
    req.build()
}

/// Log / error text of an OAGW error with its typed details (the canonical
/// `detail` of some categories is generic, e.g. "Operation precondition not met").
fn describe(err: &CanonicalError) -> String {
    ServiceGatewayError::from(err.clone()).to_string()
}

fn deferred(spec: &UpstreamSpec, err: &CanonicalError) -> Outcome {
    warn!(
        provider = %spec.label,
        alias = %spec.alias,
        error = %describe(err),
        "OAGW provisioning deferred (credential not readable yet or OAGW unavailable); retrying in the background"
    );
    Outcome::Deferred
}

fn route_rules(route: &RouteSpec) -> MatchRules {
    MatchRules {
        http: Some(HttpMatch {
            methods: vec![route.method],
            path: route.path.clone(),
            query_allowlist: route.query_allowlist.clone(),
            path_suffix_mode: route.suffix,
        }),
        grpc: None,
    }
}

/// Create a route. When an overlapping route exists (same path, priority and
/// method), it is reused if it matches at least what the spec needs, else
/// widened: path suffix allowed when either allows it, query allowlists
/// merged, its other methods kept.
async fn ensure_route(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    upstream_id: Uuid,
    route: &RouteSpec,
) -> Result<(), CanonicalError> {
    let req = CreateRouteRequest::builder(upstream_id, route_rules(route)).build();
    match gw.create_route(ctx.clone(), req).await {
        Ok(_) => Ok(()),
        Err(CanonicalError::AlreadyExists { .. }) => {
            reconcile_route(gw, ctx, upstream_id, route).await
        }
        Err(e) => Err(e),
    }
}

async fn reconcile_route(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    upstream_id: Uuid,
    route: &RouteSpec,
) -> Result<(), CanonicalError> {
    let routes = list_all_routes(gw, ctx, upstream_id).await?;
    let Some((existing, http)) = routes.iter().find_map(|r| {
        let h = r.match_rules.http.as_ref()?;
        (r.enabled && r.priority == 0 && h.path == route.path && h.methods.contains(&route.method))
            .then_some((r, h))
    }) else {
        // The conflicting route is not visible; nothing to reconcile.
        return Ok(());
    };
    let suffix_ok =
        route.suffix == PathSuffixMode::Disabled || http.path_suffix_mode == PathSuffixMode::Append;
    let missing: Vec<&String> = route
        .query_allowlist
        .iter()
        .filter(|q| !http.query_allowlist.contains(q))
        .collect();
    if suffix_ok && missing.is_empty() {
        return Ok(());
    }
    let mut merged = http.clone();
    if !suffix_ok {
        merged.path_suffix_mode = PathSuffixMode::Append;
    }
    merged.query_allowlist.extend(missing.into_iter().cloned());
    let mut req = UpdateRouteRequest::builder(MatchRules {
        http: Some(merged),
        grpc: existing.match_rules.grpc.clone(),
    })
    .tags(existing.tags.clone())
    .priority(existing.priority)
    .enabled(existing.enabled);
    if let Some(p) = existing.plugins.clone() {
        req = req.plugins(p);
    }
    if let Some(r) = existing.rate_limit.clone() {
        req = req.rate_limit(r);
    }
    if let Some(c) = existing.cors.clone() {
        req = req.cors(c);
    }
    gw.update_route(ctx.clone(), existing.id, req.build())
        .await?;
    warn!(path = %route.path, "existing OAGW route matched less than required; widened");
    Ok(())
}

const PAGE: u32 = 100;

async fn list_all_routes(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    upstream_id: Uuid,
) -> Result<Vec<Route>, CanonicalError> {
    let mut all = Vec::new();
    let mut skip = 0;
    loop {
        let page = gw
            .list_routes(
                ctx.clone(),
                Some(upstream_id),
                &ListQuery { top: PAGE, skip },
            )
            .await?;
        let done = page.len() < PAGE as usize;
        all.extend(page);
        if done {
            return Ok(all);
        }
        skip += PAGE;
    }
}

/// The upstream registered under `alias` (OAGW normalizes aliases to
/// lowercase).
async fn find_upstream(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    alias: &str,
) -> Result<Option<Upstream>, CanonicalError> {
    let mut skip = 0;
    loop {
        let page = gw
            .list_upstreams(ctx.clone(), &ListQuery { top: PAGE, skip })
            .await?;
        let done = page.len() < PAGE as usize;
        if let Some(u) = page
            .into_iter()
            .find(|u| u.alias.eq_ignore_ascii_case(alias))
        {
            return Ok(Some(u));
        }
        if done {
            return Ok(None);
        }
        skip += PAGE;
    }
}

/// First reconcile delay.
const RETRY_INITIAL: Duration = Duration::from_secs(2);
/// Reconcile delay cap.
const RETRY_MAX: Duration = Duration::from_secs(60);
/// Pending time after which one warning names the pending entries.
const PENDING_WARN_AFTER: Duration = Duration::from_secs(120);

/// Delay before reconcile attempt `attempt` (0-based): 2 s doubling to 60 s.
#[must_use]
pub fn retry_delay(attempt: u32) -> Duration {
    RETRY_INITIAL
        .checked_mul(1 << attempt.min(16))
        .map_or(RETRY_MAX, |d| d.min(RETRY_MAX))
}

/// Retry `pending` (2 s, doubling to 60 s) until all are provisioned or
/// `cancel` fires; the S2S context is (re-)obtained on every attempt. One
/// warning names the still-pending entries after 2 minutes. An entry that
/// fails deterministically during a retry is logged and dropped.
pub async fn reconcile_deferred(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &S2sContextProvider,
    mut pending: Vec<UpstreamSpec>,
    cancel: CancellationToken,
) {
    let started = tokio::time::Instant::now();
    let mut warned = false;
    let mut attempt = 0;
    while !pending.is_empty() {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(retry_delay(attempt)) => {}
        }
        attempt = attempt.saturating_add(1);
        pending = retry_pending(gw, s2s, pending).await;
        if !warned && !pending.is_empty() && started.elapsed() >= PENDING_WARN_AFTER {
            warned = true;
            warn_pending(&pending);
        }
    }
    info!("OAGW provisioning reconciled");
}

fn warn_pending(pending: &[UpstreamSpec]) {
    let labels: Vec<&str> = pending.iter().map(|s| s.label.as_str()).collect();
    warn!(providers = ?labels, "OAGW provisioning still pending after 2 minutes");
}

/// One reconcile attempt; returns the specs still pending.
async fn retry_pending(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &S2sContextProvider,
    pending: Vec<UpstreamSpec>,
) -> Vec<UpstreamSpec> {
    let ctx = match s2s.get().await {
        Ok(ctx) => ctx,
        Err(e) => {
            warn!(error = %e, "S2S credentials exchange failed; OAGW provisioning retried later");
            return pending;
        }
    };
    let mut still = Vec::new();
    for spec in pending {
        match provision_one(gw, &ctx, &spec).await {
            Ok(Outcome::Ok) => {}
            Ok(Outcome::Deferred) => still.push(spec),
            Err(e) => {
                error!(error = %format!("{e:#}"), "OAGW provisioning failed; provider stays unavailable");
            }
        }
    }
    still
}

#[cfg(test)]
#[path = "oagw_provisioning_tests.rs"]
mod tests;
