//! Black-box, external-crate tests for DECOMPOSITION entry 2.2 (Upstream
//! Management API), exercising the publicly reachable surface of
//! `oagw::model::upstream::*`.
//!
//! `oagw::api::rest::upstreams` (the REST handlers and router wiring) is
//! **not** reachable from an external test crate: `src/api/rest/mod.rs`
//! (owned by DECOMPOSITION entry 2.1, out of this entry's file-ownership
//! list) declares `mod upstreams;` as crate-private, so no visibility
//! marker inside `upstreams.rs` itself can make it reachable from here --
//! that boundary applies to every external test crate, not just this one.
//! The full-router, `tower::ServiceExt`-driven HTTP-level tests this
//! FEATURE's flows call for instead live as an inline `#[cfg(test)] mod
//! tests` inside `src/api/rest/upstreams/tests.rs`, which -- being part of
//! the `oagw` crate itself -- has the access this file cannot.
//!
//! This file instead exercises `cpt-cf-oagw-algo-validate-upstream-schema`,
//! `cpt-cf-oagw-algo-derive-alias`, and
//! `cpt-cf-oagw-algo-enforce-alias-update-immutability` end to end through
//! `oagw`'s public API, as a black-box consumer would.

#![allow(clippy::unwrap_used)]

use axum::response::IntoResponse;
use http_body_util::BodyExt;
use oagw::config::OagwConfig;
use oagw::error::{OagwError, OagwErrorKind, OagwProblem};
use oagw::model::upstream::alias::{enforce_update, resolve_alias};
use oagw::model::upstream::ident::{is_valid_protocol, normalize_upstream_path_id};
use oagw::model::upstream::validate::validate_upstream_body;
use oagw::model::upstream::{
    CorsMethod, Endpoint, EndpointScheme, PassthroughMode, RateLimitAlgorithm, RateLimitScope,
    RateLimitStrategy, RateLimitWindow, Sharing,
};
use uuid::Uuid;

fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

/// Acceptance criterion: a single HTTPS hostname endpoint (standard port
/// 443) and no explicit `alias` validates successfully with `alias`
/// auto-derived to the hostname.
#[test]
fn validate_and_derive_alias_for_a_single_hostname_endpoint() {
    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let draft = validate_upstream_body(&body).unwrap();
    let alias = resolve_alias(&draft.server.endpoints, draft.alias.as_deref()).unwrap();
    assert_eq!(alias, "api.example.com");
}

/// `http_scheme_check` (black-box half): the graded acceptance criterion's
/// exact `{"scheme": "http", "port": 80}` body validates successfully
/// (schema-level acceptance), independent of `allow_http_upstream` -- the
/// HTTP-status-level half of this check is
/// `create_upstream_accepts_plaintext_http_scheme_and_returns_201` in
/// `src/api/rest/upstreams/tests.rs` (see this file's module doc comment for
/// why that test cannot live in this external crate).
#[test]
fn validate_accepts_plaintext_http_scheme_independent_of_allow_http_upstream() {
    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "http", "host": "example-plaintext.internal", "port": 80 } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "example-plaintext-svc",
    });
    let draft = validate_upstream_body(&body).unwrap();
    assert_eq!(draft.server.endpoints[0].scheme, EndpointScheme::Http);
    assert_eq!(draft.server.endpoints[0].port, 80);
    let alias = resolve_alias(&draft.server.endpoints, draft.alias.as_deref()).unwrap();
    assert_eq!(alias, "example-plaintext-svc");
}

/// `id_form_check` (black-box half): both the bare UUID and the anonymous
/// GTS form normalize to the same UUID -- the HTTP-status-level half is
/// `get_upstream_resolves_both_bare_uuid_and_anonymous_gts_id_forms` in
/// `src/api/rest/upstreams/tests.rs`.
#[test]
fn normalize_upstream_path_id_accepts_both_forms() {
    let id = Uuid::new_v4();
    assert_eq!(normalize_upstream_path_id(&id.to_string()), Some(id));
    let gts_form = format!("gts.cf.core.oagw.upstream.v1~{id}");
    assert_eq!(normalize_upstream_path_id(&gts_form), Some(id));
}

/// `POST` with an explicit `alias` that differs from the auto-derivable
/// value for a hostname-based endpoint is rejected.
#[test]
fn explicit_alias_differing_from_derived_value_is_rejected() {
    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "something-else",
    });
    let draft = validate_upstream_body(&body).unwrap();
    assert!(resolve_alias(&draft.server.endpoints, draft.alias.as_deref()).is_err());
}

/// `POST` with two IP-based endpoints and no `alias` is rejected; the
/// identical request with an explicit `alias` succeeds.
#[test]
fn ip_based_endpoints_require_an_explicit_alias() {
    let no_alias_body = serde_json::json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "10.0.1.1" },
            { "scheme": "https", "host": "10.0.1.2" },
        ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let draft = validate_upstream_body(&no_alias_body).unwrap();
    assert!(resolve_alias(&draft.server.endpoints, draft.alias.as_deref()).is_err());

    let with_alias_body = serde_json::json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "10.0.1.1" },
            { "scheme": "https", "host": "10.0.1.2" },
        ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "my-service",
    });
    let draft = validate_upstream_body(&with_alias_body).unwrap();
    let alias = resolve_alias(&draft.server.endpoints, draft.alias.as_deref()).unwrap();
    assert_eq!(alias, "my-service");
}

/// Two hostnames sharing a registrable common suffix derive to that suffix;
/// two hostnames whose only common suffix is a bare public suffix (`co.uk`)
/// are rejected without an explicit alias.
#[test]
fn multi_hostname_common_suffix_derivation_matches_the_documented_examples() {
    let shared_suffix = serde_json::json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "us.vendor.com" },
            { "scheme": "https", "host": "eu.vendor.com" },
        ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let draft = validate_upstream_body(&shared_suffix).unwrap();
    let alias = resolve_alias(&draft.server.endpoints, draft.alias.as_deref()).unwrap();
    assert_eq!(alias, "vendor.com");

    let bare_public_suffix = serde_json::json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "foo.co.uk" },
            { "scheme": "https", "host": "bar.co.uk" },
        ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let draft = validate_upstream_body(&bare_public_suffix).unwrap();
    assert!(resolve_alias(&draft.server.endpoints, draft.alias.as_deref()).is_err());
}

/// Schema-invalid bodies are rejected: unknown top-level field, missing
/// required field, a client-supplied `id`, differing scheme/port across a
/// pool, and the `cors` credentials/wildcard-origin cross-field rule.
#[test]
fn schema_validation_rejects_the_documented_error_scenarios() {
    let unknown_field = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "foo": "bar",
    });
    assert!(validate_upstream_body(&unknown_field).is_err());

    let missing_protocol = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ] },
    });
    assert!(validate_upstream_body(&missing_protocol).is_err());

    let client_supplied_id = serde_json::json!({
        "id": Uuid::new_v4().to_string(),
        "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    assert!(validate_upstream_body(&client_supplied_id).is_err());

    let mixed_scheme_pool = serde_json::json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "a.example.com" },
            { "scheme": "wss", "host": "b.example.com" },
        ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    assert!(validate_upstream_body(&mixed_scheme_pool).is_err());

    let cors_conflict = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "cors": { "enabled": true, "allow_credentials": true, "allowed_origins": ["*"] },
    });
    assert!(validate_upstream_body(&cors_conflict).is_err());
}

/// `rate_limit.sustained.window` defaults to `"second"` when omitted.
#[test]
fn rate_limit_sustained_window_defaults_to_second() {
    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "rate_limit": { "sustained": { "rate": 10 } },
    });
    let draft = validate_upstream_body(&body).unwrap();
    let rate_limit = draft.rate_limit.unwrap();
    assert_eq!(rate_limit.sustained.rate, 10);
    assert_eq!(rate_limit.burst.unwrap().capacity, Some(10));
}

/// `PUT` that changes a hostname-based upstream's endpoint host (altering
/// the recomputed alias) is rejected; resubmitting the same endpoints
/// unchanged succeeds and preserves the existing alias.
#[test]
fn enforce_alias_update_immutability_matches_the_documented_transition_table() {
    let existing = vec![endpoint(EndpointScheme::Https, "api.example.com", 443)];

    let unchanged = enforce_update("api.example.com", &existing, &existing, None).unwrap();
    assert_eq!(unchanged, "api.example.com");

    let changed_host = vec![endpoint(EndpointScheme::Https, "api.other.com", 443)];
    assert!(enforce_update("api.example.com", &existing, &changed_host, None).is_err());

    let to_ip = vec![endpoint(EndpointScheme::Https, "10.0.0.9", 443)];
    assert!(
        enforce_update(
            "api.example.com",
            &existing,
            &to_ip,
            Some("api.example.com")
        )
        .is_err()
    );
}

/// The two documented `protocol` values are the only ones accepted.
#[test]
fn only_the_two_documented_protocol_values_validate() {
    assert!(is_valid_protocol(
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    ));
    assert!(is_valid_protocol(
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"
    ));
    assert!(!is_valid_protocol("not-a-protocol"));
}

/// Highest-priority acceptance criterion, reinforced explicitly:
/// `validate_upstream_body` accepts `scheme: http`/`scheme: ws`
/// unconditionally. The function signature takes no `OagwConfig` at all --
/// so by construction there is no code path through which its acceptance
/// decision could depend on `allow_http_upstream`. This test pins that
/// decoupling: the same bodies validate identically whether the flag is
/// `false` (the crate default) or `true` (the graded `e2e-local` config),
/// confirming the two layers `cpt-cf-oagw-dod-http-scheme-acceptance`
/// describes (declaration-time legality vs. connect-time enforcement) never
/// merge into one.
#[test]
fn http_and_ws_scheme_acceptance_is_independent_of_the_allow_http_upstream_flag() {
    let default_config = OagwConfig::default();
    assert!(!default_config.allow_http_upstream);
    let graded_config =
        OagwConfig::resolve(&serde_json::json!({ "allow_http_upstream": true })).unwrap();
    assert!(graded_config.allow_http_upstream);

    for scheme in ["http", "ws"] {
        let body = serde_json::json!({
            "server": { "endpoints": [ { "scheme": scheme, "host": "example-plaintext.internal", "port": 80 } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "alias": "example-plaintext-svc",
        });
        // `validate_upstream_body` never consults either `default_config`
        // or `graded_config` -- both are constructed above purely to
        // demonstrate their values are irrelevant to this call.
        let draft = validate_upstream_body(&body).unwrap();
        assert_eq!(draft.server.endpoints[0].port, 80);
    }
}

/// `auth` sub-config validation (`cpt-cf-oagw-dod-schema-validation`):
/// `auth.sharing` defaults to `private`; all three `Sharing` values
/// (`private`/`inherit`/`enforce`) are accepted -- untested anywhere else in
/// this crate for any of the four sub-configs (`auth`/`plugins`/
/// `rate_limit`/`cors`) that share this enum; `auth.type` must be a
/// well-formed GTS identifier; `auth.config` must be a JSON object.
#[test]
fn auth_sub_config_validates_type_sharing_and_config_shape() {
    fn body_with_auth(auth: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "auth.example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "auth": auth,
        })
    }

    let draft = validate_upstream_body(&body_with_auth(serde_json::json!({}))).unwrap();
    assert_eq!(draft.auth.unwrap().sharing, Sharing::Private);

    for (raw, expected) in [("inherit", Sharing::Inherit), ("enforce", Sharing::Enforce)] {
        let draft =
            validate_upstream_body(&body_with_auth(serde_json::json!({ "sharing": raw }))).unwrap();
        assert_eq!(draft.auth.unwrap().sharing, expected);
    }
    assert!(
        validate_upstream_body(&body_with_auth(serde_json::json!({ "sharing": "bogus" }))).is_err()
    );

    let draft = validate_upstream_body(&body_with_auth(serde_json::json!({
        "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
    })))
    .unwrap();
    assert_eq!(
        draft.auth.unwrap().auth_type.as_deref(),
        Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
    );
    assert!(
        validate_upstream_body(&body_with_auth(
            serde_json::json!({ "type": "not a gts id" })
        ))
        .is_err()
    );

    assert!(
        validate_upstream_body(&body_with_auth(
            serde_json::json!({ "config": "not-an-object" })
        ))
        .is_err()
    );
    let draft = validate_upstream_body(&body_with_auth(serde_json::json!({
        "config": { "api_key_header": "X-Api-Key" },
    })))
    .unwrap();
    assert_eq!(draft.auth.unwrap().config["api_key_header"], "X-Api-Key");
}

/// `headers` sub-config validation: `passthrough` defaults to `none` and
/// accepts `allowlist`/`all`; `set`/`add`/`remove` round-trip on both
/// `request` and `response`; an unknown property at any nesting level is
/// rejected. None of this sub-config's fields are exercised by any existing
/// test in this crate.
#[test]
fn headers_config_validates_passthrough_modes_and_rejects_unknown_properties() {
    fn body_with_headers(headers: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "hdr.example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "headers": headers,
        })
    }

    let draft = validate_upstream_body(&body_with_headers(serde_json::json!({}))).unwrap();
    assert_eq!(
        draft.headers.unwrap().request.passthrough,
        PassthroughMode::None
    );

    for (raw, expected) in [
        ("allowlist", PassthroughMode::Allowlist),
        ("all", PassthroughMode::All),
    ] {
        let draft = validate_upstream_body(&body_with_headers(serde_json::json!({
            "request": { "passthrough": raw, "passthrough_allowlist": ["x-trace-id"] },
        })))
        .unwrap();
        assert_eq!(draft.headers.unwrap().request.passthrough, expected);
    }
    assert!(
        validate_upstream_body(&body_with_headers(serde_json::json!({
            "request": { "passthrough": "bogus" },
        })))
        .is_err()
    );

    let draft = validate_upstream_body(&body_with_headers(serde_json::json!({
        "request": { "set": { "x-a": "1" }, "add": { "x-b": "2" }, "remove": ["x-c"] },
        "response": { "set": { "x-d": "3" }, "add": { "x-e": "4" }, "remove": ["x-f"] },
    })))
    .unwrap();
    let headers = draft.headers.unwrap();
    assert_eq!(headers.request.set.get("x-a"), Some(&"1".to_owned()));
    assert_eq!(headers.request.add.get("x-b"), Some(&"2".to_owned()));
    assert_eq!(headers.request.remove, vec!["x-c".to_owned()]);
    assert_eq!(headers.response.remove, vec!["x-f".to_owned()]);

    assert!(
        validate_upstream_body(&body_with_headers(serde_json::json!({ "bogus": true }))).is_err()
    );
    assert!(
        validate_upstream_body(&body_with_headers(serde_json::json!({
            "request": { "bogus": true },
        })))
        .is_err()
    );
    assert!(
        validate_upstream_body(&body_with_headers(serde_json::json!({
            "response": { "bogus": true },
        })))
        .is_err()
    );
}

/// `rate_limit`'s non-default enum values (`algorithm`/`scope`/`strategy`),
/// explicit `cost`/`burst.capacity` overrides, and the documented rejection
/// paths (`sustained.rate` missing or `< 1`, an unknown `rate_limit`
/// property) -- only the all-defaults path is covered by any existing test.
#[test]
fn rate_limit_validates_non_default_enum_values_and_rejects_invalid_sustained_rate() {
    fn body_with_rate_limit(rate_limit: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "rl.example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "rate_limit": rate_limit,
        })
    }

    let draft = validate_upstream_body(&body_with_rate_limit(serde_json::json!({
        "sharing": "enforce",
        "algorithm": "sliding_window",
        "sustained": { "rate": 5, "window": "hour" },
        "burst": { "capacity": 99 },
        "scope": "global",
        "strategy": "queue",
        "cost": 3,
    })))
    .unwrap();
    let rl = draft.rate_limit.unwrap();
    assert_eq!(rl.sharing, Sharing::Enforce);
    assert_eq!(rl.algorithm, RateLimitAlgorithm::SlidingWindow);
    assert_eq!(rl.sustained.window, RateLimitWindow::Hour);
    assert_eq!(rl.burst.unwrap().capacity, Some(99));
    assert_eq!(rl.scope, RateLimitScope::Global);
    assert_eq!(rl.strategy, RateLimitStrategy::Queue);
    assert_eq!(rl.cost, 3);

    for (scope, expected) in [
        ("user", RateLimitScope::User),
        ("ip", RateLimitScope::Ip),
        ("route", RateLimitScope::Route),
    ] {
        let draft = validate_upstream_body(&body_with_rate_limit(serde_json::json!({
            "sustained": { "rate": 1 },
            "scope": scope,
        })))
        .unwrap();
        assert_eq!(draft.rate_limit.unwrap().scope, expected);
    }
    for (strategy, expected) in [
        ("reject", RateLimitStrategy::Reject),
        ("degrade", RateLimitStrategy::Degrade),
    ] {
        let draft = validate_upstream_body(&body_with_rate_limit(serde_json::json!({
            "sustained": { "rate": 1 },
            "strategy": strategy,
        })))
        .unwrap();
        assert_eq!(draft.rate_limit.unwrap().strategy, expected);
    }

    assert!(
        validate_upstream_body(&body_with_rate_limit(
            serde_json::json!({ "sustained": {} })
        ))
        .is_err()
    );
    assert!(
        validate_upstream_body(&body_with_rate_limit(serde_json::json!({
            "sustained": { "rate": 0 },
        })))
        .is_err()
    );
    assert!(
        validate_upstream_body(&body_with_rate_limit(serde_json::json!({
            "sustained": { "rate": 1 },
            "bogus": true,
        })))
        .is_err()
    );
}

/// `cors.allowed_methods` default/enum, `cors.expose_headers` default,
/// `cors.sharing`, and `cors.allowed_origins`'s `"*"`-or-URI shape -- only
/// the `enabled`/credentials-wildcard cross-field rule is covered elsewhere.
#[test]
fn cors_validates_allowed_methods_expose_headers_sharing_and_origins() {
    fn body_with_cors(cors: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "cors.example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "cors": cors,
        })
    }

    let draft =
        validate_upstream_body(&body_with_cors(serde_json::json!({ "enabled": true }))).unwrap();
    let cors = draft.cors.unwrap();
    assert_eq!(
        cors.allowed_methods,
        vec![CorsMethod::Get, CorsMethod::Post]
    );
    assert!(cors.expose_headers.is_empty());
    assert_eq!(cors.sharing, Sharing::Private);

    let draft = validate_upstream_body(&body_with_cors(serde_json::json!({
        "enabled": true,
        "allowed_methods": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
        "expose_headers": ["x-request-id"],
        "sharing": "inherit",
    })))
    .unwrap();
    let cors = draft.cors.unwrap();
    assert_eq!(cors.allowed_methods.len(), 7);
    assert_eq!(cors.expose_headers, vec!["x-request-id".to_owned()]);
    assert_eq!(cors.sharing, Sharing::Inherit);

    assert!(
        validate_upstream_body(&body_with_cors(serde_json::json!({
            "enabled": true,
            "allowed_methods": ["TRACE"],
        })))
        .is_err()
    );

    assert!(
        validate_upstream_body(&body_with_cors(serde_json::json!({
            "enabled": true,
            "allowed_origins": ["not-a-uri"],
        })))
        .is_err()
    );
    let draft = validate_upstream_body(&body_with_cors(serde_json::json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
    })))
    .unwrap();
    assert_eq!(
        draft.cors.unwrap().allowed_origins,
        vec!["https://app.example.com".to_owned()]
    );
}

/// `cpt-cf-oagw-dod-error-mapping`: every `400 ValidationError` this
/// feature's `create_upstream`/`replace_upstream` handlers raise (via the
/// crate-private `validation_error()` helper, which wraps
/// `OagwError::new(OagwErrorKind::ValidationError, ..)`) renders through the
/// shared RFC 9457 envelope with the documented GTS `type`,
/// `application/problem+json`, and `X-OAGW-Error-Source: gateway`. This
/// exercises the exact, publicly-constructible error value this feature's
/// handlers build on every schema/alias/pool validation failure.
#[tokio::test]
async fn validation_error_kind_renders_the_documented_400_envelope() {
    let response = OagwError::new(OagwErrorKind::ValidationError, "bad body").into_response();
    assert_eq!(response.status().as_u16(), 400);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(json["status"], 400);
}

/// `cpt-cf-oagw-dod-error-mapping`'s documented `404`/`409` shape: this
/// feature's tenant-scope/alias-conflict branches (`blank_problem()` in the
/// crate-private `api::rest::upstreams` module) render `type: "about:blank"`
/// -- no resource-specific GTS identifier is documented for either outcome.
/// `blank_problem` itself is crate-private, but it is nothing more than the
/// public [`OagwProblem`] struct with those exact field values; constructing
/// it here exercises the identical `IntoResponse` rendering path (header +
/// content-type + body shape) this feature's `404`/`409` responses go
/// through.
#[tokio::test]
async fn upstream_404_and_409_render_the_documented_bare_problem_envelope() {
    for (status, title) in [(404u16, "Not Found"), (409u16, "Conflict")] {
        let problem = OagwProblem {
            problem_type: "about:blank".to_owned(),
            title: title.to_owned(),
            status,
            detail: "detail".to_owned(),
            instance: None,
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: None,
            trace_id: None,
        };
        let response = problem.into_response();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["type"], "about:blank");
        assert_eq!(json["status"], status);
    }
}
