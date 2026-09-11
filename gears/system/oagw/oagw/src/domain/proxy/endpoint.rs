//! Endpoint selection: `X-OAGW-Target-Host` consumption and validation, and
//! round-robin distribution across a multi-endpoint pool
//! (`cpt-cf-oagw-algo-endpoint-selection`, `cpt-cf-oagw-dod-target-host-selection`,
//! `cpt-cf-oagw-adr-request-routing`'s behaviour matrix).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use axum::http::HeaderName;
use uuid::Uuid;

use crate::domain::alias::{Derivation, derive_root_alias, is_valid_host, normalize_alias};
use crate::domain::model::Endpoint;
use crate::error::OagwError;
use crate::state::ControlPlaneState;

/// The routing header read during endpoint selection and always stripped
/// before the request reaches the upstream (`cpt-cf-oagw-dod-target-host-selection`).
pub static TARGET_HOST_HEADER: HeaderName = HeaderName::from_static("x-oagw-target-host");

/// `true` when `endpoints` share a registrable common domain suffix (a
/// "common suffix" alias, ADR-0001's matrix), as opposed to an "explicit"
/// alias over heterogeneous or IP-addressed endpoints. Reuses the same
/// derivation logic the upstream management feature applies at write time.
fn is_common_suffix_pool(endpoints: &[Endpoint]) -> bool {
    matches!(derive_root_alias(endpoints), Derivation::Derived(_))
}

/// Selects one endpoint from `endpoints` per ADR-0001's `X-OAGW-Target-Host`
/// behaviour matrix (`cpt-cf-oagw-dod-target-host-selection`,
/// `cpt-cf-oagw-algo-endpoint-selection`).
///
/// # Errors
///
/// Returns [`OagwError::invalid_target_host`] when `header_value` is
/// present but not a bare hostname or IP address;
/// [`OagwError::unknown_target_host`] when it names no configured endpoint;
/// [`OagwError::missing_target_host`] when the pool holds several endpoints
/// behind a common-suffix alias and no header was supplied.
// @cpt-begin:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-select-fn-01
pub fn select_endpoint(
    state: &ControlPlaneState,
    upstream_id: Uuid,
    endpoints: &[Endpoint],
    header_value: Option<&str>,
) -> Result<Endpoint, OagwError> {
    if let Some(value) = header_value {
        return select_by_header(endpoints, value);
    }
    select_without_header(state, upstream_id, endpoints)
}

fn select_by_header(endpoints: &[Endpoint], value: &str) -> Result<Endpoint, OagwError> {
    if !is_valid_host(value) {
        return Err(OagwError::invalid_target_host(format!(
            "'{value}' is not a bare hostname or IP address"
        ))
        .with_invalid_value(value));
    }
    let normalized = normalize_alias(value);
    endpoints
        .iter()
        .find(|endpoint| endpoint.host.to_ascii_lowercase() == normalized)
        .cloned()
        .ok_or_else(|| {
            OagwError::unknown_target_host(format!(
                "'{value}' does not match any configured endpoint"
            ))
            .with_invalid_value(value)
            .with_valid_hosts(valid_hosts(endpoints))
        })
}

fn select_without_header(
    state: &ControlPlaneState,
    upstream_id: Uuid,
    endpoints: &[Endpoint],
) -> Result<Endpoint, OagwError> {
    match endpoints.len() {
        0 => Err(OagwError::downstream_error(
            "upstream declares no server endpoints",
        )),
        1 => Ok(endpoints[0].clone()),
        _ if is_common_suffix_pool(endpoints) => Err(OagwError::missing_target_host(
            "X-OAGW-Target-Host is required for this multi-endpoint, common-suffix-alias upstream",
        )
        .with_valid_hosts(valid_hosts(endpoints))),
        pool_len => {
            let idx = state.next_round_robin_index(upstream_id, pool_len);
            Ok(endpoints[idx].clone())
        }
    }
}

fn valid_hosts(endpoints: &[Endpoint]) -> Vec<String> {
    endpoints.iter().map(|e| e.host.clone()).collect()
}
// @cpt-end:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-select-fn-01

#[cfg(test)]
mod tests {
    use super::{select_endpoint, valid_hosts};
    use crate::domain::model::Scheme;
    use crate::state::ControlPlaneState;
    use uuid::Uuid;

    fn endpoint(host: &str) -> crate::domain::model::Endpoint {
        crate::domain::model::Endpoint {
            scheme: Scheme::Https,
            host: host.to_owned(),
            port: Some(443),
        }
    }

    #[test]
    fn a_single_endpoint_is_selected_with_no_header() {
        let state = ControlPlaneState::new();
        let endpoints = vec![endpoint("api.example.com")];
        let selected = select_endpoint(&state, Uuid::new_v4(), &endpoints, None)
            .expect("single endpoint must be selected");
        assert_eq!(selected.host, "api.example.com");
    }

    #[test]
    fn a_single_endpoint_with_a_valid_header_is_validated_then_selected() {
        let state = ControlPlaneState::new();
        let endpoints = vec![endpoint("api.example.com")];
        let selected = select_endpoint(&state, Uuid::new_v4(), &endpoints, Some("api.example.com"))
            .expect("must select");
        assert_eq!(selected.host, "api.example.com");
    }

    // @cpt-begin:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-explicit-round-robin-test-01
    #[test]
    fn an_explicit_multi_endpoint_pool_round_robins_with_no_header() {
        let state = ControlPlaneState::new();
        let upstream_id = Uuid::new_v4();
        let endpoints = vec![endpoint("us.foo.com"), endpoint("eu.bar.com")];
        let mut counts = std::collections::HashMap::new();
        for _ in 0..10 {
            let selected = select_endpoint(&state, upstream_id, &endpoints, None)
                .expect("round robin must select");
            *counts.entry(selected.host).or_insert(0) += 1;
        }
        assert_eq!(counts.get("us.foo.com"), Some(&5));
        assert_eq!(counts.get("eu.bar.com"), Some(&5));
    }
    // @cpt-end:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-explicit-round-robin-test-01

    #[test]
    fn an_explicit_multi_endpoint_pool_with_a_header_bypasses_round_robin() {
        let state = ControlPlaneState::new();
        let upstream_id = Uuid::new_v4();
        let endpoints = vec![endpoint("us.foo.com"), endpoint("eu.bar.com")];
        let selected = select_endpoint(&state, upstream_id, &endpoints, Some("eu.bar.com"))
            .expect("must select");
        assert_eq!(selected.host, "eu.bar.com");
    }

    // @cpt-begin:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-missing-header-test-01
    #[test]
    fn a_common_suffix_pool_with_no_header_is_rejected_as_missing() {
        let state = ControlPlaneState::new();
        let endpoints = vec![endpoint("us.vendor.com"), endpoint("eu.vendor.com")];
        let error = select_endpoint(&state, Uuid::new_v4(), &endpoints, None)
            .expect_err("common-suffix pool requires the header");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
    // @cpt-end:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-missing-header-test-01

    // @cpt-begin:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-header-selects-test-01
    #[test]
    fn a_common_suffix_pool_with_a_matching_header_selects_that_endpoint() {
        let state = ControlPlaneState::new();
        let endpoints = vec![endpoint("us.vendor.com"), endpoint("eu.vendor.com")];
        let selected = select_endpoint(&state, Uuid::new_v4(), &endpoints, Some("us.vendor.com"))
            .expect("must select");
        assert_eq!(selected.host, "us.vendor.com");
    }
    // @cpt-end:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-header-selects-test-01

    // @cpt-begin:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-invalid-header-test-01
    #[test]
    fn a_header_carrying_a_port_is_rejected_as_invalid() {
        let state = ControlPlaneState::new();
        let endpoints = vec![endpoint("us.vendor.com"), endpoint("eu.vendor.com")];
        let error = select_endpoint(
            &state,
            Uuid::new_v4(),
            &endpoints,
            Some("us.vendor.com:8443"),
        )
        .expect_err("a port-carrying value must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        let problem = error.to_problem();
        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-invalid-header-test-01

    // @cpt-begin:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-unknown-header-test-01
    #[test]
    fn a_header_matching_no_endpoint_is_rejected_as_unknown() {
        let state = ControlPlaneState::new();
        let endpoints = vec![endpoint("us.vendor.com"), endpoint("eu.vendor.com")];
        let error = select_endpoint(&state, Uuid::new_v4(), &endpoints, Some("apac.vendor.com"))
            .expect_err("an unmatched value must be rejected");
        let problem = error.to_problem();
        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-target-host-selection:p1:inst-endpoint-unknown-header-test-01

    #[test]
    fn valid_hosts_lists_every_endpoint_host() {
        let endpoints = vec![endpoint("a.example.com"), endpoint("b.example.com")];
        assert_eq!(
            valid_hosts(&endpoints),
            vec!["a.example.com".to_owned(), "b.example.com".to_owned()]
        );
    }
}
