//! Credential-resolution port.
//!
//! Plugins never see a literal secret: an upstream's `auth.config` carries
//! `cred://` references, and the reference is resolved through this port at
//! request time (`docs/DESIGN.md` — principle `cpt-cf-oagw-principle-cred-isolation`).
//! The data plane installs an adapter over the `credstore` client; tests
//! install an in-memory one.
use async_trait::async_trait;

/// A secret value, already decrypted, as a UTF-8 string.
///
/// The wrapper keeps the value out of `Debug`/`Display` output so a secret is
/// never written to a log or a problem document by accident.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ResolvedSecret(String);

impl ResolvedSecret {
    /// Wrap an already-resolved value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the value is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResolvedSecret(***)")
    }
}

/// Why a `cred://` reference could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// The reference names no secret in the credential store.
    #[error("credential reference '{reference}' does not exist")]
    NotFound {
        /// The offending reference.
        reference: String,
    },
    /// The credential store is unreachable or rejected the read.
    #[error("credential store unavailable: {0}")]
    Unavailable(String),
    /// The reference is not a well-formed `cred://` URI or secret name.
    #[error("invalid credential reference '{reference}': {reason}")]
    Invalid {
        /// The offending reference.
        reference: String,
        /// Why it is invalid.
        reason: String,
    },
}

/// Port the auth plugins use to resolve `cred://` references.
///
/// A reference is spelled either `cred://<name>` or bare `<name>`; the
/// implementations of this port normalise both.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolve a reference. `Ok(None)` means the reference is absent from the
    /// configuration, which the caller treats as a configuration error.
    ///
    /// # Errors
    ///
    /// [`SecretError::NotFound`] when the store holds no such secret,
    /// [`SecretError::Invalid`] when the reference is malformed, and
    /// [`SecretError::Unavailable`] when the store cannot be reached.
    async fn resolve(&self, reference: &str) -> Result<Option<ResolvedSecret>, SecretError>;
}

#[async_trait]
impl SecretResolver for std::sync::Arc<dyn SecretResolver> {
    async fn resolve(&self, reference: &str) -> Result<Option<ResolvedSecret>, SecretError> {
        (**self).resolve(reference).await
    }
}

/// Strip the `cred://` scheme of a reference, when present.
#[must_use]
pub fn strip_cred_scheme(reference: &str) -> &str {
    reference
        .strip_prefix("cred://")
        .unwrap_or(reference)
        .trim()
}
