//! OAGW provisioning: one upstream (plus chat and storage routes) per provider entry and per
//! tenant override, created under the S2S context (DESIGN "OAGW provisioning", spec 4a.4).
//!
//! [`provision_all`] runs at start. Deterministic misconfiguration (e.g. OAGW validation errors)
//! is fatal; an unreadable credstore secret or a transient OAGW failure defers the provider to
//! the background task started by [`spawn_reconcile`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID,
    HeadersConfig, HttpMatch, HttpMethod, ListQuery, MatchRules, PassthroughMode, PathSuffixMode,
    RequestHeaderRules, Scheme, Server, ServiceGatewayClientV1, ServiceGatewayError, SharingMode,
    Upstream,
};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::resolver::storage_prefix;
use super::s2s::S2sContext;
use crate::config::providers::UpstreamSpec;
use crate::config::{ProviderEntry, StorageKind};

/// `PreconditionViolation.subject` OAGW uses for a secret reference that is not readable yet.
const SECRET_REF_SUBJECT: &str = "auth.config.secret_ref";
/// Request headers OAGW forwards to the provider.
const PASSTHROUGH_HEADERS: [&str; 3] = ["accept", "anthropic-version", "anthropic-beta"];
const FIRST_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(60);
const PENDING_WARNING_AFTER: Duration = Duration::from_secs(120);
const PAGE_SIZE: u32 = 100;

/// Result of a provisioning pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProvisionReport {
    /// Provider ids to retry later (secret not readable yet, OAGW unavailable).
    pub deferred: Vec<String>,
}

enum StepError {
    Deferred(String),
    Fatal(String),
}

fn classify(err: ServiceGatewayError) -> StepError {
    match err {
        ServiceGatewayError::FailedPrecondition {
            subject, detail, ..
        } if subject == SECRET_REF_SUBJECT => {
            StepError::Deferred(format!("secret not readable yet: {detail}"))
        }
        ServiceGatewayError::Unavailable { .. } => {
            StepError::Deferred("OAGW unavailable".to_owned())
        }
        ServiceGatewayError::Timeout => StepError::Deferred("OAGW timed out".to_owned()),
        ServiceGatewayError::Internal { detail } => StepError::Deferred(detail),
        other => StepError::Fatal(other.to_string()),
    }
}

/// Route path and query allowlist of the chat route derived from `api_path`: the path without the
/// query, cut before `/{model}` when present; the allowlist holds the query keys.
fn chat_route(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let path = path.find("/{model}").map_or(path, |at| &path[..at]);
    let path = if path.is_empty() { "/" } else { path };
    let mut keys: Vec<String> = Vec::new();
    for key in query.split('&').filter_map(|kv| kv.split('=').next()) {
        if !key.is_empty() && !keys.iter().any(|k| k == key) {
            keys.push(key.to_owned());
        }
    }
    (path.to_owned(), keys)
}

fn upstream_request(id: &str, spec: &UpstreamSpec) -> CreateUpstreamRequest {
    let endpoint = Endpoint {
        scheme: if spec.use_http {
            Scheme::Http
        } else {
            Scheme::Https
        },
        host: spec.host.clone(),
        port: spec.port,
    };
    let mut builder = CreateUpstreamRequest::builder(
        Server {
            endpoints: vec![endpoint],
        },
        HTTP_PROTOCOL_ID,
    )
    .alias(spec.alias.clone())
    .headers(HeadersConfig {
        request: Some(RequestHeaderRules {
            passthrough: PassthroughMode::Allowlist,
            passthrough_allowlist: PASSTHROUGH_HEADERS.map(str::to_owned).to_vec(),
            ..RequestHeaderRules::default()
        }),
        response: None,
    })
    .tags(vec![
        "mini-chat".to_owned(),
        format!("mini-chat-provider:{id}"),
    ]);
    if let Some(plugin_type) = &spec.auth_plugin_type {
        builder = builder.auth(AuthConfig {
            plugin_type: plugin_type.clone(),
            sharing: SharingMode::Inherit,
            config: spec.auth_config.as_ref().map(|c| {
                c.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<HashMap<_, _>>()
            }),
        });
    }
    builder.build()
}

async fn find_upstream(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    alias: &str,
) -> Result<Option<Upstream>, StepError> {
    let mut skip = 0;
    loop {
        let page = gw
            .list_upstreams(
                s2s.clone(),
                &ListQuery {
                    top: PAGE_SIZE,
                    skip,
                },
            )
            .await
            .map_err(|e| classify(e.into()))?;
        if let Some(found) = page.iter().find(|u| u.alias.eq_ignore_ascii_case(alias)) {
            return Ok(Some(found.clone()));
        }
        if page.len() < PAGE_SIZE as usize {
            return Ok(None);
        }
        skip += PAGE_SIZE;
    }
}

/// Plugin type and config of an upstream's auth, comparable regardless of map order.
type AuthKey<'a> = Option<(&'a str, Option<BTreeMap<&'a String, &'a String>>)>;

fn auth_key(auth: Option<&AuthConfig>) -> AuthKey<'_> {
    auth.map(|a| {
        let config = a.config.as_ref().map(|c| c.iter().collect());
        (a.plugin_type.as_str(), config)
    })
}

/// Logs a warning when the upstream OAGW already holds under `spec.alias` differs from `spec`
/// (endpoint or auth): it is reused as is, so the configured settings are not in effect. Only the
/// names of the differing settings are logged, never their values (secret references).
fn warn_on_drift(id: &str, spec: &UpstreamSpec, existing: &Upstream) {
    let wanted = upstream_request(id, spec);
    let mut differs = Vec::new();
    if existing.server != *wanted.server() {
        differs.push("endpoint");
    }
    if auth_key(existing.auth.as_ref()) != auth_key(wanted.auth()) {
        differs.push("auth");
    }
    if !differs.is_empty() {
        tracing::warn!(provider = id, alias = %spec.alias, differs = %differs.join(", "),
            "existing OAGW upstream differs from the configured one; it is reused as is");
    }
}

async fn ensure_upstream(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    id: &str,
    spec: &UpstreamSpec,
) -> Result<Uuid, StepError> {
    match gw
        .create_upstream(s2s.clone(), upstream_request(id, spec))
        .await
    {
        Ok(upstream) => Ok(upstream.id),
        Err(err) => match ServiceGatewayError::from(err) {
            ServiceGatewayError::AlreadyExists { .. } => {
                let existing = find_upstream(gw, s2s, &spec.alias).await?.ok_or_else(|| {
                    StepError::Fatal(format!(
                        "upstream '{}' already exists but is not listable",
                        spec.alias
                    ))
                })?;
                warn_on_drift(id, spec, &existing);
                Ok(existing.id)
            }
            other => Err(classify(other)),
        },
    }
}

/// Creates a route; an existing identical route (`AlreadyExists`) is reused.
async fn ensure_route(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    upstream_id: Uuid,
    methods: Vec<HttpMethod>,
    path: &str,
    query_allowlist: Vec<String>,
) -> Result<(), StepError> {
    let rules = MatchRules {
        http: Some(HttpMatch {
            methods,
            path: path.to_owned(),
            query_allowlist,
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    };
    match gw
        .create_route(
            s2s.clone(),
            CreateRouteRequest::builder(upstream_id, rules).build(),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(err) => match ServiceGatewayError::from(err) {
            ServiceGatewayError::AlreadyExists { .. } => Ok(()),
            other => Err(classify(other)),
        },
    }
}

async fn provision_provider(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    id: &str,
    entry: &ProviderEntry,
) -> Result<(), StepError> {
    for spec in entry.upstream_specs(id) {
        let upstream_id = ensure_upstream(gw, s2s, id, &spec).await?;

        let (path, allowlist) = chat_route(&entry.api_path);
        ensure_route(
            gw,
            s2s,
            upstream_id,
            vec![HttpMethod::Post],
            &path,
            allowlist,
        )
        .await?;

        let Some(kind) = entry.storage_kind else {
            continue;
        };
        let prefix = storage_prefix(kind);
        let allowlist = || match kind {
            StorageKind::Openai => Vec::new(),
            StorageKind::Azure => vec!["api-version".to_owned()],
        };
        let storage_routes = [
            ("files", vec![HttpMethod::Post, HttpMethod::Delete]),
            (
                "vector_stores",
                vec![HttpMethod::Get, HttpMethod::Post, HttpMethod::Delete],
            ),
        ];
        for (name, methods) in storage_routes {
            let path = format!("{prefix}/{name}");
            // A missing RAG route only degrades RAG: log and go on.
            if let Err(StepError::Deferred(msg) | StepError::Fatal(msg)) =
                ensure_route(gw, s2s, upstream_id, methods, &path, allowlist()).await
            {
                tracing::warn!(provider = id, alias = %spec.alias, %path, error = %msg,
                    "could not provision storage route; RAG may be degraded");
            }
        }
    }
    Ok(())
}

async fn provision_ids<'a>(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    providers: &BTreeMap<String, ProviderEntry>,
    ids: impl IntoIterator<Item = &'a String>,
) -> anyhow::Result<ProvisionReport> {
    let mut report = ProvisionReport::default();
    for id in ids {
        let Some(entry) = providers.get(id) else {
            continue;
        };
        match provision_provider(gw, s2s, id, entry).await {
            Ok(()) => tracing::info!(provider = %id, "OAGW upstream and routes provisioned"),
            Err(StepError::Deferred(reason)) => {
                tracing::warn!(provider = %id, %reason, "OAGW provisioning deferred");
                report.deferred.push(id.clone());
            }
            Err(StepError::Fatal(reason)) => {
                anyhow::bail!("OAGW provisioning of provider '{id}' failed: {reason}");
            }
        }
    }
    Ok(report)
}

/// Provisions every provider entry (and tenant override).
///
/// # Errors
/// A deterministic failure (OAGW validation error, ...) of any entry; the gear must not start.
pub async fn provision_all(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    providers: &BTreeMap<String, ProviderEntry>,
) -> anyhow::Result<ProvisionReport> {
    provision_ids(gw, s2s, providers, providers.keys()).await
}

/// Retries the `deferred` providers in the background until all succeed or `cancel` fires: first
/// retry after 2 s, the interval doubling up to 60 s; after 2 minutes without success one warning
/// names the providers still pending. Returns `None` when there is nothing to retry.
pub fn spawn_reconcile(
    gw: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
    providers: BTreeMap<String, ProviderEntry>,
    deferred: Vec<String>,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if deferred.is_empty() {
        return None;
    }
    Some(tokio::spawn(async move {
        let warn_at = Instant::now() + PENDING_WARNING_AFTER;
        let mut warned = false;
        let mut delay = FIRST_RETRY;
        let mut pending: BTreeSet<String> = deferred.into_iter().collect();
        while !pending.is_empty() {
            let retry_at = Instant::now() + delay;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep_until(retry_at) => break,
                    () = tokio::time::sleep_until(warn_at), if !warned => {
                        warned = true;
                        tracing::warn!(providers = ?pending,
                            "OAGW provisioning still pending after 2 minutes; retrying");
                    }
                }
            }
            delay = (delay * 2).min(MAX_RETRY);
            let ctx = match s2s.get() {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::warn!(error = %e, "OAGW reconcile: no S2S context");
                    continue;
                }
            };
            let ids: Vec<String> = pending.iter().cloned().collect();
            for id in ids {
                match provision_ids(gw.as_ref(), &ctx, &providers, [&id]).await {
                    Ok(report) if report.deferred.is_empty() => {
                        pending.remove(&id);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        // Deterministic failure: retrying cannot help.
                        tracing::error!(provider = %id, error = %e, "OAGW reconcile gave up");
                        pending.remove(&id);
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::providers::derive_alias;
    use crate::test_support::app::test_provider;
    use crate::test_support::gateway::{self, FakeGateway};
    use toolkit_security::SecurityContext;

    fn s2s() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(1))
            .subject_tenant_id(Uuid::from_u128(2))
            .build()
            .expect("ctx")
    }

    fn providers(entry: ProviderEntry) -> BTreeMap<String, ProviderEntry> {
        BTreeMap::from([("openai".to_owned(), entry)])
    }

    fn azure() -> ProviderEntry {
        ProviderEntry {
            host: "res.openai.azure.com".to_owned(),
            port: None,
            use_http: false,
            api_path: "/openai/v1/responses?api-version=2025-03-01-preview".to_owned(),
            storage_kind: Some(StorageKind::Azure),
            api_version: Some("2025-03-01-preview".to_owned()),
            ..test_provider()
        }
    }

    type RouteSummary = (Vec<HttpMethod>, String, Vec<String>);

    /// `(methods, path, allowlist)` of every provisioned route.
    fn routes(gw: &FakeGateway) -> Vec<RouteSummary> {
        gw.routes()
            .into_iter()
            .map(|r| {
                let h = r.match_rules.http.expect("http route");
                (h.methods, h.path, h.query_allowlist)
            })
            .collect()
    }

    #[test]
    fn alias_rules() {
        assert_eq!(
            derive_alias(None, "127.0.0.1", Some(8080), true),
            "127.0.0.1"
        );
        assert_eq!(
            derive_alias(None, "api.openai.com", None, false),
            "api.openai.com"
        );
        assert_eq!(
            derive_alias(None, "api.openai.com", Some(443), false),
            "api.openai.com"
        );
        assert_eq!(derive_alias(None, "localhost", Some(80), true), "localhost");
        assert_eq!(
            derive_alias(None, "localhost", Some(8080), true),
            "localhost:8080"
        );
        assert_eq!(
            derive_alias(None, "localhost", Some(443), true),
            "localhost:443"
        );
        assert_eq!(derive_alias(None, "[::1]", Some(8080), true), "[::1]");
        assert_eq!(derive_alias(Some("x"), "localhost", Some(8080), true), "x");
    }

    #[test]
    fn chat_route_paths() {
        assert_eq!(
            chat_route("/v1/responses"),
            ("/v1/responses".to_owned(), vec![])
        );
        assert_eq!(
            chat_route(
                "/openai/deployments/{model}/chat/completions?api-version=1&x=2&api-version=3"
            ),
            (
                "/openai/deployments".to_owned(),
                vec!["api-version".to_owned(), "x".to_owned()]
            )
        );
        assert_eq!(chat_route("/{model}/v1").0, "/");
    }

    #[tokio::test]
    async fn provisions_upstream_and_routes_openai() {
        let gw = FakeGateway::new();
        let report = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap();
        assert!(report.deferred.is_empty());

        let upstreams = gw.upstreams();
        assert_eq!(upstreams.len(), 1);
        let u = &upstreams[0];
        assert_eq!(u.alias, "127.0.0.1");
        assert_eq!(u.protocol, HTTP_PROTOCOL_ID);
        assert_eq!(u.tags, ["mini-chat", "mini-chat-provider:openai"]);
        let ep = &u.server.endpoints[0];
        assert_eq!(
            (ep.scheme, ep.host.as_str(), ep.port),
            (Scheme::Http, "127.0.0.1", 9)
        );
        assert!(u.auth.is_none(), "no auth plugin configured");
        let rules = u.headers.as_ref().and_then(|h| h.request.as_ref()).unwrap();
        assert_eq!(rules.passthrough, PassthroughMode::Allowlist);
        assert_eq!(
            rules.passthrough_allowlist,
            ["accept", "anthropic-version", "anthropic-beta"]
        );

        assert_eq!(
            routes(&gw),
            vec![
                (vec![HttpMethod::Post], "/v1/responses".to_owned(), vec![]),
                (
                    vec![HttpMethod::Post, HttpMethod::Delete],
                    "/v1/files".to_owned(),
                    vec![]
                ),
                (
                    vec![HttpMethod::Get, HttpMethod::Post, HttpMethod::Delete],
                    "/v1/vector_stores".to_owned(),
                    vec![]
                ),
            ]
        );
        assert!(gw.routes().iter().all(|r| {
            let h = r.match_rules.http.as_ref().unwrap();
            h.path_suffix_mode == PathSuffixMode::Append && r.upstream_id == u.id
        }));
    }

    #[tokio::test]
    async fn upstream_auth_is_inherited_and_tenant_overrides_get_their_own_upstream() {
        let mut entry = test_provider();
        entry.auth_plugin_type = Some(crate::config::APIKEY_AUTH_PLUGIN.to_owned());
        entry.auth_config = Some(BTreeMap::from([(
            "secret_ref".to_owned(),
            "cred://a".to_owned(),
        )]));
        entry.tenant_overrides.insert(
            Uuid::from_u128(5).to_string(),
            crate::config::TenantOverride {
                host: Some("127.0.0.2".to_owned()),
                auth_config: Some(BTreeMap::from([(
                    "secret_ref".to_owned(),
                    "cred://b".to_owned(),
                )])),
                ..Default::default()
            },
        );
        let gw = FakeGateway::new();
        provision_all(&gw, &s2s(), &providers(entry)).await.unwrap();

        let upstreams = gw.upstreams();
        assert_eq!(
            upstreams
                .iter()
                .map(|u| u.alias.as_str())
                .collect::<Vec<_>>(),
            ["127.0.0.1", "127.0.0.2"]
        );
        let auth = upstreams[0].auth.as_ref().unwrap();
        assert_eq!(auth.plugin_type, crate::config::APIKEY_AUTH_PLUGIN);
        assert_eq!(auth.sharing, SharingMode::Inherit);
        assert_eq!(auth.config.as_ref().unwrap()["secret_ref"], "cred://a");
        let ov_auth = upstreams[1].auth.as_ref().unwrap();
        assert_eq!(
            ov_auth.plugin_type,
            crate::config::APIKEY_AUTH_PLUGIN,
            "falls back to the entry"
        );
        assert_eq!(ov_auth.config.as_ref().unwrap()["secret_ref"], "cred://b");
        assert_eq!(upstreams[1].server.endpoints[0].host, "127.0.0.2");
        assert_eq!(gw.routes().len(), 6, "three routes per upstream");
    }

    #[tokio::test]
    async fn azure_routes_allowlist_api_version() {
        let gw = FakeGateway::new();
        provision_all(&gw, &s2s(), &providers(azure()))
            .await
            .unwrap();
        let ep = &gw.upstreams()[0].server.endpoints[0];
        assert_eq!((ep.scheme, ep.port), (Scheme::Https, 443));
        let allow = || vec!["api-version".to_owned()];
        assert_eq!(
            routes(&gw),
            vec![
                (
                    vec![HttpMethod::Post],
                    "/openai/v1/responses".to_owned(),
                    allow()
                ),
                (
                    vec![HttpMethod::Post, HttpMethod::Delete],
                    "/openai/files".to_owned(),
                    allow()
                ),
                (
                    vec![HttpMethod::Get, HttpMethod::Post, HttpMethod::Delete],
                    "/openai/vector_stores".to_owned(),
                    allow()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn model_placeholder_route_prefix() {
        let mut entry = azure();
        entry.api_path = "/openai/deployments/{model}/chat/completions".to_owned();
        entry.storage_kind = None;
        let gw = FakeGateway::new();
        provision_all(&gw, &s2s(), &providers(entry)).await.unwrap();
        assert_eq!(
            routes(&gw),
            vec![(
                vec![HttpMethod::Post],
                "/openai/deployments".to_owned(),
                vec![]
            )]
        );
    }

    #[tokio::test]
    async fn already_exists_reuses() {
        let gw = FakeGateway::new();
        let report = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap();
        assert!(report.deferred.is_empty());
        let first = gw.upstreams()[0].id;

        // A second pass (e.g. after a restart against a surviving OAGW) finds everything present.
        let report = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap();
        assert!(report.deferred.is_empty());
        assert_eq!(gw.upstreams().len(), 1);
        assert_eq!(gw.routes().len(), 3);
        assert_eq!(gw.upstreams()[0].id, first);
        assert_eq!(gw.create_upstream_calls(), 2);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn already_exists_with_different_settings_is_reused_with_a_warning() {
        let with_secret = |secret: &str| ProviderEntry {
            auth_plugin_type: Some(crate::config::APIKEY_AUTH_PLUGIN.to_owned()),
            auth_config: Some(BTreeMap::from([(
                "secret_ref".to_owned(),
                secret.to_owned(),
            )])),
            ..test_provider()
        };
        let gw = FakeGateway::new();
        provision_all(&gw, &s2s(), &providers(with_secret("cred://key-a")))
            .await
            .unwrap();

        // Same settings again (a restart): reused silently.
        provision_all(&gw, &s2s(), &providers(with_secret("cred://key-a")))
            .await
            .unwrap();
        assert!(!logs_contain("differs from the configured one"));

        // Another key under the same alias (e.g. an upstream left by an earlier configuration):
        // the existing upstream is still reused, but the drift is logged without the secret.
        provision_all(&gw, &s2s(), &providers(with_secret("cred://key-b")))
            .await
            .unwrap();
        assert_eq!(gw.upstreams().len(), 1);
        assert!(logs_contain("differs from the configured one"));
        assert!(logs_contain("auth"));
        assert!(!logs_contain("cred://key-"));
    }

    #[tokio::test]
    async fn already_exists_from_the_gateway_is_resolved_by_alias() {
        let gw = FakeGateway::new();
        // Scripted conflicts on every create: the existing upstream is found by paging the list.
        provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap();
        gw.script_create_upstream(vec![Err(gateway::already_exists_upstream("127.0.0.1"))]);
        gw.script_create_route(vec![
            Err(gateway::already_exists_route()),
            Err(gateway::already_exists_route()),
            Err(gateway::already_exists_route()),
        ]);
        let report = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap();
        assert!(report.deferred.is_empty());
        assert_eq!(gw.upstreams().len(), 1);
        assert_eq!(gw.routes().len(), 3);
    }

    #[tokio::test]
    async fn already_exists_but_not_listable_is_fatal() {
        let gw = FakeGateway::new();
        gw.script_create_upstream(vec![Err(gateway::already_exists_upstream("127.0.0.1"))]);
        let err = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not listable"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn secret_not_readable_is_deferred_then_reconciled() {
        let gw = Arc::new(FakeGateway::new());
        gw.script_create_upstream(vec![
            Err(gateway::secret_not_readable()),
            Err(gateway::secret_not_readable()),
        ]);
        let providers = providers(test_provider());
        let report = provision_all(gw.as_ref(), &s2s(), &providers)
            .await
            .unwrap();
        assert_eq!(report.deferred, ["openai"]);
        assert!(gw.upstreams().is_empty());

        let s2s_ctx = S2sContext::new();
        s2s_ctx.set(s2s());
        let started = Instant::now();
        let handle = spawn_reconcile(
            gw.clone(),
            s2s_ctx,
            providers,
            report.deferred,
            CancellationToken::new(),
        )
        .expect("something to reconcile");
        handle.await.unwrap();

        // Attempt 1 at +2 s fails again, attempt 2 at +2 s + 4 s succeeds.
        assert_eq!(started.elapsed(), Duration::from_secs(6));
        assert_eq!(gw.create_upstream_calls(), 3);
        assert_eq!(gw.upstreams().len(), 1);
        assert_eq!(gw.routes().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_backs_off_to_60_seconds_and_stops_on_cancel() {
        let gw = Arc::new(FakeGateway::new());
        gw.script_create_upstream(
            (0..40)
                .map(|_| Err(gateway::secret_not_readable()))
                .collect(),
        );
        let s2s_ctx = S2sContext::new();
        s2s_ctx.set(s2s());
        let cancel = CancellationToken::new();
        let handle = spawn_reconcile(
            gw.clone(),
            s2s_ctx,
            providers(test_provider()),
            vec!["openai".to_owned()],
            cancel.clone(),
        )
        .unwrap();
        // Attempts at 2, 6, 14, 30, 62, 122, 182 s.
        tokio::time::sleep(Duration::from_secs(190)).await;
        assert_eq!(gw.create_upstream_calls(), 7);
        cancel.cancel();
        handle.await.unwrap();
        assert_eq!(gw.create_upstream_calls(), 7, "no attempt after cancel");
    }

    #[tokio::test]
    async fn nothing_deferred_spawns_nothing() {
        assert!(
            spawn_reconcile(
                Arc::new(FakeGateway::new()),
                S2sContext::new(),
                BTreeMap::new(),
                Vec::new(),
                CancellationToken::new()
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn validation_error_is_fatal() {
        let gw = FakeGateway::new();
        gw.script_create_upstream(vec![Err(gateway::validation_error())]);
        let err = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("openai"), "{err}");
        assert!(gw.upstreams().is_empty());
    }

    #[tokio::test]
    async fn unavailable_and_internal_are_deferred() {
        let gw = FakeGateway::new();
        gw.script_create_upstream(vec![Err(gateway::unavailable_error())]);
        let report = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap();
        assert_eq!(report.deferred, ["openai"]);
        gw.script_create_upstream(vec![Err(gateway::internal_error())]);
        let report = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap();
        assert_eq!(report.deferred, ["openai"]);
    }

    #[tokio::test]
    async fn storage_route_errors_are_skipped_but_chat_route_errors_are_fatal() {
        let gw = FakeGateway::new();
        // chat route ok, files route fails, vector_stores route ok.
        gw.script_create_route(vec![Ok(()), Err(gateway::validation_error()), Ok(())]);
        let report = provision_all(&gw, &s2s(), &providers(test_provider()))
            .await
            .unwrap();
        assert!(report.deferred.is_empty());
        assert_eq!(gw.routes().len(), 2, "RAG degraded, chat route present");

        let gw = FakeGateway::new();
        gw.script_create_route(vec![Err(gateway::validation_error())]);
        assert!(
            provision_all(&gw, &s2s(), &providers(test_provider()))
                .await
                .is_err()
        );
    }
}
