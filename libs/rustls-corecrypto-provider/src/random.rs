//! `SecureRandom` implementation backed by `SecRandomCopyBytes` (kSecRandomDefault).
//!
//! Apple's documentation guarantees that this RNG is cryptographically secure
//! and, on macOS, terminates inside the FIPS-validated corecrypto module.

use rustls::crypto::{GetRandomFailed, SecureRandom};
use security_framework::random::SecRandom;

/// Singleton `SecureRandom` instance for the provider.
#[derive(Debug, Default)]
pub struct CoreCryptoRandom;

impl SecureRandom for CoreCryptoRandom {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        SecRandom::default()
            .copy_bytes(buf)
            .map_err(|_| GetRandomFailed)
    }

    fn fips(&self) -> bool {
        // Runtime witness — see [`crate::oe::fips_witness_ok`]. Returns
        // `false` if the running macOS major is outside the active Apple
        // corecrypto CMVP cert OE (no panic; downstream
        // `ClientConfig::fips()` reflects the witness, not design intent).
        crate::oe::fips_witness_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_fill_nonzero() {
        let mut buf = [0u8; 32];
        CoreCryptoRandom.fill(&mut buf).expect("fill");
        // Probabilistically impossible to be all zero for 32 bytes.
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn random_fill_empty() {
        // Edge case: SecRandomCopyBytes may misbehave on zero-length input.
        // Contract: must succeed without panicking and leave the buffer alone.
        let mut buf: [u8; 0] = [];
        CoreCryptoRandom.fill(&mut buf).expect("fill empty");
    }

    /// Two consecutive `fill` calls on the same instance must produce
    /// different outputs (probability ~1 for 32-byte buffers). Catches a
    /// hypothetical state regression where SecRandom returns a constant.
    /// Also exercises the FIPS-claim contract so downstream rustls
    /// `ClientConfig::fips()` remains true.
    #[test]
    fn random_fill_distinct_across_calls_and_advertises_fips() {
        let r = CoreCryptoRandom;
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        r.fill(&mut a).expect("fill a");
        r.fill(&mut b).expect("fill b");
        assert_ne!(a, b, "two consecutive fills must differ");
        assert!(r.fips(), "must advertise FIPS to rustls");
    }

    /// Large buffer (1 KiB, well over one syscall page boundary) must be
    /// fully filled — no zero tail from a truncated FFI call.
    #[test]
    fn random_fill_large_buffer_no_zero_tail() {
        let mut buf = vec![0u8; 1024];
        CoreCryptoRandom.fill(&mut buf).expect("fill 1 KiB");
        let zero_tail = buf.iter().rev().take_while(|&&b| b == 0).count();
        // A correct RNG output has ~1/256 chance of any tail byte being 0;
        // a 16-byte zero tail is ~2^-128 — effectively impossible.
        assert!(
            zero_tail < 16,
            "suspicious zero tail of {zero_tail} bytes — FFI may have truncated"
        );
    }
}
