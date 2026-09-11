//! Unit tests for the data-plane service's own helpers.

use super::*;
use axum::http::HeaderMap;

fn request(method: &str, is_preflight: bool) -> ProxyRequest {
    ProxyRequest {
        tenant_id: Uuid::nil(),
        alias: "api.example.com".to_owned(),
        path_suffix: "v1/chat".to_owned(),
        query: String::new(),
        method: method.to_owned(),
        headers: HeaderMap::new(),
        body: Bytes::new(),
        target_host: None,
        request_id: "req-1".to_owned(),
        is_upgrade: false,
        is_preflight,
    }
}

#[test]
fn a_plain_request_is_matched_by_its_own_method() {
    assert_eq!(negotiated_method(&request("POST", false)), "POST");
}

#[test]
fn a_preflight_is_matched_by_the_method_it_asks_about() {
    let mut request = request("OPTIONS", true);
    request
        .headers
        .insert("access-control-request-method", "DELETE".parse().unwrap());
    assert_eq!(negotiated_method(&request), "DELETE");
}

#[test]
fn a_preflight_without_a_negotiated_method_falls_back_to_options() {
    assert_eq!(negotiated_method(&request("OPTIONS", true)), "OPTIONS");
}

#[test]
fn a_blank_negotiated_method_falls_back_to_options() {
    let mut request = request("OPTIONS", true);
    request
        .headers
        .insert("access-control-request-method", "   ".parse().unwrap());
    assert_eq!(negotiated_method(&request), "OPTIONS");
}

#[test]
fn the_outbound_path_joins_the_prefix_and_the_suffix() {
    assert_eq!(
        outbound_path("/v1/chat", "completions"),
        "/v1/chat/completions"
    );
    assert_eq!(
        outbound_path("/v1/chat/", "completions"),
        "/v1/chat/completions"
    );
    assert_eq!(outbound_path("", "v1/things"), "/v1/things");
    assert_eq!(outbound_path("/v1/chat", ""), "/v1/chat");
    assert_eq!(outbound_path("", ""), "/");
}

#[test]
fn the_matcher_path_is_always_slash_prefixed() {
    let request = request("POST", false);
    let _ = &request;
    assert_eq!(super::match_path("v1/chat"), "/v1/chat");
    assert_eq!(super::match_path("/v1/chat"), "/v1/chat");
}

#[test]
fn the_tenant_chain_walks_to_the_anonymous_tenant() {
    let tenant = Uuid::from_u128(7);
    assert_eq!(tenant_chain(tenant), vec![tenant, Uuid::nil()]);
    assert_eq!(tenant_chain(Uuid::nil()), vec![Uuid::nil()]);
}

#[test]
fn a_credential_names_the_principal_and_its_absence_names_the_tenant() {
    let mut identified = request("POST", false);
    identified
        .headers
        .insert("authorization", "Bearer sk-value".parse().unwrap());
    assert!(
        principal_of(&identified).starts_with("key:"),
        "the credential names the principal"
    );
    let anonymous = request("POST", false);
    assert!(principal_of(&anonymous).starts_with("tenant:"));
}

mod target_host {
    //! The `X-OAGW-Target-Host` contract of ADR-0001's behaviour matrix.

    use super::*;
    use crate::domain::alias::DerivedAlias;
    use crate::domain::model::{Endpoint, EndpointScheme, PluginsConfig, Protocol, ServerConfig};
    use crate::infra::memory_repo::{MemoryRouteRepo, MemoryUpstreamRepo};

    /// A data plane with no plugins and no credentials, so only routing runs.
    fn plane() -> DataPlaneServiceImpl {
        DataPlaneServiceImpl::new(
            Arc::new(MemoryUpstreamRepo::new()),
            Arc::new(MemoryRouteRepo::new()),
            Arc::new(crate::domain::plugin::test_support::empty_resolver()),
            AuthPluginRegistry::new(),
            GuardPluginRegistry::new(),
            TransformPluginRegistry::new(),
            Arc::new(RateLimiter::new()),
            OutboundClient::new(Duration::from_secs(5), true),
            Arc::new(Metrics::new()),
        )
    }

    fn endpoint(host: &str) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Http,
            host: host.to_owned(),
            port: 80,
        }
    }

    fn upstream(alias: &str, hosts: &[&str]) -> Upstream {
        Upstream {
            id: "up-1".to_owned(),
            tenant_id: Uuid::nil(),
            alias: alias.to_owned(),
            enabled: true,
            protocol: Protocol::Http,
            server: ServerConfig {
                endpoints: hosts.iter().map(|host| endpoint(host)).collect(),
            },
            auth: None,
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: Vec::new(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn a_single_endpoint_ignores_the_header_when_it_is_absent() {
        let upstream = upstream("api.example.com", &["api.example.com"]);
        assert_eq!(
            plane()
                .select_endpoint(&upstream, None)
                .expect("one endpoint needs no header")
                .normalised_host(),
            "api.example.com"
        );
    }

    #[test]
    fn a_single_endpoint_validates_the_header_when_it_is_present() {
        let upstream = upstream("api.example.com", &["api.example.com"]);
        let error = plane()
            .select_endpoint(&upstream, Some("api.example.com:8080"))
            .expect_err("a value with a port is not a bare hostname");
        assert!(
            matches!(error, OagwError::InvalidTargetHost(_)),
            "{error:?}"
        );
    }

    #[test]
    fn an_explicit_multi_endpoint_alias_round_robins_without_a_header() {
        let upstream = upstream(
            "my-service",
            &["server-a.example.com", "server-b.example.com"],
        );
        let service = plane();
        let first = service
            .select_endpoint(&upstream, None)
            .expect("an explicit alias needs no header");
        let second = service
            .select_endpoint(&upstream, None)
            .expect("an explicit alias needs no header");
        assert_ne!(
            first.normalised_host(),
            second.normalised_host(),
            "the two calls spread over the pool"
        );
    }

    #[test]
    fn a_suffix_alias_requires_the_header() {
        let upstream = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        assert!(
            matches!(
                crate::domain::alias::derive(&upstream.server.endpoints),
                DerivedAlias::Derived(_)
            ),
            "the pool's alias is its common suffix"
        );
        let error = plane()
            .select_endpoint(&upstream, None)
            .expect_err("a common-suffix alias names no endpoint on its own");
        assert!(
            matches!(error, OagwError::MissingTargetHost(_)),
            "{error:?}"
        );
    }

    #[test]
    fn a_suffix_alias_routes_to_the_named_endpoint() {
        let upstream = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let chosen = plane()
            .select_endpoint(&upstream, Some(" eu.vendor.com "))
            .expect("a bare hostname resolves inside the pool");
        assert_eq!(chosen.normalised_host(), "eu.vendor.com");
    }

    #[test]
    fn a_header_with_a_port_or_a_path_is_invalid() {
        let upstream = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        for value in ["us.vendor.com:443", "us.vendor.com/", "us vendor.com"] {
            let error = plane()
                .select_endpoint(&upstream, Some(value))
                .expect_err("the value is not a bare hostname");
            assert!(
                matches!(error, OagwError::InvalidTargetHost(_)),
                "{value} read as {error:?}"
            );
        }
    }

    #[test]
    fn an_unknown_but_well_formed_host_is_reported_as_unknown() {
        let upstream = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let error = plane()
            .select_endpoint(&upstream, Some("apac.vendor.com"))
            .expect_err("the host is not an endpoint");
        assert!(
            matches!(error, OagwError::UnknownTargetHost(_)),
            "{error:?}"
        );
    }

    #[test]
    fn an_ip_literal_pool_still_round_robins() {
        let upstream = upstream("10.0.0.1", &["10.0.0.1", "10.0.0.2"]);
        assert!(
            matches!(
                crate::domain::alias::derive(&upstream.server.endpoints),
                DerivedAlias::NotDerivable(_)
            ),
            "an IP pool has no registrable suffix"
        );
        assert!(
            plane().select_endpoint(&upstream, None).is_ok(),
            "an IP pool has no suffix to disambiguate"
        );
    }

    #[test]
    fn a_reserved_auth_identifier_has_no_implementation() {
        // FR-036: `basic` and `bearer` are catalogue-only identifiers, so a
        // configuration that selects either is refused rather than executed.
        for identifier in [
            crate::gts_helpers::AUTH_BASIC,
            crate::gts_helpers::AUTH_BEARER,
        ] {
            let Err(error) = AuthPluginRegistry::new().get(identifier) else {
                panic!("{identifier} must not resolve");
            };
            assert!(
                matches!(error, OagwError::PluginNotFound(_)),
                "{identifier} read as {error:?}"
            );
        }
    }
}
