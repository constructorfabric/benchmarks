//! Scripted `AuthNResolverClient`: [`FakeAuthn`] answers `exchange_client_credentials` from a queue
//! of results, then with the fixed [`s2s_security_context`].

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use authn_resolver_sdk::{
    AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest,
};
use secrecy::ExposeSecret;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Subject of the fixed S2S context (the static authn plugin's default identity).
pub const S2S_SUBJECT: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
/// Tenant of the fixed S2S context.
pub const S2S_TENANT: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);

/// The S2S context tests run with.
pub fn s2s_security_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(S2S_SUBJECT)
        .subject_tenant_id(S2S_TENANT)
        .build()
        .expect("s2s context")
}

/// See the module docs.
#[derive(Default)]
pub struct FakeAuthn {
    script: Mutex<VecDeque<Result<(), AuthNResolverError>>>,
    exchanges: Mutex<Vec<(String, String)>>,
    calls: AtomicUsize,
}

impl FakeAuthn {
    pub fn new() -> Self {
        Self::default()
    }

    /// One result per exchange call, in order (`Ok(())` yields the fixed context); afterwards every
    /// call succeeds.
    pub fn script(&self, results: Vec<Result<(), AuthNResolverError>>) -> &Self {
        self.script.lock().expect("lock").extend(results);
        self
    }

    /// Number of `exchange_client_credentials` calls.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// `(client_id, client_secret)` of every call.
    pub fn exchanges(&self) -> Vec<(String, String)> {
        self.exchanges.lock().expect("lock").clone()
    }
}

#[async_trait]
impl AuthNResolverClient for FakeAuthn {
    async fn authenticate(
        &self,
        _bearer_token: &str,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        Err(AuthNResolverError::Internal("not scripted".to_owned()))
    }

    async fn exchange_client_credentials(
        &self,
        request: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.exchanges.lock().expect("lock").push((
            request.client_id.clone(),
            request.client_secret.expose_secret().to_owned(),
        ));
        match self.script.lock().expect("lock").pop_front() {
            Some(Err(e)) => Err(e),
            _ => Ok(AuthenticationResult {
                security_context: s2s_security_context(),
            }),
        }
    }
}
