//! OAGW provisioning with deferred retry (algorithm `oagw-provisioning`, state machine
//! `provider-provisioning`).
//!
//! The first pass exchanges `client_credentials` for an S2S context through `authn_resolver`
//! and, for every provider entry and every tenant override with its own host or alias,
//! creates (or reuses) the upstream under the alias and the chat and RAG routes. A credstore
//! secret that is not yet readable defers the entry; any other failure is a startup error.

use std::collections::HashMap;

use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayError,
    SharingMode,
};
use thiserror::Error;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::ClientCredentials;
use crate::config::StorageKind;
use crate::domain::provider_registry::{EffectiveProvider, ProviderRegistry};

/// One upstream to provision: a provider entry or a tenant override of it.
#[derive(Debug, Clone)]
pub struct ProvisionTarget {
    /// Names the entry in errors and warnings: the provider id, and for an override the
    /// provider id followed by `/` and the tenant key.
    pub label: String,
    pub provider: EffectiveProvider,
}

/// A startup error of the provisioning.
#[derive(Debug, Error)]
pub enum ProvisioningError {
    /// The `client_credentials` exchange failed.
    #[error("S2S credentials exchange failed: {0}")]
    S2sExchange(String),
    /// An entry is deterministically misconfigured (anything but an unreadable secret).
    #[error("provider `{provider}` is misconfigured: {reason}")]
    Misconfigured { provider: String, reason: String },
}

/// What the first pass returns.
#[derive(Debug)]
pub struct FirstPassOutcome {
    /// The S2S context, reused by the reconcile task.
    pub s2s: SecurityContext,
    /// The entries whose secret was not yet readable.
    pub deferred: Vec<ProvisionTarget>,
}

/// The result of provisioning one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOutcome {
    /// Upstream and routes created or reused.
    Provisioned,
    /// The credstore secret is not yet readable.
    Deferred,
}

/// `subject` of the precondition violation OAGW raises when the credstore secret of the auth
/// config is not readable yet, and its `type`.
const SECRET_REF_SUBJECT: &str = "auth.config.secret_ref";
const SECRET_REF_TYPE: &str = "STATE";

/// What creating the upstream of an entry came to.
enum UpstreamStep {
    /// The upstream exists (created or reused) under this id.
    Ready(Uuid),
    /// The credstore secret of the entry is not yet readable.
    SecretUnreadable,
}

/// The provisioning targets of a registry: every entry, plus every tenant override that has
/// its own host or alias. Works on copies, so nothing OAGW returns reaches the registry.
#[must_use]
pub fn provision_targets(registry: &ProviderRegistry) -> Vec<ProvisionTarget> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-copy
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-each-entry
    let mut targets = Vec::new();
    for entry in registry.entries() {
        targets.push(ProvisionTarget {
            label: entry.base.provider_id.clone(),
            provider: entry.base.clone(),
        });
        for (tenant, over) in &entry.overrides {
            // an override that keeps the entry host and alias needs no upstream of its own
            if over.host != entry.base.host || over.upstream_alias != entry.base.upstream_alias {
                targets.push(ProvisionTarget {
                    label: format!("{}/{tenant}", entry.base.provider_id),
                    provider: over.clone(),
                });
            }
        }
    }
    targets
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-each-entry
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-copy
}

/// Two targets that resolve to the same OAGW alias but would need different upstreams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasConflict {
    pub alias: String,
    pub first: String,
    pub second: String,
    /// What differs: the endpoint (scheme, host, port) or the auth (plugin type, secret ref).
    pub reason: &'static str,
}

/// The endpoint and the auth identity of a target, as the alias sharing is judged by.
fn upstream_identity(
    provider: &EffectiveProvider,
) -> (bool, String, u16, Option<&str>, Option<&str>) {
    (
        provider.use_http,
        provider
            .host
            .to_ascii_lowercase()
            .trim_end_matches('.')
            .to_owned(),
        provider.port,
        provider.auth_plugin_type.as_deref(),
        provider.auth_config.get("secret_ref").map(String::as_str),
    )
}

/// The targets that share an alias with another target but differ in endpoint or auth.
/// OAGW keeps one upstream per alias, so the later target would silently reuse the earlier
/// one. Identical duplicates are fine and are not reported.
#[must_use]
pub fn alias_conflicts(targets: &[ProvisionTarget]) -> Vec<AliasConflict> {
    let mut seen: HashMap<String, &ProvisionTarget> = HashMap::new();
    let mut conflicts = Vec::new();
    for target in targets {
        let alias = target.provider.upstream_alias.to_ascii_lowercase();
        let Some(first) = seen.get(&alias) else {
            seen.insert(alias, target);
            continue;
        };
        let (a, b) = (
            upstream_identity(&first.provider),
            upstream_identity(&target.provider),
        );
        let reason = if (a.0, &a.1, a.2) != (b.0, &b.1, b.2) {
            "endpoint (scheme, host or port)"
        } else if (a.3, a.4) != (b.3, b.4) {
            "auth (plugin type or secret_ref)"
        } else {
            continue;
        };
        conflicts.push(AliasConflict {
            alias,
            first: first.label.clone(),
            second: target.label.clone(),
            reason,
        });
    }
    conflicts
}

/// The upstream request of a provider: server (scheme `http` when `use_http`, else `https`;
/// host; port), the HTTP protocol id, the alias, and the auth plugin (`auth_plugin_type`,
/// private sharing, `auth_config` as flat config) when one is configured.
#[must_use]
pub fn upstream_request(provider: &EffectiveProvider) -> CreateUpstreamRequest {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-upstream
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if provider.use_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: provider.host.clone(),
            port: provider.port,
        }],
    };
    let mut builder = CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID)
        .alias(provider.upstream_alias.clone());
    if let Some(plugin_type) = &provider.auth_plugin_type {
        builder = builder.auth(AuthConfig {
            plugin_type: plugin_type.clone(),
            sharing: SharingMode::Private,
            config: Some(
                provider
                    .auth_config
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<HashMap<_, _>>(),
            ),
        });
    }
    builder.build()
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-upstream
}

/// The chat route: `POST` on the prefix derived from `api_path` (the text before `{model}`
/// or before the query, without a trailing `/`), path suffix `Append`, and the query
/// allowlist taken from the query of `api_path`.
#[must_use]
pub fn chat_route_match(provider: &EffectiveProvider) -> MatchRules {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-chat-route
    let (path, query) = provider
        .api_path
        .split_once('?')
        .unwrap_or((provider.api_path.as_str(), ""));
    // the `{model}` placeholder and what follows it are matched as the path suffix
    let prefix = path
        .split_once("{model}")
        .map_or(path, |(before, _)| before);
    let query_allowlist = query
        .split('&')
        .filter_map(|pair| pair.split('=').next())
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect();
    http_match(
        HttpMethod::Post,
        prefix.trim_end_matches('/'),
        PathSuffixMode::Append,
        query_allowlist,
    )
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-chat-route
}

fn http_match(
    method: HttpMethod,
    path: &str,
    path_suffix_mode: PathSuffixMode,
    query_allowlist: Vec<String>,
) -> MatchRules {
    MatchRules {
        http: Some(HttpMatch {
            methods: vec![method],
            path: path.to_owned(),
            query_allowlist,
            path_suffix_mode,
        }),
        grpc: None,
    }
}

/// The five RAG routes: `POST {prefix}/files`, `DELETE {prefix}/files/*`,
/// `POST {prefix}/vector_stores*`, `DELETE {prefix}/vector_stores/*` and
/// `GET {prefix}/vector_stores/*`; prefix `/v1` for `openai`, `/openai` for `azure`, which
/// also allows the `api-version` query parameter.
#[must_use]
pub fn rag_route_matches(provider: &EffectiveProvider) -> Vec<MatchRules> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-has-storage
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-rag-routes
    let (prefix, query) = match provider.storage_kind {
        StorageKind::Openai => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    let route = |method, suffix: &str, mode| {
        http_match(method, &format!("{prefix}/{suffix}"), mode, query.clone())
    };
    vec![
        route(HttpMethod::Post, "files", PathSuffixMode::Disabled),
        route(HttpMethod::Delete, "files", PathSuffixMode::Append),
        route(HttpMethod::Post, "vector_stores", PathSuffixMode::Append),
        route(HttpMethod::Delete, "vector_stores", PathSuffixMode::Append),
        route(HttpMethod::Get, "vector_stores", PathSuffixMode::Append),
    ]
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-rag-routes
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-has-storage
}

/// Exchange `client_credentials` through `authn_resolver`.
///
/// # Errors
///
/// [`ProvisioningError::S2sExchange`] for any exchange failure.
pub async fn exchange_s2s(
    authn: &dyn AuthNResolverClient,
    credentials: &ClientCredentials,
) -> Result<SecurityContext, ProvisioningError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-s2s
    let request = ClientCredentialsRequest {
        client_id: credentials.client_id.clone(),
        client_secret: credentials.client_secret.clone(),
        scopes: Vec::new(),
    };
    let exchanged = authn.exchange_client_credentials(&request).await;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-s2s
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-s2s-failed
    match exchanged {
        Ok(result) => Ok(result.security_context),
        Err(error) => {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-return-s2s-error
            // the error never carries the secret: only the resolver's own message is kept
            Err(ProvisioningError::S2sExchange(error.to_string()))
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-return-s2s-error
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-s2s-failed
}

/// The id of the upstream OAGW already holds under `alias`.
async fn existing_upstream_id(
    oagw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    alias: &str,
) -> Result<Uuid, String> {
    let mut query = ListQuery::default();
    loop {
        let page = oagw
            .list_upstreams(s2s.clone(), &query)
            .await
            .map_err(|e| {
                format!(
                    "cannot list the upstreams: {}",
                    ServiceGatewayError::from(e)
                )
            })?;
        if let Some(found) = page
            .iter()
            .find(|upstream| upstream.alias.eq_ignore_ascii_case(alias))
        {
            return Ok(found.id);
        }
        let fetched = u32::try_from(page.len()).unwrap_or(u32::MAX);
        if fetched < query.top {
            return Err(format!(
                "OAGW reports the upstream `{alias}` as existing but does not list it"
            ));
        }
        query.skip = query.skip.saturating_add(fetched);
    }
}

/// Create the upstream of the entry, or reuse it when OAGW reports it already exists. `Err`
/// carries the reason of a deterministic misconfiguration.
async fn create_or_reuse_upstream(
    oagw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    provider: &EffectiveProvider,
) -> Result<UpstreamStep, String> {
    let created = oagw
        .create_upstream(s2s.clone(), upstream_request(provider))
        .await;
    match created.map_err(classify) {
        Ok(upstream) => Ok(UpstreamStep::Ready(upstream.id)),
        Err(Failure::AlreadyExists) => existing_upstream_id(oagw, s2s, &provider.upstream_alias)
            .await
            .map(UpstreamStep::Ready),
        Err(Failure::SecretUnreadable) => Ok(UpstreamStep::SecretUnreadable),
        Err(Failure::Other(reason)) => Err(reason),
    }
}

/// How an OAGW failure is read by the provisioning.
enum Failure {
    /// The `auth.config.secret_ref` precondition: the secret is not readable yet.
    SecretUnreadable,
    /// The resource already exists.
    AlreadyExists,
    /// Anything else.
    Other(String),
}

fn classify(error: CanonicalError) -> Failure {
    match ServiceGatewayError::from(error) {
        ServiceGatewayError::FailedPrecondition {
            precondition_type,
            subject,
            ..
        } if subject == SECRET_REF_SUBJECT && precondition_type == SECRET_REF_TYPE => {
            Failure::SecretUnreadable
        }
        ServiceGatewayError::AlreadyExists { .. } => Failure::AlreadyExists,
        other => Failure::Other(other.to_string()),
    }
}

/// The error of a failed route creation, or `None` when OAGW reports the route as already
/// existing (an identical enabled route on a reused upstream): that counts as provisioned.
fn route_failure(error: CanonicalError) -> Option<ServiceGatewayError> {
    match ServiceGatewayError::from(error) {
        ServiceGatewayError::AlreadyExists { .. } => None,
        other => Some(other),
    }
}

/// Create the chat route and the RAG routes of an upstream. A failing chat route is a
/// misconfiguration; a failing RAG route only degrades RAG.
async fn provision_routes(
    oagw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    upstream_id: Uuid,
    target: &ProvisionTarget,
) -> Result<(), String> {
    let chat = CreateRouteRequest::builder(upstream_id, chat_route_match(&target.provider)).build();
    if let Err(error) = oagw.create_route(s2s.clone(), chat).await
        && let Some(error) = route_failure(error)
    {
        return Err(format!("cannot create the chat route: {error}"));
    }
    for rag in rag_route_matches(&target.provider) {
        let request = CreateRouteRequest::builder(upstream_id, rag).build();
        if let Err(error) = oagw.create_route(s2s.clone(), request).await
            && let Some(error) = route_failure(error)
        {
            tracing::warn!(
                provider = %target.label,
                %error,
                "cannot create a RAG route; RAG is degraded for this provider"
            );
        }
    }
    Ok(())
}

/// Provision one entry: create the upstream (or reuse it when OAGW reports it already
/// exists), then the chat route and the RAG routes (a failing RAG route only degrades RAG).
///
/// # Errors
///
/// [`ProvisioningError::Misconfigured`] for a failure that is not an unreadable secret.
pub async fn provision_entry(
    oagw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    target: &ProvisionTarget,
) -> Result<EntryOutcome, ProvisioningError> {
    let outcome = match create_or_reuse_upstream(oagw, s2s, &target.provider).await {
        Ok(UpstreamStep::Ready(upstream_id)) => provision_routes(oagw, s2s, upstream_id, target)
            .await
            .map(|()| EntryOutcome::Provisioned),
        Ok(UpstreamStep::SecretUnreadable) => Ok(EntryOutcome::Deferred),
        Err(reason) => Err(reason),
    };
    match outcome {
        Ok(EntryOutcome::Provisioned) => {
            // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-provisioned
            tracing::debug!(provider = %target.label, "provider provisioned in OAGW");
            Ok(EntryOutcome::Provisioned)
            // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-provisioned
        }
        Ok(EntryOutcome::Deferred) => {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-secret-unreadable
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-defer
            // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-deferred
            // the caller adds the entry to the deferred set; nothing is retried here
            tracing::debug!(
                provider = %target.label,
                "the credstore secret of the provider is not yet readable"
            );
            Ok(EntryOutcome::Deferred)
            // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-deferred
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-defer
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-secret-unreadable
        }
        Err(reason) => {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-misconfigured
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-return-misconfigured
            // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-failed
            Err(ProvisioningError::Misconfigured {
                provider: target.label.clone(),
                reason,
            })
            // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-failed
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-return-misconfigured
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-misconfigured
        }
    }
}

/// The synchronous first pass: exchange the credentials, provision every target once and
/// return the deferred set right away, without retrying.
///
/// # Errors
///
/// [`ProvisioningError`] for a failed exchange or a deterministically misconfigured entry.
pub async fn first_pass(
    authn: &dyn AuthNResolverClient,
    oagw: &dyn ServiceGatewayClientV1,
    credentials: &ClientCredentials,
    registry: &ProviderRegistry,
) -> Result<FirstPassOutcome, ProvisioningError> {
    let s2s = exchange_s2s(authn, credentials).await?;
    let mut deferred = Vec::new();
    let targets = provision_targets(registry);
    for conflict in alias_conflicts(&targets) {
        tracing::warn!(
            alias = %conflict.alias,
            first = %conflict.first,
            second = %conflict.second,
            "providers share an OAGW upstream alias but differ in {}; \
             the second one reuses the upstream of the first",
            conflict.reason
        );
    }
    for target in targets {
        match provision_entry(oagw, &s2s, &target).await? {
            EntryOutcome::Provisioned => {}
            EntryOutcome::Deferred => {
                tracing::warn!(
                    provider = %target.label,
                    "the credstore secret of the provider is not yet readable; \
                     OAGW provisioning continues in the background"
                );
                deferred.push(target);
            }
        }
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-return
    // right after the first pass, without retrying the deferred entries
    Ok(FirstPassOutcome { s2s, deferred })
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-provisioning:p1:inst-prov-return
}

#[cfg(test)]
#[path = "provisioning_tests.rs"]
mod provisioning_tests;
