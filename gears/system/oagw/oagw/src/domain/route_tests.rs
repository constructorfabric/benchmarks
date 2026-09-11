//! Tests for the `Route` entity and its match rules.

use uuid::Uuid;

use crate::domain::route::{GrpcMatch, HttpMatch, PathSuffixMode, Route, RouteMatch, route_id};
use crate::error::ErrorKind;

fn http_match() -> HttpMatch {
    HttpMatch {
        methods: vec!["GET".to_owned()],
        path: "/v1/items".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

#[test]
fn the_route_identifier_is_prefixed_by_the_gts_type() {
    let id = route_id(Uuid::nil());
    assert!(id.starts_with("gts.cf.core.oagw.route.v1~"), "{id}");
}

#[test]
fn a_route_is_enabled_by_default_and_names_no_match() {
    let route = Route::default();
    assert!(route.enabled);
    assert!(route.http_match().is_none());
    assert!(route.primary_method().is_none());
}

#[test]
fn the_primary_method_is_the_first_declared_method() {
    let mut route = Route::default();
    route.r#match = Some(RouteMatch::Http(http_match()));
    assert_eq!(route.primary_method(), Some("GET"));
    assert_eq!(route.http_match().map(|m| m.path.as_str()), Some("/v1/items"));
}

#[test]
fn a_route_without_a_match_rule_is_rejected() {
    let err = Route::default().validate().expect_err("no match rule");
    assert_eq!(err.kind(), ErrorKind::ValidationError);
}

#[test]
fn a_route_needs_at_least_one_method() {
    let mut route = Route::default();
    let mut http = http_match();
    http.methods.clear();
    route.r#match = Some(RouteMatch::Http(http));
    assert_eq!(route.validate().unwrap_err().kind(), ErrorKind::ValidationError);
}

#[test]
fn a_route_path_must_be_absolute() {
    let mut route = Route::default();
    let mut http = http_match();
    http.path = "v1/items".to_owned();
    route.r#match = Some(RouteMatch::Http(http));
    assert_eq!(route.validate().unwrap_err().kind(), ErrorKind::ValidationError);
}

#[test]
fn a_grpc_route_validates_its_service() {
    let mut route = Route::default();
    route.r#match = Some(RouteMatch::Grpc(GrpcMatch {
        service: String::new(),
        method: String::new(),
    }));
    assert!(route.validate().is_err());

    route.r#match = Some(RouteMatch::Grpc(GrpcMatch {
        service: "cf.example.V1".to_owned(),
        method: "Charge".to_owned(),
    }));
    assert!(route.validate().is_ok());
}

#[test]
fn tags_are_validated() {
    let mut route = Route::default();
    route.r#match = Some(RouteMatch::Http(http_match()));
    route.tags = vec!["payments".to_owned()];
    assert!(route.validate().is_ok());

    route.tags = vec!["UPPER".to_owned()];
    assert_eq!(
        route.validate().unwrap_err().kind(),
        ErrorKind::ValidationError
    );
}

#[test]
fn a_disabled_route_carries_the_flag() {
    let mut route = Route::default();
    route.r#match = Some(RouteMatch::Http(http_match()));
    route.enabled = false;
    assert!(!route.enabled);
    assert!(route.validate().is_ok(), "disabling is not a validation error");
}
