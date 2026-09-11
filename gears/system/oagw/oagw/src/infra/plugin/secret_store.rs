//! The credential-store-backed [`SecretResolver`].
//!
//! A configuration references material as `cred://<key>`; the prefix is
//! stripped before the key is handed to the credential store, so the store only
//! ever sees the key it validates against.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::plugin::{PluginContext, SecretResolver};

/// The prefix that marks a reference as credential-store-backed.
pub const CREDENTIAL_SCHEME: &str = "cred://";

/// Strips [`CREDENTIAL_SCHEME`] from a reference, if it carries one.
#[must_use]
pub fn strip_scheme(reference: &str) -> &str {
    reference.strip_prefix(CREDENTIAL_SCHEME).unwrap_or(reference)
}

/// Resolves `cred://` references through a client hub.
///
/// Gears that register the credential-store client initialize after this gear
/// does, so the client is looked up at resolve time rather than captured at
/// init. The lookup is a read-locked map probe, not a network call.
pub struct HubSecretResolver {
    hub: Arc<toolkit::client_hub::ClientHub>,
}

impl HubSecretResolver {
    /// Builds the resolver over a client hub.
    #[must_use]
    pub fn new(hub: Arc<toolkit::client_hub::ClientHub>) -> Self {
        Self { hub }
    }

    /// The credential-store client, when one is registered.
    fn store(&self) -> Option<Arc<dyn CredStoreClientV1>> {
        self.hub.try_get::<dyn CredStoreClientV1>()
    }
}

#[async_trait]
impl SecretResolver for HubSecretResolver {
    async fn resolve(
        &self,
        context: &PluginContext,
        reference: &str,
    ) -> Result<Option<String>, DomainError> {
        let Some(store) = self.store() else {
            return Ok(None);
        };
        CredStoreSecretResolver::new(store).resolve(context, reference).await
    }
}

/// Resolves `cred://` references through the credential store.
pub struct CredStoreSecretResolver {
    store: Arc<dyn CredStoreClientV1>,
}

impl CredStoreSecretResolver {
    /// Builds the resolver over a credential-store client.
    #[must_use]
    pub fn new(store: Arc<dyn CredStoreClientV1>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl SecretResolver for CredStoreSecretResolver {
    async fn resolve(
        &self,
        context: &PluginContext,
        reference: &str,
    ) -> Result<Option<String>, DomainError> {
        let Ok(key) = SecretRef::new(strip_scheme(reference)) else {
            return Err(DomainError::AuthenticationFailed(format!(
                "credential reference `{reference}` is not a valid secret key"
            )));
        };
        let caller = SecurityContext::builder()
            .subject_id(context.subject_id)
            .subject_tenant_id(context.tenant_id)
            .build()
            .map_err(|error| {
                DomainError::AuthenticationFailed(format!(
                    "cannot build a credential-store context: {error}"
                ))
            })?;
        match self.store.get(&caller, &key).await {
            Ok(Some(response)) => Ok(Some(
                String::from_utf8_lossy(response.value.as_bytes()).into_owned(),
            )),
            // A missing secret and a denied lookup both read as "no secret
            // configured", so they share one arm.
            Ok(None) | Err(CredStoreError::AccessDenied | CredStoreError::NotFound) => Ok(None),
            Err(error) => Err(DomainError::DownstreamError(format!(
                "credential store unavailable: {error}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;

    fn context() -> PluginContext {
        PluginContext {
            tenant_id: uuid::Uuid::nil(),
            subject_id: uuid::Uuid::nil(),
            upstream_id: uuid::Uuid::nil(),
            route_id: None,
            alias: "alias".to_owned(),
            bearer_token: None,
            request_id: None,
        }
    }

    async fn resolve(store: Arc<dyn CredStoreClientV1>, reference: &str) -> Result<Option<String>, DomainError> {
        CredStoreSecretResolver::new(store)
            .resolve(&context(), reference)
            .await
    }

    #[tokio::test]
    async fn a_cred_reference_resolves_to_the_stored_value() {
        let store: Arc<dyn CredStoreClientV1> =
            Arc::new(MockCredStoreClient::with_secrets(vec![(
                "openai-key".to_owned(),
                "sk-123".to_owned(),
            )]));
        let resolved = resolve(store, "cred://openai-key")
            .await
            .expect("resolves");
        assert_eq!(resolved.as_deref(), Some("sk-123"));
    }

    #[tokio::test]
    async fn an_unknown_reference_resolves_to_nothing() {
        let store: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::empty());
        let resolved = resolve(store, "cred://missing").await.expect("resolves");
        assert!(resolved.is_none());
    }

    #[tokio::test]
    async fn an_absent_client_resolves_to_nothing() {
        let hub = toolkit::client_hub::ClientHub::default();
        let resolved = HubSecretResolver::new(std::sync::Arc::new(hub))
            .resolve(&context(), "cred://openai-key")
            .await
            .expect("resolves");
        assert!(resolved.is_none(), "no credential store, no credential");
    }

    #[tokio::test]
    async fn a_late_registered_client_is_picked_up() {
        let hub = Arc::new(toolkit::client_hub::ClientHub::default());
        let resolver = HubSecretResolver::new(Arc::clone(&hub));
        assert!(
            resolver
                .resolve(&context(), "cred://openai-key")
                .await
                .expect("resolves")
                .is_none()
        );
        hub.register::<dyn CredStoreClientV1>(Arc::new(
            MockCredStoreClient::with_secrets(vec![("openai-key".to_owned(), "late".to_owned())]),
        ));
        let late = resolver
            .resolve(&context(), "cred://openai-key")
            .await
            .expect("resolves");
        assert_eq!(late.as_deref(), Some("late"));
    }

    #[tokio::test]
    async fn an_unusable_store_is_a_downstream_error() {
        let store: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::always_failing());
        let error = resolve(store, "cred://openai-key").await.expect_err("fails");
        assert_eq!(error.status(), 502);
    }

    #[tokio::test]
    async fn an_invalid_key_is_an_authentication_failure() {
        let store: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::empty());
        let error = resolve(store, "cred://a:b").await.expect_err("fails");
        assert_eq!(error.status(), 401);
    }

    #[test]
    fn the_scheme_is_stripped_once() {
        assert_eq!(strip_scheme("cred://openai-key"), "openai-key");
        assert_eq!(strip_scheme("openai-key"), "openai-key");
        assert_eq!(strip_scheme("cred://cred://nested"), "cred://nested");
    }
}
