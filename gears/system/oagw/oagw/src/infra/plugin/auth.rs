//! Built-in auth plugins — `noop` and `apikey` (DoD
//! `cpt-cf-oagw-dod-plugin-system-builtins`, algorithm
//! `cpt-cf-oagw-algo-plugin-system-resolve-secret`, flow
//! `cpt-cf-oagw-flow-plugin-system-proxy-auth`).
//!
//! Both implement the domain [`AuthPlugin`] trait and are registered under
//! their canonical GTS identifiers (see [`crate::domain::plugin::ids`]).
//!
//! `apikey` resolves its value through a `cred://` reference via the CredStore
//! SDK (steps `inst-ps-secret-parse` .. `inst-ps-secret-return`): the material
//! is held in a zeroizing type while being injected and is never logged,
//! metered, or serialized (DoD `cpt-cf-oagw-dod-plugin-system-cred-isolation`,
//! principle `cpt-cf-oagw-principle-cred-isolation`).

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::DomainError;
use crate::domain::plugin::ids::INSTANCE_PREFIX;
use crate::domain::plugin::{AuthPlugin, RequestContext};

/// Derives the short plugin name (`noop`, `apikey`, ...) from a canonical GTS
/// identifier (the instance segment sans prefix/version).  Falls back to the
/// full identifier for malformed references so the `id()` accessor never
/// panics on registry entries outside the GTS shape.
#[must_use]
fn short_name(gts_id: &str) -> &str {
    gts_id
        .rsplit('~')
        .next()
        .and_then(|instance| instance.strip_prefix(INSTANCE_PREFIX))
        .and_then(|name| name.strip_suffix(".v1"))
        .unwrap_or(gts_id)
}

/// `noop` — performs no credential injection (flow
/// `cpt-cf-oagw-flow-plugin-system-proxy-auth`, step `inst-ps-auth-execute`).
#[derive(Debug, Clone)]
pub struct NoopAuthPlugin {
    /// The canonical GTS identifier registered with the router (e.g. `NOOP_AUTH`).
    pub gts_id: &'static str,
}

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        short_name(self.gts_id)
    }

    fn plugin_type(&self) -> &str {
        self.gts_id
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), DomainError> {
        Ok(())
    }
}

/// `apikey` — injects a static API key as a header or query parameter (DoD
/// `cpt-cf-oagw-dod-plugin-system-builtins`, step `inst-ps-auth-execute`).
///
/// The plugin configuration (`ctx.config`) keys:
///
/// | Key | Required | Description |
/// |-----|----------|-------------|
/// | `value_ref` | Yes (to inject) | `cred://` reference for the key value |
/// | `header_name` | No | Header to inject the key into (default `X-API-Key`); mutually exclusive with `query_param` |
/// | `query_param` | No | Query parameter to append the key to; mutually exclusive with `header_name` |
///
/// An absent or null config (or one with no `value_ref` / no injection target)
/// is a no-op (fail-open), mirroring the built-in contract of never changing
/// behavior for an unconfigured upstream.  A `cred://` reference that cannot
/// be resolved (malformed, absent, or access denied) yields the 500
/// `secret.not_found` gateway error (steps `inst-ps-secret-notfound`,
/// `inst-ps-secret-zeroize`).
pub struct ApiKeyAuthPlugin {
    /// The canonical GTS identifier (`APIKEY_AUTH`).
    pub gts_id: &'static str,
    /// CredStore client resolving the `cred://` value reference.
    pub credstore: Arc<dyn CredStoreClientV1>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The CredStore client may capture secret material — exclude it from
        // `Debug` output.
        f.debug_struct("ApiKeyAuthPlugin")
            .field("gts_id", &self.gts_id)
            .finish_non_exhaustive()
    }
}

impl ApiKeyAuthPlugin {
    /// Creates the plugin bound to the given CredStore client.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>, gts_id: &'static str) -> Self {
        Self { gts_id, credstore }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        short_name(self.gts_id)
    }

    fn plugin_type(&self) -> &str {
        self.gts_id
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let config = &ctx.config;
        if config.is_null() || !config.is_object() {
            return Ok(());
        }

        // Fail-open when no value reference is configured.
        let value_ref = match config.get("value_ref").and_then(serde_json::Value::as_str) {
            Some(v) if !v.trim().is_empty() => v,
            _ => return Ok(()),
        };

        // Injection target: `query_param` takes precedence when configured;
        // otherwise `header_name`, defaulting to `X-API-Key`.
        let query_param = config
            .get("query_param")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let header_name = config
            .get("header_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let value =
            resolve_secret(self.credstore.as_ref(), ctx.security.as_ref(), value_ref).await?;

        match query_param {
            Some(name) => {
                ctx.query.push((name.to_owned(), value));
            }
            None => {
                ctx.headers
                    .insert(header_name.unwrap_or("X-API-Key"), value);
            }
        }
        Ok(())
    }
}

/// Resolves a `cred://` reference through the CredStore SDK (algorithm
/// `cpt-cf-oagw-algo-plugin-system-resolve-secret`, steps
/// `inst-ps-secret-parse` .. `inst-ps-secret-return`).
///
/// The reference must spell the `cred://<key>` URI form; the `cred://` prefix
/// is stripped before `credstore_sdk::SecretRef` validation (the SDK rejects a
/// scheme prefix).  A missing/denied/undecodable secret returns the 500
/// `secret.not_found` gateway error.
///
/// # Errors
/// - [`DomainError::SecretNotFound`] — the reference is malformed, resolves to
///   nothing, is access-denied, or its value is not valid UTF-8.
async fn resolve_secret(
    credstore: &dyn CredStoreClientV1,
    security: Option<&SecurityContext>,
    value_ref: &str,
) -> Result<String, DomainError> {
    let bare = value_ref
        .strip_prefix("cred://")
        .map(str::trim)
        .unwrap_or(value_ref.trim());
    let key = SecretRef::new(bare).map_err(|e| DomainError::SecretNotFound {
        detail: format!("invalid cred:// reference '{value_ref}': {e}"),
    })?;

    let ctx = security.cloned().unwrap_or_else(SecurityContext::anonymous);
    let response = credstore
        .get(&ctx, &key)
        .await
        .map_err(|e| DomainError::SecretNotFound {
            detail: format!("credstore lookup failed for '{value_ref}': {e}"),
        })?;
    let secret = response.ok_or_else(|| DomainError::SecretNotFound {
        detail: format!("secret '{value_ref}' not found or access denied"),
    })?;

    // The secret bytes are zeroized on drop; we copy into a plain String that
    // lives only for the duration of the request header/query injection (the
    // acknowledged short-lived plaintext from ADR 0008).
    String::from_utf8(secret.value.as_bytes().to_vec()).map_err(|_| DomainError::SecretNotFound {
        detail: format!("secret '{value_ref}' is not valid UTF-8"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::Headers;
    use crate::domain::plugin::ids::{APIKEY_AUTH, NOOP_AUTH};

    fn credstore_with(creds: Vec<(String, String)>) -> Arc<dyn CredStoreClientV1> {
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
            creds,
        ))
    }

    fn ctx(config: serde_json::Value) -> RequestContext {
        RequestContext {
            method: "POST".to_owned(),
            path: "/v1/models".to_owned(),
            query: Vec::new(),
            headers: Headers::new(),
            config,
            security: None,
        }
    }

    #[tokio::test]
    async fn noop_injects_nothing() {
        let plugin = NoopAuthPlugin { gts_id: NOOP_AUTH };
        let mut c = ctx(serde_json::json!({ "anything": true }));
        plugin.authenticate(&mut c).await.expect("noop never fails");
        assert!(c.headers.is_empty());
        assert!(c.query.is_empty());
        assert_eq!(plugin.id(), "noop");
        assert_eq!(plugin.plugin_type(), NOOP_AUTH);
    }

    #[tokio::test]
    async fn apikey_fail_open_on_absent_or_blank_config() {
        let plugin = ApiKeyAuthPlugin::new(credstore_with(vec![]), APIKEY_AUTH);
        for config in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({ "header_name": "X-API-Key" }),
            serde_json::json!({ "value_ref": "" }),
        ] {
            let mut c = ctx(config);
            plugin
                .authenticate(&mut c)
                .await
                .expect("unconfigured apikey is a no-op");
            assert!(c.headers.is_empty(), "no injection for {c:?}");
            assert!(c.query.is_empty());
        }
    }

    #[tokio::test]
    async fn apikey_injects_header_from_cred_reference() {
        let plugin = ApiKeyAuthPlugin::new(
            credstore_with(vec![("partner-key".to_owned(), "sk-secret".to_owned())]),
            APIKEY_AUTH,
        );
        let mut c = ctx(serde_json::json!({
            "header_name": "X-API-Key",
            "value_ref": "cred://partner-key",
        }));
        plugin.authenticate(&mut c).await.expect("resolves");
        assert_eq!(c.headers.get("x-api-key"), Some("sk-secret"));

        // Default header name when only value_ref is supplied... still needs a
        // target; `X-API-Key` is the documented default.
        let mut c = ctx(serde_json::json!({ "value_ref": "cred://partner-key" }));
        plugin.authenticate(&mut c).await.expect("resolves");
        assert_eq!(c.headers.get("x-api-key"), Some("sk-secret"));
    }

    #[tokio::test]
    async fn apikey_injects_query_param_from_cred_reference() {
        let plugin = ApiKeyAuthPlugin::new(
            credstore_with(vec![("partner-key".to_owned(), "sk-secret".to_owned())]),
            APIKEY_AUTH,
        );
        let mut c = ctx(serde_json::json!({
            "query_param": "api_key",
            "value_ref": "cred://partner-key",
        }));
        plugin.authenticate(&mut c).await.expect("resolves");
        assert_eq!(
            c.query,
            vec![("api_key".to_owned(), "sk-secret".to_owned())]
        );
        assert!(c.headers.is_empty());
    }

    #[tokio::test]
    async fn apikey_missing_secret_yields_secret_not_found_500() {
        let plugin = ApiKeyAuthPlugin::new(credstore_with(vec![]), APIKEY_AUTH);
        let mut c = ctx(serde_json::json!({
            "header_name": "X-API-Key",
            "value_ref": "cred://nope",
        }));
        let err = plugin
            .authenticate(&mut c)
            .await
            .expect_err("missing secret must fail");
        assert_eq!(err.status(), 500);
        assert_eq!(
            err.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
    }

    #[tokio::test]
    async fn apikey_creds_never_appear_in_plugin_or_error_debug() {
        let plugin = ApiKeyAuthPlugin::new(
            credstore_with(vec![("k1".to_owned(), "sekrit-123".to_owned())]),
            APIKEY_AUTH,
        );
        let mut c = ctx(serde_json::json!({
            "header_name": "X-API-Key",
            "value_ref": "cred://k1",
        }));
        plugin.authenticate(&mut c).await.expect("resolves");
        // The plugin stores no credential material, so its Debug is clear.
        let plugin_dbg = format!("{plugin:?}");
        assert!(
            !plugin_dbg.contains("sekrit-123"),
            "plugin Debug leaks the secret"
        );

        // Gateway errors surfaced for other requests must not echo secrets.
        let mut missing = ctx(serde_json::json!({
            "header_name": "X-API-Key",
            "value_ref": "cred://missing",
        }));
        let err = plugin
            .authenticate(&mut missing)
            .await
            .expect_err("missing");
        let err_dbg = format!("{err:?}");
        assert!(
            !err_dbg.contains("sekrit-123"),
            "error Debug leaks a secret"
        );
    }
}
