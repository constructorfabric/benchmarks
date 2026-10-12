//! OAGW provisioning (DESIGN section 3.2 "OAGW provisioning", section 3.5
//! "LLM Provider" route table, ADR-0005).
//!
//! At gear start the gear obtains an S2S security context from
//! `authn_resolver` (client credentials, [`AuthnSystemContext`]) and registers,
//! for every provider entry and every tenant override with its own alias, an
//! OAGW upstream under that alias (`host`, `port`, `use_http`, auth plugin) and
//! its routes:
//!
//! - the chat route: `POST` on the prefix derived from `api_path` (a `{model}`
//!   placeholder becomes a path suffix; the query allowlist comes from the
//!   `api_path` query);
//! - the RAG routes (best effort: a failure is only logged): `POST
//!   {prefix}/files`, `DELETE {prefix}/files/*`, `POST {prefix}/vector_stores*`,
//!   `GET`/`DELETE {prefix}/vector_stores/*`, with prefix `/v1` (`openai`) or
//!   `/openai` and the `api-version` query (`azure`).
//!
//! An `anthropic_messages` upstream also forwards the `anthropic-version` and
//! `anthropic-beta` request headers (OAGW forwards no inbound header by
//! default) and always has the Anthropic Files API routes `POST /v1/files`
//! and `DELETE /v1/files/*` (secondary image copies).
//!
//! An upstream or route that OAGW reports as existing is reused. Registration
//! runs on a copy of the entries ([`plan_upstreams`]); the alias OAGW returns is
//! reported in [`RegisteredUpstream`] and never reaches the resolver.
//!
//! A deterministic misconfiguration (OAGW validation failure, rejected S2S
//! credentials) fails startup. Transient failures, notably a credstore secret
//! that is not readable yet, leave the entry pending; [`spawn_reconcile`]
//! retries pending entries in the background: first 2 s after start, the
//! interval then doubling up to 60 s, one warning after 2 minutes naming the
//! providers still pending, until the gear stops.
//!
//! Chat and RAG requests are proxied with the same S2S context
//! ([`SystemContextSource`]): the upstreams belong to its tenant.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, AuthNResolverError, ClientCredentialsRequest};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HeadersConfig, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PassthroughMode, PathSuffixMode, RequestHeaderRules, Scheme,
    Server, ServiceGatewayError, SharingMode, Upstream,
};
use secrecy::SecretString;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::{ClientCredentials, ProviderEntry, ProviderKind, StorageKind, TenantOverride};

/// Delay of the first background retry after start.
pub const FIRST_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Upper bound of the retry interval.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Pending providers are named in one warning after this time.
pub const PENDING_WARNING_AFTER: Duration = Duration::from_secs(120);
/// A cached S2S context is refreshed after this time.
pub const S2S_CONTEXT_TTL: Duration = Duration::from_secs(300);

const LIST_PAGE_SIZE: u32 = 100;
const AZURE_API_VERSION_PARAM: &str = "api-version";
const SECRET_REF_KEY: &str = "secret_ref";

// ── S2S security context ─────────────────────────────────────────────────

/// Failure to obtain the S2S security context.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SystemContextError {
    /// The credentials were rejected (deterministic misconfiguration).
    #[error("S2S client credentials rejected: {0}")]
    Rejected(String),
    /// The exchange failed for another reason (retried).
    #[error("S2S security context unavailable: {0}")]
    Unavailable(String),
}

/// Source of the gear's S2S security context.
#[async_trait]
pub trait SystemContextSource: Send + Sync {
    /// # Errors
    ///
    /// [`SystemContextError`] when no context can be obtained.
    async fn system_context(&self) -> Result<SecurityContext, SystemContextError>;
}

/// S2S context from an `authn_resolver` client-credentials exchange, cached
/// for [`S2S_CONTEXT_TTL`]. When a refresh fails transiently the previous
/// context is used.
pub struct AuthnSystemContext {
    authn: Arc<dyn AuthNResolverClient>,
    client_id: String,
    client_secret: SecretString,
    cached: Mutex<Option<(SecurityContext, Instant)>>,
}

impl std::fmt::Debug for AuthnSystemContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthnSystemContext")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl AuthnSystemContext {
    /// Context source over the `authn_resolver` client and the gear's
    /// `client_credentials`.
    #[must_use]
    pub fn new(authn: Arc<dyn AuthNResolverClient>, credentials: &ClientCredentials) -> Self {
        Self {
            authn,
            client_id: credentials.client_id.clone(),
            client_secret: credentials.client_secret.clone(),
            cached: Mutex::new(None),
        }
    }
}

#[async_trait]
impl SystemContextSource for AuthnSystemContext {
    async fn system_context(&self) -> Result<SecurityContext, SystemContextError> {
        let mut cached = self.cached.lock().await;
        if let Some((ctx, obtained)) = cached.as_ref()
            && obtained.elapsed() < S2S_CONTEXT_TTL
        {
            return Ok(ctx.clone());
        }
        let request = ClientCredentialsRequest {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            scopes: Vec::new(),
        };
        match self.authn.exchange_client_credentials(&request).await {
            Ok(result) => {
                let ctx = result.security_context;
                *cached = Some((ctx.clone(), Instant::now()));
                Ok(ctx)
            }
            Err(err) => {
                let err = classify_authn_error(err);
                if let (SystemContextError::Unavailable(_), Some((ctx, _))) =
                    (&err, cached.as_ref())
                {
                    warn!(target: "mini_chat.oagw", error = %err, "S2S context refresh failed; using the previous context");
                    return Ok(ctx.clone());
                }
                Err(err)
            }
        }
    }
}

fn classify_authn_error(err: AuthNResolverError) -> SystemContextError {
    match err {
        AuthNResolverError::Unauthorized(detail) => SystemContextError::Rejected(detail),
        other => SystemContextError::Unavailable(other.to_string()),
    }
}

// ── Plan ─────────────────────────────────────────────────────────────────

/// Deterministic provisioning failure; fails startup.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("mini-chat OAGW provisioning: {label}: {detail}")]
pub struct ProvisioningError {
    /// Provider entry (or `client_credentials`).
    pub label: String,
    pub detail: String,
}

/// One route to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    pub methods: Vec<HttpMethod>,
    /// Path prefix.
    pub path: String,
    pub query_allowlist: Vec<String>,
    pub path_suffix_mode: PathSuffixMode,
}

impl RoutePlan {
    fn new(
        methods: Vec<HttpMethod>,
        path: String,
        query_allowlist: &[String],
        mode: PathSuffixMode,
    ) -> Self {
        Self {
            methods,
            path,
            query_allowlist: query_allowlist.to_vec(),
            path_suffix_mode: mode,
        }
    }

    fn match_rules(&self) -> MatchRules {
        MatchRules {
            http: Some(HttpMatch {
                methods: self.methods.clone(),
                path: self.path.clone(),
                query_allowlist: self.query_allowlist.clone(),
                path_suffix_mode: self.path_suffix_mode,
            }),
            grpc: None,
        }
    }
}

/// One upstream to register with its routes.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamPlan {
    /// `provider_id`, or `provider_id/tenant_id` for a tenant override.
    pub label: String,
    pub provider_id: String,
    /// Alias passed to OAGW.
    pub alias: String,
    pub endpoint: Endpoint,
    pub auth: Option<AuthConfig>,
    pub chat_route: RoutePlan,
    pub rag_routes: Vec<RoutePlan>,
    /// Request header rules of the upstream (`anthropic_messages` only).
    pub headers: Option<HeadersConfig>,
}

impl UpstreamPlan {
    fn create_request(&self) -> CreateUpstreamRequest {
        let builder = CreateUpstreamRequest::builder(
            Server {
                endpoints: vec![self.endpoint.clone()],
            },
            oagw_sdk::HTTP_PROTOCOL_ID,
        )
        .alias(self.alias.clone());
        let builder = match self.headers.clone() {
            Some(headers) => builder.headers(headers),
            None => builder,
        };
        match self.auth.clone() {
            Some(auth) => builder.auth(auth),
            None => builder,
        }
        .build()
    }
}

/// Plans for every provider entry and every tenant override with an alias of
/// its own, built from a copy of the entries.
///
/// # Errors
///
/// [`ProvisioningError`] for an `api_path` that is not an absolute path.
pub fn plan_upstreams(
    providers: &BTreeMap<String, ProviderEntry>,
) -> Result<Vec<UpstreamPlan>, ProvisioningError> {
    let mut plans = Vec::new();
    for (provider_id, entry) in providers {
        let chat_route = chat_route(&entry.api_path).map_err(|detail| ProvisioningError {
            label: provider_id.clone(),
            detail,
        })?;
        let mut rag_routes = rag_routes(entry.storage_kind);
        let headers = (entry.kind == ProviderKind::AnthropicMessages).then(|| {
            if entry.storage_kind != StorageKind::Openai {
                rag_routes.extend(anthropic_file_routes());
            }
            anthropic_headers()
        });
        let base = UpstreamPlan {
            label: provider_id.clone(),
            provider_id: provider_id.clone(),
            alias: entry
                .upstream_alias
                .clone()
                .unwrap_or_else(|| entry.host.clone()),
            endpoint: endpoint(entry, &entry.host),
            auth: auth_config(
                entry.auth_plugin_type.as_deref(),
                entry.auth_config.as_ref(),
            ),
            chat_route,
            rag_routes,
            headers,
        };
        let overrides: Vec<UpstreamPlan> = entry
            .tenant_overrides
            .iter()
            .filter_map(|(tenant, entry_override)| {
                override_plan(&base, entry, tenant, entry_override)
            })
            .collect();
        plans.push(base);
        plans.extend(overrides);
    }
    Ok(plans)
}

fn override_plan(
    base: &UpstreamPlan,
    entry: &ProviderEntry,
    tenant: &str,
    entry_override: &TenantOverride,
) -> Option<UpstreamPlan> {
    let alias = entry_override
        .upstream_alias
        .as_deref()
        .or(entry_override.host.as_deref())?;
    if normalize_alias(alias) == normalize_alias(&base.alias) {
        debug!(target: "mini_chat.oagw", provider = %base.provider_id, tenant = %tenant, "tenant override shares the provider upstream");
        return None;
    }
    let host = entry_override.host.as_deref().unwrap_or(&entry.host);
    let plugin_type = entry_override
        .auth_plugin_type
        .as_deref()
        .or(entry.auth_plugin_type.as_deref());
    let config = entry_override
        .auth_config
        .as_ref()
        .or(entry.auth_config.as_ref());
    Some(UpstreamPlan {
        label: format!("{}/{tenant}", base.provider_id),
        alias: alias.to_owned(),
        endpoint: endpoint(entry, host),
        auth: auth_config(plugin_type, config),
        ..base.clone()
    })
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

fn auth_config(
    plugin_type: Option<&str>,
    config: Option<&std::collections::HashMap<String, String>>,
) -> Option<AuthConfig> {
    plugin_type.map(|plugin_type| AuthConfig {
        plugin_type: plugin_type.to_owned(),
        sharing: SharingMode::Private,
        config: config.cloned(),
    })
}

/// Chat route of an `api_path`: `POST` on the path, or on the part before
/// `{model}` with the rest as path suffix; the query keys are allowed.
///
/// # Errors
///
/// A description when the path is not absolute.
pub fn chat_route(api_path: &str) -> Result<RoutePlan, String> {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    if !path.starts_with('/') {
        return Err(format!("api_path '{api_path}' must start with '/'"));
    }
    let allowlist = query_keys(query);
    let route = match path.find("{model}") {
        Some(position) => RoutePlan::new(
            vec![HttpMethod::Post],
            path[..position].to_owned(),
            &allowlist,
            PathSuffixMode::Append,
        ),
        None => RoutePlan::new(
            vec![HttpMethod::Post],
            path.to_owned(),
            &allowlist,
            PathSuffixMode::Disabled,
        ),
    };
    Ok(route)
}

fn query_keys(query: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for pair in query.split('&') {
        let key = pair.split_once('=').map_or(pair, |(key, _)| key);
        if !key.is_empty() && !keys.iter().any(|known| known == key) {
            keys.push(key.to_owned());
        }
    }
    keys
}

/// RAG routes of a storage kind (DESIGN section 3.5 route table).
#[must_use]
pub fn rag_routes(storage_kind: StorageKind) -> Vec<RoutePlan> {
    let (prefix, allowlist) = match storage_kind {
        StorageKind::Openai => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec![AZURE_API_VERSION_PARAM.to_owned()]),
    };
    vec![
        RoutePlan::new(
            vec![HttpMethod::Post],
            format!("{prefix}/files"),
            &allowlist,
            PathSuffixMode::Disabled,
        ),
        RoutePlan::new(
            vec![HttpMethod::Delete],
            format!("{prefix}/files/"),
            &allowlist,
            PathSuffixMode::Append,
        ),
        RoutePlan::new(
            vec![HttpMethod::Post],
            format!("{prefix}/vector_stores"),
            &allowlist,
            PathSuffixMode::Append,
        ),
        RoutePlan::new(
            vec![HttpMethod::Get, HttpMethod::Delete],
            format!("{prefix}/vector_stores/"),
            &allowlist,
            PathSuffixMode::Append,
        ),
    ]
}

/// Request headers an `anthropic_messages` upstream forwards.
pub const ANTHROPIC_FORWARDED_HEADERS: &[&str] = &["anthropic-version", "anthropic-beta"];

/// Header rules of an `anthropic_messages` upstream: forward
/// [`ANTHROPIC_FORWARDED_HEADERS`].
#[must_use]
pub fn anthropic_headers() -> HeadersConfig {
    HeadersConfig {
        request: Some(RequestHeaderRules {
            passthrough: PassthroughMode::Allowlist,
            passthrough_allowlist: ANTHROPIC_FORWARDED_HEADERS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            ..RequestHeaderRules::default()
        }),
        response: None,
    }
}

/// Anthropic Files API routes (`POST /v1/files`, `DELETE /v1/files/*`).
#[must_use]
pub fn anthropic_file_routes() -> Vec<RoutePlan> {
    rag_routes(StorageKind::Openai)
        .into_iter()
        .filter(|route| route.path == "/v1/files" || route.path == "/v1/files/")
        .collect()
}

/// OAGW alias normalization (lowercase, no trailing dots).
fn normalize_alias(alias: &str) -> String {
    alias.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

// ── Registration ─────────────────────────────────────────────────────────

/// An upstream registered (or reused) in OAGW.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredUpstream {
    pub label: String,
    /// Alias OAGW returned.
    pub alias: String,
    pub upstream_id: Uuid,
    /// The upstream already existed.
    pub reused: bool,
}

/// An upstream left for the background reconcile task.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingUpstream {
    pub plan: UpstreamPlan,
    /// Last transient failure.
    pub reason: String,
}

/// Outcome of [`provision`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProvisioningReport {
    pub registered: Vec<RegisteredUpstream>,
    pub pending: Vec<PendingUpstream>,
}

impl ProvisioningReport {
    /// Plans still pending.
    #[must_use]
    pub fn pending_plans(&self) -> Vec<UpstreamPlan> {
        self.pending
            .iter()
            .map(|pending| pending.plan.clone())
            .collect()
    }
}

/// How an OAGW management error is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The object exists: reuse it.
    Exists,
    /// Retry later (secret not readable yet, OAGW or credstore not ready).
    Transient,
    /// Misconfiguration: fail startup.
    Deterministic,
}

/// Classify an OAGW management error. A `secret_ref` that is not accessible
/// yet (precondition) or that the credstore could not check (reported by OAGW
/// as a validation failure) is transient.
#[must_use]
pub fn classify(err: &CanonicalError) -> ErrorClass {
    match ServiceGatewayError::from(err.clone()) {
        ServiceGatewayError::AlreadyExists { .. } => ErrorClass::Exists,
        ServiceGatewayError::Validation { detail, .. } if is_secret_check_failure(&detail) => {
            ErrorClass::Transient
        }
        ServiceGatewayError::Validation { .. }
        | ServiceGatewayError::InvalidTargetHost { .. }
        | ServiceGatewayError::PayloadTooLarge { .. }
        | ServiceGatewayError::PermissionDenied { .. } => ErrorClass::Deterministic,
        _ => ErrorClass::Transient,
    }
}

fn is_secret_check_failure(detail: &str) -> bool {
    detail.contains(SECRET_REF_KEY) && detail.contains("validation failed")
}

enum StepError {
    Pending(String),
    Fatal(String),
}

fn step_error(action: &str, err: &CanonicalError) -> StepError {
    let detail = format!("{action}: {}", ServiceGatewayError::from(err.clone()));
    match classify(err) {
        ErrorClass::Deterministic => StepError::Fatal(detail),
        ErrorClass::Exists | ErrorClass::Transient => StepError::Pending(detail),
    }
}

/// Register every plan once with the S2S context.
///
/// # Errors
///
/// [`ProvisioningError`] for rejected S2S credentials or a deterministic OAGW
/// failure; transient failures are reported as pending.
pub async fn provision(
    gateway: &dyn ServiceGatewayClientV1,
    system: &dyn SystemContextSource,
    plans: &[UpstreamPlan],
) -> Result<ProvisioningReport, ProvisioningError> {
    let ctx = match system.system_context().await {
        Ok(ctx) => ctx,
        Err(SystemContextError::Rejected(detail)) => {
            return Err(ProvisioningError {
                label: "client_credentials".to_owned(),
                detail,
            });
        }
        Err(err) => return Ok(deferred_report(plans, &err)),
    };
    let mut report = ProvisioningReport::default();
    for plan in plans {
        let outcome = register_upstream(gateway, &ctx, plan).await;
        record_outcome(&mut report, plan, outcome)?;
    }
    Ok(report)
}

/// Every plan pending: the S2S context is not available yet.
fn deferred_report(plans: &[UpstreamPlan], err: &SystemContextError) -> ProvisioningReport {
    warn!(target: "mini_chat.oagw", error = %err, "OAGW provisioning deferred");
    let pending = plans
        .iter()
        .map(|plan| PendingUpstream {
            plan: plan.clone(),
            reason: err.to_string(),
        })
        .collect();
    ProvisioningReport {
        registered: Vec::new(),
        pending,
    }
}

fn record_outcome(
    report: &mut ProvisioningReport,
    plan: &UpstreamPlan,
    outcome: Result<RegisteredUpstream, StepError>,
) -> Result<(), ProvisioningError> {
    match outcome {
        Ok(registered) => {
            info!(target: "mini_chat.oagw", provider = %registered.label, alias = %registered.alias, reused = registered.reused, "provider upstream registered");
            report.registered.push(registered);
            Ok(())
        }
        Err(StepError::Pending(reason)) => {
            warn!(target: "mini_chat.oagw", provider = %plan.label, reason = %reason, "provider upstream registration pending");
            report.pending.push(PendingUpstream {
                plan: plan.clone(),
                reason,
            });
            Ok(())
        }
        Err(StepError::Fatal(detail)) => Err(ProvisioningError {
            label: plan.label.clone(),
            detail,
        }),
    }
}

async fn register_upstream(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    plan: &UpstreamPlan,
) -> Result<RegisteredUpstream, StepError> {
    let (upstream, reused) = ensure_upstream(gateway, ctx, plan).await?;
    create_route(gateway, ctx, upstream.id, &plan.chat_route).await?;
    for route in &plan.rag_routes {
        if let Err(StepError::Pending(reason) | StepError::Fatal(reason)) =
            create_route(gateway, ctx, upstream.id, route).await
        {
            warn!(target: "mini_chat.oagw", provider = %plan.label, path = %route.path, reason = %reason, "RAG route not registered; RAG is degraded for this provider");
        }
    }
    Ok(RegisteredUpstream {
        label: plan.label.clone(),
        alias: upstream.alias,
        upstream_id: upstream.id,
        reused,
    })
}

async fn ensure_upstream(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    plan: &UpstreamPlan,
) -> Result<(Upstream, bool), StepError> {
    match gateway
        .create_upstream(ctx.clone(), plan.create_request())
        .await
    {
        Ok(upstream) => Ok((upstream, false)),
        Err(err) if classify(&err) == ErrorClass::Exists => {
            find_upstream(gateway, ctx, &plan.alias)
                .await
                .map(|upstream| (upstream, true))
        }
        Err(err) => Err(step_error("create upstream", &err)),
    }
}

async fn find_upstream(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    alias: &str,
) -> Result<Upstream, StepError> {
    let wanted = normalize_alias(alias);
    let mut skip = 0;
    loop {
        let query = ListQuery {
            top: LIST_PAGE_SIZE,
            skip,
        };
        let page = gateway
            .list_upstreams(ctx.clone(), &query)
            .await
            .map_err(|err| step_error("list upstreams", &err))?;
        let page_len = page.len();
        if let Some(upstream) = page
            .into_iter()
            .find(|upstream| normalize_alias(&upstream.alias) == wanted)
        {
            return Ok(upstream);
        }
        if page_len < LIST_PAGE_SIZE as usize {
            return Err(StepError::Pending(format!(
                "upstream '{alias}' reported as existing but not listed"
            )));
        }
        skip += LIST_PAGE_SIZE;
    }
}

async fn create_route(
    gateway: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    upstream_id: Uuid,
    route: &RoutePlan,
) -> Result<(), StepError> {
    let request = CreateRouteRequest::builder(upstream_id, route.match_rules()).build();
    match gateway.create_route(ctx.clone(), request).await {
        Ok(_) => Ok(()),
        Err(err) if classify(&err) == ErrorClass::Exists => Ok(()),
        Err(err) => Err(step_error("create route", &err)),
    }
}

// ── Background reconcile ─────────────────────────────────────────────────

/// Delay before retry number `attempt` (0-based): 2 s, doubling, capped at
/// 60 s.
#[must_use]
pub fn retry_delay(attempt: u32) -> Duration {
    let factor = 1_u32.checked_shl(attempt).unwrap_or(u32::MAX);
    FIRST_RETRY_DELAY
        .checked_mul(factor)
        .map_or(MAX_RETRY_DELAY, |delay| delay.min(MAX_RETRY_DELAY))
}

/// Whether the single "still pending" warning is due.
#[must_use]
pub fn pending_warning_due(since_start: Duration, already_warned: bool) -> bool {
    !already_warned && since_start >= PENDING_WARNING_AFTER
}

/// Retry `pending` once; returns the plans still pending. Plans that fail
/// deterministically are dropped with an error log (startup is over).
pub async fn retry_pending(
    gateway: &dyn ServiceGatewayClientV1,
    system: &dyn SystemContextSource,
    pending: Vec<UpstreamPlan>,
) -> Vec<UpstreamPlan> {
    let ctx = match system.system_context().await {
        Ok(ctx) => ctx,
        Err(err) => {
            debug!(target: "mini_chat.oagw", error = %err, "S2S context unavailable for OAGW reconcile");
            return pending;
        }
    };
    let mut still_pending = Vec::new();
    for plan in pending {
        let outcome = register_upstream(gateway, &ctx, &plan).await;
        if keep_pending(&plan, outcome) {
            still_pending.push(plan);
        }
    }
    still_pending
}

/// Log a reconcile outcome; `true` when the plan stays pending.
fn keep_pending(plan: &UpstreamPlan, outcome: Result<RegisteredUpstream, StepError>) -> bool {
    match outcome {
        Ok(registered) => {
            info!(target: "mini_chat.oagw", provider = %registered.label, alias = %registered.alias, "provider upstream registered by reconcile");
            false
        }
        Err(StepError::Pending(reason)) => {
            debug!(target: "mini_chat.oagw", provider = %plan.label, reason = %reason, "provider upstream still pending");
            true
        }
        Err(StepError::Fatal(detail)) => {
            log_reconcile_gave_up(plan, &detail);
            false
        }
    }
}

fn log_reconcile_gave_up(plan: &UpstreamPlan, detail: &str) {
    error!(target: "mini_chat.oagw", provider = %plan.label, detail = %detail, "provider upstream misconfigured; reconcile gives up");
}

/// Start the background reconcile task for `pending` plans; `None` when there
/// is nothing to retry. The task stops when `cancel` fires (gear stop) or
/// when every plan is registered.
#[must_use]
pub fn spawn_reconcile(
    gateway: Arc<dyn ServiceGatewayClientV1>,
    system: Arc<dyn SystemContextSource>,
    pending: Vec<UpstreamPlan>,
    started: Instant,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if pending.is_empty() {
        return None;
    }
    Some(tokio::spawn(reconcile_loop(
        gateway, system, pending, started, cancel,
    )))
}

async fn reconcile_loop(
    gateway: Arc<dyn ServiceGatewayClientV1>,
    system: Arc<dyn SystemContextSource>,
    mut pending: Vec<UpstreamPlan>,
    started: Instant,
    cancel: CancellationToken,
) {
    let mut attempt = 0_u32;
    let mut warned = false;
    let mut next = started + retry_delay(attempt);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep_until(next) => {}
        }
        pending = retry_pending(gateway.as_ref(), system.as_ref(), pending).await;
        if pending.is_empty() {
            info!(target: "mini_chat.oagw", "all provider upstreams registered");
            return;
        }
        if pending_warning_due(started.elapsed(), warned) {
            warn_still_pending(&pending);
            warned = true;
        }
        attempt = attempt.saturating_add(1);
        next = Instant::now() + retry_delay(attempt);
    }
}

fn warn_still_pending(pending: &[UpstreamPlan]) {
    let labels: Vec<&str> = pending.iter().map(|plan| plan.label.as_str()).collect();
    warn!(target: "mini_chat.oagw", providers = %labels.join(", "), "OAGW provisioning still pending after 2 minutes");
}

#[cfg(test)]
#[path = "oagw_provisioning_tests.rs"]
mod tests;
