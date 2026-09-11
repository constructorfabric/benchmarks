//! Shared doubles for the plugin unit tests.
//!
//! Kept beside the plugin contracts so each `*_tests.rs` file can build a
//! request context and a credential resolver without repeating the wiring.

use super::{Credential, CredentialResolver, PluginRequestContext};
use crate::domain::error::OagwError;
use async_trait::async_trait;
use http::HeaderMap;
use std::collections::BTreeMap;
use uuid::Uuid;

/// A resolver refusing every reference with an authentication failure.
#[must_use]
pub fn denied_resolver() -> DeniedResolver {
    DeniedResolver
}

/// A resolver that never resolves anything.
#[must_use]
pub fn empty_resolver() -> EmptyResolver {
    EmptyResolver
}

/// The tenant the plugin tests run against.
#[must_use]
pub fn tenant() -> Uuid {
    Uuid::nil()
}

/// A request context for the given method and path.
#[must_use]
pub fn context(method: &str, path: &str) -> PluginRequestContext {
    PluginRequestContext {
        request_id: "11111111-2222-3333-4444-555555555555".to_owned(),
        tenant_id: tenant(),
        upstream_id: "gts.cf.core.oagw.upstream.v1~test".to_owned(),
        route_id: None,
        alias: "api.example.com".to_owned(),
        target_host: "api.example.com".to_owned(),
        method: method.to_owned(),
        path: path.to_owned(),
        query: String::new(),
        headers: HeaderMap::new(),
        body: bytes::Bytes::new(),
        credential: None,
    }
}

/// A resolver that never resolves anything.
#[derive(Debug, Default)]
pub struct EmptyResolver;

#[async_trait]
impl CredentialResolver for EmptyResolver {
    async fn resolve(&self, _tenant_id: Uuid, reference: &str) -> Result<Credential, OagwError> {
        Err(OagwError::SecretNotFound(format!(
            "credential '{reference}' does not exist"
        )))
    }
}

/// A resolver that refuses every reference the way the store does when the
/// caller may not read it.
#[derive(Debug, Default)]
pub struct DeniedResolver;

#[async_trait]
impl CredentialResolver for DeniedResolver {
    async fn resolve(&self, _tenant_id: Uuid, reference: &str) -> Result<Credential, OagwError> {
        Err(OagwError::AuthenticationFailed(format!(
            "credential '{reference}' may not be read by this tenant"
        )))
    }
}

/// A resolver backed by a fixed `reference → value` table.
#[derive(Debug, Default, Clone)]
pub struct StaticResolver {
    secrets: BTreeMap<String, Vec<u8>>,
}

impl StaticResolver {
    /// A resolver serving `secrets`.
    #[must_use]
    pub fn new(secrets: &[(&str, &str)]) -> Self {
        Self {
            secrets: secrets
                .iter()
                .map(|(reference, value)| ((*reference).to_owned(), (*value).as_bytes().to_vec()))
                .collect(),
        }
    }
}

#[async_trait]
impl CredentialResolver for StaticResolver {
    async fn resolve(&self, _tenant_id: Uuid, reference: &str) -> Result<Credential, OagwError> {
        // The store resolves hierarchically, so only the final segment names
        // a secret; mirroring that keeps the double honest about references
        // like `cred://acme/inner-key`.
        let path = reference.strip_prefix("cred://").unwrap_or(reference);
        let key = path.rsplit('/').next().unwrap_or(path);
        self.secrets.get(key).map_or_else(
            || {
                Err(OagwError::SecretNotFound(format!(
                    "credential '{key}' does not exist"
                )))
            },
            |value| Ok(Credential::new(value.clone())),
        )
    }
}
