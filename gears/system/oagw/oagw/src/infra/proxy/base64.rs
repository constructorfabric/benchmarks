//! Minimal base64 encoder (RFC 4648, standard alphabet, with padding).
//!
//! Implemented locally rather than pulled in as a dependency: the only
//! consumer is HTTP Basic credential encoding, whose input is a
//! `username:password` pair that must never appear in a log line or error.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `input` as standard base64 with padding.
#[must_use]
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = usize::from(chunk[0]);
        let b1 = usize::from(*chunk.get(1).unwrap_or(&0));
        let b2 = usize::from(*chunk.get(2).unwrap_or(&0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Encode `input` as unpadded base64 (base64url-style length, standard
/// alphabet); used where a fixed-length credential is not required.
#[must_use]
pub fn base64_encode_unpadded(input: &[u8]) -> String {
    let encoded = base64_encode(input);
    encoded.trim_end_matches('=').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_rfc_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn basic_credentials_round_trip() {
        assert_eq!(base64_encode(b"alice:s3cret"), "YWxpY2U6czNjcmV0");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn unpadded_drops_only_the_padding() {
        assert_eq!(base64_encode_unpadded(b"foo"), "Zm9v");
        assert_eq!(base64_encode_unpadded(b"fo"), "Zm8");
    }
}
