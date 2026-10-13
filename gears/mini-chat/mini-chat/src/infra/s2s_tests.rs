use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use authn_resolver_sdk::{
    AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::*;
use crate::config::ClientCredentials;

struct FakeAuthn {
    calls: AtomicUsize,
    fail: bool,
}

#[async_trait]
impl AuthNResolverClient for FakeAuthn {
    async fn authenticate(&self, _token: &str) -> Result<AuthenticationResult, AuthNResolverError> {
        Err(AuthNResolverError::Unauthorized("unused".into()))
    }

    async fn exchange_client_credentials(
        &self,
        request: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(AuthNResolverError::ServiceUnavailable("not ready".into()));
        }
        assert_eq!(request.client_id, "mini-chat");
        let ctx = SecurityContext::builder()
            .subject_id(Uuid::from_u128(7))
            .subject_tenant_id(Uuid::from_u128(8))
            .build()
            .unwrap();
        Ok(AuthenticationResult {
            security_context: ctx,
        })
    }
}

fn creds() -> ClientCredentials {
    serde_json::from_value(serde_json::json!({"client_id": "mini-chat", "client_secret": "s"}))
        .unwrap()
}

#[tokio::test]
async fn fixed_context_is_returned() {
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .build()
        .unwrap();
    let p = S2sContextProvider::fixed(ctx);
    assert_eq!(p.get().await.unwrap().subject_id(), Uuid::from_u128(1));
}

#[tokio::test]
async fn context_is_exchanged_lazily_once_and_cached() {
    let authn = Arc::new(FakeAuthn {
        calls: AtomicUsize::new(0),
        fail: false,
    });
    let p = S2sContextProvider::new(authn.clone(), creds());
    assert_eq!(authn.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        p.get().await.unwrap().subject_tenant_id(),
        Uuid::from_u128(8)
    );
    assert_eq!(p.get().await.unwrap().subject_id(), Uuid::from_u128(7));
    assert_eq!(authn.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_exchange_is_not_cached() {
    let authn = Arc::new(FakeAuthn {
        calls: AtomicUsize::new(0),
        fail: true,
    });
    let p = S2sContextProvider::new(authn.clone(), creds());
    assert!(matches!(
        p.exchange().await,
        Err(AuthNResolverError::ServiceUnavailable(_))
    ));
    assert!(p.get().await.is_err());
    assert_eq!(authn.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn rejected_credentials_are_fatal_unavailability_is_not() {
    assert!(exchange_error_is_fatal(&AuthNResolverError::Unauthorized(
        "x".into()
    )));
    assert!(exchange_error_is_fatal(
        &AuthNResolverError::TokenAcquisitionFailed("invalid client credentials".into())
    ));
    assert!(!exchange_error_is_fatal(
        &AuthNResolverError::NoPluginAvailable
    ));
    assert!(!exchange_error_is_fatal(
        &AuthNResolverError::ServiceUnavailable("x".into())
    ));
    assert!(!exchange_error_is_fatal(&AuthNResolverError::Internal(
        "x".into()
    )));
}
