// Created: 2026-08-29 by Constructor Tech
//! Built-in auth plugins.
//!
//! Security: credentials are read from the credential store and injected into
//! the outbound request only. They are never logged, never embedded in an
//! error message and never returned in a response.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use pingora_memory_cache::{CacheStatus, MemoryCache};
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};

use crate::domain::error::OagwError;
use crate::domain::plugin::{
    AUTH_APIKEY, AUTH_NOOP, AUTH_OAUTH2_CC, AUTH_OAUTH2_CC_BASIC, PluginError, RequestContext,
};

/// Single-read accessor for a string-valued plugin config key.
pub(crate) fn config_str<'a>(
    config: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    config.get(key).and_then(serde_json::Value::as_str)
}

/// Read a comma separated config entry into trimmed, lower-cased tokens.
pub(crate) fn config_list(
    config: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<String> {
    config_str(config, key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Turn a `cred://…` reference (or a bare key) into a [`SecretRef`].
///
/// `SecretRef` accepts `[a-zA-Z0-9_-]` only, so hierarchical references are
/// reduced to their last path segment.
fn secret_ref(reference: &str) -> Option<SecretRef> {
    let stripped = reference.trim().trim_start_matches("cred://");
    let last = stripped.rsplit('/').next().unwrap_or(stripped);
    SecretRef::new(last).ok()
}

/// Read a secret through the credential store without ever formatting it.
async fn resolve_secret(
    credstore: &Option<Arc<dyn CredStoreClientV1>>,
    security: &toolkit_security::SecurityContext,
    reference: &str,
) -> Result<String, PluginError> {
    let store = credstore
        .as_ref()
        .ok_or_else(|| PluginError::Authentication("credential store unavailable".to_owned()))?;
    let key = secret_ref(reference).ok_or_else(|| {
        PluginError::Authentication("credential reference is not a valid key".to_owned())
    })?;
    match store.get(security, &key).await {
        Ok(Some(found)) => Ok(String::from_utf8_lossy(found.value.as_bytes()).into_owned()),
        // `Ok(None)` (absent or inaccessible) and `Err(_)` collapse into the
        // same failure: a 401 with no credential material in the detail.
        _ => Err(PluginError::Authentication(
            "credential not available".to_owned(),
        )),
    }
}

// ---------------------------------------------------------------------------------------
// no-op
// ---------------------------------------------------------------------------------------

/// `cf.core.oagw.noop.v1` — always succeeds.
pub struct NoopAuth;

#[async_trait]
impl crate::domain::plugin::AuthPlugin for NoopAuth {
    fn id(&self) -> &str {
        AUTH_NOOP
    }

    fn plugin_type(&self) -> &str {
        "auth_plugin"
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// api key
// ---------------------------------------------------------------------------------------

/// `cf.core.oagw.apikey.v1` — resolves an API key from the credential store and
/// injects it under `header_name`, or into the query under `query_param_name`
/// when that is configured (ADR-0002 "API key injection (header/query)").
pub struct ApiKeyAuth {
    credstore: Option<Arc<dyn CredStoreClientV1>>,
}

impl ApiKeyAuth {
    pub(crate) fn new(credstore: Option<Arc<dyn CredStoreClientV1>>) -> Self {
        Self { credstore }
    }
}

#[async_trait]
impl crate::domain::plugin::AuthPlugin for ApiKeyAuth {
    fn id(&self) -> &str {
        AUTH_APIKEY
    }

    fn plugin_type(&self) -> &str {
        "auth_plugin"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let reference = config_str(&ctx.config, "api_key_ref")
            .ok_or_else(|| {
                PluginError::Authentication(
                    "auth plugin 'apikey' requires 'api_key_ref'".to_owned(),
                )
            })?
            .to_owned();
        let secret = resolve_secret(&self.credstore, &ctx.security, &reference).await?;
        if let Some(query_param) = config_str(&ctx.config, "query_param_name") {
            if query_param.is_empty() {
                return Err(PluginError::Authentication(
                    "auth plugin 'apikey' has an empty query_param_name".to_owned(),
                ));
            }
            ctx.query = Some(append_query_param(
                ctx.query.as_deref(),
                query_param,
                &secret,
            ));
            return Ok(());
        }
        let header_name = config_str(&ctx.config, "header_name")
            .unwrap_or("authorization")
            .to_ascii_lowercase();
        let value = axum::http::HeaderValue::from_str(&secret).map_err(|_| {
            PluginError::Authentication("credential is not a valid header value".to_owned())
        })?;
        ctx.headers.insert(
            axum::http::HeaderName::from_bytes(header_name.as_bytes()).map_err(|_| {
                PluginError::Authentication(
                    "auth plugin 'apikey' has an invalid header_name".to_owned(),
                )
            })?,
            value,
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// oauth2 client credentials
// ---------------------------------------------------------------------------------------

/// Cached OAuth2 token. The bearer material is wrapped in a redacting type.
#[derive(Clone)]
pub struct CachedToken {
    /// Cache key the entry was stored under (verified on lookup).
    pub key: String,
    /// The bearer token.
    pub token: SecretString,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Build the `400 OAUTH2_CONFIG_INVALID` rejection used for malformed auth
/// plugin configuration.
fn invalid_oauth_config(message: &str) -> PluginError {
    PluginError::Rejected {
        status: axum::http::StatusCode::BAD_REQUEST,
        error_code: "OAUTH2_CONFIG_INVALID".to_owned(),
        message: message.to_owned(),
    }
}

/// Deterministic 64-bit FNV-1a digest of the sorted plugin config JSON.
fn config_hash(config: &serde_json::Map<String, serde_json::Value>) -> String {
    let canonical = canonical_json(&serde_json::Value::Object(config.clone()));
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Deterministic, key-sorted JSON rendering (no third-party serializer needed).
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(&map[*key])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

/// `cf.core.oagw.oauth2_client_cred.v1` / `…_basic.v1`.
pub struct OAuth2ClientCredentials {
    credstore: Option<Arc<dyn CredStoreClientV1>>,
    cache: Arc<MemoryCache<String, CachedToken>>,
    ttl: Duration,
    basic: bool,
}

impl OAuth2ClientCredentials {
    pub(crate) fn new(
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        cache_capacity: usize,
        ttl: Duration,
        basic: bool,
    ) -> Self {
        Self {
            credstore,
            cache: Arc::new(MemoryCache::new(cache_capacity.max(1))),
            ttl,
            basic,
        }
    }

    async fn oauth_config(&self, ctx: &RequestContext) -> Result<OAuthClientConfig, PluginError> {
        let endpoint = config_str(&ctx.config, "token_endpoint");
        let issuer = config_str(&ctx.config, "issuer_url");
        match (endpoint.is_some(), issuer.is_some()) {
            (true, true) | (false, false) => {
                return Err(invalid_oauth_config(
                    "auth plugin requires exactly one of 'token_endpoint' or 'issuer_url'",
                ));
            }
            (true, false) | (false, true) => {}
        }
        let client_id_ref = config_str(&ctx.config, "client_id_ref")
            .ok_or_else(|| invalid_oauth_config("auth plugin requires 'client_id_ref'"))?
            .to_owned();
        let client_secret_ref = config_str(&ctx.config, "client_secret_ref")
            .ok_or_else(|| invalid_oauth_config("auth plugin requires 'client_secret_ref'"))?
            .to_owned();

        // Resolve both credentials before building the config so that a missing
        // secret never reaches the token endpoint.
        let client_id = resolve_secret(&self.credstore, &ctx.security, &client_id_ref).await?;
        let client_secret =
            resolve_secret(&self.credstore, &ctx.security, &client_secret_ref).await?;

        let token_endpoint = match endpoint.map(url::Url::parse) {
            Some(Ok(parsed)) => Some(parsed),
            Some(Err(_)) => {
                return Err(invalid_oauth_config(
                    "auth plugin endpoint is not a valid URL",
                ));
            }
            None => None,
        };
        let issuer_url = match issuer.map(url::Url::parse) {
            Some(Ok(parsed)) => Some(parsed),
            Some(Err(_)) => {
                return Err(invalid_oauth_config(
                    "auth plugin endpoint is not a valid URL",
                ));
            }
            None => None,
        };

        let scopes = config_str(&ctx.config, "scopes")
            .unwrap_or_default()
            .split([',', ' '])
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();

        Ok(OAuthClientConfig {
            token_endpoint,
            issuer_url,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes,
            auth_method: if self.basic {
                ClientAuthMethod::Basic
            } else {
                ClientAuthMethod::Form
            },
            ..OAuthClientConfig::default()
        })
    }

    fn cache_key(&self, ctx: &RequestContext) -> String {
        let method_tag = if self.basic { "basic" } else { "form" };
        // The subject's *home* tenant, not the resolved one: a caller acting on
        // behalf of another tenant still owns its own client credentials, so the
        // cached token must not leak across that boundary.
        format!(
            "{}:{}:{}:{}",
            ctx.security.subject_tenant_id(),
            ctx.security.subject_id(),
            method_tag,
            config_hash(&ctx.config)
        )
    }
}

#[async_trait]
impl crate::domain::plugin::AuthPlugin for OAuth2ClientCredentials {
    fn id(&self) -> &str {
        if self.basic {
            AUTH_OAUTH2_CC_BASIC
        } else {
            AUTH_OAUTH2_CC
        }
    }

    fn plugin_type(&self) -> &str {
        "auth_plugin"
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let key = self.cache_key(ctx);
        let (cached, status) = self.cache.get(&key);
        if let (Some(entry), CacheStatus::Hit) = (cached, status)
            && entry.key == key
        {
            inject_bearer(ctx, entry.token.expose())?;
            return Ok(());
        }

        let config = self.oauth_config(ctx).await?;
        let fetched = fetch_token(config).await;
        // A failed fetch is never cached.
        let fetched = fetched.map_err(|_| {
            PluginError::Authentication("token endpoint rejected the client credentials".to_owned())
        })?;

        inject_bearer(ctx, fetched.bearer.expose())?;

        let ttl = self
            .ttl
            .min(fetched.expires_in.saturating_sub(Duration::from_secs(30)));
        if ttl > Duration::ZERO {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: fetched.bearer,
                },
                Some(ttl),
            );
        }
        Ok(())
    }
}

fn inject_bearer(ctx: &mut RequestContext, token: &str) -> Result<(), PluginError> {
    let value = format!("Bearer {token}");
    let header = axum::http::HeaderValue::from_str(&value)
        .map_err(|_| PluginError::Authentication("token is not a valid header value".to_owned()))?;
    ctx.headers
        .insert(axum::http::header::AUTHORIZATION, header);
    Ok(())
}

/// Append `name=value` to a query string, preserving any existing parameters.
///
/// The caller's parameters keep their original order and encoding; the injected
/// one goes last.
#[must_use]
pub fn append_query_param(query: Option<&str>, name: &str, value: &str) -> String {
    // `append_pair` emits exactly `name=value` with the value percent-encoded.
    let pair = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(name, value)
        .finish();
    match query {
        Some(existing) if !existing.is_empty() => format!("{existing}&{pair}"),
        _ => pair,
    }
}

/// Convert a credential-store failure into a domain error without leaking the
/// secret reference or value.
#[must_use]
pub fn secret_error(message: &str) -> OagwError {
    OagwError::SecretNotFound(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parameters_are_appended_without_disturbing_the_callers() {
        assert_eq!(
            append_query_param(None, "api-key", "abc123"),
            "api-key=abc123"
        );
        assert_eq!(
            append_query_param(Some("page=2"), "api-key", "abc123"),
            "page=2&api-key=abc123"
        );
        assert_eq!(
            append_query_param(Some(""), "api-key", "abc123"),
            "api-key=abc123"
        );
        // A value needing encoding stays a single parameter.
        assert_eq!(
            append_query_param(Some("page=2"), "api-key", "a=b&c"),
            "page=2&api-key=a%3Db%26c"
        );
    }

    #[test]
    fn canonical_json_is_key_sorted() {
        let mut map = serde_json::Map::new();
        map.insert("b".to_owned(), serde_json::json!(1));
        map.insert("a".to_owned(), serde_json::json!({"z": true, "y": [1, 2]}));
        assert_eq!(
            canonical_json(&serde_json::Value::Object(map)),
            r#"{"a":{"y":[1,2],"z":true},"b":1}"#
        );
    }

    #[test]
    fn secret_ref_reduces_hierarchical_reference() {
        let reference = secret_ref("cred://oagw/my-api-key-1").expect("valid");
        assert_eq!(reference.as_ref(), "my-api-key-1");
    }

    #[test]
    fn secret_ref_accepts_bare_key() {
        let reference = secret_ref("api_key").expect("valid");
        assert_eq!(reference.as_ref(), "api_key");
    }

    #[test]
    fn config_list_trims_lowercases_and_drops_empty() {
        let mut map = serde_json::Map::new();
        map.insert(
            "required_request_headers".to_owned(),
            serde_json::json!(" X-Trace-Id ,, x-Auth"),
        );
        assert_eq!(
            config_list(&map, "required_request_headers"),
            vec!["x-trace-id", "x-auth"]
        );
        assert!(config_list(&map, "missing").is_empty());
    }

    #[test]
    fn secret_error_has_no_material() {
        let error = secret_error("credential not available");
        assert!(!error.to_string().contains("Bearer"));
    }
}
