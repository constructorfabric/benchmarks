//! The `required_headers` guard plugin (`cpt-cf-oagw-algo-required-headers-check`,
//! `cpt-cf-oagw-dod-required-headers-guard`, `cpt-cf-oagw-adr-required-headers-guard-plugin`).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use axum::http::{HeaderMap, HeaderName};

use crate::error::OagwError;

/// The RFC 9457 `error_code` carried by every rejection this guard raises.
pub const ERROR_CODE_REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

const REQUEST_KEY: &str = "required_request_headers";
const RESPONSE_KEY: &str = "required_response_headers";

/// Checks the request phase's configured header names for presence
/// (`cpt-cf-oagw-algo-required-headers-check`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] (`400`) carrying
/// [`ERROR_CODE_REQUIRED_HEADER_MISSING`] naming the first missing header,
/// in configured order.
pub fn check_request(
    headers: &HeaderMap,
    config: Option<&serde_json::Value>,
) -> Result<(), OagwError> {
    check_phase(headers, config, REQUEST_KEY, Phase::Request)
}

/// Checks the response phase's configured header names for presence
/// (`cpt-cf-oagw-algo-required-headers-check`).
///
/// # Errors
///
/// Returns [`OagwError::downstream_error`] (`502`) carrying
/// [`ERROR_CODE_REQUIRED_HEADER_MISSING`] naming the first missing header,
/// in configured order.
pub fn check_response(
    headers: &HeaderMap,
    config: Option<&serde_json::Value>,
) -> Result<(), OagwError> {
    check_phase(headers, config, RESPONSE_KEY, Phase::Response)
}

#[derive(Clone, Copy)]
enum Phase {
    Request,
    Response,
}

// @cpt-begin:cpt-cf-oagw-algo-required-headers-check:p2:inst-required-headers-check-fn-01
fn check_phase(
    headers: &HeaderMap,
    config: Option<&serde_json::Value>,
    key: &str,
    phase: Phase,
) -> Result<(), OagwError> {
    let Some(raw) = config.and_then(|c| c.get(key)).and_then(|v| v.as_str()) else {
        // Absent configuration fails open for this phase
        // (`cpt-cf-oagw-adr-required-headers-guard-plugin`).
        return Ok(());
    };
    let names = parse_names(raw);
    let Some(missing) = first_missing(headers, &names) else {
        return Ok(());
    };
    let detail = format!("required header '{missing}' is missing");
    Err(match phase {
        Phase::Request => {
            OagwError::validation_error(detail).with_error_code(ERROR_CODE_REQUIRED_HEADER_MISSING)
        }
        Phase::Response => {
            OagwError::downstream_error(detail).with_error_code(ERROR_CODE_REQUIRED_HEADER_MISSING)
        }
    })
}
// @cpt-end:cpt-cf-oagw-algo-required-headers-check:p2:inst-required-headers-check-fn-01

/// Splits on `,`, trims each entry, lowercases it, and drops empty entries —
/// so a blank-only configuration (e.g. `", , ,"`) yields an empty list and
/// the phase admits every request.
fn parse_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The first configured name (in order) absent from `headers`, compared
/// case-insensitively and checking presence only.
fn first_missing(headers: &HeaderMap, names: &[String]) -> Option<String> {
    names
        .iter()
        .find(|name| {
            HeaderName::from_bytes(name.as_bytes())
                .is_ok_and(|header_name| !headers.contains_key(header_name))
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::{check_request, check_response};
    use axum::http::HeaderMap;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    // @cpt-begin:cpt-cf-oagw-dod-required-headers-guard:p2:inst-required-headers-missing-test-01
    #[test]
    fn a_missing_configured_header_rejects_with_400() {
        let config = json!({"required_request_headers": "x-correlation-id,accept"});
        let error =
            check_request(&headers(&[("accept", "*/*")]), Some(&config)).expect_err("must reject");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
    // @cpt-end:cpt-cf-oagw-dod-required-headers-guard:p2:inst-required-headers-missing-test-01

    // @cpt-begin:cpt-cf-oagw-dod-required-headers-guard:p2:inst-required-headers-case-test-01
    #[test]
    fn matching_is_case_insensitive() {
        let config = json!({"required_request_headers": "x-correlation-id,accept"});
        let ok = check_request(
            &headers(&[("x-correlation-id", "abc"), ("Accept", "*/*")]),
            Some(&config),
        );
        assert!(ok.is_ok());
    }
    // @cpt-end:cpt-cf-oagw-dod-required-headers-guard:p2:inst-required-headers-case-test-01

    // @cpt-begin:cpt-cf-oagw-dod-required-headers-guard:p2:inst-required-headers-response-test-01
    #[test]
    fn a_missing_response_header_rejects_with_502() {
        let config = json!({"required_response_headers": "content-type"});
        let error = check_response(&HeaderMap::new(), Some(&config)).expect_err("must reject");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_GATEWAY);
    }
    // @cpt-end:cpt-cf-oagw-dod-required-headers-guard:p2:inst-required-headers-response-test-01

    // @cpt-begin:cpt-cf-oagw-dod-required-headers-guard:p2:inst-required-headers-blank-test-01
    #[test]
    fn a_blank_only_configuration_admits_every_request() {
        let config = json!({"required_request_headers": ", , ,"});
        assert!(check_request(&HeaderMap::new(), Some(&config)).is_ok());
    }
    // @cpt-end:cpt-cf-oagw-dod-required-headers-guard:p2:inst-required-headers-blank-test-01

    #[test]
    fn absent_configuration_fails_open() {
        assert!(check_request(&HeaderMap::new(), None).is_ok());
        assert!(check_response(&HeaderMap::new(), None).is_ok());
    }

    #[test]
    fn the_two_phase_keys_are_independent() {
        let config = json!({"required_request_headers": "x-only"});
        // Response phase is unconfigured and therefore a no-op even though
        // the request phase would reject.
        assert!(check_response(&HeaderMap::new(), Some(&config)).is_ok());
    }
}
