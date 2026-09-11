//! Shared `$top`/`$skip` pagination-parameter parsing, used by both the
//! Upstream (`cpt-cf-oagw-dod-list-query-params`) and Route
//! (`cpt-cf-oagw-dod-route-list-query`) list endpoints.
//!
//! Both endpoints document the same contract -- `$top` defaults to 50 and
//! is capped at 100, `$skip` defaults to 0 -- but used to implement it
//! independently, and disagreed on malformed input: the upstreams handler
//! rejected it with `400` while the routes handler silently substituted the
//! default. This module is the single source of truth for both, and
//! standardises on reject-with-400 -- the safer failure mode, and already
//! the documented behaviour on the upstreams side (RF-005).

/// Documented default for `$top` when the parameter is absent.
pub(crate) const DEFAULT_TOP: usize = 50;
/// Documented maximum for `$top`; a larger requested value is silently
/// clamped down to this, not rejected.
pub(crate) const MAX_TOP: usize = 100;
/// Documented default for `$skip` when the parameter is absent.
pub(crate) const DEFAULT_SKIP: usize = 0;

/// Parsed, validated `$top`/`$skip` pagination parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageParams {
    pub top: usize,
    pub skip: usize,
}

/// Parse `$top` (default [`DEFAULT_TOP`], capped at [`MAX_TOP`]) and `$skip`
/// (default [`DEFAULT_SKIP`]) from raw query-string values. A present value
/// that fails to parse as a non-negative integer is rejected with an
/// `Err` naming the offending parameter and its raw value -- callers render
/// this as a `400`; a malformed pagination parameter is never silently
/// coerced to the default.
pub(crate) fn parse_page_params(
    top: Option<&str>,
    skip: Option<&str>,
) -> Result<PageParams, String> {
    let top = match top {
        None => DEFAULT_TOP,
        Some(s) => {
            let n: usize = s
                .parse()
                .map_err(|_| format!("`$top` `{s}` is not a valid non-negative integer"))?;
            n.min(MAX_TOP)
        }
    };
    let skip = match skip {
        None => DEFAULT_SKIP,
        Some(s) => s
            .parse()
            .map_err(|_| format!("`$skip` `{s}` is not a valid non-negative integer"))?,
    };
    Ok(PageParams { top, skip })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_both_absent() {
        let params = parse_page_params(None, None).unwrap();
        assert_eq!(params.top, DEFAULT_TOP);
        assert_eq!(params.skip, DEFAULT_SKIP);
    }

    #[test]
    fn valid_values_pass_through() {
        let params = parse_page_params(Some("30"), Some("5")).unwrap();
        assert_eq!(params.top, 30);
        assert_eq!(params.skip, 5);
    }

    #[test]
    fn top_over_max_is_clamped_not_rejected() {
        let params = parse_page_params(Some("500"), None).unwrap();
        assert_eq!(params.top, MAX_TOP);
    }

    #[test]
    fn malformed_top_is_rejected() {
        let err = parse_page_params(Some("notanumber"), None).unwrap_err();
        assert!(err.contains("$top"));
        assert!(err.contains("notanumber"));
    }

    #[test]
    fn malformed_skip_is_rejected() {
        let err = parse_page_params(None, Some("notanumber")).unwrap_err();
        assert!(err.contains("$skip"));
        assert!(err.contains("notanumber"));
    }

    #[test]
    fn negative_values_are_rejected_not_defaulted() {
        assert!(parse_page_params(Some("-1"), None).is_err());
        assert!(parse_page_params(None, Some("-1")).is_err());
    }
}
