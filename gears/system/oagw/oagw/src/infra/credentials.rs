// Created: 2026-08-29 by Constructor Tech
//! CredStore-backed credential resolution (DESIGN `cpt-cf-oagw-principle-cred-isolation`).
//!
//! Plugins reference secret material as `cred://` URIs and never see the value
//! of a reference they were not configured with; the store's own hierarchical
//! resolution decides whether a tenant may read it.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use credstore_sdk::models::SecretRef;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainResult;
use crate::infra::plugin::CredentialResolver;

/// Resolves `cred://` references through the CredStore gear.
pub struct CredStoreResolver {
    store: Arc<dyn CredStoreClientV1>,
}

impl std::fmt::Debug for CredStoreResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CredStoreResolver").finish()
    }
}

impl CredStoreResolver {
    /// Wraps a CredStore client handle.
    #[must_use]
    pub fn new(store: Arc<dyn CredStoreClientV1>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl CredentialResolver for CredStoreResolver {
    async fn resolve(
        &self,
        reference: &str,
        tenant_id: &str,
        subject_id: Option<&str>,
    ) -> DomainResult<String> {
        // The wire spelling is `cred://<ref>`; the store keys the bare
        // reference, so the scheme is stripped before the lookup.
        let bare = reference.strip_prefix("cred://").unwrap_or(reference);
        let Ok(key) = SecretRef::new(bare) else {
            return Err(crate::domain::error::DomainError::SecretNotFound(
                reference.to_owned(),
            ));
        };
        // A tenant id that is not a UUID is not a namespace the store knows, and
        // the nil UUID is reserved for the store's internal namespaces, so it is
        // never used as a fallback.
        let Ok(tenant) = uuid::Uuid::parse_str(tenant_id) else {
            return Err(crate::domain::error::DomainError::AuthenticationFailed(
                format!("credential tenant {tenant_id:?} is not a tenant identifier"),
            ));
        };
        let subject = subject_id.and_then(|id| uuid::Uuid::parse_str(id).ok());
        let context = SecurityContext::builder()
            .subject_id(subject.unwrap_or(uuid::Uuid::nil()))
            .subject_type("service")
            .subject_tenant_id(tenant)
            .build()
            .map_err(|error| {
                crate::domain::error::DomainError::AuthenticationFailed(error.to_string())
            })?;
        match self.store.get(&context, &key).await {
            Ok(Some(response)) => {
                Ok(String::from_utf8_lossy(response.value.as_bytes()).into_owned())
            }
            Ok(None) | Err(_) => Err(crate::domain::error::DomainError::SecretNotFound(
                reference.to_owned(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_empty_reference_is_rejected_before_the_store() {
        let resolver = CredStoreResolver::new(std::sync::Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::empty(),
        ));
        let error = resolver
            .resolve("", "not-a-uuid", None)
            .await
            .expect_err("empty reference");
        assert_eq!(error.status_code(), 500);
    }

    #[tokio::test]
    async fn a_missing_secret_reports_secret_not_found() {
        let resolver = CredStoreResolver::new(std::sync::Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::empty(),
        ));
        let error = resolver
            .resolve("cred://absent", &uuid::Uuid::new_v4().to_string(), None)
            .await
            .expect_err("absent secret");
        assert_eq!(error.status_code(), 500);
    }

    #[tokio::test]
    async fn a_non_uuid_tenant_is_refused_rather_than_defaulted() {
        let resolver = CredStoreResolver::new(std::sync::Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::empty(),
        ));
        let error = resolver
            .resolve("cred://key", "not-a-uuid", None)
            .await
            .expect_err("unusable tenant");
        assert_eq!(error.status_code(), 401);
    }

    #[tokio::test]
    async fn a_resolved_secret_is_decoded_as_utf8() {
        let tenant = uuid::Uuid::new_v4();
        let resolver = CredStoreResolver::new(std::sync::Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![(
                "cred://key".to_owned(),
                "hunter2".to_owned(),
            )]),
        ));
        let value = resolver
            .resolve("cred://key", &tenant.to_string(), None)
            .await
            .expect("secret resolves");
        assert_eq!(value, "hunter2");
    }
}
