//! Target path construction and route-prefix matching.

use crate::domain::dto::PathSuffixMode;
use crate::domain::error::DomainError;

/// Whether the request suffix begins with the route's path prefix.
///
/// The comparison is on whole path segments: `/v1` matches `/v1` and
/// `/v1/models` but not `/v1x`. A route path of `/` matches everything.
#[must_use]
pub fn suffix_matches(route_path: &str, suffix: &str) -> bool {
    let route = normalize(route_path);
    let suffix = normalize(suffix);
    if route == "/" {
        return true;
    }
    if suffix.is_empty() {
        return false;
    }
    if suffix == route {
        return true;
    }
    if !suffix.starts_with(&route) {
        return false;
    }
    if route.ends_with('/') {
        return true;
    }
    suffix.as_bytes().get(route.len()) == Some(&b'/')
}

/// The part of `suffix` that follows `route_path`, without a leading slash.
#[must_use]
pub fn suffix_remainder(route_path: &str, suffix: &str) -> String {
    let route = normalize(route_path);
    let suffix = normalize(suffix);
    if suffix == route {
        return String::new();
    }
    // A suffix that opens with the route's own path contributes only what
    // follows it; a bare one (`models` against `/v1`) is used whole.
    if suffix.starts_with(&route) && suffix.as_bytes().get(route.len()) == Some(&b'/') {
        return suffix[route.len()..].trim_start_matches('/').to_owned();
    }
    suffix.trim_start_matches('/').to_owned()
}

fn normalize(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    let with_slash = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    };
    let trimmed_end = with_slash.trim_end_matches('/');
    if trimmed_end.is_empty() {
        "/".to_owned()
    } else {
        trimmed_end.to_owned()
    }
}

/// Builds the path sent to the upstream.
///
/// In `append` mode the remainder of the suffix is joined onto the route's
/// path; in `disabled` mode any suffix is refused.
///
/// # Errors
/// Returns [`DomainError::Validation`] when a suffix is supplied in `disabled`
/// mode.
pub fn build_target_path(
    route_path: &str,
    suffix: &str,
    mode: PathSuffixMode,
) -> Result<String, DomainError> {
    let route = normalize(route_path);
    if suffix.is_empty() {
        return Ok(route);
    }
    match mode {
        PathSuffixMode::Disabled => Err(DomainError::Validation(
            "this route does not accept a path suffix".into(),
        )),
        PathSuffixMode::Append => {
            let remainder = suffix_remainder(&route, suffix);
            if remainder.is_empty() {
                Ok(route)
            } else if route == "/" {
                Ok(format!("/{remainder}"))
            } else {
                Ok(format!("{route}/{remainder}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_the_suffix_to_the_route_path() {
        assert_eq!(
            build_target_path("/v1", "/v1/models", PathSuffixMode::Append).expect("appends"),
            "/v1/models"
        );
        assert_eq!(
            build_target_path("/v1/chat", "/v1/chat/completions", PathSuffixMode::Append)
                .expect("appends"),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn no_suffix_returns_the_route_path() {
        assert_eq!(
            build_target_path("/v1", "", PathSuffixMode::Append).expect("unchanged"),
            "/v1"
        );
    }

    #[test]
    fn a_bare_suffix_appends_to_the_route() {
        assert_eq!(
            build_target_path("/v1", "models", PathSuffixMode::Append).expect("appends"),
            "/v1/models"
        );
    }

    #[test]
    fn disabled_mode_rejects_a_suffix() {
        let error = build_target_path("/v1", "/v1/models", PathSuffixMode::Disabled)
            .expect_err("rejected");
        assert_eq!(error.status(), 400);
        assert_eq!(
            build_target_path("/v1", "", PathSuffixMode::Disabled).expect("no suffix"),
            "/v1"
        );
    }

    #[test]
    fn prefix_matching_is_segment_aware() {
        assert!(suffix_matches("/v1", "/v1"));
        assert!(suffix_matches("/v1", "/v1/models"));
        assert!(!suffix_matches("/v1", "/v1x"));
        assert!(!suffix_matches("/v1", ""));
        assert!(!suffix_matches("/v1/chat", "/v1"));
        assert!(suffix_matches("/", "/anything/at/all"));
        assert!(suffix_matches("/", ""));
    }

    #[test]
    fn remainders_strip_the_matched_prefix() {
        assert_eq!(suffix_remainder("/v1", "/v1/models"), "models");
        assert_eq!(suffix_remainder("/v1", "/v1"), "");
        assert_eq!(suffix_remainder("/", "/v1/models"), "v1/models");
    }
}
