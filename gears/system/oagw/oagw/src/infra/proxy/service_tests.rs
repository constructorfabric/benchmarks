//! Data Plane helpers that decide routing and header hygiene.

use super::*;

#[test]
fn a_plain_host_is_a_hostname_or_ip_and_nothing_else() {
    assert!(is_plain_host("us.vendor.com"));
    assert!(is_plain_host("10.0.1.1"));
    assert!(is_plain_host("::1"));

    // Ports, schemes, paths and separators are all disqualifying.
    assert!(!is_plain_host("us.vendor.com:8443"));
    assert!(!is_plain_host("https://us.vendor.com"));
    assert!(!is_plain_host("us.vendor.com/path"));
    assert!(!is_plain_host("us vendor.com"));
    assert!(!is_plain_host("-leading.example.com"));
    assert!(!is_plain_host(""));
}

#[test]
fn hop_by_hop_headers_are_stripped_from_a_response() {
    let mut headers = HeaderMap::new();
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.insert(
            HeaderName::try_from(name).unwrap(),
            HeaderValue::from_static("x"),
        );
    }
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );

    strip_hop_by_hop(&mut headers);

    assert_eq!(headers.len(), 1);
    assert!(headers.contains_key("content-type"));
}

#[test]
fn the_proxy_permission_is_the_documented_identifier() {
    assert_eq!(PROXY_INVOKE_PERMISSION, "gts.cf.core.oagw.proxy.v1~");
}

#[test]
fn the_target_host_header_is_the_documented_name() {
    assert_eq!(TARGET_HOST_HEADER, "x-oagw-target-host");
}
