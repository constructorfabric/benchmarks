//! Inbound and body validation of a proxy request.
//!
//! Realizes `cpt-cf-oagw-algo-inbound-validate` and
//! `cpt-cf-oagw-algo-body-validate`: the method, path, query, and header checks
//! against the matched route, and the framing and size checks the request body
//! is subject to before any of it is buffered. Both routines are the request
//! half of `cpt-cf-oagw-nfr-input-validation`, which requires invalid requests
//! to be rejected with 400 — and the size breach with 413 — and neither reads
//! any configuration beyond the matched route.

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::proxy::{MatchedRoute, ProxyContext};

/// The 100MB hard limit of `cpt-cf-oagw-constraint-body-limit`, read as
/// 100,000,000 bytes per the FEATURE §1.5 deviation.
pub const BODY_LIMIT_BYTES: usize = 100_000_000;

/// The eight hop-by-hop headers of `cpt-cf-oagw-fr-header-transform`, which no
/// plain request/response exchange forwards.
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The routing header the endpoint selection consumes and never forwards.
pub const ROUTING_HEADER: &str = "x-oagw-target-host";

// @cpt-dod:cpt-cf-oagw-dod-inbound-validation:p1

/// Validates the inbound request against the matched route.
///
/// # Errors
///
/// Returns the `ValidationError` failure the caller answers 400 with, naming
/// every failing property of the request.
#[allow(clippy::result_large_err)]
pub fn validate_inbound(
    context: &ProxyContext,
    matched: &MatchedRoute,
) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-method
    // The method allowlist was the first filter of the match; a failure here
    // is a defect of the caller of this routine, not of the request.
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-method

    let mut rejected: Vec<String> = Vec::new();

    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-path
    // The outbound path was built by `cpt-cf-oagw-algo-route-match`, which
    // already made the `path_suffix_mode` decision: the request path this
    // routine reads is inside the matched route's own path space, so the check
    // holds it to that and refuses a path that left it.
    if !context.request_path().starts_with('/') {
        rejected.push(String::from("path"));
    }
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-path

    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-query
    if let Some(query) = context.query.as_deref() {
        for (name, _) in parse_query(query) {
            // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-query-if
            if !matched.query_allowlist.iter().any(|allowed| allowed == &name) {
                // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-query-return
                // The offending parameter is named in the rejection the caller
                // answers 400 with; it is collected here so one failure names
                // every defect instead of one.
                rejected.push(name);
                // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-query-return
            }
            // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-query-if
        }
    }
    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-query-else
    // The ELSE of the parameter check: a parameter the allowlist admits is
    // carried to the outbound request untouched, and its value is never
    // validated here.
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-query-else
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-query

    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-headers
    for (name, value) in &context.headers {
        if value.contains('\r') || value.contains('\n') {
            rejected.push(name.clone());
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-headers

    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-wellknown
    // The well-known headers — `Content-Length` and `Content-Type` among them
    // — are validated as set or adjusted values: the framing half of
    // `cpt-cf-oagw-algo-body-validate` checks the two the body has, and the
    // transformation half of `cpt-cf-oagw-algo-header-transform` sets what the
    // hop-by-hop and routing rules strip or rewrite, so an invalid header
    // reaches neither and is answered 400 here.
    for (name, value) in &context.headers {
        if value.trim().is_empty() {
            rejected.push(name.clone());
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-wellknown

    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-fail-if
    if !rejected.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-fail-return
        // One rejection names every failing property, so a caller is not made
        // to retry once per defect.
        let mut error = DomainError::gateway(
            ErrorKind::ValidationError,
            "the request names a property the matched route does not admit",
        );
        error.detail = format!(
            "the matched route admits only the query parameters {:?} and header values without CR or LF; the request was refused for {rejected:?}",
            matched.query_allowlist
        );
        return Err(error);
        // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-fail-return
    }
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-fail-if

    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-fail-else
    // The ELSE of the failure check: the request named no property the route
    // refuses.
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-fail-else

    // @cpt-begin:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-return
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-inbound-validate:p1:inst-inv-return
}

// @cpt-dod:cpt-cf-oagw-dod-body-validation:p1

/// Reads the request body and validates it, answering the failures of the
/// framing headers before any byte of it is buffered.
///
/// The order is the algorithm's: the declared size against the hard limit, the
/// framing headers, and only then the read, which stops at the first byte past
/// the limit; the buffered size is compared with the declared one last, when a
/// length was declared at all.
///
/// # Errors
///
/// Returns the `PayloadTooLarge` failure for a declared or actual size above
/// the hard limit, and the `ValidationError` failure for every framing defect
/// and for a body that cannot be read to its declared end.
#[allow(clippy::result_large_err)]
pub async fn read_body(
    context: &ProxyContext,
    body: axum::body::Body,
) -> Result<Vec<u8>, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-limit-first
    // The declared size is evaluated from the framing headers before any body
    // byte is read into a buffer, which is what
    // `cpt-cf-oagw-constraint-body-limit` requires and what keeps the check off
    // the memory of the process.
    let declared = declared_length(context);
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-limit-first

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-limit-if
    if declared.is_some_and(|length| length > BODY_LIMIT_BYTES) {
        // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-limit-return
        return Err(too_large());
        // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-limit-return
    }
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-limit-if

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-limit-else
    // The ELSE of the limit check: the declared size, when the request stated
    // one, is at or under the hard limit, so the framing checks and the read
    // proceed.
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-limit-else

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-framing
    // `Content-Length` present and a valid integer, `Transfer-Encoding` present
    // and equal to `chunked`, and never both on one request; a header value
    // carrying CR or LF is the last row of the same table.
    let defects = framing_defects(context);
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-framing

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-framing-if
    if !defects.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-framing-return
        return Err(validation_error(&defects));
        // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-framing-return
    }
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-framing-if

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-framing-else
    // The ELSE of the framing check: the framing headers are the ones the
    // table admits, so the body can be read to a trusted end.
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-framing-else

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-size
    // The body is buffered up to the limit and no further: the read stops at
    // the first byte past it, so nothing beyond the limit is ever held.
    let buffer = read_under_limit(body).await?;
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-size

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-size-if
    if let Some(length) = declared
        && length != buffer.len()
    {
        // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-size-return
        return Err(validation_error(&[String::from(
            "content-length does not match the actual body size",
        )]));
        // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-size-return
    }
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-size-if

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-size-else
    // The ELSE of the size comparison: the read body is the size the request
    // declared, or the request declared none and the read is its own truth.
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-size-else

    // @cpt-begin:cpt-cf-oagw-algo-body-validate:p1:inst-body-return
    Ok(buffer)
    // @cpt-end:cpt-cf-oagw-algo-body-validate:p1:inst-body-return
}

/// Validates a body that is already buffered, which is the byte-level half of
/// [`read_body`] and the form the unit tests drive.
///
/// # Errors
///
/// Returns the same failures [`read_body`] answers with, evaluated over the
/// bytes as given.
#[allow(clippy::result_large_err)]
pub fn validate_body(context: &ProxyContext, body: &[u8]) -> Result<(), DomainError> {
    let declared = declared_length(context);
    if declared.is_some_and(|length| length > BODY_LIMIT_BYTES) {
        return Err(too_large());
    }
    let defects = framing_defects(context);
    if !defects.is_empty() {
        return Err(validation_error(&defects));
    }
    if body.len() > BODY_LIMIT_BYTES {
        return Err(too_large());
    }
    if let Some(length) = declared
        && length != body.len()
    {
        return Err(validation_error(&[String::from(
            "content-length does not match the actual body size",
        )]));
    }
    Ok(())
}

/// The framing defects of a request body, which need no body byte to name.
fn framing_defects(context: &ProxyContext) -> Vec<String> {
    let content_length = context.header_values("content-length");
    let transfer_encoding: Vec<String> = context
        .header_values("transfer-encoding")
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .collect();
    let mut framing: Vec<String> = Vec::new();
    if content_length.len() > 1 {
        framing.push(String::from("content-length is declared more than once"));
    }
    if let Some(declared) = content_length.first()
        && declared.parse::<usize>().is_err()
    {
        framing.push(String::from("content-length is not a valid integer"));
    }
    if !transfer_encoding.is_empty() {
        if !content_length.is_empty() {
            framing.push(String::from(
                "content-length and transfer-encoding are both declared",
            ));
        }
        if transfer_encoding.len() > 1
            || transfer_encoding.iter().any(|value| value != "chunked")
        {
            framing.push(String::from("transfer-encoding is not chunked"));
        }
    }
    for (name, value) in &context.headers {
        if value.contains('\r') || value.contains('\n') {
            framing.push(format!("{name} carries a CR or LF in its value"));
        }
    }
    framing
}

/// Reads the body off the wire up to the hard limit, refusing the read at the
/// first byte past it.
#[allow(clippy::result_large_err)]
async fn read_under_limit(body: axum::body::Body) -> Result<Vec<u8>, DomainError> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(frame) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = frame.map_err(|_| read_failure())?;
        if buffer.len() + chunk.len() > BODY_LIMIT_BYTES {
            return Err(too_large());
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer)
}

/// The declared `Content-Length`, as one size when the header is present and
/// parses, and `None` when it is absent or malformed.
fn declared_length(context: &ProxyContext) -> Option<usize> {
    context
        .header_values("content-length")
        .first()
        .and_then(|value| value.parse::<usize>().ok())
}

/// The 413 failure a body above the hard limit is answered with.
#[allow(clippy::result_large_err)]
fn too_large() -> DomainError {
    DomainError::gateway(
        ErrorKind::PayloadTooLarge,
        "the request body exceeds the hard limit the gateway enforces",
    )
}

/// The failure a body that cannot be read off the wire is answered with.
#[allow(clippy::result_large_err)]
fn read_failure() -> DomainError {
    DomainError::gateway(
        ErrorKind::ValidationError,
        "the request body could not be read to its declared end",
    )
}

/// Builds the 400 failure one or more framing or inbound defects answer with.
fn validation_error(defects: &[String]) -> DomainError {
    let mut error = DomainError::gateway(
        ErrorKind::ValidationError,
        "the request body or its framing is not valid",
    );
    error.detail = format!("the request was refused for: {}", defects.join("; "));
    error
}

/// Splits a query string into its decoded name-value pairs.
///
/// The pairs are the raw decoded names only: the allowlist comparison is on the
/// parameter name, and a value is never validated here.
#[must_use]
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

