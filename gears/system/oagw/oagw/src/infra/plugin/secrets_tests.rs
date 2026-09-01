//! `SecretResolver` port tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{
    CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef, SecretType, SecretValue,
    SharingMode, TenantId,
};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::infra::plugin::secrets::{
    CredStoreSecretResolver, SecretResolver, StaticSecretResolver,
};

#[tokio::test]
async fn static_resolver_serves_its_entries() {
    let resolver = StaticSecretResolver::single("stripe-key", "sk_live_1");
    let value = resolver
        .resolve(uuid::Uuid::nil(), uuid::Uuid::nil(), "stripe-key")
        .await
        .unwrap();
    assert_eq!(value, "sk_live_1");
}

#[tokio::test]
async fn static_resolver_misses_are_secret_not_found() {
    let resolver = StaticSecretResolver::default();
    let error = resolver
        .resolve(uuid::Uuid::nil(), uuid::Uuid::nil(), "missing")
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::SecretNotFound { .. }));
}

/// A store double whose `get` is the only behaviour the resolver uses.
struct StubStore {
    outcome: Result<Option<Vec<u8>>, CredStoreError>,
    seen: std::sync::Mutex<Option<(uuid::Uuid, uuid::Uuid, String)>>,
}

impl StubStore {
    fn returning(value: Option<Vec<u8>>) -> Self {
        Self {
            outcome: Ok(value),
            seen: std::sync::Mutex::new(None),
        }
    }

    fn failing() -> Self {
        Self {
            outcome: Err(CredStoreError::internal("backend failure")),
            seen: std::sync::Mutex::new(None),
        }
    }
}

fn canned(value: Vec<u8>) -> GetSecretResponse {
    GetSecretResponse {
        value: SecretValue::new(value),
        id: uuid::Uuid::nil(),
        owner_tenant_id: TenantId::nil(),
        sharing: SharingMode::default(),
        is_inherited: false,
        version: 1,
        secret_type: SecretType::generic().gts_id().to_owned(),
        expires_at: None,
    }
}

#[async_trait]
impl CredStoreClientV1 for StubStore {
    async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        *self.seen.lock().unwrap() = Some((
            ctx.subject_tenant_id(),
            ctx.subject_id(),
            key.as_ref().to_owned(),
        ));
        match &self.outcome {
            Ok(value) => Ok(value.clone().map(canned)),
            Err(_) => Err(CredStoreError::internal("backend failure")),
        }
    }
}

#[tokio::test]
async fn credstore_resolver_returns_the_secret_value() {
    let store = Arc::new(StubStore::returning(Some(b"sk_live_1".to_vec())));
    let resolver = CredStoreSecretResolver::new(Arc::clone(&store) as _);
    let tenant = uuid::Uuid::now_v7();
    let subject = uuid::Uuid::now_v7();
    let value = resolver
        .resolve(tenant, subject, "stripe-key")
        .await
        .unwrap();
    assert_eq!(value, "sk_live_1");
    let seen = store.seen.lock().unwrap().clone().unwrap();
    assert_eq!(seen, (tenant, subject, "stripe-key".to_owned()));
}

#[tokio::test]
async fn credstore_resolver_maps_absent_to_secret_not_found() {
    let resolver = CredStoreSecretResolver::new(Arc::new(StubStore::returning(None)) as _);
    let error = resolver
        .resolve(uuid::Uuid::nil(), uuid::Uuid::nil(), "stripe-key")
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::SecretNotFound { .. }));
}

#[tokio::test]
async fn credstore_resolver_maps_store_failure_to_service_unavailable() {
    let resolver = CredStoreSecretResolver::new(Arc::new(StubStore::failing()) as _);
    let error = resolver
        .resolve(uuid::Uuid::nil(), uuid::Uuid::nil(), "stripe-key")
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::ServiceUnavailable { .. }));
}

#[tokio::test]
async fn credstore_resolver_rejects_an_unspellable_reference() {
    let resolver = CredStoreSecretResolver::new(Arc::new(StubStore::returning(None)) as _);
    let error = resolver
        .resolve(uuid::Uuid::nil(), uuid::Uuid::nil(), "not a ref")
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::SecretNotFound { .. }));
}
