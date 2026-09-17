// Created: 2026-09-03 by Constructor Tech
//! OAuth2 client-credentials token acquisition and caching.
//!
//! Implements `ADR/0008-oauth2-client-credentials-auth-plugin.md`: the cache
//! key is derived from the tenant, subject, auth method tag and configuration
//! hash, and the entry TTL is the lesser of the configured ceiling and
//! `expires_in - 30s`.

use http::header::{AUTHORIZATION, CONTENT_TYPE};
use pingora_memory_cache::MemoryCache;
use serde_json::Value;

use crate::error::{ErrorKind, OagwError};
use crate::gts;
use crate::plugins::{config_str, AUTH_OAUTH2, AUTH_OAUTH2_BASIC};

/// A cached access token with its absolute expiry.
#[derive(Debug, Clone)]
pub struct CachedToken {
    /// The `access_token` value returned by the token endpoint.
    pub access_token: String,
    /// Absolute expiry in milliseconds since the Unix epoch.
    pub expires_at: i64,
}

/// Process-local cache of OAuth2 access tokens.
pub struct TokenCache {
    cache: MemoryCache<String, CachedToken>,
    ttl_ceiling: u64,
}

impl TokenCache {
    /// Creates a cache bounded to `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize, ttl_ceiling_secs: u64) -> Self {
        Self {
            cache: MemoryCache::new(capacity.max(1)),
            ttl_ceiling: ttl_ceiling_secs,
        }
    }

    /// Looks up a live token.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<CachedToken> {
        let (token, _status) = self.cache.get(key);
        token
    }

    /// Stores a token under `key` for `ttl_secs`.
    pub fn put(&self, key: &str, token: CachedToken, ttl_secs: u64) {
        self.cache.put(
            key,
            token,
            Some(std::time::Duration::from_secs(ttl_secs.max(1))),
        );
    }

    /// The configured cache ceiling, in seconds.
    #[must_use]
    pub fn ttl_ceiling(&self) -> u64 {
        self.ttl_ceiling
    }
}

/// The cache key of a client-credentials token.
#[must_use]
pub fn cache_key(tenant: uuid::Uuid, subject: uuid::Uuid, tag: &str, config: &Value) -> String {
    let fingerprint = serde_json::to_string(config).unwrap_or_default();
    format!("{tenant}:{subject}:{tag}:{fingerprint}")
}

/// Resolves a credential value that may be a `cred://` reference.
///
/// # Errors
/// Returns 500 `SecretNotFound` when the reference cannot be resolved.
pub async fn resolve_secret(
    credstore: Option<&std::sync::Arc<dyn credstore_sdk::CredStoreClientV1>>,
    sec: &toolkit_security::SecurityContext,
    value: &str,
) -> Result<String, OagwError> {
    let Some(path) = value.strip_prefix("cred://") else {
        return Ok(value.to_owned());
    };
    let Some(client) = credstore else {
        return Err(OagwError::new(
            ErrorKind::SecretNotFound,
            format!("credential reference '{value}' cannot be resolved: no cred store configured"),
        ));
    };
    let reference = credstore_sdk::SecretRef::new(path).map_err(|error| {
        OagwError::new(
            ErrorKind::Validation,
            format!("credential reference '{value}' is malformed: {error}"),
        )
    })?;
    let resolved = client.get(sec, &reference).await.map_err(|error| {
        OagwError::new(
            ErrorKind::SecretNotFound,
            format!("credential reference '{value}' could not be read: {error}"),
        )
    })?;
    let bytes = resolved
        .map(|secret| secret.value.as_bytes().to_vec())
        .unwrap_or_default();
    String::from_utf8(bytes).map_err(|_| {
        OagwError::new(
            ErrorKind::SecretNotFound,
            format!("credential reference '{value}' is not valid UTF-8"),
        )
    })
}

/// The auth method tag used in the token cache key.
#[must_use]
pub fn method_tag(basic: bool) -> &'static str {
    if basic {
        AUTH_OAUTH2_BASIC
    } else {
        AUTH_OAUTH2
    }
}

/// Requests an access token with the client-credentials grant.
///
/// `basic` selects the HTTP Basic variant of the grant; otherwise the client
/// credentials travel in the request body.
///
/// # Errors
/// Returns 400 `ValidationError` for a missing token URL, 500
/// `SecretNotFound` for unresolvable credentials and 401 `AuthFailed` when
/// the token endpoint refuses the grant.
pub async fn fetch_token(
    credstore: Option<&std::sync::Arc<dyn credstore_sdk::CredStoreClientV1>>,
    connector: &crate::upstream_client::UpstreamConnector,
    sec: &toolkit_security::SecurityContext,
    config: &Value,
    basic: bool,
    request_timeout: std::time::Duration,
) -> Result<CachedToken, OagwError> {
    let token_url = config_str(config, "token_url").ok_or_else(|| {
        OagwError::new(
            ErrorKind::Validation,
            "oauth2 client-credentials plugin requires config.token_url",
        )
    })?;
    let client_id = resolve_secret(
        credstore,
        sec,
        config_str(config, "client_id")
            .as_deref()
            .unwrap_or_default(),
    )
    .await?;
    let client_secret = resolve_secret(
        credstore,
        sec,
        config_str(config, "client_secret")
            .as_deref()
            .unwrap_or_default(),
    )
    .await?;
    let scopes = crate::plugins::config_list(config, "scopes").join(" ");
    let scope = scopes.as_str();

    let body = {
        let mut form = form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "client_credentials");
        if !basic {
            form.append_pair("client_id", &client_id);
            form.append_pair("client_secret", &client_secret);
        }
        if !scope.is_empty() {
            form.append_pair("scope", scope);
        }
        bytes::Bytes::from(form.finish())
    };

    let parsed = url::Url::parse(&token_url).map_err(|_| {
        OagwError::new(
            ErrorKind::Validation,
            format!("config.token_url '{token_url}' is not a valid URL"),
        )
    })?;
    let host = parsed.host_str().unwrap_or_default().to_owned();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let scheme = if parsed.scheme() == "https" {
        crate::model::EndpointScheme::Https
    } else {
        crate::model::EndpointScheme::Http
    };
    let io = crate::upstream_client::dial(
        connector,
        scheme,
        &host,
        port,
        request_timeout,
    )
    .await?;
    let mut builder = http::Request::builder()
        .method(http::Method::POST)
        .uri(parsed.as_str())
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded");
    if basic {
        let credentials = format!("{client_id}:{client_secret}");
        builder = builder.header(AUTHORIZATION, format!("Basic {}", b64(&credentials)));
    }
    let request = builder
        .body(crate::body::ProxyBody::new(
            axum::body::Body::from(body),
            u64::MAX,
        ))
        .map_err(|error| {
            OagwError::new(
                ErrorKind::ProtocolError,
                format!("token request could not be built: {error}"),
            )
        })?;
    let response = crate::upstream_client::dispatch(io, request, request_timeout).await?;
    let status = response.status();
    let payload = crate::body::collect(response.into_body(), 1024 * 1024).await?;
    if !status.is_success() {
        return Err(OagwError::new(
            ErrorKind::AuthFailed,
            format!("token endpoint returned {status}"),
        ));
    }
    let document: Value = serde_json::from_slice(&payload).map_err(|error| {
        OagwError::new(
            ErrorKind::ProtocolError,
            format!("token endpoint returned an unreadable payload: {error}"),
        )
    })?;
    let access_token = document
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            OagwError::new(
                ErrorKind::AuthFailed,
                "token endpoint did not return an access_token",
            )
        })?;
    let expires_in = document
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(300);
    let configured = config_str(config, "ttl_secs")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(crate::config::DEFAULT_TOKEN_CACHE_TTL_SECS);
    let ttl = configured.min(expires_in.saturating_sub(30)).max(1);
    Ok(CachedToken {
        access_token,
        expires_at: crate::model::now_millis() + i64::try_from(ttl * 1000).unwrap_or(i64::MAX),
    })
}

/// Standard base64 encoding (RFC 4648, no padding-aware URL alphabet needed
/// for Basic credentials).
fn b64(input: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |b| u32::from(*b));
        let b2 = chunk.get(2).map_or(0, |b| u32::from(*b));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[usize::try_from((triple >> 18) & 0x3F).unwrap_or(0)]));
        out.push(char::from(ALPHABET[usize::try_from((triple >> 12) & 0x3F).unwrap_or(0)]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[usize::try_from((triple >> 6) & 0x3F).unwrap_or(0)])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[usize::try_from(triple & 0x3F).unwrap_or(0)])
        } else {
            '='
        });
    }
    out
}

/// The named plugin identifier of an OAuth2 auth plugin variant.
#[must_use]
pub fn plugin_id(basic: bool) -> String {
    let name = if basic {
        AUTH_OAUTH2_BASIC
    } else {
        AUTH_OAUTH2
    };
    gts::named_plugin_id(gts::AUTH_PLUGIN_TYPE, name)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_and_scoped() {
        let config = serde_json::json!({ "token_url": "https://auth.example.com/token" });
        let a = cache_key(uuid::Uuid::nil(), uuid::Uuid::nil(), "t", &config);
        let b = cache_key(uuid::Uuid::nil(), uuid::Uuid::nil(), "t", &config);
        let c = cache_key(uuid::Uuid::nil(), uuid::Uuid::max(), "t", &config);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn ttl_ceiling_is_reported() {
        let cache = TokenCache::new(10, 300);
        assert_eq!(cache.ttl_ceiling(), 300);
    }

    #[test]
    fn base64_encodes_known_values() {
        assert_eq!(b64(""), "");
        assert_eq!(b64("f"), "Zg==");
        assert_eq!(b64("fo"), "Zm8=");
        assert_eq!(b64("foo"), "Zm9v");
        assert_eq!(b64("foob"), "Zm9vYg==");
        assert_eq!(b64("fooba"), "Zm9vYmE=");
        assert_eq!(b64("foobar"), "Zm9vYmFy");
    }

    #[test]
    fn plugin_ids_follow_the_named_shape() {
        assert_eq!(
            plugin_id(false),
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1"
        );
        assert_eq!(
            plugin_id(true),
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"
        );
    }
}
