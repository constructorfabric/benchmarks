#![cfg(feature = "fips")]

//! Regression for issue #1935: under `--features fips`, building a TLS client
//! before `toolkit::bootstrap::init_crypto_provider` has installed the
//! process-wide rustls [`CryptoProvider`] MUST fail closed with
//! [`TlsConfigError::NoCryptoProvider`] rather than silently constructing
//! an uninstalled provider via the historical `unwrap_or_else` fallback.
//!
//! Lives in its own integration-test binary (separate process from
//! `cargo test`'s in-crate unit tests) so the runtime state is guaranteed
//! clean — no other test installs a default provider before this one runs.

use toolkit_http::{HttpClient, HttpError, TlsConfigError};

#[test]
fn build_client_without_init_crypto_provider_returns_no_crypto_provider() {
    // Precondition: this test binary has not installed any rustls provider.
    // If it had, the `get_default().is_some()` branch in `build_client_config`
    // would succeed and the test premise would be invalid.
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_none(),
        "test precondition: no crypto provider must be installed before this test runs"
    );

    // `let-else` instead of `.expect_err()` because `HttpClient` does not
    // implement `Debug` (clippy::manual_let_else points this pattern at us).
    let Err(err) = HttpClient::builder().build() else {
        panic!(
            "HttpClient::builder().build() must fail closed under --features fips \
             when no crypto provider is installed"
        );
    };

    let inner = match err {
        HttpError::Tls(inner) => inner,
        other => panic!("expected HttpError::Tls(_), got {other:?}"),
    };

    let tls_err = inner
        .downcast_ref::<TlsConfigError>()
        .expect("HttpError::Tls source must be a TlsConfigError");

    assert!(
        matches!(tls_err, TlsConfigError::NoCryptoProvider),
        "expected TlsConfigError::NoCryptoProvider, got {tls_err:?}"
    );
}
