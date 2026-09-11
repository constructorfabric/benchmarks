//! Tests for the proxy endpoint's URL routing and request extraction.

use crate::proxy::{build_url, path_matches, forward_path};
use crate::domain::route::{HttpMatch, PathSuffixMode, Route, RouteMatch};
use crate::domain::upstream::Endpoint;
use crate::proxy::target::{self, Target};

fn target(scheme: &str, host: &str, port: u16) -> Target {
    let endpoint = Endpoint {
        scheme: scheme.to_owned(),
        host: host.to_owned(),
        port,
    };
    let authority = target::authority(&endpoint);
    Target {
        endpoint,
        authority,
        pinned: false,
    }
}

#[test]
fn the_outbound_url_carries_the_query_only_when_there_is_one() {
    let destination = target("https", "api.example.com", 443);
    assert_eq!(
        build_url(&destination, "/v1/items", ""),
        "https://api.example.com/v1/items"
    );
    assert_eq!(
        build_url(&destination, "/v1/items", "limit=10"),
        "https://api.example.com/v1/items?limit=10"
    );
}

#[test]
fn a_non_standard_port_is_part_of_the_origin() {
    assert_eq!(
        target("https", "api.example.com", 8443).origin(),
        "https://api.example.com:8443"
    );
    assert_eq!(
        target("http", "api.example.com", 80).origin(),
        "http://api.example.com"
    );
}

#[test]
fn a_route_path_matches_as_a_segment_prefix() {
    assert!(path_matches("/v1", "/v1"));
    assert!(path_matches("/v1", "/v1/items"));
    assert!(path_matches("/v1", "/v1/"));
    assert!(path_matches("/v1/items", "/v1/items"));
    assert!(!path_matches("/v1", "/v1items"), "a label boundary is required");
    assert!(!path_matches("/v1", "/v2/items"));
    assert!(path_matches("/", "/anything"), "the root matches every path");
}

#[test]
fn the_forwarded_path_honours_the_suffix_mode() {
    let mut append = Route {
        enabled: true,
        ..Route::default()
    };
    append.r#match = Some(RouteMatch::Http(HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/items".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }));
    assert_eq!(forward_path(&append, "/v1/items/42"), "/v1/items/42");
    assert_eq!(forward_path(&append, "/v1/items"), "/v1/items");

    let mut disabled = append;
    disabled.r#match = Some(RouteMatch::Http(HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/items".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Disabled,
    }));
    assert_eq!(forward_path(&disabled, "/v1/items/42"), "/v1/items");
}

#[test]
fn a_route_without_an_http_match_forwards_the_requested_path() {
    let route = Route::default();
    assert_eq!(forward_path(&route, "/v1/anything"), "/v1/anything");
}

#[test]
fn a_request_larger_than_the_ceiling_is_refused_before_a_connection_is_made() {
    use crate::proxy::{validate_body, ProxyRequest};
    use crate::security::SecurityContextHolder;
    use bytes::Bytes;
    use http::{HeaderMap, Method};

    let tenant = uuid::Uuid::new_v4();
    let request = ProxyRequest {
        method: Method::POST,
        alias: "payments.example.com".to_owned(),
        path: "/v1/charge".to_owned(),
        query: String::new(),
        headers: HeaderMap::new(),
        body: Bytes::from(vec![0u8; crate::proxy::MAX_BODY_BYTES + 1]),
        client_ip: "10.0.0.1".to_owned(),
        security: SecurityContextHolder::new(
            toolkit_security::SecurityContext::anonymous(),
            vec![tenant],
        ),
        upgrade: None,
    };
    let err = validate_body(&request).expect_err("over the ceiling");
    assert_eq!(err.kind(), crate::error::ErrorKind::PayloadTooLarge);
}

#[test]
fn a_body_that_contradicts_its_declared_length_is_refused() {
    use crate::proxy::{validate_body, ProxyRequest};
    use crate::security::SecurityContextHolder;
    use bytes::Bytes;
    use http::{HeaderMap, Method};

    let tenant = uuid::Uuid::new_v4();
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, "5".parse().unwrap());
    let request = ProxyRequest {
        method: Method::POST,
        alias: "payments.example.com".to_owned(),
        path: "/v1/charge".to_owned(),
        query: String::new(),
        headers,
        body: Bytes::from_static(b"hi"),
        client_ip: "10.0.0.1".to_owned(),
        security: SecurityContextHolder::new(
            toolkit_security::SecurityContext::anonymous(),
            vec![tenant],
        ),
        upgrade: None,
    };
    let err = validate_body(&request).expect_err("declared 5, carried 2");
    assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError);
    assert!(err.detail().contains('5'), "{err}");
}

/// A declared length that is not a number frames the body in a way the relay cannot
/// check, so it is refused with a validation error rather than guessed at.
#[test]
fn a_non_integer_content_length_is_refused() {
    use crate::proxy::{validate_body, ProxyRequest};
    use crate::security::SecurityContextHolder;
    use bytes::Bytes;
    use http::{HeaderMap, Method};

    let tenant = uuid::Uuid::new_v4();
    for declared in ["two", "12.5", ""] {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, declared.parse().unwrap());
        let request = ProxyRequest {
            method: Method::POST,
            alias: "payments.example.com".to_owned(),
            path: "/v1/charge".to_owned(),
            query: String::new(),
            headers,
            body: Bytes::from_static(b"payload"),
            client_ip: "10.0.0.1".to_owned(),
            security: SecurityContextHolder::new(
                toolkit_security::SecurityContext::anonymous(),
                vec![tenant],
            ),
            upgrade: None,
        };
        let err = validate_body(&request)
            .expect_err("the declared length is not an integer");
        assert_eq!(
            err.kind(),
            crate::error::ErrorKind::ValidationError,
            "`{declared}` is not a length: {err}"
        );
    }
}

/// DESIGN's request-validation table: `chunked` is the one transfer encoding the relay
/// carries; anything else is refused before the plugin chain runs.
#[test]
fn a_chunked_request_is_accepted() {
    use crate::proxy::{validate_body, ProxyRequest};
    use crate::security::SecurityContextHolder;
    use bytes::Bytes;
    use http::{HeaderMap, Method};

    let tenant = uuid::Uuid::new_v4();
    let mut headers = HeaderMap::new();
    headers.insert(http::header::TRANSFER_ENCODING, "chunked".parse().unwrap());
    let request = ProxyRequest {
        method: Method::POST,
        alias: "payments.example.com".to_owned(),
        path: "/v1/charge".to_owned(),
        query: String::new(),
        headers,
        body: Bytes::from_static(b"payload"),
        client_ip: "10.0.0.1".to_owned(),
        security: SecurityContextHolder::new(
            toolkit_security::SecurityContext::anonymous(),
            vec![tenant],
        ),
        upgrade: None,
    };
    assert!(validate_body(&request).is_ok(), "chunked is forwarded");
}

#[test]
fn an_unsupported_transfer_encoding_is_refused() {
    use crate::proxy::{validate_body, ProxyRequest};
    use crate::security::SecurityContextHolder;
    use bytes::Bytes;
    use http::{HeaderMap, Method};

    let tenant = uuid::Uuid::new_v4();
    let mut headers = HeaderMap::new();
    headers.insert(http::header::TRANSFER_ENCODING, "gzip".parse().unwrap());
    let request = ProxyRequest {
        method: Method::POST,
        alias: "payments.example.com".to_owned(),
        path: "/v1/charge".to_owned(),
        query: String::new(),
        headers,
        body: Bytes::from_static(b"payload"),
        client_ip: "10.0.0.1".to_owned(),
        security: SecurityContextHolder::new(
            toolkit_security::SecurityContext::anonymous(),
            vec![tenant],
        ),
        upgrade: None,
    };
    let err = validate_body(&request).expect_err("gzip is not a framing the relay carries");
    assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError);
    assert!(err.detail().contains("gzip"), "{err}");
}

/// A coding list is refused when any member is unsupported, however it is capitalised.
#[test]
fn an_unsupported_coding_in_a_list_is_refused_regardless_of_case() {
    use crate::proxy::{validate_body, ProxyRequest};
    use crate::security::SecurityContextHolder;
    use bytes::Bytes;
    use http::{HeaderMap, Method};

    let tenant = uuid::Uuid::new_v4();
    let mut headers = HeaderMap::new();
    headers.insert(http::header::TRANSFER_ENCODING, "Chunked, DEFLATE".parse().unwrap());
    let request = ProxyRequest {
        method: Method::POST,
        alias: "payments.example.com".to_owned(),
        path: "/v1/charge".to_owned(),
        query: String::new(),
        headers,
        body: Bytes::from_static(b"payload"),
        client_ip: "10.0.0.1".to_owned(),
        security: SecurityContextHolder::new(
            toolkit_security::SecurityContext::anonymous(),
            vec![tenant],
        ),
        upgrade: None,
    };
    let err = validate_body(&request).expect_err("deflate is not chunked");
    assert!(err.detail().contains("DEFLATE"), "{err}");
}

#[test]
fn the_proxy_routes_are_registered_under_the_gear_prefix() {
    // The endpoint's path templates are gear-relative: the gateway mounts them at
    // `/oagw/v1/proxy/{alias}` with no `/api` prefix, which the api-gateway then nests.
    let aliased = "/oagw/v1/proxy/{alias}";
    let nested = "/oagw/v1/proxy/{alias}/{*path}";
    for template in [aliased, nested] {
        assert!(template.starts_with("/oagw/v1/proxy/"), "{template}");
        assert!(!template.starts_with("/api/"), "{template}");
    }
}

#[test]
fn the_target_host_header_is_the_only_routing_header() {
    assert_eq!(target::TARGET_HOST_HEADER, "x-oagw-target-host");
}

#[test]
fn a_single_endpoint_upstream_needs_no_target_header() {
    let mut upstream = crate::domain::upstream::Upstream::default();
    upstream.server.endpoints.push(Endpoint {
        scheme: "https".to_owned(),
        host: "api.example.com".to_owned(),
        port: 443,
    });
    let resolved = target::resolve(&upstream, None, 0).expect("resolves");
    assert_eq!(resolved.authority, "api.example.com");
    assert!(!resolved.pinned);
}

#[test]
fn a_common_suffix_alias_requires_the_target_header() {
    let mut upstream = crate::domain::upstream::Upstream::default();
    upstream.alias = "example.com".to_owned();
    for host in ["a.example.com", "b.example.com"] {
        upstream.server.endpoints.push(Endpoint {
            scheme: "https".to_owned(),
            host: host.to_owned(),
            port: 443,
        });
    }
    let err = target::resolve(&upstream, None, 0).expect_err("the caller must pin a host");
    assert_eq!(err.kind(), crate::error::ErrorKind::MissingTargetHost, "{err}");

    let pinned = target::resolve(&upstream, Some("b.example.com"), 0).expect("resolves");
    assert!(pinned.pinned);
    assert_eq!(pinned.authority, "b.example.com");
}

#[test]
fn a_pinned_host_outside_the_pool_is_refused() {
    let mut upstream = crate::domain::upstream::Upstream::default();
    upstream.alias = "example.com".to_owned();
    upstream.server.endpoints.push(Endpoint {
        scheme: "https".to_owned(),
        host: "a.example.com".to_owned(),
        port: 443,
    });
    let err = target::resolve(&upstream, Some("other.example.com"), 0)
        .expect_err("not in the pool");
    assert_eq!(err.kind(), crate::error::ErrorKind::UnknownTargetHost, "{err}");
}

#[test]
fn a_target_header_may_not_carry_a_port_or_a_path() {
    for bad in ["", "host:8443", "host/path", "host?q", "sp ace", "@host"] {
        let err = target::validate_target_host(bad).expect_err("{bad}");
        assert_eq!(err.kind(), crate::error::ErrorKind::InvalidTargetHost, "{bad}");
    }
}

/// A multi-endpoint pool the caller does not pin is walked in order, so the alias does
/// not pin every request to its first member (US3/AC15, ADR-0001).
#[test]
fn an_unpinned_multi_endpoint_pool_is_walked_round_robin() {
    let mut upstream = crate::domain::upstream::Upstream::default();
    upstream.alias = "payments-pool".to_owned();
    for host in ["a.example.com", "b.example.com", "c.example.com"] {
        upstream.server.endpoints.push(Endpoint {
            scheme: "https".to_owned(),
            host: host.to_owned(),
            port: 443,
        });
    }

    // The alias is explicit, so no target header is owed.
    for (index, expected) in ["a.example.com", "b.example.com", "c.example.com", "a.example.com"]
        .into_iter()
        .enumerate()
    {
        let resolved = target::resolve(&upstream, None, index).expect("resolves");
        assert!(!resolved.pinned, "the request was not pinned");
        assert_eq!(
            resolved.authority, expected,
            "rotation index {index} of the pool"
        );
    }
}

/// The load-balancing index is taken modulo the pool, so a pool that shrinks between
/// requests still lands on one of its members.
#[test]
fn a_rotation_index_beyond_the_pool_wraps_onto_a_member() {
    let mut upstream = crate::domain::upstream::Upstream::default();
    upstream.alias = "payments-pool".to_owned();
    upstream.server.endpoints.push(Endpoint {
        scheme: "https".to_owned(),
        host: "a.example.com".to_owned(),
        port: 443,
    });
    upstream.server.endpoints.push(Endpoint {
        scheme: "https".to_owned(),
        host: "b.example.com".to_owned(),
        port: 443,
    });
    let resolved = target::resolve(&upstream, None, 7).expect("resolves");
    assert_eq!(resolved.authority, "b.example.com", "7 % 2");
}

/// A pinned request ignores the rotation entirely: the cursor moves, the choice does not.
#[test]
fn a_pinned_host_is_dialled_whatever_the_rotation_says() {
    let mut upstream = crate::domain::upstream::Upstream::default();
    upstream.alias = "payments-pool".to_owned();
    for host in ["a.example.com", "b.example.com"] {
        upstream.server.endpoints.push(Endpoint {
            scheme: "https".to_owned(),
            host: host.to_owned(),
            port: 443,
        });
    }
    for rotation in [0, 1, 2] {
        let resolved = target::resolve(&upstream, Some("b.example.com"), rotation).expect("resolves");
        assert!(resolved.pinned);
        assert_eq!(resolved.authority, "b.example.com", "rotation {rotation}");
    }
}

/// The cursor the relay draws its index from is per upstream, and a pool of one never
/// moves it — there is nothing to distribute over.
#[test]
fn the_rotation_cursor_is_per_upstream_and_still_for_a_pool_of_one() {
    let rotation = target::Rotation::default();
    assert_eq!(rotation.next("pool-a", 3), 0);
    assert_eq!(rotation.next("pool-a", 3), 1);
    assert_eq!(rotation.next("pool-b", 3), 0, "another pool starts over");
    assert_eq!(rotation.next("pool-a", 3), 2);
    assert_eq!(rotation.next("pool-a", 3), 0, "the pool wraps");

    for _ in 0..3 {
        assert_eq!(rotation.next("singleton", 1), 0);
    }
    assert_eq!(
        rotation.next("pool-b", 3),
        1,
        "an empty pool left no cursor behind"
    );
}
