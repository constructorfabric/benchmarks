//! Built-in `oauth2_client_credentials` auth plugin (GTS name
//! `oauth2_client_cred`, see DESIGN §Plugin catalog).
//!
//! Performs the OAuth2 client-credentials grant against the configured token
//! endpoint and caches the resulting bearer token per ADR-0008. The client
//! secret is resolved through the credstore (inline value allowed for
//! non-production) and is never logged or echoed.

use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use serde::Deserialize;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, PluginContext};
use crate::infra::plugin::secret::CredentialSource;

/// Configuration of the OAuth2 client-credentials plugin.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct OAuth2Config {
    /// Token endpoint (`https://…/oauth/token`).
    token_url: String,
    /// OAuth2 client id.
    client_id: String,
    /// Credstore reference holding the client secret.
    client_secret_ref: Option<String>,
    /// Inline client secret (non-production).
    client_secret: Option<String>,
    /// Optional space-separated scope.
    scope: Option<String>,
    /// Optional `audience` parameter.
    audience: Option<String>,
    /// Additional headers for the token request.
    headers: std::collections::BTreeMap<String, String>,
    /// Skew subtracted from `expires_in` before caching (seconds).
    refresh_skew_seconds: u64,
}

/// A cached access token.
#[derive(Clone)]
struct CachedToken {
    access_token: String,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Auth plugin implementing the OAuth2 client-credentials grant.
///
/// The same flow is registered twice: once as `oauth2_client_cred` (credentials
/// in the form body) and once as `oauth2_client_cred_basic` (credentials as an
/// HTTP `Authorization: Basic …` header at the token endpoint). The two spell
/// out the `ClientAuthMethod::form` / `ClientAuthMethod::basic` alternatives of
/// RFC 6749 §2.3.1.
pub struct OAuth2ClientCredentialsPlugin {
    secrets: CredentialSource,
    cache: MemoryCache<String, CachedToken>,
    /// HTTP client used for the token request.
    http: toolkit_http::HttpClient,
    /// How the client credentials reach the token endpoint.
    auth_method: toolkit_auth::oauth2::ClientAuthMethod,
}

impl std::fmt::Debug for OAuth2ClientCredentialsPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredentialsPlugin")
            .field("secrets", &self.secrets)
            .finish()
    }
}

impl OAuth2ClientCredentialsPlugin {
    /// Build the form-body variant with the given token-cache capacity.
    ///
    /// # Errors
    /// Returns an error when the underlying HTTP client cannot be built.
    pub fn new(secrets: CredentialSource, cache_capacity: usize) -> Result<Self, DomainError> {
        Self::with_auth_method(
            secrets,
            cache_capacity,
            toolkit_auth::oauth2::ClientAuthMethod::Form,
        )
    }

    /// Build the plugin with an explicit client-authentication method.
    ///
    /// # Errors
    /// Returns an error when the underlying HTTP client cannot be built.
    pub fn with_auth_method(
        secrets: CredentialSource,
        cache_capacity: usize,
        auth_method: toolkit_auth::oauth2::ClientAuthMethod,
    ) -> Result<Self, DomainError> {
        Ok(Self {
            secrets,
            cache: MemoryCache::new(cache_capacity.max(1)),
            http: toolkit_http::HttpClient::new()
                .map_err(|e| DomainError::AuthPluginUnavailable(format!("http client: {e}")))?,
            auth_method,
        })
    }

    fn cache_key(tenant_id: uuid::Uuid, config: &serde_json::Value) -> String {
        format!("{tenant_id}:{config}")
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredentialsPlugin {
    fn name(&self) -> &'static str {
        if self.auth_method == toolkit_auth::oauth2::ClientAuthMethod::Basic {
            "oauth2_client_cred_basic"
        } else {
            "oauth2_client_cred"
        }
    }

    fn validate_config(&self, config: &serde_json::Value) -> Result<(), DomainError> {
        let parsed: OAuth2Config = serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Validation(format!("invalid oauth2 config: {e}")))?;
        if parsed.token_url.trim().is_empty() {
            return Err(DomainError::Validation(
                "oauth2 requires a non-empty 'token_url'".into(),
            ));
        }
        if parsed.client_id.trim().is_empty() {
            return Err(DomainError::Validation(
                "oauth2 requires a non-empty 'client_id'".into(),
            ));
        }
        if parsed.client_secret_ref.is_none() && parsed.client_secret.is_none() {
            return Err(DomainError::Validation(
                "oauth2 requires either 'client_secret_ref' or 'client_secret'".into(),
            ));
        }
        Ok(())
    }

    async fn authenticate(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError> {
        let parsed: OAuth2Config = serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Validation(format!("invalid oauth2 config: {e}")))?;
        let key = Self::cache_key(ctx.tenant_id, config);
        if let (Some(cached), _) = self.cache.get(&key) {
            let value = http::HeaderValue::from_str(&format!("Bearer {}", cached.access_token))
                .map_err(|_| DomainError::AuthFailed("cached token is not header-safe".into()))?;
            headers.insert(http::header::AUTHORIZATION, value);
            return Ok(());
        }

        let token = self.fetch_token(ctx, config, &parsed).await?;
        let ttl = token
            .expires_in
            .unwrap_or(300)
            .saturating_sub(parsed.refresh_skew_seconds)
            .max(1);
        self.cache.put(
            &key,
            CachedToken {
                access_token: token.access_token.clone(),
            },
            Some(Duration::from_secs(ttl)),
        );
        let value = http::HeaderValue::from_str(&format!("Bearer {}", token.access_token))
            .map_err(|_| DomainError::AuthFailed("token is not header-safe".into()))?;
        headers.insert(http::header::AUTHORIZATION, value);
        Ok(())
    }
}

impl OAuth2ClientCredentialsPlugin {
    async fn fetch_token(
        &self,
        ctx: &PluginContext,
        raw_config: &serde_json::Value,
        config: &OAuth2Config,
    ) -> Result<TokenResponse, DomainError> {
        let secret = self.secrets.resolve(ctx, raw_config).await?;

        // RFC 6749 §2.3.1: the credentials travel either in the request body or
        // in an `Authorization: Basic` header. Both paths carry the secret in a
        // header-safe form and never in a log line.
        let mut builder = self.http.post(config.token_url.trim());
        let mut pairs: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
        if self.auth_method == toolkit_auth::oauth2::ClientAuthMethod::Basic {
            let credentials = super::base64::encode(format!("{}:{}", config.client_id, secret).as_bytes());
            let header_value = format!("Basic {credentials}");
            builder = builder.header("authorization", header_value.as_str());
        } else {
            pairs.push(("client_id", config.client_id.as_str()));
            pairs.push(("client_secret", secret.as_str()));
        }
        if let Some(scope) = config.scope.as_deref().filter(|s| !s.trim().is_empty()) {
            pairs.push(("scope", scope));
        }
        if let Some(audience) = config.audience.as_deref().filter(|s| !s.trim().is_empty()) {
            pairs.push(("audience", audience));
        }

        let response = builder
            .form(&pairs)
            .map_err(|e| DomainError::AuthFailed(format!("token request encoding: {e}")))?
            .send()
            .await
            .map_err(|e| DomainError::DownstreamError(format!("token endpoint unreachable: {e}")))?;
        if !response.status().is_success() {
            return Err(DomainError::AuthFailed(format!(
                "token endpoint returned {}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| DomainError::ProtocolError(format!("token response body: {e}")))?;
        let parsed: TokenResponse = serde_json::from_slice(&bytes)
            .map_err(|_| DomainError::ProtocolError("token response is not JSON".into()))?;
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_per_tenant_and_per_config() {
        let config = serde_json::json!({ "token_url": "https://a", "client_id": "c" });
        let a = OAuth2ClientCredentialsPlugin::cache_key(uuid::Uuid::nil(), &config);
        let b = OAuth2ClientCredentialsPlugin::cache_key(uuid::Uuid::nil(), &config);
        assert_eq!(a, b);
        let other = OAuth2ClientCredentialsPlugin::cache_key(uuid::Uuid::now_v7(), &config);
        assert_ne!(a, other);
    }

    fn test_plugin() -> OAuth2ClientCredentialsPlugin {
        init_test_crypto();
        OAuth2ClientCredentialsPlugin::new(CredentialSource::inline_only(), 16)
            .expect("default http client")
    }

    /// Install a rustls provider for tests that build an HTTP client.
    ///
    /// The runtime does this in the toolkit bootstrap
    /// (`toolkit::bootstrap::init_crypto_provider`); unit tests have to do it
    /// themselves because nothing else in the process ran the bootstrap.
    fn init_test_crypto() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let _ = rustls::crypto::CryptoProvider::install_default(
                rustls::crypto::aws_lc_rs::default_provider(),
            );
        });
    }

    #[tokio::test]
    async fn basic_auth_method_is_advertised_under_its_own_short_name() {
        init_test_crypto();
        let form = OAuth2ClientCredentialsPlugin::new(CredentialSource::inline_only(), 16)
            .expect("form plugin");
        let basic = OAuth2ClientCredentialsPlugin::with_auth_method(
            CredentialSource::inline_only(),
            16,
            toolkit_auth::oauth2::ClientAuthMethod::Basic,
        )
        .expect("basic plugin");
        assert_eq!(form.name(), "oauth2_client_cred");
        assert_eq!(basic.name(), "oauth2_client_cred_basic");
    }

    /// RFC 6749 §2.3.1: the client id and secret travel as `id:secret`,
    /// base64-encoded, in an `Authorization: Basic` header — instead of the
    /// form body the `oauth2_client_cred` variant uses.
    #[test]
    fn basic_credentials_encode_client_id_and_secret() {
        let secret = "s3cr3t";
        let expected = super::super::base64::encode(format!("client-id:{secret}").as_bytes());
        assert_eq!(expected, "Y2xpZW50LWlkOnMzY3IzdA==");
        assert!(
            !expected.contains(secret),
            "the secret is not readable in the encoded header value"
        );
    }

    /// A plugin whose HTTP client may talk to a plaintext mock server.
    #[cfg(not(feature = "fips"))]
    fn test_plugin_with_auth_method(
        auth_method: toolkit_auth::oauth2::ClientAuthMethod,
    ) -> OAuth2ClientCredentialsPlugin {
        init_test_crypto();
        let http = toolkit_http::HttpClient::builder()
            .transport(toolkit_http::TransportSecurity::AllowInsecureHttp)
            .build()
            .expect("test http client");
        OAuth2ClientCredentialsPlugin {
            secrets: CredentialSource::inline_only(),
            cache: MemoryCache::new(16),
            http,
            auth_method,
        }
    }

    // The token-endpoint round trip needs a plaintext mock server, which the
    // `fips` build forbids (`HttpError::InsecureTransport`), so these tests only
    // run in non-FIPS configurations — the shape the example server ships with.
    // Each test installs a *single* mock that matches exactly the credential
    // placement it expects: if the plugin picked the other spelling, no mock
    // matches, the token endpoint answers 404 and `authenticate` fails.
    #[cfg(not(feature = "fips"))]
    fn basic_token_plugin() -> (httpmock::MockServer, OAuth2ClientCredentialsPlugin) {
        let server = httpmock::MockServer::start();
        let credentials = super::super::base64::encode(b"client-id:s3cr3t");
        server.mock(|when, then| {
            when.method(httpmock::prelude::POST)
                .path("/oauth/token")
                .header("authorization", format!("Basic {credentials}"));
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-basic","expires_in":300}"#);
        });
        (
            server,
            test_plugin_with_auth_method(toolkit_auth::oauth2::ClientAuthMethod::Basic),
        )
    }

    #[cfg(not(feature = "fips"))]
    fn form_token_plugin() -> (httpmock::MockServer, OAuth2ClientCredentialsPlugin) {
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::prelude::POST)
                .path("/oauth/token")
                .body_includes("client_id=client-id")
                .body_includes("client_secret=s3cr3t");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"access_token":"tok-form","expires_in":300}"#);
        });
        (
            server,
            test_plugin_with_auth_method(toolkit_auth::oauth2::ClientAuthMethod::Form),
        )
    }

    #[cfg(not(feature = "fips"))]
    async fn authenticate_with(plugin: &OAuth2ClientCredentialsPlugin, token_url: &str) -> http::HeaderMap {
        let ctx = PluginContext {
            tenant_id: uuid::Uuid::nil(),
            ..PluginContext::default()
        };
        let config = serde_json::json!({
            "token_url": token_url,
            "client_id": "client-id",
            "client_secret": "s3cr3t"
        });
        let mut headers = http::HeaderMap::new();
        plugin
            .authenticate(&ctx, &config, &mut headers)
            .await
            .expect("token acquired");
        headers
    }

    #[cfg(not(feature = "fips"))]
    #[tokio::test]
    async fn basic_variant_sends_the_basic_header_not_a_form_body() {
        let (server, plugin) = basic_token_plugin();
        let headers = authenticate_with(&plugin, &server.url("/oauth/token")).await;
        assert_eq!(
            headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer tok-basic",
            "the access token is injected on the proxied request"
        );
        // The mock only matched an `Authorization: Basic …` header, so reaching
        // it at all proves the credentials travelled as a header.
    }

    #[cfg(not(feature = "fips"))]
    #[tokio::test]
    async fn form_variant_sends_the_credentials_in_the_body() {
        let (server, plugin) = form_token_plugin();
        let headers = authenticate_with(&plugin, &server.url("/oauth/token")).await;
        assert_eq!(
            headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer tok-form",
            "the access token is injected on the proxied request"
        );
        // The mock only matched a form body carrying both credentials, so
        // reaching it proves the form spelling was used.
    }

    #[tokio::test]
    async fn config_validation_rejects_missing_fields() {
        let plugin = test_plugin();
        assert!(
            plugin
                .validate_config(&serde_json::json!({}))
                .is_err()
        );
        assert!(
            plugin
                .validate_config(&serde_json::json!({ "token_url": "https://t", "client_id": "c" }))
                .is_err()
        );
        assert!(
            plugin
                .validate_config(&serde_json::json!({
                    "token_url": "https://t", "client_id": "c", "client_secret": "s"
                }))
                .is_ok()
        );
    }
}
