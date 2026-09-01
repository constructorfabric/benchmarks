//! `rustls::crypto::CryptoProvider` backed by Apple corecrypto via
//! Security.framework and CommonCrypto.
//!
//! See `README.md` for scope and compliance notes.
//!
//! ## Architecture
//!
//! - [`ffi`] — raw `extern "C"` declarations for CommonCrypto.
//! - [`hash`] — SHA-2 implementations (`Hash` trait).
//! - [`hmac`] — HMAC-SHA-2 (`Hmac` trait).
//! - [`hkdf`] — HKDF on top of HMAC (`tls13::Hkdf` trait).
//! - [`aead`] — AES-128/256-GCM (`Tls13AeadAlgorithm` / `Tls12AeadAlgorithm`).
//! - [`random`] — `SecureRandom` via `SecRandomCopyBytes`.
//! - [`kx`] — `SupportedKxGroup` for P-256 / P-384.
//! - [`verify`] — signature verification algorithms.
//! - [`signer`] — `KeyProvider` for server-side TLS / mTLS, routing RSA
//!   (PSS + PKCS#1 v1.5, SHA-256/384/512) and ECDSA (P-256/P-384/P-521)
//!   private-key signing through Apple corecrypto. See ADR 0004.
//! - [`tls13`], [`tls12`] — cipher suite registrations.
//! - [`provider`] — assembly of the final `CryptoProvider`.
//!
//! The entire crate is gated on `cfg(target_os = "macos")`. On other
//! platforms the public surface is empty.

#![cfg(target_os = "macos")]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod ffi;

pub mod aead;
pub mod hash;
pub mod hkdf;
pub mod hmac;
pub mod kx;
pub mod oe;
pub mod provider;
pub mod random;
pub mod signer;
pub mod tls12;
pub mod tls13;
pub mod verify;

pub use oe::{OeError, SUPPORTED_OE_MACOS_MAJOR, validate_oe};
pub use provider::{default_provider, default_provider_arc, fips_provider, fips_provider_arc};

/// Compile-time guard that every public type rustls might hand across
/// threads is in fact `Send + Sync`. A regression that removed `unsafe
/// impl Send for SecKey` upstream would surface here as a compile error
/// (much louder than a runtime crash mid-handshake).
///
/// Has no runtime effect — `#[cfg(test)]` keeps it out of the release
/// binary; the `#[allow(dead_code)]` silences the never-called warning.
#[cfg(test)]
#[allow(dead_code)]
fn _assert_thread_safety() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}

    // rustls hands these across threads via `Arc` and stores them on
    // shared `ClientConfig`/`ServerConfig`.
    assert_send_sync::<random::CoreCryptoRandom>();
    assert_send_sync::<signer::CoreCryptoKeyProvider>();
    assert_send_sync::<kx::P256KxGroup>();
    assert_send_sync::<kx::P384KxGroup>();

    // `CryptoProvider` itself is `Arc<CryptoProvider>` after
    // `install_default()` — it must be Send + Sync.
    assert_send_sync::<rustls::crypto::CryptoProvider>();

    // `SigningKey` and `Signer` from rustls are trait objects with
    // `Send + Sync` super-bounds; we hand them out via `Arc<dyn ...>`.
    fn _assert_dyn() {
        fn assert_dyn_send_sync<T: ?Sized + Send + Sync + 'static>() {}
        assert_dyn_send_sync::<dyn rustls::sign::SigningKey>();
        assert_dyn_send_sync::<dyn rustls::sign::Signer>();
        assert_dyn_send_sync::<dyn rustls::crypto::ActiveKeyExchange>();
    }
}
