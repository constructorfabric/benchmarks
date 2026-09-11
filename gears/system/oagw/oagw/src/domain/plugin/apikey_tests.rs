//! Unit tests for the API-key auth plugin.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::domain::error::OagwError;
use crate::domain::plugin::AuthPlugin;
use crate::domain::plugin::test_support::{
    StaticResolver, context, denied_resolver, empty_resolver, tenant,
};
use crate::gts_helpers;
use std::collections::BTreeMap;
use uuid::Uuid;

fn config(entries: &[(&str, &str)]) -> AuthConfig {
    AuthConfig {
        plugin_type: gts_helpers::AUTH_APIKEY.to_owned(),
        sharing: crate::domain::model::SharingMode::default(),
        config: entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), serde_json::Value::from(*value)))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[tokio::test]
async fn injects_the_key_into_a_header() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let resolver = StaticResolver::new(&[("openai-key", "sk-secret-value")]);
    let configuration = config(&[
        ("header_name", "x-api-key"),
        ("credential_ref", "cred://openai-key"),
    ]);

    let decision = plugin
        .authenticate(&mut context, &configuration, &resolver)
        .await;

    assert_eq!(decision.unwrap(), AuthDecision::Injected);
    assert_eq!(context.headers.get("x-api-key").unwrap(), "sk-secret-value");
    let credential = context.credential.expect("credential is carried");
    assert_eq!(credential.as_text().unwrap(), "sk-secret-value");
    assert_eq!(context.query, "");
}

#[tokio::test]
async fn accepts_the_documented_secret_ref_spelling() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let resolver = StaticResolver::new(&[("openai-key", "sk-secret-value")]);
    let configuration = config(&[
        ("header_name", "authorization"),
        ("secret_ref", "cred://openai-key"),
    ]);

    let decision = plugin
        .authenticate(&mut context, &configuration, &resolver)
        .await;

    assert_eq!(decision.unwrap(), AuthDecision::Injected);
    assert_eq!(
        context.headers.get("authorization").unwrap(),
        "sk-secret-value"
    );
}

#[tokio::test]
async fn injects_the_key_into_a_query_parameter() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    context.query = "lang=en".to_owned();
    let resolver = StaticResolver::new(&[("openai-key", "sk-secret-value")]);
    let configuration = config(&[
        ("query_param", "api_key"),
        ("credential_ref", "cred://openai-key"),
    ]);

    let decision = plugin
        .authenticate(&mut context, &configuration, &resolver)
        .await;

    assert_eq!(decision.unwrap(), AuthDecision::Injected);
    assert_eq!(context.query, "lang=en&api_key=sk-secret-value");
    assert!(context.headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn a_header_wins_over_the_query() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let resolver = StaticResolver::new(&[("openai-key", "sk-secret-value")]);
    let configuration = config(&[
        ("header_name", "x-api-key"),
        ("query_param", "api_key"),
        ("credential_ref", "cred://openai-key"),
    ]);

    plugin
        .authenticate(&mut context, &configuration, &resolver)
        .await
        .unwrap();

    assert_eq!(context.headers.get("x-api-key").unwrap(), "sk-secret-value");
    assert_eq!(context.query, "");
}

#[tokio::test]
async fn a_tenant_scoped_reference_resolves_to_its_final_segment() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let resolver = StaticResolver::new(&[("inner-key", "sk-inner")]);
    let configuration = config(&[
        ("header_name", "x-api-key"),
        ("credential_ref", "cred://acme/inner-key"),
    ]);

    plugin
        .authenticate(&mut context, &configuration, &resolver)
        .await
        .unwrap();

    assert_eq!(context.headers.get("x-api-key").unwrap(), "sk-inner");
}

#[tokio::test]
async fn a_missing_reference_is_a_validation_error() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");

    let decision = plugin
        .authenticate(
            &mut context,
            &config(&[("header_name", "x-api-key")]),
            &empty_resolver(),
        )
        .await;

    let error = decision.unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(
        error.type_id(),
        crate::gts_helpers::error_type_id("validation.error")
    );
}

#[tokio::test]
async fn neither_a_header_nor_a_query_parameter_is_a_validation_error() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let resolver = StaticResolver::new(&[("openai-key", "sk-secret-value")]);

    let decision = plugin
        .authenticate(
            &mut context,
            &config(&[("credential_ref", "cred://openai-key")]),
            &resolver,
        )
        .await;

    assert_eq!(decision.unwrap_err().status(), 400);
}

#[tokio::test]
async fn an_unknown_secret_is_a_500() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let configuration = config(&[
        ("header_name", "x-api-key"),
        ("credential_ref", "cred://no-such-key"),
    ]);

    let decision = plugin
        .authenticate(&mut context, &configuration, &StaticResolver::new(&[]))
        .await;

    let error = decision.unwrap_err();
    assert_eq!(error.status(), 500);
    assert!(!error.detail().contains("sk-"), "no secret value may leak");
}

#[tokio::test]
async fn an_inaccessible_secret_is_a_401() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let configuration = config(&[
        ("header_name", "x-api-key"),
        ("credential_ref", "cred://openai-key"),
    ]);

    let decision = plugin
        .authenticate(&mut context, &configuration, &denied_resolver())
        .await;

    assert_eq!(decision.unwrap_err().status(), 401);
}

#[tokio::test]
async fn an_invalid_header_name_is_rejected() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let resolver = StaticResolver::new(&[("openai-key", "sk-secret-value")]);
    let configuration = config(&[
        ("header_name", "x api key"),
        ("credential_ref", "cred://openai-key"),
    ]);

    let decision = plugin
        .authenticate(&mut context, &configuration, &resolver)
        .await;

    assert_eq!(decision.unwrap_err().status(), 400);
}

#[tokio::test]
async fn a_non_utf8_credential_is_an_authentication_failure() {
    let plugin = ApiKeyAuthPlugin;
    let mut context = context("GET", "/v1/things");
    let resolver = NonUtf8Resolver;
    let configuration = config(&[
        ("header_name", "x-api-key"),
        ("credential_ref", "cred://openai-key"),
    ]);

    let decision = plugin
        .authenticate(&mut context, &configuration, &resolver)
        .await;

    assert_eq!(decision.unwrap_err().status(), 401);
    assert!(context.headers.get("x-api-key").is_none());
}

#[test]
fn the_identifier_is_the_built_in_apikey() {
    assert_eq!(ApiKeyAuthPlugin.id(), gts_helpers::AUTH_APIKEY);
    assert!(!crate::domain::plugin::registry::is_catalogue_only(
        ApiKeyAuthPlugin.id()
    ));
}

/// Resolves every reference to a non-UTF-8 byte string.
#[derive(Debug, Default)]
struct NonUtf8Resolver;

#[async_trait::async_trait]
impl crate::domain::plugin::CredentialResolver for NonUtf8Resolver {
    async fn resolve(
        &self,
        _tenant: Uuid,
        _reference: &str,
    ) -> Result<crate::domain::plugin::Credential, OagwError> {
        Ok(crate::domain::plugin::Credential::new(vec![
            0xff, 0xfe, 0x00,
        ]))
    }
}

#[test]
fn the_tenant_helper_agrees_with_the_context() {
    assert_eq!(context("GET", "/").tenant_id, tenant());
}
