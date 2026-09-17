//! OAuth2 client-credentials credential injection (ADR-0008).
//!
//! The data plane resolves `client_id_ref` / `client_secret_ref` from the
//! credential store, exchanges them for a bearer token with
//! `toolkit_auth::oauth2::fetch_token`, and injects `Authorization: Bearer
//! <token>` into the outbound request. Tokens are cached per
//! `(tenant, subject, auth_method, config_hash)` in an in-memory cache with
//! TTL `min(config_ttl, expires_in − 30s)`; failed fetches are not cached.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use axum::body::Body;
use http::StatusCode;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use toolkit_security::SecurityContext;
use url::Url;

use crate::gts;
use crate::model::AuthConfig;
use crate::state::OagwState;

/// Cached token entry. Carries the exact lookup key so a (2^-45 probability)
/// `TinyUfo` hash collision can never leak another tenant's token: a hit is
/// only honored when `key == lookup_key` (ADR-0008 defense-in-depth).
#[derive(Clone)]
pub struct CachedToken {
    pub(crate) key: String,
    pub(crate) token: SecretString,
}

/// Obtain a bearer token for `auth` (client-credentials flow), using the
/// per-tenant/per-subject cache where possible.
pub(crate) async fn token_for(
    state: &OagwState,
    sec: &SecurityContext,
    auth: &AuthConfig,
    is_basic: bool,
    suffix: &str,
) -> Result<String, http::Response<Body>> {
    let auth_tag = if is_basic { "basic" } else { "form" };
    let lookup_key = format!(
        "{}:{}:{}:{}",
        sec.subject_tenant_id(),
        sec.subject_id(),
        auth_tag,
        hash_config(&auth.config)
    );

    // Cache probe with collision verification.
    let (cached, _status) = state.token_cache.get(&lookup_key);
    if let Some(entry) = cached {
        if entry.key == lookup_key {
            return Ok(entry.token.expose().to_string());
        }
    }

    // Cache miss (or key mismatch): resolve credentials + endpoints.
    let client_id = resolve_oauth_secret(state, sec, auth, "client_id_ref", "client_id", suffix)
        .await?;
    let client_secret = resolve_oauth_secret(
        state,
        sec,
        auth,
        "client_secret_ref",
        "client_secret",
        suffix,
    )
    .await?;

    let mut config = OAuthClientConfig {
        client_id,
        client_secret: SecretString::new(client_secret),
        auth_method: if is_basic {
            ClientAuthMethod::Basic
        } else {
            ClientAuthMethod::Form
        },
        scopes: auth
            .config
            .get("scopes")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        ..Default::default()
    };

    if let Some(ep) = auth.config.get("token_endpoint").and_then(|v| v.as_str()) {
        match Url::parse(ep) {
            Ok(u) => config.token_endpoint = Some(u),
            Err(_) => {
                return Err(oauth_config_problem(
                    "invalid token_endpoint in oauth2 auth config",
                    suffix,
                ));
            }
        }
    } else if let Some(issuer) = auth.config.get("issuer_url").and_then(|v| v.as_str()) {
        match Url::parse(issuer) {
            Ok(u) => config.issuer_url = Some(u),
            Err(_) => {
                return Err(oauth_config_problem(
                    "invalid issuer_url in oauth2 auth config",
                    suffix,
                ));
            }
        }
    } else {
        return Err(oauth_config_problem(
            "oauth2 auth config requires token_endpoint or issuer_url",
            suffix,
        ));
    }

    // Token endpoint HTTP policy follows the upstream HTTP policy so
    // allow-http deployments (and tests) can use local token endpoints.
    if state.config.http_allowed() {
        config.http_config = Some(toolkit_http::HttpClientConfig::for_testing());
    }

    let fetched = match fetch_token(config).await {
        Ok(t) => t,
        Err(e) => {
            return Err(super::gateway_problem(
                StatusCode::UNAUTHORIZED,
                gts::ERR_AUTH_FAILED,
                "Auth Failed",
                format!("oauth2 token exchange failed: {e}"),
                suffix,
                None,
            ));
        }
    };

    // TTL: `min(config_ttl, expires_in − 30s safety margin)` (ADR-0008).
    let expires = fetched.expires_in.saturating_sub(Duration::from_secs(30));
    let ttl = std::cmp::min(state.config.token_cache_ttl(), expires.max(Duration::ZERO));
    let token_plain = fetched.bearer.expose().to_string();

    state.token_cache.put(
        &lookup_key,
        CachedToken {
            key: lookup_key.clone(),
            token: fetched.bearer,
        },
        Some(ttl),
    );

    Ok(token_plain)
}

/// Deterministic, order-stable hash of a plugin config object.
fn hash_config(config: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    // serde_json::Map is key-sorted (BTreeMap-backed), so serialization is
    // deterministic across processes.
    serde_json::to_string(config)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

async fn resolve_oauth_secret(
    state: &OagwState,
    sec: &SecurityContext,
    auth: &AuthConfig,
    ref_key: &str,
    inline_key: &str,
    suffix: &str,
) -> Result<String, http::Response<Body>> {
    // `cred://` reference wins; inline value is the fallback (test
    // convenience / static config).
    if let Some(r) = auth.config.get(ref_key).and_then(|v| v.as_str()) {
        match super::resolve_secret(state, sec, Some(r), suffix).await {
            Ok(Some(v)) => return Ok(v),
            _ => {
                return Err(super::gateway_problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    gts::ERR_SECRET_NOT_FOUND,
                    "Secret Not Found",
                    format!("could not resolve {ref_key} = {r:?}"),
                    suffix,
                    None,
                ));
            }
        }
    }
    match auth.config.get(inline_key).and_then(|v| v.as_str()) {
        Some(v) if !v.is_empty() => Ok(v.to_string()),
        _ => Err(super::gateway_problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            gts::ERR_SECRET_NOT_FOUND,
            "Secret Not Found",
            format!("oauth2 auth config requires {ref_key}"),
            suffix,
            None,
        )),
    }
}

fn oauth_config_problem(detail: &str, suffix: &str) -> http::Response<Body> {
    super::gateway_problem(
        StatusCode::BAD_GATEWAY,
        gts::ERR_PROTOCOL_ERROR,
        "Protocol Error",
        detail.to_string(),
        suffix,
        None,
    )
}
