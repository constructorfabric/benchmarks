//! Credential resolution from the cred store
//! (`cpt-cf-oagw-algo-cred-resolve`).
//!
//! The chain resolves every credential it needs at request time, by `cred://`
//! reference, through the `credstore-sdk` contract
//! `cpt-cf-oagw-contract-cred-store`. The resolution is scoped to the calling
//! tenant the security context carries, so a reference that names another
//! tenant's credential is indistinguishable from an unknown one, and every
//! failure class of the store — an unknown reference, a cross-tenant one, a
//! store error and a store timeout — is one outcome: `500` SecretNotFound with
//! a detail that names the configuration key and nothing else
//! (`inst-acr-05`, `inst-acr-06`).
//!
//! The resolved value is handed to the caller as a [`SecretString`] and to
//! nothing else: no request-context field, no log line, no error body and no
//! response carries it (`inst-acr-08`). The caller drops it as soon as the
//! injection or the token fetch that consumed it completes (`inst-acr-09`).

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_auth::oauth2::SecretString;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

/// The scheme of every credential reference the chain resolves.
pub const CRED_SCHEME: &str = "cred://";

/// The instance part of a `cred://` reference, without its scheme.
///
/// A reference that is absent, not a `cred://` URI, or whose instance is empty
/// resolves to nothing: the caller reports it as a resolution failure
/// (`inst-acr-01`).
#[must_use]
pub fn reference_instance(reference: &str) -> Option<&str> {
    let instance = reference.strip_prefix(CRED_SCHEME)?;
    let instance = instance.trim();
    (!instance.is_empty()).then_some(instance)
}

/// The store key a `cred://` reference resolves through.
///
/// The `credstore-sdk` reference type admits the instance characters only, so
/// the scheme is not part of it.
fn store_key(reference: &str) -> Result<SecretRef, DomainError> {
    let instance = reference_instance(reference).ok_or_else(|| DomainError::SecretNotFound {
        detail: "the credential reference is not a well-formed `cred://` URI".to_owned(),
    })?;
    SecretRef::new(instance).map_err(|_| DomainError::SecretNotFound {
        detail: "the credential reference is not a well-formed `cred://` URI".to_owned(),
    })
}

/// A client that resolves nothing.
///
/// The mount installs it when the gear carries no credstore dependency, so a
/// chain that asks for a credential fails closed with `500` SecretNotFound
/// instead of forwarding a request without the credential it declares.
#[derive(Debug, Default)]
struct UnresolvedCredStore;

#[async_trait::async_trait]
impl CredStoreClientV1 for UnresolvedCredStore {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
        Ok(None)
    }
}

/// The credential source the chain resolves its references through.
///
/// It is the one dependency the chain holds on the credstore, and it is never
/// read for anything but a `cred://` reference: the merged configuration the
/// pipeline hands the hook carries references only, never a resolved value
/// (`cpt-cf-oagw-dod-cred-isolation`).
#[derive(Clone)]
pub struct CredentialSource {
    client: std::sync::Arc<dyn CredStoreClientV1>,
}

impl std::fmt::Debug for CredentialSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The client is never rendered: it holds no secret material, but its
        // concrete type is an implementation detail of the host.
        formatter.write_str("CredentialSource")
    }
}

impl CredentialSource {
    /// A source over `client`.
    #[must_use]
    pub fn new(client: std::sync::Arc<dyn CredStoreClientV1>) -> Self {
        Self { client }
    }

    /// A source that resolves nothing, for a mount without the dependency.
    #[must_use]
    pub fn unresolved() -> Self {
        Self::new(std::sync::Arc::new(UnresolvedCredStore))
    }

    /// Resolve one `cred://` reference for `key`.
    ///
    /// # Errors
    ///
    /// Returns `500` SecretNotFound when the reference is malformed, unknown to
    /// the store, owned by another tenant, or when the store itself fails or
    /// times out. The detail names `key` only.
    pub async fn resolve(
        &self,
        security: &SecurityContext,
        key: &str,
        reference: &str,
    ) -> Result<SecretString, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-03
        // @cpt-begin:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-01
        // A reference that is absent, malformed or not a `cred://` URI is a
        // resolution failure, decided before the store is contacted.
        let key_ref = match store_key(reference) {
            Ok(key_ref) => key_ref,
            Err(_) => return Err(named(key)),
        };
        // @cpt-end:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-01

        // @cpt-begin:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-02
        // The resolution is scoped to the calling tenant: the store sees the
        // security context, so a reference that names another tenant's
        // credential is indistinguishable from an unknown one.
        // @cpt-begin:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-04
        // The store is called once per reference per request that needs it: an
        // OAuth2 cache miss resolves its two references inside the same fetch
        // path, and no credential is cached separately from its token.
        let resolved = self.client.get(security, &key_ref).await;
        // @cpt-end:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-04
        // @cpt-end:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-02

        // @cpt-begin:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-05
        // @cpt-begin:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-06
        // One outcome for every failure class, naming the configuration key
        // only: the reference's target, the secret and any part of it are
        // never in the detail.
        let value = match resolved {
            Ok(Some(response)) => response,
            Ok(None) | Err(_) => return Err(named(key)),
        };
        // @cpt-end:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-06
        // @cpt-end:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-05

        // @cpt-begin:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-07
        // The value is wrapped as `SecretString` and handed to the caller only.
        Ok(SecretString::new(
            String::from_utf8_lossy(value.value.as_bytes()).into_owned(),
        ))
        // @cpt-end:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-07
        // @cpt-end:cpt-cf-oagw-algo-cred-resolve:p1:inst-acr-03
    }
}

/// The `500` outcome of a resolution that failed, named after the
/// configuration key the reference came from (`inst-acr-10`).
fn named(key: &str) -> DomainError {
    DomainError::SecretNotFound {
        detail: format!("the credential reference of `{key}` cannot be resolved"),
    }
}
