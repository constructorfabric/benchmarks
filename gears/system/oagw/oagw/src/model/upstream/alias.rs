//! `cpt-cf-oagw-algo-derive-alias` and
//! `cpt-cf-oagw-algo-enforce-alias-update-immutability`: alias
//! classification/derivation at create time, and immutability enforcement
//! at update time.

use super::host::{HostClass, classify_host};
use super::{Endpoint, EndpointScheme};

/// A `400 ValidationError`-worthy alias-resolution failure, carrying an
/// occurrence-specific detail message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasError(pub String);

/// The result of classifying an endpoint pool's derivability
/// (`cpt-cf-oagw-algo-derive-alias`, `inst-alias-classify-hosts` onward).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasClass {
    /// Hostname-based endpoints from which an alias was auto-derived.
    Derivable(String),
    /// IP-based, or hostname-based with no PSL-validated common suffix: an
    /// explicit `alias` is required.
    NonDerivable,
}

fn is_standard_port(scheme: EndpointScheme, port: u16) -> bool {
    match scheme {
        EndpointScheme::Http | EndpointScheme::Ws => port == 80,
        EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc => {
            port == 443
        }
    }
}

fn with_port_suffix(base: &str, scheme: EndpointScheme, port: u16) -> String {
    if is_standard_port(scheme, port) {
        base.to_owned()
    } else {
        format!("{base}:{port}")
    }
}

/// `true` when `host`'s suffix is a *listed* public suffix (not merely the
/// PSL implicit wildcard rule every unrecognized TLD falls back to).
fn has_known_public_suffix(host: &str) -> bool {
    psl::suffix(host.as_bytes()).is_some_and(|s| s.is_known())
}

/// Longest common trailing-label suffix across all `hostnames`, validated
/// against the public suffix list via `psl::domain_str` -- a bare public
/// suffix (e.g. `co.uk`) yields `None`, matching
/// `inst-alias-psl-validate`/`inst-alias-no-suffix`.
fn common_registrable_suffix(hostnames: &[String]) -> Option<String> {
    let label_lists: Vec<Vec<&str>> = hostnames.iter().map(|h| h.rsplit('.').collect()).collect();
    let min_len = label_lists.iter().map(Vec::len).min()?;

    let mut common_rev: Vec<&str> = Vec::new();
    for i in 0..min_len {
        let label = label_lists[0][i];
        if label_lists.iter().all(|labels| labels[i] == label) {
            common_rev.push(label);
        } else {
            break;
        }
    }
    if common_rev.is_empty() {
        return None;
    }
    let common_suffix = common_rev.into_iter().rev().collect::<Vec<_>>().join(".");
    psl::domain_str(&common_suffix).map(str::to_owned)
}

/// Classify an endpoint pool's derivability and, when derivable, compute its
/// alias (`cpt-cf-oagw-algo-derive-alias`, steps `inst-alias-classify-hosts`
/// through `inst-alias-suffix-derive`). Assumes every `host` already passed
/// `cpt-cf-oagw-algo-validate-upstream-schema`'s host-format check.
#[must_use]
// @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-classify-hosts
pub fn classify_pool(endpoints: &[Endpoint]) -> AliasClass {
    let classes: Vec<HostClass> = endpoints
        .iter()
        .map(|e| classify_host(&e.host).unwrap_or(HostClass::Ip))
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-classify-hosts

    // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-branch
    if classes.iter().any(|c| matches!(c, HostClass::Ip)) {
        return AliasClass::NonDerivable;
    }
    // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-branch

    // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-hostname-branch
    let hostnames: Vec<String> = classes
        .into_iter()
        .map(|c| match c {
            HostClass::Hostname(h) => h,
            HostClass::Ip => unreachable!("IP hosts returned above"),
        })
        .collect();

    let scheme = endpoints[0].scheme;
    let port = endpoints[0].port;

    let mut distinct: Vec<&str> = Vec::new();
    for h in &hostnames {
        if !distinct.contains(&h.as_str()) {
            distinct.push(h.as_str());
        }
    }

    // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-single-hostname
    if distinct.len() == 1 {
        // A single hostname is only auto-derivable when its suffix is a
        // publicly *known* one (`psl::suffix(..).is_known()`) -- the same
        // registrability bar `inst-alias-psl-validate` applies to the
        // multi-hostname path. This resolves the acceptance criterion for
        // `{"scheme": "http", "host": "example-plaintext.internal", ...}`
        // with an explicit `alias`: `.internal` is not a listed public
        // suffix (PSL's implicit wildcard rule applies), so the endpoint is
        // non-derivable and the explicit alias is accepted as-is, while an
        // ordinary hostname like `api.openai.com` (a known `.com` suffix)
        // still auto-derives and still rejects a differing explicit alias.
        if has_known_public_suffix(distinct[0]) {
            // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-single-derive
            return AliasClass::Derivable(with_port_suffix(distinct[0], scheme, port));
            // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-single-derive
        }
        return AliasClass::NonDerivable;
    }
    // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-single-hostname
    // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-hostname-branch

    // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-multi-hostname
    // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-common-suffix
    // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-psl-validate
    match common_registrable_suffix(&hostnames) {
        // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-psl-validate
        // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-common-suffix
        // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-suffix-ok
        // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-suffix-derive
        Some(suffix) => AliasClass::Derivable(with_port_suffix(&suffix, scheme, port)),
        // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-suffix-derive
        // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-suffix-ok
        // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix
        None => AliasClass::NonDerivable,
        // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix
    }
    // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-multi-hostname
}

/// Normalize an alias literal (ASCII-lowercase, trailing dot stripped) and
/// validate it against `upstream.v1.schema.json`'s
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` pattern.
// @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-normalize
// @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix-normalize
pub fn normalize_alias_literal(raw: &str) -> Result<String, AliasError> {
    let stripped = raw.strip_suffix('.').unwrap_or(raw);
    let normalized = stripped.to_ascii_lowercase();
    if !is_valid_alias_pattern(&normalized) {
        return Err(AliasError(format!(
            "alias `{raw}` does not match the required pattern ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )));
    }
    Ok(normalized)
}
// @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix-normalize
// @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-normalize

fn is_valid_alias_pattern(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let is_edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !is_edge(bytes[0]) || !is_edge(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes
        .iter()
        .all(|&b| is_edge(b) || b == b'.' || b == b':' || b == b'-')
}

/// Resolve the effective alias for a `POST` (create) request
/// (`cpt-cf-oagw-algo-derive-alias`, `inst-alias-derived-vs-supplied`
/// through `inst-alias-return`).
// @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-derived-vs-supplied
// @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-return
pub fn resolve_alias(endpoints: &[Endpoint], supplied: Option<&str>) -> Result<String, AliasError> {
    match classify_pool(endpoints) {
        AliasClass::Derivable(derived) => {
            if let Some(raw) = supplied {
                let normalized = normalize_alias_literal(raw)?;
                // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-match
                if normalized != derived {
                    // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-mismatch
                    // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-mismatch-return
                    return Err(AliasError(
                        "explicit `alias` differs from the auto-derived value for \
                         hostname-based endpoints"
                            .to_owned(),
                    ));
                    // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-mismatch-return
                    // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-mismatch
                }
                // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-match
            }
            // A matching explicit alias, or no explicit alias at all, both
            // fall through here: idempotent no-op accept of the derived
            // value.
            // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-match-accept
            Ok(derived)
            // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-match-accept
        }
        // Both the IP-based and the no-common-suffix hostname cases collapse
        // into `NonDerivable` (see `classify_pool`): from `resolve_alias`'s
        // point of view they are the identical "explicit alias required"
        // branch, so `inst-alias-ip-missing`/`-supplied` and
        // `inst-alias-no-suffix-missing`/`-supplied` both realize here.
        // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-missing
        // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix-missing
        AliasClass::NonDerivable => match supplied {
            // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-missing-return
            // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix-missing-return
            None => Err(AliasError(
                "an explicit `alias` is required for IP-based or non-derivable endpoints"
                    .to_owned(),
            )),
            // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix-missing-return
            // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-missing-return
            // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix-missing
            // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-missing
            // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-supplied
            // @cpt-begin:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix-supplied
            Some(raw) => normalize_alias_literal(raw),
            // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-no-suffix-supplied
            // @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-ip-supplied
        },
    }
}
// @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-return
// @cpt-end:cpt-cf-oagw-algo-derive-alias:p1:inst-alias-derived-vs-supplied

/// Enforce alias immutability for a `PUT` (replace) request
/// (`cpt-cf-oagw-algo-enforce-alias-update-immutability`).
// @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-classify-old
// @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-classify-new
pub fn enforce_update(
    existing_alias: &str,
    existing_endpoints: &[Endpoint],
    proposed_endpoints: &[Endpoint],
    proposed_alias: Option<&str>,
) -> Result<String, AliasError> {
    let old_class = classify_pool(existing_endpoints);
    let new_class = classify_pool(proposed_endpoints);
    // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-classify-new
    // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-classify-old

    // Every arm below either returns the accepted alias (always equal to
    // `existing_alias`) or rejects -- realizing `inst-alias-update-return`.
    // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-return
    match new_class {
        // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-new-derivable
        // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute
        AliasClass::Derivable(recomputed) => {
            // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute
            if let Some(raw) = proposed_alias {
                let normalized = normalize_alias_literal(raw)?;
                // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-override-attempt
                if normalized != recomputed {
                    // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-override-attempt-return
                    return Err(AliasError(
                        "alias override is not allowed on update".to_owned(),
                    ));
                    // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-override-attempt-return
                }
                // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-override-attempt
            }
            // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute-match
            if recomputed == existing_alias {
                // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute-accept
                Ok(recomputed)
                // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute-accept
            } else {
                // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute-match
                // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute-mismatch
                // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute-return
                Err(AliasError(
                    "an endpoint change that would alter the derived alias is rejected; \
                     delete and re-create the upstream instead"
                        .to_owned(),
                ))
                // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute-return
                // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-recompute-mismatch
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-new-derivable
        // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-new-nonderivable
        AliasClass::NonDerivable => match old_class {
            // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-hostname-to-ip
            // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-hostname-to-ip-return
            AliasClass::Derivable(_) => Err(AliasError(
                "a hostname-to-IP endpoint transition is always rejected".to_owned(),
            )),
            // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-hostname-to-ip-return
            // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-hostname-to-ip
            // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-to-ip
            AliasClass::NonDerivable => match proposed_alias {
                // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-retain
                // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-retain-accept
                None => Ok(existing_alias.to_owned()),
                // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-retain-accept
                // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-retain
                Some(raw) => {
                    let normalized = normalize_alias_literal(raw)?;
                    if normalized == existing_alias {
                        Ok(existing_alias.to_owned())
                    } else {
                        // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-diff
                        // @cpt-begin:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-diff-return
                        Err(AliasError(
                            "a differing user-supplied alias is not accepted on update".to_owned(),
                        ))
                        // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-diff-return
                        // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-diff
                    }
                }
            },
            // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-ip-to-ip
        },
        // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-new-nonderivable
    }
    // @cpt-end:cpt-cf-oagw-algo-enforce-alias-update-immutability:p1:inst-alias-update-return
}
// @cpt-dod:cpt-cf-oagw-dod-alias-derivation:p1
// @cpt-dod:cpt-cf-oagw-dod-alias-immutability:p1

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_hostname_alias() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        assert_eq!(
            resolve_alias(&endpoints, None).unwrap(),
            "api.openai.com".to_owned()
        );
    }

    #[test]
    fn single_hostname_non_standard_port_appends_port() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", 8443)];
        assert_eq!(
            resolve_alias(&endpoints, None).unwrap(),
            "api.openai.com:8443".to_owned()
        );
    }

    #[test]
    fn matching_explicit_alias_is_an_idempotent_noop() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        assert_eq!(
            resolve_alias(&endpoints, Some("api.openai.com")).unwrap(),
            "api.openai.com".to_owned()
        );
    }

    #[test]
    fn differing_explicit_alias_for_hostname_endpoint_is_rejected() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        assert!(resolve_alias(&endpoints, Some("something-else")).is_err());
    }

    #[test]
    fn ip_endpoints_without_alias_are_rejected() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "10.0.1.1", 443)];
        assert!(resolve_alias(&endpoints, None).is_err());
    }

    #[test]
    fn ip_endpoints_with_explicit_alias_succeed() {
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "10.0.1.1", 443),
            endpoint(EndpointScheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(
            resolve_alias(&endpoints, Some("my-service")).unwrap(),
            "my-service".to_owned()
        );
    }

    #[test]
    fn multi_hostname_common_registrable_suffix_is_derived() {
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(
            resolve_alias(&endpoints, None).unwrap(),
            "vendor.com".to_owned()
        );
    }

    #[test]
    fn multi_hostname_bare_public_suffix_only_requires_explicit_alias() {
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "foo.co.uk", 443),
            endpoint(EndpointScheme::Https, "bar.co.uk", 443),
        ];
        assert!(resolve_alias(&endpoints, None).is_err());
        assert_eq!(
            resolve_alias(&endpoints, Some("shared-uk")).unwrap(),
            "shared-uk".to_owned()
        );
    }

    #[test]
    fn multi_hostname_no_common_suffix_requires_explicit_alias() {
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "us.foo.com", 443),
            endpoint(EndpointScheme::Https, "eu.bar.com", 443),
        ];
        assert!(resolve_alias(&endpoints, None).is_err());
    }

    #[test]
    fn resubmitting_unchanged_hostname_endpoints_on_update_succeeds() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        let resolved = enforce_update("api.openai.com", &endpoints, &endpoints, None).unwrap();
        assert_eq!(resolved, "api.openai.com");
    }

    #[test]
    fn hostname_endpoint_change_that_alters_alias_is_rejected() {
        let old = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        let new = vec![endpoint(EndpointScheme::Https, "api.other.com", 443)];
        assert!(enforce_update("api.openai.com", &old, &new, None).is_err());
    }

    #[test]
    fn hostname_to_ip_transition_is_always_rejected() {
        let old = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        let new = vec![endpoint(EndpointScheme::Https, "10.0.0.1", 443)];
        assert!(enforce_update("api.openai.com", &old, &new, Some("api.openai.com")).is_err());
    }

    #[test]
    fn ip_to_ip_retains_existing_alias_when_omitted() {
        let old = vec![endpoint(EndpointScheme::Https, "10.0.0.1", 443)];
        let new = vec![endpoint(EndpointScheme::Https, "10.0.0.2", 443)];
        assert_eq!(
            enforce_update("my-service", &old, &new, None).unwrap(),
            "my-service"
        );
    }

    #[test]
    fn ip_to_ip_with_differing_supplied_alias_is_rejected() {
        let old = vec![endpoint(EndpointScheme::Https, "10.0.0.1", 443)];
        let new = vec![endpoint(EndpointScheme::Https, "10.0.0.2", 443)];
        assert!(enforce_update("my-service", &old, &new, Some("different")).is_err());
    }

    #[test]
    fn alias_override_attempt_on_a_derivable_update_is_rejected() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        assert!(enforce_update("api.openai.com", &endpoints, &endpoints, Some("nope")).is_err());
    }
}
