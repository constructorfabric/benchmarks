//! `cred://` reference handling.

use super::*;
use credstore_sdk::test_util::MockCredStoreClient;

fn client(secrets: Vec<(String, String)>) -> Arc<dyn CredStoreClientV1> {
    Arc::new(MockCredStoreClient::with_secrets(secrets))
}

#[test]
fn the_scheme_is_optional_and_stripped() {
    assert_eq!(strip_scheme("cred://openai-key"), "openai-key");
    assert_eq!(strip_scheme("credstore://openai-key"), "openai-key");
    assert_eq!(strip_scheme("  openai-key "), "openai-key");
}

#[tokio::test]
async fn a_known_reference_resolves_to_its_value() {
    let client = client(vec![("openai-key".to_owned(), "sk-test".to_owned())]);
    let ctx = SecurityContext::anonymous();
    let secret = resolve_secret(&client, &ctx, "cred://openai-key").await.unwrap();
    assert_eq!(secret.expose(), "sk-test");
}

#[tokio::test]
async fn an_unknown_reference_is_a_secret_not_found() {
    let client = client(Vec::new());
    let ctx = SecurityContext::anonymous();
    let err = resolve_secret(&client, &ctx, "cred://absent").await.unwrap_err();
    assert!(matches!(err, PluginError::SecretNotFound(_)), "{err}");
    // The reference may appear in the message; the value never can.
    assert!(err.to_string().contains("absent"));
}

#[tokio::test]
async fn a_malformed_reference_is_a_configuration_error() {
    let client = client(Vec::new());
    let ctx = SecurityContext::anonymous();
    let err = resolve_secret(&client, &ctx, "cred://not a key!").await.unwrap_err();
    assert!(matches!(err, PluginError::InvalidConfig(_)), "{err}");
}

#[tokio::test]
async fn a_non_utf8_secret_cannot_be_injected_as_a_header() {
    let client: Arc<dyn CredStoreClientV1> =
        Arc::new(MockCredStoreClient::returning_raw_value(vec![0xff, 0xfe]));
    let ctx = SecurityContext::anonymous();
    let err = resolve_secret(&client, &ctx, "cred://binary").await.unwrap_err();
    assert!(matches!(err, PluginError::InvalidConfig(_)), "{err}");
}

#[tokio::test]
async fn a_refusing_credential_store_surfaces_as_an_auth_failure() {
    let client: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::always_failing());
    let ctx = SecurityContext::anonymous();
    let err = resolve_secret(&client, &ctx, "cred://openai-key").await.unwrap_err();
    assert!(matches!(err, PluginError::Unauthenticated(_)), "{err}");
}
