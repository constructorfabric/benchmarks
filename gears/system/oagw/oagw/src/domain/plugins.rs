//! Data-plane plugin registry and executable built-in plugins.
//!
//! Implements the plugin catalog the GTS type-provisioning registers:
//! auth plugins (`noop`, `apikey`, `oauth2_client_cred`,
//! `oauth2_client_cred_basic`), guard plugins (`required_headers`), transform
//! plugins (`request_id`, `logging`, `metrics`), and the catalog-only
//! plugins (`basic`, `bearer`, `timeout`, `cors`) which are discoverable but
//! not executed in the request path.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue};
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig};
use toolkit_security::SecurityContext;
use tracing::info;

use crate::domain::credentials::CredentialResolver;
use crate::domain::error::DomainError;
use crate::domain::models::{Plugin, PluginKind};

/// Everything an executable plugin needs to know about the in-flight request.
pub struct AuthContext<'a> {
    /// Client request headers.
    pub headers: &'a HeaderMap,
    /// Authenticated subject context supplied by the host.
    pub security: &'a SecurityContext,
    /// Resolved tenant id for the request (derived from the security
    /// context by the gate, never a hardcoded default).
    pub tenant_id: String,
}

/// Outcome of an auth-plugin execution.
#[derive(Debug)]
pub enum AuthOutcome {
    /// Authentication satisfied; optionally inject a header before forwarding.
    Ok { inject: Option<(String, String)> },
    /// Authentication rejected.
    Rejected,
    /// Plugin could not evaluate.
    Error(DomainError),
}

/// Executable auth plugin.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Stable plugin identity (alias).
    fn alias(&self) -> &str;
    /// Executes the plugin against the request.
    async fn authenticate(&self, ctx: &AuthContext<'_>) -> AuthOutcome;
}

/// Executable guard plugin.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Stable plugin identity (alias).
    fn alias(&self) -> &str;
    /// Executes the guard; `Err` rejects the request (400-family).
    async fn check(&self, ctx: &AuthContext<'_>) -> Result<(), DomainError>;
}

/// An `Into`-free header injection helper used by the pipeline.
#[must_use]
pub fn header_pair(name: &str, value: impl Into<String>) -> (String, String) {
    (name.to_owned(), value.into())
}

/// Timing-safe comparison of two byte slices (constant work per byte in the
/// equal-length case, so secret comparison does not leak prefix lengths).
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        // `*x`/`*y` are `&u8`; XOR accumulates any difference.
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// noop auth plugin
// ---------------------------------------------------------------------------

/// Permissive auth plugin — accepts every request without touching it.
#[derive(Debug, Clone, Default)]
pub struct NoopAuthPlugin {
    alias: String,
}

impl NoopAuthPlugin {
    /// Creates the plugin.
    #[must_use]
    pub fn new(alias: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
        }
    }
}

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn alias(&self) -> &str {
        &self.alias
    }

    async fn authenticate(&self, _ctx: &AuthContext<'_>) -> AuthOutcome {
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-noop
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-noop-done
        AuthOutcome::Ok { inject: None }
        // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-noop
        // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-noop-done
    }
}

// ---------------------------------------------------------------------------
// apikey auth plugin
// ---------------------------------------------------------------------------

/// API-key auth plugin: compares the `apikey_header` value against a secret
/// resolved from the credential store (`cred_ref`).
pub struct ApiKeyAuthPlugin {
    alias: String,
    header: String,
    cred_ref: String,
    resolver: Arc<CredentialResolver>,
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin from its JSON config.
    ///
    /// # Errors
    ///
    /// Returns `Validation` when the config is incomplete.
    pub fn try_new(
        alias: &str,
        config: &serde_json::Value,
        resolver: Arc<CredentialResolver>,
    ) -> Result<Self, DomainError> {
        let header = config
            .get("header")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("X-Api-Key");
        let cred_ref = config
            .get("cred_ref")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DomainError::validation("apikey plugin requires `cred_ref`"))?;
        Ok(Self {
            alias: alias.to_owned(),
            header: header.to_owned(),
            cred_ref: cred_ref.to_owned(),
            resolver,
        })
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn alias(&self) -> &str {
        &self.alias
    }

    async fn authenticate(&self, ctx: &AuthContext<'_>) -> AuthOutcome {
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-apikey
        let Some(present) = ctx.headers.get(&self.header) else {
            // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-apikey
            return AuthOutcome::Error(DomainError::AuthFailed(format!(
                "missing required header `{}`",
                self.header
            )));
        };
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-apikey-lookup
        match self.resolver.resolve_secret(&self.cred_ref, ctx).await {
            // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-apikey-lookup
            Ok(secret) => {
                let expected = secret.trim_end_matches('\n');
                if constant_time_eq(present.as_bytes(), expected.as_bytes()) {
                    AuthOutcome::Ok {
                        inject: Some(header_pair(&self.header, expected)),
                    }
                } else {
                    AuthOutcome::Rejected
                }
            }
            Err(e) => AuthOutcome::Error(e),
        }
    }
}

// ---------------------------------------------------------------------------
// OAuth2 client-credentials auth plugin
// ---------------------------------------------------------------------------

/// OAuth2 client-credentials auth plugin. Fetches a bearer token from the
/// configured token endpoint (cached via the resolver), then injects
/// `Authorization: Bearer`.
pub struct OAuth2ClientCredAuthPlugin {
    alias: String,
    client_id: String,
    scopes: Vec<String>,
    token_endpoint: Option<url::Url>,
    auth_method: ClientAuthMethod,
    resolver: Arc<CredentialResolver>,
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds the plugin from its JSON config with an explicit auth method.
    ///
    /// # Errors
    ///
    /// Returns `Validation` when the config is incomplete.
    pub fn try_new_with_auth_method(
        alias: &str,
        config: &serde_json::Value,
        auth_method: ClientAuthMethod,
        resolver: Arc<CredentialResolver>,
    ) -> Result<Self, DomainError> {
        let client_id = config
            .get("client_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DomainError::validation("oauth2 plugin requires `client_id`"))?;
        let token_endpoint = config
            .get("token_endpoint")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| url::Url::parse(s).ok());
        let scopes = config
            .get("scopes")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if token_endpoint.is_none() {
            return Err(DomainError::validation(
                "oauth2 plugin requires a parsable `token_endpoint`",
            ));
        }
        Ok(Self {
            alias: alias.to_owned(),
            client_id: client_id.to_owned(),
            scopes,
            token_endpoint,
            auth_method,
            resolver,
        })
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn alias(&self) -> &str {
        &self.alias
    }

    async fn authenticate(&self, ctx: &AuthContext<'_>) -> AuthOutcome {
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-oauth2
        let cred_ref = ctx
            // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-oauth2
            .headers
            .get("x-oagw-internal-client-secret")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .unwrap_or_default();
        if cred_ref.is_empty() {
            return AuthOutcome::Error(DomainError::CredentialError(
                "oauth2 plugin requires a client_secret credential".to_owned(),
            ));
        }
        let client_secret = match self.resolver.resolve_secret(&cred_ref, ctx).await {
            Ok(s) => toolkit_auth::oauth2::SecretString::new(s),
            Err(e) => return AuthOutcome::Error(e),
        };
        let config = OAuthClientConfig {
            token_endpoint: self.token_endpoint.clone(),
            issuer_url: None,
            client_id: self.client_id.clone(),
            client_secret,
            scopes: self.scopes.clone(),
            auth_method: self.auth_method,
            ..Default::default()
        };
        match self.resolver.fetch_and_cache_token(&config, ctx).await {
            Ok(token) => AuthOutcome::Ok {
                inject: Some(header_pair("Authorization", format!("Bearer {token}"))),
            },
            Err(e) => AuthOutcome::Error(e),
        }
    }
}

// ---------------------------------------------------------------------------
// guard plugins
// ---------------------------------------------------------------------------

/// Guard that requires a set of request headers to be present and non-empty.
pub struct RequiredHeadersGuard {
    alias: String,
    required: Vec<String>,
}

impl RequiredHeadersGuard {
    /// Builds the guard from its JSON config.
    ///
    /// # Errors
    ///
    /// Returns `Validation` when no headers are configured.
    pub fn try_new(alias: &str, config: &serde_json::Value) -> Result<Self, DomainError> {
        let required: Vec<String> = config
            .get("headers")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .ok_or_else(|| DomainError::validation("required_headers plugin requires `headers`"))?;
        if required.is_empty() {
            return Err(DomainError::validation(
                "required_headers plugin requires at least one header",
            ));
        }
        Ok(Self {
            alias: alias.to_owned(),
            required,
        })
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuard {
    fn alias(&self) -> &str {
        &self.alias
    }

    async fn check(&self, ctx: &AuthContext<'_>) -> Result<(), DomainError> {
        for header in &self.required {
            let exists = ctx
                .headers
                .get(header)
                .is_some_and(|v| !v.as_bytes().is_empty());
            if !exists {
                return Err(DomainError::GuardRejected(format!(
                    "missing required header `{header}`"
                )));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// transform plugins
// ---------------------------------------------------------------------------

/// Generates a request id and injects it on the forwarded request.
#[derive(Debug, Clone, Default)]
pub struct RequestIdTransform;

impl RequestIdTransform {
    /// Creates the transform.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Injects a fresh `X-Request-Id` header when none is present; returns
    /// the request id (existing or generated).
    #[must_use]
    pub fn apply(&self, headers: &mut HeaderMap) -> String {
        if let Some(existing) = headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
        {
            return existing.to_owned();
        }
        let id = uuid::Uuid::new_v4().to_string();
        if let Ok(value) = HeaderValue::from_str(&id) {
            headers.insert("x-request-id", value);
        }
        id
    }
}

/// Emits an access-log line per proxied request.
#[derive(Debug, Clone, Default)]
pub struct LoggingTransform {
    alias: String,
}

impl LoggingTransform {
    /// Creates the transform.
    #[must_use]
    pub fn new(alias: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
        }
    }

    /// Logs the request/response summary.
    pub fn record(&self, alias: &str, method: &str, status: u16, elapsed: Duration) {
        info!(
            plugin = %self.alias,
            route_alias = alias,
            method,
            status,
            elapsed_ms = elapsed.as_millis() as u64,
            "oagw proxy access log"
        );
    }
}

/// Emits request metrics (opentelemetry counters).
#[derive(Debug, Default)]
pub struct MetricsTransform {
    total: std::sync::atomic::AtomicU64,
}

/// Process-wide OTel counter for proxied OAGW requests, Lazily built from
/// the global meter provider (instrumentation scope `oagw`) so the metrics
/// transform actually exports the counters it maintains.
static REQUEST_COUNTER: std::sync::OnceLock<opentelemetry::metrics::Counter<u64>> =
    std::sync::OnceLock::new();

fn request_counter() -> &'static opentelemetry::metrics::Counter<u64> {
    REQUEST_COUNTER.get_or_init(|| {
        let scope = opentelemetry::InstrumentationScope::builder("oagw").build();
        let meter = opentelemetry::global::meter_with_scope(scope);
        meter
            .u64_counter("oagw_proxy_requests_total")
            .with_description("Total proxied OAGW requests by HTTP status class")
            .build()
    })
}

impl MetricsTransform {
    /// Creates the transform.
    #[must_use]
    pub fn new() -> Self {
        Self {
            total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Records a completed proxy attempt, exporting it to the OTel counter
    /// with the status class as an attribute.
    pub fn record(&self, status: u16) {
        use std::sync::atomic::Ordering;
        self.total.fetch_add(1, Ordering::Relaxed);
        let class = match status {
            0 => "none",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            _ => "5xx",
        };
        request_counter().add(1, &[opentelemetry::KeyValue::new("status_class", class)]);
    }

    /// Total requests observed since construction.
    #[must_use]
    pub fn total(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.total.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Registry of executable plugins for one effective route. Constructed per
/// request from the effective plugin list; only executable kinds are wired.
pub struct PluginRegistry {
    auth: Vec<Box<dyn AuthPlugin>>,
    guards: Vec<Box<dyn GuardPlugin>>,
    request_id: Option<RequestIdTransform>,
    logging: Option<LoggingTransform>,
    metrics: Option<MetricsTransform>,
}

impl PluginRegistry {
    /// Builds the registry from the effective plugin list.
    ///
    /// # Errors
    ///
    /// Returns `Validation` when a plugin's config is malformed (fail loud
    /// at build so misconfigured routes reject deterministically).
    pub fn try_build(
        plugins: &[Plugin],
        resolver: Arc<CredentialResolver>,
    ) -> Result<Self, DomainError> {
        let mut registry = Self {
            auth: Vec::new(),
            guards: Vec::new(),
            request_id: None,
            logging: None,
            metrics: None,
        };
        for plugin in plugins {
            if !plugin.enabled {
                continue;
            }
            match plugin.kind {
                PluginKind::Noop => {
                    registry
                        .auth
                        .push(Box::new(NoopAuthPlugin::new(plugin.alias.clone())));
                }
                PluginKind::ApiKey => {
                    registry.auth.push(Box::new(ApiKeyAuthPlugin::try_new(
                        &plugin.alias,
                        &plugin.config,
                        resolver.clone(),
                    )?));
                }
                PluginKind::OAuth2ClientCred | PluginKind::OAuth2ClientCredBasic => {
                    let method = if plugin.kind == PluginKind::OAuth2ClientCredBasic {
                        ClientAuthMethod::Basic
                    } else {
                        ClientAuthMethod::Form
                    };
                    registry.auth.push(Box::new(
                        OAuth2ClientCredAuthPlugin::try_new_with_auth_method(
                            &plugin.alias,
                            &plugin.config,
                            method,
                            resolver.clone(),
                        )?,
                    ));
                }
                PluginKind::RequiredHeaders => {
                    registry.guards.push(Box::new(RequiredHeadersGuard::try_new(
                        &plugin.alias,
                        &plugin.config,
                    )?));
                }
                PluginKind::RequestId => {
                    registry.request_id = Some(RequestIdTransform::new());
                }
                PluginKind::Logging => {
                    registry.logging = Some(LoggingTransform::new(plugin.alias.clone()));
                }
                PluginKind::Metrics => {
                    registry.metrics = Some(MetricsTransform::new());
                }
                // Catalog-only plugins are discovered via GTS but never
                // executed in the request path.
                PluginKind::Basic | PluginKind::Bearer | PluginKind::Timeout | PluginKind::Cors => {
                }
            }
        }
        Ok(registry)
    }

    /// Executes auth plugins in declaration order; first rejection wins.
    pub async fn run_auth(
        &self,
        ctx: &AuthContext<'_>,
    ) -> Result<Vec<(String, String)>, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-determine-method
        let mut injections = Vec::new();
        // @cpt-end:cpt-cf-oagw-algo-data-plane-credential-resolution:ph-1:inst-determine-method
        for plugin in &self.auth {
            match plugin.authenticate(ctx).await {
                AuthOutcome::Ok { inject } => {
                    if let Some((k, v)) = inject {
                        injections.push((k, v));
                    }
                }
                AuthOutcome::Rejected => {
                    return Err(DomainError::AuthFailed(format!(
                        "auth rejected by plugin `{}`",
                        plugin.alias()
                    )));
                }
                AuthOutcome::Error(e) => return Err(e),
            }
        }
        Ok(injections)
    }

    /// Executes guard plugins in declaration order; first rejection wins.
    pub async fn run_guards(&self, ctx: &AuthContext<'_>) -> Result<(), DomainError> {
        for guard in &self.guards {
            guard.check(ctx).await?;
        }
        Ok(())
    }

    /// Runs the request-id transform on the outbound headers.
    #[must_use]
    pub fn apply_request_id(&self, headers: &mut HeaderMap) -> Option<String> {
        self.request_id.as_ref().map(|t| t.apply(headers))
    }

    /// Emits log/metrics for a completed request if configured.
    pub fn record_access(&self, alias: &str, method: &str, status: u16, elapsed: Duration) {
        if let Some(log) = &self.logging {
            log.record(alias, method, status, elapsed);
        }
        if let Some(metrics) = &self.metrics {
            metrics.record(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_exact_bytes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_differences() {
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(
            !constant_time_eq(b"secret", b"secretx"),
            "length mismatch must fail"
        );
        assert!(!constant_time_eq(b"secretx", b"secret"));
    }
}
