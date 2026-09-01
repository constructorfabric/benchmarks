//! Alias derivation, normalization and validation.
//!
//! Implements DESIGN.md §3.2 "Alias Resolution / Alias Enforcement Rules":
//! auto-derivation from hostname endpoints (single hostname, or a pool of
//! hostnames sharing a PSL-validated registrable common suffix), explicit
//! aliases for IP / non-derivable pools, RFC 1123 hostname validation,
//! standard-port elision and ASCII lowercase normalization.

/// Standard port used to decide whether the `:port` is elided from a derived
/// alias (HTTP 80, HTTPS/WSS/WebTransport/gRPC 443) and the default port for
/// an endpoint that omits `port`.
#[must_use]
pub fn default_port_for_scheme(scheme: &str) -> u16 {
    match scheme {
        "http" | "ws" => 80,
        _ => 443,
    }
}

/// Whether `port` is the scheme's standard port (elided from derived alias).
#[must_use]
pub fn is_standard_port(scheme: &str, port: u16) -> bool {
    default_port_for_scheme(scheme) == port
}

/// Normalize an alias: trim, ASCII-lowercase, strip trailing dots.
#[must_use]
pub fn normalize_alias(input: &str) -> String {
    let lower = input.trim().to_ascii_lowercase();
    lower.trim_end_matches('.').to_owned()
}

/// Is `host` an IP literal (v4 or v6)?
#[must_use]
pub fn is_ip_address(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Validate a hostname per RFC 1123: max 253 chars, each label 1–63 chars,
/// labels contain only ASCII alphanumerics and hyphens and cannot start or
/// end with a hyphen.  A trailing dot (FQDN notation) is tolerated and
/// stripped.  IP literals are valid hosts.
pub fn is_valid_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if is_ip_address(host) {
        return true;
    }
    host.split('.').all(is_valid_label)
}

fn is_valid_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    let bytes = label.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// Longest common suffix of dot-separated labels across all hosts
/// (e.g. `us.vendor.com` + `eu.vendor.com` → `["vendor", "com"]`).
fn common_label_suffix(hosts: &[String]) -> Option<Vec<String>> {
    let label_sets: Vec<Vec<String>> = hosts
        .iter()
        .map(|h| h.split('.').map(str::to_owned).collect::<Vec<_>>())
        .collect();
    if label_sets.is_empty() {
        return None;
    }
    let min_len = label_sets.iter().map(Vec::len).min()?;
    let mut common: Vec<String> = Vec::new();
    for i in 0..min_len {
        let idx = label_sets[0].len() - 1 - i;
        let label = &label_sets[0][idx];
        let all_match = label_sets
            .iter()
            .all(|l| l.get(l.len() - 1 - i).is_some_and(|x| x == label));
        if all_match {
            common.insert(0, label.clone());
        } else {
            break;
        }
    }
    if common.is_empty() {
        None
    } else {
        Some(common)
    }
}

/// Compute the derived alias for a set of `(scheme, host, port)` endpoints.
///
/// Returns `None` (not derivable, explicit alias required) when:
/// * the pool is empty,
/// * any endpoint is an IP literal,
/// * endpoints use mixed ports, or
/// * the pool's only common suffix is a bare public suffix / not a
///   registrable domain (PSL-validated, ≥2 labels).
///
/// A derived alias omits the standard port and preserves a `:port` suffix for
/// non-standard ports.
#[must_use]
pub fn compute_derived_alias(endpoints: &[(String, String, u16)]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    let scheme = &endpoints[0].0;
    let ports: Vec<u16> = endpoints.iter().map(|e| e.2).collect();
    if ports.iter().any(|p| *p != ports[0]) {
        return None;
    }
    let port = ports[0];

    let hosts: Vec<String> = endpoints.iter().map(|e| normalize_alias(&e.1)).collect();

    if hosts.iter().any(|h| is_ip_address(h)) {
        return None;
    }
    if !hosts.iter().all(|h| is_valid_host(h)) {
        return None;
    }

    if hosts.len() == 1 {
        let mut alias = hosts[0].clone();
        if !is_standard_port(scheme, port) {
            alias = format!("{alias}:{port}");
        }
        return Some(alias);
    }

    let common = common_label_suffix(&hosts)?;
    if common.len() < 2 {
        return None;
    }
    let suffix = common.join(".");

    // The shared suffix must be a registrable domain, not a bare public
    // suffix (e.g. `co.uk`) and not an unknown suffix (`psl::domain_str`
    // returns None for both cases).
    match psl::domain_str(&suffix) {
        Some(registrable) if registrable.eq_ignore_ascii_case(&suffix) => {}
        _ => return None,
    }

    let mut alias = suffix;
    if !is_standard_port(scheme, port) {
        alias = format!("{alias}:{port}");
    }
    Some(alias)
}

/// Whether a user-supplied alias string is well-formed for storage: ASCII
/// lowercase, alphanumeric start/end, and only `[a-z0-9:.-]` in between
/// (mirrors the schema's alias pattern with the `:port` form allowed).
#[must_use]
pub fn is_valid_alias_input(input: &str) -> bool {
    let normalized = normalize_alias(input);
    if normalized.is_empty() || normalized.len() > 253 {
        return false;
    }
    let bytes = normalized.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b':' || *b == b'.' || *b == b'-')
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // ---- single hostname derivation ------------------------------------

    #[test]
    fn single_hostname_standard_port() {
        assert_eq!(
            compute_derived_alias(&[("https".into(), "api.openai.com".into(), 443)]),
            Some("api.openai.com".into())
        );
        assert_eq!(
            compute_derived_alias(&[("http".into(), "api.example.com".into(), 80)]),
            Some("api.example.com".into())
        );
    }

    #[test]
    fn single_hostname_non_standard_port_keeps_port() {
        assert_eq!(
            compute_derived_alias(&[("https".into(), "api.example.com".into(), 8443)]),
            Some("api.example.com:8443".into())
        );
    }

    #[test]
    fn single_hostname_mixed_case_and_trailing_dot_normalized() {
        assert_eq!(
            compute_derived_alias(&[("https".into(), "Api.OpenAI.COM.".into(), 443)]),
            Some("api.openai.com".into())
        );
    }

    // ---- multi-hostname common suffix ----------------------------------

    #[test]
    fn multi_hostname_registrable_common_suffix() {
        assert_eq!(
            compute_derived_alias(&[
                ("https".into(), "us.vendor.com".into(), 443),
                ("https".into(), "eu.vendor.com".into(), 443),
            ]),
            Some("vendor.com".into())
        );
    }

    #[test]
    fn multi_hostname_common_suffix_keeps_non_standard_port() {
        assert_eq!(
            compute_derived_alias(&[
                ("https".into(), "us.vendor.com".into(), 8443),
                ("https".into(), "eu.vendor.com".into(), 8443),
            ]),
            Some("vendor.com:8443".into())
        );
    }

    #[test]
    fn multi_hostname_deeper_inner_suffix() {
        assert_eq!(
            compute_derived_alias(&[
                ("https".into(), "a.foo.co.uk".into(), 443),
                ("https".into(), "b.foo.co.uk".into(), 443),
            ]),
            Some("foo.co.uk".into())
        );
    }

    #[test]
    fn multi_hostname_bare_public_suffix_rejected() {
        // `co.uk` is a bare public suffix → not derivable.
        assert_eq!(
            compute_derived_alias(&[
                ("https".into(), "foo.co.uk".into(), 443),
                ("https".into(), "bar.co.uk".into(), 443),
            ]),
            None
        );
    }

    #[test]
    fn multi_hostname_no_common_suffix_rejected() {
        assert_eq!(
            compute_derived_alias(&[
                ("https".into(), "us.foo.com".into(), 443),
                ("https".into(), "eu.bar.com".into(), 443),
            ]),
            None
        );
    }

    #[test]
    fn multi_hostname_unknown_suffix_rejected() {
        assert_eq!(
            compute_derived_alias(&[
                ("https".into(), "s1.internal".into(), 443),
                ("https".into(), "s2.internal".into(), 443),
            ]),
            None
        );
    }

    // ---- IP hosts --------------------------------------------------------

    #[test]
    fn ip_address_not_derivable() {
        assert_eq!(
            compute_derived_alias(&[("http".into(), "10.0.1.1".into(), 8080)]),
            None
        );
        assert_eq!(
            compute_derived_alias(&[
                ("http".into(), "10.0.1.1".into(), 8080),
                ("http".into(), "10.0.1.2".into(), 8080),
            ]),
            None
        );
        assert_eq!(
            compute_derived_alias(&[("http".into(), "::1".into(), 8080)]),
            None
        );
    }

    // ---- mixed constraints ----------------------------------------------

    #[test]
    fn mixed_ports_not_derivable() {
        assert_eq!(
            compute_derived_alias(&[
                ("https".into(), "a.example.com".into(), 443),
                ("https".into(), "b.example.com".into(), 444),
            ]),
            None
        );
    }

    #[test]
    fn empty_pool_not_derivable() {
        assert_eq!(compute_derived_alias(&[]), None);
    }

    // ---- hostname validation --------------------------------------------

    #[test]
    fn hostname_validation_accepts_valid_forms() {
        assert!(is_valid_host("api.example.com"));
        assert!(is_valid_host("localhost"));
        assert!(is_valid_host("example.")); // trailing dot tolerated
        assert!(is_valid_host("10.0.1.1")); // IP literal
        assert!(!is_valid_host("a-b_c.example")); // underscore not in RFC 1123 labels
        assert!(!is_valid_host("")); // empty in middle? no — blank
    }

    #[test]
    fn hostname_validation_rejects_invalid_forms() {
        assert!(!is_valid_host("exa mple.com"));
        assert!(!is_valid_host("-bad.example.com"));
        assert!(!is_valid_host("bad-.example.com"));
        assert!(!is_valid_host("example..com"));
        assert!(!is_valid_host(&("a".repeat(64) + ".com")));
        assert!(!is_valid_host(&(".".to_owned() + &"a".repeat(253))));
    }

    // ---- alias input validation -----------------------------------------

    #[test]
    fn alias_input_validation() {
        assert!(is_valid_alias_input("my-service"));
        assert!(is_valid_alias_input("api.openai.com:8443"));
        assert!(is_valid_alias_input("Api.OPENAI.com"));
        assert!(is_valid_alias_input("vendor.com"));
        assert!(!is_valid_alias_input("bad alias"));
        assert!(!is_valid_alias_input("bad/alias"));
        assert!(!is_valid_alias_input(""));
    }

    // ---- default ports ---------------------------------------------------

    #[test]
    fn default_and_standard_ports() {
        assert_eq!(default_port_for_scheme("http"), 80);
        assert_eq!(default_port_for_scheme("ws"), 80);
        assert_eq!(default_port_for_scheme("https"), 443);
        assert_eq!(default_port_for_scheme("wss"), 443);
        assert_eq!(default_port_for_scheme("wt"), 443);
        assert_eq!(default_port_for_scheme("grpc"), 443);
        assert!(is_standard_port("https", 443));
        assert!(!is_standard_port("https", 8443));
        assert!(is_standard_port("http", 80));
    }
}
