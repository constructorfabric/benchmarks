//! The proxy data plane.
//!
//! [`DataPlane::execute`] is the whole hop, in the order the design pins:
//!
//! ```text
//! alias resolution (tenant chain, descendant → root)
//!   → CORS preflight, answered locally with 204
//!   → route matching (ADR 0001)
//!   → CORS origin / method check (403)
//!   → configuration layering (PRD 5.5)
//!   → rate limiting (429 + Retry-After + X-RateLimit-*)
//!   → endpoint selection (X-OAGW-Target-Host matrix)
//!   → auth plugin → guards → request transforms
//!   → scheme / SSRF screen → dial → upstream exchange
//!   → response guards → response transforms
//!   → streamed back to the caller
//! ```
//!
//! Failures carry their own GTS `type` and are rendered as RFC 9457 problem
//! documents by `crate::api::proxy`. An upstream that answered — even with a
//! 4xx or a 5xx — is not a gateway failure: its response is passed through
//! unchanged and only `X-OAGW-Error-Source: upstream` marks it
//! (`docs/ADR/0007`).
pub mod config;
pub mod connector;
pub mod cors;
pub mod failure;
pub mod forward;
pub mod headers;
pub mod rate_limit;
pub mod request;
pub mod response;
pub mod route;
pub mod ssrf;

use std::time::Duration;

use http::Method;

use crate::domain::model::{Route, Upstream};
use crate::domain::plugin::{PluginAttributes, PluginError, RequestContext, ResponseContext};
use crate::infra::plugin::PluginEngine;

pub use config::{ConfigSource, ResolverChain};
pub use connector::{Connection, UpstreamDialer};
pub use failure::{ErrorSource, ProxyFailure};
pub use headers::{EndpointCursor, TARGET_HOST_HEADER};
pub use rate_limit::{RateDecision, RateLimiter, scope_key};
pub use request::ProxyRequest;
pub use response::ProxyResponse;
pub use route::{EffectiveConfig, ResolvedUpstream, SelectedEndpoint};
pub use ssrf::SsrfPolicy;

/// The proxy data plane.
#[derive(Clone)]
pub struct DataPlane {
    config: ConfigSource,
    dialer: UpstreamDialer,
    engine: PluginEngine,
    limiter: rate_limit::RateLimiter,
    cursor: headers::EndpointCursor,
    timeout: Duration,
}

impl DataPlane {
    /// Assemble a data plane.
    #[must_use]
    pub fn new(
        config: ConfigSource,
        dialer: UpstreamDialer,
        engine: PluginEngine,
        timeout: Duration,
    ) -> Self {
        Self {
            config,
            dialer,
            engine,
            limiter: rate_limit::RateLimiter::new(),
            cursor: headers::EndpointCursor::new(),
            timeout,
        }
    }

    /// The plugin engine, so external plugins can be registered before the
    /// gear starts serving.
    #[must_use]
    pub const fn engine(&self) -> &PluginEngine {
        &self.engine
    }

    /// The rate limiter, for tests and diagnostics.
    #[must_use]
    pub const fn limiter(&self) -> &rate_limit::RateLimiter {
        &self.limiter
    }

    /// The upstream exchange timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Execute one proxy hop.
    ///
    /// # Errors
    ///
    /// [`ProxyFailure`] for every gateway-side refusal. An upstream response is
    /// returned whatever its status.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(&self, request: ProxyRequest) -> Result<ProxyResponse, ProxyFailure> {
        // A preflight is answered locally, *before* the alias is resolved: the
        // browser sends it without credentials, so there is no tenant context
        // to resolve it against and the ADR pins "no upstream resolution" as
        // step 2 (`docs/ADR/0004`). The gateway middleware guarantees such a
        // request arrives with an anonymous subject, so resolving first would
        // answer every preflight with a 404.
        if request.is_preflight() {
            return Ok(ProxyResponse::preflight(cors::preflight_reply(
                &request.cors,
            )));
        }

        let chain = self.config.chain(&request.tenant_id).await;
        let resolved = route::resolve_alias(&request.alias, &chain).ok_or_else(|| {
            ProxyFailure::new(
                404,
                crate::domain::plugin::ROUTE_NOT_FOUND,
                "Route Not Found",
                format!(
                    "no enabled upstream of this tenant carries alias '{}'",
                    request.alias
                ),
            )
        })?;

        if !resolved.upstream.enabled {
            return Err(route::upstream_unavailable(&request.alias));
        }

        let routes = self.config.chain_routes(&request.tenant_id).await;
        let Some(route) = route::match_route(
            &resolved.upstream,
            &routes,
            &request.method,
            &request.path,
            request.query.as_deref(),
        )
        .cloned() else {
            return Err(route::route_not_found(
                &request.alias,
                &request.method,
                &request.path,
            ));
        };
        let effective = route::effective_config(&resolved.upstream, Some(&route));

        // Origin and method are validated before the upstream is dialed.
        if let cors::CorsOutcome::Rejected(failure) =
            cors::evaluate(effective.cors.as_ref(), &request.cors)
        {
            return Err(failure.with_header(cors::VARY, "Origin"));
        }

        if let Some(failure) =
            self.enforce_rate_limit(&effective, &resolved.upstream, &route, &request)
        {
            return Err(failure);
        }

        let target = self.select_target(&request, &resolved.upstream)?;
        let endpoint = target.endpoint.clone();
        let upstream_path = route::matched_upstream_path(&route, &request.path);

        let mut context = self.build_context(&request, &resolved.upstream, &route, upstream_path);
        // Hop-by-hop headers are stripped once, before any plugin sees them.
        headers::strip_request_hop_by_hop(&mut context.headers, request.upgrade);

        self.engine
            .authenticate(
                &mut context,
                effective.auth.clone(),
                &effective.guards,
                &effective.transforms,
            )
            .await
            .map_err(plugin_failure)?;

        headers::apply_request_headers(effective.headers.as_ref(), &mut context.headers);

        let connection = self
            .dialer
            .dial(endpoint.scheme, &endpoint.host, endpoint.port)
            .await?;
        let uri = forward::upstream_uri(&endpoint, &context.path, context.query.as_deref())?;
        let outbound = forward::build_request(
            &Method::from_bytes(request.method.as_bytes()).unwrap_or(Method::GET),
            uri,
            forward::prepare_request_headers(context.headers.clone()),
            &endpoint,
            request.body.unwrap_or_else(axum::body::Body::empty),
        )?;

        let (response, upgrade) = forward::exchange(connection, outbound, self.timeout).await?;
        let status = response.status().as_u16();
        let streaming = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(response::is_streaming_media_type);

        let mut response_headers =
            forward::prepare_response_headers(response.headers().clone(), upgrade);
        headers::apply_response_headers(effective.headers.as_ref(), &mut response_headers);
        for (name, value) in
            response::cors_headers(effective.cors.as_ref(), &request.cors).unwrap_or_default()
        {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name),
                http::HeaderValue::try_from(value),
            ) {
                response_headers.insert(name, value);
            }
        }

        // A 101 response hands the socket over; nothing else is read from it.
        let (upgraded, mut upstream_body) = if upgrade && status == 101 {
            let mut response = response;
            let socket = match hyper::upgrade::on(&mut response).await {
                Ok(socket) => socket,
                Err(error) => {
                    return Err(ProxyFailure::protocol_error(format!(
                        "the upstream did not complete the upgrade: {error}"
                    )));
                }
            };
            (Some(socket), None)
        } else {
            let body = response.into_body();
            (None, Some(axum::body::Body::new(body)))
        };

        let mut response_context = ResponseContext {
            status,
            headers: response_headers.clone(),
            body: None,
            streaming,
            config: serde_json::Value::Null,
            attributes: std::mem::take(&mut context.attributes),
        };
        // Only a response the plugin phases must be able to inspect is
        // buffered; a streaming one is handed on untouched, chunk by chunk.
        if !streaming && upgraded.is_none() {
            response_context.body =
                response::buffer_body(upstream_body.take().unwrap_or_else(axum::body::Body::empty))
                    .await;
        }

        self.engine
            .guard_response(&mut response_context, &effective.guards)
            .await
            .map_err(plugin_failure)?;
        self.engine
            .transform_response(&mut response_context, &effective.transforms)
            .await
            .map_err(plugin_failure)?;

        let body = if upgraded.is_some() {
            axum::body::Body::empty()
        } else if streaming {
            upstream_body.unwrap_or_else(axum::body::Body::empty)
        } else {
            response_context
                .body
                .map(axum::body::Body::from)
                .unwrap_or_else(axum::body::Body::empty)
        };

        Ok(ProxyResponse {
            status,
            headers: response_context.headers,
            body,
            upgraded,
            source: if status >= 400 {
                ErrorSource::Upstream
            } else {
                ErrorSource::Gateway
            },
        })
    }

    /// Select the endpoint to dial, applying the target-host matrix.
    #[allow(clippy::result_large_err)]
    fn select_target(
        &self,
        request: &ProxyRequest,
        upstream: &Upstream,
    ) -> Result<SelectedEndpoint, ProxyFailure> {
        let position = headers::select_endpoint(
            &request.alias,
            upstream,
            request.target_host.as_deref(),
            &self.cursor,
        )
        .map_err(|error| headers::target_failure(&request.alias, error))?;
        let endpoint = upstream
            .server
            .endpoints
            .get(position)
            .cloned()
            .ok_or_else(|| ProxyFailure::internal("endpoint pool is empty"))?;
        Ok(SelectedEndpoint {
            endpoint,
            position,
            balanced: request.target_host.is_none() && upstream.server.endpoints.len() > 1,
        })
    }

    /// Apply the effective rate limit, if any.
    fn enforce_rate_limit(
        &self,
        effective: &EffectiveConfig,
        upstream: &Upstream,
        route: &Route,
        request: &ProxyRequest,
    ) -> Option<ProxyFailure> {
        let config = effective.rate_limit.as_ref()?;
        let key = scope_key(
            config,
            upstream.id,
            route.id,
            request.tenant_id,
            &request.subject_id,
        );
        let decision = self.limiter.check(config, &key, config.cost);
        if decision.allowed {
            return None;
        }
        Some(
            ProxyFailure::new(
                429,
                crate::domain::plugin::RATE_LIMIT_EXCEEDED,
                "Rate Limit Exceeded",
                "too many requests for this scope",
            )
            .with_header("retry-after", &decision.retry_after_seconds.to_string())
            .with_header("x-ratelimit-limit", &config.sustained.rate.to_string())
            .with_header("x-ratelimit-remaining", &decision.remaining.to_string())
            .with_header("x-ratelimit-reset", &decision.reset_seconds.to_string()),
        )
    }

    /// Assemble the plugin request context.
    fn build_context(
        &self,
        request: &ProxyRequest,
        upstream: &Upstream,
        route: &Route,
        upstream_path: String,
    ) -> RequestContext {
        RequestContext {
            method: request.method.clone(),
            path: upstream_path,
            query: request.query.clone(),
            // The inbound passthrough policy decides how much of the caller's
            // header set reaches the upstream; the `set`/`add`/`remove` rules
            // are applied afterwards, once the plugins have run.
            headers: headers::inbound_request_headers(
                upstream.headers.as_ref(),
                &request.headers,
                request.upgrade,
            ),
            body: None,
            upgrade: request.upgrade,
            config: serde_json::Value::Null,
            attributes: PluginAttributes::default(),
            tenant_id: request.tenant_id,
            upstream_id: upstream.id,
            route_id: Some(route.id),
            subject_id: request.subject_id.clone(),
            subject_tenant_id: request.subject_tenant_id.clone(),
        }
    }
}

/// Map a plugin error onto a gateway failure.
///
/// A plugin's `code` is already the full GTS `type` (`docs/DESIGN.md` §3.3), so
/// it is carried verbatim; the `title` comes from the catalogue name of the
/// bare id it carries.
fn plugin_failure(error: PluginError) -> ProxyFailure {
    let title = plugin_title(&error.code);
    ProxyFailure::with_type_uri(error.status, error.code, title, error.detail)
}

/// The catalogue title of a plugin error's GTS id.
///
/// `docs/DESIGN.md` §3.3 names every catalogue entry; a plugin error raised
/// away from the constants still lands on the wire with its name, so the id's
/// bare form is looked up here. An id outside the catalogue degrades to the
/// words it carries rather than a generic `Plugin Error`.
fn plugin_title(type_uri: &str) -> String {
    let bare = type_uri.rsplit("err.v1~").next().unwrap_or(type_uri);
    match bare {
        crate::domain::plugin::VALIDATION => "Validation Error",
        crate::domain::plugin::MISSING_TARGET_HOST => "Missing Target Host",
        crate::domain::plugin::INVALID_TARGET_HOST => "Invalid Target Host",
        crate::domain::plugin::UNKNOWN_TARGET_HOST => "Unknown Target Host",
        crate::domain::plugin::AUTH_FAILED => "Authentication Failed",
        crate::domain::plugin::CORS_ORIGIN_NOT_ALLOWED => "Origin Not Allowed",
        crate::domain::plugin::CORS_METHOD_NOT_ALLOWED => "Method Not Allowed",
        crate::domain::plugin::ROUTE_NOT_FOUND => "Route Not Found",
        crate::domain::plugin::PAYLOAD_TOO_LARGE => "Payload Too Large",
        crate::domain::plugin::RATE_LIMIT_EXCEEDED => "Rate Limit Exceeded",
        crate::domain::plugin::SECRET_NOT_FOUND => "Secret Not Found",
        crate::domain::plugin::INTERNAL => "Internal Error",
        crate::domain::plugin::PROTOCOL_ERROR => "Protocol Error",
        crate::domain::plugin::DOWNSTREAM_ERROR => "Downstream Error",
        crate::domain::plugin::STREAM_ABORTED => "Stream Aborted",
        crate::domain::plugin::LINK_UNAVAILABLE => "Link Unavailable",
        crate::domain::plugin::PLUGIN_NOT_FOUND => "Plugin Not Found",
        crate::domain::plugin::CONNECTION_TIMEOUT => "Connection Timeout",
        crate::domain::plugin::REQUEST_TIMEOUT => "Request Timeout",
        other => return titled_words(other),
    }
    .to_owned()
}

/// Title-cased words of a bare id outside the catalogue.
fn titled_words(bare: &str) -> String {
    let name = bare
        .trim_start_matches("cf.")
        .trim_start_matches("oagw.")
        .trim_start_matches("core.")
        .trim_start_matches("err.")
        .trim_end_matches(".v1")
        .replace(['.', '-'], " ");
    let mut title = String::new();
    for word in name.split_whitespace() {
        let mut capitals = word.chars();
        if let Some(first) = capitals.next() {
            title.push(first.to_ascii_uppercase());
            title.push_str(capitals.as_str());
            title.push(' ');
        }
    }
    title.trim_end().to_owned()
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
