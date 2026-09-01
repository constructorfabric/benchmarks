//! Tests for [`crate::infra::plugin::secret`].

use std::collections::HashMap;
use std::sync::Arc;

use credstore_sdk::test_util::MockCredStoreClient;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{
    CredStoreSecretResolver, SECRET_REF_SCHEME, SecretResolver, StaticSecretResolver,
    UnavailableSecretResolver, secret_ref, strip_secret_scheme,
};

const TENANT: Uuid = Uuid::from_u128(0x11);

fn context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x33))
        .subject_tenant_id(TENANT)
        .build()
        .expect("security context")
}

#[tokio::test]
async fn credstore_resolver_returns_the_secret() {
    let resolver =
        CredStoreSecretResolver::new(Arc::new(MockCredStoreClient::with_secrets(vec![(
            "payments-key".to_owned(),
            "s3cret".to_owned(),
        )])));
    let resolved = resolver
        .resolve(&context(), "cred://payments-key")
        .await
        .expect("resolved");
    assert_eq!(resolved.as_deref(), Some("s3cret"));
}

#[tokio::test]
async fn credstore_resolver_maps_a_missing_secret_to_none() {
    let resolver = CredStoreSecretResolver::new(Arc::new(MockCredStoreClient::empty()));
    let resolved = resolver
        .resolve(&context(), "cred://absent-key")
        .await
        .expect("no error");
    assert_eq!(resolved, None);
}

#[tokio::test]
async fn credstore_resolver_maps_a_store_failure_to_secret_not_found() {
    let resolver = CredStoreSecretResolver::new(Arc::new(MockCredStoreClient::always_failing()));
    let error = resolver
        .resolve(&context(), "cred://payments-key")
        .await
        .expect_err("500");
    assert_eq!(
        error.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(error.detail().contains("credential store lookup"));
}

#[tokio::test]
async fn credstore_resolver_rejects_a_malformed_reference() {
    let resolver = CredStoreSecretResolver::new(Arc::new(MockCredStoreClient::empty()));
    let error = resolver
        .resolve(&context(), "cred://not a valid key!")
        .await
        .expect_err("malformed reference");
    assert_eq!(
        error.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn unavailable_resolver_fails_closed() {
    let resolver = UnavailableSecretResolver;
    let error = resolver
        .resolve(&context(), "cred://payments-key")
        .await
        .expect_err("fail closed");
    assert_eq!(
        error.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(error.detail().contains("no credential store"));
}

#[tokio::test]
async fn static_resolver_serves_its_map() {
    let mut secrets = HashMap::new();
    secrets.insert("payments-key".to_owned(), "static-value".to_owned());
    let resolver = StaticSecretResolver::new(secrets);
    let resolved = resolver
        .resolve(&context(), "cred://payments-key")
        .await
        .expect("resolved");
    assert_eq!(resolved.as_deref(), Some("static-value"));
    assert_eq!(
        resolver
            .resolve(&context(), "cred://absent")
            .await
            .expect("none"),
        None
    );
}

#[test]
fn scheme_and_stripping_are_consistent() {
    assert_eq!(SECRET_REF_SCHEME, "cred://");
    assert_eq!(strip_secret_scheme("cred://payments-key"), "payments-key");
    assert_eq!(strip_secret_scheme("cred:payments-key"), "payments-key");
    assert_eq!(strip_secret_scheme("payments-key"), "payments-key");
    assert_eq!(
        strip_secret_scheme("  cred://payments-key  "),
        "payments-key"
    );
}

#[test]
fn secret_ref_builds_a_valid_store_reference() {
    let reference = secret_ref("cred://payments-key").expect("reference");
    assert_eq!(reference.as_ref(), "payments-key");
    assert!(secret_ref("cred://invalid key!").is_err());
}

#[tokio::test]
async fn static_resolver_is_usable_through_the_trait_object() {
    let mut secrets = HashMap::new();
    secrets.insert("k".to_owned(), "v".to_owned());
    let resolver: Arc<dyn SecretResolver> = Arc::new(StaticSecretResolver::new(secrets));
    assert_eq!(
        resolver
            .resolve(&context(), "cred://k")
            .await
            .expect("resolved"),
        Some("v".to_owned())
    );
}
