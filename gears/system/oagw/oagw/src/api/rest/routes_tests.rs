//! Route registration tests: the paths OAGW claims, and the auth posture it
//! declares for them.

use super::{
    PLUGINS_PATH, PROXY_PATH, PROXY_PATH_SUFFIX, ROUTES_PATH, UPSTREAMS_PATH, register_routes,
};
use crate::test_support::{control_plane, data_plane};
use http::Method;
use std::collections::BTreeSet;
use std::sync::Mutex;
use toolkit::api::{OpenApiRegistry, OperationSpec};

#[derive(Default)]
struct RecordingRegistry {
    operations: Mutex<Vec<OperationSpec>>,
}

impl OpenApiRegistry for RecordingRegistry {
    fn register_operation(&self, spec: &OperationSpec) {
        if let Ok(mut operations) = self.operations.lock() {
            operations.push(spec.clone());
        }
    }

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn registered() -> Vec<OperationSpec> {
    let registry = RecordingRegistry::default();
    let control_plane = control_plane();
    let data_plane = data_plane(&control_plane);
    let _router = register_routes(axum::Router::new(), &registry, control_plane, data_plane);
    registry.operations.into_inner().expect("operations")
}

#[test]
fn every_documented_management_endpoint_is_registered_gear_relative() {
    let specs = registered();
    let claimed: BTreeSet<(Method, String)> = specs
        .iter()
        .map(|spec| (spec.method.clone(), spec.path.clone()))
        .collect();

    let expected = [
        (Method::POST, UPSTREAMS_PATH.to_owned()),
        (Method::GET, UPSTREAMS_PATH.to_owned()),
        (Method::GET, format!("{UPSTREAMS_PATH}/{{id}}")),
        (Method::PUT, format!("{UPSTREAMS_PATH}/{{id}}")),
        (Method::DELETE, format!("{UPSTREAMS_PATH}/{{id}}")),
        (Method::POST, ROUTES_PATH.to_owned()),
        (Method::GET, ROUTES_PATH.to_owned()),
        (Method::GET, format!("{ROUTES_PATH}/{{id}}")),
        (Method::PUT, format!("{ROUTES_PATH}/{{id}}")),
        (Method::DELETE, format!("{ROUTES_PATH}/{{id}}")),
        (Method::POST, PLUGINS_PATH.to_owned()),
        (Method::GET, PLUGINS_PATH.to_owned()),
        (Method::GET, format!("{PLUGINS_PATH}/{{id}}")),
        (Method::DELETE, format!("{PLUGINS_PATH}/{{id}}")),
        (Method::GET, format!("{PLUGINS_PATH}/{{id}}/source")),
    ];
    for entry in expected {
        assert!(
            claimed.contains(&entry),
            "{} {} must be registered; got {claimed:?}",
            entry.0,
            entry.1
        );
    }
}

#[test]
fn no_registered_path_repeats_the_gateway_prefix() {
    for spec in registered() {
        assert!(
            spec.path.starts_with("/oagw/v1/"),
            "'{}' must be gear-relative: the gateway nests this router under its own prefix",
            spec.path
        );
        assert!(
            !spec.path.starts_with("/api/"),
            "'{}' must not repeat the operator gateway's prefix",
            spec.path
        );
    }
}

#[test]
fn the_proxy_endpoint_covers_every_method_on_both_shapes() {
    let specs = registered();
    let proxy: BTreeSet<(Method, String)> = specs
        .iter()
        .filter(|spec| spec.path.starts_with("/oagw/v1/proxy/"))
        .map(|spec| (spec.method.clone(), spec.path.clone()))
        .collect();
    for path in [PROXY_PATH, PROXY_PATH_SUFFIX] {
        for method in [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
        ] {
            assert!(
                proxy.contains(&(method.clone(), path.to_owned())),
                "{method} {path} must be registered"
            );
        }
    }
}

#[test]
fn every_endpoint_requires_authentication() {
    for spec in registered() {
        assert!(
            spec.authenticated,
            "'{} {}' must require a bearer token",
            spec.method, spec.path
        );
    }
}

#[test]
fn list_endpoints_declare_the_odata_options() {
    for spec in registered() {
        let is_list = spec.method == Method::GET
            && [UPSTREAMS_PATH, ROUTES_PATH, PLUGINS_PATH].contains(&spec.path.as_str());
        if !is_list {
            continue;
        }
        let names: BTreeSet<&str> = spec
            .params
            .iter()
            .map(|param| param.name.as_str())
            .collect();
        for option in ["$filter", "$select", "$orderby", "$top", "$skip"] {
            assert!(
                names.contains(option),
                "'{}' must document {option}",
                spec.path
            );
        }
    }
}

#[test]
fn the_proxy_endpoint_documents_the_target_host_header() {
    for spec in registered() {
        if !spec.path.starts_with("/oagw/v1/proxy/") {
            continue;
        }
        assert!(
            spec.params
                .iter()
                .any(|param| param.name == "X-OAGW-Target-Host"),
            "'{} {}' must document X-OAGW-Target-Host",
            spec.method,
            spec.path
        );
    }
}
