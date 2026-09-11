//! Auth plugin invocation and credential injection
//! (`cpt-cf-oagw-algo-plugin-auth-invoke`).
//!
//! Dispatches by resolved implementation kind: `noop` touches nothing;
//! `apikey` injects a `cred_store`-resolved value into a configured header
//! or query parameter; both client-credentials variants inject `Authorization:
//! Bearer <token>` obtained through [`super::oauth2::acquire_token`].
//!
//! RF-001: reached for real from `crate::proxy::engine` (via
//! `super::execute`'s chain executor) on every request whose merged
//! `AuthConfig` names a backed auth plugin.

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use serde_json::Value;
use toolkit_auth::oauth2::ClientAuthMethod;
use toolkit_security::SecurityContext;

use credstore_sdk::CredStoreClientV1;

use super::binding::ResolvedAuth;
use super::credential::{CredentialError, resolve_secret};
use super::oauth2::{TokenAcquireError, acquire_token};
use super::registry::AuthKind;
use super::token_cache::TokenCache;

/// Everything [`invoke_auth`] needs beyond the binding itself: the
/// injected `cred_store` client, the shared token cache, and the two
/// gear-level settings the token cache's TTL ceiling and the outbound
/// token-exchange timeout are read from.
pub(crate) struct AuthRuntime<'a> {
    pub credstore: &'a dyn CredStoreClientV1,
    pub token_cache: &'a TokenCache,
    pub token_cache_ttl_secs: u64,
    pub proxy_timeout_secs: u32,
}

/// An auth-plugin invocation failure -- every variant maps to `401
/// AuthenticationFailed` (`inst-auth-invoke-09`/`-10`).
#[derive(Debug)]
pub(crate) enum AuthError {
    /// The `apikey` plugin's `config` names neither a `header` nor a
    /// `query` placement.
    MissingPlacement,
    // The inner `CredentialError` is never read (only matched on by kind,
    // both here in `safe_detail` and by this module's own tests): it
    // exists so `Debug`/`matches!` can distinguish this variant, not for
    // its data.
    #[allow(dead_code)]
    Credential(CredentialError),
    Token(TokenAcquireError),
}

impl AuthError {
    /// A message safe to place in an RFC 9457 `detail`
    /// (`cpt-cf-oagw-dod-plugin-secret-nondisclosure`): references at most
    /// the credential reference *key*, the failure kind, or a fixed
    /// string -- never the resolved secret or token.
    pub(crate) fn safe_detail(&self) -> String {
        match self {
            Self::MissingPlacement => {
                "apikey plugin config must set a `header` or `query` placement".to_owned()
            }
            Self::Credential(_) => "credential reference could not be resolved".to_owned(),
            Self::Token(err) => err.safe_detail(),
        }
    }
}

async fn invoke_apikey(
    config: &Value,
    ctx: &SecurityContext,
    credstore: &dyn CredStoreClientV1,
    headers: &mut HeaderMap,
    query: &mut Vec<(String, String)>,
) -> Result<(), AuthError> {
    let secret_ref = config
        .get("secret_ref")
        .and_then(Value::as_str)
        .unwrap_or("");
    let secret = resolve_secret(secret_ref, ctx, credstore)
        .await
        .map_err(AuthError::Credential)?;

    if let Some(header_name) = config.get("header").and_then(Value::as_str) {
        let name = HeaderName::from_bytes(header_name.as_bytes())
            .map_err(|_| AuthError::MissingPlacement)?;
        let value =
            HeaderValue::from_str(secret.expose()).map_err(|_| AuthError::MissingPlacement)?;
        // Replace any inbound value at this position -- a caller can
        // neither pre-seed nor observe the credential slot
        // (`inst-auth-invoke-05`).
        headers.insert(name, value);
        return Ok(());
    }
    if let Some(query_name) = config.get("query").and_then(Value::as_str) {
        query.retain(|(name, _)| name != query_name);
        query.push((query_name.to_owned(), secret.expose().to_owned()));
        return Ok(());
    }
    Err(AuthError::MissingPlacement)
}

/// Invoke the resolved auth plugin exactly once
/// (`inst-auth-invoke-01` through `-11`).
// @cpt-algo:cpt-cf-oagw-algo-plugin-auth-invoke:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-cred-injection:p1
// @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-01
// @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-02
// @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-03
// @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-04
// @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-05
// @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-06
// @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-07
// @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-08
pub(crate) async fn invoke_auth(
    auth: &ResolvedAuth,
    ctx: &SecurityContext,
    runtime: &AuthRuntime<'_>,
    headers: &mut HeaderMap,
    query: &mut Vec<(String, String)>,
) -> Result<(), AuthError> {
    match auth.kind {
        AuthKind::Noop => Ok(()),
        AuthKind::ApiKey => {
            invoke_apikey(&auth.config, ctx, runtime.credstore, headers, query).await
        }
        AuthKind::OAuth2ClientCred | AuthKind::OAuth2ClientCredBasic => {
            let method = if matches!(auth.kind, AuthKind::OAuth2ClientCredBasic) {
                ClientAuthMethod::Basic
            } else {
                ClientAuthMethod::Form
            };
            let token = acquire_token(
                &auth.config,
                method,
                ctx,
                runtime.credstore,
                runtime.token_cache,
                runtime.token_cache_ttl_secs,
                runtime.proxy_timeout_secs,
            )
            .await
            .map_err(AuthError::Token)?;
            let value = format!("Bearer {}", token.expose());
            let header_value =
                HeaderValue::from_str(&value).map_err(|_| AuthError::MissingPlacement)?;
            headers.insert(header::AUTHORIZATION, header_value);
            // @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-11
            Ok(())
            // @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-11
        }
    }
}
// @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-08
// @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-07
// @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-06
// @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-05
// @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-04
// @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-03
// @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-02
// @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-01

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use serde_json::json;
    use uuid::Uuid;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap()
    }

    fn runtime<'a>(credstore: &'a dyn CredStoreClientV1, cache: &'a TokenCache) -> AuthRuntime<'a> {
        AuthRuntime {
            credstore,
            token_cache: cache,
            token_cache_ttl_secs: 300,
            proxy_timeout_secs: 5,
        }
    }

    #[tokio::test]
    async fn noop_touches_neither_headers_nor_cred_store() {
        let store = MockCredStoreClient::always_failing();
        let cache = TokenCache::new(10);
        let auth = ResolvedAuth {
            kind: AuthKind::Noop,
            config: Value::Null,
        };
        let mut headers = HeaderMap::new();
        let mut query = Vec::new();
        invoke_auth(
            &auth,
            &ctx(),
            &runtime(&store, &cache),
            &mut headers,
            &mut query,
        )
        .await
        .unwrap();
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn apikey_injects_into_the_configured_header_replacing_any_inbound_value() {
        let store = MockCredStoreClient::with_secrets(vec![(
            "partner-key".to_owned(),
            "sk-123".to_owned(),
        )]);
        let cache = TokenCache::new(10);
        let auth = ResolvedAuth {
            kind: AuthKind::ApiKey,
            config: json!({"secret_ref": "cred://partner-key", "header": "x-api-key"}),
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "caller-supplied".parse().unwrap());
        let mut query = Vec::new();
        invoke_auth(
            &auth,
            &ctx(),
            &runtime(&store, &cache),
            &mut headers,
            &mut query,
        )
        .await
        .unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-123");
    }

    #[tokio::test]
    async fn apikey_injects_into_the_configured_query_parameter() {
        let store = MockCredStoreClient::with_secrets(vec![(
            "partner-key".to_owned(),
            "sk-123".to_owned(),
        )]);
        let cache = TokenCache::new(10);
        let auth = ResolvedAuth {
            kind: AuthKind::ApiKey,
            config: json!({"secret_ref": "cred://partner-key", "query": "api_key"}),
        };
        let mut headers = HeaderMap::new();
        let mut query = vec![("api_key".to_owned(), "caller-supplied".to_owned())];
        invoke_auth(
            &auth,
            &ctx(),
            &runtime(&store, &cache),
            &mut headers,
            &mut query,
        )
        .await
        .unwrap();
        assert_eq!(query, vec![("api_key".to_owned(), "sk-123".to_owned())]);
    }

    #[tokio::test]
    async fn apikey_with_an_inaccessible_secret_fails() {
        let store = MockCredStoreClient::empty();
        let cache = TokenCache::new(10);
        let auth = ResolvedAuth {
            kind: AuthKind::ApiKey,
            config: json!({"secret_ref": "cred://absent", "header": "x-api-key"}),
        };
        let mut headers = HeaderMap::new();
        let mut query = Vec::new();
        let err = invoke_auth(
            &auth,
            &ctx(),
            &runtime(&store, &cache),
            &mut headers,
            &mut query,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AuthError::Credential(_)));
    }

    #[tokio::test]
    async fn apikey_without_a_placement_key_is_a_config_error() {
        let store = MockCredStoreClient::with_secrets(vec![("k".to_owned(), "v".to_owned())]);
        let cache = TokenCache::new(10);
        let auth = ResolvedAuth {
            kind: AuthKind::ApiKey,
            config: json!({"secret_ref": "cred://k"}),
        };
        let mut headers = HeaderMap::new();
        let mut query = Vec::new();
        let err = invoke_auth(
            &auth,
            &ctx(),
            &runtime(&store, &cache),
            &mut headers,
            &mut query,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AuthError::MissingPlacement));
    }

    #[test]
    fn safe_detail_never_echoes_the_secret_reference_or_value() {
        let err = AuthError::Credential(CredentialError::NotAccessible);
        assert!(!err.safe_detail().contains("partner-key"));
    }
}
