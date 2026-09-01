//! `apikey.v1` — API key injection (header or query) from `cred_store`
//! (PRD `cpt-cf-oagw-fr-auth-injection`).

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use http::HeaderName;
use http::header::HeaderValue;

use crate::domain::plugin::{
    API_KEY_AUTH_PLUGIN_ID, AuthContext, AuthPlugin, PluginError, cfg_string, secret_ref_name,
};

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`
///
/// Resolves the API key from `cred_store` at request time and injects it as a
/// request header (`in: header`, the default) or query parameter
/// (`in: query`).
///
/// Config keys:
///
/// | Key | Required | Description |
/// |---|---|---|
/// | `value_ref` | yes | `cred://` reference to the API key secret |
/// | `name` | yes | Header name (or query parameter name) |
/// | `in` | no | `"header"` (default) or `"query"` |
pub struct ApiKeyAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl ApiKeyAuthPlugin {
    /// Create the plugin bound to a `cred_store` client.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

/// Where the injected credential is placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Injection {
    Header,
    Query,
}

#[derive(Debug)]
struct Config {
    value_ref: String,
    name: String,
    injection: Injection,
}

fn parse_config(config: &serde_json::Value) -> Result<Config, PluginError> {
    let value_ref = cfg_string(config, "value_ref").ok_or_else(|| {
        PluginError::AuthenticationFailed(
            "apikey plugin requires a 'value_ref' (cred://...) in auth.config".to_owned(),
        )
    })?;
    let name = cfg_string(config, "name").ok_or_else(|| {
        PluginError::AuthenticationFailed(
            "apikey plugin requires a 'name' (header or query param) in auth.config".to_owned(),
        )
    })?;
    let injection = match cfg_string(config, "in").unwrap_or("header") {
        "header" => Injection::Header,
        "query" => Injection::Query,
        other => {
            return Err(PluginError::AuthenticationFailed(format!(
                "apikey plugin: unsupported injection location '{other}' (expected 'header' or 'query')"
            )));
        }
    };
    Ok(Config {
        value_ref: value_ref.to_owned(),
        name: name.to_owned(),
        injection,
    })
}

#[async_trait::async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        API_KEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut AuthContext<'_>) -> Result<(), PluginError> {
        let cfg = parse_config(ctx.config)?;

        let reference = SecretRef::new(secret_ref_name(&cfg.value_ref)).map_err(|e| {
            PluginError::AuthenticationFailed(format!(
                "apikey plugin: invalid secret reference '{}': {e}",
                cfg.value_ref
            ))
        })?;
        let Some(secret) = self
            .credstore
            .get(ctx.security_context, &reference)
            .await
            .map_err(|e| internal_from_credstore(&e))?
        else {
            return Err(PluginError::SecretNotFound(format!(
                "apikey plugin: secret '{}' not found or not accessible",
                cfg.value_ref
            )));
        };
        let value = std::str::from_utf8(secret.value.as_bytes()).map_err(|_| {
            PluginError::AuthenticationFailed(format!(
                "apikey plugin: secret '{}' is not valid UTF-8",
                cfg.value_ref
            ))
        })?;
        if value.trim().is_empty() {
            return Err(PluginError::AuthenticationFailed(format!(
                "apikey plugin: secret '{}' is empty",
                cfg.value_ref
            )));
        }

        match cfg.injection {
            Injection::Header => {
                let name = HeaderName::from_bytes(cfg.name.as_bytes()).map_err(|e| {
                    PluginError::AuthenticationFailed(format!(
                        "apikey plugin: header name '{}' is invalid: {e}",
                        cfg.name
                    ))
                })?;
                let value = HeaderValue::from_str(value).map_err(|e| {
                    PluginError::AuthenticationFailed(format!(
                        "apikey plugin: secret '{}' is not a valid header value: {e}",
                        cfg.value_ref
                    ))
                })?;
                ctx.headers.insert(name, value);
            }
            Injection::Query => {
                ctx.query_params.push((cfg.name.clone(), value.to_owned()));
            }
        }
        Ok(())
    }
}

/// Map a `cred_store` backend failure to [`PluginError::Internal`].
fn internal_from_credstore(e: &credstore_sdk::CredStoreError) -> PluginError {
    PluginError::Internal(format!("apikey plugin: cred_store error: {e}"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_header_and_query_configs() {
        let header = parse_config(&json!({"value_ref": "cred://k", "name": "x-api-key"}))
            .expect("header config");
        assert_eq!(header.injection, Injection::Header);
        let query =
            parse_config(&json!({"value_ref": "cred://k", "name": "api_key", "in": "query"}))
                .expect("query config");
        assert_eq!(query.injection, Injection::Query);
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(parse_config(&json!({"name": "x-api-key"})).is_err()); // no value_ref
        assert!(parse_config(&json!({"value_ref": "cred://k"})).is_err()); // no name
        let bad = parse_config(&json!({"value_ref": "cred://k", "name": "x", "in": "cookie"}));
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn missing_secret_reports_secret_not_found() {
        let store = Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
        let plugin = ApiKeyAuthPlugin::new(store);
        let ctx = toolkit_security::SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::from_u128(1))
            .build()
            .unwrap();
        let mut headers = http::HeaderMap::new();
        let mut query = Vec::new();
        let mut actx = AuthContext {
            security_context: &ctx,
            config: &json!({"value_ref": "cred://ghost", "name": "x-api-key"}),
            headers: &mut headers,
            query_params: &mut query,
        };
        let err = plugin.authenticate(&mut actx).await.unwrap_err();
        assert!(matches!(err, PluginError::SecretNotFound(_)));
    }
}
