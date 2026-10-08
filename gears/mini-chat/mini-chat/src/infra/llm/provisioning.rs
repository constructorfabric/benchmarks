//! OAGW upstream / route provisioning at gear start (DESIGN §3.2 "OAGW provisioning", ADR-0005).
//!
//! The gear exchanges its `client_credentials` for an S2S security context, then registers one
//! upstream per provider entry and tenant override (alias = the alias the provider resolver
//! routes by) plus the chat route and, for entries with a `storage_kind`, the Files / Vector
//! Stores routes. An upstream that already exists is reused and existing routes are not
//! duplicated. Deterministic configuration errors are logged; entries failing for transient
//! reasons (secret not yet readable, gateway unavailable, ...) are retried in the background
//! (2 s, doubling up to 60 s) until they succeed or the gear stops; after 2 minutes one warning
//! names the providers still pending.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use oagw_sdk::gts::HTTP_PROTOCOL_ID;
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HttpMatch, HttpMethod,
    ListQuery, MatchRules, PathSuffixMode, Route, Scheme, Server, ServiceGatewayClientV1,
    ServiceGatewayError, SharingMode, Upstream,
};
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use crate::config::{
    ClientCredentialsConfig, MiniChatConfig, ProviderConfig, StorageKind, TenantOverride,
};
use crate::domain::services::AppServices;
use crate::infra::llm::resolver::derive_alias;
use crate::infra::llm::transport::OagwTransport;

const LIST_PAGE: u32 = 100;

/// One OAGW upstream to provision (a provider entry or a tenant override).
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamSpec {
    /// Human-readable label used in logs (`openai`, `openai (tenant <id>)`).
    pub label: String,
    /// Alias the provider resolver routes by.
    pub alias: String,
    /// Whether the alias is passed to OAGW (IP hosts and explicitly configured aliases only:
    /// OAGW derives the alias of hostname endpoints itself and rejects a different one).
    pub send_alias: bool,
    pub endpoint: Endpoint,
    pub auth: Option<AuthConfig>,
    pub routes: Vec<HttpMatch>,
}

/// Outcome of provisioning one upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionOutcome {
    Ready,
    /// Transient failure (e.g. the credstore secret is not readable yet): retried later.
    Deferred(String),
    /// Deterministic configuration error: not retried.
    Failed(String),
}

/// Retry schedule of deferred entries.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub initial: Duration,
    pub max: Duration,
    /// Pending entries after this long produce one warning.
    pub warn_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(2),
            max: Duration::from_secs(60),
            warn_after: Duration::from_secs(120),
        }
    }
}

/// Final state of a provisioning run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionReport {
    pub ready: Vec<String>,
    pub failed: Vec<String>,
    /// Still pending when the run was cancelled.
    pub pending: Vec<String>,
    /// Whether the "still pending" warning was logged.
    pub warned: bool,
}

/// Exchanges client credentials, provisions upstreams/routes and keeps retrying deferred ones.
pub async fn run(
    app: Arc<AppServices>,
    hub: Arc<ClientHub>,
    transport: Arc<OagwTransport>,
    cancel: CancellationToken,
) {
    let Some(ctx) = obtain_s2s_context(&hub, &app.cfg.client_credentials, &cancel).await else {
        return;
    };
    transport.set_s2s_context(ctx.clone());

    let specs = build_specs(&app.cfg);
    if specs.is_empty() {
        tracing::info!("mini-chat: no provider entries to provision in OAGW");
        return;
    }
    let Some(gw) = wait_for_gateway(&hub, &cancel).await else {
        return;
    };
    let report =
        provision_all_with_gate(gw.as_ref(), &ctx, specs, RetryPolicy::default(), &cancel, Some(&app.provisioning)).await;
    tracing::info!(
        ready = ?report.ready,
        failed = ?report.failed,
        pending = ?report.pending,
        "mini-chat OAGW provisioning finished"
    );
}

/// Backoff for the S2S exchange / gateway lookup (the resolvers may start lazily).
async fn backoff_sleep(delay: &mut Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(*delay) => {
            *delay = (*delay * 2).min(Duration::from_secs(30));
            true
        }
    }
}

async fn obtain_s2s_context(
    hub: &ClientHub,
    creds: &ClientCredentialsConfig,
    cancel: &CancellationToken,
) -> Option<SecurityContext> {
    let mut delay = Duration::from_millis(500);
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        match hub.get::<dyn AuthNResolverClient>() {
            Ok(authn) => {
                let req = ClientCredentialsRequest {
                    client_id: creds.client_id.clone(),
                    client_secret: secrecy::SecretString::from(creds.client_secret.clone()),
                    scopes: vec![],
                };
                match authn.exchange_client_credentials(&req).await {
                    Ok(res) => return Some(res.security_context),
                    Err(e) => {
                        tracing::warn!(error = %e, client_id = %creds.client_id, "mini-chat S2S client credentials exchange failed; retrying");
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "authn resolver client unavailable; retrying"),
        }
        if !backoff_sleep(&mut delay, cancel).await {
            return None;
        }
    }
}

async fn wait_for_gateway(
    hub: &ClientHub,
    cancel: &CancellationToken,
) -> Option<Arc<dyn ServiceGatewayClientV1>> {
    let mut delay = Duration::from_millis(500);
    loop {
        match hub.get::<dyn ServiceGatewayClientV1>() {
            Ok(gw) => return Some(gw),
            Err(e) => tracing::warn!(error = %e, "OAGW client unavailable; retrying"),
        }
        if !backoff_sleep(&mut delay, cancel).await {
            return None;
        }
    }
}

// ───────────────────────────── specs ─────────────────────────────

fn is_ip_host(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
}

/// Chat route: `api_path` without query and without everything from `{model}` on.
#[must_use]
pub fn chat_route(api_path: &str) -> HttpMatch {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let path = path
        .split("{model}")
        .next()
        .unwrap_or(path)
        .trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let mut query_allowlist: Vec<String> = Vec::new();
    for key in query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .filter(|k| !k.is_empty())
    {
        if !query_allowlist.iter().any(|k| k == key) {
            query_allowlist.push(key.to_owned());
        }
    }
    HttpMatch {
        methods: vec![HttpMethod::Post],
        path,
        query_allowlist,
        path_suffix_mode: PathSuffixMode::Append,
    }
}

/// Files and Vector Stores routes of a storage kind.
#[must_use]
pub fn rag_routes(kind: StorageKind) -> Vec<HttpMatch> {
    let (prefix, query_allowlist) = match kind {
        StorageKind::Openai => ("/v1", vec![]),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    vec![
        HttpMatch {
            methods: vec![HttpMethod::Post, HttpMethod::Delete],
            path: format!("{prefix}/files"),
            query_allowlist: query_allowlist.clone(),
            path_suffix_mode: PathSuffixMode::Append,
        },
        HttpMatch {
            methods: vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete],
            path: format!("{prefix}/vector_stores"),
            query_allowlist,
            path_suffix_mode: PathSuffixMode::Append,
        },
    ]
}

fn spec_for(
    label: String,
    p: &ProviderConfig,
    host: &str,
    configured_alias: Option<&str>,
    auth_plugin_type: Option<&str>,
    auth_config: Option<&std::collections::HashMap<String, String>>,
) -> UpstreamSpec {
    let port = p.effective_port();
    let configured = configured_alias.filter(|a| !a.trim().is_empty());
    let alias = derive_alias(host, port, p.use_http, configured);
    let mut routes = vec![chat_route(&p.api_path)];
    if let Some(kind) = p.storage_kind {
        routes.extend(rag_routes(kind));
    }
    UpstreamSpec {
        label,
        alias,
        send_alias: configured.is_some() || is_ip_host(host),
        endpoint: Endpoint {
            scheme: if p.use_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: host.to_owned(),
            port,
        },
        auth: auth_plugin_type
            .filter(|t| !t.trim().is_empty())
            .map(|t| AuthConfig {
                plugin_type: t.to_owned(),
                sharing: SharingMode::Private,
                config: auth_config.cloned(),
            }),
        routes,
    }
}

fn override_spec(id: &str, tenant: &str, p: &ProviderConfig, ov: &TenantOverride) -> UpstreamSpec {
    let host = ov.host.clone().unwrap_or_else(|| p.host.clone());
    // Same alias selection as the provider resolver.
    let configured = ov.upstream_alias.clone().or_else(|| {
        if ov.host.is_some() {
            None
        } else {
            p.upstream_alias.clone()
        }
    });
    let plugin = ov
        .auth_plugin_type
        .as_deref()
        .or(p.auth_plugin_type.as_deref());
    let auth_cfg = ov.auth_config.as_ref().or(p.auth_config.as_ref());
    spec_for(
        format!("{id} (tenant {tenant})"),
        p,
        &host,
        configured.as_deref(),
        plugin,
        auth_cfg,
    )
}

/// One spec per provider entry and tenant override; entries sharing an alias are merged into
/// one upstream (routes united, first entry's endpoint and auth kept).
#[must_use]
pub fn build_specs(cfg: &MiniChatConfig) -> Vec<UpstreamSpec> {
    let providers: BTreeMap<&String, &ProviderConfig> = cfg.providers.iter().collect();
    let mut specs: Vec<UpstreamSpec> = Vec::new();
    let mut add = |spec: UpstreamSpec| {
        if let Some(existing) = specs.iter_mut().find(|s| s.alias == spec.alias) {
            if existing.endpoint != spec.endpoint || existing.auth != spec.auth {
                tracing::warn!(
                    alias = %spec.alias,
                    entry = %spec.label,
                    shared_with = %existing.label,
                    "provider entries share an OAGW alias but differ in endpoint or auth; the first entry wins"
                );
            }
            for r in spec.routes {
                if !existing.routes.iter().any(|e| same_match(e, &r)) {
                    existing.routes.push(r);
                }
            }
            existing.label = format!("{}, {}", existing.label, spec.label);
        } else {
            specs.push(spec);
        }
    };
    for (id, p) in providers {
        add(spec_for(
            id.clone(),
            p,
            &p.host,
            p.upstream_alias.as_deref(),
            p.auth_plugin_type.as_deref(),
            p.auth_config.as_ref(),
        ));
        let overrides: BTreeMap<&String, &TenantOverride> = p.tenant_overrides.iter().collect();
        for (tenant, ov) in overrides {
            add(override_spec(id, tenant, p, ov));
        }
    }
    specs
}

fn same_match(a: &HttpMatch, b: &HttpMatch) -> bool {
    let same_set =
        |x: &[String], y: &[String]| x.len() == y.len() && x.iter().all(|v| y.contains(v));
    a.path == b.path
        && a.path_suffix_mode == b.path_suffix_mode
        && a.methods.len() == b.methods.len()
        && a.methods.iter().all(|m| b.methods.contains(m))
        && same_set(&a.query_allowlist, &b.query_allowlist)
}

// ───────────────────────────── provisioning ─────────────────────────────

fn classify(err: &CanonicalError) -> ProvisionOutcome {
    let projected = ServiceGatewayError::from(err.clone());
    let msg = projected.to_string();
    match projected {
        ServiceGatewayError::Validation { .. }
        | ServiceGatewayError::InvalidTargetHost { .. }
        | ServiceGatewayError::PayloadTooLarge { .. }
        | ServiceGatewayError::NotFound { .. } => ProvisionOutcome::Failed(msg),
        _ => ProvisionOutcome::Deferred(msg),
    }
}

async fn find_upstream(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    alias: &str,
) -> Result<Option<Upstream>, CanonicalError> {
    let mut skip = 0;
    loop {
        let page = gw
            .list_upstreams(
                ctx.clone(),
                &ListQuery {
                    top: LIST_PAGE,
                    skip,
                },
            )
            .await?;
        if let Some(u) = page.iter().find(|u| u.alias == alias) {
            return Ok(Some(u.clone()));
        }
        if page.len() < LIST_PAGE as usize {
            return Ok(None);
        }
        skip += LIST_PAGE;
    }
}

async fn list_routes(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    upstream_id: uuid::Uuid,
) -> Result<Vec<Route>, CanonicalError> {
    let mut all = Vec::new();
    let mut skip = 0;
    loop {
        let page = gw
            .list_routes(
                ctx.clone(),
                Some(upstream_id),
                &ListQuery {
                    top: LIST_PAGE,
                    skip,
                },
            )
            .await?;
        let n = page.len();
        all.extend(page);
        if n < LIST_PAGE as usize {
            return Ok(all);
        }
        skip += LIST_PAGE;
    }
}

async fn ensure_upstream(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    spec: &UpstreamSpec,
) -> Result<Upstream, ProvisionOutcome> {
    let mut b = CreateUpstreamRequest::builder(
        Server {
            endpoints: vec![spec.endpoint.clone()],
        },
        HTTP_PROTOCOL_ID,
    );
    if spec.send_alias {
        b = b.alias(spec.alias.clone());
    }
    if let Some(auth) = &spec.auth {
        b = b.auth(auth.clone());
    }
    match gw.create_upstream(ctx.clone(), b.build()).await {
        Ok(u) => Ok(u),
        Err(err) => {
            if !matches!(
                ServiceGatewayError::from(err.clone()),
                ServiceGatewayError::AlreadyExists { .. }
            ) {
                return Err(classify(&err));
            }
            match find_upstream(gw, ctx, &spec.alias).await {
                Ok(Some(u)) => {
                    tracing::debug!(alias = %spec.alias, "reusing existing OAGW upstream");
                    Ok(u)
                }
                Ok(None) => Err(ProvisionOutcome::Deferred(format!(
                    "upstream '{}' reported as existing but not found",
                    spec.alias
                ))),
                Err(e) => Err(classify(&e)),
            }
        }
    }
}

/// Provisions one upstream and its routes (creating only the routes that do not exist yet).
pub async fn provision_upstream(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    spec: &UpstreamSpec,
) -> ProvisionOutcome {
    let upstream = match ensure_upstream(gw, ctx, spec).await {
        Ok(u) => u,
        Err(outcome) => return outcome,
    };
    if upstream.alias != spec.alias {
        tracing::warn!(
            expected = %spec.alias,
            actual = %upstream.alias,
            entry = %spec.label,
            "OAGW upstream alias differs from the alias the provider resolver routes by"
        );
    }
    let existing = match list_routes(gw, ctx, upstream.id).await {
        Ok(r) => r,
        Err(e) => return classify(&e),
    };
    for m in &spec.routes {
        let exists = existing.iter().any(|r| {
            r.match_rules
                .http
                .as_ref()
                .is_some_and(|h| same_match(h, m))
        });
        if exists {
            continue;
        }
        let req = CreateRouteRequest::builder(
            upstream.id,
            MatchRules {
                http: Some(m.clone()),
                grpc: None,
            },
        )
        .build();
        if let Err(e) = gw.create_route(ctx.clone(), req).await {
            if matches!(
                ServiceGatewayError::from(e.clone()),
                ServiceGatewayError::AlreadyExists { .. }
            ) {
                continue;
            }
            return classify(&e);
        }
    }
    ProvisionOutcome::Ready
}

#[allow(clippy::cognitive_complexity, reason = "tracing macro expansion only")]
fn log_outcome(spec: &UpstreamSpec, outcome: &ProvisionOutcome, first: bool) {
    match outcome {
        ProvisionOutcome::Ready => {
            tracing::info!(entry = %spec.label, alias = %spec.alias, "OAGW upstream and routes provisioned");
        }
        ProvisionOutcome::Failed(e) => tracing::error!(
            entry = %spec.label,
            alias = %spec.alias,
            error = %e,
            "OAGW provisioning failed: provider entry is misconfigured"
        ),
        ProvisionOutcome::Deferred(e) if first => tracing::info!(
            entry = %spec.label,
            alias = %spec.alias,
            error = %e,
            "OAGW provisioning deferred; retrying in background"
        ),
        ProvisionOutcome::Deferred(e) => {
            tracing::debug!(entry = %spec.label, error = %e, "OAGW provisioning still deferred");
        }
    }
}

/// One provisioning pass; returns the specs that must be retried.
async fn provision_pass(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    specs: Vec<UpstreamSpec>,
    first: bool,
    report: &mut ProvisionReport,
) -> Vec<UpstreamSpec> {
    let mut deferred = Vec::new();
    for spec in specs {
        let outcome = provision_upstream(gw, ctx, &spec).await;
        log_outcome(&spec, &outcome, first);
        match outcome {
            ProvisionOutcome::Ready => report.ready.push(spec.label),
            ProvisionOutcome::Failed(_) => report.failed.push(spec.label),
            ProvisionOutcome::Deferred(_) => deferred.push(spec),
        }
    }
    deferred
}

/// Provisions every spec, then retries deferred ones with backoff until all are done or
/// `cancel` fires.
pub async fn provision_all(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    specs: Vec<UpstreamSpec>,
    policy: RetryPolicy,
    cancel: &CancellationToken,
) -> ProvisionReport {
    provision_all_with_gate(gw, ctx, specs, policy, cancel, None).await
}

/// `provision_all` that publishes pending aliases to `gate` and retries immediately when a turn
/// nudges it (a provider used before its upstream exists).
pub async fn provision_all_with_gate(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    specs: Vec<UpstreamSpec>,
    policy: RetryPolicy,
    cancel: &CancellationToken,
    gate: Option<&crate::infra::llm::gate::ProvisioningGate>,
) -> ProvisionReport {
    let started = tokio::time::Instant::now();
    let mut report = ProvisionReport::default();
    let publish = |pending: &[UpstreamSpec]| {
        if let Some(g) = gate {
            g.set_pending(pending.iter().map(|s| s.alias.clone()));
        }
    };
    let mut pending = provision_pass(gw, ctx, specs, true, &mut report).await;
    publish(&pending);
    let mut delay = policy.initial;
    while !pending.is_empty() {
        let nudged = async {
            match gate {
                Some(g) => g.retry_requested().await,
                None => futures::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = cancel.cancelled() => {
                report.pending = pending.iter().map(|s| s.label.clone()).collect();
                return report;
            }
            () = tokio::time::sleep(delay) => {
                delay = (delay * 2).min(policy.max);
            }
            () = nudged => {}
        }
        pending = provision_pass(gw, ctx, pending, false, &mut report).await;
        publish(&pending);
        if !pending.is_empty() && !report.warned && started.elapsed() >= policy.warn_after {
            report.warned = true;
            let names: Vec<&str> = pending.iter().map(|s| s.label.as_str()).collect();
            tracing::warn!(providers = ?names, "OAGW provisioning still pending for providers");
        }
    }
    report
}

#[cfg(test)]
#[path = "provisioning_tests.rs"]
mod tests;
