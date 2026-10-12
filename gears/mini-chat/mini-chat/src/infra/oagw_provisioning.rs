//! OAGW provisioning at gear start (DESIGN "OAGW provisioning", ADR-0005): one upstream per
//! provider entry (and per tenant override with its own host or alias), a chat route and best-effort
//! RAG routes.
//!
//! A deterministically misconfigured entry fails startup; an entry whose credential is not yet
//! readable is retried by a background reconcile task with a 2 s delay doubling up to 60 s.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1,
    ServiceGatewayError, SharingMode, Upstream,
};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use tracing::{debug, error, info, warn};

use super::llm::{
    is_ip_host, normalized, normalized_host, upstream_alias, upstream_alias_for_tenant,
};
use crate::config::{ProviderEntry, StorageKind};

const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const WARN_AFTER: Duration = Duration::from_secs(120);
const PAGE_SIZE: u32 = 100;

/// One OAGW upstream to provision.
#[derive(Debug, Clone)]
pub struct ProvisionTarget {
    /// Human-readable name (provider id, with the tenant for an override).
    pub label: String,
    scheme: Scheme,
    host: String,
    port: u16,
    /// The routing alias: what OAGW derives for a hostname endpoint, the explicit alias of an IP
    /// endpoint. The provider resolver routes by the same value.
    alias: String,
    /// Whether the alias is sent to OAGW: only an IP endpoint takes an explicit alias, OAGW
    /// rejects any other than the derived one for a hostname endpoint.
    explicit_alias: bool,
    auth: Option<AuthConfig>,
    api_path: String,
    storage_kind: StorageKind,
}

/// Why a target could not be provisioned.
#[derive(Debug)]
enum ProvisionError {
    /// Deterministic: retrying cannot help; startup fails.
    Misconfigured(String),
    /// Transient (for example the credential is not readable yet): retried in the background.
    Deferred(String),
}

fn auth_of(
    plugin_type: Option<&String>,
    config: Option<&HashMap<String, String>>,
) -> Option<AuthConfig> {
    plugin_type.map(|plugin_type| AuthConfig {
        plugin_type: plugin_type.clone(),
        // Inherit: users of descendant tenants resolve the upstream through the tenant chain.
        sharing: SharingMode::Inherit,
        config: config.cloned(),
    })
}

/// OAGW derives the alias of a hostname endpoint: a configured `upstream_alias` is not registered
/// and requests route by the derived alias.
fn warn_if_alias_ignored(label: &str, host: &str, configured: Option<&str>, alias: &str) {
    if let Some(configured) = configured
        && !is_ip_host(host)
        && normalized(configured) != alias
    {
        warn!(
            provider = %label, configured = %configured, alias = %alias,
            "OAGW derives the alias of a hostname endpoint; the configured upstream_alias is not registered and requests route by the derived alias"
        );
    }
}

/// The targets of the `providers` section: the entry itself and every tenant override that
/// brings its own host or alias.
#[must_use]
pub fn targets_of<S: std::hash::BuildHasher>(
    providers: &HashMap<String, ProviderEntry, S>,
) -> Vec<ProvisionTarget> {
    let mut ids: Vec<&String> = providers.keys().collect();
    ids.sort();
    let mut targets = Vec::new();
    for id in ids {
        let entry = &providers[id];
        let scheme = if entry.use_http {
            Scheme::Http
        } else {
            Scheme::Https
        };
        let alias = upstream_alias(entry);
        warn_if_alias_ignored(id, &entry.host, entry.upstream_alias.as_deref(), &alias);
        targets.push(ProvisionTarget {
            label: id.clone(),
            scheme,
            host: entry.host.clone(),
            port: entry.effective_port(),
            alias,
            explicit_alias: is_ip_host(&entry.host),
            auth: auth_of(entry.auth_plugin_type.as_ref(), entry.auth_config.as_ref()),
            api_path: entry.api_path.clone(),
            storage_kind: entry.storage_kind,
        });
        let mut tenants: Vec<&String> = entry.tenant_overrides.keys().collect();
        tenants.sort();
        for tenant in tenants {
            let over = &entry.tenant_overrides[tenant];
            if over.host.is_none() && over.upstream_alias.is_none() {
                continue;
            }
            let host = over.host.clone().unwrap_or_else(|| entry.host.clone());
            let explicit_alias = is_ip_host(&host);
            let label = format!("{id} (tenant {tenant})");
            let alias = upstream_alias_for_tenant(entry, Some(over));
            warn_if_alias_ignored(&label, &host, over.upstream_alias.as_deref(), &alias);
            targets.push(ProvisionTarget {
                label,
                scheme,
                host,
                port: entry.effective_port(),
                alias,
                explicit_alias,
                auth: auth_of(
                    over.auth_plugin_type
                        .as_ref()
                        .or(entry.auth_plugin_type.as_ref()),
                    over.auth_config.as_ref().or(entry.auth_config.as_ref()),
                ),
                api_path: entry.api_path.clone(),
                storage_kind: entry.storage_kind,
            });
        }
    }
    targets
}

/// The route path prefix and the query allowlist of a chat `api_path`; the `{model}` placeholder
/// and everything after it are covered by the path suffix.
fn chat_route_match(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = match path.find("{model}") {
        Some(index) => path[..index].trim_end_matches('/'),
        None => path,
    };
    let allowlist = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split('=').next().unwrap_or(pair).to_owned())
        .collect();
    (
        if prefix.is_empty() {
            "/".to_owned()
        } else {
            prefix.to_owned()
        },
        allowlist,
    )
}

fn route_request(
    upstream_id: uuid::Uuid,
    methods: Vec<HttpMethod>,
    path: String,
    query_allowlist: Vec<String>,
) -> CreateRouteRequest {
    CreateRouteRequest::builder(
        upstream_id,
        MatchRules {
            http: Some(HttpMatch {
                methods,
                path,
                query_allowlist,
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
    )
    .build()
}

fn classify(err: toolkit_canonical_errors::CanonicalError) -> ServiceGatewayError {
    ServiceGatewayError::from(err)
}

fn failure(label: &str, what: &str, err: ServiceGatewayError) -> ProvisionError {
    match err {
        ServiceGatewayError::FailedPrecondition {
            subject, detail, ..
        } if subject.contains("secret_ref") => {
            ProvisionError::Deferred(format!("{label}: {what}: {detail}"))
        }
        ServiceGatewayError::Unavailable { .. }
        | ServiceGatewayError::Timeout
        | ServiceGatewayError::RateLimited { .. }
        | ServiceGatewayError::Internal { .. } => {
            ProvisionError::Deferred(format!("{label}: {what}: {err}"))
        }
        other => ProvisionError::Misconfigured(format!("{label}: {what}: {other}")),
    }
}

async fn find_upstream(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    alias: &str,
) -> Result<Option<Upstream>, ServiceGatewayError> {
    let mut skip = 0;
    loop {
        let page = gateway
            .list_upstreams(
                ctx.clone(),
                &ListQuery {
                    top: PAGE_SIZE,
                    skip,
                },
            )
            .await
            .map_err(classify)?;
        let page_len = u32::try_from(page.len()).unwrap_or(u32::MAX);
        if let Some(found) = page
            .into_iter()
            .find(|upstream| normalized(&upstream.alias) == alias)
        {
            return Ok(Some(found));
        }
        if page_len < PAGE_SIZE {
            return Ok(None);
        }
        skip += PAGE_SIZE;
    }
}

/// The create request of a target. The alias is sent only for an IP endpoint: OAGW derives the
/// alias of a hostname endpoint itself and rejects any other.
fn upstream_request(target: &ProvisionTarget) -> CreateUpstreamRequest {
    let mut builder = CreateUpstreamRequest::builder(
        Server {
            endpoints: vec![Endpoint {
                scheme: target.scheme,
                host: target.host.clone(),
                port: target.port,
            }],
        },
        HTTP_PROTOCOL_ID,
    );
    if target.explicit_alias {
        builder = builder.alias(target.alias.clone());
    }
    if let Some(auth) = &target.auth {
        builder = builder.auth(auth.clone());
    }
    builder.build()
}

/// The resolver routes by `target.alias`; OAGW must have registered exactly that.
fn warn_on_alias_mismatch(target: &ProvisionTarget, registered: &str) {
    if normalized(registered) != target.alias {
        warn!(
            provider = %target.label, routing_alias = %target.alias, oagw_alias = %registered,
            "OAGW registered a different alias than the one requests route by"
        );
    }
}

/// An upstream found under the target's alias must point at the target's endpoint: reusing one
/// that does not would route the target's requests (and credential) to another host.
fn check_same_endpoint(
    target: &ProvisionTarget,
    existing: &Upstream,
) -> Result<(), ProvisionError> {
    let wanted_host = normalized_host(&target.host);
    let matches = existing.server.endpoints.len() == 1
        && existing.server.endpoints.iter().all(|endpoint| {
            endpoint.scheme == target.scheme
                && endpoint.port == target.port
                && normalized_host(&endpoint.host) == wanted_host
        });
    if matches {
        return Ok(());
    }
    let registered: Vec<String> = existing
        .server
        .endpoints
        .iter()
        .map(|endpoint| {
            format!(
                "{:?}://{}:{}",
                endpoint.scheme, endpoint.host, endpoint.port
            )
        })
        .collect();
    Err(ProvisionError::Misconfigured(format!(
        "{}: the OAGW upstream `{}` already exists with the endpoint {registered:?}, not {:?}://{}:{}; \
         use a different host or upstream_alias",
        target.label, target.alias, target.scheme, target.host, target.port
    )))
}

async fn ensure_upstream(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    target: &ProvisionTarget,
) -> Result<uuid::Uuid, ProvisionError> {
    match gateway
        .create_upstream(ctx.clone(), upstream_request(target))
        .await
    {
        Ok(upstream) => {
            warn_on_alias_mismatch(target, &upstream.alias);
            Ok(upstream.id)
        }
        Err(err) => match classify(err) {
            ServiceGatewayError::AlreadyExists { .. } => {
                match find_upstream(gateway, ctx, &target.alias).await {
                    Ok(Some(existing)) => {
                        check_same_endpoint(target, &existing)?;
                        debug!(provider = %target.label, alias = %target.alias, "reusing the existing OAGW upstream");
                        Ok(existing.id)
                    }
                    Ok(None) => Err(ProvisionError::Deferred(format!(
                        "{}: the upstream exists but cannot be listed yet",
                        target.label
                    ))),
                    Err(err) => Err(failure(&target.label, "listing upstreams", err)),
                }
            }
            other => Err(failure(&target.label, "creating the upstream", other)),
        },
    }
}

async fn ensure_route(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    label: &str,
    request: CreateRouteRequest,
) -> Result<(), ProvisionError> {
    match gateway.create_route(ctx.clone(), request).await {
        Ok(_) => Ok(()),
        Err(err) => match classify(err) {
            ServiceGatewayError::AlreadyExists { .. } => Ok(()),
            other => Err(failure(label, "creating a route", other)),
        },
    }
}

async fn provision_target(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    target: &ProvisionTarget,
) -> Result<(), ProvisionError> {
    let upstream_id = ensure_upstream(gateway, ctx, target).await?;
    let (path, allowlist) = chat_route_match(&target.api_path);
    ensure_route(
        gateway,
        ctx,
        &target.label,
        route_request(upstream_id, vec![HttpMethod::Post], path, allowlist),
    )
    .await?;
    provision_rag_routes(gateway, ctx, target, upstream_id).await;
    info!(provider = %target.label, alias = %target.alias, "OAGW upstream provisioned");
    Ok(())
}

/// RAG routes are best effort: a missing route only degrades RAG.
async fn provision_rag_routes(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    target: &ProvisionTarget,
    upstream_id: uuid::Uuid,
) {
    let (prefix, allowlist) = match target.storage_kind {
        StorageKind::Openai => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    for (suffix, methods) in [
        ("/files", vec![HttpMethod::Post, HttpMethod::Delete]),
        (
            "/vector_stores",
            vec![HttpMethod::Post, HttpMethod::Delete, HttpMethod::Get],
        ),
    ] {
        let request = route_request(
            upstream_id,
            methods,
            format!("{prefix}{suffix}"),
            allowlist.clone(),
        );
        if let Err(err) = ensure_route(gateway, ctx, &target.label, request).await {
            warn!(provider = %target.label, route = suffix, error = ?err, "a RAG route could not be provisioned");
        }
    }
}

/// Rejects targets that share an alias but not their endpoint or credentials.
///
/// OAGW derives the alias of a hostname endpoint, so a tenant override on the provider's host (or
/// any second target on the same host and port) resolves to the same alias as the provider entry:
/// OAGW would answer `AlreadyExists` and the target would silently be served through the other
/// target's upstream, with the other target's credential. Identical targets are fine: they share
/// the upstream (their routes are still provisioned, as their `api_path` may differ).
fn check_alias_collisions(targets: &[ProvisionTarget]) -> Result<(), String> {
    let mut first_by_alias: HashMap<&str, &ProvisionTarget> = HashMap::new();
    for target in targets {
        let Some(first) = first_by_alias.get(target.alias.as_str()) else {
            first_by_alias.insert(&target.alias, target);
            continue;
        };
        if !same_upstream(first, target) {
            return Err(format!(
                "`{}` ({:?}://{}:{}) and `{}` ({:?}://{}:{}) resolve to the same OAGW alias `{}` but differ in endpoint or credentials. \
                 OAGW derives the alias of a hostname endpoint, so a hostname tenant override that needs another credential must use a different host; \
                 an IP host can set a distinct upstream_alias",
                first.label,
                first.scheme,
                first.host,
                first.port,
                target.label,
                target.scheme,
                target.host,
                target.port,
                target.alias
            ));
        }
    }
    Ok(())
}

/// Whether two targets would register the same upstream: endpoint and auth plugin with its config.
fn same_upstream(a: &ProvisionTarget, b: &ProvisionTarget) -> bool {
    let same_auth = match (&a.auth, &b.auth) {
        (None, None) => true,
        (Some(x), Some(y)) => x.plugin_type == y.plugin_type && x.config == y.config,
        _ => false,
    };
    a.scheme == b.scheme
        && a.port == b.port
        && normalized_host(&a.host) == normalized_host(&b.host)
        && same_auth
}

/// Provisions every target. Returns the targets that must be retried in the background.
///
/// # Errors
/// An error naming the first deterministically misconfigured target (or the targets whose alias
/// collides); startup must fail.
pub async fn provision(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    targets: Vec<ProvisionTarget>,
) -> anyhow::Result<Vec<ProvisionTarget>> {
    if let Err(cause) = check_alias_collisions(&targets) {
        anyhow::bail!("OAGW provisioning failed: {cause}");
    }
    let mut pending = Vec::new();
    for target in targets {
        match provision_target(gateway, ctx, &target).await {
            Ok(()) => {}
            Err(ProvisionError::Misconfigured(cause)) => {
                anyhow::bail!("OAGW provisioning failed: {cause}");
            }
            Err(ProvisionError::Deferred(cause)) => {
                warn!(cause = %cause, "OAGW provisioning deferred");
                pending.push(target);
            }
        }
    }
    Ok(pending)
}

/// Retries the deferred targets until they succeed or the gear stops: the first retry runs after
/// 2 s, the delay doubles up to 60 s and one warning names the still pending providers after
/// 2 minutes.
#[must_use]
pub fn spawn_reconcile(
    gateway: Arc<dyn ServiceGatewayClientV1>,
    ctx: SecurityContext,
    mut pending: Vec<ProvisionTarget>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut delay = INITIAL_RETRY_DELAY;
        let mut warned = false;
        while !pending.is_empty() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            let mut still_pending = Vec::new();
            for target in pending {
                match provision_target(gateway.as_ref(), &ctx, &target).await {
                    Ok(()) => {}
                    Err(ProvisionError::Deferred(cause)) => {
                        debug!(cause = %cause, "OAGW provisioning is still deferred");
                        still_pending.push(target);
                    }
                    Err(ProvisionError::Misconfigured(cause)) => {
                        error!(cause = %cause, "OAGW provisioning failed permanently; the provider is unavailable");
                    }
                }
            }
            pending = still_pending;
            if !warned && !pending.is_empty() && started.elapsed() >= WARN_AFTER {
                warned = true;
                let names: Vec<&str> = pending.iter().map(|t| t.label.as_str()).collect();
                warn!(providers = ?names, "OAGW provisioning is still pending after 2 minutes");
            }
            delay = (delay * 2).min(MAX_RETRY_DELAY);
        }
    })
}

#[cfg(test)]
#[path = "oagw_provisioning_tests.rs"]
mod tests;
