//! Data-plane proxy engine.
//!
//! Pipeline: resolve upstream by alias → resolve route → CORS → rate limit →
//! guard plugins → auth plugins → header policy → outbound hop → response
//! header policy.

/// Header policy (DESIGN §3.2).
pub mod headers;
/// Rate limiting (ADR-0003).
pub mod rate_limit;
/// Outbound transport (pingora connector + hyper codec).
pub mod transport;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::dto::{CorsConfig, MatchRules, Upstream, parse_plugin_ref};
use crate::domain::error::DomainError;
use crate::domain::plugin::{PluginContext, PluginRegistry};
use crate::domain::services::RouteService;
use crate::infra::proxy::headers::{build_request_headers, build_response_headers};
use crate::infra::proxy::rate_limit::RateLimiter;
use crate::infra::proxy::transport::{OutboundTransport, Target};

/// Everything the engine needs to execute one proxied request.
#[derive(Debug, Clone, Default)]
pub struct ProxyRequest {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing alias taken from the request path.
    pub alias: String,
    /// HTTP method.
    pub method: http::Method,
    /// Target path (everything after `/proxy/{alias}`), always starts with `/`.
    pub path: String,
    /// Parsed query parameters.
    pub query: Vec<(String, String)>,
    /// Inbound headers.
    pub headers: http::HeaderMap,
    /// Buffered request body.
    pub body: bytes::Bytes,
    /// Client IP, when the edge supplied one.
    pub client_ip: Option<std::net::IpAddr>,
    /// Authenticated downstream subject, when known.
    pub subject: Option<String>,
    /// Security context of the downstream caller, when the request was
    /// authenticated. Plugins that read a credential use it to scope the
    /// credstore lookup (see `infra::plugin::secret`).
    pub security_context: Option<crate::domain::SecurityContext>,
    /// `true` when the client asked for a protocol upgrade.
    pub upgrade: bool,
}

/// The proxy engine.
pub struct ProxyEngine {
    config: Arc<OagwConfig>,
    upstreams: Arc<crate::domain::services::UpstreamService>,
    routes: Arc<RouteService>,
    /// Built-in plugin registry.
    plugins: Arc<PluginRegistry>,
    /// Tenant-defined plugin rows, resolved for custom plugin bindings.
    plugin_store: Arc<dyn crate::domain::repo::PluginRepo>,
    limiter: RateLimiter,
    transport: OutboundTransport,
    counter: AtomicUsize,
}

impl std::fmt::Debug for ProxyEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyEngine").finish()
    }
}

impl ProxyEngine {
    /// Build the engine.
    #[must_use]
    pub fn new(
        config: Arc<OagwConfig>,
        upstreams: Arc<crate::domain::services::UpstreamService>,
        routes: Arc<RouteService>,
        plugins: Arc<PluginRegistry>,
        plugin_store: Arc<dyn crate::domain::repo::PluginRepo>,
    ) -> Self {
        let transport =
            OutboundTransport::new(Duration::from_secs(config.connect_timeout_secs.max(1)));
        Self {
            config,
            upstreams,
            routes,
            plugins,
            plugin_store,
            limiter: RateLimiter::new(),
            transport,
            counter: AtomicUsize::new(0),
        }
    }

    /// Rate limiter access (for tests and metrics).
    #[must_use]
    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    /// Execute a proxied request.
    ///
    /// # Errors
    /// Any [`DomainError`] from the pipeline; the REST layer maps them to
    /// `problem+json`.
    pub async fn execute(
        &self,
        request: ProxyRequest,
    ) -> Result<axum::response::Response, DomainError> {
        self.run(request, None).await
    }

    /// Execute a proxied request that asked for a protocol upgrade
    /// (`Connection: upgrade` + `Upgrade: websocket`).
    ///
    /// On an upstream `101` the two hops are spliced together with a byte
    /// pump; on any other upstream status the response is forwarded as-is.
    ///
    /// # Errors
    /// Any [`DomainError`] from the pipeline; the REST layer maps them to
    /// `problem+json`.
    pub async fn execute_with_upgrade(
        &self,
        request: ProxyRequest,
        on_upgrade: hyper::upgrade::OnUpgrade,
    ) -> Result<axum::response::Response, DomainError> {
        self.run(request, Some(on_upgrade)).await
    }

    async fn run(
        &self,
        request: ProxyRequest,
        on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Result<axum::response::Response, DomainError> {
        // Resolution walks the caller's tenant chain (DESCENDANT → ROOT), so an
        // upstream an ancestor shares with its descendants serves a request
        // they did not define themselves, and the graded configuration of the
        // chain is folded into one effective upstream (DESIGN §"Hierarchical
        // Configuration").
        let resolved = self
            .upstreams
            .resolve_effective(
                request.security_context.as_ref(),
                request.tenant_id,
                &request.alias,
            )
            .await?;
        let upstream = resolved.upstream;
        self.ensure_supported_protocol(&upstream)?;

        let route = self
            .routes
            .resolve_in_chain(
                &resolved.chain,
                upstream.id,
                request.method.as_str(),
                &request.path,
                &request.query,
            )
            .await?
            .ok_or(DomainError::RouteNotFound)?;

        self.check_cors(&upstream, &request)?;
        self.apply_rate_limits(&upstream, &route, &request)?;

        let mut plugin_ctx = PluginContext {
            tenant_id: request.tenant_id,
            chain: resolved.chain.clone(),
            upstream_id: upstream.id,
            route_id: Some(route.id),
            alias: request.alias.clone(),
            client_ip: request.client_ip,
            subject: request.subject.clone(),
            security_context: request.security_context.clone(),
            ..PluginContext::default()
        };

        self.run_guards(&upstream, &route, &plugin_ctx, &request)
            .await?;

        let target = transport::Target::resolve(&upstream.config.server.endpoints, self.next_offset())?;
        let forwarded_path = self.forwarded_path(&route.config.matcher, &request.path)?;
        let outbound_headers = build_request_headers(
            &request.headers,
            &upstream.config.headers.request,
            &target.host_header,
            target.port,
            target.tls,
            request.client_ip,
            &self.config.default_upstream_user_agent,
        );

        // Auth plugins write credentials straight into the header map.
        let mut outbound_headers = outbound_headers;
        if let Some(auth) = &upstream.config.auth {
            self.apply_auth(&mut plugin_ctx, auth, &mut outbound_headers)
                .await?;
        }

        // Transform plugins run last so they can override auth-set headers.
        let outbound_headers = self
            .run_transforms(
                &upstream,
                &route,
                &mut plugin_ctx,
                outbound_headers,
                &forwarded_path,
            )
            .await;

        let upgrade = if request.upgrade { on_upgrade } else { None };
        let mut response = self
            .send(
                &target,
                &request,
                &upstream.config.headers,
                outbound_headers,
                &forwarded_path,
                upgrade,
            )
            .await?;

        // ADR-0007: every response leaving the proxy names its origin. This
        // one came from the upstream — success *and* failure — while errors
        // OAGW generates are stamped `gateway` by `ApiError`.
        headers::stamp_error_source(&mut response, crate::domain::error::SOURCE_UPSTREAM);

        if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
            // A guard may reject an upstream response that violates policy
            // (ADR-0009 response phase). The head is handed over by value:
            // a shared borrow of the response would be neither `Send` nor
            // usable across the await.
            let snapshot_headers = response.headers().clone();
            self.run_response_guards(
                &upstream,
                &route,
                &plugin_ctx,
                response.status(),
                &snapshot_headers,
            )
            .await?;
            // A transform plugin may stamp response headers (`request_id`, …).
            self.run_response_transforms(&upstream, &route, &mut plugin_ctx, &mut response)
                .await;
        }
        self.apply_cors_response_headers(&upstream, &request, &mut response);
        Ok(response)
    }

    fn ensure_supported_protocol(&self, upstream: &Upstream) -> Result<(), DomainError> {
        if matches!(upstream.config.protocol, crate::domain::dto::Protocol::Grpc) {
            return Err(DomainError::LinkUnavailable(
                "gRPC proxying is not available in this build".into(),
            ));
        }
        Ok(())
    }

    /// The path actually sent upstream.
    ///
    /// The route's `match.path` already matched the request path, so the
    /// forwarded path is the request path itself; a suffix is only legal when
    /// the route opted into `path_suffix_mode: append`.
    fn forwarded_path(
        &self,
        matcher: &MatchRules,
        request_path: &str,
    ) -> Result<String, DomainError> {
        let MatchRules::Http(matcher) = matcher else {
            return Err(DomainError::RouteNotFound);
        };
        let base = matcher.path.trim_end_matches('/');
        let candidate = request_path.trim_end_matches('/');
        if candidate == base {
            return Ok(base.to_owned());
        }
        if candidate.starts_with(base) && candidate.as_bytes().get(base.len()) == Some(&b'/') {
            if matches!(
                matcher.path_suffix_mode,
                crate::domain::dto::PathSuffixMode::Disabled
            ) {
                return Err(DomainError::Validation(format!(
                    "path suffix is not permitted for route '{}'",
                    matcher.path
                )));
            }
            return Ok(candidate.to_owned());
        }
        Ok(base.to_owned())
    }

    fn check_cors(&self, upstream: &Upstream, request: &ProxyRequest) -> Result<(), DomainError> {
        let Some(cors) = upstream.config.cors.as_ref().filter(|c| c.enabled) else {
            return Ok(());
        };
        let Some(origin) = request
            .headers
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|o| !o.is_empty())
        else {
            return Ok(());
        };
        if !origin_allowed(cors, origin) {
            return Err(DomainError::CorsRejected("origin not allowed"));
        }
        let method = request.method.as_str().to_ascii_uppercase();
        if !method_allowed(cors, &method) {
            return Err(DomainError::CorsRejected("method not allowed"));
        }
        Ok(())
    }

    fn apply_rate_limits(
        &self,
        upstream: &Upstream,
        route: &crate::domain::dto::Route,
        request: &ProxyRequest,
    ) -> Result<(), DomainError> {
        let subject = request.subject.as_deref();
        let route_id = Some(route.id);
        let candidates = [
            upstream.config.rate_limit.as_ref().map(|c| (c, "upstream")),
            route.config.rate_limit.as_ref().map(|c| (c, "route")),
        ];
        for (config, label) in candidates.into_iter().flatten() {
            let key = RateLimiter::scope_key(
                config,
                request.tenant_id,
                subject,
                request.client_ip,
                route_id,
            );
            let decision = self.limiter.check(&key, config);
            if !decision.allowed {
                tracing::info!(scope = %label, "rate limit exceeded");
                return Err(self.limiter.rejection(decision));
            }
        }
        Ok(())
    }

    async fn run_guards(
        &self,
        upstream: &Upstream,
        route: &crate::domain::dto::Route,
        ctx: &PluginContext,
        request: &ProxyRequest,
    ) -> Result<(), DomainError> {
        let snapshot = crate::domain::plugin::PluginRequest {
            method: request.method.to_string(),
            path: request.path.clone(),
            query: request.query.clone(),
            headers: request.headers.clone(),
            body_len: request.body.len(),
        };
        let upstream_auth = upstream.config.auth.as_ref().and_then(|a| a.plugin_type.clone());
        for (bindings, auth_ref) in [
            (Some(&upstream.config.plugins), upstream_auth),
            (Some(&route.config.plugins), None),
        ] {
            if let Some(bindings) = bindings {
                for reference in &bindings.items {
                    match parse_plugin_ref(reference) {
                        crate::domain::dto::PluginRef::Named(id) => {
                            if let Some(plugin) = self.plugins.guard(&id) {
                                plugin.guard(ctx, &plugin_config(&id), &snapshot).await?;
                            }
                        }
                        crate::domain::dto::PluginRef::Custom(id) => {
                            // A UUID binding resolves against the custom-plugin
                            // store: the row decides which built-in executes and
                            // carries its own configuration.
                            let resolved = self.resolve_custom(ctx, id).await;
                            if let Some((reference, config)) =
                                resolved.filter(|(reference, _)| self.plugins.guard(reference).is_some())
                            {
                                if let Some(plugin) = self.plugins.guard(&reference) {
                                    plugin.guard(ctx, &config, &snapshot).await?;
                                }
                            }
                        }
                    }
                }
            }
            if let Some(auth_ref) = auth_ref {
                if let Some(plugin) = self.plugins.guard(&auth_ref) {
                    plugin.guard(ctx, &plugin_config(&auth_ref), &snapshot).await?;
                }
            }
        }
        Ok(())
    }

    /// Run the response half of the guard chain (ADR-0009 response phase).
    ///
    /// A guard that rejects here turns the upstream response into a
    /// *gateway* error, so the upstream body never reaches the client.
    async fn run_response_guards(
        &self,
        upstream: &Upstream,
        route: &crate::domain::dto::Route,
        ctx: &PluginContext,
        status: http::StatusCode,
        response_headers: &http::HeaderMap,
    ) -> Result<(), DomainError> {
        if !self.has_response_guards(upstream, route) {
            return Ok(());
        }
        let snapshot = crate::domain::plugin::PluginResponse::from_parts(status, response_headers);
        for (bindings, auth_ref) in [
            (
                Some(&upstream.config.plugins),
                upstream.config.auth.as_ref().and_then(|a| a.plugin_type.clone()),
            ),
            (Some(&route.config.plugins), None),
        ] {
            let bindings = bindings.filter(|b| !b.items.is_empty());
            if let Some(bindings) = bindings {
                for reference in &bindings.items {
                    match parse_plugin_ref(reference) {
                        crate::domain::dto::PluginRef::Named(id) => {
                            if let Some(plugin) = self.plugins.guard(&id) {
                                plugin.guard_response(ctx, &plugin_config(&id), &snapshot).await?;
                            }
                        }
                        crate::domain::dto::PluginRef::Custom(id) => {
                            let resolved = self.resolve_custom(ctx, id).await;
                            if let Some((reference, config)) = resolved
                                .filter(|(reference, _)| self.plugins.guard(reference).is_some())
                            {
                                if let Some(plugin) = self.plugins.guard(&reference) {
                                    plugin.guard_response(ctx, &config, &snapshot).await?;
                                }
                            }
                        }
                    }
                }
            }
            if let Some(auth_ref) = auth_ref {
                if let Some(plugin) = self.plugins.guard(&auth_ref) {
                    plugin.guard_response(ctx, &plugin_config(&auth_ref), &snapshot).await?;
                }
            }
        }
        Ok(())
    }

    /// `true` when any plugin is bound to the upstream, the route or the
    /// upstream's auth configuration.
    ///
    /// Cloning the response head for the guard snapshot is skipped entirely on
    /// the (common) hot path where no plugin is bound at all.
    fn has_response_guards(
        &self,
        upstream: &Upstream,
        route: &crate::domain::dto::Route,
    ) -> bool {
        !upstream.config.plugins.items.is_empty()
            || !route.config.plugins.items.is_empty()
            || upstream
                .config
                .auth
                .as_ref()
                .is_some_and(|a| a.plugin_type.is_some() || a.plugin_ref.is_some())
    }

    async fn apply_auth(
        &self,
        ctx: &mut PluginContext,
        auth: &crate::domain::dto::AuthConfig,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError> {
        let Some(reference) = auth.plugin_type.as_deref().or(auth.plugin_ref.as_deref()) else {
            return Ok(());
        };
        let Some(plugin) = self.plugins.auth(reference) else {
            return Err(DomainError::AuthPluginUnavailable(reference.to_owned()));
        };
        plugin.authenticate(ctx, &auth.config, headers).await
    }

    /// Run the transform chain and return the (possibly rewritten) request head.
    async fn run_transforms(
        &self,
        upstream: &Upstream,
        route: &crate::domain::dto::Route,
        ctx: &mut PluginContext,
        mut headers: http::HeaderMap,
        path: &str,
    ) -> http::HeaderMap {
        let mut request = http::Request::builder()
            .method(http::Method::GET)
            .uri(path)
            .body(())
            .unwrap_or_else(|_| http::Request::new(()));
        *request.headers_mut() = std::mem::take(&mut headers);

        for (bindings, auth_ref) in [
            (Some(&upstream.config.plugins), upstream.config.auth.as_ref().and_then(|a| a.plugin_type.clone())),
            (Some(&route.config.plugins), None),
        ] {
            if let Some(bindings) = bindings {
                for reference in &bindings.items {
                    match parse_plugin_ref(reference) {
                        crate::domain::dto::PluginRef::Named(id) => {
                            if let Some(plugin) = self.plugins.transform(&id) {
                                plugin
                                    .transform_request(ctx, &plugin_config(&id), &mut request)
                                    .await;
                            }
                        }
                        crate::domain::dto::PluginRef::Custom(id) => {
                            let resolved = self.resolve_custom(ctx, id).await;
                            if let Some((reference, config)) = resolved
                                .filter(|(reference, _)| self.plugins.transform(reference).is_some())
                            {
                                if let Some(plugin) = self.plugins.transform(&reference) {
                                    plugin.transform_request(ctx, &config, &mut request).await;
                                }
                            }
                        }
                    }
                }
            }
            if let Some(auth_ref) = auth_ref {
                if let Some(plugin) = self.plugins.transform(&auth_ref) {
                    plugin
                        .transform_request(ctx, &plugin_config(&auth_ref), &mut request)
                        .await;
                }
            }
        }
        request.into_parts().0.headers
    }

    /// Run the response half of the transform chain.
    ///
    /// Only the response head is handed to a plugin: the body keeps streaming
    /// untouched, which is what makes an SSE/long-poll hop safe.
    async fn run_response_transforms(
        &self,
        upstream: &Upstream,
        route: &crate::domain::dto::Route,
        ctx: &mut PluginContext,
        response: &mut axum::response::Response,
    ) {
        let mut head = http::Response::builder()
            .status(response.status())
            .body(())
            .unwrap_or_else(|_| http::Response::new(()));
        *head.headers_mut() = std::mem::take(response.headers_mut());

        for (bindings, auth_ref) in [
            (
                Some(&upstream.config.plugins),
                upstream.config.auth.as_ref().and_then(|a| a.plugin_type.clone()),
            ),
            (Some(&route.config.plugins), None),
        ] {
            if let Some(bindings) = bindings {
                for reference in &bindings.items {
                    match parse_plugin_ref(reference) {
                        crate::domain::dto::PluginRef::Named(id) => {
                            if let Some(plugin) = self.plugins.transform(&id) {
                                plugin
                                    .transform_response(ctx, &plugin_config(&id), &mut head)
                                    .await;
                            }
                        }
                        crate::domain::dto::PluginRef::Custom(id) => {
                            let resolved = self.resolve_custom(ctx, id).await;
                            if let Some((reference, config)) = resolved
                                .filter(|(reference, _)| self.plugins.transform(reference).is_some())
                            {
                                if let Some(plugin) = self.plugins.transform(&reference) {
                                    plugin.transform_response(ctx, &config, &mut head).await;
                                }
                            }
                        }
                    }
                }
            }
            if let Some(auth_ref) = auth_ref {
                if let Some(plugin) = self.plugins.transform(&auth_ref) {
                    plugin.transform_response(ctx, &plugin_config(&auth_ref), &mut head).await;
                }
            }
        }
        *response.headers_mut() = head.into_parts().0.headers;
    }

    /// Resolve a custom plugin binding into `(built-in reference, config)`.
    ///
    /// A `plugins.items` entry may be a UUID pointing at a tenant-defined
    /// plugin row. That row stores both the configuration the plugin should run
    /// with and — through its type, its name or a config discriminator key —
    /// which built-in implementation executes. Starlark source text is not
    /// executed by this build: only the built-in behaviour behind a custom row
    /// is.
    async fn resolve_custom(
        &self,
        ctx: &PluginContext,
        id: uuid::Uuid,
    ) -> Option<(String, serde_json::Value)> {
        // The binding may name a row an ancestor defined and shared down the
        // chain, so the lookup walks the caller's tenant chain, closest first.
        for tenant in std::iter::once(ctx.tenant_id).chain(ctx.chain.iter().copied()) {
            if let Some(row) = self
                .plugin_store
                .get(tenant, id)
                .await
                .map_err(|err| {
                    tracing::debug!(plugin = %id, error = %err, "custom plugin binding unresolved");
                    err
                })
                .ok()
            {
                return self.resolve_plugin_row(&row);
            }
        }
        None
    }

    /// Resolve a stored plugin row into `(built-in reference, config)`.
    ///
    /// A `plugins.items` entry may be a UUID pointing at a tenant-defined
    /// plugin row. That row stores both the configuration the plugin should run
    /// with and — through its type, its name or a config discriminator key —
    /// which built-in implementation executes. Starlark source text is not
    /// executed by this build: only the built-in behaviour behind a custom row
    /// is.
    fn resolve_plugin_row(&self, row: &crate::domain::dto::Plugin) -> Option<(String, serde_json::Value)> {
        // 1. An explicit implementation discriminator in the stored config.
        for key in ["type", "plugin", "implementation"] {
            if let Some(name) = row
                .config
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                return Some((name.to_owned(), row.config.clone()));
            }
        }
        // 2. The row's own name, but only when it names a built-in plugin this
        //    build actually implements. `builtin_plugin_name` happily extracts
        //    a short name from *any* non-UUID string, so testing it alone would
        //    short-circuit every freely named row and drop its configuration on
        //    the floor (the registry then fails to match the invented name).
        if let Some(name) = crate::domain::dto::builtin_plugin_name(&row.name)
            && self.registry_knows(&name)
        {
            return Some((row.name.clone(), row.config.clone()));
        }
        // 3. Configuration-shape recognition for rows named freely (ADR-0002).
        if let Some(name) = infer_builtin_from_config(&row.config) {
            return Some((name.to_owned(), row.config.clone()));
        }
        tracing::debug!(
            plugin = %row.name,
            "custom plugin row names no built-in implementation; skipping (starlark execution is out of scope)"
        );
        None
    }

    fn apply_cors_response_headers(
        &self,
        upstream: &Upstream,
        request: &ProxyRequest,
        response: &mut axum::response::Response,
    ) {
        let Some(cors) = upstream.config.cors.as_ref().filter(|c| c.enabled) else {
            return;
        };
        let Some(origin) = request
            .headers
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|o| !o.is_empty())
        else {
            return;
        };
        if !origin_allowed(cors, origin) {
            return;
        }
        let headers = response.headers_mut();
        if let Ok(value) = http::HeaderValue::from_str(origin) {
            headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        }
        if let Ok(value) = http::HeaderValue::from_str(&cors.allowed_methods.join(", ")) {
            headers.insert(http::header::ACCESS_CONTROL_ALLOW_METHODS, value);
        }
        if cors.allow_credentials {
            headers.insert(
                http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                http::HeaderValue::from_static("true"),
            );
        }
        if !cors.expose_headers.is_empty() {
            if let Ok(value) = http::HeaderValue::from_str(&cors.expose_headers.join(", ")) {
                headers.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
            }
        }
        headers.append(http::header::VARY, http::HeaderValue::from_static("Origin"));
    }

    async fn send(
        &self,
        target: &Target,
        request: &ProxyRequest,
        response_rules: &crate::domain::dto::HeadersConfig,
        mut outbound_headers: http::HeaderMap,
        path: &str,
        on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Result<axum::response::Response, DomainError> {
        let (mut sender, connection) = self.transport.connect(target).await?;

        // RFC 9112 §3.2: a client talks to an origin server in **origin-form**
        // (`/path?query`); absolute-form is only for requests made *to a
        // proxy*. Sending absolute-form here would leak the target authority
        // into upstreams that route on the request target, so the authority
        // travels in `Host` instead and the URI carries just the path.
        let uri = build_uri(target, path, &request.query)?;
        let origin_form = http::Uri::builder()
            .path_and_query(match uri.query() {
                Some(query) => format!("{}?{query}", uri.path()),
                None => uri.path().to_owned(),
            })
            .build()
            .map_err(|e| DomainError::Validation(format!("invalid upstream URI: {e}")))?;
        let method = request.method.clone();
        let empty_body = matches!(
            method,
            http::Method::GET | http::Method::HEAD | http::Method::OPTIONS | http::Method::DELETE
        );

        // An upgrade request must re-declare the hop-by-hop headers the policy
        // stripped: without `Connection: Upgrade` + `Upgrade: websocket` (and
        // the `Sec-WebSocket-*` negotiation headers) the upstream sees a plain
        // request and answers with a normal response.
        if on_upgrade.is_some() {
            headers::apply_upgrade_headers(&mut outbound_headers, &request.headers);
        }

        let mut builder = http::Request::builder().method(method).uri(origin_form);
        for (name, value) in outbound_headers.iter() {
            builder = builder.header(name, value);
        }
        let outbound = builder
            .body(if empty_body && request.body.is_empty() {
                Body::empty()
            } else {
                Body::from(request.body.clone())
            })
            .map_err(|e| DomainError::Validation(format!("outbound request could not be built: {e}")))?;

        let response = tokio::time::timeout(
            Duration::from_secs(self.config.proxy_timeout_secs.max(1)),
            sender.send_request(outbound),
        )
        .await
        .map_err(|_| DomainError::RequestTimeout)?
        .map_err(|e| DomainError::DownstreamError(format!("upstream rejected the request: {e}")))?;

        if response.status() == http::StatusCode::SWITCHING_PROTOCOLS {
            // Relay the negotiated handshake verbatim (`Sec-WebSocket-Accept`,
            // subprotocol, extensions); the client validates the accept key.
            let relayed = headers::upgrade_response_headers(response.headers());
            if let Some(on_upgrade) = on_upgrade {
                // The connection driver owns the socket until the tunnel takes
                // it over, so it must outlive this call: dropping (and thereby
                // aborting) it here would tear the tunnel down before the
                // handshake is spliced.
                splice_upgrade(response, on_upgrade, connection);
            }
            // A `101` has no body to stream; the pump owns the socket from here.
            let mut client_response = axum::http::Response::builder()
                .status(http::StatusCode::SWITCHING_PROTOCOLS);
            if let Some(headers) = client_response.headers_mut() {
                *headers = relayed;
            }
            return client_response
                .body(Body::empty())
                .map_err(|e| DomainError::DownstreamError(format!("upgrade response: {e}")));
        }

        Ok(map_response(response, &response_rules.response))
    }

    /// `true` when the built-in plugin registry resolves `name` in any family.
    fn registry_knows(&self, name: &str) -> bool {
        self.plugins.guard(name).is_some()
            || self.plugins.auth(name).is_some()
            || self.plugins.transform(name).is_some()
    }

    fn next_offset(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}

/// The configuration handed to a bound built-in plugin.
///
/// The `plugins.items` wire shape is a list of GTS ids, so a named binding
/// carries no configuration object of its own; built-ins therefore run against
/// an empty object and pick up their documented defaults. (`auth.config` is the
/// one place where a plugin configuration travels on the wire.) A custom-plugin
/// binding resolves its own config instead — see
/// [`ProxyEngine::resolve_custom`].
fn plugin_config(reference: &str) -> &'static serde_json::Value {
    let _ = reference;
    static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| serde_json::Value::Object(serde_json::Map::new()))
}

///
/// Only the *shape* of the stored configuration is used, never its values, so
/// no secret material is inspected beyond key names.
#[must_use]
fn infer_builtin_from_config(config: &serde_json::Value) -> Option<&'static str> {
    let Some(object) = config.as_object() else {
        return None;
    };
    let has = |key: &str| object.contains_key(key);
    if has("required")
        || has("required_non_empty")
        || has("forbidden")
        || has("required_request_headers")
        || has("required_response_headers")
    {
        return Some("required_headers");
    }
    if has("token_url") {
        return Some("oauth2_client_cred");
    }
    if has("prefix") || has("forward_incoming") || has("header") {
        return Some("request_id");
    }
    if has("secret_ref") || has("api_key") || has("key_ref") || has("key_header") {
        return Some("apikey");
    }
    None
}

/// `true` when `origin` is permitted by `cors`.
#[must_use]
pub fn origin_allowed(cors: &CorsConfig, origin: &str) -> bool {
    cors.allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

/// `true` when `method` is permitted by `cors`.
///
/// `allowed_origins` spells "any" as `["*"]`, and the method list honours the
/// same spelling: an operator who wrote `["*"]` asked for every method, and a
/// preflight against such an upstream must not be answered with a 403.
#[must_use]
pub fn method_allowed(cors: &CorsConfig, method: &str) -> bool {
    cors.allowed_methods
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(method))
}


/// Build the absolute target URI.
fn build_uri(
    target: &Target,
    path: &str,
    query: &[(String, String)],
) -> Result<http::Uri, DomainError> {
    let scheme = if target.tls { "https" } else { "http" };
    let host = if target.socket.port() == 0 {
        target.host_header.clone()
    } else {
        format!("{}:{}", target.host_header, target.port)
    };
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let query_string = if query.is_empty() {
        String::new()
    } else {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish()
    };
    let uri = format!("{scheme}://{}{}{}", host, path, if query_string.is_empty() {
        String::new()
    } else {
        format!("?{query_string}")
    });
    http::Uri::try_from(uri.as_str())
        .map_err(|e| DomainError::Validation(format!("invalid upstream URI: {e}")))
}

/// Convert the hyper response into an axum response, streaming the body.
fn map_response(
    response: http::Response<hyper::body::Incoming>,
    rules: &crate::domain::dto::HeaderRules,
) -> axum::response::Response {
    let status = response.status();
    let outbound_headers = build_response_headers(
        response.headers(),
        rules,
        headers::RESPONSE_PASSTHROUGH,
    );
    let (_, incoming) = response.into_parts();
    let body = Body::new(incoming);
    let mut response = axum::http::Response::builder().status(status);
    if let Some(headers) = response.headers_mut() {
        *headers = outbound_headers;
    }
    response.body(body).unwrap_or_else(|_| {
        axum::http::Response::builder()
            .status(http::StatusCode::BAD_GATEWAY)
            .body(Body::empty())
            .expect("fallback response")
    })
}

/// Splice a completed `101` upgrade into the client's socket.
///
/// Both halves are awaited in a detached task: the upstream `Upgraded` IO and
/// the client `Upgraded` IO are then pumped into each other with
/// [`tokio::io::copy_bidirectional`] until either side closes. No websocket
/// framing is interpreted — an edge gateway tunnels the bytes.
///
/// The upstream `Sec-WebSocket-Accept` (and any negotiated subprotocol) has
/// already been relayed verbatim by [`map_response`]'s caller, so the client
/// sees exactly the handshake it would have seen from the upstream.
fn splice_upgrade(
    mut upstream: http::Response<hyper::body::Incoming>,
    client: hyper::upgrade::OnUpgrade,
    connection: crate::infra::proxy::transport::ConnectionHandle,
) {
    let upstream_upgrade = hyper::upgrade::on(&mut upstream);
    tokio::spawn(async move {
        // Kept alive for the tunnel: the driver task owns the upstream socket
        // until hyper hands the upgraded half over.
        let _connection = connection;
        let (client_io, upstream_io) = match (client.await, upstream_upgrade.await) {
            (Ok(client), Ok(upstream)) => (client, upstream),
            (Err(err), _) | (_, Err(err)) => {
                tracing::debug!(error = %err, "upgrade handshake did not complete");
                return;
            }
        };
        let mut client_io = hyper_util::rt::TokioIo::new(client_io);
        let mut upstream_io = hyper_util::rt::TokioIo::new(upstream_io);
        match tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
            Ok((client_to_upstream, upstream_to_client)) => {
                tracing::debug!(
                    sent = client_to_upstream,
                    received = upstream_to_client,
                    "upgrade tunnel closed"
                );
            }
            Err(err) => tracing::debug!(error = %err, "upgrade tunnel errored"),
        }
    });
}

#[cfg(test)]
mod cors_wildcard_tests {
    use super::*;

    fn cors(methods: &[&str]) -> CorsConfig {
        CorsConfig {
            allowed_methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            ..CorsConfig::default()
        }
    }

    #[test]
    fn a_wildcard_method_list_permits_every_method() {
        let any = cors(&["*"]);
        for method in ["GET", "POST", "OPTIONS", "DELETE", "patch"] {
            assert!(method_allowed(&any, method), "{method}");
        }
        // An empty list permits nothing (validation refuses it upstream).
        assert!(!method_allowed(&cors(&[]), "GET"));
    }

    #[test]
    fn an_explicit_method_list_stays_narrow() {
        let explicit = cors(&["GET", "POST"]);
        assert!(method_allowed(&explicit, "GET"));
        assert!(method_allowed(&explicit, "post"));
        assert!(!method_allowed(&explicit, "DELETE"));
        assert!(!method_allowed(&explicit, "*"));
    }

    #[test]
    fn a_wildcard_origin_list_permits_every_origin() {
        let any = CorsConfig {
            allowed_origins: vec![String::from("*")],
            ..CorsConfig::default()
        };
        assert!(origin_allowed(&any, "https://app.example.com"));
        let exact = CorsConfig {
            allowed_origins: vec![String::from("https://app.example.com")],
            ..CorsConfig::default()
        };
        assert!(origin_allowed(&exact, "https://app.example.com"));
        assert!(!origin_allowed(&exact, "https://evil.example.com"));
    }
}


/// Compile-time guard: the proxy future must stay `Send`, or the axum proxy
/// handlers silently stop satisfying `Handler` (an upstream response head is
/// borrowed across an await is the classic way to lose it).
#[allow(dead_code)]
fn assert_proxy_future_is_send(engine: std::sync::Arc<ProxyEngine>, request: ProxyRequest) {
    fn require_send<F: std::future::Future + Send>(future: F) {
        drop(future);
    }
    require_send(async move { engine.run(request, None).await });
}
