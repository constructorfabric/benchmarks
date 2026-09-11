//! The api-key auth plugin (`cpt-cf-oagw-algo-apikey-injection`,
//! `cpt-cf-oagw-dod-static-auth-plugins`).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use credstore_sdk::CredStoreClientV1;
use toolkit_security::SecurityContext;

use super::binding::{config_str, parse_cred_ref, resolve_secret};
use crate::error::OagwError;

/// Injects the credential resolved from `config`'s `credential_ref` into
/// either the configured request header or the configured query parameter,
/// replacing any inbound value of that name
/// (`cpt-cf-oagw-algo-apikey-injection`).
///
/// `config`'s keys: `placement` (`"header"` or `"query"`, defaulting to
/// `"header"`), `name` (the target header or parameter name), and
/// `credential_ref` (a `cred://` reference — required).
///
/// # Errors
///
/// Returns [`OagwError::plugin_not_found`] (`503`) when `name` is absent or
/// not a valid header name, `credential_ref` is absent or carries an inline
/// value instead of a `cred://` reference, or `placement` names anything
/// other than `header`/`query` — every case is a structurally invalid
/// binding (`cpt-cf-oagw-dod-credential-isolation`). Returns
/// [`OagwError::secret_not_found`] or [`OagwError::authentication_failed`]
/// per [`resolve_secret`]'s documented mapping.
///
/// # Returns
///
/// `Ok(Some(header_name))` for the header placement, naming the header the
/// caller must force through any passthrough filter regardless of the
/// upstream's configured mode (`cpt-cf-oagw-algo-header-transformation`
/// forwards nothing by default); `Ok(None)` for the query placement, which
/// is merged directly into the outbound URL rather than filtered as a
/// header.
// @cpt-begin:cpt-cf-oagw-algo-apikey-injection:p2:inst-apikey-injection-fn-01
pub async fn inject_credential(
    config: Option<&serde_json::Value>,
    credstore: &dyn CredStoreClientV1,
    security_context: &SecurityContext,
    proxy_timeout: Duration,
    headers: &mut HeaderMap,
    query: &mut BTreeMap<String, String>,
) -> Result<Option<HeaderName>, OagwError> {
    let placement = config_str(config, "placement").unwrap_or("header");
    let name = config_str(config, "name")
        .ok_or_else(|| OagwError::plugin_not_found("apikey binding is missing a target 'name'"))?;
    let secret_ref =
        parse_cred_ref(config.and_then(|c| c.get("credential_ref"))).ok_or_else(|| {
            OagwError::plugin_not_found(
                "apikey binding requires a 'cred://' credential_ref, not an inline value",
            )
        })?;

    let resolved = resolve_secret(credstore, security_context, &secret_ref, proxy_timeout).await?;

    match placement {
        "header" => {
            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                OagwError::plugin_not_found(format!("'{name}' is not a valid header name"))
            })?;
            let header_value = HeaderValue::from_str(resolved.expose()).map_err(|_| {
                OagwError::authentication_failed("resolved credential is not a valid header value")
            })?;
            headers.insert(header_name.clone(), header_value);
            Ok(Some(header_name))
        }
        "query" => {
            query.insert(name.to_owned(), resolved.expose().to_owned());
            Ok(None)
        }
        other => Err(OagwError::plugin_not_found(format!(
            "apikey binding names unknown placement '{other}'"
        ))),
    }
}
// @cpt-end:cpt-cf-oagw-algo-apikey-injection:p2:inst-apikey-injection-fn-01

#[cfg(test)]
mod tests {
    use super::inject_credential;
    use axum::http::HeaderMap;
    use credstore_sdk::test_util::MockCredStoreClient;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use toolkit_security::SecurityContext;

    fn ctx() -> SecurityContext {
        SecurityContext::anonymous()
    }

    // @cpt-begin:cpt-cf-oagw-dod-static-auth-plugins:p2:inst-apikey-header-test-01
    #[tokio::test]
    async fn a_header_placement_binding_injects_the_resolved_value() {
        let credstore = MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-resolved".to_owned(),
        )]);
        let config = json!({"placement": "header", "name": "X-Api-Key", "credential_ref": "cred://openai-key"});
        let mut headers = HeaderMap::new();
        let mut query = BTreeMap::new();

        inject_credential(
            Some(&config),
            &credstore,
            &ctx(),
            Duration::from_secs(1),
            &mut headers,
            &mut query,
        )
        .await
        .expect("must inject");

        assert_eq!(
            headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-resolved")
        );
        assert!(query.is_empty());
    }
    // @cpt-end:cpt-cf-oagw-dod-static-auth-plugins:p2:inst-apikey-header-test-01

    #[tokio::test]
    async fn a_query_placement_binding_sets_the_query_parameter_only() {
        let credstore = MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-resolved".to_owned(),
        )]);
        let config =
            json!({"placement": "query", "name": "api_key", "credential_ref": "cred://openai-key"});
        let mut headers = HeaderMap::new();
        let mut query = BTreeMap::new();

        inject_credential(
            Some(&config),
            &credstore,
            &ctx(),
            Duration::from_secs(1),
            &mut headers,
            &mut query,
        )
        .await
        .expect("must inject");

        assert_eq!(
            query.get("api_key").map(String::as_str),
            Some("sk-resolved")
        );
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn header_placement_replaces_a_caller_supplied_value() {
        let credstore = MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-resolved".to_owned(),
        )]);
        let config = json!({"placement": "header", "name": "X-Api-Key", "credential_ref": "cred://openai-key"});
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "caller-supplied".parse().unwrap());
        let mut query = BTreeMap::new();

        inject_credential(
            Some(&config),
            &credstore,
            &ctx(),
            Duration::from_secs(1),
            &mut headers,
            &mut query,
        )
        .await
        .expect("must inject");

        assert_eq!(
            headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-resolved")
        );
    }

    // @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p2:inst-apikey-inline-reject-test-01
    #[tokio::test]
    async fn an_inline_credential_value_is_rejected_with_no_credstore_call() {
        let credstore = MockCredStoreClient::always_failing();
        let config =
            json!({"placement": "header", "name": "X-Api-Key", "credential_ref": "sk-inline"});
        let mut headers = HeaderMap::new();
        let mut query = BTreeMap::new();

        let error = inject_credential(
            Some(&config),
            &credstore,
            &ctx(),
            Duration::from_secs(1),
            &mut headers,
            &mut query,
        )
        .await
        .expect_err("inline secret must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
    // @cpt-end:cpt-cf-oagw-dod-credential-isolation:p2:inst-apikey-inline-reject-test-01

    #[tokio::test]
    async fn a_missing_secret_maps_to_500() {
        let credstore = MockCredStoreClient::empty();
        let config = json!({"name": "X-Api-Key", "credential_ref": "cred://missing"});
        let mut headers = HeaderMap::new();
        let mut query = BTreeMap::new();

        let error = inject_credential(
            Some(&config),
            &credstore,
            &ctx(),
            Duration::from_secs(1),
            &mut headers,
            &mut query,
        )
        .await
        .expect_err("missing secret must 500");
        assert_eq!(
            error.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
