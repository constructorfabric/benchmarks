//! The CORS decisions of the domain layer.
//!
//! Covers `cpt-cf-oagw-algo-cors-fold`, `cpt-cf-oagw-algo-cors-decide`, and
//! `cpt-cf-oagw-algo-cors-preflight-headers`: the per-member overlay over the
//! two layer results in the upstream, then route order, the ancestor `enforce`
//! that no descendant widens, the exact origin matching ADR 0004's Origin
//! Matching section demonstrates, the origin-before-method order, the
//! credentials restriction, the decoration of an admitted request, and the
//! preflight header set with its echoed values and its three-member `Vary`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use oagw::domain::cors::{
    CorsDecision, CorsRefusal, PREFLIGHT_MAX_AGE, PREFLIGHT_VARY, VARY_ORIGIN,
};
use oagw::domain::effective::EffectiveCors;
use oagw::domain::upstream::{CorsConfig, SharingMode};

const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0x31);
const ANCESTOR: uuid::Uuid = uuid::Uuid::from_u128(0x30);

/// One layer result whose CORS object the caller states.
fn layer(owner: uuid::Uuid, mode: SharingMode, cors: CorsConfig) -> EffectiveCors {
    EffectiveCors {
        owner,
        mode,
        cors,
    }
}

/// A CORS object with the shipped defaults and the members the caller states.
#[allow(clippy::too_many_arguments)]
fn cors_object(
    enabled: bool,
    origins: &[&str],
    methods: &[&str],
    expose: &[&str],
    credentials: bool,
) -> CorsConfig {
    CorsConfig {
        sharing: None,
        enabled,
        allowed_origins: origins.iter().map(|origin| String::from(*origin)).collect(),
        allowed_methods: methods.iter().map(|method| String::from(*method)).collect(),
        expose_headers: expose.iter().map(|header| String::from(*header)).collect(),
        allow_credentials: credentials,
    }
}

// @cpt-dod:cpt-cf-oagw-dod-cors-hierarchy:p1

#[test]
fn no_layer_carrying_cors_folds_to_the_absent_family() {
    assert!(oagw::domain::cors::fold(None, None).is_none());
}

#[test]
fn a_disabled_prevailing_family_folds_to_the_absent_outcome() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(false, &["https://app.example.com"], &["GET"], &[], false),
    );
    assert!(oagw::domain::cors::fold(Some(&upstream), None).is_none());
}

#[test]
fn an_absent_route_layer_leaves_the_upstream_layer_alone() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(
            true,
            &["https://app.example.com"],
            &["GET", "POST"],
            &["X-Request-ID"],
            true,
        ),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    assert_eq!(policy.allowed_origins, vec![String::from("https://app.example.com")]);
    assert_eq!(policy.allowed_methods, vec![String::from("GET"), String::from("POST")]);
    assert_eq!(policy.expose_headers, vec![String::from("X-Request-ID")]);
    assert!(policy.allow_credentials);
}

#[test]
fn the_route_layer_prevails_for_the_members_it_declares() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://app.example.com"], &["GET"], &["X-Request-ID"], false),
    );
    let route = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://admin.example.com"], &["DELETE"], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), Some(&route))
        .expect("the family is present");
    assert_eq!(policy.allowed_origins, vec![String::from("https://admin.example.com")]);
    assert_eq!(policy.allowed_methods, vec![String::from("DELETE")]);
    // The route object omits its exposure, so the upstream's list stands.
    assert_eq!(policy.expose_headers, vec![String::from("X-Request-ID")]);
}

#[test]
fn a_member_the_route_omits_is_taken_from_the_upstream_object() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://app.example.com"], &["GET", "POST"], &["X-Request-ID"], false),
    );
    // The route object names no origins and no methods at all.
    let route = layer(TENANT, SharingMode::Private, cors_object(true, &[], &[], &[], false));
    let policy = oagw::domain::cors::fold(Some(&upstream), Some(&route))
        .expect("the family is present");
    assert_eq!(policy.allowed_origins, vec![String::from("https://app.example.com")]);
    assert_eq!(policy.allowed_methods, vec![String::from("GET"), String::from("POST")]);
}

#[test]
fn an_ancestor_inherit_union_is_enforced_as_the_layer_result_carries_it() {
    // The merge of the hierarchical feature already unioned the two chains'
    // origins into the ancestor's layer result; the fold consumes it as is.
    let upstream = layer(
        ANCESTOR,
        SharingMode::Inherit,
        cors_object(
            true,
            &["https://app.example.com", "https://admin.example.com"],
            &["GET"],
            &[],
            false,
        ),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    assert_eq!(
        policy.allowed_origins,
        vec![
            String::from("https://app.example.com"),
            String::from("https://admin.example.com")
        ]
    );
}

#[test]
fn an_ancestor_enforce_takes_the_layer_result_whole() {
    let upstream = layer(
        ANCESTOR,
        SharingMode::Enforce,
        cors_object(true, &["https://app.example.com"], &["GET"], &[], false),
    );
    let route = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://admin.example.com"], &["DELETE"], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), Some(&route))
        .expect("the family is present");
    assert_eq!(policy.allowed_origins, vec![String::from("https://app.example.com")]);
    assert_eq!(policy.allowed_methods, vec![String::from("GET")]);
    assert!(policy.enabled);
}

#[test]
fn a_private_ancestor_contributes_nothing_to_a_descendant_with_no_object() {
    // The merge already withheld the ancestor's private object, so no layer
    // result exists and the fold reports the absent family.
    let route = layer(TENANT, SharingMode::Private, cors_object(false, &[], &[], &[], false));
    assert!(oagw::domain::cors::fold(Some(&route), None).is_none());
}

#[test]
fn methods_absent_at_every_layer_take_the_shipped_default() {
    // A layer result can carry an empty method list only when the write path
    // defaulted it; the fold's own default applies when neither layer names one.
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://app.example.com"], &[], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    assert_eq!(policy.allowed_methods, vec![String::from("GET"), String::from("POST")]);
}

// @cpt-dod:cpt-cf-oagw-dod-cors-origin-matching:p1

#[test]
fn an_exact_origin_is_admitted_and_every_other_spelling_is_refused() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://app.example.com"], &["GET"], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    assert!(matches!(
        oagw::domain::cors::decide(&policy, Some("https://app.example.com"), "GET"),
        CorsDecision::Allowed(_)
    ));
    for refused in [
        "https://evil.com",
        "https://app.example.com:8080",
        "http://app.example.com",
        "https://evil.com.example.com",
        "HTTPS://APP.EXAMPLE.COM",
        "https://app.example.com/",
        "https://app.example.com:443",
    ] {
        assert!(
            matches!(
                oagw::domain::cors::decide(&policy, Some(refused), "GET"),
                CorsDecision::Refused(CorsRefusal::Origin)
            ),
            "{refused} must be refused"
        );
    }
}

#[test]
fn a_wildcard_admits_every_origin_and_echoes_it() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["*"], &["GET"], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    let CorsDecision::Allowed(decoration) =
        oagw::domain::cors::decide(&policy, Some("https://anywhere.test"), "GET")
    else {
        panic!("the wildcard admits every origin");
    };
    assert_eq!(decoration.allow_origin, "https://anywhere.test");
}

#[test]
fn credentials_beside_a_wildcard_refuse_every_origin() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["*"], &["GET"], &[], true),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    for origin in ["https://app.example.com", "https://anywhere.test", "*"] {
        assert!(matches!(
            oagw::domain::cors::decide(&policy, Some(origin), "GET"),
            CorsDecision::Refused(CorsRefusal::Origin)
        ));
    }
}

#[test]
fn an_enabled_family_with_no_origin_allows_no_origin() {
    let upstream = layer(TENANT, SharingMode::Private, cors_object(true, &[], &["GET"], &[], false));
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    assert!(matches!(
        oagw::domain::cors::decide(&policy, Some("https://app.example.com"), "GET"),
        CorsDecision::Refused(CorsRefusal::Origin)
    ));
}

#[test]
fn a_refused_origin_names_itself_and_no_allowed_value() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://app.example.com"], &["GET"], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    let oagw::domain::cors::CorsDecision::Refused(reason) =
        oagw::domain::cors::decide(&policy, Some("https://evil.com"), "GET")
    else {
        panic!("the disallowed origin is refused");
    };
    assert_eq!(reason, CorsRefusal::Origin);
    assert_eq!(
        oagw::domain::cors::refusal_detail(reason, "https://evil.com", "GET"),
        "Origin 'https://evil.com' not in allowed origins list"
    );
}

#[test]
fn a_refused_method_names_itself_and_no_allowed_value() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://app.example.com"], &["GET", "POST"], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    let oagw::domain::cors::CorsDecision::Refused(reason) =
        oagw::domain::cors::decide(&policy, Some("https://app.example.com"), "DELETE")
    else {
        panic!("the disallowed method is refused");
    };
    assert_eq!(reason, CorsRefusal::Method);
    assert_eq!(
        oagw::domain::cors::refusal_detail(reason, "https://app.example.com", "DELETE"),
        "Method 'DELETE' not in allowed methods list"
    );
}

#[test]
fn the_origin_check_precedes_the_method_check() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://app.example.com"], &["GET"], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    assert!(matches!(
        oagw::domain::cors::decide(&policy, Some("https://evil.com"), "DELETE"),
        CorsDecision::Refused(CorsRefusal::Origin)
    ));
}

// @cpt-dod:cpt-cf-oagw-dod-cors-headers:p1

#[test]
fn an_admitted_request_carries_the_actual_request_decoration() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(
            true,
            &["https://app.example.com"],
            &["GET", "POST"],
            &["X-Request-ID"],
            true,
        ),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    let CorsDecision::Allowed(decoration) =
        oagw::domain::cors::decide(&policy, Some("https://app.example.com"), "POST")
    else {
        panic!("the admitted request carries a decoration");
    };
    assert_eq!(decoration.allow_origin, "https://app.example.com");
    assert!(decoration.allow_credentials);
    assert_eq!(decoration.expose_headers, vec![String::from("X-Request-ID")]);
    assert_eq!(decoration.vary, VARY_ORIGIN);
}

#[test]
fn no_credentials_and_no_exposure_are_omitted_from_the_decoration() {
    let upstream = layer(
        TENANT,
        SharingMode::Private,
        cors_object(true, &["https://app.example.com"], &["GET"], &[], false),
    );
    let policy = oagw::domain::cors::fold(Some(&upstream), None).expect("the family is present");
    let CorsDecision::Allowed(decoration) =
        oagw::domain::cors::decide(&policy, Some("https://app.example.com"), "GET")
    else {
        panic!("the admitted request carries a decoration");
    };
    assert!(!decoration.allow_credentials);
    assert!(decoration.expose_headers.is_empty());
}

// @cpt-dod:cpt-cf-oagw-dod-cors-preflight:p1

#[test]
fn the_preflight_answer_echoes_the_three_request_values() {
    let answer = oagw::domain::cors::preflight_answer(
        Some("https://app.example.com"),
        Some("POST"),
        Some("Content-Type, Authorization"),
    );
    assert_eq!(answer.status, 204);
    let value = |name: &str| {
        answer
            .headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.clone())
    };
    assert_eq!(
        value("Access-Control-Allow-Origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(value("Access-Control-Allow-Methods").as_deref(), Some("POST"));
    assert_eq!(
        value("Access-Control-Allow-Headers").as_deref(),
        Some("Content-Type, Authorization")
    );
    assert_eq!(value("Access-Control-Max-Age").as_deref(), Some(PREFLIGHT_MAX_AGE));
    assert_eq!(value("Vary").as_deref(), Some(PREFLIGHT_VARY));
    assert!(value("Access-Control-Allow-Credentials").is_none());
    assert!(value("Access-Control-Expose-Headers").is_none());
}

#[test]
fn a_preflight_that_names_no_request_header_omits_the_header_answer() {
    let answer = oagw::domain::cors::preflight_answer(
        Some("https://app.example.com"),
        Some("GET"),
        None,
    );
    assert!(
        !answer
            .headers
            .iter()
            .any(|(header, _)| header == "Access-Control-Allow-Headers")
    );
}

#[test]
fn the_same_preflight_answered_twice_produces_the_same_header_set() {
    let first = oagw::domain::cors::preflight_answer(
        Some("https://app.example.com"),
        Some("POST"),
        None,
    );
    let second = oagw::domain::cors::preflight_answer(
        Some("https://app.example.com"),
        Some("POST"),
        None,
    );
    assert_eq!(first.headers, second.headers);
    assert_eq!(first.status, second.status);
}
