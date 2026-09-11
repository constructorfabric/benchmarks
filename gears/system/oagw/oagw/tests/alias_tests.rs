//! Alias and hostname normalization tests.
//!
//! Covers `cpt-cf-oagw-algo-alias-normalize`: trimming, trailing-dot
//! stripping, ASCII lowercasing, RFC 1123 validation, and the `:port` suffix
//! that participates in alias identity.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use oagw::{Alias, AliasError, EndpointHost, Hostname};

#[test]
fn lowercases_and_strips_trailing_dots() {
    let alias = Alias::parse("API.OpenAI.com.").expect("valid alias");
    assert_eq!(alias.to_string(), "api.openai.com");
}

#[test]
fn hostname_lowercases_and_strips_trailing_dots() {
    let host = Hostname::parse("API.OpenAI.com..").expect("valid hostname");
    assert_eq!(host.as_str(), "api.openai.com");
}

#[test]
fn rejects_non_ascii_instead_of_transliterating() {
    assert_eq!(Alias::parse("api.openai.comé"), Err(AliasError::NonAscii));
    assert_eq!(Hostname::parse("exämple.com"), Err(AliasError::NonAscii));
    assert_eq!(
        EndpointHost::parse("exämple.com"),
        Err(AliasError::NonAscii)
    );
}

#[test]
fn rejects_a_label_longer_than_63_characters() {
    let host = format!("{}.example.com", "a".repeat(64));
    assert_eq!(Alias::parse(&host), Err(AliasError::TooLong));
}

#[test]
fn rejects_a_total_length_over_253_characters() {
    // 63 + 1 + 63 + 1 + 63 + 1 + 62 = 254 characters.
    let host = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(62),
    ]
    .join(".");
    assert_eq!(host.len(), 254);
    assert_eq!(Alias::parse(&host), Err(AliasError::TooLong));
}

#[test]
fn accepts_a_hostname_of_exactly_253_characters() {
    let host = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61),
    ]
    .join(".");
    assert_eq!(host.len(), 253);
    assert!(
        Alias::parse(&host).is_ok(),
        "253 characters is the RFC 1123 maximum"
    );
}

#[test]
fn rejects_leading_and_trailing_hyphens_and_empty_labels() {
    assert_eq!(
        Alias::parse("-api.openai.com"),
        Err(AliasError::InvalidLabel)
    );
    assert_eq!(
        Alias::parse("api-.openai.com"),
        Err(AliasError::InvalidLabel)
    );
    assert_eq!(
        Alias::parse("api..openai.com"),
        Err(AliasError::InvalidLabel)
    );
    assert_eq!(
        Alias::parse(".api.openai.com"),
        Err(AliasError::InvalidLabel)
    );
}

#[test]
fn rejects_a_label_with_a_character_outside_the_rfc_1123_set() {
    assert_eq!(
        Alias::parse("api_openai.com"),
        Err(AliasError::InvalidLabel)
    );
    assert_eq!(
        Alias::parse("api openai.com"),
        Err(AliasError::InvalidLabel)
    );
}

#[test]
fn rejects_empty_input() {
    assert_eq!(Alias::parse(""), Err(AliasError::Empty));
    assert_eq!(Alias::parse("   "), Err(AliasError::Empty));
    assert_eq!(Alias::parse("..."), Err(AliasError::Empty));
    assert_eq!(Hostname::parse(""), Err(AliasError::Empty));
}

#[test]
fn port_suffix_is_kept_and_separates_identity() {
    let bare = Alias::parse("api.openai.com").expect("bare alias");
    let ported = Alias::parse("api.openai.com:8443").expect("ported alias");
    assert_ne!(bare, ported, "the port participates in alias identity");
    assert_eq!(ported.port(), Some(8443));
    assert_eq!(ported.host().as_str(), "api.openai.com");
    assert_eq!(ported.to_string(), "api.openai.com:8443");
    assert_eq!(bare.port(), None);
}

#[test]
fn port_boundaries() {
    assert!(Alias::parse("api.openai.com:1").is_ok(), "port 1 accepted");
    assert!(
        Alias::parse("api.openai.com:65535").is_ok(),
        "port 65535 accepted"
    );
    assert_eq!(
        Alias::parse("api.openai.com:0"),
        Err(AliasError::InvalidPort)
    );
    assert_eq!(
        Alias::parse("api.openai.com:65536"),
        Err(AliasError::InvalidPort)
    );
    assert_eq!(
        Alias::parse("api.openai.com:"),
        Err(AliasError::InvalidPort)
    );
    assert_eq!(
        Alias::parse("api.openai.com:8443x"),
        Err(AliasError::InvalidPort)
    );
}

#[test]
fn port_suffix_is_normalized_the_same_way_as_the_host() {
    let alias = Alias::parse("  API.OpenAI.COM.:8443  ").expect("trimmed ported alias");
    assert_eq!(alias.to_string(), "api.openai.com:8443");
    assert_eq!(alias.port(), Some(8443));
}

#[test]
fn accepts_the_try_from_conversions() {
    let from_str = Hostname::try_from("api.openai.com").expect("hostname from &str");
    let from_string = Alias::try_from(String::from("api.openai.com")).expect("alias from String");
    assert_eq!(from_str.as_str(), "api.openai.com");
    assert_eq!(from_string.to_string(), "api.openai.com");
}

#[test]
fn alias_round_trips_through_its_normalized_string_form() {
    let alias = Alias::parse("API.OpenAI.COM.:8443").expect("valid alias");
    let rendered = alias.to_string();
    let back = Alias::try_from(rendered.clone()).expect("normalized form re-parses");
    assert_eq!(alias, back);
    let as_json = serde_json::to_value(&alias).expect("alias serializes");
    assert_eq!(as_json, serde_json::Value::String(rendered));
    let from_json: Alias = serde_json::from_value(as_json).expect("alias deserializes");
    assert_eq!(alias, from_json);
}

#[test]
fn endpoint_host_accepts_rfc_1123_names_and_ip_literals() {
    let name = EndpointHost::parse("API.OpenAI.Com.").expect("hostname endpoint host");
    assert_eq!(name.as_str(), "api.openai.com");
    let v4 = EndpointHost::parse("10.0.0.7").expect("ipv4 endpoint host");
    assert_eq!(v4.as_str(), "10.0.0.7");
    let v6 = EndpointHost::parse("2001:DB8::1").expect("ipv6 endpoint host");
    assert_eq!(v6.as_str(), "2001:db8::1");
    assert_eq!(
        EndpointHost::parse("-bad-"),
        Err(AliasError::InvalidLabel),
        "neither an RFC 1123 name nor an IP literal"
    );
}
