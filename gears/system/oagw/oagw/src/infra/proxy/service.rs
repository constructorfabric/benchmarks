//! `DataPlaneServiceImpl` — proxy orchestration (ADR-0001 §Request Flows).
//!
//! `Auth -> Guards -> Transform(on_request) -> upstream call ->
//! Transform(on_response)`, with rate limiting, CORS enforcement, endpoint
//! selection and header transformation around it.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use pingora_core::protocols::http::v1::client::HttpSession as H1Session;
use pingora_http::RequestHeader;
use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::domain::dto::{EffectiveConfig, ResolvedBinding};
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts_helpers;
use crate::domain::model::{Endpoint, PluginKind, RateLimitScope};
use crate::domain::plugin::{GuardDecision, RequestContext, ResponseContext};
use crate::domain::services::DataPlaneService;
use crate::domain::services::management::{ControlPlaneService, ProxyTarget};
use crate::infra::cors;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::PluginRegistries;
use crate::infra::proxy::connector::UpstreamConnector;
use crate::infra::proxy::endpoint::EndpointSelector;
use crate::infra::proxy::{headers as header_rules, upgrade};
use crate::infra::ratelimit::{RateLimiterRegistry, rejects};
use crate::util::{ERROR_SOURCE_HEADER, REQUEST_ID_HEADER, TARGET_HOST_HEADER};

pub struct DataPlaneServiceImpl {
    control_plane: Arc<ControlPlaneService>,
    registries: Arc<PluginRegistries>,
    connector: Arc<UpstreamConnector>,
    limiter: Arc<RateLimiterRegistry>,
    selector: EndpointSelector,
    metrics: Arc<OagwMetrics>,
    config: OagwConfig,
}

impl DataPlaneServiceImpl {
    #[must_use]
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        registries: Arc<PluginRegistries>,
        connector: Arc<UpstreamConnector>,
        limiter: Arc<RateLimiterRegistry>,
        metrics: Arc<OagwMetrics>,
        config: OagwConfig,
    ) -> Self {
        Self {
            control_plane,
            registries,
            connector,
            limiter,
            selector: EndpointSelector::new(),
            metrics,
            config,
        }
    }

    /// Resolve the target and merge configuration for this request.
    async fn resolve_target(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        method: &Method,
        path_suffix: &str,
    ) -> OagwResult<ProxyTarget> {
        let resolved = self
            .control_plane
            .resolve_alias(ctx, alias)
            .await?
            .ok_or_else(|| {
                OagwError::new(
                    ErrorKind::RouteNotFound,
                    format!("no upstream is registered for alias '{alias}'"),
                )
                .with_ext("alias", alias.to_owned())
            })?;

        if !resolved.enabled {
            return Err(OagwError::new(
                ErrorKind::UpstreamDisabled,
                format!("upstream '{alias}' is disabled"),
            )
            .with_ext("alias", alias.to_owned())
            .with_ext(
                "upstream_id",
                gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, resolved.selected.id),
            )
            .with_retry_after(30));
        }

        if resolved.selected.is_grpc() {
            // Phase 3 work: no gRPC proxy code path exists yet, so the request
            // is refused rather than silently proxied as HTTP.
            return Err(OagwError::new(
                ErrorKind::ProtocolError,
                format!("upstream '{alias}' is a gRPC upstream; gRPC proxying is not implemented"),
            ));
        }

        self.control_plane
            .match_route(&resolved, method.as_str(), path_suffix)
            .await
    }

    /// Enforce the effective rate limit, if any.
    fn check_rate_limit(
        &self,
        ctx: &SecurityContext,
        target: &ProxyTarget,
        client_ip: Option<&str>,
    ) -> OagwResult<HeaderMap> {
        let mut headers = HeaderMap::new();
        let Some(config) = target.effective.rate_limit.as_ref() else {
            return Ok(headers);
        };

        let scope_id = match config.scope {
            RateLimitScope::Global => "global".to_owned(),
            RateLimitScope::Tenant => ctx.subject_tenant_id().to_string(),
            RateLimitScope::User => ctx.subject_id().to_string(),
            RateLimitScope::Ip => client_ip.unwrap_or("unknown").to_owned(),
            RateLimitScope::Route => target.route.id.to_string(),
        };
        let key = RateLimiterRegistry::build_key(
            "upstream",
            &target.upstream.id.to_string(),
            config.scope,
            &scope_id,
        );
        let outcome = self.limiter.check(&key, config);

        if config.response_headers {
            insert_str(&mut headers, "x-ratelimit-limit", &outcome.limit.to_string());
            insert_str(
                &mut headers,
                "x-ratelimit-remaining",
                &outcome.remaining.to_string(),
            );
            insert_str(
                &mut headers,
                "x-ratelimit-reset",
                &outcome.reset_epoch.to_string(),
            );
        }

        if !outcome.allowed && rejects(config.strategy) {
            self.metrics
                .record_rate_limited(&target.upstream.alias, &target.outbound_path);
            let mut err = OagwError::new(
                ErrorKind::RateLimitExceeded,
                format!(
                    "Rate limit exceeded for upstream {}",
                    target.upstream.alias
                ),
            )
            .with_ext("host", target.upstream.alias.clone())
            .with_ext(
                "upstream_id",
                gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, target.upstream.id),
            )
            .with_retry_after(outcome.retry_after_seconds)
            // RFC 6585 / draft-ietf-httpapi-ratelimit-headers: the quota
            // headers travel with the rejection as well as the success.
            .with_headers(&headers);
            err.extensions.insert(
                "retry_after_seconds".to_owned(),
                outcome.retry_after_seconds.into(),
            );
            return Err(err);
        }
        Ok(headers)
    }

    /// Run the auth plugin declared by the effective configuration.
    async fn run_auth(
        &self,
        effective: &EffectiveConfig,
        request: &mut RequestContext,
    ) -> OagwResult<()> {
        let Some(auth) = effective.auth.as_ref() else {
            return Ok(());
        };
        let Some(plugin_type) = auth.plugin_type.as_deref() else {
            return Ok(());
        };
        let plugin = self.registries.auth.get(plugin_type).ok_or_else(|| {
            OagwError::new(
                ErrorKind::PluginNotFound,
                format!("unknown auth plugin: {plugin_type}"),
            )
        })?;
        let saved = std::mem::replace(&mut request.config, auth.config.clone());
        let result = plugin.authenticate(request).await;
        request.config = saved;
        result.map_err(OagwError::from)
    }

    /// Guard chain (upstream bindings first, then route bindings).
    async fn run_guards(
        &self,
        bindings: &[ResolvedBinding],
        request: &mut RequestContext,
    ) -> OagwResult<()> {
        for binding in bindings {
            let Some(plugin) = self.registries.guard.get(&binding.plugin_ref) else {
                continue;
            };
            let saved = std::mem::replace(&mut request.config, binding.config.clone());
            let decision = plugin.guard_request(request).await;
            request.config = saved;
            match decision.map_err(OagwError::from)? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    error_code,
                    message,
                } => {
                    return Err(guard_rejection(status, &error_code, message, &binding.plugin_ref));
                }
            }
        }
        Ok(())
    }

    async fn run_response_guards(
        &self,
        bindings: &[ResolvedBinding],
        response: &mut ResponseContext,
    ) -> OagwResult<()> {
        for binding in bindings {
            let Some(plugin) = self.registries.guard.get(&binding.plugin_ref) else {
                continue;
            };
            let saved = std::mem::replace(&mut response.config, binding.config.clone());
            let decision = plugin.guard_response(response).await;
            response.config = saved;
            match decision.map_err(OagwError::from)? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    error_code,
                    message,
                } => {
                    return Err(guard_rejection(status, &error_code, message, &binding.plugin_ref));
                }
            }
        }
        Ok(())
    }

    async fn run_request_transforms(
        &self,
        bindings: &[ResolvedBinding],
        request: &mut RequestContext,
    ) -> OagwResult<()> {
        for binding in bindings {
            let Some(plugin) = self.registries.transform.get(&binding.plugin_ref) else {
                continue;
            };
            let saved = std::mem::replace(&mut request.config, binding.config.clone());
            let result = plugin.transform_request(request).await;
            request.config = saved;
            result.map_err(OagwError::from)?;
        }
        Ok(())
    }

    async fn run_response_transforms(
        &self,
        bindings: &[ResolvedBinding],
        response: &mut ResponseContext,
    ) -> OagwResult<()> {
        for binding in bindings {
            let Some(plugin) = self.registries.transform.get(&binding.plugin_ref) else {
                continue;
            };
            let saved = std::mem::replace(&mut response.config, binding.config.clone());
            let result = plugin.transform_response(response).await;
            response.config = saved;
            result.map_err(OagwError::from)?;
        }
        Ok(())
    }

    /// Reject bindings that name a plugin no registry can resolve, so a
    /// misconfigured chain fails loudly instead of silently doing nothing.
    fn ensure_bindings_resolvable(&self, bindings: &[ResolvedBinding]) -> OagwResult<()> {
        for binding in bindings {
            let known = self.registries.guard.get(&binding.plugin_ref).is_some()
                || self.registries.transform.get(&binding.plugin_ref).is_some();
            if known {
                continue;
            }
            // UUID-backed custom (Starlark) plugins have no native
            // implementation to execute yet.
            let kind = gts_helpers::split_gts(&binding.plugin_ref)
                .and_then(|(base, _)| PluginKind::from_base_type(base));
            return Err(OagwError::new(
                ErrorKind::PluginNotFound,
                match kind {
                    Some(kind) => format!(
                        "{} plugin '{}' is not resolvable in this deployment",
                        kind.as_str(),
                        binding.plugin_ref
                    ),
                    None => format!("plugin '{}' is not resolvable", binding.plugin_ref),
                },
            ));
        }
        Ok(())
    }
}

fn guard_rejection(
    status: u16,
    error_code: &str,
    message: String,
    plugin_ref: &str,
) -> OagwError {
    let kind = match status {
        400 => ErrorKind::Validation,
        401 => ErrorKind::AuthenticationFailed,
        403 => ErrorKind::Forbidden,
        413 => ErrorKind::PayloadTooLarge,
        429 => ErrorKind::RateLimitExceeded,
        502 => ErrorKind::DownstreamError,
        _ => ErrorKind::Validation,
    };
    OagwError::new(kind, message)
        .with_ext("error_code", error_code.to_owned())
        .with_ext("plugin_ref", plugin_ref.to_owned())
}

fn insert_str(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(http::HeaderName::from_static(name), v);
    }
}

/// Authority (`host[:port]`) written into the outbound `Host` header.
#[must_use]
pub fn authority_for(endpoint: &Endpoint) -> String {
    if endpoint.port == endpoint.scheme.standard_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

/// Render path + query into the request target.
#[must_use]
pub fn request_target(path: &str, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return path.to_owned();
    }
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in query {
        serializer.append_pair(key, value);
    }
    format!("{path}?{}", serializer.finish())
}

/// Validate inbound query parameters against the route allowlist.
///
/// # Errors
/// `400 ValidationError` when a parameter is not allowlisted.
pub fn filter_query(
    raw_query: Option<&str>,
    allowlist: &[String],
) -> OagwResult<Vec<(String, String)>> {
    let Some(raw) = raw_query.filter(|q| !q.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
        if !allowlist.iter().any(|a| a == key.as_ref()) {
            return Err(OagwError::validation(format!(
                "query parameter '{key}' is not in the route's query_allowlist"
            )));
        }
        out.push((key.into_owned(), value.into_owned()));
    }
    Ok(out)
}

#[async_trait]
impl DataPlaneService for DataPlaneServiceImpl {
    async fn execute_proxy(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        path_suffix: &str,
        mut request: Request<Body>,
    ) -> OagwResult<Response> {
        let started = Instant::now();
        let method = request.method().clone();
        let inbound_headers = request.headers().clone();
        let raw_query = request.uri().query().map(str::to_owned);
        let origin = inbound_headers
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let request_id = inbound_headers
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let target_host = inbound_headers
            .get(TARGET_HOST_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let target = self
            .resolve_target(ctx, alias, &method, path_suffix)
            .await?;
        let audit_request_id = request_id.clone();

        self.metrics.inc_in_flight(&target.upstream.alias);
        let result = self
            .execute_resolved(
                ctx,
                &target,
                &method,
                &inbound_headers,
                raw_query.as_deref(),
                origin.as_deref(),
                request_id,
                target_host.as_deref(),
                &mut request,
            )
            .await;
        self.metrics.dec_in_flight(&target.upstream.alias);

        let route_label = target
            .route
            .match_config
            .http
            .as_ref()
            .map_or_else(|| target.outbound_path.clone(), |m| m.path.clone());
        self.metrics.record_duration(
            &target.upstream.alias,
            &route_label,
            "total",
            started.elapsed().as_secs_f64(),
        );
        let (status, error_type) = match &result {
            Ok(response) => (response.status().as_u16(), None),
            Err(err) => (err.status(), Some(err.kind.gts_type())),
        };
        if let Some(error_type) = error_type {
            self.metrics
                .record_error(&target.upstream.alias, &route_label, error_type);
        }
        self.metrics.record_request(
            &target.upstream.alias,
            method.as_str(),
            &route_label,
            status,
        );

        // Audit trail (ADR-0001 "Audit Log JSON Format"). Bodies, query
        // strings and headers are never logged; neither are credentials.
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if status >= 500 {
            tracing::error!(
                target: "oagw.audit",
                event = "proxy_request",
                request_id = audit_request_id.as_deref().unwrap_or_default(),
                tenant_id = %ctx.subject_tenant_id(),
                principal_id = %ctx.subject_id(),
                host = %target.upstream.alias,
                path = %route_label,
                method = %method,
                status,
                duration_ms,
                error_type = error_type.unwrap_or_default(),
                "proxy request failed"
            );
        } else {
            tracing::info!(
                target: "oagw.audit",
                event = "proxy_request",
                request_id = audit_request_id.as_deref().unwrap_or_default(),
                tenant_id = %ctx.subject_tenant_id(),
                principal_id = %ctx.subject_id(),
                host = %target.upstream.alias,
                path = %route_label,
                method = %method,
                status,
                duration_ms,
                error_type = error_type.unwrap_or_default(),
                "proxy request completed"
            );
        }
        result
    }
}

impl DataPlaneServiceImpl {
    #[allow(clippy::too_many_arguments)]
    async fn execute_resolved(
        &self,
        ctx: &SecurityContext,
        target: &ProxyTarget,
        method: &Method,
        inbound_headers: &HeaderMap,
        raw_query: Option<&str>,
        origin: Option<&str>,
        request_id: Option<String>,
        target_host: Option<&str>,
        request: &mut Request<Body>,
    ) -> OagwResult<Response> {
        let http_match = target.route.match_config.http.as_ref().ok_or_else(|| {
            OagwError::internal("matched route has no HTTP match keys")
        })?;

        // --- guards that do not need the upstream connection ---------------
        let query = filter_query(raw_query, &http_match.query_allowlist)?;

        if let Some(cors_cfg) = target.effective.cors.as_ref()
            && let Some(origin) = origin
        {
            cors::validate_actual(cors_cfg, origin, method.as_str())?;
        }

        let client_ip = inbound_headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim);
        let rate_limit_headers = self.check_rate_limit(ctx, target, client_ip)?;

        self.ensure_bindings_resolvable(&target.effective.plugins)?;

        let (endpoint, selection) = self.selector.select(&target.upstream, target_host)?;
        if target_host.is_some() {
            self.metrics
                .record_target_host_used(&target.upstream.id.to_string(), &endpoint.host);
        }
        self.metrics.record_endpoint_selected(
            &target.upstream.id.to_string(),
            &endpoint.host,
            selection.as_str(),
        );

        let authority = authority_for(endpoint);
        let outbound_headers = header_rules::build_outbound(
            inbound_headers,
            &target.effective.headers.request,
            &authority,
        );

        // --- plugin chain --------------------------------------------------
        let mut request_ctx = RequestContext {
            security_context: ctx.clone(),
            config: std::collections::BTreeMap::new(),
            method: method.as_str().to_owned(),
            path: target.outbound_path.clone(),
            query,
            headers: outbound_headers,
            alias: target.upstream.alias.clone(),
            upstream_id: target.upstream.id,
            route_id: Some(target.route.id),
            request_id,
            body: None,
        };

        self.run_auth(&target.effective, &mut request_ctx).await?;
        self.run_guards(&target.effective.plugins, &mut request_ctx)
            .await?;
        self.run_request_transforms(&target.effective.plugins, &mut request_ctx)
            .await?;

        // --- upgrade or plain exchange -------------------------------------
        if upgrade::is_upgrade_request(inbound_headers) {
            return self
                .proxy_upgrade(target, endpoint, &authority, inbound_headers, &request_ctx, request)
                .await;
        }

        let body = std::mem::replace(request.body_mut(), Body::empty());
        let body_bytes = axum::body::to_bytes(
            body,
            usize::try_from(self.config.max_body_bytes).unwrap_or(usize::MAX),
        )
        .await
        .map_err(|_| {
            OagwError::new(
                ErrorKind::PayloadTooLarge,
                format!(
                    "request body exceeds the {}-byte limit",
                    self.config.max_body_bytes
                ),
            )
        })?;

        self.proxy_http(
            target,
            endpoint,
            &authority,
            &request_ctx,
            body_bytes,
            origin,
            rate_limit_headers,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn proxy_http(
        &self,
        target: &ProxyTarget,
        endpoint: &Endpoint,
        authority: &str,
        request_ctx: &RequestContext,
        body: Bytes,
        origin: Option<&str>,
        rate_limit_headers: HeaderMap,
    ) -> OagwResult<Response> {
        let stream = self.connector.connect(endpoint).await?;
        let mut session = H1Session::new(stream);
        session.read_timeout = Some(self.config.proxy_timeout());
        session.write_timeout = Some(self.config.proxy_timeout());

        let request_target = request_target(&request_ctx.path, &request_ctx.query);
        let mut head = RequestHeader::build(
            request_ctx.method.as_str(),
            request_target.as_bytes(),
            Some(request_ctx.headers.len() + 4),
        )
        .map_err(|err| {
            OagwError::new(
                ErrorKind::Validation,
                format!("could not build the outbound request: {err}"),
            )
        })?;
        head.set_version(http::Version::HTTP_11);
        for (name, value) in &request_ctx.headers {
            head.append_header(name.clone(), value.clone()).map_err(|err| {
                OagwError::new(
                    ErrorKind::Validation,
                    format!("invalid outbound header '{name}': {err}"),
                )
            })?;
        }
        head.insert_header(http::header::HOST, authority)
            .map_err(|err| {
                OagwError::new(
                    ErrorKind::Validation,
                    format!("invalid upstream authority '{authority}': {err}"),
                )
            })?;
        let needs_length = !body.is_empty()
            || matches!(
                request_ctx.method.as_str(),
                "POST" | "PUT" | "PATCH" | "DELETE"
            );
        if needs_length {
            head.insert_header(http::header::CONTENT_LENGTH, body.len().to_string())
                .map_err(|err| {
                    OagwError::new(
                        ErrorKind::Validation,
                        format!("could not set Content-Length: {err}"),
                    )
                })?;
        }

        let exchange = async {
            session
                .write_request_header(Box::new(head))
                .await
                .map_err(|err| downstream_error("write request header", &err))?;
            if !body.is_empty() {
                session
                    .write_body(&body)
                    .await
                    .map_err(|err| downstream_error("write request body", &err))?;
            }
            session
                .finish_body()
                .await
                .map_err(|err| downstream_error("finish request body", &err))?;
            session
                .read_response()
                .await
                .map_err(|err| downstream_error("read response header", &err))?;
            Ok::<(), OagwError>(())
        };

        match tokio::time::timeout(self.config.proxy_timeout(), exchange).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                return Err(OagwError::new(
                    ErrorKind::RequestTimeout,
                    format!(
                        "upstream '{}' did not respond within {}s",
                        target.upstream.alias,
                        self.config.proxy_timeout().as_secs()
                    ),
                )
                .with_ext("host", target.upstream.alias.clone())
                .with_retry_after(self.config.proxy_timeout().as_secs()));
            }
        }

        let response_header = session
            .resp_header()
            .cloned()
            .ok_or_else(|| OagwError::new(ErrorKind::ProtocolError, "upstream sent no response"))?;
        let status = response_header.status;

        let mut response_ctx = ResponseContext {
            config: std::collections::BTreeMap::new(),
            status: status.as_u16(),
            headers: header_rules::build_response(
                &response_header.headers,
                &target.effective.headers.response,
            ),
            request_id: request_ctx.request_id.clone(),
        };
        self.run_response_guards(&target.effective.plugins, &mut response_ctx)
            .await?;
        self.run_response_transforms(&target.effective.plugins, &mut response_ctx)
            .await?;

        // Body streaming: SSE and chunked responses are forwarded frame by
        // frame, so no read timeout is applied past the response head.
        session.read_timeout = None;
        let stream = futures_util::stream::unfold(Some(session), |state| async move {
            let mut session = state?;
            match session.read_body_bytes().await {
                Ok(Some(chunk)) if !chunk.is_empty() => {
                    Some((Ok::<Bytes, std::io::Error>(chunk), Some(session)))
                }
                Ok(Some(_)) | Ok(None) => None,
                Err(err) => Some((
                    Err(std::io::Error::other(err.to_string())),
                    None,
                )),
            }
        })
        .boxed();

        let mut builder = Response::builder().status(status);
        if let Some(headers) = builder.headers_mut() {
            *headers = response_ctx.headers;
            headers.remove(http::header::CONTENT_LENGTH);
            headers.remove(http::header::TRANSFER_ENCODING);
            for (name, value) in &rate_limit_headers {
                headers.insert(name.clone(), value.clone());
            }
            if let Some(cors_cfg) = target.effective.cors.as_ref()
                && let Some(origin) = origin
            {
                cors::merge_into(headers, cors::response_headers(cors_cfg, origin));
            }
            headers.insert(
                http::HeaderName::from_static(ERROR_SOURCE_HEADER),
                HeaderValue::from_static("upstream"),
            );
        }
        builder.body(Body::from_stream(stream)).map_err(|err| {
            OagwError::internal(format!("could not build the proxy response: {err}"))
        })
    }

    async fn proxy_upgrade(
        &self,
        target: &ProxyTarget,
        endpoint: &Endpoint,
        authority: &str,
        inbound_headers: &HeaderMap,
        request_ctx: &RequestContext,
        request: &mut Request<Body>,
    ) -> OagwResult<Response> {
        let client_upgrade = hyper::upgrade::on(&mut *request);

        let stream = self.connector.connect(endpoint).await?;
        let handshake_headers =
            header_rules::build_upgrade_headers(inbound_headers, &request_ctx.headers);
        let request_target = request_target(&request_ctx.path, &request_ctx.query);

        let exchange = upgrade::perform_handshake(
            stream,
            &request_ctx.method,
            &request_target,
            authority,
            &handshake_headers,
        );
        let exchange = match tokio::time::timeout(self.config.proxy_timeout(), exchange).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(OagwError::new(
                    ErrorKind::RequestTimeout,
                    format!(
                        "upstream '{}' did not complete the upgrade handshake within {}s",
                        target.upstream.alias,
                        self.config.proxy_timeout().as_secs()
                    ),
                ));
            }
        };

        if exchange.status != StatusCode::SWITCHING_PROTOCOLS {
            // The upstream declined the upgrade; surface its answer verbatim.
            let mut builder = Response::builder().status(exchange.status);
            if let Some(headers) = builder.headers_mut() {
                *headers = header_rules::build_response(
                    &exchange.headers,
                    &target.effective.headers.response,
                );
                headers.remove(http::header::CONTENT_LENGTH);
                headers.insert(
                    http::HeaderName::from_static(ERROR_SOURCE_HEADER),
                    HeaderValue::from_static("upstream"),
                );
            }
            return builder
                .body(Body::from(exchange.leftover))
                .map_err(|err| OagwError::internal(format!("could not build response: {err}")));
        }

        let upstream_stream = exchange.stream;
        let leftover = exchange.leftover;
        tokio::spawn(async move {
            match client_upgrade.await {
                Ok(upgraded) => {
                    let io = hyper_util::rt::TokioIo::new(upgraded);
                    upgrade::splice(io, upstream_stream, leftover).await;
                }
                Err(err) => tracing::debug!(
                    target: "oagw.proxy",
                    error = %err,
                    "client connection could not be upgraded"
                ),
            }
        });

        let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
        if let Some(headers) = builder.headers_mut() {
            for (name, value) in &exchange.headers {
                headers.append(name.clone(), value.clone());
            }
            headers.insert(
                http::HeaderName::from_static(ERROR_SOURCE_HEADER),
                HeaderValue::from_static("upstream"),
            );
        }
        builder
            .body(Body::empty())
            .map_err(|err| OagwError::internal(format!("could not build 101 response: {err}")))
    }
}

fn downstream_error(phase: &str, err: &pingora_core::Error) -> OagwError {
    OagwError::new(
        ErrorKind::DownstreamError,
        format!("upstream exchange failed during {phase}: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_omits_the_standard_port() {
        use crate::domain::model::Scheme;
        let https = Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".to_owned(),
            port: 443,
        };
        assert_eq!(authority_for(&https), "api.openai.com");

        let odd = Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".to_owned(),
            port: 8443,
        };
        assert_eq!(authority_for(&odd), "api.openai.com:8443");

        let plain = Endpoint {
            scheme: Scheme::Http,
            host: "localhost".to_owned(),
            port: 80,
        };
        assert_eq!(authority_for(&plain), "localhost");
    }

    #[test]
    fn request_target_encodes_the_allowed_query() {
        assert_eq!(request_target("/v1/models", &[]), "/v1/models");
        let query = vec![
            ("limit".to_owned(), "10".to_owned()),
            ("q".to_owned(), "a b".to_owned()),
        ];
        assert_eq!(
            request_target("/v1/models", &query),
            "/v1/models?limit=10&q=a+b"
        );
    }

    #[test]
    fn query_allowlist_rejects_unknown_parameters() {
        let allow = vec!["limit".to_owned()];
        let ok = filter_query(Some("limit=10"), &allow).expect("allowed");
        assert_eq!(ok, vec![("limit".to_owned(), "10".to_owned())]);

        let err = filter_query(Some("limit=10&secret=1"), &allow).expect_err("rejected");
        assert_eq!(err.status(), 400);

        // An empty allowlist admits no parameters at all.
        assert!(filter_query(Some("a=1"), &[]).is_err());
        assert!(filter_query(None, &[]).expect("no query").is_empty());
        assert!(filter_query(Some(""), &[]).expect("empty query").is_empty());
    }

    #[test]
    fn guard_rejections_map_onto_gateway_statuses() {
        let err = guard_rejection(400, "REQUIRED_HEADER_MISSING", "missing".to_owned(), "p");
        assert_eq!(err.status(), 400);
        assert_eq!(
            err.extensions.get("error_code").and_then(|v| v.as_str()),
            Some("REQUIRED_HEADER_MISSING")
        );

        let err = guard_rejection(502, "REQUIRED_HEADER_MISSING", "missing".to_owned(), "p");
        assert_eq!(err.status(), 502);
    }
}
