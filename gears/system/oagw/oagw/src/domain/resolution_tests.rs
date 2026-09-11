//! Tests for the data-plane target resolution: path matching, the
//! `X-OAGW-Target-Host` matrix of ADR-0001 Appendix A and the SSRF/scheme
//! policies.

use uuid::Uuid;

use super::{
    ResolvedTarget, enforce_scheme_policy, enforce_ssrf_policy, parse_target_host, path_matches,
    resolve_proxy_target, select_endpoint,
};
use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::control_plane::ControlPlane;
use crate::domain::model::{
    Endpoint, EndpointScheme, GrpcMatch, HttpMethod, PathSuffixMode, RouteMatch, RouteSpec,
    ServerConfig, UpstreamProtocol, UpstreamSpec, normalize_host, upstream_gts_id,
};
use crate::error::{
    INVALID_TARGET_HOST_TYPE, LINK_UNAVAILABLE_TYPE, MISSING_TARGET_HOST_TYPE, OagwError,
    ROUTE_NOT_FOUND_TYPE, UNKNOWN_TARGET_HOST_TYPE, VALIDATION_ERROR_TYPE,
};

/// Tenant that owns every resource the tests create.
fn tenant() -> Uuid {
    Uuid::from_u128(0xA17)
}

/// A tenant that owns nothing.
fn other_tenant() -> Uuid {
    Uuid::from_u128(0xB17)
}

/// An upstream document with a derived or explicit alias and one endpoint per
/// `(scheme, host, port)` triple.
fn upstream_spec(alias: Option<&str>, endpoints: &[(EndpointScheme, &str, u16)]) -> UpstreamSpec {
    UpstreamSpec {
        id: None,
        alias: alias.map(str::to_owned),
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: endpoints
                .iter()
                .map(|(scheme, host, port)| Endpoint {
                    scheme: *scheme,
                    host: (*host).to_owned(),
                    port: *port,
                })
                .collect(),
        },
        protocol: UpstreamProtocol::Http,
        enabled: true,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

/// The routes a test upstream is created with.
enum Template {
    /// `GET`, `POST`, `PUT`, `DELETE` and `PATCH` under `/` (append): matches
    /// every proxied request, so a test can concentrate on the endpoint rules.
    CatchAll,
    /// An HTTP route with an explicit method set, path and suffix mode.
    Http(&'static [HttpMethod], &'static str, PathSuffixMode),
    /// A gRPC route.
    Grpc(&'static str, &'static str),
}

fn build_template(template: &Template, upstream_id: &str) -> RouteSpec {
    match template {
        Template::CatchAll => RouteSpec {
            id: None,
            upstream_id: upstream_id.to_owned(),
            tags: Vec::new(),
            match_rules: RouteMatch {
                http: Some(crate::domain::model::HttpMatch {
                    methods: [
                        HttpMethod::Get,
                        HttpMethod::Post,
                        HttpMethod::Put,
                        HttpMethod::Delete,
                        HttpMethod::Patch,
                    ]
                    .to_vec(),
                    path: "/".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
        },
        Template::Http(methods, path, mode) => RouteSpec {
            id: None,
            upstream_id: upstream_id.to_owned(),
            tags: Vec::new(),
            match_rules: RouteMatch {
                http: Some(crate::domain::model::HttpMatch {
                    methods: methods.to_vec(),
                    path: (*path).to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: *mode,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
        },
        Template::Grpc(service, method) => RouteSpec {
            id: None,
            upstream_id: upstream_id.to_owned(),
            tags: Vec::new(),
            match_rules: RouteMatch {
                http: None,
                grpc: Some(GrpcMatch {
                    service: (*service).to_owned(),
                    method: (*method).to_owned(),
                }),
            },
            plugins: None,
            rate_limit: None,
        },
    }
}

/// A control plane holding one upstream plus its routes.
fn plane_with(
    tenant_id: Uuid,
    alias: Option<&str>,
    endpoints: &[(EndpointScheme, &str, u16)],
    routes: &[Template],
) -> ControlPlane {
    let plane = ControlPlane::new();
    let upstream = plane
        .create_upstream(tenant_id, upstream_spec(alias, endpoints))
        .expect("the test upstream must be accepted");
    let id = upstream_gts_id(upstream.id);
    for template in routes {
        plane
            .create_route(tenant_id, build_template(template, &id))
            .expect("the test route must be accepted");
    }
    plane
}

/// Configuration for a loopback upstream: the SSRF policy is off and plaintext
/// upstreams are allowed.
fn config_with(ssrf: bool, allow_http: bool) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: allow_http,
        ssrf_policy: SsrfPolicy { enabled: ssrf },
        ..OagwConfig::default()
    }
}

fn resolve(
    plane: &ControlPlane,
    config: &OagwConfig,
    alias: &str,
    method: &str,
    suffix: &str,
    target_host: Option<&str>,
) -> Result<ResolvedTarget, OagwError> {
    resolve_proxy_target(plane, config, tenant(), alias, method, suffix, target_host)
}

/// Compare a `valid_hosts` extension against a list of `&str` literals.
fn hosts_of(extension: Option<&[String]>) -> Option<Vec<&str>> {
    extension.map(|hosts| hosts.iter().map(String::as_str).collect())
}

/// A single-endpoint upstream with a catch-all route, addressed by `alias`.
fn catch_all_plane(alias: &str, endpoints: &[(EndpointScheme, &str, u16)]) -> ControlPlane {
    plane_with(tenant(), Some(alias), endpoints, &[Template::CatchAll])
}

/// A single plaintext loopback endpoint behind a catch-all route.
fn loopback_plane() -> ControlPlane {
    catch_all_plane(
        "api.local.test",
        &[(EndpointScheme::Http, "127.0.0.1", 8080)],
    )
}

/// A two-endpoint upstream whose alias is not a suffix of either host.
///
/// A pool shares one port (DESIGN "Multi-Endpoint Load Balancing"), so the two
/// endpoints differ in host only: the round-robin rotation and the
/// `X-OAGW-Target-Host` selection are what distinguish them.
fn explicit_pool_plane() -> ControlPlane {
    plane_with(
        tenant(),
        Some("pool.local.test"),
        &[
            // The hosts share no registrable domain suffix, so the alias is
            // explicit: `derive_alias` cannot derive one from the pool.
            (EndpointScheme::Http, "a.example.test", 8081),
            (EndpointScheme::Http, "b.example.org", 8081),
        ],
        &[Template::CatchAll],
    )
}

/// A two-endpoint upstream whose alias is the common suffix of both hosts.
fn common_suffix_plane() -> ControlPlane {
    // Port 80 is the scheme default, so the derived alias carries no port
    // suffix and the explicit alias `vendor.test` matches the derivation.
    plane_with(
        tenant(),
        Some("vendor.test"),
        &[
            (EndpointScheme::Http, "us.vendor.test", 80),
            (EndpointScheme::Http, "eu.vendor.test", 80),
        ],
        &[Template::CatchAll],
    )
}

// ── Path matching ────────────────────────────────────────────────────────────

#[test]
fn an_append_mode_route_matches_its_prefix_and_segment_boundaries() {
    for (prefix, suffix, expected) in [
        ("/v1/chat", "/v1/chat", true),
        ("/v1/chat", "/v1/chat/completions", true),
        ("/v1/chat", "/v1/chatfoo", false),
        ("/v1/chat", "/v1", false),
        ("/v1/chat", "", false),
        ("/", "/anything", true),
        ("/", "", true),
    ] {
        assert_eq!(
            path_matches(prefix, PathSuffixMode::Append, suffix),
            expected,
            "append: `{prefix}` against `{suffix}`"
        );
    }
}

#[test]
fn a_disabled_suffix_mode_only_serves_the_exact_prefix() {
    for (prefix, suffix, expected) in [
        ("/v1/health", "/v1/health", true),
        ("/v1/health", "/v1/health/live", false),
        ("/v1/health", "", false),
        ("/", "/", true),
        ("/", "/x", false),
    ] {
        assert_eq!(
            path_matches(prefix, PathSuffixMode::Disabled, suffix),
            expected,
            "disabled: `{prefix}` against `{suffix}`"
        );
    }
}

#[test]
fn path_suffix_mode_defaults_to_append() {
    // route.v1 declares `default: append`; the model must agree, otherwise a
    // route without the member would stop serving its own suffixes.
    let spec: RouteSpec = serde_json::from_value(serde_json::json!({
        "upstream_id": upstream_gts_id(Uuid::from_u128(1)),
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    }))
    .expect("a minimal route document");
    let http = spec.match_rules.http().expect("an HTTP match");
    assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
    assert!(http.query_allowlist.is_empty());
}

// ── Alias lookup ─────────────────────────────────────────────────────────────

#[test]
fn an_unknown_alias_is_a_404_with_the_alias_extension() {
    let plane = loopback_plane();
    let error = resolve(
        &plane,
        &config_with(false, true),
        "no-such-alias.test",
        "GET",
        "/",
        None,
    )
    .expect_err("an unknown alias must be rejected");

    assert_eq!(error.status_code(), 404);
    assert_eq!(error.gts_type(), ROUTE_NOT_FOUND_TYPE);
    assert_eq!(
        error.extensions().alias.as_deref(),
        Some("no-such-alias.test")
    );
}

#[test]
fn alias_normalization_is_applied_before_the_lookup() {
    let plane = loopback_plane();
    let target = resolve(
        &plane,
        &config_with(false, true),
        "API.LOCAL.TEST.",
        "GET",
        "/",
        None,
    )
    .expect("a normalized alias resolves");
    assert_eq!(target.upstream.alias, "api.local.test");
}

#[test]
fn alias_resolution_is_tenant_scoped() {
    let plane = loopback_plane();
    let error = resolve_proxy_target(
        &plane,
        &config_with(false, true),
        other_tenant(),
        "api.local.test",
        "GET",
        "/",
        None,
    )
    .expect_err("another tenant's upstream must be invisible");
    assert_eq!(error.status_code(), 404);
    assert_eq!(error.gts_type(), ROUTE_NOT_FOUND_TYPE);
}

#[test]
fn a_disabled_upstream_is_a_503_link_unavailable() {
    let plane = loopback_plane();
    let upstream = plane
        .find_upstream_by_alias(tenant(), "api.local.test")
        .expect("the test upstream");
    let mut document = upstream.spec.clone();
    document.enabled = false;
    plane
        .update_upstream(tenant(), upstream.id, document)
        .expect("the upstream to be disabled");

    let error = resolve(
        &plane,
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/",
        None,
    )
    .expect_err("a disabled upstream accepts no traffic");

    assert_eq!(error.status_code(), 503);
    assert_eq!(error.gts_type(), LINK_UNAVAILABLE_TYPE);
    assert_eq!(error.extensions().alias.as_deref(), Some("api.local.test"));
}

// ── Route matching ───────────────────────────────────────────────────────────

#[test]
fn the_longest_matching_prefix_wins() {
    let plane = plane_with(
        tenant(),
        Some("api.local.test"),
        &[(EndpointScheme::Http, "127.0.0.1", 8080)],
        &[
            Template::Http(&[HttpMethod::Get], "/v1", PathSuffixMode::Append),
            Template::Http(&[HttpMethod::Get], "/v1/chat", PathSuffixMode::Append),
        ],
    );
    let target = resolve(
        &plane,
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/v1/chat/completions",
        None,
    )
    .expect("the longest prefix must win");
    let http = target.route.spec.match_rules.http().expect("an HTTP route");
    assert_eq!(http.path, "/v1/chat");
}

#[test]
fn a_route_only_serves_its_own_methods() {
    let plane = plane_with(
        tenant(),
        Some("api.local.test"),
        &[(EndpointScheme::Http, "127.0.0.1", 8080)],
        &[Template::Http(
            &[HttpMethod::Get, HttpMethod::Post],
            "/v1/chat",
            PathSuffixMode::Append,
        )],
    );
    for method in ["GET", "POST", "get"] {
        resolve(
            &plane,
            &config_with(false, true),
            "api.local.test",
            method,
            "/v1/chat",
            None,
        )
        .unwrap_or_else(|error| panic!("{method} must be served: {error}"));
    }
    let error = resolve(
        &plane,
        &config_with(false, true),
        "api.local.test",
        "DELETE",
        "/v1/chat",
        None,
    )
    .expect_err("an unlisted method has no route");
    assert_eq!(error.status_code(), 404);
    assert_eq!(error.gts_type(), ROUTE_NOT_FOUND_TYPE);

    let expected = plane
        .find_upstream_by_alias(tenant(), "api.local.test")
        .map(|upstream| upstream_gts_id(upstream.id))
        .expect("the test upstream");
    assert_eq!(
        error.extensions().upstream_id.as_deref(),
        Some(expected.as_str())
    );
}

#[test]
fn a_grpc_route_never_serves_an_http_proxy_request() {
    let plane = plane_with(
        tenant(),
        Some("grpc.local.test"),
        &[(EndpointScheme::Grpc, "127.0.0.1", 9090)],
        &[Template::Grpc("cf.example.Echo", "Echo")],
    );
    let error = resolve(
        &plane,
        &config_with(false, true),
        "grpc.local.test",
        "POST",
        "/cf.example.Echo/Echo",
        None,
    )
    .expect_err("gRPC matching is Phase 3");
    assert_eq!(error.status_code(), 404);
    assert_eq!(error.gts_type(), ROUTE_NOT_FOUND_TYPE);
}

#[test]
fn a_suffix_beyond_a_disabled_mode_prefix_is_not_served() {
    let plane = plane_with(
        tenant(),
        Some("api.local.test"),
        &[(EndpointScheme::Http, "127.0.0.1", 8080)],
        &[Template::Http(
            &[HttpMethod::Get],
            "/v1/health",
            PathSuffixMode::Disabled,
        )],
    );
    resolve(
        &plane,
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/v1/health",
        None,
    )
    .expect("the route serves its own prefix");

    let error = resolve(
        &plane,
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/v1/health/live",
        None,
    )
    .expect_err("a disabled suffix mode rejects the deeper path");
    assert_eq!(error.status_code(), 404);
}

#[test]
fn a_request_without_a_route_is_a_404() {
    let plane = plane_with(
        tenant(),
        Some("api.local.test"),
        &[(EndpointScheme::Http, "127.0.0.1", 8080)],
        &[Template::Http(
            &[HttpMethod::Get],
            "/v1/x",
            PathSuffixMode::Append,
        )],
    );
    let error = resolve(
        &plane,
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/other",
        None,
    )
    .expect_err("an unmatched suffix has no route");
    assert_eq!(error.status_code(), 404);
    assert_eq!(error.gts_type(), ROUTE_NOT_FOUND_TYPE);
    assert!(error.extensions().upstream_id.is_some());
}

// ── X-OAGW-Target-Host matrix ────────────────────────────────────────────────

#[test]
fn a_single_endpoint_is_used_without_the_header() {
    let target = resolve(
        &loopback_plane(),
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/",
        None,
    )
    .expect("a single endpoint needs no header");
    assert_eq!(target.endpoint.port, 8080);
    assert_eq!(normalize_host(&target.endpoint.host), "127.0.0.1");
}

#[test]
fn a_single_endpoint_still_validates_the_header() {
    let error = resolve(
        &loopback_plane(),
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/",
        Some("other.local.test"),
    )
    .expect_err("a header that names no endpoint is rejected");
    assert_eq!(error.status_code(), 400);
    assert_eq!(error.gts_type(), UNKNOWN_TARGET_HOST_TYPE);
    assert_eq!(
        hosts_of(error.extensions().valid_hosts.as_deref()),
        Some(vec!["127.0.0.1"])
    );
}

#[test]
fn an_explicit_alias_rotates_over_the_pool_without_the_header() {
    let plane = explicit_pool_plane();
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..4 {
        let target = resolve(
            &plane,
            &config_with(false, true),
            "pool.local.test",
            "GET",
            "/",
            None,
        )
        .expect("an explicit alias rotates");
        seen.push(target.endpoint.host.clone());
    }
    assert_eq!(
        seen,
        [
            "a.example.test",
            "b.example.org",
            "a.example.test",
            "b.example.org"
        ]
    );
}

#[test]
fn a_common_suffix_alias_makes_the_header_mandatory() {
    let plane = common_suffix_plane();
    let error = resolve(
        &plane,
        &config_with(false, true),
        "vendor.test",
        "GET",
        "/",
        None,
    )
    .expect_err("a common-suffix alias needs the header");

    assert_eq!(error.status_code(), 400);
    assert_eq!(error.gts_type(), MISSING_TARGET_HOST_TYPE);
    assert_eq!(error.extensions().alias.as_deref(), Some("vendor.test"));
    assert_eq!(
        hosts_of(error.extensions().valid_hosts.as_deref()),
        Some(vec!["us.vendor.test", "eu.vendor.test"])
    );
}

#[test]
fn a_common_suffix_alias_accepts_the_bare_alias_host_as_a_target() {
    // An endpoint may carry the alias host itself; the bare alias host then
    // satisfies the mandatory header and selects that endpoint.
    let plane = plane_with(
        tenant(),
        Some("vendor.test"),
        &[
            (EndpointScheme::Http, "vendor.test", 80),
            (EndpointScheme::Http, "us.vendor.test", 80),
        ],
        &[Template::CatchAll],
    );
    let target = resolve(
        &plane,
        &config_with(false, true),
        "vendor.test",
        "GET",
        "/",
        Some("vendor.test"),
    )
    .expect("the alias host is one of the endpoints");
    assert_eq!(target.endpoint.host, "vendor.test");
}

#[test]
fn the_header_selects_the_named_endpoint_case_insensitively() {
    let plane = explicit_pool_plane();
    for (value, host) in [
        ("A.EXAMPLE.TEST.", "a.example.test"),
        ("b.example.org", "b.example.org"),
        ("B.Example.ORG", "b.example.org"),
    ] {
        let target = resolve(
            &plane,
            &config_with(false, true),
            "pool.local.test",
            "GET",
            "/",
            Some(value),
        )
        .unwrap_or_else(|error| panic!("{value} must select an endpoint: {error}"));
        assert_eq!(target.endpoint.host, host, "header value `{value}`");
    }
}

#[test]
fn an_unknown_header_value_is_rejected_with_valid_hosts() {
    let plane = explicit_pool_plane();
    let error = resolve(
        &plane,
        &config_with(false, true),
        "pool.local.test",
        "GET",
        "/",
        Some("c.local.test"),
    )
    .expect_err("only configured endpoints may be pinned");
    assert_eq!(error.status_code(), 400);
    assert_eq!(error.gts_type(), UNKNOWN_TARGET_HOST_TYPE);
    assert_eq!(
        hosts_of(error.extensions().valid_hosts.as_deref()),
        Some(vec!["a.example.test", "b.example.org"])
    );
    assert_eq!(error.extensions().alias.as_deref(), Some("pool.local.test"));
    assert!(error.extensions().upstream_id.is_some());
}

#[test]
fn a_non_bare_header_value_is_rejected() {
    let plane = loopback_plane();
    for value in [
        "api.local.test:8443",
        "https://api.local.test",
        "api.local.test/",
        "a b",
        "",
    ] {
        let error = resolve(
            &plane,
            &config_with(false, true),
            "api.local.test",
            "GET",
            "/",
            Some(value),
        )
        .expect_err("only a bare hostname is a valid target host");
        assert_eq!(error.status_code(), 400, "value `{value}`");
        assert_eq!(
            error.gts_type(),
            INVALID_TARGET_HOST_TYPE,
            "value `{value}`"
        );
        assert_eq!(error.extensions().invalid_value.as_deref(), Some(value));
    }
}

#[test]
fn target_host_values_are_canonicalized() {
    assert_eq!(
        parse_target_host("API.Example.COM.")
            .expect("a hostname is valid")
            .as_str(),
        "api.example.com"
    );
    assert_eq!(
        parse_target_host("[::1]")
            .expect("a bracketed IPv6 address is valid")
            .as_str(),
        "::1"
    );
    assert_eq!(
        parse_target_host(" 10.0.0.1 ")
            .expect("surrounding blanks are trimmed")
            .as_str(),
        "10.0.0.1"
    );
    let error = parse_target_host("host_underscore").expect_err("a label may not hold `_`");
    assert_eq!(error.status_code(), 400);
}

// ── SSRF and scheme policies ─────────────────────────────────────────────────

#[test]
fn the_ssrf_policy_refuses_private_loopback_and_link_local_endpoints() {
    for host in ["127.0.0.1", "10.1.2.3", "192.168.5.5", "169.254.10.11"] {
        let plane = catch_all_plane("api.local.test", &[(EndpointScheme::Http, host, 8080)]);
        let error = resolve(
            &plane,
            &config_with(true, true),
            "api.local.test",
            "GET",
            "/",
            None,
        )
        .expect_err("{host} must be refused by the SSRF policy");
        assert_eq!(error.status_code(), 400, "host `{host}`");
        assert_eq!(error.gts_type(), VALIDATION_ERROR_TYPE, "host `{host}`");
        assert_eq!(error.extensions().host.as_deref(), Some(host));
    }
}

#[test]
fn the_ssrf_policy_refuses_unique_local_ipv6_endpoints() {
    let plane = catch_all_plane("api.local.test", &[(EndpointScheme::Http, "fd00::1", 8080)]);
    let error = resolve(
        &plane,
        &config_with(true, true),
        "api.local.test",
        "GET",
        "/",
        None,
    )
    .expect_err("a unique-local address must be refused");
    assert_eq!(error.status_code(), 400);
    assert_eq!(error.extensions().host.as_deref(), Some("fd00::1"));
}

#[test]
fn the_ssrf_policy_allows_public_addresses_and_hostnames() {
    // A hostname endpoint derives its alias, so it is created on the scheme's
    // default port (80 for `http`) and addressed by its own host.
    for (host, port, alias) in [
        ("93.184.216.34", 8080, "api.local.test"),
        ("upstream.example.test", 80, "upstream.example.test"),
    ] {
        let plane = catch_all_plane(alias, &[(EndpointScheme::Http, host, port)]);
        let target = resolve(&plane, &config_with(true, true), alias, "GET", "/", None)
            .unwrap_or_else(|error| panic!("{host} must be dialable: {error}"));
        assert_eq!(normalize_host(&target.endpoint.host), host);
    }
}

#[test]
fn the_ssrf_policy_can_be_disabled() {
    let plane = catch_all_plane(
        "api.local.test",
        &[(EndpointScheme::Http, "10.1.2.3", 8080)],
    );
    let target = resolve(
        &plane,
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/",
        None,
    )
    .expect("a disabled policy dials the endpoint");
    assert_eq!(normalize_host(&target.endpoint.host), "10.1.2.3");
}

#[test]
fn plaintext_upstreams_are_gated_by_the_configuration() {
    let plane = catch_all_plane(
        "upstream.example.test",
        &[(EndpointScheme::Http, "upstream.example.test", 80)],
    );

    let error = resolve(
        &plane,
        &config_with(false, false),
        "upstream.example.test",
        "GET",
        "/",
        None,
    )
    .expect_err("a plaintext upstream is not dialable by default");
    assert_eq!(error.status_code(), 503);
    assert_eq!(error.gts_type(), LINK_UNAVAILABLE_TYPE);
    assert_eq!(
        error.extensions().host.as_deref(),
        Some("upstream.example.test")
    );

    let target = resolve(
        &plane,
        &config_with(false, true),
        "upstream.example.test",
        "GET",
        "/",
        None,
    )
    .expect("allow_http_upstream dials the plaintext endpoint");
    assert_eq!(target.endpoint.port, 80);
}

#[test]
fn the_scheme_policy_only_gates_plaintext_endpoints() {
    for (scheme, allow_http, blocked) in [
        (EndpointScheme::Http, false, true),
        (EndpointScheme::Http, true, false),
        (EndpointScheme::Https, false, false),
        (EndpointScheme::Grpc, false, false),
    ] {
        let endpoint = Endpoint {
            scheme,
            host: "upstream.example.test".to_owned(),
            port: 8080,
        };
        assert_eq!(
            enforce_scheme_policy(&config_with(false, allow_http), &endpoint).is_err(),
            blocked,
            "scheme `{}` with allow_http_upstream = {allow_http}",
            scheme.name()
        );
    }
}

#[test]
fn the_ssrf_helpers_pass_hostname_endpoints_through() {
    // No DNS on the hot path: a hostname is pinned and checked by the transport
    // layer, so resolution lets it through even with the policy enabled.
    let config = config_with(true, true);
    let hostname = Endpoint {
        scheme: EndpointScheme::Http,
        host: "internal.service".to_owned(),
        port: 8080,
    };
    enforce_ssrf_policy(&config, &hostname).expect("a hostname is not resolved here");

    let loopback = Endpoint {
        scheme: EndpointScheme::Http,
        host: "127.0.0.1".to_owned(),
        port: 8080,
    };
    let error = enforce_ssrf_policy(&config, &loopback).expect_err("a loopback is refused");
    assert_eq!(error.extensions().host.as_deref(), Some("127.0.0.1"));
}

// ── Endpoint selection helper ────────────────────────────────────────────────

#[test]
fn endpoint_selection_names_the_upstream_it_evaluated() {
    let plane = explicit_pool_plane();
    let upstream = plane
        .find_upstream_by_alias(tenant(), "pool.local.test")
        .expect("the test upstream");
    let error = select_endpoint(
        &plane,
        upstream_gts_id(upstream.id),
        &upstream,
        "pool.local.test",
        Some("c.local.test"),
    )
    .expect_err("the header must name a configured endpoint");
    assert_eq!(error.gts_type(), UNKNOWN_TARGET_HOST_TYPE);
    assert_eq!(
        error.extensions().upstream_id.as_deref(),
        Some(upstream_gts_id(upstream.id).as_str())
    );
}

#[test]
fn endpoint_selection_fails_for_an_endpoint_less_upstream() {
    let plane = loopback_plane();
    let mut upstream = plane
        .find_upstream_by_alias(tenant(), "api.local.test")
        .expect("the test upstream");
    upstream.spec.server.endpoints.clear();
    let error = select_endpoint(
        &plane,
        upstream_gts_id(upstream.id),
        &upstream,
        "api.local.test",
        None,
    )
    .expect_err("an endpoint-less upstream cannot be dialed");
    assert_eq!(error.status_code(), 500);
}

#[test]
fn resolution_returns_upstream_endpoint_and_route_together() {
    let target = resolve(
        &loopback_plane(),
        &config_with(false, true),
        "api.local.test",
        "GET",
        "/status",
        None,
    )
    .expect("the request resolves");
    assert_eq!(target.upstream.alias, "api.local.test");
    assert_eq!(target.endpoint.port, 8080);
    assert!(target.route.enabled);
    assert_eq!(
        target.route.spec.upstream_id,
        upstream_gts_id(target.upstream.id)
    );
}
