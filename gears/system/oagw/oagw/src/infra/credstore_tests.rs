//! Unit tests for `cred://` resolution over the credstore client.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{CredStoreResolver, secret_key};
use crate::domain::plugin::{Credential, CredentialResolver};
use credstore_sdk::test_util::MockCredStoreClient;
use std::sync::Arc;
use uuid::Uuid;

fn tenant() -> Uuid {
    Uuid::nil()
}

fn resolver(client: MockCredStoreClient) -> CredStoreResolver {
    CredStoreResolver::new(Some(Arc::new(client)))
}

#[test]
fn the_key_is_the_final_segment_of_the_reference() {
    assert_eq!(secret_key("cred://openai-key").unwrap(), "openai-key");
    assert_eq!(secret_key("cred://acme/inner-key").unwrap(), "inner-key");
    assert_eq!(secret_key("cred://a/b/c").unwrap(), "c");
}

#[test]
fn a_reference_outside_the_cred_scheme_is_not_a_key() {
    assert!(secret_key("openai-key").is_none());
    assert!(secret_key("cred://").is_none());
    assert!(secret_key("").is_none());
}

#[tokio::test]
async fn resolves_a_reference_to_its_secret_value() {
    let store = resolver(MockCredStoreClient::with_secrets(vec![(
        "openai-key".to_owned(),
        "sk-secret-value".to_owned(),
    )]));

    let credential = store
        .resolve(tenant(), "cred://openai-key")
        .await
        .expect("the secret resolves");

    assert_eq!(credential.as_text().unwrap(), "sk-secret-value");
}

#[tokio::test]
async fn a_tenant_scoped_reference_resolves_its_final_segment() {
    let store = resolver(MockCredStoreClient::with_secrets(vec![(
        "inner-key".to_owned(),
        "sk-inner".to_owned(),
    )]));

    let credential = store
        .resolve(tenant(), "cred://acme/inner-key")
        .await
        .expect("the secret resolves");

    assert_eq!(credential.as_text().unwrap(), "sk-inner");
}

#[tokio::test]
async fn an_unknown_reference_is_a_500() {
    let store = resolver(MockCredStoreClient::empty());

    let error = store
        .resolve(tenant(), "cred://no-such-key")
        .await
        .unwrap_err();

    assert_eq!(error.status(), 500);
    assert_eq!(
        error.type_id(),
        crate::gts_helpers::error_type_id("secret.not_found")
    );
    assert!(!error.detail().contains("sk-"), "no value may leak");
}

#[tokio::test]
async fn a_store_failure_is_a_401_naming_only_the_key() {
    let store = resolver(MockCredStoreClient::always_failing());

    let error = store
        .resolve(tenant(), "cred://openai-key")
        .await
        .unwrap_err();

    assert_eq!(error.status(), 401);
    assert!(error.detail().contains("openai-key"));
    assert!(!error.detail().contains("sk-"));
}

#[tokio::test]
async fn a_missing_client_fails_closed_with_a_401() {
    let store = CredStoreResolver::new(None);

    let error = store
        .resolve(tenant(), "cred://openai-key")
        .await
        .unwrap_err();

    assert_eq!(error.status(), 401);
    assert!(error.detail().contains("no credential store is configured"));
}

#[tokio::test]
async fn a_malformed_reference_is_a_validation_error() {
    let store = resolver(MockCredStoreClient::empty());

    assert_eq!(
        store
            .resolve(tenant(), "openai-key")
            .await
            .unwrap_err()
            .status(),
        400
    );
    assert_eq!(
        store
            .resolve(tenant(), "cred://")
            .await
            .unwrap_err()
            .status(),
        400
    );
}

#[tokio::test]
async fn non_utf8_material_is_carried_as_bytes() {
    let store = resolver(MockCredStoreClient::returning_raw_value(vec![
        0xff, 0xfe, 0x00,
    ]));

    let credential = store
        .resolve(tenant(), "cred://binary")
        .await
        .expect("the secret resolves");

    assert_eq!(credential.expose(), &[0xff_u8, 0xfe, 0x00]);
    assert!(credential.as_text().is_none());
}

#[test]
fn the_credential_redacts_itself() {
    let credential = Credential::new(b"sk-secret-value".to_vec());

    assert_eq!(format!("{credential:?}"), "Credential(***)");
}
