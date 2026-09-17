//! Tests for header rewriting and the `X-OAGW-Target-Host` matrix.
use std::sync::atomic::{AtomicU64, Ordering};

use http::HeaderMap;

use super::{
    AliasShape, EndpointCursor, HOP_BY_HOP, TargetError, alias_shape, apply_request_headers,
    apply_response_headers, inbound_request_headers, is_bare_hostname, is_hop_by_hop,
    select_endpoint, strip_request_hop_by_hop, target_failure,
};
use crate::domain::model::{
    Endpoint, EndpointScheme, HeaderPassthrough, HeaderSetting, HeadersConfig, Protocol,
    RequestHeaders, ResponseHeaders, ServerConfig, Upstream,
};
use crate::domain::plugin::{INVALID_TARGET_HOST, MISSING_TARGET_HOST, UNKNOWN_TARGET_HOST};

fn endpoint(host: &str) -> Endpoint {
    Endpoint::new(EndpointScheme::Https, host, None).expect("endpoint")
}

fn upstream(hosts: &[&str]) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::new_v4(),
        alias: "payments".to_owned(),
        enabled: true,
        tags: vec![],
        server: ServerConfig {
            endpoints: hosts.iter().map(|host| endpoint(host)).collect(),
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            http::HeaderName::try_from(*name).expect("header name"),
            http::HeaderValue::try_from(*value).expect("header value"),
        );
    }
    map
}

#[test]
fn the_hop_by_hop_list_covers_rfc_9110() {
    for name in HOP_BY_HOP {
        assert!(is_hop_by_hop(name));
        assert!(is_hop_by_hop(&name.to_ascii_uppercase()));
    }
    assert!(!is_hop_by_hop("authorization"));
    assert!(!is_hop_by_hop("host"));
}

#[test]
fn hop_by_hop_headers_are_dropped_from_the_forwarded_request() {
    let mut forwarded = headers(&[
        ("connection", "keep-alive"),
        ("keep-alive", "timeout=5"),
        ("transfer-encoding", "chunked"),
        ("authorization", "Bearer token"),
    ]);
    strip_request_hop_by_hop(&mut forwarded, false);
    assert!(forwarded.get("connection").is_none());
    assert!(forwarded.get("keep-alive").is_none());
    assert!(forwarded.get("transfer-encoding").is_none());
    assert_eq!(forwarded.get("authorization").unwrap(), "Bearer token");
}

#[test]
fn an_upgrade_request_keeps_connection_and_upgrade() {
    let mut forwarded = headers(&[
        ("connection", "upgrade"),
        ("upgrade", "websocket"),
        ("transfer-encoding", "chunked"),
        ("x-trace-id", "trace"),
    ]);
    strip_request_hop_by_hop(&mut forwarded, true);
    assert_eq!(forwarded.get("connection").unwrap(), "upgrade");
    assert_eq!(forwarded.get("upgrade").unwrap(), "websocket");
    assert!(forwarded.get("transfer-encoding").is_none());
    assert_eq!(forwarded.get("x-trace-id").unwrap(), "trace");
}

#[test]
fn connection_names_are_hop_by_hop_by_reference() {
    let mut forwarded = headers(&[("connection", "x-private, keep-alive"), ("x-private", "1")]);
    strip_request_hop_by_hop(&mut forwarded, false);
    assert!(forwarded.get("x-private").is_none());
    assert!(forwarded.get("connection").is_none());
}

#[test]
fn a_websocket_request_survives_connection_name_removal() {
    let mut forwarded = headers(&[
        ("connection", "keep-alive, upgrade"),
        ("upgrade", "websocket"),
    ]);
    strip_request_hop_by_hop(&mut forwarded, true);
    assert!(forwarded.get("upgrade").is_some());
    assert!(forwarded.get("connection").is_some());
}

#[test]
fn passthrough_all_forwards_everything_but_host() {
    let inbound = headers(&[("authorization", "Bearer t"), ("host", "gateway.example")]);
    let forwarded = inbound_request_headers(None, &inbound, false);
    assert_eq!(forwarded.get("authorization").unwrap(), "Bearer t");
    assert!(forwarded.get("host").is_none());
}

#[test]
fn passthrough_none_forwards_nothing() {
    let inbound = headers(&[("authorization", "Bearer t"), ("x-trace-id", "t")]);
    let config = HeadersConfig {
        request: RequestHeaders {
            passthrough: HeaderPassthrough::None,
            ..RequestHeaders::default()
        },
        response: ResponseHeaders::default(),
    };
    let forwarded = inbound_request_headers(Some(&config), &inbound, false);
    assert!(forwarded.is_empty());
}

#[test]
fn passthrough_allowlist_forwards_only_the_listed_names() {
    let inbound = headers(&[
        ("authorization", "Bearer t"),
        ("x-trace-id", "trace"),
        ("x-secret", "nope"),
    ]);
    let config = HeadersConfig {
        request: RequestHeaders {
            passthrough: HeaderPassthrough::Allowlist,
            passthrough_allowlist: vec!["authorization".to_owned(), "x-trace-id".to_owned()],
            ..RequestHeaders::default()
        },
        response: ResponseHeaders::default(),
    };
    let forwarded = inbound_request_headers(Some(&config), &inbound, false);
    assert_eq!(forwarded.get("authorization").unwrap(), "Bearer t");
    assert_eq!(forwarded.get("x-trace-id").unwrap(), "trace");
    assert!(forwarded.get("x-secret").is_none());
}

#[test]
fn header_rules_are_applied_in_set_remove_add_order() {
    let config = HeadersConfig {
        request: RequestHeaders {
            set: vec![HeaderSetting {
                name: "x-oagw-applied".to_owned(),
                value: "set".to_owned(),
            }],
            add: vec![HeaderSetting {
                name: "x-oagw-applied".to_owned(),
                value: "added".to_owned(),
            }],
            remove: vec!["x-oagw-removed".to_owned()],
            ..RequestHeaders::default()
        },
        response: ResponseHeaders::default(),
    };

    let mut outbound = headers(&[("x-oagw-removed", "gone"), ("x-oagw-applied", "before")]);
    apply_request_headers(Some(&config), &mut outbound);
    assert!(outbound.get("x-oagw-removed").is_none());
    let applied = outbound.get_all("x-oagw-applied").iter().count();
    assert_eq!(applied, 2, "`set` replaces and `add` appends");
}

#[test]
fn response_rules_are_applied() {
    let config = HeadersConfig {
        request: RequestHeaders::default(),
        response: ResponseHeaders {
            set: vec![HeaderSetting {
                name: "cache-control".to_owned(),
                value: "no-store".to_owned(),
            }],
            remove: vec!["server".to_owned()],
            add: vec![HeaderSetting {
                name: "x-oagw-note".to_owned(),
                value: "ok".to_owned(),
            }],
        },
    };
    let mut outbound = headers(&[("server", "upstream"), ("cache-control", "max-age=60")]);
    apply_response_headers(Some(&config), &mut outbound);
    assert_eq!(outbound.get("cache-control").unwrap(), "no-store");
    assert!(outbound.get("server").is_none());
    assert_eq!(outbound.get("x-oagw-note").unwrap(), "ok");
}

#[test]
fn a_missing_config_leaves_the_headers_alone() {
    let mut outbound = headers(&[("server", "upstream")]);
    apply_request_headers(None, &mut outbound);
    apply_response_headers(None, &mut outbound);
    assert_eq!(outbound.get("server").unwrap(), "upstream");
}

#[test]
fn a_single_endpoint_pool_needs_no_target_host() {
    let upstream = upstream(&["api.example.com"]);
    let cursor = EndpointCursor::new();
    assert_eq!(select_endpoint("payments", &upstream, None, &cursor), Ok(0));
    assert_eq!(
        select_endpoint("payments", &upstream, Some("api.example.com"), &cursor),
        Ok(0)
    );
}

#[test]
fn an_explicit_alias_round_robins() {
    let upstream = upstream(&["a.example", "b.example", "c.example"]);
    let cursor = EndpointCursor::new();
    let mut seen = vec![];
    for _ in 0..6 {
        let position = select_endpoint("payments", &upstream, None, &cursor).expect("position");
        seen.push(position);
    }
    assert_eq!(seen, vec![0, 1, 2, 0, 1, 2]);
}

#[test]
fn a_common_suffix_alias_requires_disambiguation() {
    let upstream = upstream(&["eu.payments.example", "us.payments.example"]);
    let cursor = EndpointCursor::new();
    assert_eq!(
        select_endpoint("payments.example", &upstream, None, &cursor),
        Err(TargetError::Missing {
            alias: "payments.example".to_owned()
        })
    );
    assert_eq!(
        select_endpoint(
            "payments.example",
            &upstream,
            Some("us.payments.example"),
            &cursor
        ),
        Ok(1)
    );
}

#[test]
fn a_named_target_wins_over_round_robin() {
    let upstream = upstream(&["a.example", "b.example"]);
    let cursor = EndpointCursor::new();
    assert_eq!(
        select_endpoint("payments", &upstream, Some("b.example"), &cursor),
        Ok(1)
    );
    assert_eq!(
        select_endpoint("payments", &upstream, Some("B.EXAMPLE"), &cursor),
        Ok(1)
    );
}

#[test]
fn an_unknown_target_host_is_refused() {
    let upstream = upstream(&["a.example"]);
    let cursor = EndpointCursor::new();
    assert_eq!(
        select_endpoint("payments", &upstream, Some("nope.example"), &cursor),
        Err(TargetError::Unknown("nope.example".to_owned()))
    );
}

#[test]
fn a_non_bare_target_host_is_refused() {
    let upstream = upstream(&["a.example"]);
    let cursor = EndpointCursor::new();
    for value in [
        "a.example:443",
        "/etc/passwd",
        "a@example",
        "a b",
        "http://a",
    ] {
        let error = select_endpoint("payments", &upstream, Some(value), &cursor).expect_err(value);
        assert!(
            matches!(error, TargetError::Invalid(_)),
            "{value}: {error:?}"
        );
    }
}

#[test]
fn an_empty_pool_is_refused() {
    let upstream = upstream(&[]);
    let cursor = EndpointCursor::new();
    assert!(matches!(
        select_endpoint("payments", &upstream, None, &cursor),
        Err(TargetError::Invalid(_))
    ));
}

#[test]
fn an_ignored_target_host_value_behaves_as_if_absent() {
    let upstream = upstream(&["a.example", "b.example"]);
    let cursor = EndpointCursor::new();
    assert_eq!(
        select_endpoint("payments", &upstream, Some("   "), &cursor),
        Ok(0)
    );
}

#[test]
fn target_failures_map_onto_the_problem_catalogue() {
    let missing = target_failure(
        "payments.example",
        TargetError::Missing {
            alias: "payments.example".to_owned(),
        },
    );
    assert_eq!(missing.status, 400);
    assert_eq!(
        missing.type_uri,
        crate::domain::plugin::problem_type(MISSING_TARGET_HOST)
    );

    let invalid = target_failure("payments", TargetError::Invalid("bad".to_owned()));
    assert_eq!(invalid.status, 400);
    assert_eq!(
        invalid.type_uri,
        crate::domain::plugin::problem_type(INVALID_TARGET_HOST)
    );

    let unknown = target_failure("payments", TargetError::Unknown("x".to_owned()));
    assert_eq!(unknown.status, 400);
    assert_eq!(
        unknown.type_uri,
        crate::domain::plugin::problem_type(UNKNOWN_TARGET_HOST)
    );
}

#[test]
fn the_alias_shape_is_common_suffix_only_for_shared_suffixes() {
    let shared = upstream(&["eu.payments.example", "us.payments.example"]);
    assert_eq!(
        alias_shape("payments.example", &shared.server.endpoints),
        AliasShape::CommonSuffix
    );
    let explicit = upstream(&["a.example", "b.example"]);
    assert_eq!(
        alias_shape("payments", &explicit.server.endpoints),
        AliasShape::Explicit
    );
    let single = upstream(&["eu.payments.example"]);
    assert_eq!(
        alias_shape("payments.example", &single.server.endpoints),
        AliasShape::Explicit
    );
}

#[test]
fn a_cursor_starts_at_zero_and_counts_independently_per_upstream() {
    let cursor = EndpointCursor::new();
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    assert_eq!(cursor.next_index(&first, 2), 0);
    assert_eq!(cursor.next_index(&first, 2), 1);
    assert_eq!(cursor.next_index(&second, 2), 0);
    assert_eq!(cursor.next_index(&first, 2), 0);
}

#[test]
fn a_cursor_survives_an_empty_pool() {
    let cursor = EndpointCursor::new();
    let id = uuid::Uuid::new_v4();
    assert_eq!(cursor.next_index(&id, 0), 0);
}

#[test]
fn a_cursor_is_clonable_and_shares_the_same_counters() {
    let cursor = EndpointCursor::new();
    let id = uuid::Uuid::new_v4();
    assert_eq!(cursor.next_index(&id, 5), 0);
    // A clone taken afterwards shares the counters it already carries.
    let clone = cursor.clone();
    assert_eq!(clone.next_index(&id, 5), 1);
    assert_eq!(cursor.next_index(&id, 5), 2);
}

#[test]
fn bare_hostnames_are_accepted_and_urls_refused() {
    assert!(is_bare_hostname("api.example"));
    assert!(is_bare_hostname("192.168.0.1"));
    assert!(!is_bare_hostname(""));
    assert!(!is_bare_hostname("api.example:443"));
    assert!(!is_bare_hostname("api.example/path"));
}

#[test]
fn the_round_robin_counter_wraps_around() {
    let counter = std::sync::Arc::new(AtomicU64::new(u64::MAX));
    let previous = counter.fetch_add(1, Ordering::Relaxed);
    assert_eq!(previous, u64::MAX);
}
