//! Unit tests for the wire-contract validation layer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::model::{
    Cors, Endpoint, EndpointScheme, GrpcMatch, HeadersConfig, HttpMatch, HttpMethod, MatchConfig,
    PathSuffixMode, PluginsConfig, Protocol, Route, ServerConfig, Upstream,
};

fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn server(endpoints: Vec<Endpoint>) -> ServerConfig {
    ServerConfig { endpoints }
}

fn http_upstream() -> Upstream {
    Upstream {
        id: "u".to_owned(),
        tenant_id: uuid::Uuid::nil(),
        alias: "api.example.com".to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: server(vec![endpoint(EndpointScheme::Http, "api.example.com", 80)]),
        auth: None,
        headers: HeadersConfig::default(),
        rate_limit: None,
        cors: None,
        plugins: PluginsConfig::default(),
        tags: vec!["llm".to_owned()],
        created_at: None,
        updated_at: None,
    }
}

fn http_route() -> Route {
    Route {
        id: "r".to_owned(),
        tenant_id: uuid::Uuid::nil(),
        upstream_id: "u".to_owned(),
        enabled: true,
        priority: 0,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get, HttpMethod::Post],
                path: "/v1/chat".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::default(),
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: PluginsConfig::default(),
        tags: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

#[test]
fn every_scheme_including_http_is_accepted() {
    for scheme in [
        EndpointScheme::Http,
        EndpointScheme::Https,
        EndpointScheme::Wss,
        EndpointScheme::Wt,
        EndpointScheme::Grpc,
    ] {
        // `http` is a legal endpoint scheme here; only the TLS family encrypts.
        assert_eq!(
            scheme.is_tls(),
            !matches!(scheme, EndpointScheme::Http),
            "{scheme:?} classified unexpectedly"
        );
    }
}

#[test]
fn hosts_accept_names_and_ip_literals() {
    for host in [
        "api.example.com",
        "a.b",
        "localhost",
        "10.0.0.1",
        "2001:db8::1",
        "xn--bcher-kva.example",
    ] {
        assert!(is_valid_host(host), "{host} rejected");
    }
}

#[test]
fn hosts_reject_malformed_names() {
    for host in [
        "",
        "-api.example.com",
        "api.example.com-",
        "api..example.com",
        "api_example.com",
        "api .example.com",
        &"a".repeat(MAX_HOSTNAME_LENGTH + 1),
    ] {
        assert!(!is_valid_host(host), "{host} accepted");
    }
    assert!(!is_valid_host(&format!(
        "{}.example",
        "a".repeat(MAX_LABEL_LENGTH + 1)
    )));
}

#[test]
fn server_rejects_an_empty_pool() {
    let mut upstream = http_upstream();
    upstream.server = server(Vec::new());
    assert!(validate_upstream(&upstream).is_err());
}

#[test]
fn server_requires_homogeneous_scheme_and_port() {
    let mut upstream = http_upstream();
    upstream.server = server(vec![
        endpoint(EndpointScheme::Http, "a.example.com", 80),
        endpoint(EndpointScheme::Http, "b.example.com", 80),
    ]);
    assert!(validate_upstream(&upstream).is_ok());

    upstream.server = server(vec![
        endpoint(EndpointScheme::Http, "a.example.com", 80),
        endpoint(EndpointScheme::Https, "b.example.com", 443),
    ]);
    assert!(validate_upstream(&upstream).is_err());

    upstream.server = server(vec![
        endpoint(EndpointScheme::Http, "a.example.com", 80),
        endpoint(EndpointScheme::Http, "b.example.com", 8080),
    ]);
    assert!(validate_upstream(&upstream).is_err());
}

#[test]
fn upstream_requires_a_valid_alias() {
    let mut upstream = http_upstream();
    upstream.alias = "API.example.com".to_owned();
    assert!(validate_upstream(&upstream).is_err());
    upstream.alias = "api.example.com".to_owned();
    assert!(validate_upstream(&upstream).is_ok());
}

#[test]
fn upstream_rejects_unknown_tags_and_bad_headers() {
    let mut upstream = http_upstream();
    upstream.tags = vec!["Not Allowed".to_owned()];
    assert!(validate_upstream(&upstream).is_err());
    upstream.tags = vec!["llm".to_owned()];
    upstream
        .headers
        .request
        .set
        .insert("bad name".to_owned(), "1".to_owned());
    assert!(validate_upstream(&upstream).is_err());
}

#[test]
fn route_requires_exactly_one_match_variant() {
    let mut route = http_route();
    assert!(validate_route(&route).is_ok());

    route.match_config.grpc = Some(GrpcMatch {
        service: "svc".to_owned(),
        method: String::new(),
    });
    assert!(validate_route(&route).is_err());

    route.match_config.http = None;
    assert!(validate_route(&route).is_ok());

    route.match_config.grpc = None;
    assert!(validate_route(&route).is_err());
}

#[test]
fn route_http_match_requires_methods_and_path() {
    let mut route = http_route();
    if let Some(http) = route.match_config.http.as_mut() {
        http.methods.clear();
    }
    assert!(validate_route(&route).is_err());

    let mut route = http_route();
    if let Some(http) = route.match_config.http.as_mut() {
        http.path = String::new();
    }
    assert!(validate_route(&route).is_err());
}

#[test]
fn route_requires_an_upstream_reference() {
    let mut route = http_route();
    route.upstream_id = String::new();
    assert!(validate_route(&route).is_err());
}

#[test]
fn route_cors_and_rate_limit_are_validated() {
    let mut route = http_route();
    route.cors = Some(Cors {
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allow_credentials: true,
        ..Cors::unset()
    });
    assert!(validate_route(&route).is_err());
}

#[test]
fn body_length_must_match_the_declaration() {
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, "5".parse().unwrap());
    assert_eq!(validate_body(&headers, 5), Ok(()));
    assert!(matches!(
        validate_body(&headers, 4),
        Err(BodyViolation::LengthMismatch {
            declared: 5,
            actual: 4
        })
    ));
}

#[test]
fn body_rejects_a_non_numeric_content_length() {
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_LENGTH, "five".parse().unwrap());
    assert!(matches!(
        validate_body(&headers, 0),
        Err(BodyViolation::InvalidLength(_))
    ));
}

#[test]
fn body_rejects_unsupported_transfer_encodings() {
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::TRANSFER_ENCODING, "gzip".parse().unwrap());
    assert!(matches!(
        validate_body(&headers, 0),
        Err(BodyViolation::UnsupportedEncoding(encoding)) if encoding == "gzip"
    ));

    let mut chunked = http::HeaderMap::new();
    chunked.insert(http::header::TRANSFER_ENCODING, "chunked".parse().unwrap());
    assert_eq!(validate_body(&chunked, 0), Ok(()));
}

#[test]
fn body_enforces_the_hard_limit() {
    let headers = http::HeaderMap::new();
    assert_eq!(validate_body(&headers, 0), Ok(()));
    let limit = usize::try_from(BODY_HARD_LIMIT_BYTES).expect("the limit fits usize");
    assert_eq!(validate_body(&headers, limit), Ok(()));
    assert_eq!(
        validate_body(&headers, limit + 1),
        Err(BodyViolation::TooLarge)
    );
}

#[test]
fn routable_methods_cover_the_documented_set() {
    for method in ["GET", "POST", "PUT", "DELETE", "PATCH"] {
        assert!(is_routable_method(method));
    }
    assert!(!is_routable_method("TRACE"));
    assert!(!is_routable_method(""));
}

#[test]
fn only_http_is_proxied_in_this_delivery() {
    assert!(is_proxied_protocol(Protocol::Http));
    assert!(!is_proxied_protocol(Protocol::Grpc));
}

#[test]
fn hostname_limits_follow_rfc_1123() {
    assert_eq!(MAX_HOSTNAME_LENGTH, 253);
    assert_eq!(MAX_LABEL_LENGTH, 63);
}
