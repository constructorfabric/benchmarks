//! Tests for the built-in guard plugins.

use crate::plugins::{guard, BoundPlugin, PluginContext};
use crate::security::{NoopCredentialResolver, SecurityContextHolder};
use crate::plugins::token_cache::TokenCache;

fn binding(name: &str, config: serde_json::Value) -> BoundPlugin {
    // A binding that is not a JSON object carries no settings at all.
    let config = if let serde_json::Value::Object(map) = config {
        map
    } else {
        serde_json::Map::new()
    };
    BoundPlugin { name: name.to_owned(), config }
}

fn ctx(h: &Harness) -> PluginContext<'_> {
    PluginContext {
        security: &h.holder,
        config: empty_config(),
        upstream_id: "gts.cf.core.oagw.upstream.v1~abc",
        host: "api.example.com",
        path: "/v1/items",
        request_id: "",
        credentials: &NoopCredentialResolver,
        token_cache: &h.token_cache,
    }
}

fn empty_config() -> &'static serde_json::Map<String, serde_json::Value> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, serde_json::Value>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(serde_json::Map::new)
}

struct Harness {
    holder: SecurityContextHolder,
    token_cache: TokenCache,
}

fn harness() -> Harness {
    Harness {
        holder: holder(),
        token_cache: TokenCache::new(8, std::time::Duration::from_mins(5)),
    }
}

fn holder() -> SecurityContextHolder {
    let tenant = uuid::Uuid::new_v4();
    SecurityContextHolder::new(
        toolkit_security::SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .expect("the context is complete"),
        vec![tenant],
    )
}

#[test]
fn a_required_header_present_on_the_request_is_accepted() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding("required_headers", serde_json::json!({ "headers": ["x-tenant"] }));

    let mut headers = http::HeaderMap::new();
    headers.insert("x-tenant", http::HeaderValue::from_static("acme"));
    assert!(guard::execute_request(&plugin, &ctx, &headers).is_ok());
}

#[test]
fn a_missing_or_empty_required_header_is_rejected() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding("required_headers", serde_json::json!({ "headers": ["x-tenant"] }));

    let err = guard::execute_request(&plugin, &ctx, &http::HeaderMap::new())
        .expect_err("the header is missing");
    assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError);
    assert!(err.detail().contains("x-tenant"), "{err}");

    let mut empty = http::HeaderMap::new();
    empty.insert("x-tenant", http::HeaderValue::from_static(""));
    assert!(
        guard::execute_request(&plugin, &ctx, &empty).is_err(),
        "an empty header is as good as an absent one"
    );
}

#[test]
fn a_bare_string_names_one_header() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding("required_headers", serde_json::json!({ "headers": "x-single" }));

    let mut headers = http::HeaderMap::new();
    headers.insert("x-single", http::HeaderValue::from_static("1"));
    assert!(guard::execute_request(&plugin, &ctx, &headers).is_ok());

    assert!(guard::execute_request(&plugin, &ctx, &http::HeaderMap::new()).is_err());
}

#[test]
fn a_guard_with_no_headers_configured_demands_nothing() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding("required_headers", serde_json::json!({}));
    assert!(guard::execute_request(&plugin, &ctx, &http::HeaderMap::new()).is_ok());
}

#[test]
fn an_unknown_guard_is_rejected_on_both_sides() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding("timeout", serde_json::json!({}));
    let err = guard::execute_request(&plugin, &ctx, &http::HeaderMap::new())
        .expect_err("timeout is catalogued only");
    assert_eq!(err.kind(), crate::error::ErrorKind::PluginNotFound);
    assert!(
        guard::execute_response(&plugin, &ctx, &http::HeaderMap::new()).is_err(),
        "the response pass rejects it too"
    );
}

/// FR-017's response half: an upstream answer that omits a header the binding demands is
/// refused with a `502` naming the first missing header, not relayed to the caller.
#[test]
fn a_response_missing_a_required_header_is_refused() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding(
        "required_headers",
        serde_json::json!({ "required_response_headers": "content-type" }),
    );

    let err = guard::execute_response(&plugin, &ctx, &http::HeaderMap::new())
        .expect_err("the upstream sent no content type");
    assert_eq!(err.kind(), crate::error::ErrorKind::DownstreamError);
    assert_eq!(err.kind().status(), 502, "{err}");
    assert!(err.detail().contains("content-type"), "{err}");
}

/// Only the first missing name is reported, in the order the binding names them.
#[test]
fn the_response_names_only_the_first_missing_header() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding(
        "required_headers",
        serde_json::json!({ "required_response_headers": "x-a, x-b" }),
    );

    let err = guard::execute_response(&plugin, &ctx, &http::HeaderMap::new())
        .expect_err("neither header arrived");
    assert!(err.detail().contains("x-a"), "{err}");
    assert!(!err.detail().contains("`x-b`"), "{err}");
}

/// An answer carrying every demanded header is relayed, whatever case the binding used.
#[test]
fn a_response_carrying_the_required_headers_is_relayed() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding(
        "required_headers",
        serde_json::json!({ "required_response_headers": "Content-Type, x-trace" }),
    );
    let mut headers = http::HeaderMap::new();
    headers.insert("content-TYPE", http::HeaderValue::from_static("application/json"));
    headers.insert("X-TRACE", http::HeaderValue::from_static("1"));
    assert!(guard::execute_response(&plugin, &ctx, &headers).is_ok());
}

/// The two phases are configured independently: a response-side demand does not bind the
/// request, and a request-side demand does not bind the response.
#[test]
fn the_two_phases_are_configured_independently() {
    let h = harness();
    let ctx = ctx(&h);
    let response_only = binding(
        "required_headers",
        serde_json::json!({ "required_response_headers": "content-type" }),
    );
    assert!(
        guard::execute_request(&response_only, &ctx, &http::HeaderMap::new()).is_ok(),
        "the request phase reads only its own key"
    );

    let request_only = binding(
        "required_headers",
        serde_json::json!({ "required_request_headers": "x-correlation-id" }),
    );
    assert!(
        guard::execute_response(&request_only, &ctx, &http::HeaderMap::new()).is_ok(),
        "the response phase reads only its own key"
    );
}

/// A blank or absent list demands nothing, so a binding added without configuration
/// changes no behaviour (fail-open).
#[test]
fn a_blank_response_list_demands_nothing() {
    let h = harness();
    let ctx = ctx(&h);
    for config in [
        serde_json::json!({ "required_response_headers": "" }),
        serde_json::json!({ "required_response_headers": " , ," }),
        serde_json::json!({}),
    ] {
        let plugin = binding("required_headers", config);
        assert!(
            guard::execute_response(&plugin, &ctx, &http::HeaderMap::new()).is_ok(),
            "an unconfigured guard fails open"
        );
    }
}

/// The request phase reads the key ADR-0009 documents, not only the alias it shipped
/// with: an upstream configured per the ADR is enforced, not silently fail-open.
#[test]
fn the_request_phase_reads_the_adr_key() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding(
        "required_headers",
        serde_json::json!({ "required_request_headers": "X-Correlation-Id, accept" }),
    );

    let mut headers = http::HeaderMap::new();
    headers.insert("x-correlation-ID", http::HeaderValue::from_static("c-1"));
    headers.insert("ACCEPT", http::HeaderValue::from_static("application/json"));
    assert!(guard::execute_request(&plugin, &ctx, &headers).is_ok());

    let err = guard::execute_request(&plugin, &ctx, &http::HeaderMap::new())
        .expect_err("neither header arrived");
    assert_eq!(err.kind(), crate::error::ErrorKind::ValidationError, "{err}");
}

/// Empty entries in a comma-separated list are dropped, not read as names.
#[test]
fn a_list_with_blank_entries_names_only_the_real_headers() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding(
        "required_headers",
        serde_json::json!({ "required_request_headers": ", ,x-only," }),
    );
    let mut headers = http::HeaderMap::new();
    headers.insert("x-only", http::HeaderValue::from_static("1"));
    assert!(guard::execute_request(&plugin, &ctx, &headers).is_ok());
}

/// A definition written in the `headers` alias shape still enforces on the request.
#[test]
fn the_headers_alias_still_names_request_headers() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding("required_headers", serde_json::json!({ "headers": ["x-legacy"] }));
    assert!(guard::execute_request(&plugin, &ctx, &http::HeaderMap::new()).is_err());

    // The alias is a request-phase spelling only; it never binds the response.
    assert!(guard::execute_response(&plugin, &ctx, &http::HeaderMap::new()).is_ok());
}

#[test]
fn a_guard_error_carries_the_request_context() {
    let h = harness();
    let ctx = ctx(&h);
    let plugin = binding("required_headers", serde_json::json!({ "headers": ["x-tenant"] }));
    let err = guard::execute_request(&plugin, &ctx, &http::HeaderMap::new())
        .expect_err("rejected");
    let extensions = err.extensions();
    assert_eq!(
        extensions.host.as_deref(),
        Some("api.example.com"),
        "the context reaches the error document"
    );
}
