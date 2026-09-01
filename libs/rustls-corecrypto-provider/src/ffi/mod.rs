//! Foreign function interface bindings to Apple's user-space crypto APIs.
//!
//! `security-framework` covers high-level objects (`SecKey`, `SecRandom`) but
//! does not expose symmetric primitives. `CommonCrypto` is the canonical
//! lower-level user-space C API that, on macOS, terminates inside the
//! FIPS-validated `libcorecrypto.dylib`.
//!
//! ## Linking
//!
//! - `commoncrypto` symbols (`CC_SHA256_*`, `CCHmac*`, `CCCryptor*`) live in
//!   `libSystem.dylib`, which rustc auto-links on macOS. No explicit
//!   `#[link]` attribute is needed (the previous claim of one here was
//!   stale — corrected per security-review finding).
//! - `security` symbols (`SecKeyCreateWithData`, `kSecAttrKeyType*` statics,
//!   etc.) come from `Security.framework` and use an explicit
//!   `#[link(name = "Security", kind = "framework")]` in
//!   [`security.rs`](self::security).

pub mod commoncrypto;
pub mod security;
