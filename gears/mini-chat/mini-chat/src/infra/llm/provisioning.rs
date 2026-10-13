//! OAGW upstream and route provisioning (ADR-0005). Runs at gear start with
//! the S2S security context; providers whose upstream cannot be created yet
//! (for example a secret that is not readable) are retried in the background
//! (2 s, doubling up to 60 s).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HeadersConfig, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PassthroughMode, PathSuffixMode, RequestHeaderRules,
    Scheme, Server, ServiceGatewayClientV1, SharingMode,
};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use super::{ProviderResolver, S2sContext};
use crate::config::{ClientCredentialsConfig, ProviderConfig, StorageKind};

/// Default alias of an endpoint: `host`, or `host:port` for non-standard ports.
#[must_use]
pub fn default_alias(host: &str, port: u16) -> String {
    Endpoint {
        scheme: Scheme::Https,
        host: host.to_owned(),
        port,
    }
    .alias_contribution()
}

/// One upstream to provision.
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    pub label: String,
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub alias: String,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<HashMap<String, String>>,
    pub routes: Vec<HttpMatch>,
}

fn chat_route(api_path: &str) -> HttpMatch {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = path.split("{model}").next().unwrap_or(path);
    let prefix = if prefix.len() > 1 {
        prefix.trim_end_matches('/').to_owned()
    } else {
        prefix.to_owned()
    };
    let query_allowlist: Vec<String> = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| kv.split('=').next().unwrap_or(kv).to_owned())
        .collect();
    HttpMatch {
        methods: vec![HttpMethod::Post],
        path: if prefix.is_empty() { "/".to_owned() } else { prefix },
        query_allowlist,
        path_suffix_mode: PathSuffixMode::Append,
    }
}

fn storage_routes(kind: StorageKind) -> Vec<HttpMatch> {
    let (prefix, query) = match kind {
        StorageKind::Openai => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    ["files", "vector_stores"]
        .iter()
        .map(|p| HttpMatch {
            methods: vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete],
            path: format!("{prefix}/{p}"),
            query_allowlist: query.clone(),
            path_suffix_mode: PathSuffixMode::Append,
        })
        .collect()
}

/// Upstream specs of every provider entry and tenant override.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn upstream_specs(providers: &HashMap<String, ProviderConfig>) -> Vec<UpstreamSpec> {
    let mut out = Vec::new();
    let mut ids: Vec<&String> = providers.keys().collect();
    ids.sort();
    for id in ids {
        let p = &providers[id];
        let mut routes = vec![chat_route(&p.api_path)];
        if let Some(kind) = p.storage_kind {
            routes.extend(storage_routes(kind));
        }
        let scheme = if p.use_http { Scheme::Http } else { Scheme::Https };
        let port = p.effective_port();
        out.push(UpstreamSpec {
            label: id.clone(),
            scheme,
            host: p.host.clone(),
            port,
            alias: super::configured_alias(p.upstream_alias.as_ref(), &p.host, port),
            auth_plugin_type: p.auth_plugin_type.clone(),
            auth_config: p.auth_config.clone(),
            routes: routes.clone(),
        });
        let mut tenants: Vec<&String> = p.tenant_overrides.keys().collect();
        tenants.sort();
        for t in tenants {
            let o = &p.tenant_overrides[t];
            let host = o.host.clone().unwrap_or_else(|| p.host.clone());
            out.push(UpstreamSpec {
                label: format!("{id}/{t}"),
                scheme,
                host: host.clone(),
                port,
                alias: super::configured_alias(o.upstream_alias.as_ref(), &host, port),
                auth_plugin_type: o.auth_plugin_type.clone().or_else(|| p.auth_plugin_type.clone()),
                auth_config: o.auth_config.clone().or_else(|| p.auth_config.clone()),
                routes: routes.clone(),
            });
        }
    }
    out
}

/// Outcome of provisioning one upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionOutcome {
    Ready,
    /// Retry later (e.g. secret not readable yet).
    Deferred(String),
}

/// OAGW provisioner.
pub struct Provisioner {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    authn: Option<Arc<dyn AuthNResolverClient>>,
    creds: ClientCredentialsConfig,
    resolver: Arc<ProviderResolver>,
    s2s: Arc<S2sContext>,
    /// Upstreams deferred at start, retried by the reconcile loop and on demand.
    pending: tokio::sync::Mutex<Vec<UpstreamSpec>>,
}

impl Provisioner {
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        authn: Option<Arc<dyn AuthNResolverClient>>,
        creds: ClientCredentialsConfig,
        resolver: Arc<ProviderResolver>,
        s2s: Arc<S2sContext>,
    ) -> Self {
        Self {
            gateway,
            authn,
            creds,
            resolver,
            s2s,
            pending: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// Exchanges the client credentials for the S2S context.
    ///
    /// # Errors
    /// `AuthN` failure.
    pub async fn ensure_s2s(&self) -> Result<SecurityContext, String> {
        if let Some(ctx) = self.s2s.get() {
            return Ok(ctx);
        }
        let authn = self
            .authn
            .as_ref()
            .ok_or_else(|| "authn resolver client is not available".to_owned())?;
        let res = authn
            .exchange_client_credentials(&ClientCredentialsRequest {
                client_id: self.creds.client_id.clone(),
                client_secret: SecretString::from(self.creds.client_secret.clone()),
                scopes: Vec::new(),
            })
            .await
            .map_err(|e| format!("client credentials exchange failed: {e}"))?;
        self.s2s.set(res.security_context.clone());
        Ok(res.security_context)
    }

    async fn find_by_alias(&self, ctx: &SecurityContext, alias: &str) -> Option<uuid::Uuid> {
        let mut skip = 0;
        loop {
            let page = self
                .gateway
                .list_upstreams(ctx.clone(), &ListQuery { top: 100, skip })
                .await
                .ok()?;
            if let Some(u) = page.iter().find(|u| u.alias.eq_ignore_ascii_case(alias)) {
                return Some(u.id);
            }
            if page.len() < 100 {
                return None;
            }
            skip += 100;
        }
    }

    fn build_upstream(spec: &UpstreamSpec, with_alias: bool) -> CreateUpstreamRequest {
        let mut b = CreateUpstreamRequest::builder(
            Server {
                endpoints: vec![Endpoint {
                    scheme: spec.scheme,
                    host: spec.host.clone(),
                    port: spec.port,
                }],
            },
            oagw_sdk::HTTP_PROTOCOL_ID,
        )
        .headers(HeadersConfig {
            request: Some(RequestHeaderRules {
                passthrough: PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["content-type".to_owned(), "accept".to_owned()],
                ..RequestHeaderRules::default()
            }),
            response: None,
        })
        .tags(vec!["mini-chat".to_owned(), format!("mini-chat-provider:{}", spec.label)]);
        if with_alias {
            b = b.alias(spec.alias.clone());
        }
        if let Some(plugin) = spec.auth_plugin_type.clone().filter(|p| !p.trim().is_empty()) {
            b = b.auth(AuthConfig {
                plugin_type: plugin,
                sharing: SharingMode::Inherit,
                config: spec.auth_config.clone(),
            });
        }
        b.build()
    }

    /// Provisions one upstream and its routes.
    pub async fn provision_one(&self, ctx: &SecurityContext, spec: &UpstreamSpec) -> ProvisionOutcome {
        let mut created = self
            .gateway
            .create_upstream(ctx.clone(), Self::build_upstream(spec, true))
            .await;
        if let Err(CanonicalError::InvalidArgument { .. }) = &created {
            // Hostname endpoints get a derived alias; retry without an explicit one.
            created = self
                .gateway
                .create_upstream(ctx.clone(), Self::build_upstream(spec, false))
                .await;
        }
        let upstream_id = match created {
            Ok(u) => {
                self.resolver.record_actual_alias(&spec.alias, &u.alias);
                u.id
            }
            Err(CanonicalError::AlreadyExists { .. }) => {
                match self.find_by_alias(ctx, &spec.alias).await {
                    Some(id) => id,
                    None => return ProvisionOutcome::Deferred("existing upstream not found".to_owned()),
                }
            }
            Err(e) => {
                return ProvisionOutcome::Deferred(format!("create upstream: {e}"));
            }
        };
        for http in &spec.routes {
            let req = CreateRouteRequest::builder(
                upstream_id,
                MatchRules {
                    http: Some(http.clone()),
                    grpc: None,
                },
            )
            .tags(vec!["mini-chat".to_owned()])
            .build();
            match self.gateway.create_route(ctx.clone(), req).await {
                Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => {}
                Err(e) => {
                    tracing::warn!(provider = %spec.label, path = %http.path, error = %e, "route creation failed");
                }
            }
        }
        ProvisionOutcome::Ready
    }

    /// Provisions every upstream; returns the labels still pending.
    #[allow(clippy::cognitive_complexity)]
    pub async fn provision_all(&self, specs: &[UpstreamSpec]) -> Vec<UpstreamSpec> {
        let ctx = match self.ensure_s2s().await {
            Ok(ctx) => ctx,
            Err(e) => {
                tracing::warn!(error = %e, "OAGW provisioning deferred: no S2S context");
                return specs.to_vec();
            }
        };
        let mut pending = Vec::new();
        for spec in specs {
            match self.provision_one(&ctx, spec).await {
                ProvisionOutcome::Ready => {
                    tracing::info!(provider = %spec.label, alias = %spec.alias, "OAGW upstream provisioned");
                }
                ProvisionOutcome::Deferred(reason) => {
                    tracing::warn!(provider = %spec.label, %reason, "OAGW upstream provisioning deferred");
                    pending.push(spec.clone());
                }
            }
        }
        pending
    }

    /// Provisions every upstream at start; deferred ones are kept for the
    /// reconcile loop and on-demand attempts. Returns whether any is pending.
    pub async fn start(&self, specs: &[UpstreamSpec]) -> bool {
        let remaining = self.provision_all(specs).await;
        self.resolver
            .set_pending(remaining.iter().map(|s| s.alias.clone()));
        let any = !remaining.is_empty();
        *self.pending.lock().await = remaining;
        any
    }

    async fn retry_pending(&self, only_alias: Option<&str>) {
        let mut guard = self.pending.lock().await;
        let (try_now, keep): (Vec<UpstreamSpec>, Vec<UpstreamSpec>) = guard
            .drain(..)
            .partition(|s| only_alias.is_none_or(|a| s.alias == a));
        if try_now.is_empty() {
            *guard = keep;
            return;
        }
        let still = self.provision_all(&try_now).await;
        for s in &try_now {
            if !still.iter().any(|p| p.alias == s.alias) {
                self.resolver.mark_ready(&s.alias);
            }
        }
        *guard = keep;
        guard.extend(still);
    }

    /// Background reconcile loop for deferred upstreams: first retry after
    /// 2 s, doubling up to 60 s; one warning after 2 minutes.
    pub async fn reconcile(self: Arc<Self>, cancel: CancellationToken) {
        let mut delay = Duration::from_secs(2);
        let started = tokio::time::Instant::now();
        let mut warned = false;
        loop {
            if self.pending.lock().await.is_empty() {
                return;
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            self.retry_pending(None).await;
            delay = (delay * 2).min(Duration::from_secs(60));
            let pending = self.pending.lock().await;
            if !warned && !pending.is_empty() && started.elapsed() > Duration::from_secs(120) {
                warned = true;
                let labels: Vec<&str> = pending.iter().map(|s| s.label.as_str()).collect();
                tracing::warn!(providers = ?labels, "OAGW providers still pending after 2 minutes");
            }
        }
    }
}

#[async_trait::async_trait]
impl super::ProvisionHook for Provisioner {
    async fn ensure_ready(&self, alias: &str) {
        self.retry_pending(Some(alias)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_route_prefix_and_query() {
        let r = chat_route("/openai/deployments/{model}/chat/completions?api-version=2024-10-21");
        assert_eq!(r.path, "/openai/deployments");
        assert_eq!(r.query_allowlist, vec!["api-version".to_owned()]);
        let r = chat_route("/v1/responses");
        assert_eq!(r.path, "/v1/responses");
        assert!(r.query_allowlist.is_empty());
    }

    #[test]
    fn default_alias_includes_non_standard_port() {
        assert_eq!(default_alias("api.openai.com", 443), "api.openai.com");
        assert_eq!(default_alias("127.0.0.1", 18080), "127.0.0.1:18080");
    }

    #[test]
    fn specs_cover_overrides_and_storage_routes() {
        let mut p = ProviderConfig::default_openai();
        p.tenant_overrides.insert(
            "t1".to_owned(),
            crate::config::TenantOverrideConfig {
                host: Some("t1.example.com".to_owned()),
                ..Default::default()
            },
        );
        let mut m = HashMap::new();
        m.insert("openai".to_owned(), p);
        let specs = upstream_specs(&m);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].routes.len(), 3);
        assert_eq!(specs[1].alias, "t1.example.com");
        assert_eq!(specs[1].auth_plugin_type, specs[0].auth_plugin_type);
    }
}
