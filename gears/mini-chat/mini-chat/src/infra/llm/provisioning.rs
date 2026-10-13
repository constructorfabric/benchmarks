//! OAGW provisioning at gear start (DESIGN 3.2 "OAGW provisioning", 3.5 "LLM
//! Provider", "Gear lifecycle" `start`).
//!
//! For every `providers.<id>` entry and every tenant override one upstream
//! (alias, endpoint, auth plugin) and one chat route are registered; entries
//! with a `storage_kind` also get best-effort RAG routes. The functions run on
//! a copy of the configuration, so the alias OAGW returns never reaches the
//! provider resolver.
//!
//! Error policy per entry: a canonical `invalid_argument` is a deterministic
//! misconfiguration and fails startup; any other failure (for example a
//! credstore secret that is not readable yet) defers the entry to the
//! background reconcile task.

use std::time::Duration;

use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PathSuffixMode, Route, Scheme, Server,
    ServiceGatewayClientV1, SharingMode, Upstream,
};
use secrecy::ExposeSecret;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use crate::config::{
    ClientCredentialsConfig, MiniChatConfig, ProviderConfig, ProviderTarget, StorageKind,
};

/// First reconcile delay.
const RECONCILE_FIRST_DELAY: Duration = Duration::from_secs(2);

/// Longest reconcile delay.
const RECONCILE_MAX_DELAY: Duration = Duration::from_secs(60);

/// Pending providers are reported once after this long without success.
const PENDING_WARNING_AFTER: Duration = Duration::from_secs(120);

/// Page size of the OAGW list calls.
const PAGE: u32 = 100;

/// Outcome of one provisioning run (provider ids).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionReport {
    /// Fully provisioned providers.
    pub provisioned: Vec<String>,
    /// Providers whose provisioning failed transiently and is retried.
    pub pending: Vec<String>,
}

/// A failure that must stop the gear.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// OAGW rejected the entry as invalid; retrying cannot help.
    #[error("provider `{provider}` is misconfigured: {detail}")]
    Misconfigured {
        /// `providers.<id>` key.
        provider: String,
        /// OAGW's description (never a secret).
        detail: String,
    },
}

/// Delay before reconcile retry `retry` (0-based): 2 s, doubling, capped at 60 s.
#[must_use]
pub fn reconcile_delay(retry: u32) -> Duration {
    RECONCILE_FIRST_DELAY
        .checked_mul(2_u32.checked_pow(retry).unwrap_or(u32::MAX))
        .map_or(RECONCILE_MAX_DELAY, |delay| delay.min(RECONCILE_MAX_DELAY))
}

/// A step failure: deterministic or transient.
enum StepError {
    Fatal(String),
    Transient(String),
}

impl StepError {
    fn from_canonical(error: &CanonicalError) -> Self {
        if matches!(error, CanonicalError::InvalidArgument { .. }) {
            Self::Fatal(error.detail().to_owned())
        } else {
            Self::Transient(format!("{}: {}", error.gts_type(), error.detail()))
        }
    }
}

/// Registers the upstreams and routes of every provider entry.
///
/// # Errors
/// [`ProvisionError::Misconfigured`] for the first deterministically invalid
/// entry.
pub async fn provision_providers(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    config: &MiniChatConfig,
) -> Result<ProvisionReport, ProvisionError> {
    let mut ids: Vec<&String> = config.providers.keys().collect();
    ids.sort();
    let mut report = ProvisionReport::default();
    for id in ids {
        let provider = &config.providers[id];
        match provision_provider(oagw, ctx, provider).await {
            Ok(()) => report.provisioned.push(id.clone()),
            Err(StepError::Fatal(detail)) => {
                return Err(ProvisionError::Misconfigured {
                    provider: id.clone(),
                    detail,
                });
            }
            Err(StepError::Transient(detail)) => {
                tracing::warn!(provider = %id, %detail, "OAGW provisioning deferred");
                report.pending.push(id.clone());
            }
        }
    }
    Ok(report)
}

/// The entry itself and every tenant override, in a stable order.
fn targets(provider: &ProviderConfig) -> Vec<ProviderTarget> {
    let mut overrides: Vec<_> = provider.tenant_overrides.iter().collect();
    overrides.sort_by(|a, b| a.0.cmp(b.0));
    std::iter::once(provider.target())
        .chain(
            overrides
                .into_iter()
                .map(|(_, tenant_override)| provider.tenant_target(tenant_override)),
        )
        .collect()
}

async fn provision_provider(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    provider: &ProviderConfig,
) -> Result<(), StepError> {
    let mut deferred = None;
    for target in targets(provider) {
        match provision_target(oagw, ctx, provider, &target).await {
            Ok(()) => {}
            Err(error @ StepError::Fatal(_)) => return Err(error),
            Err(error @ StepError::Transient(_)) => deferred = Some(error),
        }
    }
    deferred.map_or(Ok(()), Err)
}

async fn provision_target(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    provider: &ProviderConfig,
    target: &ProviderTarget,
) -> Result<(), StepError> {
    let upstream = ensure_upstream(oagw, ctx, target).await?;
    let (path, suffix_mode, query_allowlist) = chat_route_shape(&provider.api_path);
    let chat = RouteShape {
        methods: vec![HttpMethod::Post],
        path,
        suffix_mode,
        query_allowlist,
    };
    ensure_route(oagw, ctx, &upstream, &chat)
        .await
        .map_err(|error| StepError::from_canonical(&error))?;

    // File and vector-store routes only degrade RAG when missing.
    for route in rag_routes(provider.storage_kind) {
        if let Err(error) = ensure_route(oagw, ctx, &upstream, &route).await {
            tracing::warn!(path = %route.path, detail = error.detail(),
                "RAG route was not provisioned");
        }
    }
    Ok(())
}

/// Creates the upstream, or reuses the one OAGW reports as already existing.
async fn ensure_upstream(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    target: &ProviderTarget,
) -> Result<Upstream, StepError> {
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if target.use_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: target.host.clone(),
            port: target.port,
        }],
    };
    let mut builder = CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID).alias(&target.alias);
    if let Some(plugin_type) = &target.auth_plugin_type {
        builder = builder.auth(AuthConfig {
            plugin_type: plugin_type.clone(),
            // Tenants below the S2S tenant resolve the alias through inheritance.
            sharing: SharingMode::Inherit,
            config: target.auth_config.clone(),
        });
    }
    match oagw.create_upstream(ctx.clone(), builder.build()).await {
        Ok(upstream) => Ok(upstream),
        Err(CanonicalError::AlreadyExists { .. }) => find_upstream(oagw, ctx, &target.alias).await,
        Err(error) => Err(StepError::from_canonical(&error)),
    }
}

async fn find_upstream(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    alias: &str,
) -> Result<Upstream, StepError> {
    let mut skip = 0;
    loop {
        let page = oagw
            .list_upstreams(ctx.clone(), &ListQuery { top: PAGE, skip })
            .await
            .map_err(|error| StepError::from_canonical(&error))?;
        let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
        if let Some(found) = page.into_iter().find(|upstream| upstream.alias == alias) {
            return Ok(found);
        }
        if count < PAGE {
            return Err(StepError::Transient(
                "the upstream exists but is not visible".to_owned(),
            ));
        }
        skip += count;
    }
}

/// One route to register.
struct RouteShape {
    methods: Vec<HttpMethod>,
    path: String,
    suffix_mode: PathSuffixMode,
    query_allowlist: Vec<String>,
}

/// Route path, suffix mode and query allowlist of a chat `api_path`: the
/// query is split off (its keys become the allowlist) and the path stops
/// before the first `{model}`-style segment, which becomes a path suffix.
fn chat_route_shape(api_path: &str) -> (String, PathSuffixMode, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let query_allowlist = query
        .split('&')
        .filter_map(|pair| pair.split('=').next())
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect();
    let mut kept = Vec::new();
    let mut has_placeholder = false;
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        if segment.contains('{') {
            has_placeholder = true;
            break;
        }
        kept.push(segment);
    }
    let route_path = format!("/{}", kept.join("/"));
    let mode = if has_placeholder {
        PathSuffixMode::Append
    } else {
        PathSuffixMode::Disabled
    };
    (route_path, mode, query_allowlist)
}

/// File and vector-store routes under the storage prefix (`/v1` for `OpenAI`,
/// `/openai` with the `api-version` query parameter for Azure).
fn rag_routes(storage: StorageKind) -> Vec<RouteShape> {
    let (prefix, query) = match storage {
        StorageKind::OpenAi => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    ["files", "vector_stores"]
        .into_iter()
        .map(|resource| RouteShape {
            methods: vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete],
            path: format!("{prefix}/{resource}"),
            suffix_mode: PathSuffixMode::Append,
            query_allowlist: query.clone(),
        })
        .collect()
}

/// Creates the route unless the upstream already has one with the same path
/// and methods.
async fn ensure_route(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    upstream: &Upstream,
    shape: &RouteShape,
) -> Result<(), CanonicalError> {
    if route_exists(oagw, ctx, upstream, shape).await? {
        return Ok(());
    }
    let rules = MatchRules {
        http: Some(HttpMatch {
            methods: shape.methods.clone(),
            path: shape.path.clone(),
            query_allowlist: shape.query_allowlist.clone(),
            path_suffix_mode: shape.suffix_mode,
        }),
        grpc: None,
    };
    oagw.create_route(
        ctx.clone(),
        CreateRouteRequest::builder(upstream.id, rules).build(),
    )
    .await
    .map(|_| ())
}

async fn route_exists(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    upstream: &Upstream,
    shape: &RouteShape,
) -> Result<bool, CanonicalError> {
    let mut skip = 0;
    loop {
        let page: Vec<Route> = oagw
            .list_routes(
                ctx.clone(),
                Some(upstream.id),
                &ListQuery { top: PAGE, skip },
            )
            .await?;
        let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
        let found = page.iter().any(|route| {
            route
                .match_rules
                .http
                .as_ref()
                .is_some_and(|http| http.path == shape.path && http.methods == shape.methods)
        });
        if found {
            return Ok(true);
        }
        if count < PAGE {
            return Ok(false);
        }
        skip += count;
    }
}

/// Exchanges the configured `client_credentials` for the S2S security
/// context used to register upstreams and routes. The secret is only handed
/// to `authn_resolver`; it is never logged.
///
/// # Errors
/// A description of the failure (never contains the secret).
pub async fn s2s_context(
    authn: &dyn AuthNResolverClient,
    credentials: &ClientCredentialsConfig,
) -> Result<SecurityContext, String> {
    let request = ClientCredentialsRequest {
        client_id: credentials.client_id.clone(),
        client_secret: secrecy::SecretString::from(
            credentials.client_secret.expose_secret().to_owned(),
        ),
        scopes: Vec::new(),
    };
    authn
        .exchange_client_credentials(&request)
        .await
        .map(|result| result.security_context)
        .map_err(|error| format!("client credentials exchange failed: {error}"))
}

/// One full provisioning run: S2S exchange, then registration. An exchange
/// failure defers every provider to the reconcile task.
///
/// # Errors
/// [`ProvisionError::Misconfigured`] only.
pub async fn provision_with_credentials(
    authn: &dyn AuthNResolverClient,
    oagw: &dyn ServiceGatewayClientV1,
    config: &MiniChatConfig,
) -> Result<ProvisionReport, ProvisionError> {
    match s2s_context(authn, &config.client_credentials).await {
        Ok(ctx) => provision_providers(oagw, &ctx, config).await,
        Err(detail) => {
            tracing::warn!(%detail, "OAGW provisioning deferred");
            let mut pending: Vec<String> = config.providers.keys().cloned().collect();
            pending.sort();
            Ok(ProvisionReport {
                provisioned: Vec::new(),
                pending,
            })
        }
    }
}

/// Retries the pending providers until all are provisioned or `cancel` fires:
/// first retry after 2 s, doubling up to 60 s. One warning names the providers
/// still pending after 2 minutes.
///
/// A deterministic misconfiguration found by a retry is logged and stops the
/// retries of that run (startup already completed).
pub async fn reconcile_pending(
    authn: &dyn AuthNResolverClient,
    oagw: &dyn ServiceGatewayClientV1,
    config: &MiniChatConfig,
    mut pending: Vec<String>,
    cancel: CancellationToken,
) {
    let started = tokio::time::Instant::now();
    let mut warned = false;
    let mut retry: u32 = 0;
    while !pending.is_empty() {
        let delay = reconcile_delay(retry);
        retry = retry.saturating_add(1);
        if !sleep_or_cancel(delay, &cancel).await {
            return;
        }
        match provision_with_credentials(authn, oagw, config).await {
            Ok(report) => pending = report.pending,
            Err(error) => {
                tracing::error!(%error, "OAGW provisioning failed permanently");
                return;
            }
        }
        if !warned && started.elapsed() >= PENDING_WARNING_AFTER {
            warned = warn_pending(&pending);
        }
    }
    tracing::info!("OAGW provisioning complete");
}

/// `false` when `cancel` fired before `delay` elapsed.
async fn sleep_or_cancel(delay: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(delay) => true,
    }
}

/// Warns once about the providers still pending; `true` when it warned.
fn warn_pending(pending: &[String]) -> bool {
    if pending.is_empty() {
        return false;
    }
    tracing::warn!(
        providers = ?pending,
        "OAGW provisioning still pending after 2 minutes (is the provider secret readable?)"
    );
    true
}
