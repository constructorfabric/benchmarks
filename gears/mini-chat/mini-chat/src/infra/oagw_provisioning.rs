//! OAGW provisioning at gear start (DESIGN §3.2 "OAGW provisioning", §3.5).
//!
//! One upstream per provider entry and per tenant override with its own alias,
//! created under the gear's S2S context (Ruling R2), plus a chat route and, for
//! entries with `storage_kind`, the RAG routes. `AlreadyExists` reuses the
//! upstream; `FailedPrecondition` (credstore secret not readable yet) defers the
//! target to the background reconcile loop; any other error is a deterministic
//! misconfiguration and fails startup. Provisioning works on a copy of the
//! entries: the resolver keeps routing by the configured alias.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PathSuffixMode, ROUTE_SCHEMA, Scheme, Server,
    ServiceGatewayClientV1, SharingMode, UPSTREAM_SCHEMA,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use toolkit::{Healthcheck, HealthcheckResult};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderEntry, StorageKind};

/// Tag put on every upstream the gear creates.
const UPSTREAM_TAG: &str = "mini-chat";
/// Page size used when looking up an existing upstream by alias.
const LIST_PAGE: u32 = 100;
/// Reconcile schedule (DESIGN §3.2): first retry after 2 s, doubling to 60 s.
const RECONCILE_FIRST_DELAY: Duration = Duration::from_secs(2);
const RECONCILE_MAX_DELAY: Duration = Duration::from_secs(60);
/// One warning when targets are still pending after this long.
const RECONCILE_WARN_AFTER: Duration = Duration::from_secs(120);

/// Outcome of [`OagwProvisioner::provision_all`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProvisionReport {
    /// Targets whose credstore secret is not readable yet (retried by reconcile).
    pub pending: Vec<String>,
}

/// A deterministic provisioning failure (fails startup).
#[derive(Debug, thiserror::Error)]
#[error("OAGW provisioning of '{target}' failed: {source}")]
pub struct ProvisionError {
    /// Target key: the provider id, or `<provider id>@<tenant>` for an override.
    pub target: String,
    #[source]
    pub source: Box<CanonicalError>,
}

/// One upstream to provision with its routes.
#[derive(Debug, Clone)]
struct Target {
    key: String,
    alias: String,
    endpoint: Endpoint,
    auth: Option<AuthConfig>,
    chat_route: HttpMatch,
    rag_routes: Vec<HttpMatch>,
}

enum Outcome {
    Ready,
    Pending,
}

pub struct OagwProvisioner {
    gw: Arc<dyn ServiceGatewayClientV1>,
    targets: Vec<Target>,
}

impl OagwProvisioner {
    /// Build the targets from a copy of the (alias-filled) provider entries.
    #[must_use]
    pub fn new(gw: Arc<dyn ServiceGatewayClientV1>, cfg: &MiniChatConfig) -> Self {
        let mut ids: Vec<&String> = cfg.providers.keys().collect();
        ids.sort();
        let mut targets = Vec::new();
        for id in ids {
            let entry = &cfg.providers[id];
            let base_alias = entry.alias().to_owned();
            targets.push(target(
                id.clone(),
                entry,
                base_alias.clone(),
                &entry.host,
                entry.auth_plugin_type.as_deref(),
                &entry.auth_config,
            ));
            let mut tenants: Vec<&String> = entry.tenant_overrides.keys().collect();
            tenants.sort();
            for tenant in tenants {
                let ov = &entry.tenant_overrides[tenant];
                let alias = ov
                    .upstream_alias
                    .clone()
                    .unwrap_or_else(|| base_alias.clone());
                if same_alias(&alias, &base_alias) {
                    tracing::warn!(
                        provider = %id, tenant = %tenant, alias = %alias,
                        "mini-chat: tenant override uses the provider's own upstream alias; \
                         no separate upstream is provisioned for it"
                    );
                    continue;
                }
                targets.push(target(
                    format!("{id}@{tenant}"),
                    entry,
                    alias,
                    ov.host.as_deref().unwrap_or(&entry.host),
                    ov.auth_plugin_type
                        .as_deref()
                        .or(entry.auth_plugin_type.as_deref()),
                    ov.auth_config.as_ref().unwrap_or(&entry.auth_config),
                ));
            }
        }
        warn_shared_aliases(&targets);
        Self { gw, targets }
    }

    /// Provision every target once.
    ///
    /// # Errors
    /// [`ProvisionError`] for the first deterministically misconfigured target.
    pub async fn provision_all(
        &self,
        ctx: &SecurityContext,
    ) -> Result<ProvisionReport, ProvisionError> {
        let mut report = ProvisionReport::default();
        for t in &self.targets {
            match self.provision_target(t, ctx).await {
                Ok(Outcome::Ready) => {}
                Ok(Outcome::Pending) => report.pending.push(t.key.clone()),
                Err(source) => {
                    return Err(ProvisionError {
                        target: t.key.clone(),
                        source: Box::new(source),
                    });
                }
            }
        }
        Ok(report)
    }

    /// Retry `pending` targets once; returns those still pending. Retries go on
    /// until the gear stops (DESIGN §3.2): only a clearly invalid request
    /// (`InvalidArgument` / `OutOfRange`) drops the target; any other error,
    /// including a failed chat route after the upstream exists, keeps it pending.
    pub async fn provision_pending(
        &self,
        ctx: &SecurityContext,
        pending: &[String],
    ) -> Vec<String> {
        let mut still = Vec::new();
        for key in pending {
            let Some(t) = self.targets.iter().find(|t| &t.key == key) else {
                continue;
            };
            match self.provision_target(t, ctx).await {
                Ok(Outcome::Ready) => {}
                Ok(Outcome::Pending) => still.push(key.clone()),
                Err(
                    e
                    @ (CanonicalError::InvalidArgument { .. } | CanonicalError::OutOfRange { .. }),
                ) => tracing::error!(
                    provider = %key, alias = %t.alias, error = %e,
                    "mini-chat: OAGW provisioning rejected; provider stays unavailable"
                ),
                Err(e) => {
                    tracing::warn!(
                        provider = %key, alias = %t.alias, error = %e,
                        "mini-chat: OAGW provisioning failed; will retry"
                    );
                    still.push(key.clone());
                }
            }
        }
        still
    }

    async fn provision_target(
        &self,
        t: &Target,
        ctx: &SecurityContext,
    ) -> Result<Outcome, CanonicalError> {
        let Some(upstream_id) = self.ensure_upstream(t, ctx).await? else {
            return Ok(Outcome::Pending);
        };
        self.ensure_route(t, ctx, upstream_id, &t.chat_route)
            .await?;
        for rag in &t.rag_routes {
            if let Err(e) = self.ensure_route(t, ctx, upstream_id, rag).await {
                tracing::warn!(
                    provider = %t.key, alias = %t.alias, path = %rag.path, error = %e,
                    "mini-chat: OAGW RAG route not provisioned; RAG degraded for this provider"
                );
            }
        }
        Ok(Outcome::Ready)
    }

    /// `Ok(None)` = deferred (secret not readable yet).
    async fn ensure_upstream(
        &self,
        t: &Target,
        ctx: &SecurityContext,
    ) -> Result<Option<Uuid>, CanonicalError> {
        match self
            .gw
            .create_upstream(ctx.clone(), upstream_request(t))
            .await
        {
            Ok(u) => {
                tracing::info!(
                    provider = %t.key, alias = %u.alias, upstream_id = %u.id,
                    host = %t.endpoint.host, port = t.endpoint.port,
                    "mini-chat: OAGW upstream created"
                );
                Ok(Some(u.id))
            }
            Err(e @ CanonicalError::AlreadyExists { .. })
                if e.resource_type() == Some(UPSTREAM_SCHEMA) =>
            {
                self.reuse_upstream(t, ctx, e).await.map(Some)
            }
            Err(e @ CanonicalError::FailedPrecondition { .. }) => {
                tracing::info!(
                    provider = %t.key, alias = %t.alias, error = %e,
                    "mini-chat: OAGW upstream deferred (credstore secret not readable yet)"
                );
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Look up the upstream an `AlreadyExists` refers to (`conflict` if absent).
    async fn reuse_upstream(
        &self,
        t: &Target,
        ctx: &SecurityContext,
        conflict: CanonicalError,
    ) -> Result<Uuid, CanonicalError> {
        let wanted = conflict.resource_name().unwrap_or(&t.alias).to_owned();
        let Some(id) = self.find_upstream(ctx, &wanted).await? else {
            return Err(conflict);
        };
        tracing::info!(
            provider = %t.key, alias = %wanted, upstream_id = %id,
            "mini-chat: OAGW upstream already exists; reused"
        );
        Ok(id)
    }

    async fn find_upstream(
        &self,
        ctx: &SecurityContext,
        alias: &str,
    ) -> Result<Option<Uuid>, CanonicalError> {
        let mut skip = 0;
        loop {
            let page = self
                .gw
                .list_upstreams(
                    ctx.clone(),
                    &ListQuery {
                        top: LIST_PAGE,
                        skip,
                    },
                )
                .await?;
            if let Some(u) = page.iter().find(|u| same_alias(&u.alias, alias)) {
                return Ok(Some(u.id));
            }
            if page.len() < LIST_PAGE as usize {
                return Ok(None);
            }
            skip += LIST_PAGE;
        }
    }

    async fn ensure_route(
        &self,
        t: &Target,
        ctx: &SecurityContext,
        upstream_id: Uuid,
        rule: &HttpMatch,
    ) -> Result<(), CanonicalError> {
        let req = CreateRouteRequest::builder(
            upstream_id,
            MatchRules {
                http: Some(rule.clone()),
                grpc: None,
            },
        )
        .tags(vec![UPSTREAM_TAG.to_owned()])
        .build();
        match self.gw.create_route(ctx.clone(), req).await {
            Ok(r) => {
                tracing::info!(
                    provider = %t.key, alias = %t.alias, route_id = %r.id,
                    methods = ?rule.methods, path = %rule.path,
                    query_allowlist = ?rule.query_allowlist,
                    "mini-chat: OAGW route created"
                );
                Ok(())
            }
            Err(e @ CanonicalError::AlreadyExists { .. })
                if e.resource_type() == Some(ROUTE_SCHEMA) =>
            {
                tracing::info!(
                    provider = %t.key, alias = %t.alias, methods = ?rule.methods,
                    path = %rule.path, "mini-chat: OAGW route already exists; reused"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

fn upstream_request(t: &Target) -> CreateUpstreamRequest {
    let mut req = CreateUpstreamRequest::builder(
        Server {
            endpoints: vec![t.endpoint.clone()],
        },
        HTTP_PROTOCOL_ID,
    )
    .alias(t.alias.clone())
    .tags(vec![UPSTREAM_TAG.to_owned()]);
    if let Some(auth) = &t.auth {
        req = req.auth(auth.clone());
    }
    req.build()
}

fn target(
    key: String,
    entry: &ProviderEntry,
    alias: String,
    host: &str,
    auth_plugin_type: Option<&str>,
    auth_config: &std::collections::HashMap<String, String>,
) -> Target {
    let auth = auth_plugin_type.map(|plugin_type| AuthConfig {
        plugin_type: plugin_type.to_owned(),
        // Proxying uses the same S2S context, so the upstream stays private to it.
        sharing: SharingMode::Private,
        config: (!auth_config.is_empty()).then(|| auth_config.clone()),
    });
    Target {
        key,
        alias,
        endpoint: Endpoint {
            scheme: if entry.use_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: host.to_owned(),
            port: entry.effective_port(),
        },
        auth,
        chat_route: chat_route(&entry.api_path),
        rag_routes: entry.storage_kind.map(rag_routes).unwrap_or_default(),
    }
}

/// `POST` on the `api_path` prefix: query stripped (its keys become the
/// allowlist), `{model}` and everything after it served as path suffix.
fn chat_route(api_path: &str) -> HttpMatch {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let path = path
        .split_once("{model}")
        .map_or(path, |(prefix, _)| prefix);
    let mut allow: Vec<String> = Vec::new();
    for key in query
        .split('&')
        .map(|kv| kv.split('=').next().unwrap_or(""))
    {
        if !key.is_empty() && !allow.iter().any(|k| k == key) {
            allow.push(key.to_owned());
        }
    }
    http_match(vec![HttpMethod::Post], path.to_owned(), &allow)
}

/// DESIGN §3.5 RAG routes: prefix `/v1` (openai) or `/openai` + `api-version` (azure).
fn rag_routes(kind: StorageKind) -> Vec<HttpMatch> {
    let (p, allow): (&str, Vec<String>) = match kind {
        StorageKind::Openai => ("/v1", vec![]),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    vec![
        http_match(vec![HttpMethod::Post], format!("{p}/files"), &allow),
        http_match(vec![HttpMethod::Delete], format!("{p}/files/"), &allow),
        http_match(vec![HttpMethod::Post], format!("{p}/vector_stores"), &allow),
        http_match(vec![HttpMethod::Get], format!("{p}/vector_stores/"), &allow),
        http_match(
            vec![HttpMethod::Delete],
            format!("{p}/vector_stores/"),
            &allow,
        ),
    ]
}

fn http_match(methods: Vec<HttpMethod>, path: String, allow: &[String]) -> HttpMatch {
    HttpMatch {
        methods,
        path,
        query_allowlist: allow.to_vec(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

/// Two targets on one alias share one OAGW upstream (the second one reuses it).
fn warn_shared_aliases(targets: &[Target]) {
    for (i, a) in targets.iter().enumerate() {
        if let Some(b) = targets[..i].iter().find(|b| same_alias(&a.alias, &b.alias)) {
            tracing::warn!(
                alias = %a.alias, first = %b.key, second = %a.key,
                "mini-chat: two providers share one OAGW upstream alias; \
                 the second reuses the first one's upstream"
            );
        }
    }
}

/// OAGW compares aliases normalized: lower-case, trailing `.` trimmed.
fn same_alias(a: &str, b: &str) -> bool {
    a.trim_end_matches('.')
        .eq_ignore_ascii_case(b.trim_end_matches('.'))
}

const HEALTH_STARTING: u8 = 0;
const HEALTH_READY: u8 = 1;
const HEALTH_FAILED: u8 = 2;

/// Gear readiness check (`/readyz`): unhealthy until start-phase provisioning
/// succeeded, and permanently unhealthy when it failed. The S2S exchange cannot
/// run in `init` (types-registry resolves plugins only after `post_init`), and a
/// start-phase error does not stop the host, so this is how a failed start is
/// reported. Deferred (pending) providers do not affect it.
pub struct ProvisioningHealth {
    state: AtomicU8,
}

impl Default for ProvisioningHealth {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(HEALTH_STARTING),
        }
    }
}

impl ProvisioningHealth {
    pub fn mark_ready(&self) {
        self.state.store(HEALTH_READY, Ordering::Release);
    }

    pub fn mark_failed(&self) {
        self.state.store(HEALTH_FAILED, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl Healthcheck for ProvisioningHealth {
    fn name(&self) -> &'static str {
        "mini-chat-oagw-provisioning"
    }

    async fn check(&self) -> HealthcheckResult {
        match self.state.load(Ordering::Acquire) {
            HEALTH_READY => HealthcheckResult::healthy(),
            HEALTH_FAILED => HealthcheckResult::unhealthy("OAGW provisioning failed")
                .with_code("oagw_provisioning_failed"),
            _ => HealthcheckResult::unhealthy("OAGW provisioning not complete")
                .with_code("oagw_provisioning_starting"),
        }
    }
}

/// Spawn the background reconcile loop for `pending` targets (no-op when empty).
/// The task ends when nothing is pending or `cancel` fires.
pub fn spawn_reconcile(
    workers: &mut JoinSet<()>,
    provisioner: Arc<OagwProvisioner>,
    ctx: SecurityContext,
    pending: Vec<String>,
    cancel: CancellationToken,
) {
    if pending.is_empty() {
        return;
    }
    workers.spawn(reconcile_loop(provisioner, ctx, pending, cancel));
}

async fn reconcile_loop(
    provisioner: Arc<OagwProvisioner>,
    ctx: SecurityContext,
    mut pending: Vec<String>,
    cancel: CancellationToken,
) {
    let started = tokio::time::Instant::now();
    let mut delay = RECONCILE_FIRST_DELAY;
    let mut warned = false;
    while !pending.is_empty() {
        if cancel
            .run_until_cancelled(tokio::time::sleep(delay))
            .await
            .is_none()
        {
            return;
        }
        let Some(still) = cancel
            .run_until_cancelled(provisioner.provision_pending(&ctx, &pending))
            .await
        else {
            return;
        };
        pending = still;
        if pending.is_empty() {
            tracing::info!("mini-chat: all deferred OAGW providers provisioned");
        } else if !warned && started.elapsed() >= RECONCILE_WARN_AFTER {
            warned = true;
            tracing::warn!(
                pending = ?pending,
                "mini-chat: OAGW providers still pending after 2 minutes (credstore secret not readable)"
            );
        }
        delay = (delay * 2).min(RECONCILE_MAX_DELAY);
    }
}

#[cfg(test)]
#[path = "oagw_provisioning_tests.rs"]
mod oagw_provisioning_tests;
