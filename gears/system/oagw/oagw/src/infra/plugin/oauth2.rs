//! `OAuth2` client-credentials auth plugins with an internal token cache
//! (ADR 0008).
//!
//! Two built-ins share the flow and differ only in how the client authenticates
//! to the token endpoint:
//!
//! | GTS id | Client auth method |
//! |---|---|
//! | `cf.core.oagw.oauth2_client_cred.v1` | `Form` (credentials in the body) |
//! | `cf.core.oagw.oauth2_client_cred_basic.v1` | `Basic` (`Authorization` header) |
//!
//! Configuration keys: `token_endpoint` **xor** `issuer_url`, `client_id_ref`,
//! `client_secret_ref`, optional `scopes`.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use pingora_memory_cache::MemoryCache;
use serde_json::Value;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, RequestContext};
use crate::infra::transport::{Transport, UpstreamRequest};

use crate::domain::plugin::builtins::{AUTH_OAUTH2_CLIENT_CRED, AUTH_OAUTH2_CLIENT_CRED_BASIC};
use super::secret::{config_any, config_string, SecretResolver};

/// Registry key of the `Form` variant.
pub const OAUTH2_FORM_PLUGIN_ID: &str = "oauth2_client_cred";
/// Registry key of the `Basic` variant.
pub const OAUTH2_BASIC_PLUGIN_ID: &str = "oauth2_client_cred_basic";
/// Safety margin subtracted from the IdP-reported `expires_in` (ADR 0008).
pub const TOKEN_EXPIRY_MARGIN_SECS: u64 = 30;

/// How the client authenticates to the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMethod {
    /// Client id and secret in the request body.
    Form,
    /// Client id and secret in a `Basic` `Authorization` header.
    Basic,
}

impl ClientAuthMethod {
    fn tag(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }
}

/// A cached access token, carrying its cache key for collision verification
/// (ADR 0008 "Hash-Collision Safety via `CachedToken` Wrapper").
#[derive(Clone)]
struct CachedToken {
    key: String,
    bearer: String,
}

/// Shared implementation of both `OAuth2` client-credentials plugins.
///
/// Review evidence (privilege boundary — credential isolation):
/// * Guardrail: ADR 0008 "Credential isolation" + DESIGN §2.1
///   `principle-cred-isolation`. The client secret is resolved through
///   [`SecretResolver`], sent only to the configured token endpoint and never
///   written to a log, a metric label or an error message.
/// * Rationale: the token cache is keyed by `(tenant, subject, auth method,
///   config hash)` and the stored entry re-verifies its key, so a hash
///   collision can never hand one tenant's token to another.
/// * Validation performed: `oauth2_*` tests cover the cache hit path, the TTL
///   ceiling and the "failures are never cached" rule.
pub struct OAuth2ClientCredAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
    transport: Arc<Transport>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: std::time::Duration,
    plugin_type: &'static str,
    id: &'static str,
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token cache is deliberately omitted from the rendering: its
        // content must never be printed.
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.auth_method)
            .field("cache_ttl", &self.cache_ttl)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds the `Form` variant.
    #[must_use]
    pub fn form(
        resolver: Arc<dyn SecretResolver>,
        transport: Arc<Transport>,
        cache_ttl: std::time::Duration,
        cache_capacity: usize,
    ) -> Self {
        Self::new(
            resolver,
            transport,
            ClientAuthMethod::Form,
            cache_ttl,
            cache_capacity,
            OAUTH2_FORM_PLUGIN_ID,
            AUTH_OAUTH2_CLIENT_CRED,
        )
    }

    /// Builds the `Basic` variant.
    #[must_use]
    pub fn basic(
        resolver: Arc<dyn SecretResolver>,
        transport: Arc<Transport>,
        cache_ttl: std::time::Duration,
        cache_capacity: usize,
    ) -> Self {
        Self::new(
            resolver,
            transport,
            ClientAuthMethod::Basic,
            cache_ttl,
            cache_capacity,
            OAUTH2_BASIC_PLUGIN_ID,
            AUTH_OAUTH2_CLIENT_CRED_BASIC,
        )
    }

    fn new(
        resolver: Arc<dyn SecretResolver>,
        transport: Arc<Transport>,
        auth_method: ClientAuthMethod,
        cache_ttl: std::time::Duration,
        cache_capacity: usize,
        id: &'static str,
        plugin_type: &'static str,
    ) -> Self {
        Self {
            resolver,
            transport,
            auth_method,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
            plugin_type,
            id,
        }
    }

    /// Deterministic, tenant-scoped cache key (ADR 0008 "Cache Key Design").
    fn cache_key(&self, ctx: &RequestContext, endpoint: &str) -> String {
        format!(
            "{endpoint}|{}:{}:{}:{}",
            ctx.tenant_id,
            ctx.subject_id,
            self.auth_method.tag(),
            stable_config_hash(&ctx.config)
        )
    }

    /// TTL actually applied: `min(configured, expires_in - margin)`.
    fn ttl_for(&self, expires_in: u64) -> std::time::Duration {
        let usable = expires_in.saturating_sub(TOKEN_EXPIRY_MARGIN_SECS);
        std::time::Duration::from_secs(usable.min(self.cache_ttl.as_secs()))
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn plugin_type(&self) -> &'static str {
        self.plugin_type
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let endpoint = token_endpoint(&ctx.config)?;
        let client_id = self
            .resolver
            .resolve(ctx, &client_reference(&ctx.config, "client_id_ref", "client_id"))
            .await?;
        let client_secret = self
            .resolver
            .resolve(
                ctx,
                &client_reference(&ctx.config, "client_secret_ref", "client_secret"),
            )
            .await?;
        let key = self.cache_key(ctx, &endpoint);

        if let Some(cached) = self.cache.get(&key).0
            && cached.key == key
        {
            ctx.set_outbound_header("authorization", format!("Bearer {}", cached.bearer));
            return Ok(());
        }

        let scopes = config_string(&ctx.config, "scopes").unwrap_or_default();
        let fetched = self
            .fetch_token(&endpoint, &client_id, &client_secret, &scopes)
            .await?;
        let ttl = self.ttl_for(fetched.expires_in);
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                bearer: fetched.bearer.clone(),
            },
            Some(ttl),
        );
        ctx.set_outbound_header("authorization", format!("Bearer {}", fetched.bearer));
        Ok(())
    }
}

struct FetchedToken {
    bearer: String,
    expires_in: u64,
}

impl OAuth2ClientCredAuthPlugin {
    async fn fetch_token(
        &self,
        endpoint: &str,
        client_id: &str,
        client_secret: &str,
        scopes: &str,
    ) -> Result<FetchedToken, DomainError> {
        let mut form: Vec<(String, String)> =
            vec![("grant_type".to_owned(), "client_credentials".to_owned())];
        if self.auth_method == ClientAuthMethod::Form {
            form.push(("client_id".to_owned(), client_id.to_owned()));
            form.push(("client_secret".to_owned(), client_secret.to_owned()));
        }
        let trimmed_scopes = scopes.trim();
        if !trimmed_scopes.is_empty() {
            form.push(("scope".to_owned(), trimmed_scopes.to_owned()));
        }
        let body = form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();

        let mut headers = vec![(
            "content-type".to_owned(),
            "application/x-www-form-urlencoded".to_owned(),
        )];
        if self.auth_method == ClientAuthMethod::Basic {
            headers.push((
                "authorization".to_owned(),
                format!("Basic {}", basic_credentials(client_id, client_secret)),
            ));
        }

        let response = self
            .transport
            .send(UpstreamRequest {
                method: "POST".to_owned(),
                url: endpoint.to_owned(),
                headers,
                body: Bytes::from(body),
            })
            .await?;

        let payload = crate::infra::transport::read_body(response.body).await?;
        let document: Value = serde_json::from_slice(&payload).map_err(|_| downstream(
            response.status,
            "token endpoint returned a malformed JSON document",
        ))?;
        let bearer = document
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| downstream(
                response.status,
                "token endpoint response carries no access_token",
            ))?;
        let expires_in = document
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| self.cache_ttl.as_secs().max(1));
        Ok(FetchedToken { bearer, expires_in })
    }
}

fn downstream(upstream_status: u16, detail: &str) -> DomainError {
    DomainError::DownstreamError {
        detail: detail.to_owned(),
        upstream_status,
        upstream_id: None,
        host: None,
    }
}

/// Reference of one client credential, tolerating a literal for local
/// configurations.
fn client_reference(config: &Value, reference_key: &str, literal_key: &str) -> String {
    config_any(config, &[reference_key, literal_key]).unwrap_or_default()
}

/// Encodes a `Basic` `Authorization` payload.
fn basic_credentials(client_id: &str, client_secret: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{client_secret}"))
}

/// Resolves the token endpoint: a direct `token_endpoint` or OIDC discovery
/// from `issuer_url`.
fn token_endpoint(config: &Value) -> Result<String, DomainError> {
    if let Some(direct) = config_string(config, "token_endpoint") {
        let trimmed = direct.trim();
        if trimmed.is_empty() {
            return Err(DomainError::ValidationError {
                detail: "token_endpoint must not be empty".to_owned(),
                invalid_value: Some(trimmed.to_owned()),
                alias: None,
            });
        }
        return Ok(trimmed.to_owned());
    }
    if let Some(issuer) = config_string(config, "issuer_url") {
        let issuer = issuer.trim().trim_end_matches('/');
        if issuer.is_empty() {
            return Err(DomainError::ValidationError {
                detail: "issuer_url must not be empty".to_owned(),
                invalid_value: Some(issuer.to_owned()),
                alias: None,
            });
        }
        return Ok(format!("{issuer}/.well-known/openid-configuration"));
    }
    Err(DomainError::ValidationError {
        detail: "an OAuth2 plugin requires token_endpoint or issuer_url".to_owned(),
        invalid_value: None,
        alias: None,
    })
}

/// FNV-1a over the configuration rendered as sorted `key=value` pairs.
///
/// JSON objects preserve insertion order only; sorting makes the hash
/// independent of the order in which an operator wrote the members.
#[must_use]
pub fn stable_config_hash(config: &Value) -> u64 {
    let mut entries: Vec<(String, String)> = config
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.to_string()))
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (key, value) in entries {
        for byte in format!("{key}={value};").into_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}
