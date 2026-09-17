//! Shared fixtures of the credential-resolution tests.
//!
//! The auth plugins and the [`CredStoreSecretResolver`] are exercised through
//! `credstore_sdk`'s in-process mock, so the tests never touch a network: the
//! only external endpoint a test opens is an `httpmock` server it started
//! itself.

use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use serde_json::Value;
use toolkit::ClientHub;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::infra::plugins::{RequestContext, SecretResolver};
use crate::infra::secrets::CredStoreSecretResolver;

/// An authenticated caller of a fresh tenant.
#[must_use]
pub fn security() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("valid security context")
}

/// A request context for [`security`] calling a fresh upstream.
#[must_use]
pub fn context() -> RequestContext {
    RequestContext::new(security(), Uuid::new_v4(), "/v1/chat")
}

/// A request context that carries `config` as the binding configuration.
#[must_use]
pub fn context_with(config: Value) -> RequestContext {
    let mut context = context();
    context.config = Some(config);
    context
}

/// A client hub holding `client` under the `CredStoreClientV1` contract.
#[must_use]
pub fn hub_with_credstore(client: impl CredStoreClientV1 + 'static) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::new());
    let client: Arc<dyn CredStoreClientV1> = Arc::new(client);
    hub.register(client);
    hub
}

/// A client hub with no credential store at all.
#[must_use]
pub fn hub_without_credstore() -> Arc<ClientHub> {
    Arc::new(ClientHub::new())
}

/// A [`SecretResolver`] that resolves `secrets` through the credstore mock;
/// every other reference fails as missing.
#[must_use]
pub fn secret_store(secrets: Vec<(&str, &str)>) -> Arc<dyn SecretResolver> {
    let store = secrets
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    Arc::new(CredStoreSecretResolver::new(hub_with_credstore(
        credstore_sdk::test_util::MockCredStoreClient::with_secrets(store),
    )))
}
