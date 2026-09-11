//! Tests for the OAGW domain model: schema round-trip, alias rules and
//! resource identifiers.

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    BoundPluginBinding, DerivedAlias, Endpoint, EndpointScheme, PluginBinding, PluginChain,
    UPSTREAM_ID_PREFIX, UpstreamProtocol, UpstreamSpec, derive_alias, enforce_alias_update,
    is_ip_address, is_valid_alias, is_valid_hostname, is_valid_tag, normalize_alias,
    parse_resource_id, resolve_alias, upstream_gts_id,
};
use crate::error::OagwError;

// ── Fixtures ─────────────────────────────────────────────────────────────────

/// Full upstream document shaped after `upstream.v1.schema.json`.
fn full_upstream_json() -> Value {
    json!({
        "alias": "api.openai.com",
        "tags": ["openai", "llm"],
        "server": {
            "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }]
        },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "enabled": true,
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "sharing": "private",
            "config": { "header": "authorization" }
        },
        "headers": {
            "request": {
                "set": { "x-forwarded-proto": "https" },
                "remove": ["x-internal-marker"],
                "passthrough": "allowlist",
                "passthrough_allowlist": ["accept", "content-type"]
            },
            "response": { "add": { "x-served-by": "oagw" } }
        },
        "plugins": {
            "sharing": "inherit",
            "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"]
        },
        "rate_limit": {
            "sharing": "enforce",
            "algorithm": "token_bucket",
            "sustained": { "rate": 100, "window": "minute" },
            "burst": { "capacity": 200 },
            "scope": "tenant",
            "strategy": "reject",
            "cost": 2
        },
        "cors": {
            "sharing": "private",
            "enabled": true,
            "allowed_origins": ["https://console.example.com"],
            "allowed_methods": ["GET", "POST"],
            "expose_headers": ["x-request-id"],
            "allow_credentials": true
        }
    })
}

fn endpoint(host: &str, port: u16, scheme: EndpointScheme) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn spec_from_json(body: Value) -> UpstreamSpec {
    serde_json::from_value(body).expect("upstream document must deserialize")
}

// ── Schema round-trip ────────────────────────────────────────────────────────

#[test]
fn upstream_spec_round_trips_the_schema_document() {
    let document = full_upstream_json();
    let spec = spec_from_json(document.clone());

    spec.validate().expect("full document must validate");
    assert_eq!(spec.alias.as_deref(), Some("api.openai.com"));
    assert_eq!(spec.protocol, UpstreamProtocol::Http);
    assert!(spec.enabled);
    assert_eq!(
        spec.auth
            .as_ref()
            .and_then(|auth| auth.auth_type.as_deref()),
        Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
    );
    assert_eq!(
        spec.rate_limit
            .as_ref()
            .map(|rate_limit| rate_limit.sustained.rate),
        Some(100)
    );

    let encoded: Value = serde_json::to_value(&spec).expect("spec must serialize");
    assert_eq!(encoded, document, "round-trip must be lossless");
}

#[test]
fn endpoint_defaults_follow_the_schema() {
    let spec = spec_from_json(json!({
        "server": { "endpoints": [{ "host": "api.openai.com" }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    }));

    spec.validate().expect("minimal document must validate");
    let endpoint = &spec.server.endpoints[0];
    assert_eq!(endpoint.scheme, EndpointScheme::Https);
    assert_eq!(endpoint.port, 443);
    assert!(spec.enabled, "enabled defaults to true");
    assert!(spec.tags.is_empty());
    assert!(spec.auth.is_none());
    assert!(spec.headers.is_none());
    assert!(spec.plugins.is_none());
    assert!(spec.rate_limit.is_none());
    assert!(spec.cors.is_none());
}

#[test]
fn plaintext_http_scheme_is_accepted_at_create_time() {
    let spec = spec_from_json(json!({
        "alias": "mock.internal",
        "server": { "endpoints": [{ "scheme": "http", "host": "mock.internal", "port": 8080 }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    }));

    spec.validate()
        .expect("http scheme must be legal at create time");
    assert_eq!(spec.server.endpoints[0].scheme, EndpointScheme::Http);
    let encoded = serde_json::to_value(&spec).expect("spec must serialize");
    assert_eq!(encoded["server"]["endpoints"][0]["scheme"], "http");
}

#[test]
fn required_members_are_enforced_by_serde() {
    for missing in ["server", "protocol"] {
        let mut document = full_upstream_json();
        document.as_object_mut().expect("object").remove(missing);
        let error = serde_json::from_value::<UpstreamSpec>(document)
            .expect_err("missing required member must be rejected");
        assert!(
            error.to_string().contains("missing field"),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn unknown_fields_are_rejected() {
    let mut document = full_upstream_json();
    document["unknown_member"] = json!(true);
    let error = serde_json::from_value::<UpstreamSpec>(document)
        .expect_err("unknown member must be rejected");
    assert!(
        error.to_string().contains("unknown field"),
        "unexpected: {error}"
    );
}

#[test]
fn unknown_endpoint_fields_are_rejected() {
    let error = serde_json::from_value::<UpstreamSpec>(json!({
        "server": { "endpoints": [{ "host": "api.openai.com", "tls": true }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    }))
    .expect_err("unknown endpoint member must be rejected");
    assert!(
        error.to_string().contains("unknown field"),
        "unexpected: {error}"
    );
}

#[test]
fn schema_validation_rejects_bad_ports_tags_and_rate_limits() {
    let cases: [(Value, &str); 6] = [
        (
            json!({ "server": { "endpoints": [] }, "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" }),
            "at least one endpoint",
        ),
        (
            json!({ "server": { "endpoints": [{ "host": "api.openai.com", "port": 0 }] }, "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" }),
            "between 1 and 65535",
        ),
        (
            json!({ "tags": ["OpenAI"], "server": { "endpoints": [{ "host": "api.openai.com" }] }, "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" }),
            "^[a-z0-9_-]+$",
        ),
        (
            json!({ "server": { "endpoints": [{ "host": "api.openai.com" }] }, "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "rate_limit": { "sustained": { "rate": 0 } } }),
            "sustained.rate must be at least 1",
        ),
        (
            json!({ "server": { "endpoints": [{ "host": "api.openai.com" }] }, "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "rate_limit": { "sustained": { "rate": 5 }, "burst": { "capacity": 0 } } }),
            "burst.capacity must be at least 1",
        ),
        (
            json!({ "server": { "endpoints": [{ "host": "api.openai.com" }] }, "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "rate_limit": { "sustained": { "rate": 5 }, "cost": 0 } }),
            "cost must be at least 1",
        ),
    ];

    for (document, expected) in cases {
        let spec = spec_from_json(document);
        let error = spec.validate().expect_err("document must be rejected");
        assert!(
            error.detail().contains(expected),
            "expected `{expected}` in `{}`",
            error.detail()
        );
        assert_eq!(error.status_code(), 400);
    }
}

#[test]
fn schema_validation_rejects_bad_hosts_and_cors() {
    let bad_host = spec_from_json(json!({
        "server": { "endpoints": [{ "host": "api..openai.com" }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    }));
    let error = bad_host.validate().expect_err("bad host must be rejected");
    assert!(error.detail().contains("RFC 1123"), "{}", error.detail());

    let bad_method = spec_from_json(json!({
        "server": { "endpoints": [{ "host": "api.openai.com" }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "cors": { "enabled": true, "allowed_methods": ["TRACE"] }
    }));
    let error = bad_method
        .validate()
        .expect_err("bad method must be rejected");
    assert!(error.detail().contains("TRACE"), "{}", error.detail());

    let wildcard_credentials = spec_from_json(json!({
        "server": { "endpoints": [{ "host": "api.openai.com" }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "cors": { "enabled": true, "allowed_origins": ["*"], "allow_credentials": true }
    }));
    let error = wildcard_credentials
        .validate()
        .expect_err("credentials with `*` must be rejected");
    assert!(
        error.detail().contains("allow_credentials"),
        "{}",
        error.detail()
    );
}

// ── Host and tag helpers ─────────────────────────────────────────────────────

#[test]
fn host_helpers_classify_rfc1123_hostnames_and_ips() {
    for host in [
        "api.openai.com",
        "localhost",
        "a-b.c",
        "api.openai.com.",
        "US.Vendor.Com",
    ] {
        assert!(is_valid_hostname(host), "{host} must be a valid hostname");
    }
    for host in [
        "-api.openai.com",
        "api..openai.com",
        "api.openai.com-",
        "",
        "api_openai.com",
    ] {
        assert!(!is_valid_hostname(host), "{host} must be rejected");
    }
    assert!(
        !is_valid_hostname(&format!("{}.", "a".repeat(254))),
        "a 254-character hostname must be rejected"
    );
    assert!(is_ip_address("10.0.1.1"));
    assert!(is_ip_address("2001:db8::1"));
    assert!(!is_ip_address("api.openai.com"));
    assert!(is_valid_tag("openai"));
    assert!(is_valid_tag("llm-v1_beta"));
    assert!(!is_valid_tag("OpenAI"));
    assert!(!is_valid_tag(""));
}

#[test]
fn alias_normalization_is_case_insensitive_and_strips_trailing_dots() {
    assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
    assert_eq!(normalize_alias("  API.OPENAI.COM  "), "api.openai.com");
    assert!(is_valid_alias("api.openai.com"));
    assert!(is_valid_alias("a"));
    assert!(is_valid_alias("api.openai.com:8443"));
    assert!(
        is_valid_alias(&normalize_alias("Api.OpenAI.COM")),
        "aliases are normalized, never stored mixed-case"
    );
    assert!(!is_valid_alias("-api.openai.com"));
    assert!(!is_valid_alias("api.openai.com."));
    assert!(!is_valid_alias(""));
}

// ── Alias derivation ─────────────────────────────────────────────────────────

#[test]
fn single_hostname_endpoints_derive_hostname_or_hostname_port() {
    assert_eq!(
        derive_alias(&[endpoint("api.openai.com", 443, EndpointScheme::Https)]),
        DerivedAlias::Derived("api.openai.com".to_owned())
    );
    assert_eq!(
        derive_alias(&[endpoint("Api.OpenAI.Com.", 8443, EndpointScheme::Https)]),
        DerivedAlias::Derived("api.openai.com:8443".to_owned())
    );
    assert_eq!(
        derive_alias(&[endpoint("mock.internal", 80, EndpointScheme::Http)]),
        DerivedAlias::Derived("mock.internal".to_owned())
    );
    assert_eq!(
        derive_alias(&[endpoint("mock.internal", 8080, EndpointScheme::Http)]),
        DerivedAlias::Derived("mock.internal:8080".to_owned())
    );
}

#[test]
fn hostname_pools_derive_their_registrable_common_suffix() {
    assert_eq!(
        derive_alias(&[
            endpoint("us.vendor.com", 443, EndpointScheme::Https),
            endpoint("eu.vendor.com", 443, EndpointScheme::Https),
        ]),
        DerivedAlias::Derived("vendor.com".to_owned())
    );
    assert_eq!(
        derive_alias(&[
            endpoint("us.vendor.com", 8443, EndpointScheme::Https),
            endpoint("eu.vendor.com", 8443, EndpointScheme::Https),
        ]),
        DerivedAlias::Derived("vendor.com:8443".to_owned())
    );
}

#[test]
fn non_derivable_endpoint_sets_require_an_explicit_alias() {
    let cases = [
        // Bare public suffix: not a registrable domain.
        vec![
            endpoint("foo.co.uk", 443, EndpointScheme::Https),
            endpoint("bar.co.uk", 443, EndpointScheme::Https),
        ],
        // No common suffix at all.
        vec![
            endpoint("us.foo.com", 443, EndpointScheme::Https),
            endpoint("eu.bar.com", 443, EndpointScheme::Https),
        ],
        // IP addresses never derive an alias.
        vec![endpoint("10.0.1.1", 443, EndpointScheme::Https)],
        vec![
            endpoint("10.0.1.1", 443, EndpointScheme::Https),
            endpoint("10.0.1.2", 443, EndpointScheme::Https),
        ],
        // Two different non-standard ports: no single `suffix:port` alias.
        vec![
            endpoint("us.vendor.com", 8443, EndpointScheme::Https),
            endpoint("eu.vendor.com", 9443, EndpointScheme::Https),
        ],
    ];

    for endpoints in cases {
        assert_eq!(
            derive_alias(&endpoints),
            DerivedAlias::NotDerivable,
            "{endpoints:?}"
        );
    }
}

// ── Alias resolution (create) ────────────────────────────────────────────────

#[test]
fn resolve_alias_accepts_the_derived_value_and_rejects_overrides() {
    let hostname = [endpoint("api.openai.com", 443, EndpointScheme::Https)];

    assert_eq!(
        resolve_alias(None, &hostname).expect("derived alias"),
        "api.openai.com"
    );
    assert_eq!(
        resolve_alias(Some("api.openai.com"), &hostname).expect("idempotent alias"),
        "api.openai.com"
    );
    assert_eq!(
        resolve_alias(Some("API.OpenAI.COM."), &hostname).expect("normalized alias"),
        "api.openai.com"
    );

    let error =
        resolve_alias(Some("my-openai"), &hostname).expect_err("alias override must be rejected");
    assert_eq!(error.status_code(), 400);
    assert!(
        error.detail().contains("api.openai.com"),
        "{}",
        error.detail()
    );
}

#[test]
fn resolve_alias_requires_an_explicit_alias_for_non_derivable_endpoints() {
    let ips = [endpoint("10.0.1.1", 443, EndpointScheme::Https)];

    let error = resolve_alias(None, &ips).expect_err("missing alias must be rejected");
    assert_eq!(error.status_code(), 400);
    assert!(
        error.detail().contains("alias is required"),
        "{}",
        error.detail()
    );

    assert_eq!(
        resolve_alias(Some("My-Service"), &ips).expect("explicit alias"),
        "my-service"
    );
}

#[test]
fn resolve_alias_validates_the_alias_pattern() {
    let ips = [endpoint("10.0.1.1", 443, EndpointScheme::Https)];
    let error = resolve_alias(Some("-bad-"), &ips).expect_err("bad pattern must be rejected");
    assert_eq!(error.status_code(), 400);
    assert!(error.detail().contains("^[a-z0-9]"), "{}", error.detail());
    assert_eq!(error.extensions().invalid_value.as_deref(), Some("-bad-"));
}

// ── Alias immutability (update) ──────────────────────────────────────────────

fn spec_with(endpoints: Vec<Endpoint>, alias: Option<&str>) -> UpstreamSpec {
    UpstreamSpec {
        id: None,
        alias: alias.map(str::to_owned),
        tags: Vec::new(),
        server: super::ServerConfig { endpoints },
        protocol: UpstreamProtocol::Http,
        enabled: true,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

#[test]
fn update_keeps_the_alias_when_the_derivation_is_stable() {
    let current = [endpoint("api.openai.com", 443, EndpointScheme::Https)];

    assert_eq!(
        enforce_alias_update(
            "api.openai.com",
            &current,
            &spec_with(current.to_vec(), None)
        )
        .expect("no endpoint change"),
        "api.openai.com"
    );
    assert_eq!(
        enforce_alias_update(
            "api.openai.com",
            &current,
            &spec_with(current.to_vec(), Some("api.openai.com"))
        )
        .expect("exact-match alias is tolerated"),
        "api.openai.com"
    );
}

#[test]
fn update_rejects_alias_overrides() {
    let current = [endpoint("10.0.1.1", 443, EndpointScheme::Https)];
    let error = enforce_alias_update(
        "my-service",
        &current,
        &spec_with(current.to_vec(), Some("other")),
    )
    .expect_err("alias override must be rejected");
    assert_eq!(error.status_code(), 400);
    assert!(error.detail().contains("immutable"), "{}", error.detail());
    assert_eq!(error.extensions().alias.as_deref(), Some("my-service"));
}

#[test]
fn update_rejects_endpoint_changes_that_would_change_a_derived_alias() {
    let current = [endpoint("api.openai.com", 443, EndpointScheme::Https)];
    let renamed = [endpoint("api.anthropic.com", 443, EndpointScheme::Https)];
    let error = enforce_alias_update(
        "api.openai.com",
        &current,
        &spec_with(renamed.to_vec(), None),
    )
    .expect_err("derived alias change must be rejected");
    assert_eq!(error.status_code(), 400);
    assert!(
        error.detail().contains("delete and re-create"),
        "{}",
        error.detail()
    );

    // Hostname → IP is rejected even with an explicit alias that matches.
    let ips = [endpoint("10.0.1.1", 443, EndpointScheme::Https)];
    let error = enforce_alias_update(
        "api.openai.com",
        &current,
        &spec_with(ips.to_vec(), Some("api.openai.com")),
    )
    .expect_err("hostname to IP must be rejected");
    assert_eq!(error.status_code(), 400);
}

#[test]
fn update_allows_ip_pools_to_change_hosts_and_ip_to_hostname_when_stable() {
    // IP to IP keeps the existing alias.
    let current = [endpoint("10.0.1.1", 443, EndpointScheme::Https)];
    let moved = [endpoint("10.0.1.2", 443, EndpointScheme::Https)];
    assert_eq!(
        enforce_alias_update("my-service", &current, &spec_with(moved.to_vec(), None))
            .expect("IP to IP retains the alias"),
        "my-service"
    );

    // IP to hostname is allowed when the derivation equals the stored alias.
    let hostname = [endpoint("api.openai.com", 443, EndpointScheme::Https)];
    assert_eq!(
        enforce_alias_update(
            "api.openai.com",
            &current,
            &spec_with(hostname.to_vec(), None)
        )
        .expect("IP to hostname with a stable alias"),
        "api.openai.com"
    );
}

// ── Resource identifiers ─────────────────────────────────────────────────────

#[test]
fn resource_ids_accept_uuids_and_gts_identifiers() {
    let id = Uuid::new_v4();
    let gts = upstream_gts_id(id);
    assert!(gts.starts_with(UPSTREAM_ID_PREFIX));

    assert_eq!(
        parse_resource_id(UPSTREAM_ID_PREFIX, &id.to_string()).expect("bare UUID"),
        id
    );
    assert_eq!(
        parse_resource_id(UPSTREAM_ID_PREFIX, &gts).expect("GTS identifier"),
        id
    );
    let error = parse_resource_id(UPSTREAM_ID_PREFIX, "gts.cf.core.oagw.route.v1~not-a-uuid")
        .expect_err("foreign GTS identifier must be rejected");
    assert_eq!(error.status_code(), 400);
    assert_eq!(
        parse_resource_id(UPSTREAM_ID_PREFIX, "not-a-uuid")
            .expect_err("garbage must be rejected")
            .status_code(),
        400
    );
}

#[test]
fn validation_errors_are_client_errors() {
    let error = OagwError::validation("x");
    assert!(error.is_client_error());
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

// ── Plugin bindings (S4) ─────────────────────────────────────────────────────

const REQUIRED_HEADERS_PLUGIN: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

#[test]
fn a_plugin_chain_item_is_a_bare_gts_identifier() {
    let chain: PluginChain = serde_json::from_value(json!({ "items": [REQUIRED_HEADERS_PLUGIN] }))
        .expect("bare plugin reference");
    assert_eq!(
        chain.items,
        vec![PluginBinding::Ref(REQUIRED_HEADERS_PLUGIN.to_owned())]
    );
    assert_eq!(chain.items[0].plugin_ref(), REQUIRED_HEADERS_PLUGIN);
    assert!(chain.items[0].config().is_none());
}

#[test]
fn a_plugin_chain_item_carries_the_plugin_configuration() {
    // ADR-0009 "Upstream Configuration Example": the guard config travels in the
    // binding, not in the referenced plugin.
    let chain: PluginChain = serde_json::from_value(json!({
        "items": [{
            "plugin_ref": REQUIRED_HEADERS_PLUGIN,
            "config": { "required_request_headers": "x-correlation-id,accept" }
        }]
    }))
    .expect("bound plugin reference");

    assert_eq!(chain.items[0].plugin_ref(), REQUIRED_HEADERS_PLUGIN);
    assert_eq!(
        chain.items[0].config(),
        Some(&json!({ "required_request_headers": "x-correlation-id,accept" }))
    );
}

#[test]
fn an_unconfigured_binding_serializes_back_to_the_bare_identifier() {
    let bound = PluginBinding::Bound(BoundPluginBinding {
        plugin_ref: REQUIRED_HEADERS_PLUGIN.to_owned(),
        config: None,
    });
    assert_eq!(
        serde_json::to_value(&bound).expect("binding serializes"),
        json!(REQUIRED_HEADERS_PLUGIN)
    );
}

#[test]
fn a_configured_binding_round_trips_through_the_wire_shape() {
    let document = json!({
        "items": [
            REQUIRED_HEADERS_PLUGIN,
            { "plugin_ref": REQUIRED_HEADERS_PLUGIN, "config": { "required_response_headers": "content-type" } }
        ]
    });
    let chain: PluginChain = serde_json::from_value(document.clone()).expect("chain");
    let round_tripped = serde_json::to_value(&chain).expect("chain serializes");
    // `PluginChain` always emits `sharing` (the schema default), so the bindings
    // are compared on their own member.
    assert_eq!(
        round_tripped["items"], document["items"],
        "the bare reference and the configured binding both survive a round trip"
    );
    assert_eq!(round_tripped["sharing"], json!("private"));
}

#[test]
fn a_plugin_chain_with_an_unknown_member_is_refused() {
    let error = serde_json::from_value::<PluginChain>(json!({
        "sharing": "private",
        "items": [],
        "unknown": true
    }))
    .expect_err("a chain document with an unknown member is not a chain");
    assert!(
        error.to_string().contains("unknown field"),
        "the refusal names the member, not just the shape: {error}"
    );
}

#[test]
fn a_configured_binding_with_an_unknown_member_is_refused() {
    // A binding is security-relevant: a typo in a member name must not silently
    // drop the configuration it was meant to carry.
    let error = serde_json::from_value::<PluginChain>(json!({
        "items": [{ "plugin_ref": REQUIRED_HEADERS_PLUGIN, "config": {}, "unknown": true }]
    }))
    .expect_err("a binding with an unknown member is not a binding");
    // `PluginBinding` is an untagged enum, so serde folds the per-variant reason
    // (`unknown field `unknown``) into its own "did not match any variant"
    // message. The refusal is the contract; the member is named by the chain
    // rejection above, where the enum does not stand in the way.
    assert!(
        error.to_string().contains("did not match any variant"),
        "{error}"
    );
}

#[test]
fn an_upstream_chain_is_validated_by_reference() {
    let spec: UpstreamSpec = serde_json::from_value(json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "plugins": {
            "items": [{ "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~bad/ref" }]
        }
    }))
    .expect("upstream document");

    let error = spec
        .validate()
        .expect_err("a malformed plugin_ref is rejected");
    assert_eq!(error.status_code(), 400);
    assert_eq!(
        error.extensions().invalid_value.as_deref(),
        Some("gts.cf.core.oagw.guard_plugin.v1~bad/ref")
    );
}

#[test]
fn an_upstream_chain_accepts_the_configured_binding_shape() {
    let spec: UpstreamSpec = serde_json::from_value(json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "plugins": {
            "items": [{ "plugin_ref": REQUIRED_HEADERS_PLUGIN, "config": {} }]
        }
    }))
    .expect("upstream document");

    spec.validate().expect("a bound builtin plugin is accepted");
}
