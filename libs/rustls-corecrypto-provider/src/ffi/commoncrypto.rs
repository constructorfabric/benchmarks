//! FFI bindings to Apple's CommonCrypto library (`<CommonCrypto/CommonCrypto.h>`).
//!
//! CommonCrypto is shipped as part of `libSystem` and re-exports the FIPS
//! 140-3 validated `corecrypto` gear. We bind only the primitives we need:
//! SHA-2 hashing, HMAC, and the modern `CCCryptor*` family in GCM mode.
//!
//! Constants and struct sizes mirror the headers shipped in the macOS SDK
//! (`/usr/include/CommonCrypto/`). Context sizes are slightly over-allocated
//! to be future-proof — CommonCrypto returns an opaque pointer or fills a
//! fixed-size struct; size is fixed across the supported macOS versions
//! (11+).

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use core::ffi::{c_int, c_void};

// =========================================================================
// CommonDigest — SHA-2
// =========================================================================
//
// `CC_LONG`   = `uint32_t`
// `CC_LONG64` = `uint64_t`
//
// SHA-256 context (`CC_SHA256_CTX`) layout per `<CommonCrypto/CommonDigest.h>`:
//   CC_LONG count[2]; CC_LONG hash[8]; CC_LONG wbuf[16];   // 26 × 4 = 104 B
//
// SHA-384 reuses `CC_SHA512_CTX`:
//   CC_LONG64 count[2]; CC_LONG64 hash[8]; CC_LONG64 wbuf[16]; // 26 × 8 = 208 B

pub const CC_SHA256_DIGEST_LENGTH: usize = 32;
pub const CC_SHA384_DIGEST_LENGTH: usize = 48;
pub const CC_SHA256_BLOCK_BYTES: usize = 64;
pub const CC_SHA384_BLOCK_BYTES: usize = 128;

// `Clone` is sound because the context is a plain POD struct with no
// resources/handles: copying the bytes copies the entire hash state.
#[repr(C)]
#[derive(Clone)]
pub struct CC_SHA256_CTX {
    pub count: [u32; 2],
    pub hash: [u32; 8],
    pub wbuf: [u32; 16],
}

#[repr(C)]
#[derive(Clone)]
pub struct CC_SHA512_CTX {
    pub count: [u64; 2],
    pub hash: [u64; 8],
    pub wbuf: [u64; 16],
}

// Compile-time guards pinning our hand-mirrored context layouts to the
// sizes documented in the macOS SDK `<CommonCrypto/CommonDigest.h>`
// headers. If a future Apple SDK update changes the struct layout these
// `assert!`s fail the build instead of silently corrupting hash state
// at runtime. Test-gap #8 from the security review.
const _: () = assert!(core::mem::size_of::<CC_SHA256_CTX>() == 104);
const _: () = assert!(core::mem::size_of::<CC_SHA512_CTX>() == 208);

unsafe extern "C" {
    pub fn CC_SHA256_Init(c: *mut CC_SHA256_CTX) -> c_int;
    pub fn CC_SHA256_Update(c: *mut CC_SHA256_CTX, data: *const c_void, len: u32) -> c_int;
    pub fn CC_SHA256_Final(md: *mut u8, c: *mut CC_SHA256_CTX) -> c_int;
    pub fn CC_SHA256(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;

    pub fn CC_SHA384_Init(c: *mut CC_SHA512_CTX) -> c_int;
    pub fn CC_SHA384_Update(c: *mut CC_SHA512_CTX, data: *const c_void, len: u32) -> c_int;
    pub fn CC_SHA384_Final(md: *mut u8, c: *mut CC_SHA512_CTX) -> c_int;
    pub fn CC_SHA384(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
}

// =========================================================================
// CommonHMAC
// =========================================================================
//
// `CCHmacContext` is documented as opaque; the SDK reserves 96 × `uint32_t`
// = 384 bytes. We use the same layout.

pub type CCHmacAlgorithm = u32;
pub const kCCHmacAlgSHA1: CCHmacAlgorithm = 0;
pub const kCCHmacAlgMD5: CCHmacAlgorithm = 1;
pub const kCCHmacAlgSHA256: CCHmacAlgorithm = 2;
pub const kCCHmacAlgSHA384: CCHmacAlgorithm = 3;
pub const kCCHmacAlgSHA512: CCHmacAlgorithm = 4;
pub const kCCHmacAlgSHA224: CCHmacAlgorithm = 5;

// `Clone` is sound: the context is a fixed-size POD with the key state
// baked in; copying its bytes copies the entire HMAC state.
#[repr(C)]
#[derive(Clone)]
pub struct CCHmacContext {
    pub ctx: [u32; 96],
}

// Same SDK-layout guard as the digest contexts above (test-gap #8).
const _: () = assert!(core::mem::size_of::<CCHmacContext>() == 384);

unsafe extern "C" {
    pub fn CCHmacInit(
        ctx: *mut CCHmacContext,
        algorithm: CCHmacAlgorithm,
        key: *const c_void,
        key_length: usize,
    );
    pub fn CCHmacUpdate(ctx: *mut CCHmacContext, data: *const c_void, data_length: usize);
    pub fn CCHmacFinal(ctx: *mut CCHmacContext, mac_out: *mut c_void);
    pub fn CCHmac(
        algorithm: CCHmacAlgorithm,
        key: *const c_void,
        key_length: usize,
        data: *const c_void,
        data_length: usize,
        mac_out: *mut c_void,
    );
}

// =========================================================================
// CommonCryptor — modern AES-GCM via CCCryptorCreateWithMode + parameters
// =========================================================================

pub type CCCryptorStatus = i32;
pub const kCCSuccess: CCCryptorStatus = 0;
pub const kCCParamError: CCCryptorStatus = -4300;
pub const kCCBufferTooSmall: CCCryptorStatus = -4301;
pub const kCCMemoryFailure: CCCryptorStatus = -4302;
pub const kCCAlignmentError: CCCryptorStatus = -4303;
pub const kCCDecodeError: CCCryptorStatus = -4304;
pub const kCCUnimplemented: CCCryptorStatus = -4305;
pub const kCCOverflow: CCCryptorStatus = -4306;
pub const kCCRNGFailure: CCCryptorStatus = -4307;
pub const kCCUnspecifiedError: CCCryptorStatus = -4308;
pub const kCCCallSequenceError: CCCryptorStatus = -4309;
pub const kCCKeySizeError: CCCryptorStatus = -4310;

pub type CCOperation = u32;
pub const kCCEncrypt: CCOperation = 0;
pub const kCCDecrypt: CCOperation = 1;

pub type CCMode = u32;
pub const kCCModeECB: CCMode = 1;
pub const kCCModeCBC: CCMode = 2;
pub const kCCModeCFB: CCMode = 3;
pub const kCCModeCTR: CCMode = 4;
pub const kCCModeOFB: CCMode = 7;
pub const kCCModeXTS: CCMode = 8;
pub const kCCModeRC4: CCMode = 9;
pub const kCCModeCFB8: CCMode = 10;
pub const kCCModeGCM: CCMode = 11;
pub const kCCModeCCM: CCMode = 12;

pub type CCAlgorithm = u32;
pub const kCCAlgorithmAES: CCAlgorithm = 0;
pub const kCCAlgorithmDES: CCAlgorithm = 1;
pub const kCCAlgorithm3DES: CCAlgorithm = 2;

pub type CCPadding = u32;
pub const ccNoPadding: CCPadding = 0;
pub const ccPKCS7Padding: CCPadding = 1;

pub type CCModeOptions = u32;

pub type CCParameter = c_int;
pub const kCCParameterIV: CCParameter = 0;
pub const kCCParameterAuthData: CCParameter = 1;
pub const kCCParameterMacSize: CCParameter = 2;
pub const kCCParameterDataSize: CCParameter = 3;
pub const kCCParameterAuthTag: CCParameter = 4;

/// Opaque handle returned by `CCCryptorCreateWithMode`. Treat as a pointer;
/// must be released with `CCCryptorRelease`.
#[repr(C)]
pub struct CCCryptor {
    _private: [u8; 0],
}
pub type CCCryptorRef = *mut CCCryptor;

unsafe extern "C" {
    pub fn CCCryptorCreateWithMode(
        op: CCOperation,
        mode: CCMode,
        alg: CCAlgorithm,
        padding: CCPadding,
        iv: *const c_void,
        key: *const c_void,
        key_length: usize,
        tweak: *const c_void,
        tweak_length: usize,
        num_rounds: c_int,
        options: CCModeOptions,
        cryptor_ref: *mut CCCryptorRef,
    ) -> CCCryptorStatus;

    pub fn CCCryptorAddParameter(
        cryptor_ref: CCCryptorRef,
        parameter: CCParameter,
        data: *const c_void,
        data_length: usize,
    ) -> CCCryptorStatus;

    pub fn CCCryptorGetParameter(
        cryptor_ref: CCCryptorRef,
        parameter: CCParameter,
        data: *mut c_void,
        data_length: *mut usize,
    ) -> CCCryptorStatus;

    pub fn CCCryptorUpdate(
        cryptor_ref: CCCryptorRef,
        data_in: *const c_void,
        data_in_length: usize,
        data_out: *mut c_void,
        data_out_available: usize,
        data_out_moved: *mut usize,
    ) -> CCCryptorStatus;

    pub fn CCCryptorFinal(
        cryptor_ref: CCCryptorRef,
        data_out: *mut c_void,
        data_out_available: usize,
        data_out_moved: *mut usize,
    ) -> CCCryptorStatus;

    pub fn CCCryptorRelease(cryptor_ref: CCCryptorRef) -> CCCryptorStatus;

    // -----------------------------------------------------------------
    // Deprecated-but-functional GCM-specific APIs.
    //
    // The modern `CCCryptorAddParameter(kCCParameterIV / kCCParameterAuthData)`
    // path returns kCCUnimplemented or kCCCallSequenceError on shipping
    // macOS builds (11–14 tested). The dedicated `CCCryptorGCM*` family is
    // marked deprecated by Apple since macOS 10.13 in favour of the
    // AddParameter path, but the symbols remain exported and behave
    // correctly. We use them and silence deprecation warnings.
    pub fn CCCryptorGCMAddIV(
        cryptor_ref: CCCryptorRef,
        iv: *const c_void,
        iv_size: usize,
    ) -> CCCryptorStatus;

    pub fn CCCryptorGCMaddAAD(
        cryptor_ref: CCCryptorRef,
        aad: *const c_void,
        aad_size: usize,
    ) -> CCCryptorStatus;

    pub fn CCCryptorGCMFinal(
        cryptor_ref: CCCryptorRef,
        tag_out: *mut c_void,
        tag_size: *mut usize,
    ) -> CCCryptorStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: SHA-256 of empty input — known answer
    /// `e3b0c442 98fc1c14 9afbf4c8 996fb924 27ae41e4 649b934c a495991b 7852b855`.
    #[test]
    fn cc_sha256_empty() {
        let mut out = [0u8; CC_SHA256_DIGEST_LENGTH];
        unsafe {
            let r = CC_SHA256(core::ptr::null(), 0, out.as_mut_ptr());
            assert_eq!(r, out.as_mut_ptr());
        }
        let expected: [u8; 32] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(out, expected);
    }

    /// Smoke: SHA-384 of empty input — known answer
    /// `38b060a751ac9638 4cd9327eb1b1e36a 21fdb71114be0743 4c0cc7bf63f6e1da
    ///  274edebfe76f65fb d51ad2f14898b95b`.
    #[test]
    fn cc_sha384_empty() {
        let mut out = [0u8; CC_SHA384_DIGEST_LENGTH];
        unsafe {
            let r = CC_SHA384(core::ptr::null(), 0, out.as_mut_ptr());
            assert_eq!(r, out.as_mut_ptr());
        }
        let expected: [u8; 48] = [
            0x38, 0xb0, 0x60, 0xa7, 0x51, 0xac, 0x96, 0x38, 0x4c, 0xd9, 0x32, 0x7e, 0xb1, 0xb1,
            0xe3, 0x6a, 0x21, 0xfd, 0xb7, 0x11, 0x14, 0xbe, 0x07, 0x43, 0x4c, 0x0c, 0xc7, 0xbf,
            0x63, 0xf6, 0xe1, 0xda, 0x27, 0x4e, 0xde, 0xbf, 0xe7, 0x6f, 0x65, 0xfb, 0xd5, 0x1a,
            0xd2, 0xf1, 0x48, 0x98, 0xb9, 0x5b,
        ];
        assert_eq!(out, expected);
    }

    /// HMAC-SHA-256 RFC 4231 test case 1: key=20×0x0b, data="Hi There"
    /// Expected: b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7
    #[test]
    fn cc_hmac_sha256_rfc4231_case1() {
        let key = [0x0bu8; 20];
        let data = b"Hi There";
        let mut mac = [0u8; 32];
        unsafe {
            CCHmac(
                kCCHmacAlgSHA256,
                key.as_ptr() as *const c_void,
                key.len(),
                data.as_ptr() as *const c_void,
                data.len(),
                mac.as_mut_ptr() as *mut c_void,
            );
        }
        let expected: [u8; 32] = [
            0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
            0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
            0x2e, 0x32, 0xcf, 0xf7,
        ];
        assert_eq!(mac, expected);
    }
}
