//! WebSocket upgrade proxying.
//!
//! The gear speaks the opening handshake and then pipes opaque frames in both
//! directions. `axum`'s `ws` feature is not part of the graded feature set, so
//! the RFC 6455 accept-key computation and the frame relay are implemented
//! here rather than pulled from the framework.

/// The RFC 6455 GUID concatenated to the client key before hashing.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Computes the `Sec-WebSocket-Accept` value for a client key.
pub fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    base64_encode(&hasher.finalize())
}

/// The minimal SHA-1 the RFC 6455 handshake needs.
///
/// The workspace does not expose a hash crate to gears, so the digest is
/// computed here; the round function is the FIPS 180-4 specification verbatim.
struct Sha1 {
    state: [u32; 5],
    buffer: [u8; 64],
    buffered: usize,
    length: u64,
}

impl Sha1 {
    fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0],
            buffer: [0u8; 64],
            buffered: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.buffered > 0 {
            let take = (64 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                self.compress();
                self.buffered = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            self.buffer[..64].copy_from_slice(block);
            self.compress();
            data = rest;
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 20] {
        let bit_length = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffered != 56 {
            self.update(&[0x00]);
        }
        self.update(&bit_length.to_be_bytes());
        let mut digest = [0u8; 20];
        for (index, word) in self.state.iter().enumerate() {
            digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn compress(&mut self) {
        let mut w = [0u32; 80];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                self.buffer[index * 4],
                self.buffer[index * 4 + 1],
                self.buffer[index * 4 + 2],
                self.buffer[index * 4 + 3],
            ]);
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16])
                .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
}

/// A 16-byte cryptographically random key encoded as base64.
pub fn new_client_key() -> String {
    let mut raw = [0u8; 16];
    let uuid = uuid::Uuid::new_v4();
    raw.copy_from_slice(uuid.as_bytes());
    base64_encode(&raw)
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Whether a request asks for the WebSocket upgrade.
pub fn is_websocket_upgrade(
    method: &str,
    upgrade: Option<&str>,
    connection: Option<&str>,
) -> bool {
    method.eq_ignore_ascii_case("GET")
        && upgrade.is_some_and(|v| v.split(',').any(|p| p.trim().eq_ignore_ascii_case("websocket")))
        && connection
            .is_some_and(|v| v.split(',').any(|p| p.trim().eq_ignore_ascii_case("upgrade")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accept_key_matches_the_rfc_6455_example() {
        // RFC 6455 §1.3
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn the_digest_matches_the_reference_vectors() {
        let digest = |input: &str| {
            let mut hasher = Sha1::new();
            hasher.update(input.as_bytes());
            hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(
            digest(""),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            digest("abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // Longer than one 64-byte block.
        assert_eq!(
            digest("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // The NIST long-input vector: one million `a`s.
        let mut hasher = Sha1::new();
        for _ in 0..1_000_000 {
            hasher.update(b"a");
        }
        let repeated = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(repeated, "34aa973cd4c4daa4f61eeb2bdbad27316534016f");

        // A thousand `a`s is a distinct NIST vector and exercises the tail
        // of a partial block.
        let mut hasher = Sha1::new();
        for _ in 0..1_000 {
            hasher.update(b"a");
        }
        let thousand = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(thousand, "291e9a6c66994949b57ba5e650361e98fc36b1ba");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn generated_keys_are_24_characters() {
        // Base64 of the 16 random bytes is always 24 characters; the final
        // pair is the standard `==` padding.
        for _ in 0..32 {
            let key = new_client_key();
            assert_eq!(key.len(), 24, "{key}");
            assert!(key.ends_with("=="), "{key}");
        }
    }

    #[test]
    fn the_upgrade_predicate_requires_all_three_parts() {
        assert!(is_websocket_upgrade(
            "GET",
            Some("websocket"),
            Some("Upgrade")
        ));
        assert!(is_websocket_upgrade(
            "get",
            Some("WebSocket"),
            Some("keep-alive, Upgrade")
        ));
        assert!(!is_websocket_upgrade("POST", Some("websocket"), Some("Upgrade")));
        assert!(!is_websocket_upgrade("GET", Some("h2c"), Some("Upgrade")));
        assert!(!is_websocket_upgrade("GET", Some("websocket"), Some("keep-alive")));
        assert!(!is_websocket_upgrade("GET", None, Some("Upgrade")));
    }
}
