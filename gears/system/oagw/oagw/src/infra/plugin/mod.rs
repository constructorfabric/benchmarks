//! Built-in plugin implementations (ADR-0002) and registries.
//!
//! - `noop_auth` — no authentication
//! - `apikey_auth` — API key injection (header/query) from the credential
//!   store
//! - `oauth2_client_cred_auth` — `OAuth2` client credentials (Form + Basic),
//!   with a TTL-bounded token cache keyed per tenant/subject/config
//!   (ADR-0008)
//! - `required_headers_guard` — request/response header presence enforcement
//!   (ADR-0009)
//! - `request_id_transform` — `X-Request-ID` propagation
//! - `registry` — the per-family registries consulted by the data plane

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use toolkit_security::SecurityContext;

/// Resolve a `cred://<ref>` (or bare `<ref>`) reference to its raw bytes.
///
/// Strips the `cred://` scheme prefix before handing the reference to the
/// credential store. `Ok(None)` when the secret does not exist; errors are
/// propagated as-is.
pub(crate) async fn resolve_secret(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &SecurityContext,
    value: &str,
) -> Result<Option<Vec<u8>>, CredStoreError> {
    let bare = value.strip_prefix("cred://").unwrap_or(value);
    let reference = SecretRef::new(bare).map_err(|e| CredStoreError::invalid_ref(e.to_string()))?;
    Ok(credstore
        .get(ctx, &reference)
        .await?
        .map(|r| r.value.as_bytes().to_vec()))
}
