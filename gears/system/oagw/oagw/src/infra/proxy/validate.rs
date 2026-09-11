//! Request, body and CORS validation.
//!
//! `cpt-cf-oagw-algo-request-validation` owns the framing checks the DESIGN's
//! body-validation table lists: a strict header section, a `Content-Length` and
//! `Transfer-Encoding` framing that cannot be read two ways, the 100MB hard
//! limit enforced *before* any byte is buffered, the declared-versus-actual
//! comparison and the CORS enforcement of an actual cross-origin request.
//!
//! The framing check runs in two halves so the 413 never depends on a body the
//! gateway has already read: [`framing`] and [`check_declared`] are pure
//! functions of the request head and run before the handler touches the body
//! stream, and [`check_buffered`] runs after the buffer is complete.

use http::HeaderMap;

use crate::domain::error::DomainError;
use crate::infra::proxy::effective::EffectiveUpstream;

/// The hard body limit of `cpt-cf-oagw-constraint-body-limit`, in bytes.
pub const BODY_HARD_LIMIT: u64 = 100 * 1024 * 1024;

/// The `Vary` member a response to a request that carried `Origin` includes.
pub const VARY_ORIGIN: &str = "Origin";

/// The framing of an inbound request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framing {
    /// The declared `Content-Length`, when the request declares one.
    pub declared_length: Option<u64>,
    /// Whether the request declares a `chunked` body.
    pub chunked: bool,
}

impl Framing {
    /// The length the framing promises, `None` when the body has no declared
    /// end and the reader must stop at the limit.
    #[must_use]
    pub fn promised(&self) -> Option<u64> {
        if self.chunked {
            None
        } else {
            self.declared_length
        }
    }
}

// @cpt-begin:cpt-cf-oagw-dod-body-validation:p1:inst-full
/// Parse the request's header section strictly.
///
/// # Errors
///
/// Returns the mapped `400` of a header name that is not a token, a value that
/// carries CR or LF or another control byte, and of a header section that
/// cannot be read without recovery.
pub fn validate_header_section(headers: &HeaderMap) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-01
    for (name, value) in headers.iter() {
        check_header_pair(name.as_str(), value.as_bytes())?;
    }
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-01
}

/// Check one header name and value as a well-formed field.
///
/// # Errors
///
/// Returns the mapped `400` of a name that is not a token and of a value that
/// carries CR, LF or a NUL byte.
pub fn check_header_pair(name: &str, value: &[u8]) -> Result<(), DomainError> {
    if !is_token(name) {
        return Err(DomainError::ValidationError {
            detail: format!("the header name `{name}` is not a valid token"),
        });
    }
    if value
        .iter()
        .any(|byte| matches!(byte, b'\r' | b'\n' | 0x00))
    {
        return Err(DomainError::ValidationError {
            detail: format!("the header `{name}` carries a control byte"),
        });
    }
    Ok(())
}

/// Whether a header name is a valid field token.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
                        | b'^' | b'_' | b'`' | b'|' | b'~'
                )
        })
}

/// Parse the body framing of an inbound request.
///
/// # Errors
///
/// Returns the mapped `400` of a `Content-Length` that is not a valid integer,
/// a request that carries both `Content-Length` and `Transfer-Encoding`,
/// duplicate `Content-Length` values that disagree, and a `Transfer-Encoding`
/// whose value is not `chunked`.
pub fn framing(headers: &HeaderMap) -> Result<Framing, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-02
    // A body that can be read two ways is a request-smuggling vector, so every
    // ambiguous combination is refused instead of interpreted.
    let lengths: Vec<&str> = headers
        .get_all(http::header::CONTENT_LENGTH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    let te: Vec<&str> = headers
        .get_all(http::header::TRANSFER_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    let mut declared: Option<u64> = None;
    for value in &lengths {
        let Ok(parsed) = value.trim().parse::<u64>() else {
            return Err(DomainError::ValidationError {
                detail: "the `Content-Length` header is not a valid integer".to_owned(),
            });
        };
        if let Some(previous) = declared
            && previous != parsed {
                return Err(DomainError::ValidationError {
                    detail: "duplicate `Content-Length` headers disagree".to_owned(),
                });
            }
        declared = Some(parsed);
    }
    let chunked = if te.is_empty() {
        false
    } else if !lengths.is_empty() {
        return Err(DomainError::ValidationError {
            detail: "the request carries both `Content-Length` and `Transfer-Encoding`".to_owned(),
        });
    } else if te.iter().any(|value| !value.trim().eq_ignore_ascii_case("chunked")) {
        return Err(DomainError::ValidationError {
            detail: "the only supported transfer encoding is `chunked`".to_owned(),
        });
    } else {
        true
    };
    Ok(Framing {
        declared_length: declared,
        chunked,
    })
    // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-02
}

/// Check the framing before any body byte is buffered.
///
/// # Errors
///
/// Returns the mapped `413` of a declared body above the hard limit. The check
/// is a pure function of the request head, so the gateway refuses an oversized
/// declaration without reading the stream.
pub fn check_declared(framing: &Framing) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-03
    if let Some(declared) = framing.declared_length
        && declared > BODY_HARD_LIMIT {
            return Err(DomainError::PayloadTooLarge {
                detail: format!("the declared body exceeds the {BODY_HARD_LIMIT} byte limit"),
            });
        }
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-03
}

/// Check the buffered body against its framing.
///
/// # Errors
///
/// Returns the mapped `413` of an observed body above the hard limit and the
/// mapped `400` of an actual size that differs from the declared one.
pub fn check_buffered(framing: &Framing, actual: u64) -> Result<(), DomainError> {
// @cpt-end:cpt-cf-oagw-dod-body-validation:p1:inst-full
    // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-04
    // The limit is checked again on the observed size: a chunked body declares
    // no length, so the observed size is the only thing that can exceed it.
    if actual > BODY_HARD_LIMIT {
        return Err(DomainError::PayloadTooLarge {
            detail: format!("the body exceeds the {BODY_HARD_LIMIT} byte limit"),
        });
    }
    // A mismatch between what the request declared and what it delivered is a
    // framing fault, not a size fault.
    if let Some(declared) = framing.declared_length
        && !framing.chunked && declared != actual {
            return Err(DomainError::ValidationError {
                detail: format!(
                    "the body is {actual} bytes but `Content-Length` declares {declared}"
                ),
            });
        }
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-04
}

/// The outcome of the CORS check of an actual cross-origin request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsDecision {
    /// The value of `Access-Control-Allow-Origin`.
    pub allow_origin: String,
    /// Whether `Access-Control-Allow-Credentials` is set.
    pub allow_credentials: bool,
    /// The value of `Access-Control-Allow-Headers` on a preflight.
    pub request_headers: Option<String>,
    /// The headers the browser may read.
    pub expose_headers: Vec<String>,
}

// @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-01
// @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-02
/// The actual-request CORS decision of a request the chain classified as a CORS
/// request (`cpt-cf-oagw-algo-cors-enforce`).
///
/// The caller has already decided, per `inst-ace-01`, that `cors.enabled` is
/// true on the effective configuration and that the request carries an `Origin`;
/// this decision matches the origin against `allowed_origins`, compares scheme,
/// host and port so a different port or a different protocol is a different
/// origin, treats `*` as a match for any origin and applies no pattern, suffix
/// or regex matching (`inst-ace-03`).
///
/// # Errors
///
/// Returns the mapped `403` of a disallowed origin (`inst-ace-05`) and of a
/// disallowed method (`inst-ace-07`); the response carries `Vary: Origin`
/// either way (`inst-ace-09`).
pub fn enforce_actual_request(
    effective: &EffectiveUpstream,
    origin: &str,
    method: &str,
) -> Result<CorsDecision, DomainError> {
    let Some(cors) = effective.cors.as_ref().filter(|cors| cors.enabled) else {
        // Unreachable through the entry-2.4 gate, kept for a decision reached
        // without it: a configuration that is not enabled admits no origin.
        return Err(DomainError::CorsOriginNotAllowed {
            detail: format!("the origin `{origin}` is not allowed"),
        });
    };
    // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-03
    // Matching is exact-string only: no pattern, suffix or regex origin
    // matching, so a crafted origin cannot match by suffix.
    let exact = cors
        .allowed_origins
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(origin));
    let wildcard = cors.allowed_origins.iter().any(|allowed| allowed == "*");
    // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-03
    // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-02

    // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-07
    // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-05
    // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-08
    // A disallowed origin returns the rejection to the pipeline as a `403` and
    // makes no upstream call behind it.
    if !exact && !wildcard {
        return Err(DomainError::CorsOriginNotAllowed {
            detail: format!("the origin `{origin}` is not allowed"),
        });
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-08
    // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-05
    // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-07

    // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-09
    // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-06
    // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-07
    // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-10
    // An allowed origin whose method is not in `allowed_methods` is refused with
    // the method outcome, and makes no upstream call either.
    if !effective.method_allowed(method) {
        return Err(DomainError::CorsMethodNotAllowed {
            detail: format!("the method `{method}` is not allowed for the origin"),
        });
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-10
    // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-07
    // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-06
    // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-09

    // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-08
    // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-11
    // `Access-Control-Allow-Origin` echoes the request origin, or is `*` when
    // the configuration's only allowed origin is the wildcard.
    let wildcard_only = wildcard && cors.allowed_origins.len() == 1;
    Ok(CorsDecision {
        allow_origin: if wildcard_only {
            "*".to_owned()
        } else {
            origin.to_owned()
        },
        // `Access-Control-Allow-Credentials` is set only for an exact origin:
        // a wildcard match never receives it, and the combination is rejected
        // at management validation anyway (`inst-ace-10`).
        // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-10
        allow_credentials: effective.allow_credentials() && exact && !wildcard_only,
        // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-10
        request_headers: None,
        expose_headers: effective.exposed_headers().to_vec(),
    })
    // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-11
    // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-08
}
// @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-01

// @cpt-begin:cpt-cf-oagw-dod-cors:p1:inst-full
/// Validate an actual cross-origin request against the effective CORS rules.
///
/// A request without an `Origin` header is same-origin and skips the check; a
/// request with `Origin` under a disabled or absent CORS configuration is
/// rejected, because the configuration is the only thing that admits a
/// cross-origin request.
///
/// The origin and method decision and the response headers are
/// [`enforce_actual_request`], which the plugin chain of entry 2.5 executes for
/// the same request.
///
/// # Errors
///
/// Returns the mapped `403` of a disallowed origin and of a disallowed method.
pub fn validate_cors(
    effective: &EffectiveUpstream,
    origin: Option<&str>,
    method: &str,
) -> Result<CorsDecision, DomainError> {
    let Some(origin) = origin else {
        return Ok(CorsDecision::same_origin());
    };

    // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-05
    if !effective.cors_enabled() {
        return Err(DomainError::CorsOriginNotAllowed {
            detail: format!("the origin `{origin}` is not allowed"),
        });
    }
    enforce_actual_request(effective, origin, method)
    // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-05
}

impl CorsDecision {
    /// The decision of a same-origin request: no CORS header is produced.
    #[must_use]
    pub const fn same_origin() -> Self {
        Self {
            allow_origin: String::new(),
            allow_credentials: false,
            request_headers: None,
            expose_headers: Vec::new(),
        }
    }
}

/// The CORS headers of a response, including `Vary: Origin`.
///
/// A response to a request that carried `Origin` always carries `Vary: Origin`,
/// per ADR 0004, so a shared cache cannot answer one origin with another
/// origin's response.
#[must_use]
pub fn cors_response_headers(decision: &CorsDecision, carried_origin: bool) -> Vec<(String, String)> {
// @cpt-end:cpt-cf-oagw-dod-cors:p1:inst-full
    // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-06
    let mut headers: Vec<(String, String)> = Vec::new();
    // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-09
    if carried_origin {
        headers.push(("vary".to_owned(), VARY_ORIGIN.to_owned()));
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-09
    if !decision.allow_origin.is_empty() {
        headers.push((
            "access-control-allow-origin".to_owned(),
            decision.allow_origin.clone(),
        ));
    }
    if decision.allow_credentials {
        headers.push((
            "access-control-allow-credentials".to_owned(),
            "true".to_owned(),
        ));
    }
    if !decision.expose_headers.is_empty() {
        headers.push((
            "access-control-expose-headers".to_owned(),
            decision.expose_headers.join(", "),
        ));
    }
    headers
    // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-06
}

/// Validate the well-known headers the gateway must own for forwarding.
///
/// # Errors
///
/// Returns the mapped `400` of a well-known header that cannot be validated,
/// set or adjusted: an absent or unusable `Host` authority.
pub fn validate_well_known(headers: &HeaderMap) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-07
    // The gateway rewrites the authority itself, so the inbound `Host` only has
    // to be parseable; a value it cannot parse cannot be adjusted either.
    if let Some(host) = headers.get(http::header::HOST) {
        let value = host.to_str().unwrap_or_default();
        if value.is_empty() || value.contains([' ', '\t', '\r', '\n']) {
            return Err(DomainError::ValidationError {
                detail: "the `Host` header is not a usable authority".to_owned(),
            });
        }
    }
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-07
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{CorsConfig, Sharing};
    use http::{HeaderName, HeaderValue};

    fn effective(enabled: bool, origins: &[&str], methods: &[&str]) -> EffectiveUpstream {
        EffectiveUpstream {
            cors: Some(CorsConfig {
                sharing: Sharing::Inherit,
                enabled,
                allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
                allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
                expose_headers: vec!["x-request-id".to_owned()],
                allow_credentials: false,
            }),
            ..EffectiveUpstream::default()
        }
    }

    fn map(entries: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("a valid header name"),
                HeaderValue::from_str(value).expect("a valid header value"),
            );
        }
        headers
    }

    #[test]
    fn a_plain_framing_is_accepted() {
        let framing = framing(&map(&[("content-length", "12")])).expect("a plain framing");
        assert_eq!(framing.declared_length, Some(12));
        assert!(!framing.chunked);
        assert_eq!(framing.promised(), Some(12));
    }

    #[test]
    fn a_chunked_framing_is_accepted() {
        let framing = framing(&map(&[("transfer-encoding", "chunked")])).expect("a chunked framing");
        assert!(framing.chunked);
        assert_eq!(framing.promised(), None);
    }

    #[test]
    fn a_non_integer_content_length_is_rejected() {
        let error = framing(&map(&[("content-length", "abc")]))
            .expect_err("the length is not an integer");
        assert_eq!(error.status(), 400, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[test]
    fn a_request_with_both_content_length_and_transfer_encoding_is_rejected() {
        let headers = map(&[("content-length", "5"), ("transfer-encoding", "chunked")]);
        let error = framing(&headers).expect_err("the framing is ambiguous");
        assert_eq!(error.status(), 400, "{error}");
    }

    #[test]
    fn disagreeing_duplicate_content_lengths_are_rejected() {
        let mut headers = map(&[("content-length", "5")]);
        headers.append(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_static("7"),
        );
        let error = framing(&headers).expect_err("the two lengths disagree");
        assert_eq!(error.status(), 400, "{error}");
    }

    #[test]
    fn agreeing_duplicate_content_lengths_are_accepted() {
        let mut headers = map(&[("content-length", "5")]);
        headers.append(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_static("5"),
        );
        let framing = framing(&headers).expect("the two lengths agree");
        assert_eq!(framing.declared_length, Some(5));
    }

    #[test]
    fn a_transfer_encoding_that_is_not_chunked_is_rejected() {
        let error = framing(&map(&[("transfer-encoding", "gzip")]))
            .expect_err("only chunked is supported");
        assert_eq!(error.status(), 400, "{error}");
    }

    #[test]
    fn a_declared_body_above_the_limit_is_refused_before_buffering() {
        let framing = Framing {
            declared_length: Some(BODY_HARD_LIMIT + 1),
            chunked: false,
        };
        let error = check_declared(&framing).expect_err("the declaration exceeds the limit");
        assert_eq!(error.status(), 413, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
        );
    }

    #[test]
    fn a_declared_body_at_the_limit_is_accepted() {
        let framing = Framing {
            declared_length: Some(BODY_HARD_LIMIT),
            chunked: false,
        };
        check_declared(&framing).expect("the limit is the boundary");
    }

    #[test]
    fn an_observed_body_above_the_limit_is_refused() {
        let framing = Framing {
            declared_length: None,
            chunked: true,
        };
        let error = check_buffered(&framing, BODY_HARD_LIMIT + 1)
            .expect_err("the observed body exceeds the limit");
        assert_eq!(error.status(), 413, "{error}");
    }

    #[test]
    fn a_body_that_differs_from_its_declared_length_is_rejected() {
        let framing = Framing {
            declared_length: Some(5),
            chunked: false,
        };
        let error = check_buffered(&framing, 4).expect_err("the sizes disagree");
        assert_eq!(error.status(), 400, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[test]
    fn a_chunked_body_is_compared_only_against_the_limit() {
        let framing = Framing {
            declared_length: None,
            chunked: true,
        };
        check_buffered(&framing, 1024).expect("a chunked body has no declared length");
    }

    #[test]
    fn a_disallowed_origin_is_rejected_with_403() {
        let rules = effective(true, &["https://app.dev"], &["GET"]);
        let error = validate_cors(&rules, Some("https://other.dev"), "GET")
            .expect_err("the origin is not allowed");
        assert_eq!(error.status(), 403, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
    }

    #[test]
    fn a_disallowed_method_is_rejected_with_403() {
        let rules = effective(true, &["https://app.dev"], &["GET"]);
        let error = validate_cors(&rules, Some("https://app.dev"), "DELETE")
            .expect_err("the method is not allowed");
        assert_eq!(error.status(), 403, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
        );
    }

    #[test]
    fn an_allowed_cross_origin_request_gets_a_decision() {
        let rules = effective(true, &["https://app.dev"], &["GET"]);
        let decision = validate_cors(&rules, Some("https://app.dev"), "GET")
            .expect("the origin and method are allowed");
        assert_eq!(decision.allow_origin, "https://app.dev");
        assert_eq!(decision.expose_headers, vec!["x-request-id".to_owned()]);
        assert!(!decision.allow_credentials);
    }

    #[test]
    fn a_cross_origin_request_under_a_disabled_configuration_is_rejected() {
        let rules = effective(false, &["*"], &["GET"]);
        let error = validate_cors(&rules, Some("https://app.dev"), "GET")
            .expect_err("CORS is disabled");
        assert_eq!(error.status(), 403, "{error}");
    }

    #[test]
    fn a_same_origin_request_skips_the_cors_check() {
        let rules = effective(true, &["https://app.dev"], &["GET"]);
        let decision = validate_cors(&rules, None, "GET").expect("no origin, no check");
        assert!(decision.allow_origin.is_empty());
    }

    #[test]
    fn a_response_to_a_request_that_carried_origin_varys_on_origin() {
        // `Vary: Origin` is a fixed member, per ADR 0004.
        assert_eq!(VARY_ORIGIN, "Origin");
        let decision = validate_cors(&effective(true, &["https://app.dev"], &["GET"]), Some("https://app.dev"), "GET")
            .expect("the request is allowed");
        let headers = cors_response_headers(&decision, true);
        assert!(headers.contains(&("vary".to_owned(), "Origin".to_owned())));
        assert!(headers.contains(&(
            "access-control-allow-origin".to_owned(),
            "https://app.dev".to_owned()
        )));
        assert!(headers.contains(&(
            "access-control-expose-headers".to_owned(),
            "x-request-id".to_owned()
        )));
    }

    #[test]
    fn a_response_to_a_same_origin_request_carries_no_cors_header() {
        let headers = cors_response_headers(&CorsDecision::same_origin(), false);
        assert!(headers.is_empty());
    }

    #[test]
    fn a_header_value_with_a_control_byte_is_rejected() {
        let error = check_header_pair("x-inject", b"a\r\nX-Evil: 1")
            .expect_err("the value is not clean");
        assert_eq!(error.status(), 400, "{error}");
        assert!(check_header_pair("x-nul", b"a\0b").is_err());
    }

    #[test]
    fn a_header_name_that_is_not_a_token_is_rejected() {
        assert!(check_header_pair("x a", b"1").is_err());
        assert!(check_header_pair("", b"1").is_err());
        assert!(check_header_pair("x-a", b"1").is_ok());
    }

    #[test]
    fn a_clean_header_section_is_accepted() {
        let headers = map(&[("content-type", "application/json"), ("x-a", "b")]);
        validate_header_section(&headers).expect("the section is clean");
    }

    #[test]
    fn an_unusable_host_is_rejected() {
        let error = validate_well_known(&map(&[("host", "not a host")]))
            .expect_err("the authority is unusable");
        assert_eq!(error.status(), 400, "{error}");
        validate_well_known(&map(&[("host", "api.vendor.com")])).expect("a usable authority");
        validate_well_known(&HeaderMap::new()).expect("no host is nothing to check");
    }
}
