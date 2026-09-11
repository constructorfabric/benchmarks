//! Parse and Classify the Inbound Proxy Request
//! (`cpt-cf-oagw-algo-proxy-parse-request`).

use axum::http::{HeaderMap, Method};
use uuid::Uuid;

use crate::model::upstream::alias::{AliasError, normalize_alias_literal};

/// A parsed, hygiene-checked proxy request, per
/// `cpt-cf-oagw-algo-proxy-parse-request`'s documented output.
#[derive(Debug, Clone)]
pub(crate) struct ProxyRequestContext {
    pub tenant_id: Uuid,
    pub principal_id: Uuid,
    pub method: Method,
    pub alias: String,
    /// Raw path suffix, without a leading slash, empty when absent. Never
    /// percent-decoded: preserved exactly as received so an encoded `/`
    /// remains detectable (`inst-proxy-parse-suffix`).
    pub path_suffix: String,
    /// Inbound query pairs, decoded, in original order, duplicates kept.
    pub query_pairs: Vec<(String, String)>,
    pub headers: HeaderMap,
}

/// Which `cpt-cf-oagw-algo-proxy-map-error` bucket a [`ParseError`] belongs
/// to: bad alias grammar maps to `RouteError`, everything else (suffix
/// steering, header hygiene) maps to `ValidationError`. Both render an
/// identical `400` status and GTS `type`; the distinction exists only for
/// the catalog's documented title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseErrorKind {
    BadAlias,
    Validation,
}

/// A parse-time rejection: always a `400`-class failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub kind: ParseErrorKind,
    pub detail: String,
}

impl ParseError {
    fn validation(detail: impl Into<String>) -> Self {
        Self {
            kind: ParseErrorKind::Validation,
            detail: detail.into(),
        }
    }

    fn bad_alias(detail: impl Into<String>) -> Self {
        Self {
            kind: ParseErrorKind::BadAlias,
            detail: detail.into(),
        }
    }
}

impl From<AliasError> for ParseError {
    fn from(e: AliasError) -> Self {
        Self::bad_alias(e.0)
    }
}

const ALIAS_PATTERN_DETAIL: &str =
    "alias segment is empty or does not match the required alias grammar";

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16))?;
            let lo = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16))?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Split the post-prefix path at the first `/`: the alias segment, and the
/// raw remainder (`inst-proxy-parse-split`).
fn split_alias_and_suffix(rest: &str) -> (&str, &str) {
    match rest.split_once('/') {
        Some((alias, suffix)) => (alias, suffix),
        None => (rest, ""),
    }
}

/// Reject a path suffix that could steer the outbound path outside the
/// matched route: a `.`/`..` segment, a NUL byte, or an encoded `/`
/// (`inst-proxy-parse-suffix`).
fn validate_path_suffix(suffix: &str) -> Result<(), ParseError> {
    let lower = suffix.to_ascii_lowercase();
    if suffix.contains('\0') || lower.contains("%00") {
        return Err(ParseError::validation(
            "path suffix must not contain a NUL byte",
        ));
    }
    if lower.contains("%2f") {
        return Err(ParseError::validation(
            "path suffix must not contain an encoded '/'",
        ));
    }
    if suffix
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(ParseError::validation(
            "path suffix must not contain a '.' or '..' segment",
        ));
    }
    Ok(())
}

/// Reject a header name or value carrying CR, LF or NUL
/// (`inst-proxy-parse-header-hygiene`).
fn header_hygiene(headers: &HeaderMap) -> Result<(), ParseError> {
    let has_control = |bytes: &[u8]| bytes.iter().any(|b| matches!(b, b'\r' | b'\n' | 0));
    for (name, value) in headers {
        if has_control(name.as_str().as_bytes()) || has_control(value.as_bytes()) {
            return Err(ParseError::validation(
                "header name or value contains CR, LF or NUL",
            ));
        }
    }
    Ok(())
}

/// Parse an inbound query string into ordered name/value pairs, preserving
/// duplicates (`inst-proxy-parse-query`).
fn parse_query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// `cpt-cf-oagw-algo-proxy-parse-request`: split, normalize, validate and
/// classify the inbound `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
/// request.
// @cpt-algo:cpt-cf-oagw-algo-proxy-parse-request:p2
// @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-tenant
pub(crate) fn parse_request(
    tenant_id: Uuid,
    principal_id: Uuid,
    method: Method,
    rest: &str,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Result<ProxyRequestContext, ParseError> {
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-tenant
    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-split
    let (raw_alias, raw_suffix) = split_alias_and_suffix(rest);
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-split

    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-normalize-alias
    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-if-bad-alias
    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-return-bad-alias
    let decoded_alias =
        percent_decode(raw_alias).ok_or_else(|| ParseError::bad_alias(ALIAS_PATTERN_DETAIL))?;
    let alias = normalize_alias_literal(&decoded_alias)?;
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-return-bad-alias
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-if-bad-alias
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-normalize-alias

    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-suffix
    validate_path_suffix(raw_suffix)?;
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-suffix

    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-query
    let query_pairs = parse_query_pairs(query);
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-query

    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-foreach-header
    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-header-hygiene
    header_hygiene(headers)?;
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-header-hygiene
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-foreach-header

    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-classify
    // @cpt-begin:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-return
    Ok(ProxyRequestContext {
        tenant_id,
        principal_id,
        method,
        alias,
        path_suffix: raw_suffix.to_owned(),
        query_pairs,
        headers: headers.clone(),
    })
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-return
    // @cpt-end:cpt-cf-oagw-algo-proxy-parse-request:p2:inst-proxy-parse-classify
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tid() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn splits_alias_and_multi_segment_suffix() {
        let ctx = parse_request(
            tid(),
            tid(),
            Method::GET,
            "MyAlias/a/b/c",
            None,
            &HeaderMap::new(),
        )
        .unwrap();
        assert_eq!(ctx.alias, "myalias");
        assert_eq!(ctx.path_suffix, "a/b/c");
    }

    #[test]
    fn empty_alias_is_rejected() {
        assert!(parse_request(tid(), tid(), Method::GET, "", None, &HeaderMap::new()).is_err());
    }

    #[test]
    fn alias_with_illegal_characters_is_rejected() {
        assert!(
            parse_request(
                tid(),
                tid(),
                Method::GET,
                "bad alias!",
                None,
                &HeaderMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn dot_segment_in_suffix_is_rejected() {
        assert!(
            parse_request(
                tid(),
                tid(),
                Method::GET,
                "alias/../etc",
                None,
                &HeaderMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn encoded_slash_in_suffix_is_rejected() {
        assert!(
            parse_request(
                tid(),
                tid(),
                Method::GET,
                "alias/a%2Fb",
                None,
                &HeaderMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn query_pairs_preserve_order_and_duplicates() {
        let ctx = parse_request(
            tid(),
            tid(),
            Method::GET,
            "alias",
            Some("a=1&b=2&a=3"),
            &HeaderMap::new(),
        )
        .unwrap();
        assert_eq!(
            ctx.query_pairs,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned()),
                ("a".to_owned(), "3".to_owned()),
            ]
        );
    }

    #[test]
    fn well_formed_headers_pass_hygiene() {
        // `axum::http::HeaderName`/`HeaderValue` construction itself already
        // forbids raw CR, LF and NUL bytes at every safe public constructor
        // (a request carrying one is rejected by hyper's own HTTP/1 parser
        // before this handler ever sees it), so `header_hygiene`'s CR/LF/NUL
        // check is defence-in-depth that cannot be driven adversarially
        // through the public `HeaderMap` API in a unit test; this test
        // confirms the positive (accept) path instead.
        let mut headers = HeaderMap::new();
        headers.insert("x-test", axum::http::HeaderValue::from_static("a-b"));
        assert!(header_hygiene(&headers).is_ok());
    }
}
