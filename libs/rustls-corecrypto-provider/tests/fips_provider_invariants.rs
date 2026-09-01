//! G-6 regression invariants for the FIPS-claim provider.
//!
//! These tests pin the wire-level shape of `fips_provider()`: AES-GCM only,
//! NIST P-curves only, TLS 1.3 only. A future refactor that accidentally
//! adds ChaCha20-Poly1305, X25519, MLKEM, ED25519, or TLS 1.2 fallback to
//! the FIPS path would be caught here at test time rather than at audit
//! time. See FIPS PRD §9 "Verification & regression gates".
//!
//! Scope: only our own custom provider (`cf-gears-rustls-corecrypto-provider`).
//! The Linux + Windows backends (`aws-lc-fips`, `rustls-cng-crypto`) are
//! CMVP-validated upstream and shape-guaranteed by their respective
//! maintainers — out of scope for this regression file.

#![cfg(target_os = "macos")]

use rustls::{CipherSuite, NamedGroup, SupportedCipherSuite};
use rustls_corecrypto_provider::fips_provider;

/// G-6.1 — only AES-GCM cipher suites. ChaCha20, CBC, CCM must never
/// appear in the FIPS provider's suite list.
#[test]
fn fips_provider_has_only_aes_gcm_cipher_suites() {
    let p = fips_provider();
    assert!(
        !p.cipher_suites.is_empty(),
        "fips_provider must expose at least one cipher suite"
    );
    for cs in &p.cipher_suites {
        let suite = cs.suite();
        assert!(
            matches!(
                suite,
                CipherSuite::TLS13_AES_128_GCM_SHA256 | CipherSuite::TLS13_AES_256_GCM_SHA384
            ),
            "non-Approved cipher suite {suite:?} leaked into fips_provider"
        );
    }
}

/// G-6.2 — only NIST P-curves in the FIPS provider's key-exchange
/// groups. X25519, secp521r1, and post-quantum hybrids are not in scope
/// for the current corecrypto wire profile.
#[test]
fn fips_provider_has_only_nist_kx_groups() {
    let p = fips_provider();
    assert!(!p.kx_groups.is_empty(), "kx_groups must not be empty");
    for kx in &p.kx_groups {
        let name = kx.name();
        assert!(
            matches!(name, NamedGroup::secp256r1 | NamedGroup::secp384r1),
            "non-Approved key-exchange group {name:?} leaked into fips_provider"
        );
    }
}

/// G-6.3 — every cipher suite in the FIPS provider is TLS 1.3. TLS 1.2
/// is excluded because Apple corecrypto does not expose a separately
/// CAVS-listed TLS PRF primitive on macOS (see PRD §3.5).
#[test]
fn fips_provider_has_only_tls13_protocol() {
    let p = fips_provider();
    for cs in &p.cipher_suites {
        assert!(
            matches!(cs, SupportedCipherSuite::Tls13(_)),
            "non-TLS-1.3 cipher suite {:?} in fips_provider — would invalidate the FIPS claim \
             via the TLS 1.2 PRF gap",
            cs.suite()
        );
    }
}

/// G-6.4 — `fips_provider().fips() == true` for the intent-of-design
/// claim that downstream `ClientConfig::fips()` / `ServerConfig::fips()`
/// rely on. Under `--features fips` the `default_provider()` is aliased
/// to `fips_provider()` and must inherit the same `fips() = true` claim.
#[test]
fn fips_provider_advertises_fips_true() {
    let p = fips_provider();
    assert!(p.fips(), "fips_provider().fips() must be true");

    #[cfg(feature = "fips")]
    {
        use rustls_corecrypto_provider::default_provider;
        assert!(
            default_provider().fips(),
            "under --features fips, default_provider must alias to fips_provider \
             and claim fips() = true"
        );
    }
}
