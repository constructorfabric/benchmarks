//! Built-in plugin execution for the data plane.
//!
//! Execution order (DESIGN §3.3): Auth → Guards → Transform(request) →
//! upstream call → Transform(response). Only the named plugins are
//! resolvable in-process (`noop`, `apikey`, `oauth2_client_cred*`,
//! `required_headers`, `request_id`); catalog-only identifiers fail at
//! request time with `plugin_not_found` (503).

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue};
use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use toolkit_auth::oauth2::{ClientAuthMethod, SecretString};
use toolkit_security::SecurityContext;
use tracing::warn;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::model::{PluginRef, UpstreamAuth};

/// Plugin instance id suffixes (the part after the `~` in a plugin
/// GTS ref).
pub const NOOP: &str = "cf.core.oagw.noop.v1";
pub const APIKEY: &str = "cf.core.oagw.apikey.v1";
pub const OAUTH2_CC: &str = "cf.core.oagw.oauth2_client_cred.v1";
pub const OAUTH2_CC_BASIC: &str = "cf.core.oagw.oauth2_client_cred_basic.v1";
pub const REQUIRED_HEADERS: &str = "cf.core.oagw.required_headers.v1";
pub const REQUEST_ID: &str = "cf.core.oagw.request_id.v1";

/// Cache entry carrying its own lookup key so a `TinyUfo` hash collision
/// can never leak another tenant's token (ADR 0008).
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// In-process plugin engine shared by the data plane.
pub struct PluginEngine {
    credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
    token_cache: MemoryCache<String, CachedToken>,
    token_cache_ttl: u64,
}

impl PluginEngine {
    /// Create a plugin engine over the given credential store.
    ///
    /// # Panics
    /// Never — token-cache construction is infallible.
    #[must_use]
    pub fn new(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>, config: &OagwConfig) -> Self {
        Self {
            credstore,
            token_cache: MemoryCache::new(config.token_cache_capacity),
            token_cache_ttl: config.token_cache_ttl_secs,
        }
    }

    /// Classify a plugin ref by its trailing `~` segment.
    fn kind_of(ref_name: &str) -> Option<&'static str> {
        let tail = ref_name.rsplit('~').next()?;
        let lower = tail.to_ascii_lowercase();
        [
            NOOP,
            APIKEY,
            OAUTH2_CC,
            OAUTH2_CC_BASIC,
            REQUIRED_HEADERS,
            REQUEST_ID,
        ]
        .into_iter()
        .find(|k| *k == lower)
    }

    // ==================================================================
    // Auth — one plugin per upstream (DESIGN).
    // ==================================================================

    /// Apply the upstream's auth plugin: for each bound auth ref in
    /// order, the first *implemented* plugin wins; `noop` is the
    /// explicit no-op marker.
    pub async fn apply_auth(
        &self,
        ctx: &SecurityContext,
        auth: Option<&UpstreamAuth>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        let Some(auth) = auth else { return Ok(()) };
        let tail = auth.plugin_type.rsplit('~').next().unwrap_or(&auth.plugin_type);
        match tail.to_ascii_lowercase().as_str() {
            NOOP => Ok(()),
            APIKEY => {
                let key = resolve_secret(&auth.config, "api_key_ref", self.credstore.as_ref(), ctx)
                    .await?
                    .ok_or_else(|| OagwError::secret_not_found("apikey: `api_key_ref` points nowhere"))?;
                let header_name = config_str(&auth.config, "header_name")
                    .unwrap_or_else(|| "X-API-Key".to_owned());
                if let Ok(value) = HeaderValue::from_str(&key) {
                    if let Ok(name) = http::header::HeaderName::try_from(header_name.as_str()) {
                        headers.insert(name, value);
                    }
                }
                Ok(())
            }
            OAUTH2_CC | OAUTH2_CC_BASIC => {
                let method = if tail.eq_ignore_ascii_case(OAUTH2_CC_BASIC) {
                    ClientAuthMethod::Basic
                } else {
                    ClientAuthMethod::Form
                };
                let token = self.oauth2_token(ctx, &auth.config, method).await?;
                headers.insert(
                    http::header::AUTHORIZATION,
                    HeaderValue::try_from(format!("Bearer {}", token.expose()))
                        .unwrap_or_else(|_| HeaderValue::from_static("Bearer")),
                );
                Ok(())
            }
            "basic" | "bearer" => Err(OagwError::plugin_not_found(format!(
                "auth plugin `{}` is catalog-only and has no in-process implementation",
                auth.plugin_type
            ))),
            _ => Err(OagwError::plugin_not_found(format!(
                "unknown auth plugin `{}`",
                auth.plugin_type
            ))),
        }
    }

    /// Resolve (and cache) an OAuth2 client-credentials bearer token.
    ///
    /// Cache key bundles tenant, subject, auth method and a hash of the
    /// plugin config so credentials never leak across (tenant, user,
    /// config) boundaries (ADR 0008). TTL is
    /// `min(config_ttl, expires_in − 30s)`.
    async fn oauth2_token(
        &self,
        ctx: &SecurityContext,
        config: &Value,
        auth_method: ClientAuthMethod,
    ) -> Result<SecretString, OagwError> {
        let key = format!(
            "{}:{}:{}:{}",
            ctx.subject_tenant_id(),
            ctx.subject_id(),
            oauth2_method_tag(auth_method),
            hash_config(config),
        );

        if let (Some(cached), _status) = self.token_cache.get(&key) {
            if cached.key == key {
                return Ok(cached.token.clone());
            }
            warn!("token cache key collision detected (defense-in-depth)");
        }

        let client_id = resolve_secret(config, "client_id_ref", self.credstore.as_ref(), ctx)
            .await?
            .ok_or_else(|| OagwError::secret_not_found("oauth2: `client_id_ref` points nowhere"))?;
        let client_secret = resolve_secret(config, "client_secret_ref", self.credstore.as_ref(), ctx)
            .await?
            .ok_or_else(|| OagwError::secret_not_found("oauth2: `client_secret_ref` points nowhere"))?;

        let mut oauth = toolkit_auth::oauth2::OAuthClientConfig::default();
        oauth.client_id = client_id;
        oauth.client_secret = SecretString::new(client_secret);
        oauth.auth_method = auth_method;
        if let Some(endpoint) = config_str(config, "token_endpoint") {
            let Ok(url) = url::Url::parse(&endpoint) else {
                return Err(OagwError::validation("oauth2: invalid `token_endpoint` URL"));
            };
            oauth.token_endpoint = Some(url);
        } else if let Some(issuer) = config_str(config, "issuer_url") {
            let Ok(url) = url::Url::parse(&issuer) else {
                return Err(OagwError::validation("oauth2: invalid `issuer_url` URL"));
            };
            oauth.issuer_url = Some(url);
        } else {
            return Err(OagwError::validation(
                "oauth2: one of `token_endpoint` or `issuer_url` is required",
            ));
        }
        if let Some(scopes) = config_str(config, "scopes") {
            oauth.scopes = scopes.split_whitespace().map(str::to_owned).collect();
        }

        let fetched = toolkit_auth::oauth2::fetch_token(oauth)
            .await
            .map_err(|e| OagwError::auth_failed(format!("OAuth2 token fetch failed: {e}")))?;

        let ttl_secs = fetched
            .expires_in
            .checked_sub(std::time::Duration::from_secs(30))
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .min(self.token_cache_ttl);
        let token = fetched.bearer;
        let entry = CachedToken {
            key: key.clone(),
            token: token.clone(),
        };
        self.token_cache
            .put(&key, entry, Some(std::time::Duration::from_secs(ttl_secs)));
        Ok(token)
    }

    // ==================================================================
    // Guards — validation policy, may reject.
    // ==================================================================

    /// Execute the guard chain (upstream then route) in order. Guards
    /// are `required_headers` (the only bindable one); unknown guard
    /// refs fail with `plugin_not_found`.
    pub fn apply_guards(
        &self,
        upstream_refs: &[PluginRef],
        route_refs: &[PluginRef],
        request_headers: &HeaderMap,
        response_headers: &HeaderMap,
    ) -> Result<(), OagwError> {
        for plugin_ref in upstream_refs.iter().chain(route_refs) {
            // Only guard-kind refs participate in the guard chain;
            // auth/transform refs are dispatched elsewhere.
            if !plugin_ref.is_guard_kind() {
                continue;
            }
            let tail = plugin_ref
                .plugin_ref
                .rsplit('~')
                .next()
                .unwrap_or(&plugin_ref.plugin_ref);
            match tail.to_ascii_lowercase().as_str() {
                "" => continue,
                REQUIRED_HEADERS => {
                    validate_required_headers(plugin_ref, request_headers, response_headers)?
                }
                other => {
                    return Err(OagwError::plugin_not_found(format!(
                        "guard plugin `{other}` is not bindable or has no in-process implementation"
                    )));
                }
            }
        }
        Ok(())
    }

    // ==================================================================
    // Transforms — request/response mutation.
    // ==================================================================

    /// Transform plugin chain on the outbound (gateway → upstream)
    /// request headers. `request_id` injects/propagates `X-Request-ID`.
    pub fn apply_transform_request(
        &self,
        refs: &[PluginRef],
        request_headers: &mut HeaderMap,
    ) -> Result<Option<String>, OagwError> {
        let mut request_id = None;
        for plugin_ref in refs {
            // Only transform-kind refs participate here; guard refs are
            // enforced in `apply_guards`, auth refs in `apply_auth`.
            if !plugin_ref.is_transform_kind() {
                continue;
            }
            let tail = plugin_ref
                .plugin_ref
                .rsplit('~')
                .next()
                .unwrap_or(&plugin_ref.plugin_ref);
            match tail.to_ascii_lowercase().as_str() {
                "" => continue,
                REQUEST_ID => {
                    request_id = ensure_request_id(request_headers);
                }
                other => {
                    return Err(OagwError::plugin_not_found(format!(
                        "transform plugin `{other}` is not bindable or has no in-process implementation"
                    )));
                }
            }
        }
        Ok(request_id)
    }

    /// Apply response-side transforms: echo `X-Request-ID` generated on
    /// the request leg.
    pub fn apply_transform_response(
        &self,
        response_headers: &mut HeaderMap,
        request_id: Option<&str>,
    ) {
        if let Some(id) = request_id {
            if let Ok(value) = HeaderValue::from_str(id) {
                response_headers.insert("x-request-id", value);
            }
        }
    }

    /// Whether a plugin ref belongs to this engine's transform kind —
    /// unused, kept for symmetry with `kind_of`.
    #[allow(dead_code)]
    fn is_supported(&self, _ref_name: &str) -> bool {
        Self::kind_of(_ref_name).is_some()
    }
}

// =====================================================================
//                             Helpers
// =====================================================================

/// Read a string key out of a plugin `config` object.
fn config_str<'a>(config: &'a Value, key: &str) -> Option<String> {
    config.as_object()?.get(key)?.as_str().map(str::to_owned)
}

/// Resolve a `cred://foo`-style secret reference via the credential
/// store. `config[key]` must be a string of the form `cred://<ref>` or
/// a bare ref (`cred://` prefix optional — the store keys do not carry
/// it).
async fn resolve_secret(
    config: &Value,
    key: &str,
    credstore: &dyn credstore_sdk::CredStoreClientV1,
    ctx: &SecurityContext,
) -> Result<Option<String>, OagwError> {
    let Some(raw) = config_str(config, key) else {
        return Ok(None);
    };
    let secret_ref = raw.strip_prefix("cred://").unwrap_or(&raw);
    let secret_ref = credstore_sdk::SecretRef::new(secret_ref)
        .map_err(|e| OagwError::validation(format!("invalid {key} `{raw}`: {e}")))?;
    let secret = credstore
        .get(ctx, &secret_ref)
        .await
        .map_err(|e| OagwError::secret_not_found(format!("{key} lookup failed: {e}")))?;
    Ok(secret.map(|s| String::from_utf8_lossy(s.value.as_bytes()).into_owned()))
}

/// Guard: `required_headers` — comma-separated header lists checked on
/// request and response. Fail-open when the plugin config omits the
/// fields (ADR 0009).
fn validate_required_headers(
    plugin_ref: &PluginRef,
    request_headers: &HeaderMap,
    response_headers: &HeaderMap,
) -> Result<(), OagwError> {
    if let Some(list) = config_str(&plugin_ref.config, "required_request_headers") {
        for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if request_headers.get(name).is_none() {
                return Err(OagwError::validation(format!(
                    "missing required request header `{}`",
                    name.to_ascii_lowercase()
                )));
            }
        }
    }
    if let Some(list) = config_str(&plugin_ref.config, "required_response_headers") {
        for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if response_headers.get(name).is_none() {
                return Err(OagwError::validation(format!(
                    "missing required response header `{}`",
                    name.to_ascii_lowercase()
                )));
            }
        }
    }
    Ok(())
}

/// `request_id` transform: keep the client-supplied `X-Request-ID` when
/// present, otherwise allocate a fresh UUID. Returns the active value.
fn ensure_request_id(headers: &mut HeaderMap) -> Option<String> {
    if let Some(existing) = headers.get("x-request-id").and_then(|v| v.to_str().ok()) {
        return Some(existing.to_owned());
    }
    let id = Uuid::new_v4().to_string();
    if let Ok(value) = HeaderValue::from_str(&id) {
        headers.insert("x-request-id", value);
    }
    Some(id)
}

fn oauth2_method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    }
}

/// Deterministic (sorted) hash of a plugin config object for cache keys.
fn hash_config(config: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(obj) = config.as_object() {
        for (k, v) in obj {
            parts.push(format!("{k}={}", v.to_string()));
        }
    }
    parts.sort();
    // No dedicated hash crate; a stable FNV-1a over the sorted serialization.
    let joined = parts.join(";");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in joined.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}
