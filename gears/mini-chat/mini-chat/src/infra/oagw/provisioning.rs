//! OAGW provisioning (spec §17, DESIGN §3.2 "OAGW provisioning", §3.5 "LLM
//! Provider"): an upstream and its routes for every provider entry and every
//! tenant override, created with the gear's S2S security context.
//!
//! - Upstream: `Endpoint { http if use_http else https, host, port }`, the
//!   configured alias, auth `{ auth_plugin_type, Private, auth_config }`.
//!   `AlreadyExists` → the existing upstream (listed and matched by alias) is
//!   reused. When OAGW rejects the explicit alias because it derives the alias
//!   itself (hostname endpoints with a non-default port), the upstream is
//!   created without alias and the provider resolver routes that entry by the
//!   alias OAGW returned. `FailedPrecondition` (the credstore secret is not
//!   readable yet) defers the provider to a background reconcile; any other
//!   error fails startup.
//! - Routes ([`route_specs`]): chat `POST` on the `api_path` prefix before
//!   `{model}` and RAG routes on `/v1` (openai) or `/openai` (azure), all with
//!   `PathSuffixMode::Append`. Duplicate routes (`AlreadyExists`) are fine; a
//!   failed RAG route only degrades RAG and is logged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1,
    SharingMode, Upstream,
};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderEntry, StorageKind, TenantOverride};
use crate::infra::llm::{ProviderResolver, ResolvedProvider};
use crate::infra::oagw::s2s::S2sContext;

/// Text of the OAGW validation error for an explicit alias on endpoints whose
/// alias OAGW derives itself.
const AUTO_DERIVED_ALIAS: &str = "alias is auto-derived";

/// Page size of the upstream listing used to find an existing upstream.
const LIST_PAGE: u32 = 100;

/// Minimum interval between two on-demand provisioning attempts of one
/// deferred provider.
const ON_DEMAND_INTERVAL: Duration = Duration::from_secs(1);

/// One route to provision on a provider's upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    pub method: HttpMethod,
    /// Path prefix (the request suffix is appended).
    pub path: String,
    pub query_allowlist: Vec<String>,
}

/// Routes of `entry`: the chat route (first), then the RAG routes.
#[must_use]
pub fn route_specs(entry: &ProviderEntry) -> Vec<RouteSpec> {
    let (path, query) = entry
        .api_path
        .split_once('?')
        .unwrap_or((entry.api_path.as_str(), ""));
    let mut specs = vec![RouteSpec {
        method: HttpMethod::Post,
        path: chat_prefix(path),
        query_allowlist: query_keys(query),
    }];
    let (prefix, allowlist): (&str, &[&str]) = match entry.storage_kind {
        StorageKind::Openai => ("/v1", &[]),
        StorageKind::Azure => ("/openai", &["api-version"]),
    };
    for (method, resource) in [
        (HttpMethod::Post, "/files"),
        (HttpMethod::Delete, "/files"),
        (HttpMethod::Post, "/vector_stores"),
        (HttpMethod::Delete, "/vector_stores"),
        (HttpMethod::Get, "/vector_stores"),
    ] {
        specs.push(RouteSpec {
            method,
            path: format!("{prefix}{resource}"),
            query_allowlist: allowlist.iter().map(|k| (*k).to_owned()).collect(),
        });
    }
    specs
}

/// Prefix of the chat path before `{model}`, without trailing `/`.
fn chat_prefix(path: &str) -> String {
    let before_model = path.split("{model}").next().unwrap_or(path);
    let trimmed = before_model.trim_end_matches('/');
    if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    }
}

/// Distinct keys of a query string, in order.
fn query_keys(query: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for pair in query.split('&') {
        let key = pair.split('=').next().unwrap_or_default();
        if !key.is_empty() && !keys.iter().any(|k| k == key) {
            keys.push(key.to_owned());
        }
    }
    keys
}

/// The upstream of a provider entry (`tenant = None`) or of one of its tenant
/// overrides.
#[derive(Debug, Clone)]
pub struct UpstreamTarget {
    pub provider_id: String,
    pub tenant: Option<Uuid>,
    pub endpoint: Endpoint,
    /// Configured alias (the host by default).
    pub alias: String,
    pub auth: Option<AuthConfig>,
}

impl UpstreamTarget {
    /// Upstream of the entry itself.
    #[must_use]
    pub fn for_entry(provider_id: &str, entry: &ProviderEntry) -> Self {
        Self {
            provider_id: provider_id.to_owned(),
            tenant: None,
            endpoint: endpoint(entry, &entry.host),
            alias: entry.effective_upstream_alias().to_owned(),
            auth: auth(entry.auth_plugin_type.as_deref(), &entry.auth_config),
        }
    }

    /// Upstream of the tenant override `ov`; unset fields fall back to the entry.
    #[must_use]
    pub fn for_override(
        provider_id: &str,
        entry: &ProviderEntry,
        tenant: Uuid,
        ov: &TenantOverride,
    ) -> Self {
        let host = ov.host.as_deref().unwrap_or(&entry.host);
        Self {
            provider_id: provider_id.to_owned(),
            tenant: Some(tenant),
            endpoint: endpoint(entry, host),
            alias: ov
                .upstream_alias
                .clone()
                .unwrap_or_else(|| host.to_owned()),
            auth: auth(
                ov.auth_plugin_type
                    .as_deref()
                    .or(entry.auth_plugin_type.as_deref()),
                ov.auth_config.as_ref().unwrap_or(&entry.auth_config),
            ),
        }
    }

    /// The entry's upstream followed by one per tenant override (by tenant id).
    #[must_use]
    pub fn all(provider_id: &str, entry: &ProviderEntry) -> Vec<Self> {
        let mut overrides: Vec<(&Uuid, &TenantOverride)> = entry.tenant_overrides.iter().collect();
        overrides.sort_by_key(|(tenant, _)| **tenant);
        std::iter::once(Self::for_entry(provider_id, entry))
            .chain(
                overrides
                    .into_iter()
                    .map(|(tenant, ov)| Self::for_override(provider_id, entry, *tenant, ov)),
            )
            .collect()
    }

    /// The alias OAGW derives for a hostname endpoint (`host`, or `host:port`
    /// for a port that is not the scheme's default).
    fn derived_alias(&self) -> String {
        let host = self.endpoint.host.to_ascii_lowercase();
        let host = host.trim_end_matches('.');
        let default_port = match self.endpoint.scheme {
            Scheme::Http => 80,
            _ => 443,
        };
        if self.endpoint.port == default_port {
            host.to_owned()
        } else {
            format!("{host}:{}", self.endpoint.port)
        }
    }

    /// Whether the existing upstream `up` serves exactly this target's
    /// endpoint with the same auth plugin and configuration.
    fn matches(&self, up: &Upstream) -> bool {
        let endpoint_ok = matches!(up.server.endpoints.as_slice(), [ep]
            if ep.scheme == self.endpoint.scheme
                && ep.port == self.endpoint.port
                && ep.host.trim_end_matches('.').eq_ignore_ascii_case(self.endpoint.host.trim_end_matches('.')));
        endpoint_ok && auth_key(self.auth.as_ref()) == auth_key(up.auth.as_ref())
    }

    fn label(&self) -> String {
        match self.tenant {
            None => format!("provider '{}'", self.provider_id),
            Some(t) => format!("provider '{}' (tenant override {t})", self.provider_id),
        }
    }

    fn request(&self, with_alias: bool) -> CreateUpstreamRequest {
        let mut b = CreateUpstreamRequest::builder(
            Server {
                endpoints: vec![self.endpoint.clone()],
            },
            HTTP_PROTOCOL_ID,
        );
        if with_alias {
            b = b.alias(self.alias.clone());
        }
        if let Some(auth) = &self.auth {
            b = b.auth(auth.clone());
        }
        b.build()
    }
}

/// Plugin type and sorted configuration of an auth configuration.
type AuthKey<'a> = (&'a str, Vec<(&'a String, &'a String)>);

/// Comparable form of an auth configuration (`None` config = empty).
fn auth_key(auth: Option<&AuthConfig>) -> Option<AuthKey<'_>> {
    auth.map(|a| {
        let mut config: Vec<(&String, &String)> =
            a.config.iter().flat_map(HashMap::iter).collect();
        config.sort();
        (a.plugin_type.as_str(), config)
    })
}

fn describe_endpoint(ep: &Endpoint) -> String {
    let scheme = match ep.scheme {
        Scheme::Http => "http",
        _ => "https",
    };
    format!("{scheme}://{}:{}", ep.host, ep.port)
}

fn endpoint(entry: &ProviderEntry, host: &str) -> Endpoint {
    Endpoint {
        scheme: if entry.use_http {
            Scheme::Http
        } else {
            Scheme::Https
        },
        host: host.to_owned(),
        port: entry.effective_port(),
    }
}

fn auth(plugin_type: Option<&str>, config: &HashMap<String, String>) -> Option<AuthConfig> {
    plugin_type.map(|plugin_type| AuthConfig {
        plugin_type: plugin_type.to_owned(),
        sharing: SharingMode::Private,
        config: (!config.is_empty()).then(|| config.clone()),
    })
}

/// `CreateUpstreamRequest` of `target` with its configured alias.
#[must_use]
pub fn upstream_request(target: &UpstreamTarget) -> CreateUpstreamRequest {
    target.request(true)
}

/// How a `create_upstream` failure is handled.
enum Failure {
    /// The alias is taken: reuse that upstream.
    AlreadyExists,
    /// OAGW derives the alias itself: create without alias.
    AutoDerivedAlias,
    /// The credstore secret is not readable yet: retry in the background.
    Deferred,
    /// Deterministic misconfiguration.
    Fatal,
}

fn classify(err: &CanonicalError) -> Failure {
    match err {
        CanonicalError::AlreadyExists { .. } => Failure::AlreadyExists,
        CanonicalError::FailedPrecondition { .. } => Failure::Deferred,
        CanonicalError::InvalidArgument { .. } if mentions_auto_derived_alias(err) => {
            Failure::AutoDerivedAlias
        }
        _ => Failure::Fatal,
    }
}

fn mentions_auto_derived_alias(err: &CanonicalError) -> bool {
    err.detail().contains(AUTO_DERIVED_ALIAS)
        || oagw_sdk::ServiceGatewayError::from(err.clone())
            .to_string()
            .contains(AUTO_DERIVED_ALIAS)
}

/// Background reconcile timings (DESIGN: 2 s, doubling up to 60 s, one
/// warning after 2 minutes).
#[derive(Debug, Clone, Copy)]
pub struct ReconcileTimings {
    pub first_delay: Duration,
    pub max_delay: Duration,
    pub warn_after: Duration,
}

impl Default for ReconcileTimings {
    fn default() -> Self {
        Self {
            first_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
            warn_after: Duration::from_secs(120),
        }
    }
}

/// Registers the providers' upstreams and routes in OAGW.
pub struct Provisioner {
    gw: Arc<dyn ServiceGatewayClientV1>,
    /// Provider entries by id (sorted, for deterministic order).
    providers: Vec<(String, ProviderEntry)>,
    resolver: Arc<ProviderResolver>,
    s2s: Arc<S2sContext>,
    timings: ReconcileTimings,
    /// Deferred providers → time of the last on-demand attempt.
    pending: Mutex<HashMap<String, Option<Instant>>>,
    /// Per provider: held while an on-demand or reconcile attempt runs, so a
    /// request arriving meanwhile waits for its outcome (and the alias it
    /// registers) instead of proxying with the stale alias.
    gates: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl Provisioner {
    #[must_use]
    pub fn new(
        gw: Arc<dyn ServiceGatewayClientV1>,
        cfg: &MiniChatConfig,
        resolver: Arc<ProviderResolver>,
        s2s: Arc<S2sContext>,
    ) -> Self {
        let mut providers: Vec<(String, ProviderEntry)> = cfg
            .providers
            .iter()
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();
        providers.sort_by(|a, b| a.0.cmp(&b.0));
        let gates = providers
            .iter()
            .map(|(id, _)| (id.clone(), Arc::new(tokio::sync::Mutex::new(()))))
            .collect();
        Self {
            gw,
            providers,
            resolver,
            s2s,
            timings: ReconcileTimings::default(),
            pending: Mutex::new(HashMap::new()),
            gates,
        }
    }

    #[must_use]
    pub fn with_timings(mut self, timings: ReconcileTimings) -> Self {
        self.timings = timings;
        self
    }

    /// Provision every provider entry and tenant override with `ctx`.
    /// Returns the ids of the providers deferred to the background reconcile.
    ///
    /// # Errors
    /// A deterministic misconfiguration (OAGW rejected an upstream or a chat
    /// route for another reason than an unreadable secret).
    pub async fn provision_all(&self, ctx: SecurityContext) -> anyhow::Result<Vec<String>> {
        let mut deferred = Vec::new();
        for (id, entry) in &self.providers {
            if self.provision_provider(&ctx, id, entry).await? {
                warn!(
                    provider = %id,
                    "OAGW provisioning deferred: the provider's credstore secret is not readable yet; retrying in the background"
                );
                deferred.push(id.clone());
            }
        }
        let mut pending = self.lock_pending();
        for id in &deferred {
            pending.entry(id.clone()).or_insert(None);
        }
        Ok(deferred)
    }

    /// Before a request to `p`: when its provider is still deferred, try to
    /// provision it once (at most one attempt per second per provider) and
    /// return `p` re-resolved, since the alias may have changed. A request
    /// arriving while another attempt for the provider is in flight waits for
    /// it and is re-resolved when it succeeded. `None` when nothing was
    /// attempted; the request then proceeds as resolved.
    pub async fn ensure_provisioned(&self, p: &ResolvedProvider) -> Option<ResolvedProvider> {
        if !self.lock_pending().contains_key(&p.provider_id) {
            return None;
        }
        let gate = Arc::clone(self.gates.get(&p.provider_id)?);
        let _attempt = gate.lock().await;
        {
            let mut pending = self.lock_pending();
            let Some(last) = pending.get_mut(&p.provider_id) else {
                // Provisioned by the attempt this request waited for.
                drop(pending);
                return self.resolver.resolve(&p.provider_id, p.tenant_id).ok();
            };
            if last.is_some_and(|t| t.elapsed() < ON_DEMAND_INTERVAL) {
                return None;
            }
            *last = Some(Instant::now());
        }
        let ctx = self.s2s.get().ok()?;
        let (id, entry) = self.providers.iter().find(|(id, _)| *id == p.provider_id)?;
        match self.provision_provider(&ctx, id, entry).await {
            Ok(false) => {
                self.lock_pending().remove(id);
                info!(provider = %id, "OAGW provisioning completed on demand");
            }
            Ok(true) => {}
            Err(err) => {
                warn!(provider = %id, error = %format!("{err:#}"), "on-demand OAGW provisioning failed");
            }
        }
        self.resolver.resolve(&p.provider_id, p.tenant_id).ok()
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<Instant>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ids of the providers still deferred (sorted).
    fn pending_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock_pending().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Retry the `deferred` providers (and any other still pending one) in the
    /// background until all are provisioned or `cancel` fires.
    #[must_use]
    pub fn spawn_reconcile(
        self: Arc<Self>,
        deferred: Vec<String>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move { self.reconcile(deferred, cancel).await })
    }

    async fn reconcile(&self, deferred: Vec<String>, cancel: CancellationToken) {
        {
            let mut pending = self.lock_pending();
            for id in deferred {
                pending.entry(id).or_insert(None);
            }
        }
        let started = Instant::now();
        let mut delay = self.timings.first_delay;
        let mut warned = false;
        while !self.pending_ids().is_empty() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(self.timings.max_delay);
            let pending = self.retry().await;
            if !pending.is_empty() && !warned && started.elapsed() >= self.timings.warn_after {
                warned = true;
                warn!(
                    providers = %pending.join(", "),
                    "OAGW provisioning still pending; chat requests to these providers fail until their credstore secrets are readable"
                );
            }
        }
    }

    /// One reconcile attempt; returns the providers still pending.
    #[allow(clippy::cognitive_complexity)] // tracing macros inflate the score
    async fn retry(&self) -> Vec<String> {
        let ctx = match self.s2s.get() {
            Ok(ctx) => ctx,
            Err(err) => {
                warn!(%err, "OAGW provisioning retry skipped");
                return self.pending_ids();
            }
        };
        for id in self.pending_ids() {
            let Some((_, entry)) = self.providers.iter().find(|(p, _)| *p == id) else {
                self.lock_pending().remove(&id);
                continue;
            };
            let Some(gate) = self.gates.get(&id).map(Arc::clone) else {
                continue;
            };
            let _attempt = gate.lock().await;
            if !self.lock_pending().contains_key(&id) {
                continue; // provisioned on demand meanwhile
            }
            match self.provision_provider(&ctx, &id, entry).await {
                Ok(false) => {
                    self.lock_pending().remove(&id);
                    info!(provider = %id, "OAGW provisioning completed");
                }
                Ok(true) => {}
                Err(err) => {
                    warn!(provider = %id, error = %format!("{err:#}"), "OAGW provisioning retry failed");
                }
            }
        }
        self.pending_ids()
    }

    /// Provision every upstream of one provider; `true` when at least one was
    /// deferred.
    #[allow(clippy::cognitive_complexity)] // tracing macros inflate the score
    async fn provision_provider(
        &self,
        ctx: &SecurityContext,
        id: &str,
        entry: &ProviderEntry,
    ) -> anyhow::Result<bool> {
        let specs = route_specs(entry);
        let mut deferred = false;
        for target in UpstreamTarget::all(id, entry) {
            let Some(upstream) = self.ensure_upstream(ctx, &target).await? else {
                deferred = true;
                continue;
            };
            self.resolver
                .set_alias_override(id, target.tenant, &upstream.alias);
            self.ensure_routes(ctx, &target, &upstream, &specs).await?;
            info!(
                provider = %id,
                tenant = ?target.tenant,
                alias = %upstream.alias,
                "OAGW upstream and routes provisioned"
            );
        }
        Ok(deferred)
    }

    /// The upstream of `target` (created or reused); `None` when deferred.
    async fn ensure_upstream(
        &self,
        ctx: &SecurityContext,
        target: &UpstreamTarget,
    ) -> anyhow::Result<Option<Upstream>> {
        let err = match self
            .gw
            .create_upstream(ctx.clone(), target.request(true))
            .await
        {
            Ok(up) => return Ok(Some(up)),
            Err(err) => err,
        };
        match classify(&err) {
            Failure::AlreadyExists => self.reuse_upstream(ctx, target, &target.alias).await.map(Some),
            Failure::Deferred => Ok(None),
            Failure::Fatal => Err(fatal(target, &err)),
            Failure::AutoDerivedAlias => {
                let err = match self
                    .gw
                    .create_upstream(ctx.clone(), target.request(false))
                    .await
                {
                    Ok(up) => return Ok(Some(up)),
                    Err(err) => err,
                };
                match classify(&err) {
                    Failure::AlreadyExists => self
                        .reuse_upstream(ctx, target, &target.derived_alias())
                        .await
                        .map(Some),
                    Failure::Deferred => Ok(None),
                    Failure::Fatal | Failure::AutoDerivedAlias => Err(fatal(target, &err)),
                }
            }
        }
    }

    /// The existing upstream registered under `alias`, provided it serves the
    /// same endpoint with the same auth as `target` (else the target would be
    /// silently routed through another upstream and its credentials).
    async fn reuse_upstream(
        &self,
        ctx: &SecurityContext,
        target: &UpstreamTarget,
        alias: &str,
    ) -> anyhow::Result<Upstream> {
        let up = self.find_upstream(ctx, target, alias).await?;
        if !target.matches(&up) {
            return Err(anyhow::anyhow!(
                "OAGW provisioning of {} failed: the existing upstream '{}' (endpoints {}) has another endpoint or auth than the configured {}; configure a distinct host or the same auth",
                target.label(),
                up.alias,
                up.server
                    .endpoints
                    .iter()
                    .map(describe_endpoint)
                    .collect::<Vec<_>>()
                    .join(", "),
                describe_endpoint(&target.endpoint),
            ));
        }
        Ok(up)
    }

    /// The existing upstream registered under `alias`.
    async fn find_upstream(
        &self,
        ctx: &SecurityContext,
        target: &UpstreamTarget,
        alias: &str,
    ) -> anyhow::Result<Upstream> {
        let wanted = alias.to_ascii_lowercase();
        let wanted = wanted.trim_end_matches('.');
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
                .await
                .map_err(|e| fatal(target, &e))?;
            if let Some(up) = page.iter().find(|u| u.alias == wanted) {
                return Ok(up.clone());
            }
            if page.len() < LIST_PAGE as usize {
                return Err(anyhow::anyhow!(
                    "OAGW provisioning of {} failed: OAGW reports upstream alias '{wanted}' as existing but does not list it",
                    target.label()
                ));
            }
            skip += LIST_PAGE;
        }
    }

    async fn ensure_routes(
        &self,
        ctx: &SecurityContext,
        target: &UpstreamTarget,
        upstream: &Upstream,
        specs: &[RouteSpec],
    ) -> anyhow::Result<()> {
        for (i, spec) in specs.iter().enumerate() {
            let req = CreateRouteRequest::builder(
                upstream.id,
                MatchRules {
                    http: Some(HttpMatch {
                        methods: vec![spec.method],
                        path: spec.path.clone(),
                        query_allowlist: spec.query_allowlist.clone(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
            )
            .build();
            match self.gw.create_route(ctx.clone(), req).await {
                Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => {}
                Err(err) if i == 0 => return Err(fatal(target, &err)),
                Err(err) => warn!(
                    provider = %target.provider_id,
                    tenant = ?target.tenant,
                    method = ?spec.method,
                    path = %spec.path,
                    %err,
                    "OAGW RAG route not provisioned; file and vector store operations of this provider may fail"
                ),
            }
        }
        Ok(())
    }
}

fn fatal(target: &UpstreamTarget, err: &CanonicalError) -> anyhow::Error {
    anyhow::anyhow!("OAGW provisioning of {} failed: {err}", target.label())
}

#[cfg(test)]
#[path = "provisioning_tests.rs"]
mod provisioning_tests;
