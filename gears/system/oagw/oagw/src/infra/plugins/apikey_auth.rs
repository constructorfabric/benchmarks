//! `apikey` auth plugin — API key injection into a header or a query
//! parameter (`PRD.md` § 5.3).
//!
//! Configuration keys (`ctx.config`):
//!
//! | Key | Required | Description |
//! |---|---|---|
//! | `key_ref` | one of `key_ref`/`api_key` | `cred://` reference for the key |
//! | `api_key` | one of `key_ref`/`api_key` | Inline key (test setups) |
//! | `header` | No | Header to inject into, default `X-API-Key` |
//! | `in` | No | `header` (default) or `query` |
//! | `query_param` | No | Query parameter name, default `api-key` |

use async_trait::async_trait;
use http::HeaderMap;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::AUTH_APIKEY;
use crate::domain::plugin::{AuthPlugin, PluginResult, RequestContext};

/// Default header carrying the API key.
pub const DEFAULT_API_KEY_HEADER: &str = "x-api-key";

/// Default query parameter carrying the API key.
pub const DEFAULT_API_KEY_QUERY_PARAM: &str = "api-key";

/// Resolves the configured key material from `ctx.config`.
fn configured_key(config: &serde_json::Value) -> Option<KeySource> {
    let obj = config.as_object()?;
    for field in ["key_ref", "secret_ref", "api_key_ref"] {
        if let Some(value) = obj.get(field).and_then(serde_json::Value::as_str)
            && !value.trim().is_empty()
        {
            return Some(KeySource::Ref(value.trim().to_owned()));
        }
    }
    if let Some(value) = obj.get("api_key").and_then(serde_json::Value::as_str)
        && !value.trim().is_empty()
    {
        return Some(KeySource::Inline(value.trim().to_owned()));
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum KeySource {
    /// `cred://` reference resolved through the credential store.
    Ref(String),
    /// Inline key material.
    Inline(String),
}

/// Where the key is written on the outbound request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Injection {
    Header,
    Query,
}

fn injection_point(config: &serde_json::Value) -> (Injection, String) {
    let mode = config
        .get("in")
        .and_then(serde_json::Value::as_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if mode == "query" {
        let name = config
            .get("query_param")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_API_KEY_QUERY_PARAM);
        (Injection::Query, name.to_owned())
    } else {
        let name = config
            .get("header")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::trim)
            .unwrap_or(DEFAULT_API_KEY_HEADER)
            .to_ascii_lowercase();
        (Injection::Header, name)
    }
}

/// Strips a `cred://` scheme prefix, mirroring the credential store's own
/// test-double.
fn strip_scheme(reference: &str) -> &str {
    reference.strip_prefix("cred://").unwrap_or(reference)
}

/// Injects an API key into the outbound request.
#[derive(Debug, Clone)]
pub struct ApiKeyAuthPlugin;

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        AUTH_APIKEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let Some(source) = configured_key(&ctx.config) else {
            return Err(DomainError::AuthenticationFailed);
        };
        let (point, name) = injection_point(&ctx.config);

        let key = match source {
            KeySource::Inline(key) => key,
            KeySource::Ref(reference) => {
                let Some(credstore) = ctx.runtime.credstore.clone() else {
                    return Err(DomainError::SecretNotFound);
                };
                let secret_ref =
                    credstore_sdk::SecretRef::new(strip_scheme(&reference)).map_err(|_| {
                        DomainError::Validation(format!("invalid secret_ref '{reference}'"))
                    })?;
                match credstore.get(&ctx.runtime.security, &secret_ref).await {
                    Ok(Some(response)) => String::from_utf8_lossy(response.value.as_bytes())
                        .trim()
                        .to_owned(),
                    Ok(None) => return Err(DomainError::SecretNotFound),
                    Err(_) => return Err(DomainError::SecretNotFound),
                }
            }
        };
        if key.is_empty() {
            return Err(DomainError::AuthenticationFailed);
        }

        match point {
            Injection::Header => {
                let Ok(header_name) = http::HeaderName::try_from(name.as_str()) else {
                    return Err(DomainError::Validation(format!(
                        "invalid header name '{name}'"
                    )));
                };
                if let Ok(value) = http::HeaderValue::from_str(&key) {
                    ctx.headers.insert(header_name, value);
                } else {
                    return Err(DomainError::AuthenticationFailed);
                }
            }
            Injection::Query => {
                let mut pairs: Vec<(String, String)> = form_urlencoded::parse(ctx.query.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .filter(|(k, _)| k != &name)
                    .collect();
                pairs.push((name.clone(), key.clone()));
                ctx.query = form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    .finish();
            }
        }
        Ok(())
    }
}

/// `true` when `name` is present in `headers`.
#[must_use]
pub fn has_header(headers: &HeaderMap, name: &str) -> bool {
    http::HeaderName::try_from(name)
        .map(|name| headers.contains_key(name))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_the_documented_gts_id() {
        assert_eq!(ApiKeyAuthPlugin.id(), AUTH_APIKEY);
    }

    #[test]
    fn the_injection_point_defaults_to_the_api_key_header() {
        let (point, name) = injection_point(&serde_json::json!({}));
        assert_eq!(point, Injection::Header);
        assert_eq!(name, "x-api-key");
    }

    #[test]
    fn the_injection_point_follows_the_config() {
        let (point, name) = injection_point(&serde_json::json!({"in": "query"}));
        assert_eq!(point, Injection::Query);
        assert_eq!(name, "api-key");

        let (point, name) =
            injection_point(&serde_json::json!({"in": "header", "header": "X-Key"}));
        assert_eq!(point, Injection::Header);
        assert_eq!(name, "x-key");
    }

    #[test]
    fn the_key_source_prefers_a_reference() {
        assert_eq!(
            configured_key(&serde_json::json!({"key_ref": "cred://k", "api_key": "inline"})),
            Some(KeySource::Ref("cred://k".to_owned()))
        );
        assert_eq!(
            configured_key(&serde_json::json!({"api_key": "inline"})),
            Some(KeySource::Inline("inline".to_owned()))
        );
        assert_eq!(configured_key(&serde_json::json!({})), None);
    }

    #[test]
    fn the_scheme_prefix_is_stripped() {
        assert_eq!(strip_scheme("cred://partner-key"), "partner-key");
        assert_eq!(strip_scheme("partner-key"), "partner-key");
    }
}
