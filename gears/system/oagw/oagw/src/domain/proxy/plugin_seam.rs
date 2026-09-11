//! The single plugin/rate-limit invocation seam
//! (`cpt-cf-oagw-dod-plugin-hook-points`, `cpt-cf-oagw-dod-chain-invocation-point`).
//!
//! This is the one place the auth -> guard (rate-limit admission first) ->
//! request-transform -> upstream call -> response-transform chain runs
//! (`cpt-cf-oagw-algo-chain-assembly`, `cpt-cf-oagw-dod-chain-order`). Plain
//! HTTP, server-sent-event streams, and WebSocket upgrades all call the same
//! two functions below, unchanged, so plugin behaviour is identical across
//! transports (`cpt-cf-oagw-feature-streaming-proxy`).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use credstore_sdk::CredStoreClientV1;
use toolkit_auth::oauth2::ClientAuthMethod;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::super::model::{
    AuthConfig, PluginItem, PluginType, RateLimitConfig, parse_gts_plugin_ref,
};
use super::super::plugin::registry::{
    APIKEY_AUTH_PLUGIN_NAME, NOOP_AUTH_PLUGIN_NAME, OAUTH2_CLIENT_CRED_BASIC_PLUGIN_NAME,
    OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME, REQUEST_ID_TRANSFORM_PLUGIN_NAME,
    REQUIRED_HEADERS_GUARD_PLUGIN_NAME,
};
use super::super::plugin::required_headers_guard;
use super::super::plugin::token_cache::TokenCache;
use super::super::plugin::{apikey_auth, noop_auth, oauth2_client_cred_auth, request_id_transform};
use super::super::rate_limit::{self, RateLimitScopeContext, RateLimiter};
use crate::error::OagwError;

/// Fixed, unqualified resource kind rate-limit counter keys are prefixed
/// with (`cpt-cf-oagw-algo-effective-rate-limit`): every counter for one
/// upstream shares this prefix regardless of which tier (upstream, route,
/// tenant) contributed the merged limit.
const RATE_LIMIT_RESOURCE_KIND: &str = "upstream";

/// Everything the request-phase hook needs: the resolved plan's plugin
/// bindings, auth and rate-limit configuration, the outbound request
/// material to inspect or mutate, the calling identity, and the runtime
/// dependencies (credential store, token cache, rate limiter) built-in
/// plugins call on (`cpt-cf-oagw-dod-plugin-hook-points`).
///
/// Widened beyond feature 7's shape (`effective_plugins`, `effective_auth`,
/// `effective_rate_limit`, `method`, `headers`, `body`) with exactly what
/// this feature's built-in plugins need: a mutable header map (auth
/// injection), a mutable outbound query-parameter map (api-key
/// query-placement injection — the query string itself is otherwise
/// immutable `Uri` state the caller owns), the calling tenant/subject and an
/// optional client IP (rate-limit scope, `OAuth2` cache-key isolation), the
/// resolved upstream/route identifiers (rate-limit counter key), the
/// `SecurityContext` and credential-store handle (credential resolution),
/// and the shared token cache / rate limiter / timeout knobs.
pub struct RequestHookContext<'a> {
    pub effective_plugins: &'a [PluginItem],
    pub effective_auth: Option<&'a AuthConfig>,
    pub effective_rate_limit: Option<&'a RateLimitConfig>,
    pub method: &'a Method,
    pub headers: &'a mut HeaderMap,
    pub body: &'a Bytes,
    /// Api-key query-placement overrides, merged into the outbound target
    /// URL's query string by the caller after this hook returns
    /// (`cpt-cf-oagw-algo-apikey-injection`).
    pub query: &'a mut BTreeMap<String, String>,
    pub tenant_id: Uuid,
    pub subject_id: Uuid,
    pub client_ip: Option<&'a str>,
    pub upstream_id: Uuid,
    pub route_id: Uuid,
    pub security_context: &'a SecurityContext,
    pub credstore: &'a dyn CredStoreClientV1,
    pub token_cache: &'a TokenCache,
    pub rate_limiter: &'a RateLimiter,
    pub proxy_timeout: Duration,
    pub token_cache_ttl_ceiling: Duration,
    /// Test-only override threaded to the `OAuth2` plugin's internal HTTP
    /// client configuration; always `None` in production.
    pub oauth_http_config_override: Option<toolkit_http::HttpClientConfig>,
}

/// Carries the request-phase outcome forward to the response-phase hook:
/// the chosen correlation identifier (`cpt-cf-oagw-algo-request-id-propagation`)
/// and, when a rate limit is configured, the advisory header values so an
/// ADMITTED request's response carries them too
/// (`cpt-cf-oagw-algo-rate-limit-headers`).
#[derive(Debug, Clone)]
pub struct RequestChainOutcome {
    pub request_id: String,
    pub rate_limit_headers: Option<rate_limit::RateLimitHeaderValues>,
    /// Header names the chain added or overwrote on the forwarded request
    /// that the caller must force through its header-transformation plan's
    /// passthrough filter regardless of the upstream's configured mode —
    /// the same way `Content-Type` is always forwarded independent of
    /// passthrough (`cpt-cf-oagw-algo-apikey-injection`,
    /// `cpt-cf-oagw-algo-oauth2-token-acquisition`,
    /// `cpt-cf-oagw-algo-request-id-propagation`). Empty when no auth
    /// binding injected a header and no request-id transform is bound.
    pub forced_request_headers: Vec<HeaderName>,
}

/// The single request-phase plugin/rate-limit hook
/// (`cpt-cf-oagw-dod-plugin-hook-points`, `cpt-cf-oagw-dod-chain-invocation-point`).
/// Invoked once, after guard and body validation and before header-plan
/// application and endpoint selection: auth, then rate-limit admission at
/// the head of the guard phase, then guard bindings, then request-transform
/// bindings (`cpt-cf-oagw-dod-chain-order`).
///
/// # Errors
///
/// Returns the mapped [`OagwError`] for the first rejection or failure
/// encountered, in that order: an unresolvable or structurally invalid auth
/// binding, a credential/token failure, rate-limit exhaustion, or a guard
/// rejection (`cpt-cf-oagw-algo-plugin-failure-mapping`).
// @cpt-begin:cpt-cf-oagw-dod-plugin-hook-points:p1:inst-plugin-seam-request-hook-01
// @cpt-begin:cpt-cf-oagw-dod-chain-invocation-point:p2:inst-plugin-seam-request-fn-01
// @cpt-begin:cpt-cf-oagw-algo-chain-assembly:p2:inst-plugin-seam-request-fn-01
pub async fn invoke_request_hooks(
    ctx: &mut RequestHookContext<'_>,
) -> Result<RequestChainOutcome, OagwError> {
    let (guard_items, transform_items) = classify_chain(ctx.effective_plugins)?;

    let mut forced_request_headers = run_auth(ctx).await?;

    let rate_limit_headers = run_rate_limit_admission(ctx)?;

    for item in &guard_items {
        run_guard_request(item, ctx.headers)?;
    }

    // `request_id` is the only built-in transform, so a non-empty
    // classified list means it is the one bound
    // (`cpt-cf-oagw-dod-request-id-transform`); an unbound chain leaves the
    // response-phase hook nothing to propagate.
    let request_id = if transform_items.is_empty() {
        String::new()
    } else {
        let id = request_id_transform::ensure_on_request(ctx.headers);
        forced_request_headers.push(HeaderName::from_static(
            request_id_transform::REQUEST_ID_HEADER,
        ));
        id
    };

    Ok(RequestChainOutcome {
        request_id,
        rate_limit_headers,
        forced_request_headers,
    })
}
// @cpt-end:cpt-cf-oagw-algo-chain-assembly:p2:inst-plugin-seam-request-fn-01
// @cpt-end:cpt-cf-oagw-dod-chain-invocation-point:p2:inst-plugin-seam-request-fn-01
// @cpt-end:cpt-cf-oagw-dod-plugin-hook-points:p1:inst-plugin-seam-request-hook-01

/// Everything the response-phase hook needs: the resolved plan's plugin
/// bindings, the outbound (client-facing) response material to inspect or
/// mutate, and the request-phase outcome to propagate
/// (`cpt-cf-oagw-dod-plugin-hook-points`).
pub struct ResponseHookContext<'a> {
    pub effective_plugins: &'a [PluginItem],
    pub status: StatusCode,
    pub headers: &'a mut HeaderMap,
    pub outcome: &'a RequestChainOutcome,
}

/// The single response-phase plugin hook (`cpt-cf-oagw-dod-plugin-hook-points`).
/// Invoked once after the upstream call completes and the response header
/// plan and CORS headers have been applied, before the response is returned
/// to the caller: guard bindings' response phase, then the request-id
/// transform's response phase, then the rate-limit headers (present on an
/// admitted response too, `cpt-cf-oagw-algo-rate-limit-headers`).
///
/// # Errors
///
/// Returns the mapped [`OagwError`] when a guard's response-phase check
/// rejects (`cpt-cf-oagw-dod-required-headers-guard`) or a bound guard
/// reference is unresolvable.
// @cpt-begin:cpt-cf-oagw-dod-chain-invocation-point:p2:inst-plugin-seam-response-fn-01
pub fn invoke_response_hooks(ctx: &mut ResponseHookContext<'_>) -> Result<(), OagwError> {
    let (guard_items, _transform_items) = classify_chain(ctx.effective_plugins)?;
    for item in &guard_items {
        run_guard_response(item, ctx.headers)?;
    }

    if !ctx.outcome.request_id.is_empty() {
        request_id_transform::ensure_on_response(ctx.headers, &ctx.outcome.request_id);
    }

    if let Some(values) = &ctx.outcome.rate_limit_headers {
        apply_rate_limit_headers(ctx.headers, values);
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-chain-invocation-point:p2:inst-plugin-seam-response-fn-01

// ---------------------------------------------------------------------------
// Chain assembly / runtime plugin resolution
// (`cpt-cf-oagw-algo-chain-assembly`, `cpt-cf-oagw-algo-runtime-plugin-resolution`).
// ---------------------------------------------------------------------------

/// The guard-list / transform-list pair [`classify_chain`] returns.
type ClassifiedChain<'a> = (Vec<&'a PluginItem>, Vec<&'a PluginItem>);

/// Classifies every binding in `items` (already concatenated
/// upstream-then-route-then-tenant by configuration resolution, and
/// preserved in that order here — `cpt-cf-oagw-dod-chain-order`) into the
/// guard list and the request/response-transform list.
///
/// # Errors
///
/// Returns [`OagwError::plugin_not_found`] (`503`) for a malformed
/// reference, a UUID-backed custom-plugin reference (stored-plugin
/// execution is out of scope for this feature — the runtime always reports
/// it unresolved), an auth-typed reference (auth is a dedicated slot, never
/// a `plugins.items` binding), or a named guard/transform reference with no
/// backing implementation — including the six catalogue-only identifiers
/// (`cpt-cf-oagw-dod-runtime-plugin-resolution`).
// The three plugin kinds are separated here: an auth binding is a dedicated
// slot rather than a `plugins.items` entry, a guard binding may admit or
// reject, and a transform binding may mutate the request or response.
// @cpt-begin:cpt-cf-oagw-dod-plugin-kinds:p2:inst-plugin-kinds-classify-01
// @cpt-begin:cpt-cf-oagw-algo-runtime-plugin-resolution:p2:inst-runtime-resolve-items-fn-01
fn classify_chain(items: &[PluginItem]) -> Result<ClassifiedChain<'_>, OagwError> {
    let mut guards = Vec::new();
    let mut transforms = Vec::new();
    for item in items {
        let plugin_ref = item.plugin_ref();
        let (plugin_type, instance) = parse_gts_plugin_ref(plugin_ref).ok_or_else(|| {
            OagwError::plugin_not_found(format!(
                "'{plugin_ref}' is not a resolvable plugin reference"
            ))
        })?;
        if Uuid::parse_str(instance).is_ok() {
            return Err(OagwError::plugin_not_found(format!(
                "'{plugin_ref}' names a custom plugin; executing stored plugin source is out of scope"
            )));
        }
        match plugin_type {
            PluginType::Guard if instance == REQUIRED_HEADERS_GUARD_PLUGIN_NAME => {
                guards.push(item);
            }
            PluginType::Transform if instance == REQUEST_ID_TRANSFORM_PLUGIN_NAME => {
                transforms.push(item);
            }
            PluginType::Guard | PluginType::Transform | PluginType::Auth => {
                return Err(OagwError::plugin_not_found(format!(
                    "no backing implementation for '{plugin_ref}'"
                )));
            }
        }
    }
    Ok((guards, transforms))
}
// @cpt-end:cpt-cf-oagw-algo-runtime-plugin-resolution:p2:inst-runtime-resolve-items-fn-01
// @cpt-end:cpt-cf-oagw-dod-plugin-kinds:p2:inst-plugin-kinds-classify-01

// ---------------------------------------------------------------------------
// Auth phase.
// ---------------------------------------------------------------------------

/// Executes the effective auth binding, if any: at most one per upstream
/// (`cpt-cf-oagw-dod-plugin-kinds`). A binding with no `type` set (an
/// upstream configured with an `auth` object but no auth plugin) and no
/// `effective_auth` at all are both treated as no-op. Returns the header
/// names (if any) the binding injected, so the caller can force them
/// through its passthrough filter regardless of the upstream's configured
/// mode.
async fn run_auth(ctx: &mut RequestHookContext<'_>) -> Result<Vec<HeaderName>, OagwError> {
    let Some(auth) = ctx.effective_auth else {
        return Ok(Vec::new());
    };
    let Some(auth_type) = auth.auth_type.as_deref() else {
        return Ok(Vec::new());
    };
    let (plugin_type, instance) = parse_gts_plugin_ref(auth_type).ok_or_else(|| {
        OagwError::plugin_not_found(format!(
            "'{auth_type}' is not a resolvable plugin reference"
        ))
    })?;
    if !matches!(plugin_type, PluginType::Auth) || Uuid::parse_str(instance).is_ok() {
        return Err(OagwError::plugin_not_found(format!(
            "'{auth_type}' has no backing auth-plugin implementation"
        )));
    }

    let config = auth.config.as_ref();
    match instance {
        NOOP_AUTH_PLUGIN_NAME => noop_auth::authenticate().map(|()| Vec::new()),
        APIKEY_AUTH_PLUGIN_NAME => apikey_auth::inject_credential(
            config,
            ctx.credstore,
            ctx.security_context,
            ctx.proxy_timeout,
            ctx.headers,
            ctx.query,
        )
        .await
        .map(|header_name| header_name.into_iter().collect()),
        OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME | OAUTH2_CLIENT_CRED_BASIC_PLUGIN_NAME => {
            let auth_method = if instance == OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME {
                ClientAuthMethod::Form
            } else {
                ClientAuthMethod::Basic
            };
            let params = oauth2_client_cred_auth::Oauth2Params {
                token_cache_ttl_ceiling: ctx.token_cache_ttl_ceiling,
                proxy_timeout: ctx.proxy_timeout,
                http_config_override: ctx.oauth_http_config_override.clone(),
            };
            oauth2_client_cred_auth::inject_client_credentials(
                auth_method,
                config,
                ctx.credstore,
                ctx.security_context,
                ctx.tenant_id,
                ctx.subject_id,
                ctx.token_cache,
                &params,
                ctx.headers,
            )
            .await
            .map(|()| vec![axum::http::header::AUTHORIZATION])
        }
        _ => Err(OagwError::plugin_not_found(format!(
            "'{auth_type}' has no backing auth-plugin implementation"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Rate-limit admission (`cpt-cf-oagw-algo-token-bucket-admission`).
// ---------------------------------------------------------------------------

/// Runs token-bucket admission at the head of the guard phase
/// (`cpt-cf-oagw-dod-token-bucket-admission`). Returns the advisory header
/// values on admission (to be applied to the eventual response too), or the
/// mapped `429` rejection, carrying `Retry-After` and `X-RateLimit-*` as
/// both raw headers and problem-document context
/// (`cpt-cf-oagw-dod-rate-limit-response`).
fn run_rate_limit_admission(
    ctx: &RequestHookContext<'_>,
) -> Result<Option<rate_limit::RateLimitHeaderValues>, OagwError> {
    let Some(config) = ctx.effective_rate_limit else {
        return Ok(None);
    };
    let effective = rate_limit::effective_rate_limit(config);
    let scope_ctx = RateLimitScopeContext {
        tenant_id: ctx.tenant_id,
        subject_id: ctx.subject_id,
        client_ip: ctx.client_ip,
        route_id: ctx.route_id,
    };
    let key = rate_limit::counter_key(
        RATE_LIMIT_RESOURCE_KIND,
        ctx.upstream_id,
        effective.scope,
        &scope_ctx,
    );
    let outcome = ctx.rate_limiter.admit(&key, &effective);
    let values = rate_limit::rate_limit_headers(&outcome, &effective);

    if !outcome.admitted {
        let retry_after = values.retry_after_secs.unwrap_or(1);
        let mut error = OagwError::rate_limit_exceeded("rate limit exceeded")
            .with_retry_after_seconds(retry_after)
            .with_header(
                axum::http::header::RETRY_AFTER,
                HeaderValue::from(retry_after),
            )
            .with_header(rate_limit_limit_header(), HeaderValue::from(values.limit))
            .with_header(
                rate_limit_remaining_header(),
                HeaderValue::from(values.remaining),
            )
            .with_header(
                rate_limit_reset_header(),
                HeaderValue::from(values.reset_epoch_secs),
            );
        // `with_header` takes `self` by value repeatedly; keep the binding
        // mutable-free by reassigning once more for clarity than chaining
        // an unbounded builder line.
        error = error.with_error_code("RATE_LIMIT_EXCEEDED");
        return Err(error);
    }

    Ok(Some(values))
}

fn apply_rate_limit_headers(headers: &mut HeaderMap, values: &rate_limit::RateLimitHeaderValues) {
    headers.insert(rate_limit_limit_header(), HeaderValue::from(values.limit));
    headers.insert(
        rate_limit_remaining_header(),
        HeaderValue::from(values.remaining),
    );
    headers.insert(
        rate_limit_reset_header(),
        HeaderValue::from(values.reset_epoch_secs),
    );
}

fn rate_limit_limit_header() -> HeaderName {
    HeaderName::from_static("x-ratelimit-limit")
}

fn rate_limit_remaining_header() -> HeaderName {
    HeaderName::from_static("x-ratelimit-remaining")
}

fn rate_limit_reset_header() -> HeaderName {
    HeaderName::from_static("x-ratelimit-reset")
}

// ---------------------------------------------------------------------------
// Guard phase (`cpt-cf-oagw-algo-required-headers-check`).
// ---------------------------------------------------------------------------

fn run_guard_request(item: &PluginItem, headers: &HeaderMap) -> Result<(), OagwError> {
    debug_assert_required_headers_guard(item);
    required_headers_guard::check_request(headers, item.config())
}

fn run_guard_response(item: &PluginItem, headers: &HeaderMap) -> Result<(), OagwError> {
    debug_assert_required_headers_guard(item);
    required_headers_guard::check_response(headers, item.config())
}

/// `classify_chain` only ever places the one guard kind this feature
/// implements into the guard list, so this is a debug-only sanity check,
/// not a runtime branch.
fn debug_assert_required_headers_guard(item: &PluginItem) {
    debug_assert!(
        parse_gts_plugin_ref(item.plugin_ref())
            .is_some_and(|(_, instance)| instance == REQUIRED_HEADERS_GUARD_PLUGIN_NAME)
    );
}

#[cfg(test)]
mod tests {
    use super::{
        RequestChainOutcome, RequestHookContext, ResponseHookContext, invoke_request_hooks,
        invoke_response_hooks,
    };
    use crate::domain::model::{AuthConfig, PluginItem, RateLimitConfig};
    use crate::domain::plugin::{BuiltinPluginRegistry, TokenCache};
    use crate::domain::rate_limit::RateLimiter;
    use axum::http::{HeaderMap, Method, StatusCode};
    use bytes::Bytes;
    use credstore_sdk::test_util::MockCredStoreClient;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    // `BuiltinPluginRegistry` is exercised directly in
    // `crate::domain::plugin::registry`'s own tests; referenced here only so
    // this module's `use` stays representative of the seam's real
    // dependency graph (`crate::domain::plugin_resolve::NamedPluginRegistry`
    // is installed with it elsewhere).
    #[allow(dead_code)]
    const _REGISTRY_MARKER: BuiltinPluginRegistry = BuiltinPluginRegistry;

    #[allow(clippy::too_many_arguments)]
    fn base_ctx<'a>(
        headers: &'a mut HeaderMap,
        body: &'a Bytes,
        query: &'a mut BTreeMap<String, String>,
        method: &'a Method,
        plugins: &'a [PluginItem],
        auth: Option<&'a AuthConfig>,
        rate_limit: Option<&'a RateLimitConfig>,
        credstore: &'a MockCredStoreClient,
        security_context: &'a SecurityContext,
        token_cache: &'a TokenCache,
        rate_limiter: &'a RateLimiter,
    ) -> RequestHookContext<'a> {
        RequestHookContext {
            effective_plugins: plugins,
            effective_auth: auth,
            effective_rate_limit: rate_limit,
            method,
            headers,
            body,
            query,
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            client_ip: None,
            upstream_id: Uuid::new_v4(),
            route_id: Uuid::new_v4(),
            security_context,
            credstore,
            token_cache,
            rate_limiter,
            proxy_timeout: Duration::from_secs(2),
            token_cache_ttl_ceiling: Duration::from_mins(5),
            oauth_http_config_override: None,
        }
    }

    // @cpt-begin:cpt-cf-oagw-dod-chain-invocation-point:p2:inst-plugin-seam-noop-test-01
    #[tokio::test]
    async fn a_chain_with_no_plugins_and_no_auth_is_a_no_op_that_never_rejects() {
        let mut headers = HeaderMap::new();
        let body = Bytes::new();
        let mut query = BTreeMap::new();
        let method = Method::GET;
        let plugins: Vec<PluginItem> = Vec::new();
        let credstore = MockCredStoreClient::empty();
        let sc = SecurityContext::anonymous();
        let token_cache = TokenCache::new(10);
        let rate_limiter = RateLimiter::new();

        let mut ctx = base_ctx(
            &mut headers,
            &body,
            &mut query,
            &method,
            &plugins,
            None,
            None,
            &credstore,
            &sc,
            &token_cache,
            &rate_limiter,
        );
        let outcome = invoke_request_hooks(&mut ctx).await.expect("must admit");
        // No transform binding is present, so the request-id transform
        // never runs (`cpt-cf-oagw-dod-request-id-transform`).
        assert!(outcome.request_id.is_empty());
        assert!(outcome.forced_request_headers.is_empty());
    }
    // @cpt-end:cpt-cf-oagw-dod-chain-invocation-point:p2:inst-plugin-seam-noop-test-01

    #[tokio::test]
    async fn an_unknown_guard_identifier_is_rejected_before_any_guard_runs() {
        let mut headers = HeaderMap::new();
        let body = Bytes::new();
        let mut query = BTreeMap::new();
        let method = Method::GET;
        let plugins: Vec<PluginItem> =
            vec!["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1".into()];
        let credstore = MockCredStoreClient::empty();
        let sc = SecurityContext::anonymous();
        let token_cache = TokenCache::new(10);
        let rate_limiter = RateLimiter::new();

        let mut ctx = base_ctx(
            &mut headers,
            &body,
            &mut query,
            &method,
            &plugins,
            None,
            None,
            &credstore,
            &sc,
            &token_cache,
            &rate_limiter,
        );
        let error = invoke_request_hooks(&mut ctx)
            .await
            .expect_err("unresolvable guard must reject");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn the_response_hook_applies_the_carried_request_id_when_absent() {
        let mut headers = HeaderMap::new();
        let plugins: Vec<PluginItem> = Vec::new();
        let outcome = RequestChainOutcome {
            request_id: "carried-id".to_owned(),
            rate_limit_headers: None,
            forced_request_headers: Vec::new(),
        };
        let mut ctx = ResponseHookContext {
            effective_plugins: &plugins,
            status: StatusCode::OK,
            headers: &mut headers,
            outcome: &outcome,
        };
        invoke_response_hooks(&mut ctx).expect("must succeed");
        assert_eq!(
            headers.get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("carried-id")
        );
    }
}
