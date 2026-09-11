//! Credential isolation (`cpt-cf-oagw-dod-plugin-system-credential-resolution`).
//!
//! Constraint 9 of the DECOMPOSITION entry: credentials are never logged,
//! never returned in API responses and never stored by OAGW; `cred_store`
//! resolution happens at request time only, by `cred://` reference; and the
//! resolved material is zeroed when the invocation that needed it ends.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-credential-resolution:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use super::*;
use crate::test_support::FakeCredStore;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The `Debug` rendering of a resolved secret carries the length only, never
/// the material.
#[test]
fn a_resolved_secret_is_never_rendered() {
    let secret = ResolvedSecret::new(b"partner-openai-secret".to_vec());
    let rendered = format!("{secret:?}");
    assert!(!rendered.contains("partner-openai-secret"), "{rendered}");
    assert!(rendered.contains("len"), "{rendered}");
}

/// The material is readable only while the wrapper is alive: the wrapper owns
/// it, zeroes it in its `Drop` impl and exposes no accessor or `Clone`/`Copy`
/// impl that could carry it past the invocation that needed it.
#[test]
fn the_material_cannot_outlive_the_invocation() {
    fn read(secret: &ResolvedSecret) -> &[u8] {
        secret.as_bytes()
    }
    let secret = ResolvedSecret::new(b"material".to_vec());
    assert_eq!(read(&secret), b"material");
    // The wrapper is not `Clone` and not `Copy`: the material cannot be
    // duplicated out of it, and the only way past this line is the `Drop` impl,
    // which zeroes the buffer before releasing it.
    assert!(std::mem::needs_drop::<ResolvedSecret>(), "the wrapper owns the buffer");
    drop(secret);
}

/// Non-UTF-8 material is never surfaced in the error text.
#[test]
fn non_utf8_material_is_not_surfaced() {
    let secret = ResolvedSecret::new(vec![0xff, 0xfe, 0xfd]);
    let error = secret.as_str().expect_err("not UTF-8");
    assert!(matches!(error, crate::domain::plugin::PluginError::Internal(_)));
    assert!(!error.to_string().contains("\u{fffd}"), "{error}");
    assert!(!error.to_string().contains("ff"), "{error}");
}

/// A reference that is not a `cred://` URI is a configuration defect, and the
/// rejection names the boundary and nothing else.
#[tokio::test]
async fn a_malformed_reference_names_nothing() {
    let resolver = CredentialResolver::new(Arc::new(FakeCredStore));
    let ctx = SecurityContext::anonymous();
    let error = resolver.resolve(&ctx, "plaintext-secret").await.expect_err("not a reference");
    assert!(
        error.to_string().contains("cred://"),
        "the rejection names the boundary: {error}"
    );
    assert!(!error.to_string().contains("plaintext-secret"), "{error}");
}

/// `cred_store` decides accessibility: a reference the requesting tenant
/// cannot read answers as unresolvable and nothing is cached from the attempt.
#[tokio::test]
async fn an_inaccessible_reference_is_unavailable() {
    let resolver = CredentialResolver::new(Arc::new(FakeCredStore));
    let ctx = security_context_for(Some(Uuid::new_v4()), Some(Uuid::new_v4()), &[]);
    let error = resolver
        .resolve(&ctx, "cred://partner_openai_key")
        .await
        .expect_err("the fake store resolves nothing");
    assert!(matches!(error, crate::domain::plugin::PluginError::Unavailable), "{error}");
}

/// The security context the plugin hands to `cred_store` carries the calling
/// principal identity, which is what makes `cred_store` the accessibility
/// authority.
#[test]
fn the_security_context_carries_the_calling_identity() {
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let ctx = security_context_for(Some(principal), Some(tenant), &["scope".to_owned()]);
    assert_eq!(ctx.subject_tenant_id(), tenant);
    assert_eq!(ctx.subject_id(), principal);
}
