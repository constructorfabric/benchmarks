#![cfg(all(feature = "bootstrap", feature = "fips"))]

//! Regression for the FIPS caching bug: pre-installs `aws_lc_rs` to force a
//! conflict, then asserts three sequential calls all return the cached `Err`.

use toolkit::bootstrap::{CryptoProviderError, init_crypto_provider};

#[test]
fn fips_conflict_is_cached_across_calls() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("test pre-condition: no provider should be installed yet");

    let r1 = init_crypto_provider();
    let r2 = init_crypto_provider();
    let r3 = init_crypto_provider();

    assert_eq!(r1, Err(CryptoProviderError::FipsProviderConflict));
    assert_eq!(r1, r2);
    assert_eq!(r2, r3);
}
