// Updated: 2026-09-01 by Constructor Tech
//! Fixtures shared by the plugin unit tests.
//!
//! `RequestContext` needs a caller identity, an alias and an upstream id; every
//! plugin test would otherwise repeat the same six lines. Compiled only for
//! `cfg(test)`, so it never ships in the library artefact.

#![cfg(test)]

use std::collections::BTreeMap;

use toolkit_security::SecurityContext;

use crate::domain::plugin::{RequestContext, ResponseContext};

/// A caller in a synthetic tenant.
#[must_use]
pub fn security_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(tenant())
        .subject_type("user")
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("fixture security context must build")
}

/// The fixture tenant.
#[must_use]
pub fn tenant() -> uuid::Uuid {
    uuid::Uuid::from_u128(0x0a0a_0a0a_0a0a_0a0a_0a0a_0a0a_0a0a_0a0a)
}

/// A well-formed request context: JSON body, no query, no headers.
#[must_use]
pub fn request_context() -> RequestContext {
    let security_context = security_context();
    RequestContext {
        request_id: format!("req_{}", uuid::Uuid::new_v4().simple()),
        security_context,
        tenant_id: tenant(),
        alias: "api.example.com".to_owned(),
        upstream_id: crate::gts::instance_id(crate::gts::UPSTREAM_TYPE, uuid::Uuid::new_v4()),
        path: "/v1/models".to_owned(),
        query: String::new(),
        method: http::Method::GET,
        headers: http::HeaderMap::new(),
        config: BTreeMap::new(),
        body: bytes::Bytes::new(),
    }
}

/// A response context with no body and no headers.
#[must_use]
pub fn response_context(config: BTreeMap<String, serde_json::Value>) -> ResponseContext {
    ResponseContext {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: None,
        config,
    }
}

/// A stand-in for the credstore gear: a plain map of key to value.
///
/// The plugins only ever read, so the write methods keep the trait's default
/// "not supported" behaviour, which is what a value-store double is allowed to
/// do. Compiled only for `cfg(test)`.
pub struct MockCredStoreClient {
    secrets: BTreeMap<String, String>,
}

impl MockCredStoreClient {
    /// A store holding the given key/value pairs.
    #[must_use]
    pub fn with_secrets(pairs: Vec<(String, String)>) -> Self {
        Self {
            secrets: pairs.into_iter().collect(),
        }
    }

    /// A store holding nothing, so every lookup misses.
    #[must_use]
    pub fn empty() -> Self {
        Self::with_secrets(Vec::new())
    }
}

#[async_trait::async_trait]
impl credstore_sdk::CredStoreClientV1 for MockCredStoreClient {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        key: &credstore_sdk::SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
        let name = key.as_ref();
        let Some(value) = self.secrets.get(name) else {
            // A single 404 surface: a miss is indistinguishable from an
            // inaccessible secret.
            return Ok(None);
        };
        Ok(Some(credstore_sdk::GetSecretResponse {
            value: credstore_sdk::SecretValue::new(value.clone().into_bytes()),
            // The id only feeds an `ETag` the plugins never look at, so one
            // stable value serves every entry.
            id: uuid::Uuid::from_u128(0x0c0c_0c0c_0c0c_0c0c_0c0c_0c0c_0c0c_0c0c),
            owner_tenant_id: tenant_resolver_sdk::TenantId(tenant()),
            sharing: credstore_sdk::SharingMode::Private,
            is_inherited: false,
            version: 1,
            secret_type: "cf.core.credstore.secret.v1~cf.core.credstore.generic.v1~".to_owned(),
            expires_at: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_context_carries_a_tenant() {
        let ctx = request_context();
        assert_eq!(ctx.subject_tenant_id(), Some(tenant()));
        assert!(ctx.subject_id().is_some());
        assert!(ctx.request_id.starts_with("req_"));
    }

    #[test]
    fn anonymous_context_has_no_tenant() {
        let mut ctx = request_context();
        ctx.security_context = SecurityContext::anonymous();
        assert_eq!(ctx.subject_tenant_id(), None);
        assert_eq!(ctx.subject_id(), None);
    }
}
