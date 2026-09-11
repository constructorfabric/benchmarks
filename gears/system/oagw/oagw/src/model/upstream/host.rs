//! Endpoint `host` classification and RFC 1123 hostname validation, shared
//! by `cpt-cf-oagw-algo-validate-upstream-schema` (host-format checking) and
//! `cpt-cf-oagw-algo-derive-alias` (hostname-vs-IP classification).

/// The result of classifying one endpoint's `host` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostClass {
    /// A valid IPv4 or IPv6 literal.
    Ip,
    /// A valid RFC 1123 hostname, normalized (ASCII-lowercased, trailing
    /// dot stripped).
    Hostname(String),
}

/// Classify `host` as an IP literal or a normalized hostname, or return
/// `None` when it is neither (used by
/// `cpt-cf-oagw-algo-validate-upstream-schema`'s `inst-validate-host-format`
/// step to reject the endpoint).
#[must_use]
pub fn classify_host(host: &str) -> Option<HostClass> {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Some(HostClass::Ip);
    }
    if is_valid_hostname(host) {
        let stripped = host.strip_suffix('.').unwrap_or(host);
        return Some(HostClass::Hostname(stripped.to_ascii_lowercase()));
    }
    None
}

/// RFC 1123: max 253 characters total (after stripping a tolerated trailing
/// FQDN dot), each label 1-63 characters, labels contain only ASCII
/// alphanumerics and hyphens, and never start or end with a hyphen.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.is_empty() || trimmed.len() > 253 {
        return false;
    }
    trimmed.split('.').all(is_valid_label)
}

fn is_valid_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ipv4_and_ipv6_literals_as_ip() {
        assert_eq!(classify_host("10.0.1.1"), Some(HostClass::Ip));
        assert_eq!(classify_host("::1"), Some(HostClass::Ip));
    }

    #[test]
    fn classifies_and_normalizes_a_valid_hostname() {
        assert_eq!(
            classify_host("API.Example.COM."),
            Some(HostClass::Hostname("api.example.com".to_owned()))
        );
    }

    #[test]
    fn rejects_a_label_that_starts_or_ends_with_a_hyphen() {
        assert_eq!(classify_host("-bad.example.com"), None);
        assert_eq!(classify_host("bad-.example.com"), None);
    }

    #[test]
    fn rejects_an_empty_label() {
        assert_eq!(classify_host("bad..example.com"), None);
    }

    #[test]
    fn rejects_a_hostname_longer_than_253_characters() {
        let long_label = "a".repeat(63);
        let long_host = vec![long_label; 5].join(".");
        assert!(long_host.len() > 253);
        assert_eq!(classify_host(&long_host), None);
    }

    #[test]
    fn rejects_a_label_longer_than_63_characters() {
        let host = format!("{}.example.com", "a".repeat(64));
        assert_eq!(classify_host(&host), None);
    }
}
