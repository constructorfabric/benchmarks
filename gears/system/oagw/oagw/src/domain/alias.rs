//! Alias derivation, validation and shadowing rules
//! ([DESIGN.md](../../docs/DESIGN.md) `cpt-cf-oagw-design-domain-model`,
//! "Alias Enforcement Rules" / "Alias Update Behavior").
//!
//! An alias is the routing key of the proxy path (`/oagw/v1/proxy/{alias}`), so
//! it is **enforced by endpoint type** rather than chosen freely:
//!
//! - hostname endpoints always derive the alias (a differing user-provided
//!   alias is rejected; the exact derived value is tolerated for idempotency);
//! - IP literals, heterogeneous hostname pools and pools whose only common
//!   suffix is a bare public suffix are **non-derivable** and require an
//!   explicit alias;
//! - the alias is immutable once set — every transition that would change it is
//!   rejected, including the derivable → non-derivable direction.
//!
//! Derivation validates hostnames per RFC 1123, normalizes to ASCII lowercase
//! with trailing dots stripped, and validates multi-host common suffixes
//! against the public suffix list (`psl`): a shared suffix must be a
//! registrable domain, i.e. at least two labels and not a bare public suffix.
//! Standard ports (HTTP `80`, every other scheme `443`) are omitted from a
//! derived alias; a non-standard port yields `host:port`.

use std::net::{Ipv4Addr, Ipv6Addr};

use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{Alias, EndpointScheme, UpstreamEndpoint};

/// Standard port of a plaintext HTTP endpoint (omitted from a derived alias).
const STANDARD_HTTP_PORT: u16 = 80;
/// Standard port of every TLS / WebTransport / gRPC endpoint (omitted from a
/// derived alias).
const STANDARD_TLS_PORT: u16 = 443;
/// Maximum length of a hostname (RFC 1123 §2.1).
const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of a single hostname label (RFC 1123 §2.1).
const MAX_HOSTNAME_LABEL_LEN: usize = 63;

/// The resolved alias of an upstream, and whether the endpoints determined it.
///
/// `derived` records the derivation decision so a later `PUT` can tell the
/// "derivable → non-derivable" transition (always rejected) apart from a
/// "non-derivable → non-derivable" one (the existing alias is retained).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAlias {
    /// Alias the upstream is reachable under.
    pub alias: Alias,
    /// `true` when the alias came from endpoint derivation instead of the
    /// request payload.
    pub derived: bool,
}

/// Normalizes an alias or hostname: ASCII lowercase, trailing dots stripped.
///
/// Resolution is case-insensitive (`Api.OpenAI.COM` resolves to an upstream
/// stored as `api.openai.com`), so normalization happens before an alias is
/// validated or compared.
#[must_use]
pub fn normalize(value: &str) -> String {
    value.trim_end_matches('.').to_ascii_lowercase()
}

/// `true` when `host` is an IPv4 or IPv6 address literal.
///
/// IP-based endpoints are never derivable: an explicit alias is required
/// (`docs/DESIGN.md` "Alias Enforcement Rules").
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let host = normalize(host);
    if host.contains(':') {
        return host.parse::<Ipv6Addr>().is_ok();
    }
    host.parse::<Ipv4Addr>().is_ok()
}

/// `true` when `host` is a valid RFC 1123 hostname (or an IP literal).
///
/// A trailing dot (FQDN notation) is tolerated and stripped; labels contain
/// only ASCII alphanumerics and hyphens, cannot start or end with a hyphen, are
/// 1-63 characters long and the whole name is at most 253 characters.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    let host = normalize(host);
    if host.is_empty() || host.len() > MAX_HOSTNAME_LEN {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= MAX_HOSTNAME_LABEL_LEN
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// `true` when `port` is the standard port of `scheme` and is therefore omitted
/// from a derived alias (HTTP uses 80, every other scheme 443).
#[must_use]
pub fn is_standard_port(scheme: EndpointScheme, port: u16) -> bool {
    port == match scheme {
        EndpointScheme::Http => STANDARD_HTTP_PORT,
        EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc => {
            STANDARD_TLS_PORT
        }
    }
}

/// `true` when `host` is a name the public suffix list can validate, i.e. a
/// registrable domain or one of its subdomains.
///
/// This is what makes a bare public suffix (`co.uk`) and a single label
/// (`localhost`) non-derivable: neither is a registrable domain.
#[must_use]
fn is_registrable_name(host: &str) -> bool {
    psl::domain_str(host).is_some()
}

/// Longest common label suffix of `hosts`, or `None` when they share none.
///
/// Every host must be a registrable name (see [`is_registrable_name`]); a pool
/// that mixes unrelated registrable domains has no derivable alias.
#[must_use]
fn common_suffix(hosts: &[String]) -> Option<String> {
    let first = hosts.first()?;
    let labels: Vec<&str> = first.split('.').collect();
    let mut shared = 0_usize;
    for position in 0..labels.len() {
        let label = labels[labels.len() - 1 - position];
        if hosts
            .iter()
            .all(|host| host.split('.').rev().nth(position) == Some(label))
        {
            shared += 1;
        } else {
            break;
        }
    }
    (shared > 0)
        .then(|| labels[labels.len() - shared..].join("."))
        .filter(|suffix| is_registrable_name(suffix))
}

/// The `:port` suffix of a derived alias, empty for a standard port.
///
/// Endpoints of a pool must share one port, so the first endpoint decides.
#[must_use]
fn port_suffix(endpoints: &[UpstreamEndpoint]) -> String {
    let Some(endpoint) = endpoints.first() else {
        return String::new();
    };
    if is_standard_port(endpoint.scheme, endpoint.port) {
        String::new()
    } else {
        format!(":{}", endpoint.port)
    }
}

/// The endpoint-derived alias of `endpoints`, when the endpoints determine one.
///
/// Derivation follows `docs/PRD.md` "Alias Examples":
///
/// - a single hostname derives the hostname (`api.openai.com:443` →
///   `api.openai.com`);
/// - a hostname pool derives its longest common suffix when that suffix is a
///   registrable domain (`us.vendor.com` + `eu.vendor.com` → `vendor.com`);
/// - IP literals, pools without a registrable common suffix and pools whose
///   only common suffix is a bare public suffix (`co.uk`) are non-derivable;
/// - a non-standard port is preserved as `host:port`
///   (`api.openai.com:8443` → `api.openai.com:8443`).
///
/// Endpoints that do not share one port form no valid pool; such an upstream is
/// rejected by the endpoint-pool validation before derivation is consulted, so
/// it is reported here as non-derivable.
#[must_use]
pub fn derive(endpoints: &[UpstreamEndpoint]) -> Option<Alias> {
    let hosts: Vec<String> = endpoints
        .iter()
        .map(|endpoint| normalize(&endpoint.host))
        .collect();
    if hosts.is_empty() {
        return None;
    }
    // IP literals are never derivable, whatever the public suffix list happens
    // to make of a dotted-decimal string.
    if hosts.iter().any(|host| is_ip_literal(host)) {
        return None;
    }
    if hosts.iter().any(|host| !is_registrable_name(host)) {
        return None;
    }
    if endpoints
        .iter()
        .any(|endpoint| endpoint.port != endpoints[0].port)
    {
        return None;
    }

    let base = match hosts.as_slice() {
        [host] => host.clone(),
        hosts => common_suffix(hosts)?,
    };
    let candidate = format!("{base}{}", port_suffix(endpoints));
    Alias::try_new(candidate).ok()
}

/// Resolves the alias of a **new** upstream from the request payload and its
/// endpoints.
///
/// Derivable endpoints always win: an omitted alias is derived, and a
/// user-provided alias is only tolerated when it equals the derived value
/// (idempotent no-op). Non-derivable endpoints require an explicit alias.
///
/// # Errors
/// [`OagwError::Validation`] when a provided alias differs from the derived
/// one, or when the endpoints are non-derivable and no alias was provided.
pub fn resolve_new(
    provided: Option<Alias>,
    endpoints: &[UpstreamEndpoint],
) -> Result<ResolvedAlias, OagwError> {
    match derive(endpoints) {
        Some(derived) => match provided {
            None => Ok(ResolvedAlias {
                alias: derived,
                derived: true,
            }),
            Some(provided) if provided == derived => Ok(ResolvedAlias {
                alias: derived,
                derived: true,
            }),
            Some(provided) => Err(OagwError::Validation {
                message: format!(
                    "alias '{}' does not match the endpoint-derived alias '{derived}': \
                     hostname-based endpoints always use the derived alias",
                    provided.as_str()
                ),
            }),
        },
        None => provided.map_or_else(
            || {
                Err(OagwError::Validation {
                    message: "an explicit alias is required: the upstream endpoints do not \
                              determine an alias (IP literal, no registrable common suffix, or \
                              a bare public suffix)"
                        .to_owned(),
                })
            },
            |alias| {
                Ok(ResolvedAlias {
                    alias,
                    derived: false,
                })
            },
        ),
    }
}

/// Resolves the alias of an existing upstream whose endpoints are being
/// replaced.
///
/// The alias is immutable once set: it is the routing key of the proxy URL, so
/// any endpoint change that would alter the derived alias is rejected and the
/// operator must delete and re-create the upstream. The derivable →
/// non-derivable transition is rejected even when an explicit alias is
/// supplied, and an explicit alias is retained unchanged across endpoint edits.
///
/// # Errors
/// [`OagwError::Validation`] when the replacement would change the alias.
pub fn resolve_replacement(
    existing: &Alias,
    existing_derived: bool,
    provided: Option<Alias>,
    endpoints: &[UpstreamEndpoint],
) -> Result<Alias, OagwError> {
    if let Some(provided) = provided
        && provided != *existing
    {
        return Err(OagwError::Validation {
            message: format!(
                "alias '{}' does not match the existing alias '{existing}': the alias of an \
                 existing upstream is immutable",
                provided.as_str()
            ),
        });
    }

    match derive(endpoints) {
        Some(derived) if derived == *existing => Ok(existing.clone()),
        Some(derived) => Err(OagwError::Validation {
            message: format!(
                "the endpoint change would change the alias from '{existing}' to \
                 '{derived}': delete and re-create the upstream instead"
            ),
        }),
        None if existing_derived => Err(OagwError::Validation {
            message: format!(
                "the endpoints stopped determining an alias (IP literal, no registrable common \
                 suffix or a bare public suffix), which would change the derived alias \
                 '{existing}': delete and re-create the upstream instead"
            ),
        }),
        None => Ok(existing.clone()),
    }
}

/// Picks the closest match along a tenant chain ordered from the caller's
/// tenant to the root.
///
/// This is the shadowing rule of `docs/DESIGN.md`: an exact alias in the
/// caller's tenant wins over an inherited one, and a descendant tenant's
/// resource shadows its ancestor's for descendants.
#[must_use]
pub fn closest_match<T>(chain: &[Uuid], find: impl Fn(Uuid) -> Option<T>) -> Option<T> {
    chain.iter().find_map(|tenant| find(*tenant))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use uuid::Uuid;

    use super::{
        closest_match, derive, is_ip_literal, is_standard_port, is_valid_hostname, normalize,
        resolve_new, resolve_replacement,
    };
    use crate::domain::model::{Alias, EndpointScheme, UpstreamEndpoint};

    fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> UpstreamEndpoint {
        UpstreamEndpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn normalize_lowercases_and_strips_trailing_dots() {
        assert_eq!(normalize("Api.OpenAI.COM"), "api.openai.com");
        assert_eq!(normalize("api.openai.com."), "api.openai.com");
        assert_eq!(normalize("API"), "api");
        assert_eq!(normalize("Vendor.COM."), "vendor.com");
    }

    #[test]
    fn ip_literals_are_detected_by_family() {
        assert!(is_ip_literal("10.0.1.1"));
        assert!(is_ip_literal(
            "10.0.1.1:443".split(':').next().expect("host")
        ));
        assert!(is_ip_literal("2001:db8::1"));
        assert!(!is_ip_literal("api.openai.com"));
        assert!(!is_ip_literal("999.1.1.1"));
    }

    #[test]
    fn hostnames_follow_rfc_1123() {
        assert!(is_valid_hostname("api.openai.com"));
        assert!(is_valid_hostname("api.openai.com."));
        assert!(is_valid_hostname("Api.OpenAI.Com"));
        assert!(is_valid_hostname("a-b.c-d.ef"));
        assert!(!is_valid_hostname("-api.openai.com"));
        assert!(!is_valid_hostname("api-.openai.com"));
        assert!(!is_valid_hostname("api..openai.com"));
        assert!(!is_valid_hostname(""));
        assert!(!is_valid_hostname(&"a".repeat(254)));
        assert!(!is_valid_hostname(&format!("{}.com", "a".repeat(64))));
    }

    #[test]
    fn standard_ports_are_omitted_from_derived_aliases() {
        assert!(is_standard_port(EndpointScheme::Http, 80));
        assert!(is_standard_port(EndpointScheme::Https, 443));
        assert!(is_standard_port(EndpointScheme::Grpc, 443));
        assert!(!is_standard_port(EndpointScheme::Https, 8443));
        assert!(!is_standard_port(EndpointScheme::Http, 8080));
    }

    #[test]
    fn single_hostname_derives_its_normalized_host() {
        let endpoints = [endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        let derived = derive(&endpoints).expect("derivable");
        assert_eq!(derived.as_str(), "api.openai.com");
    }

    #[test]
    fn non_standard_port_is_appended_as_host_port() {
        let endpoints = [endpoint(EndpointScheme::Https, "api.openai.com", 8443)];
        assert_eq!(
            derive(&endpoints).expect("derivable").as_str(),
            "api.openai.com:8443"
        );

        let endpoints = [endpoint(EndpointScheme::Http, "api.openai.com", 8080)];
        assert_eq!(
            derive(&endpoints).expect("derivable").as_str(),
            "api.openai.com:8080"
        );

        // Port 80 is standard for plaintext HTTP and is omitted.
        let endpoints = [endpoint(EndpointScheme::Http, "api.openai.com", 80)];
        assert_eq!(
            derive(&endpoints).expect("derivable").as_str(),
            "api.openai.com"
        );
    }

    #[test]
    fn hostname_pools_derive_their_registrable_common_suffix() {
        let endpoints = [
            endpoint(EndpointScheme::Https, "us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(
            derive(&endpoints).expect("derivable").as_str(),
            "vendor.com"
        );

        // Deeper hostnames still collapse onto the registrable domain.
        let endpoints = [
            endpoint(EndpointScheme::Https, "x.us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "y.eu.vendor.com", 443),
        ];
        assert_eq!(
            derive(&endpoints).expect("derivable").as_str(),
            "vendor.com"
        );

        // Non-standard ports are preserved in the common suffix.
        let endpoints = [
            endpoint(EndpointScheme::Https, "us.vendor.com", 8443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(
            derive(&endpoints).expect("derivable").as_str(),
            "vendor.com:8443"
        );
    }

    #[test]
    fn non_derivable_pools_require_an_explicit_alias() {
        // IP literals.
        let endpoints = [endpoint(EndpointScheme::Https, "10.0.1.1", 443)];
        assert!(derive(&endpoints).is_none());

        // Unrelated hostnames share no registrable suffix.
        let endpoints = [
            endpoint(EndpointScheme::Https, "us.foo.com", 443),
            endpoint(EndpointScheme::Https, "eu.bar.com", 443),
        ];
        assert!(derive(&endpoints).is_none());

        // The only common suffix is a bare public suffix.
        let endpoints = [
            endpoint(EndpointScheme::Https, "foo.co.uk", 443),
            endpoint(EndpointScheme::Https, "bar.co.uk", 443),
        ];
        assert!(derive(&endpoints).is_none());

        // A single label is not a registrable domain.
        let endpoints = [endpoint(EndpointScheme::Https, "localhost", 443)];
        assert!(derive(&endpoints).is_none());

        // A mixed port pool is rejected by the endpoint-pool validation and
        // therefore reports no derivation.
        let endpoints = [
            endpoint(EndpointScheme::Https, "us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 8443),
        ];
        assert!(derive(&endpoints).is_none());
    }

    #[test]
    fn new_upstreams_take_the_derived_alias() {
        let endpoints = [endpoint(EndpointScheme::Https, "api.openai.com", 443)];

        let resolved = resolve_new(None, &endpoints).expect("derived");
        assert_eq!(resolved.alias.as_str(), "api.openai.com");
        assert!(resolved.derived);

        // The exact derived value is tolerated for idempotency.
        let resolved = resolve_new(Some(Alias::try_new("api.openai.com").unwrap()), &endpoints)
            .expect("idempotent");
        assert_eq!(resolved.alias.as_str(), "api.openai.com");

        // A differing alias is rejected.
        let error =
            resolve_new(Some(Alias::try_new("openai").unwrap()), &endpoints).expect_err("rejected");
        assert_eq!(error.http_status(), 400);
        assert!(error.to_string().contains("api.openai.com"), "{error}");
    }

    #[test]
    fn ip_upstreams_require_an_explicit_alias() {
        let endpoints = [endpoint(EndpointScheme::Https, "10.0.1.1", 443)];

        let error = resolve_new(None, &endpoints).expect_err("alias required");
        assert_eq!(error.http_status(), 400);

        let resolved = resolve_new(
            Some(Alias::try_new("my-internal-service").unwrap()),
            &endpoints,
        )
        .expect("explicit alias");
        assert_eq!(resolved.alias.as_str(), "my-internal-service");
        assert!(!resolved.derived);
    }

    #[test]
    fn replacements_never_change_a_derived_alias() {
        let existing = Alias::try_new("api.openai.com").unwrap();
        let same = [endpoint(EndpointScheme::Https, "api.openai.com", 443)];

        // Alias unchanged: allowed, with or without an explicit alias.
        assert_eq!(
            resolve_replacement(&existing, true, None, &same)
                .expect("allowed")
                .as_str(),
            "api.openai.com"
        );
        assert_eq!(
            resolve_replacement(&existing, true, Some(existing.clone()), &same)
                .expect("allowed")
                .as_str(),
            "api.openai.com"
        );

        // A different derived alias is rejected.
        let moved = [endpoint(EndpointScheme::Https, "api2.openai.com", 443)];
        let error = resolve_replacement(&existing, true, None, &moved).expect_err("rejected");
        assert_eq!(error.http_status(), 400);

        // Derivable -> non-derivable is rejected even with an explicit alias.
        let ip = [endpoint(EndpointScheme::Https, "10.0.1.1", 443)];
        let error = resolve_replacement(&existing, true, Some(existing.clone()), &ip)
            .expect_err("rejected");
        assert_eq!(error.http_status(), 400);
        assert!(
            error.to_string().contains("delete and re-create"),
            "{error}"
        );
    }

    #[test]
    fn replacements_retain_an_explicit_alias() {
        let existing = Alias::try_new("my-internal-service").unwrap();
        let ip = [endpoint(EndpointScheme::Https, "10.0.1.1", 443)];

        // Omitted alias: the existing one is retained.
        assert_eq!(
            resolve_replacement(&existing, false, None, &ip)
                .expect("retained")
                .as_str(),
            "my-internal-service"
        );

        // The same alias is a tolerated no-op.
        assert_eq!(
            resolve_replacement(&existing, false, Some(existing.clone()), &ip)
                .expect("no-op")
                .as_str(),
            "my-internal-service"
        );

        // A differing alias is rejected; so is a move back to a derivable
        // endpoint set that would produce another alias.
        let renamed = Alias::try_new("renamed").unwrap();
        assert!(resolve_replacement(&existing, false, Some(renamed), &ip).is_err());

        let hostname = [endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        assert!(resolve_replacement(&existing, false, None, &hostname).is_err());
    }

    #[test]
    fn closest_match_prefers_the_closest_tenant() {
        let leaf = Uuid::from_u128(1);
        let parent = Uuid::from_u128(2);
        let root = Uuid::from_u128(3);
        let chain = [leaf, parent, root];

        let found = closest_match(&chain, |tenant| (tenant == parent).then_some(tenant));
        assert_eq!(found, Some(parent));

        // An exact match in the caller's tenant wins over an inherited one.
        let found = closest_match(&chain, |tenant| (tenant == leaf).then_some(tenant));
        assert_eq!(found, Some(leaf));

        assert_eq!(closest_match(&chain, |_| None::<Uuid>), None);
    }
}
