// Created: 2026-08-31 by Constructor Tech
//! Credential-store access shared by the auth plugins (DESIGN §3.2 "Secret
//! Access Control", ADR-0002, ADR-0008).
//!
//! The gateway never stores secret material: an auth binding references a
//! secret through a `cred://` URI and the data plane resolves it **at request
//! time**, as the caller's [`SecurityContext`], so `cred_store` can apply its
//! own sharing policy. The resolved value only ever lives in a
//! [`SecretString`]; the mapping of a store failure onto the DESIGN §3.3 error
//! table lives here so the two plugins cannot drift.

use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use credstore_sdk::CredStoreError;
use toolkit_auth::oauth2::SecretString;

use crate::error::{OagwError, OagwErrorKind};

/// Resolved secret of a `cred://` reference.
///
/// Wrapping the store's raw bytes in [`SecretString`] is what keeps the value
/// out of `Debug`/`Display` output and zeroes it when the request is done.
#[derive(Clone)]
pub struct ResolvedSecret(SecretString);

impl ResolvedSecret {
    /// The secret material. Callers must not log or serialise it.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl std::fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// The credential store the plugins read through.
pub type CredStore = Arc<dyn CredStoreClientV1>;

/// Resolve a `cred://` reference as `ctx` and map a failure onto DESIGN §3.3.
///
/// * `Ok(None)` and [`CredStoreError::NotFound`] are the single "not found"
///   surface of the store → 500 `secret.not_found.v1`.
/// * [`CredStoreError::AccessDenied`] is the caller not being allowed to read
///   the secret → 401 `auth.failed.v1` (DESIGN §3.2 "Secret Access Control":
///   "If accessible → return secret material. If not → … 401").
/// * every other failure is a store outage → 503 `link.unavailable.v1`.
///
/// # Errors
/// As listed above; the detail names the reference, never the secret.
pub async fn resolve_secret(
    credstore: &CredStore,
    ctx: &toolkit_security::SecurityContext,
    reference: &str,
) -> Result<ResolvedSecret, OagwError> {
    let key = reference
        .trim()
        .strip_prefix("cred://")
        .unwrap_or(reference);
    let secret_ref =
        credstore_sdk::SecretRef::new(key).map_err(|error| invalid_reference(reference, &error))?;
    match credstore.get(ctx, &secret_ref).await {
        Ok(Some(response)) => {
            let value = std::str::from_utf8(response.value.as_bytes())
                .map_err(|_| not_utf8(reference))?
                .to_owned();
            Ok(ResolvedSecret(SecretString::new(value)))
        }
        Ok(None) => Err(missing_secret(reference)),
        Err(error) => Err(store_failure(reference, &error)),
    }
}

/// 500 `secret.not_found.v1` for a reference the store does not know.
fn missing_secret(reference: &str) -> OagwError {
    OagwError::new(
        OagwErrorKind::SecretNotFound,
        format!("referenced secret '{reference}' was not found in the credential store"),
    )
}

/// 401 `auth.failed.v1` for a reference the caller may not read.
fn denied_secret(reference: &str, error: &CredStoreError) -> OagwError {
    OagwError::new(
        OagwErrorKind::AuthenticationFailed,
        format!("referenced secret '{reference}' is not accessible: {error}"),
    )
}

/// 503 `link.unavailable.v1` for a credential store that cannot answer.
fn unavailable_store(reference: &str, error: &CredStoreError) -> OagwError {
    OagwError::new(
        OagwErrorKind::LinkUnavailable,
        format!("credential store refused '{reference}': {error}"),
    )
}

/// Map a store failure onto the error table.
fn store_failure(reference: &str, error: &CredStoreError) -> OagwError {
    if error.is_not_found() {
        return missing_secret(reference);
    }
    if error.is_permission_denied() {
        return denied_secret(reference, error);
    }
    unavailable_store(reference, error)
}

/// 400 `validation.error.v1` for a `cred://` reference the store rejects as a
/// reference.
fn invalid_reference(reference: &str, error: &CredStoreError) -> OagwError {
    OagwError::validation(format!(
        "credential reference '{reference}' is not a valid secret reference: {error}"
    ))
}

/// 500 for a secret the gateway cannot inject (binary material).
fn not_utf8(reference: &str) -> OagwError {
    OagwError::new(
        OagwErrorKind::Internal,
        format!("referenced secret '{reference}' is not text and cannot be injected"),
    )
}

/// In-process credential store for the unit tests of this module and of the
/// auth plugins.
///
/// The integration tests use the SDK's own `MockCredStoreClient`; these unit
/// tests cannot, because the `test-util` feature of `credstore-sdk` is only
/// enabled for the crate's integration tests.
#[cfg(test)]
pub(crate) mod stub {
    use async_trait::async_trait;
    use credstore_sdk::{
        CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef, SecretValue, SharingMode,
        WriteOptions, WritePrecondition,
    };
    use toolkit_security::SecurityContext;

    /// Behaviour of the stub store.
    #[derive(Clone, Copy)]
    pub enum Behaviour {
        /// Resolve every reference to this value.
        Fixed(&'static str),
        /// Resolve every reference to raw, non-UTF-8 bytes.
        RawBytes(&'static [u8]),
        /// Every reference is unknown.
        Empty,
        /// The store reports a not-found error instead of `Ok(None)`.
        NotFound,
        /// The store fails internally.
        Failing,
        /// The caller may not read the secret.
        Denied,
    }

    /// A store that answers every reference with the configured behaviour.
    pub struct StubCredStore(pub Behaviour);

    fn response(value: Vec<u8>) -> GetSecretResponse {
        GetSecretResponse {
            value: SecretValue::new(value),
            id: uuid::Uuid::nil(),
            owner_tenant_id: tenant_resolver_sdk::TenantId(uuid::Uuid::nil()),
            sharing: SharingMode::default(),
            is_inherited: false,
            version: 1,
            secret_type: String::new(),
            expires_at: None,
        }
    }

    #[async_trait]
    impl CredStoreClientV1 for StubCredStore {
        async fn get(
            &self,
            _ctx: &SecurityContext,
            _key: &SecretRef,
        ) -> Result<Option<GetSecretResponse>, CredStoreError> {
            match self.0 {
                Behaviour::Fixed(value) => Ok(Some(response(Vec::from(value.as_bytes())))),
                Behaviour::RawBytes(value) => Ok(Some(response(Vec::from(value)))),
                Behaviour::Empty => Ok(None),
                Behaviour::NotFound => Err(CredStoreError::NotFound),
                Behaviour::Failing => Err(CredStoreError::Internal("stub outage".into())),
                Behaviour::Denied => Err(CredStoreError::AccessDenied),
            }
        }

        async fn put_opts(
            &self,
            _ctx: &SecurityContext,
            _key: &SecretRef,
            _value: SecretValue,
            _sharing: SharingMode,
            _precondition: WritePrecondition,
            _opts: WriteOptions,
        ) -> Result<(), CredStoreError> {
            Ok(())
        }

        async fn create_opts(
            &self,
            _ctx: &SecurityContext,
            _key: &SecretRef,
            _value: SecretValue,
            _sharing: SharingMode,
            _opts: WriteOptions,
        ) -> Result<(), CredStoreError> {
            Ok(())
        }

        async fn delete(
            &self,
            _ctx: &SecurityContext,
            _key: &SecretRef,
            _precondition: WritePrecondition,
        ) -> Result<(), CredStoreError> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use toolkit_security::SecurityContext;

    use super::stub::{Behaviour, StubCredStore};
    use super::{ResolvedSecret, resolve_secret};

    /// Assert the problem type of `error`, by the GTS slug after the stem.
    fn assert_problem_type(error: &crate::error::OagwError, slug: &str) {
        let problem_type = error.gts_type();
        assert!(
            problem_type.ends_with(slug),
            "problem type '{problem_type}' is not '{slug}'"
        );
    }

    fn store(behaviour: Behaviour) -> super::CredStore {
        std::sync::Arc::new(StubCredStore(behaviour))
    }

    fn context() -> SecurityContext {
        SecurityContext::anonymous()
    }

    #[tokio::test]
    async fn a_known_reference_resolves_into_a_secret_string() {
        let resolved = resolve_secret(
            &store(Behaviour::Fixed("k-123")),
            &context(),
            "cred://api-key",
        )
        .await
        .unwrap();
        assert_eq!(resolved.expose(), "k-123");
    }

    #[tokio::test]
    async fn the_stem_of_the_reference_is_optional() {
        let resolved = resolve_secret(&store(Behaviour::Fixed("k-123")), &context(), "api-key")
            .await
            .unwrap();
        assert_eq!(resolved.expose(), "k-123");
    }

    #[tokio::test]
    async fn an_unknown_reference_is_a_secret_not_found() {
        let error = resolve_secret(&store(Behaviour::Empty), &context(), "cred://api-key")
            .await
            .unwrap_err();
        assert_problem_type(&error, "secret.not_found.v1");
    }

    #[tokio::test]
    async fn a_store_not_found_is_a_secret_not_found() {
        let error = resolve_secret(&store(Behaviour::NotFound), &context(), "cred://api-key")
            .await
            .unwrap_err();
        assert_problem_type(&error, "secret.not_found.v1");
    }

    #[tokio::test]
    async fn a_denied_reference_is_an_auth_failure() {
        let error = resolve_secret(&store(Behaviour::Denied), &context(), "cred://api-key")
            .await
            .unwrap_err();
        assert_problem_type(&error, "auth.failed.v1");
    }

    #[tokio::test]
    async fn a_store_outage_is_a_link_unavailable() {
        let error = resolve_secret(&store(Behaviour::Failing), &context(), "cred://api-key")
            .await
            .unwrap_err();
        assert_problem_type(&error, "link.unavailable.v1");
    }

    #[tokio::test]
    async fn binary_material_cannot_be_injected() {
        let error = resolve_secret(
            &store(Behaviour::RawBytes(&[0xff, 0xfe])),
            &context(),
            "cred://api-key",
        )
        .await
        .unwrap_err();
        assert_problem_type(&error, "internal.error.v1");
    }

    #[tokio::test]
    async fn a_reference_the_store_rejects_is_a_validation_error() {
        let error = resolve_secret(
            &store(Behaviour::Fixed("x")),
            &context(),
            "cred://bad reference",
        )
        .await
        .unwrap_err();
        assert_problem_type(&error, "validation.error.v1");
    }

    #[test]
    fn a_resolved_secret_is_never_formatted() {
        let resolved = ResolvedSecret(toolkit_auth::oauth2::SecretString::new("k-123".to_owned()));
        assert_eq!(format!("{resolved:?}"), "[REDACTED]");
        assert!(!format!("{resolved:?}").contains("k-123"));
    }
}
