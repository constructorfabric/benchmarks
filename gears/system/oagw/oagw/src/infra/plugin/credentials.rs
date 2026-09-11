//! Request-time credential resolution through `cred_store`
//! (`cpt-cf-oagw-dod-plugin-system-credential-resolution`).
//!
//! Constraint 9 of the DECOMPOSITION entry: *credentials are never logged,
//! never returned in API responses, never stored by OAGW*; `cred_store`
//! resolution happens at **request time only**, by `cred://` reference,
//! through the `CredStoreClientV1` handle entry 2.1 resolved. The management
//! path never calls `cred_store`.
//!
//! The resolver lets `cred_store` decide whether a reference is accessible to
//! the requesting tenant — owned, or shared by an ancestor — and never
//! inspects the returned material itself.

use std::sync::Arc;

use credstore_sdk::api::CredStoreClientV1;
use credstore_sdk::{CredStoreError, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::dto::CredentialRef;
use crate::domain::plugin::PluginError;

/// Resolved secret material held for the lifetime of one plugin invocation.
///
/// The buffer is zeroed on drop, which is the guarantee the Known Residual
/// Plaintext section of ADR 0008 records for the resolving invocation. The two
/// recorded exceptions — the injected `Authorization` header string and the
/// transient token held inside the IdP fetch — are documented with that ADR as
/// their authority and are deliberately **not** covered here.
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-3
// `inst-ps-cred-3`, `inst-ps-iso-3`: the material is held only inside the
// resolving invocation, in a wrapper that zeroes its buffer on drop and that
// renders no value through `Debug`, so no accessor can carry it past the
// invocation that needed it.
pub struct ResolvedSecret {
    buffer: Vec<u8>,
}
// @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-3

impl std::fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A resolved secret is never rendered: `Debug` carries the length only.
        f.debug_struct("ResolvedSecret").field("len", &self.buffer.len()).finish()
    }
}

impl Drop for ResolvedSecret {
    fn drop(&mut self) {
        for byte in &mut self.buffer {
            *byte = 0;
        }
    }
}

impl ResolvedSecret {
    /// Wrap resolved material.
    #[must_use]
    pub fn new(buffer: Vec<u8>) -> Self {
        Self { buffer }
    }

    /// The material as UTF-8, when it is textual.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Internal`] when the material is not UTF-8; the
    /// bytes are never surfaced in the error text.
    pub fn as_str(&self) -> Result<&str, PluginError> {
        std::str::from_utf8(&self.buffer)
            .map_err(|_| PluginError::Internal("a resolved credential is not valid UTF-8".to_owned()))
    }

    /// The raw material.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer
    }
}

/// The request-time `cred://` resolver every credential-injecting plugin
/// shares.
#[derive(Clone)]
pub struct CredentialResolver {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl CredentialResolver {
    /// Build the resolver over the `cred_store` handle entry 2.1 resolved.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }

    /// Resolve one `cred://` reference at request time.
    ///
    /// `cred_store` decides whether the reference is accessible to the
    /// requesting tenant, either directly or through an ancestor sharing
    /// policy, so the resolver performs no accessibility reasoning of its own.
    ///
    /// # Errors
    ///
    /// * [`PluginError::Unavailable`] when the reference does not resolve or
    ///   is not accessible to the tenant — the credential step fails and
    ///   nothing is cached;
    /// * [`PluginError::Internal`] when `cred_store` is unreachable — an
    ///   internal plugin failure, again cached by no one, so the next request
    ///   retries the resolution.
    pub async fn resolve(
        &self,
        ctx: &SecurityContext,
        reference: &str,
    ) -> Result<ResolvedSecret, PluginError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-2
        // `inst-ps-cred-2`: the resolution goes through the `cred_store` client
        // and `cred_store` alone decides whether the reference is accessible to
        // the requesting tenant, directly or through an ancestor sharing
        // policy; the resolver performs no accessibility reasoning of its own
        // and inspects no material.
        // `inst-ps-cred-4`/`-5`: a reference that does not resolve or is not
        // accessible fails the credential step as `Unavailable` and caches
        // nothing, so the next request resolves again.
        // `inst-ps-cred-6`/`-7`: an unreachable store is an internal plugin
        // failure, cached by no one. `inst-ps-cred-8`, `inst-ps-iso-2`/`-7`:
        // the material is returned to the calling plugin only — never to a log
        // sink, an error body or an API response, which carry the reference.
        // A malformed reference is a configuration defect, never secret
        // material: the rejection names the boundary and nothing else.
        if !crate::domain::dto::is_cred_reference(reference) {
            return Err(PluginError::Internal(
                "a credential-bearing configuration field must hold a `cred://` reference".to_owned(),
            ));
        }
        let key = reference
            .strip_prefix(CredentialRef::PREFIX)
            .and_then(|rest| SecretRef::new(rest).ok())
            .ok_or_else(|| {
                PluginError::Internal(
                    "a `cred://` reference must name a valid `credstore` key".to_owned(),
                )
            })?;
        match self.credstore.get(ctx, &key).await {
            // `Ok(None)` is the single 404 surface `credstore` exposes: the
            // reference either does not exist or is not accessible to the
            // tenant, and the two are indistinguishable here by design.
            Ok(Some(response)) => Ok(ResolvedSecret::new(response.value.as_bytes().to_vec())),
            Ok(None) | Err(CredStoreError::NotFound | CredStoreError::AccessDenied) => {
                Err(PluginError::Unavailable)
            }
            // Every other answer is a backing-service failure: an internal
            // plugin failure that caches nothing.
            Err(_) => Err(PluginError::Internal(
                "the credential store could not be reached".to_owned(),
            )),
        }
    }
}
// @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-2

/// Build the `SecurityContext` a resolution is performed under.
///
/// The principal the auth phase carries is the requesting subject; when no
/// subject was resolved the request is anonymous and `cred_store` applies its
/// own accessibility decision to an anonymous caller.
#[must_use]
pub fn security_context_for(
    subject_id: Option<uuid::Uuid>,
    tenant_id: Option<uuid::Uuid>,
    scopes: &[String],
) -> SecurityContext {
    // A context needs both a subject and a tenant; anything less is the
    // anonymous caller `credstore` applies its own decision to.
    let (Some(subject_id), Some(tenant_id)) = (subject_id, tenant_id) else {
        return SecurityContext::anonymous();
    };
    let builder = SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id);
    let builder = if scopes.is_empty() {
        builder
    } else {
        builder.token_scopes(scopes.to_vec())
    };
    builder.build().unwrap_or_else(|_| SecurityContext::anonymous())
}

#[cfg(test)]
#[path = "credential_resolution_tests.rs"]
mod credential_resolution_tests;
