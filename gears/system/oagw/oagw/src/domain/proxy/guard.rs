//! Guard evaluation: query-parameter allowlist, path-suffix mode, and CORS
//! enforcement on an actual (non-preflight) request
//! (`cpt-cf-oagw-algo-guard-evaluation`, `cpt-cf-oagw-dod-query-allowlist-guard`,
//! `cpt-cf-oagw-dod-path-suffix-guard`, `cpt-cf-oagw-dod-cors-request-enforcement`).
//!
//! Guards run strictly after route selection (`cpt-cf-oagw-feature-config-resolution`
//! already reported a non-matching route as `RouteNotFound`), and in the
//! fixed order the DESIGN.md table lists: CORS origin, CORS method, query
//! parameters, path suffix.
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use crate::domain::model::{CorsConfig, PathSuffixMode, WILDCARD_ORIGIN};
use crate::error::OagwError;

/// Rejects a request whose `Origin` does not match `allowed_origins`
/// exactly, or whose method is absent from `allowed_methods`, when the
/// effective CORS configuration is enabled and the request carries an
/// `Origin` header (`cpt-cf-oagw-dod-cors-request-enforcement`).
///
/// CORS enforcement is skipped entirely when `cors` is `None`, disabled, or
/// the request carries no `Origin` header — matching a same-origin or
/// non-browser caller.
///
/// # Errors
///
/// Returns [`OagwError::cors_origin_not_allowed`] or
/// [`OagwError::cors_method_not_allowed`].
// @cpt-begin:cpt-cf-oagw-dod-cors-request-enforcement:p1:inst-cors-enforce-fn-01
pub fn enforce_cors_actual_request(
    cors: Option<&CorsConfig>,
    origin: Option<&str>,
    method: &str,
) -> Result<(), OagwError> {
    let Some(cors) = cors else {
        return Ok(());
    };
    if !cors.enabled {
        return Ok(());
    }
    let Some(origin) = origin else {
        return Ok(());
    };
    reject_disallowed_origin(cors, origin)?;
    reject_disallowed_method(cors, method)
}

fn reject_disallowed_origin(cors: &CorsConfig, origin: &str) -> Result<(), OagwError> {
    let allowed = cors
        .allowed_origins
        .iter()
        .any(|allowed| allowed == WILDCARD_ORIGIN || allowed == origin);
    if allowed {
        Ok(())
    } else {
        Err(OagwError::cors_origin_not_allowed(format!(
            "origin '{origin}' not in allowed origins list"
        )))
    }
}

fn reject_disallowed_method(cors: &CorsConfig, method: &str) -> Result<(), OagwError> {
    let allowed = cors
        .allowed_methods
        .iter()
        .any(|allowed| allowed.as_str() == method);
    if allowed {
        Ok(())
    } else {
        Err(OagwError::cors_method_not_allowed(format!(
            "method '{method}' not in allowed methods list"
        )))
    }
}
// @cpt-end:cpt-cf-oagw-dod-cors-request-enforcement:p1:inst-cors-enforce-fn-01

/// Rejects a request carrying a query parameter absent from
/// `query_allowlist`, treating an empty allowlist as permitting no
/// parameter at all (`cpt-cf-oagw-dod-query-allowlist-guard`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] naming the first disallowed
/// parameter.
// @cpt-begin:cpt-cf-oagw-dod-query-allowlist-guard:p1:inst-query-allowlist-fn-01
pub fn enforce_query_allowlist(query: Option<&str>, allowlist: &[String]) -> Result<(), OagwError> {
    let Some(query) = query else {
        return Ok(());
    };
    for (name, _) in form_urlencoded::parse(query.as_bytes()) {
        if !allowlist.iter().any(|allowed| allowed == name.as_ref()) {
            return Err(OagwError::validation_error(format!(
                "query parameter '{name}' is not in the allowed list"
            )));
        }
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-query-allowlist-guard:p1:inst-query-allowlist-fn-01

/// Rejects a request supplying a non-empty path suffix beyond the matched
/// route's `match.http.path` while `path_suffix_mode` is `disabled`
/// (`cpt-cf-oagw-dod-path-suffix-guard`); regardless of `path_suffix_mode`,
/// also rejects a suffix containing a `.` or `..` path segment (BUG2-F-001):
/// `Append` mode's whole purpose is to forward a client-supplied suffix
/// verbatim into the outbound target string, which `url::Url::parse` later
/// normalizes per WHATWG dot-segment rules — an un-rejected `..` segment
/// would let a caller walk back out of the route's configured prefix onto an
/// arbitrary upstream path, defeating route-based access control.
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] naming the rejected suffix.
// @cpt-begin:cpt-cf-oagw-dod-path-suffix-guard:p1:inst-path-suffix-fn-01
pub fn enforce_path_suffix(
    route_path: &str,
    full_path: &str,
    mode: PathSuffixMode,
) -> Result<(), OagwError> {
    let suffix = full_path.strip_prefix(route_path).unwrap_or(full_path);
    reject_dot_segments(suffix)?;

    if mode != PathSuffixMode::Disabled {
        return Ok(());
    }
    if suffix.is_empty() {
        Ok(())
    } else {
        Err(OagwError::validation_error(format!(
            "path suffix '{suffix}' is not permitted; path_suffix_mode is disabled"
        )))
    }
}
// @cpt-end:cpt-cf-oagw-dod-path-suffix-guard:p1:inst-path-suffix-fn-01

/// Rejects `suffix` when any `/`-delimited segment, after percent-decoding,
/// is exactly `.` or `..` (BUG2-F-001). `pub(crate)` so the WebSocket target
/// URL builder (`crate::domain::proxy::websocket::build_ws_target_url`)
/// applies the identical check at the point its own target string is built,
/// independent of this guard already having run over the same path earlier
/// in the pipeline. A literal dot inside a longer segment (e.g.
/// `report.v2.json`) is untouched: only a segment that decodes to exactly
/// `.` or `..` is rejected, so `@`-in-path and `//host`-in-path variants
/// (confirmed not to escape the fixed authority) are left alone.
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] naming the offending suffix.
pub(crate) fn reject_dot_segments(suffix: &str) -> Result<(), OagwError> {
    let contains_dot_segment = suffix.split('/').any(|segment| {
        let decoded = percent_decode_ascii(segment);
        decoded == b"." || decoded == b".."
    });
    if contains_dot_segment {
        Err(OagwError::validation_error(format!(
            "path suffix '{suffix}' contains a disallowed '.' or '..' path segment"
        )))
    } else {
        Ok(())
    }
}

/// Decodes `%XX` escapes in `segment` into raw bytes, leaving any other byte
/// (including a malformed escape) untouched: used only to detect a segment
/// that decodes to exactly `.` or `..`, never to reconstruct a real decoded
/// path that is fed back into the outbound request.
fn percent_decode_ascii(segment: &str) -> Vec<u8> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// The value of one ASCII hex digit, case-insensitively, or `None` for a
/// non-hex byte.
fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{enforce_cors_actual_request, enforce_path_suffix, enforce_query_allowlist};
    use crate::domain::model::{CorsConfig, HttpMethod, PathSuffixMode, Sharing};

    fn cors(enabled: bool, origins: &[&str], methods: &[HttpMethod]) -> CorsConfig {
        CorsConfig {
            sharing: Sharing::Private,
            enabled,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.to_vec(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    #[test]
    fn disabled_cors_never_rejects() {
        let cfg = cors(false, &["https://app.example.com"], &[HttpMethod::Get]);
        assert!(
            enforce_cors_actual_request(Some(&cfg), Some("https://evil.com"), "DELETE").is_ok()
        );
    }

    #[test]
    fn no_origin_header_skips_enforcement() {
        let cfg = cors(true, &["https://app.example.com"], &[HttpMethod::Get]);
        assert!(enforce_cors_actual_request(Some(&cfg), None, "DELETE").is_ok());
    }

    #[test]
    fn disallowed_origin_is_rejected_with_403() {
        let cfg = cors(true, &["https://app.example.com"], &[HttpMethod::Get]);
        let error = enforce_cors_actual_request(Some(&cfg), Some("https://evil.com"), "GET")
            .expect_err("disallowed origin must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn disallowed_method_is_rejected_with_403() {
        let cfg = cors(true, &["https://app.example.com"], &[HttpMethod::Get]);
        let error =
            enforce_cors_actual_request(Some(&cfg), Some("https://app.example.com"), "DELETE")
                .expect_err("disallowed method must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn wildcard_origin_matches_any() {
        let cfg = cors(true, &["*"], &[HttpMethod::Get]);
        assert!(
            enforce_cors_actual_request(Some(&cfg), Some("https://anything.example.com"), "GET")
                .is_ok()
        );
    }

    #[test]
    fn empty_allowlist_rejects_every_parameter() {
        let error = enforce_query_allowlist(Some("debug=1"), &[])
            .expect_err("empty allowlist must reject every parameter");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn allowlisted_parameter_passes() {
        assert!(enforce_query_allowlist(Some("limit=5"), &["limit".to_owned()]).is_ok());
    }

    #[test]
    fn no_query_string_always_passes() {
        assert!(enforce_query_allowlist(None, &[]).is_ok());
    }

    #[test]
    fn disallowed_parameter_name_is_named_in_the_detail() {
        let error = enforce_query_allowlist(Some("debug=1"), &["limit".to_owned()])
            .expect_err("must reject");
        let problem = error.to_problem();
        assert!(problem.detail.contains("debug"));
    }

    #[test]
    fn suffix_disabled_mode_rejects_a_non_empty_suffix() {
        let error = enforce_path_suffix("/v1/items", "/v1/items/extra", PathSuffixMode::Disabled)
            .expect_err("a suffix must be rejected when disabled");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn suffix_disabled_mode_allows_an_exact_match() {
        assert!(enforce_path_suffix("/v1/items", "/v1/items", PathSuffixMode::Disabled).is_ok());
    }

    #[test]
    fn append_mode_allows_a_suffix() {
        assert!(
            enforce_path_suffix("/v1/items", "/v1/items/extra", PathSuffixMode::Append).is_ok()
        );
    }

    // BUG2-F-001 regression: a client-supplied suffix carrying a `.`/`..`
    // path segment must never reach `build_target_url`, regardless of
    // `path_suffix_mode`.

    #[test]
    fn a_literal_dot_dot_segment_is_rejected_even_under_append_mode() {
        let error = enforce_path_suffix(
            "/v1/items",
            "/v1/items/../../secret",
            PathSuffixMode::Append,
        )
        .expect_err("a literal '..' segment must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_percent_encoded_dot_dot_segment_is_rejected() {
        let error = enforce_path_suffix(
            "/v1/items",
            "/v1/items/%2e%2e/secret",
            PathSuffixMode::Append,
        )
        .expect_err("a percent-encoded '..' segment must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_single_dot_segment_is_rejected() {
        let error = enforce_path_suffix("/v1/items", "/v1/items/./secret", PathSuffixMode::Append)
            .expect_err("a single-dot segment must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_dot_inside_a_filename_segment_is_still_allowed() {
        assert!(
            enforce_path_suffix(
                "/v1/items",
                "/v1/items/report.v2.json",
                PathSuffixMode::Append,
            )
            .is_ok(),
            "a literal dot inside a longer segment must not be treated as a dot-segment"
        );
    }
}
