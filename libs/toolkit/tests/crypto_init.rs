#![cfg(all(feature = "bootstrap", not(feature = "fips")))]

//! Non-FIPS smoke test: subsequent calls return the cached `Ok(())`.
//! Cached-`Err` regression is in `crypto_init_fips.rs`.

use toolkit::bootstrap::init_crypto_provider;

#[test]
fn second_call_returns_cached_result() {
    let r1 = init_crypto_provider();
    let r2 = init_crypto_provider();
    let r3 = init_crypto_provider();

    assert_eq!(r1, r2);
    assert_eq!(r2, r3);
    assert!(
        r1.is_ok(),
        "non-FIPS init should succeed in a fresh process"
    );
}
