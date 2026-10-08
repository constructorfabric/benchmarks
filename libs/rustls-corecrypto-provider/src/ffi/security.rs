//! FFI bindings to Security.framework for operations not exposed by the
//! `security-framework` 3.7 safe API.
//!
//! Currently this is `SecKeyCreateWithData`, used both to:
//!
//! - **Import a peer's public key** from raw bytes received over the wire
//!   during a TLS handshake (EC uncompressed point or PKCS#1 RSAPublicKey
//!   DER). See [`import_public_key`].
//! - **Import a local private key** for server-side TLS / mTLS, in the
//!   formats Apple's `SecKeyCreateWithData` accepts directly: PKCS#1
//!   `RSAPrivateKey` DER for RSA, ANSI X9.63 `0x04 || X || Y || k` blob for
//!   EC. PKCS#8 wrapping is NOT accepted by Apple — callers MUST unwrap
//!   before calling [`import_private_key`]. See `crate::signer`.

#![allow(non_upper_case_globals, non_snake_case)]

use core_foundation::base::{CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::error::{CFError, CFErrorRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::data::CFDataRef;
use core_foundation_sys::dictionary::CFDictionaryRef;
use security_framework::key::SecKey;
use security_framework_sys::base::SecKeyRef;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    /// Imports a key from external data. The returned `SecKeyRef` follows
    /// the Create Rule (owned, must be released by the receiver).
    fn SecKeyCreateWithData(
        keyData: CFDataRef,
        attributes: CFDictionaryRef,
        error: *mut CFErrorRef,
    ) -> SecKeyRef;

    /// Returns the block length of the key in bytes. For RSA this equals
    /// the modulus size in bytes (256 for RSA-2048, 384 for RSA-3072, ...).
    /// For EC this equals the coordinate length.
    fn SecKeyGetBlockSize(key: SecKeyRef) -> usize;

    // Attribute key constants used as CFDictionary keys.
    pub static kSecAttrKeyType: CFStringRef;
    pub static kSecAttrKeyClass: CFStringRef;
    pub static kSecAttrKeySizeInBits: CFStringRef;

    // KeyType values.
    pub static kSecAttrKeyTypeRSA: CFStringRef;
    pub static kSecAttrKeyTypeECSECPrimeRandom: CFStringRef;

    // KeyClass values.
    pub static kSecAttrKeyClassPublic: CFStringRef;
    pub static kSecAttrKeyClassPrivate: CFStringRef;
}

/// Returns the block length of a key in bytes. Thin safe wrapper over
/// `SecKeyGetBlockSize` — used for RSA modulus-size enforcement.
pub fn seckey_block_size(key: &SecKey) -> usize {
    // SAFETY: `key.as_concrete_TypeRef()` returns a valid live SecKeyRef
    // owned by `key`; SecKeyGetBlockSize is documented to never fail and
    // returns 0 only for opaque-block-size keys (not RSA/EC).
    unsafe { SecKeyGetBlockSize(key.as_concrete_TypeRef()) }
}

/// Curve / algorithm hint for `import_public_key`.
#[derive(Copy, Clone, Debug)]
pub enum PublicKeyKind {
    /// Uncompressed NIST P-256 point: 0x04 || X(32) || Y(32) = 65 bytes.
    EcSecPrimeRandomP256,
    /// Uncompressed NIST P-384 point: 0x04 || X(48) || Y(48) = 97 bytes.
    EcSecPrimeRandomP384,
    /// Uncompressed NIST P-521 point: 0x04 || X(66) || Y(66) = 133 bytes.
    EcSecPrimeRandomP521,
    /// PKCS#1 RSAPublicKey DER (SEQUENCE { modulus, publicExponent }).
    RsaPkcs1,
}

/// Curve / algorithm hint for `import_private_key`.
///
/// Apple's `SecKeyCreateWithData` requires a specific raw blob format per
/// key type — PKCS#8 wrappers are NOT accepted and must be stripped by the
/// caller (see `crate::signer`):
///
/// - **EC**: ANSI X9.63 raw `0x04 || X || Y || k` (uncompressed public
///   point followed by the private scalar). Length: 1 + 3 * `coord_bytes`.
/// - **RSA**: PKCS#1 `RSAPrivateKey` DER — the inner SEQUENCE without any
///   PKCS#8 outer wrapper.
#[derive(Copy, Clone, Debug)]
pub enum PrivateKeyKind {
    /// X9.63 raw blob for NIST P-256 = 1 + 3 * 32 = 97 bytes.
    EcSecPrimeRandomP256,
    /// X9.63 raw blob for NIST P-384 = 1 + 3 * 48 = 145 bytes.
    EcSecPrimeRandomP384,
    /// X9.63 raw blob for NIST P-521 = 1 + 3 * 66 = 199 bytes.
    EcSecPrimeRandomP521,
    /// PKCS#1 `RSAPrivateKey` DER (NOT PKCS#8-wrapped).
    RsaPkcs1,
}

/// Error returned when public-key import fails.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("SecKeyCreateWithData returned null without an error")]
    NullKey,
    #[error("SecKeyCreateWithData failed: {0}")]
    CoreFoundation(String),
}

/// Wrap an attribute key (`extern static CFStringRef`) into a safe CFString
/// borrow without consuming retain count. The static lives forever, so
/// `wrap_under_get_rule` is the correct retain pattern.
fn wrap_static_str(s: CFStringRef) -> CFString {
    // SAFETY: `s` is a non-null framework-managed static `CFStringRef`.
    unsafe { TCFType::wrap_under_get_rule(s) }
}

/// Import a raw public-key blob into a [`SecKey`].
///
/// On success the returned key is FIPS-ready (the same module Apple ships)
/// and can be passed to `SecKey::key_exchange` / `verify_signature`.
pub fn import_public_key(bytes: &[u8], kind: PublicKeyKind) -> Result<SecKey, ImportError> {
    let (type_static, size_bits) = match kind {
        PublicKeyKind::EcSecPrimeRandomP256 => {
            // SAFETY: extern statics are framework-managed, non-null.
            (unsafe { kSecAttrKeyTypeECSECPrimeRandom }, 256i64)
        }
        PublicKeyKind::EcSecPrimeRandomP384 => (unsafe { kSecAttrKeyTypeECSECPrimeRandom }, 384i64),
        PublicKeyKind::EcSecPrimeRandomP521 => (unsafe { kSecAttrKeyTypeECSECPrimeRandom }, 521i64),
        PublicKeyKind::RsaPkcs1 => {
            // RSA size is encoded in the modulus length; we still set the
            // attribute as a documentation hint (Security.framework infers
            // it from the data).
            (unsafe { kSecAttrKeyTypeRSA }, 0i64)
        }
    };

    create_seckey(
        bytes,
        type_static,
        unsafe { kSecAttrKeyClassPublic },
        size_bits,
    )
}

/// Import a raw private-key blob into a [`SecKey`].
///
/// For EC: the input MUST be Apple's X9.63 raw format
/// (`0x04 || X || Y || k`). For RSA: the input MUST be PKCS#1
/// `RSAPrivateKey` DER (NOT PKCS#8-wrapped). Callers convert their
/// `PrivateKeyDer` shapes via `crate::signer` before calling here.
///
/// On success the returned key is FIPS-ready (corecrypto-backed) and can
/// be passed to `SecKey::create_signature`.
pub fn import_private_key(bytes: &[u8], kind: PrivateKeyKind) -> Result<SecKey, ImportError> {
    let (type_static, size_bits) = match kind {
        PrivateKeyKind::EcSecPrimeRandomP256 => {
            // SAFETY: extern statics are framework-managed, non-null.
            (unsafe { kSecAttrKeyTypeECSECPrimeRandom }, 256i64)
        }
        PrivateKeyKind::EcSecPrimeRandomP384 => {
            (unsafe { kSecAttrKeyTypeECSECPrimeRandom }, 384i64)
        }
        PrivateKeyKind::EcSecPrimeRandomP521 => {
            (unsafe { kSecAttrKeyTypeECSECPrimeRandom }, 521i64)
        }
        PrivateKeyKind::RsaPkcs1 => (unsafe { kSecAttrKeyTypeRSA }, 0i64),
    };

    create_seckey(
        bytes,
        type_static,
        unsafe { kSecAttrKeyClassPrivate },
        size_bits,
    )
}

/// Shared `SecKeyCreateWithData` call — public/private vary only in
/// `kSecAttrKeyClass`. Extracted so both helpers above share the dictionary
/// construction + CFError-mapping path.
fn create_seckey(
    bytes: &[u8],
    key_type_static: CFStringRef,
    key_class_static: CFStringRef,
    size_bits: i64,
) -> Result<SecKey, ImportError> {
    let key_type_key = wrap_static_str(unsafe { kSecAttrKeyType });
    let key_class_key = wrap_static_str(unsafe { kSecAttrKeyClass });
    let key_size_key = wrap_static_str(unsafe { kSecAttrKeySizeInBits });
    let key_type_val = wrap_static_str(key_type_static);
    let key_class_val = wrap_static_str(key_class_static);

    let mut pairs: Vec<(CFString, CFType)> = vec![
        (key_type_key, key_type_val.as_CFType()),
        (key_class_key, key_class_val.as_CFType()),
    ];
    if size_bits > 0 {
        pairs.push((key_size_key, CFNumber::from(size_bits).as_CFType()));
    }

    let attrs = CFDictionary::from_CFType_pairs(&pairs);
    let data = CFData::from_buffer(bytes);
    let mut error_ref: CFErrorRef = std::ptr::null_mut();

    // SAFETY: `data` and `attrs` are valid CF objects; we pass `error_ref` as
    // a mutable out-pointer. Whatever is returned is per the Create Rule.
    let key_ref = unsafe {
        SecKeyCreateWithData(
            data.as_concrete_TypeRef(),
            attrs.as_concrete_TypeRef(),
            &mut error_ref,
        )
    };

    if key_ref.is_null() {
        if !error_ref.is_null() {
            // SAFETY: error_ref points to a CFErrorRef owned by us (the
            // function fills it on failure per Apple convention).
            //
            // We deliberately format only the *structured* error fields
            // (domain + code), NOT the Debug or localized-description.
            // The Debug impl in core-foundation 0.10 includes the
            // localized description, which is owned by Apple and
            // historically has contained file paths / object IDs; even
            // though it has never been observed to contain key material,
            // we avoid the surface entirely.
            let err = unsafe { CFError::wrap_under_create_rule(error_ref) };
            return Err(ImportError::CoreFoundation(format!(
                "domain={} code={}",
                err.domain(),
                err.code()
            )));
        }
        return Err(ImportError::NullKey);
    }

    // SAFETY: SecKeyCreateWithData transfers ownership; SecKey wraps under
    // the Create Rule and will release on drop.
    Ok(unsafe { SecKey::wrap_under_create_rule(key_ref) })
}
