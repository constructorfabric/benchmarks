//! Fake `AuthN` resolver: exchanges the client credentials for a fixed service identity.

use async_trait::async_trait;
use authn_resolver_sdk::{
    AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Answers every client-credentials exchange with a service `SecurityContext` and rejects bearer
/// tokens (the harness injects the user context itself).
pub struct FakeAuthn;

#[async_trait]
impl AuthNResolverClient for FakeAuthn {
    async fn authenticate(
        &self,
        _bearer_token: &str,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        Err(AuthNResolverError::Unauthorized(
            "the harness injects the security context".to_owned(),
        ))
    }

    async fn exchange_client_credentials(
        &self,
        _request: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        let security_context = SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .subject_type("service")
            .token_scopes(vec!["*".to_owned()])
            .build()
            .map_err(|err| AuthNResolverError::Internal(err.to_string()))?;
        Ok(AuthenticationResult { security_context })
    }
}
