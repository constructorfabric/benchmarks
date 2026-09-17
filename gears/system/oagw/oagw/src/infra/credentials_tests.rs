use std::sync::Arc;

use credstore_sdk::test_util::MockCredStoreClient;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{SECRET_SCHEME, SecretResolver};
use crate::domain::error::ErrorKind;

/// Deterministic tenant used by every request in this module.
static TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00ca);

fn context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(TENANT)
        .build()
        .unwrap_or_else(|error| panic!("security context: {error}"))
}

/// The failure of a resolution that must not succeed.
fn failure(
    result: Result<String, crate::domain::error::DomainError>,
) -> crate::domain::error::DomainError {
    match result {
        Ok(value) => panic!("the secret must not resolve: {value}"),
        Err(error) => error,
    }
}

fn resolver() -> SecretResolver {
    SecretResolver::new(Arc::new(MockCredStoreClient::with_secrets(vec![
        ("payment-api-key".to_owned(), "sk_live_42".to_owned()),
        ("tenant-password".to_owned(), "s3cret".to_owned()),
    ])))
}

#[test]
fn the_wire_scheme_is_cred() {
    assert_eq!(SECRET_SCHEME, "cred://");
}

#[tokio::test]
async fn bare_reference_resolves() {
    let value = resolver()
        .resolve(&context(), "payment-api-key")
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    assert_eq!(value, "sk_live_42");
}

#[tokio::test]
async fn prefixed_reference_resolves_to_the_same_secret() {
    let bare = resolver().resolve(&context(), "payment-api-key").await;
    let prefixed = resolver()
        .resolve(&context(), "cred://payment-api-key")
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    assert_eq!(bare.unwrap_or_default(), prefixed);
}

#[tokio::test]
async fn hyphenated_references_are_accepted() {
    let value = resolver()
        .resolve(&context(), "cred://tenant-password")
        .await
        .unwrap_or_else(|error| panic!("resolve: {error}"));
    assert_eq!(value, "s3cret");
}

#[tokio::test]
async fn unknown_reference_is_a_secret_not_found() {
    let error = failure(resolver().resolve(&context(), "cred://nope").await);
    assert_eq!(error.kind, ErrorKind::SecretNotFound);
    assert_eq!(error.kind.http_status(), 500);
}

#[tokio::test]
async fn client_failures_are_reported_as_not_found() {
    let failing = SecretResolver::new(Arc::new(MockCredStoreClient::always_failing()));
    let error = failure(failing.resolve(&context(), "payment-api-key").await);
    assert_eq!(error.kind, ErrorKind::SecretNotFound);
}

#[tokio::test]
async fn not_found_errors_from_the_store_close_the_same_surface() {
    let hard = SecretResolver::new(Arc::new(MockCredStoreClient::erroring_not_found()));
    let error = failure(hard.resolve(&context(), "payment-api-key").await);
    assert_eq!(error.kind, ErrorKind::SecretNotFound);
}

#[tokio::test]
async fn malformed_references_never_reach_the_store() {
    let error = failure(resolver().resolve(&context(), "cred://not a ref!").await);
    assert_eq!(error.kind, ErrorKind::SecretNotFound);
}
