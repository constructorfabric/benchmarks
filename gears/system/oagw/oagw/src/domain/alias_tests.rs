//! Tests for alias derivation, normalization and validation.

use crate::domain::alias::{
    common_suffix, derive, is_standard_port, normalize, same_host, validate_alias,
    validate_host, validate_tag, with_port,
};

#[test]
fn a_single_hostname_endpoint_derives_the_hostname() {
    let endpoints = vec![("api.example.com".to_owned(), 443u16, "https")];
    assert_eq!(derive(&endpoints).as_deref(), Some("api.example.com"));
}

#[test]
fn a_non_standard_port_is_suffixed() {
    let endpoints = vec![("api.example.com".to_owned(), 8443u16, "https")];
    assert_eq!(
        derive(&endpoints).as_deref(),
        Some("api.example.com:8443")
    );
}

#[test]
fn a_standard_port_is_not_suffixed() {
    let endpoints = vec![("api.example.com".to_owned(), 80u16, "http")];
    assert_eq!(derive(&endpoints).as_deref(), Some("api.example.com"));
    let endpoints = vec![("api.example.com".to_owned(), 443u16, "https")];
    assert_eq!(derive(&endpoints).as_deref(), Some("api.example.com"));
    let endpoints = vec![("api.example.com".to_owned(), 80u16, "ws")];
    assert_eq!(derive(&endpoints).as_deref(), Some("api.example.com"));
    let endpoints = vec![("api.example.com".to_owned(), 443u16, "wss")];
    assert_eq!(derive(&endpoints).as_deref(), Some("api.example.com"));
}

#[test]
fn case_and_trailing_dot_are_normalized_away() {
    let endpoints = vec![("API.Example.COM.".to_owned(), 443u16, "https")];
    assert_eq!(derive(&endpoints).as_deref(), Some("api.example.com"));
}

#[test]
fn an_ip_literal_endpoint_is_not_derivable() {
    let endpoints = vec![("10.0.0.7".to_owned(), 443u16, "https")];
    assert_eq!(derive(&endpoints), None);
    let endpoints = vec![("::1".to_owned(), 443u16, "https")];
    assert_eq!(derive(&endpoints), None);
    let endpoints = vec![("[2001:db8::1]".to_owned(), 443u16, "https")];
    assert_eq!(derive(&endpoints), None);
}

#[test]
fn an_empty_endpoint_set_is_not_derivable() {
    assert_eq!(derive(&[]), None);
}

#[test]
fn a_multi_endpoint_pool_derives_the_longest_common_registrable_suffix() {
    let endpoints = vec![
        ("a.example.com".to_owned(), 443u16, "https"),
        ("b.example.com".to_owned(), 443u16, "https"),
        ("c.example.com".to_owned(), 443u16, "https"),
    ];
    assert_eq!(derive(&endpoints).as_deref(), Some("example.com"));
}

#[test]
fn a_deeper_common_suffix_wins_over_the_registrable_domain() {
    let endpoints = vec![
        ("one.pay.example.co".to_owned(), 443u16, "https"),
        ("two.pay.example.co".to_owned(), 443u16, "https"),
    ];
    assert_eq!(derive(&endpoints).as_deref(), Some("pay.example.co"));
}

#[test]
fn disjoint_hosts_are_not_derivable() {
    let endpoints = vec![
        ("one.example.com".to_owned(), 443u16, "https"),
        ("other.example.org".to_owned(), 443u16, "https"),
    ];
    assert_eq!(derive(&endpoints), None);
}

#[test]
fn a_bare_public_suffix_is_not_derivable() {
    let endpoints = vec![
        ("a.co.uk".to_owned(), 443u16, "https"),
        ("b.co.uk".to_owned(), 443u16, "https"),
    ];
    // `co.uk` is a public suffix, so the pool's common suffix carries no registrable
    // domain of its own.
    assert_eq!(derive(&endpoints), None);
}

#[test]
fn an_ip_literal_in_the_pool_blocks_derivation() {
    let endpoints = vec![
        ("a.example.com".to_owned(), 443u16, "https"),
        ("10.0.0.1".to_owned(), 443u16, "https"),
    ];
    assert_eq!(derive(&endpoints), None);
}

#[test]
fn normalization_lowercases_and_strips_the_trailing_dot() {
    assert_eq!(normalize("API.Example.COM."), "api.example.com");
    assert_eq!(normalize("  api.example.com  "), "api.example.com");
    assert_eq!(normalize("api.example.com"), "api.example.com");
}

#[test]
fn same_host_ignores_case_and_trailing_dot() {
    assert!(same_host("API.example.COM.", "api.example.com"));
    assert!(!same_host("api.example.com", "api2.example.com"));
}

#[test]
fn the_port_is_appended_only_when_it_is_non_standard() {
    assert_eq!(with_port("Example.COM", 443, "https"), "example.com");
    assert_eq!(with_port("Example.COM", 8443, "https"), "example.com:8443");
    assert_eq!(with_port("Example.COM", 80, "ws"), "example.com");
    assert_eq!(with_port("Example.COM", 443, "wt"), "example.com");
}

#[test]
fn standard_ports_are_per_scheme() {
    assert!(is_standard_port(80, "http"));
    assert!(is_standard_port(443, "https"));
    assert!(is_standard_port(80, "ws"));
    assert!(is_standard_port(443, "wss"));
    assert!(is_standard_port(443, "wt"));
    assert!(!is_standard_port(8080, "http"));
    assert!(!is_standard_port(80, "https"));
}

#[test]
fn the_longest_common_suffix_is_label_aligned() {
    assert_eq!(
        common_suffix("a.example.com", "b.example.com").as_deref(),
        Some("example.com")
    );
    // A raw substring is not a suffix: only the label both names share survives.
    assert_eq!(common_suffix("ample.com", "example.com").as_deref(), Some("com"));
    assert_eq!(
        common_suffix("x.pay.example.com", "y.pay.example.com").as_deref(),
        Some("pay.example.com")
    );
    assert_eq!(common_suffix("example.com", "example.org").as_deref(), None);
}

#[test]
fn an_explicit_alias_must_match_the_component_pattern() {
    assert!(validate_alias("example.com").is_ok());
    assert!(validate_alias("example.com:8443").is_ok());
    assert!(validate_alias("a-b.c.d").is_ok());
    assert!(validate_alias("payments").is_ok());

    assert!(validate_alias("").is_err());
    assert!(validate_alias(".example.com").is_err());
    assert!(validate_alias("example.com.").is_err());
    assert!(validate_alias("-example").is_err());
    assert!(validate_alias("example_").is_err());
    assert!(validate_alias("has space").is_err());
    assert!(validate_alias("UPPER").is_err());
    assert!(validate_alias("slash/ed").is_err());
}

#[test]
fn tags_are_lower_case_words() {
    assert!(validate_tag("payments").is_ok());
    assert!(validate_tag("tier-1_v2").is_ok());
    assert!(validate_tag("").is_err());
    assert!(validate_tag("Upper").is_err());
    assert!(validate_tag("has space").is_err());
}

#[test]
fn hosts_are_rfc1123_or_ip_literals() {
    assert!(validate_host("api.example.com").is_ok());
    assert!(validate_host("a-b.example.com").is_ok());
    assert!(validate_host("10.0.0.1").is_ok());
    assert!(validate_host("2001:db8::1").is_ok());

    assert!(validate_host("").is_err());
    assert!(validate_host("-leading.example.com").is_err());
    assert!(validate_host("trailing-.example.com").is_err());
    assert!(validate_host("bad_host.example.com").is_err());
    assert!(validate_host(&format!("{}.com", "a".repeat(64))).is_err());
}
