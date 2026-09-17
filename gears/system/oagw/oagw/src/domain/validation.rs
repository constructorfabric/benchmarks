//! Configuration validation: endpoint / hostname checks and alias derivation.
//!
//! Rules come from `DESIGN.md §3.1` (Alias Enforcement Rules, Hostname
//! Validation) and the wire schemas in `docs/schemas/`.

use std::collections::BTreeSet;
use std::net::IpAddr;

use crate::domain::model::{
    CorsConfig, Endpoint, MatchRule, PluginBinding, PluginsConfig, RateLimit, Route, SharingMode,
    Upstream,
};
use crate::error::{ErrorKind, OagwError};

/// Reject a request with `ValidationError` carrying `fields` details.
fn validation(detail: impl Into<String>, fields: Vec<&'static str>) -> OagwError {
    let mut e = OagwError::new(ErrorKind::ValidationError, detail);
    if !fields.is_empty() {
        e = e.with_ext("fields", serde_json::Value::from(fields));
    }
    e
}

/// Validate a host: RFC 1123 hostname, or an IPv4/IPv6 literal.
///
/// A trailing dot (FQDN notation) is tolerated and stripped.
pub fn validate_host(host: &str) -> Result<&str, OagwError> {
    if host.is_empty() {
        return Err(validation("endpoint host must not be empty", vec!["server.endpoints[].host"]));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(host);
    }
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.len() > 253 {
        return Err(validation(
            "endpoint host exceeds the 253 character RFC 1123 limit",
            vec!["server.endpoints[].host"],
        ));
    }
    let is_valid = trimmed.split('.').all(|label| {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });
    if !is_valid {
        return Err(validation(
            format!("endpoint host '{host}' is not a valid RFC 1123 hostname or IP address"),
            vec!["server.endpoints[].host"],
        ));
    }
    Ok(trimmed)
}

/// Normalize an alias: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim().to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// Validate an alias against the wire-schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
pub fn validate_alias(alias: &str) -> Result<(), OagwError> {
    let ok = !alias.is_empty()
        && alias.len() <= 253
        && alias.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && alias
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && alias
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | ':' | '-'));
    if ok {
        Ok(())
    } else {
        Err(validation(
            format!("alias '{alias}' does not match the required pattern"),
            vec!["alias"],
        ))
    }
}

/// Longest public-suffix–aware common suffix shared by all hosts.
///
/// Returns `None` when the hosts do not share a registrable suffix
/// (fewer than two labels, or the shared suffix is a bare public suffix
/// such as `co.uk`).
#[must_use]
pub fn common_domain_suffix(hosts: &[&str]) -> Option<String> {
    fn labels_of(host: &str) -> Vec<&str> {
        host.trim_end_matches('.').split('.').rev().collect()
    }
    let mut shared: Option<Vec<&str>> = None;
    for host in hosts {
        let labels = labels_of(host);
        let keep = match &shared {
            Some(current) => {
                let limit = current.len().min(labels.len());
                (0..limit)
                    .take_while(|i: &usize| current[*i].eq_ignore_ascii_case(labels[*i]))
                    .count()
            }
            None => labels.len(),
        };
        if keep == 0 {
            return None;
        }
        let prefix: Vec<&str> = labels[..keep].to_vec();
        shared = Some(prefix);
    }
    let shared = shared?;
    if shared.len() < 2 {
        return None;
    }
    let mut parts = shared;
    parts.reverse();
    let suffix = parts.join(".");
    // A registrable suffix must not be a bare public suffix (e.g. `co.uk`).
    if psl::domain_str(&suffix).is_none_or(|d| d != suffix) {
        return None;
    }
    Some(suffix)
}

/// Compute the alias a hostname-based endpoint pool derives to.
///
/// Returns `None` for IP-based pools, heterogeneous pools without a
/// registrable common suffix, and pools whose only common suffix is a bare
/// public suffix — all of which require an explicit alias.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    let ports: BTreeSet<u16> = endpoints.iter().map(|e| e.port).collect();
    if ports.len() > 1 {
        return None;
    }
    let port = *ports.iter().next()?;

    if endpoints.len() == 1 {
        let e = &endpoints[0];
        if e.is_ip() {
            return None;
        }
        let host = validate_host(&e.host).ok()?;
        return Some(if e.is_standard_port() {
            host.to_ascii_lowercase()
        } else {
            format!("{}:{}", host.to_ascii_lowercase(), port)
        });
    }

    if endpoints.iter().any(Endpoint::is_ip) {
        return None;
    }
    let schemes: BTreeSet<&str> = endpoints.iter().map(|e| e.scheme.as_str()).collect();
    if schemes.len() > 1 {
        return None;
    }
    let hosts: Vec<&str> = endpoints.iter().map(|e| e.host.as_str()).collect();
    let suffix = common_domain_suffix(&hosts)?;
    Some(if endpoints[0].is_standard_port() {
        suffix
    } else {
        format!("{}:{}", suffix, port)
    })
}

/// Validate endpoint pool invariants (DESIGN: all endpoints must share
/// protocol, scheme and port).
pub fn validate_endpoints(endpoints: &[Endpoint], allow_http: bool) -> Result<(), OagwError> {
    if endpoints.is_empty() {
        return Err(validation(
            "at least one endpoint is required",
            vec!["server.endpoints"],
        ));
    }
    let mut seen = BTreeSet::new();
    for e in endpoints {
        validate_host(&e.host)?;
        if e.port == 0 {
            return Err(validation("endpoint port must be between 1 and 65535", vec!["server.endpoints[].port"]));
        }
        if e.is_plaintext() && !allow_http {
            return Err(validation(
                format!(
                    "plaintext endpoint scheme '{}' is not allowed; enable oagw.config.allow_http_upstream to permit it",
                    e.scheme
                ),
                vec!["server.endpoints[].scheme"],
            ));
        }
        if !matches!(
            e.scheme.as_str(),
            "https" | "wss" | "wt" | "grpc" | "http"
        ) {
            return Err(validation(
                format!("unsupported endpoint scheme '{}'", e.scheme),
                vec!["server.endpoints[].scheme"],
            ));
        }
        seen.insert((e.scheme.clone(), e.port));
    }
    if seen.len() > 1 {
        return Err(validation(
            "all endpoints in a pool must use the same scheme and port",
            vec!["server.endpoints[].scheme"],
        ));
    }
    Ok(())
}

/// Validate a rate-limit block.
pub fn validate_rate_limit(rl: &RateLimit) -> Result<(), OagwError> {
    if rl.sustained.rate == 0 {
        return Err(validation("rate_limit.sustained.rate must be >= 1", vec!["rate_limit.sustained.rate"]));
    }
    if rl.capacity() == 0 {
        return Err(validation("rate_limit.burst.capacity must be >= 1", vec!["rate_limit.burst.capacity"]));
    }
    if rl.cost == 0 {
        return Err(validation("rate_limit.cost must be >= 1", vec!["rate_limit.cost"]));
    }
    Ok(())
}

/// Validate a CORS block, including the `allow_credentials` / `*` rule.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), OagwError> {
    if cors.allow_credentials && cors.allows_wildcard() {
        return Err(validation(
            "cors.allow_credentials is incompatible with allowed_origins ['*']",
            vec!["cors.allowed_origins"],
        ));
    }
    for origin in &cors.allowed_origins {
        if origin == "*" {
            continue;
        }
        if url::Url::parse(origin).is_err() {
            return Err(validation(
                format!("cors.allowed_origins entry '{origin}' is not a valid URI"),
                vec!["cors.allowed_origins"],
            ));
        }
    }
    for method in &cors.allowed_methods {
        if http::Method::from_bytes(method.as_bytes()).is_err() {
            return Err(validation(
                format!("cors.allowed_methods entry '{method}' is not a valid HTTP method"),
                vec!["cors.allowed_methods"],
            ));
        }
    }
    Ok(())
}

/// Validate a plugin binding list.
pub fn validate_plugins(plugins: &PluginsConfig) -> Result<(), OagwError> {
    for b in &plugins.items {
        validate_plugin_ref(b)?;
    }
    Ok(())
}

/// Validate a single plugin reference: a built-in GTS id or a UUID.
pub fn validate_plugin_ref(b: &PluginBinding) -> Result<(), OagwError> {
    let r = b.plugin_ref.as_str();
    if crate::domain::model::PluginKind::from_ref(r).is_some() {
        return Ok(());
    }
    if uuid::Uuid::parse_str(r).is_ok() {
        return Ok(());
    }
    if crate::domain::model::parse_uuid_suffix(r).is_some() {
        return Ok(());
    }
    Err(validation(
        format!("plugin_ref '{r}' is neither a recognized GTS plugin identifier nor a UUID"),
        vec!["plugins.items[].plugin_ref"],
    ))
}

/// Validate an upstream, returning the derived-or-confirmed alias.
pub fn validate_upstream(
    alias: Option<&str>,
    endpoints: &[crate::domain::model::Endpoint],
    protocol: &str,
    rate_limit: Option<&RateLimit>,
    cors: Option<&CorsConfig>,
    plugins: &PluginsConfig,
    allow_http: bool,
) -> Result<String, OagwError> {
    validate_endpoints(endpoints, allow_http)?;
    if protocol != crate::domain::model::PROTOCOL_HTTP
        && protocol != crate::domain::model::PROTOCOL_GRPC
    {
        return Err(validation(
            format!("protocol '{protocol}' is not a recognized OAGW protocol identifier"),
            vec!["protocol"],
        ));
    }
    if let Some(rl) = rate_limit {
        validate_rate_limit(rl)?;
    }
    if let Some(c) = cors {
        validate_cors(c)?;
    }
    validate_plugins(plugins)?;

    let derived = compute_derived_alias(endpoints);
    match (derived, alias) {
        (Some(derived), Some(user)) => {
            let user = normalize_alias(user);
            if user != derived {
                return Err(validation(
                    format!(
                        "alias '{user}' does not match the auto-derived alias '{derived}'; \
                         hostname-based endpoints always auto-derive the alias"
                    ),
                    vec!["alias"],
                ));
            }
            Ok(derived)
        }
        (Some(derived), None) => Ok(derived),
        (None, Some(user)) => {
            let user = normalize_alias(user);
            validate_alias(&user)?;
            Ok(user)
        }
        (None, None) => Err(validation(
            "an explicit alias is required for IP-based or non-derivable endpoints",
            vec!["alias"],
        )),
    }
}

/// Validate a route against its parent upstream.
pub fn validate_route(route: &Route, upstream: &Upstream) -> Result<(), OagwError> {
    if route.tenant_id != upstream.tenant_id {
        return Err(validation(
            "route tenant does not match the referenced upstream tenant",
            vec!["upstream_id"],
        ));
    }
    if route.upstream_id != upstream.id {
        return Err(validation(
            "upstream_id does not reference the resolved upstream",
            vec!["upstream_id"],
        ));
    }
    match &route.r#match {
        MatchRule::Grpc(_) => {
            if upstream.protocol != crate::domain::model::PROTOCOL_GRPC {
                return Err(validation(
                    "grpc match rule requires an upstream with the grpc protocol",
                    vec!["match.grpc"],
                ));
            }
        }
        MatchRule::Http(http) => {
            if upstream.protocol != crate::domain::model::PROTOCOL_HTTP {
                return Err(validation(
                    "http match rule requires an upstream with the http protocol",
                    vec!["match.http"],
                ));
            }
            if http.methods.is_empty() {
                return Err(validation(
                    "match.http.methods must contain at least one method",
                    vec!["match.http.methods"],
                ));
            }
            for m in &http.methods {
                if http::Method::from_bytes(m.as_bytes()).is_err() {
                    return Err(validation(
                        format!("match.http.methods entry '{m}' is not a valid HTTP method"),
                        vec!["match.http.methods"],
                    ));
                }
            }
            if http.path.is_empty() || !http.path.starts_with('/') {
                return Err(validation(
                    "match.http.path must be an absolute path starting with '/'",
                    vec!["match.http.path"],
                ));
            }
        }
    }
    if let Some(rl) = &route.rate_limit {
        validate_rate_limit(rl)?;
    }
    if let Some(c) = &route.cors {
        validate_cors(c)?;
    }
    validate_plugins(&route.plugins)?;
    Ok(())
}

/// Enforce alias immutability on replace (`DESIGN.md` alias update table).
pub fn enforce_alias_update(
    existing: &str,
    provided: Option<&str>,
    new_endpoints: &[crate::domain::model::Endpoint],
) -> Result<(), OagwError> {
    let derived_now = compute_derived_alias(new_endpoints);
    match (derived_now, provided) {
        (Some(derived), Some(user)) => {
            if normalize_alias(user) != existing {
                return Err(validation(
                    format!(
                        "alias is immutable once set; endpoints derive '{derived}' but the \
                         upstream is registered as '{existing}' — delete and re-create instead"
                    ),
                    vec!["alias"],
                ));
            }
            Ok(())
        }
        (Some(derived), None) => {
            // "Derivable → Derivable (endpoints change)": an endpoint change
            // that would alter the derived alias is rejected — the alias is
            // the routing key, so the operator deletes and re-creates instead.
            if derived != existing {
                return Err(validation(
                    format!(
                        "alias is immutable once set; the new endpoints derive '{derived}' but \
                         the upstream is registered as '{existing}' — delete and re-create instead"
                    ),
                    vec!["alias"],
                ));
            }
            Ok(())
        }
        (None, Some(user)) => {
            if normalize_alias(user) != existing {
                return Err(validation(
                    "alias is immutable once set and cannot be changed on a non-derivable upstream",
                    vec!["alias"],
                ));
            }
            Ok(())
        }
        (None, None) => Ok(()),
    }
}

/// Effective sharing-mode check used by control-plane writes.
#[must_use]
pub fn allows_override(mode: SharingMode) -> bool {
    !matches!(mode, SharingMode::Enforce)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(scheme: &str, host: &str, port: u16) -> crate::domain::model::Endpoint {
        crate::domain::model::Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn derives_single_hostname_alias() {
        assert_eq!(
            compute_derived_alias(&[ep("https", "api.openai.com", 443)]).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn derives_non_standard_port_alias() {
        assert_eq!(
            compute_derived_alias(&[ep("https", "api.openai.com", 8443)]).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn derives_common_suffix_alias() {
        assert_eq!(
            compute_derived_alias(&[ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)])
                .as_deref(),
            Some("vendor.com")
        );
    }

    #[test]
    fn derives_common_suffix_with_port() {
        assert_eq!(
            compute_derived_alias(&[ep("https", "us.vendor.com", 8443), ep("https", "eu.vendor.com", 8443)])
                .as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        assert_eq!(
            compute_derived_alias(&[ep("https", "foo.co.uk", 443), ep("https", "bar.co.uk", 443)]),
            None
        );
    }

    #[test]
    fn heterogeneous_hosts_are_not_derivable() {
        assert_eq!(
            compute_derived_alias(&[ep("https", "us.foo.com", 443), ep("https", "eu.bar.com", 443)]),
            None
        );
    }

    #[test]
    fn ips_require_explicit_alias() {
        assert_eq!(
            compute_derived_alias(&[ep("https", "10.0.1.1", 443), ep("https", "10.0.1.2", 443)]),
            None
        );
    }

    #[test]
    fn mismatched_alias_is_rejected() {
        let err = validate_upstream(
            Some("my-alias"),
            &[ep("https", "api.openai.com", 443)],
            crate::domain::model::PROTOCOL_HTTP,
            None,
            None,
            &Default::default(),
            false,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ValidationError);
    }

    #[test]
    fn matching_alias_is_tolerated() {
        let alias = validate_upstream(
            Some("api.openai.com"),
            &[ep("https", "api.openai.com", 443)],
            crate::domain::model::PROTOCOL_HTTP,
            None,
            None,
            &Default::default(),
            false,
        )
        .expect("idempotent alias");
        assert_eq!(alias, "api.openai.com");
    }

    #[test]
    fn missing_alias_on_ip_pool_is_rejected() {
        let err = validate_upstream(
            None,
            &[ep("https", "10.0.1.1", 443)],
            crate::domain::model::PROTOCOL_HTTP,
            None,
            None,
            &Default::default(),
            false,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ValidationError);
    }

    #[test]
    fn plaintext_requires_flag() {
        let err = validate_upstream(
            Some("localhost"),
            &[ep("http", "localhost", 8080)],
            crate::domain::model::PROTOCOL_HTTP,
            None,
            None,
            &Default::default(),
            false,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ValidationError);
    }

    #[test]
    fn plaintext_allowed_with_flag() {
        let alias = validate_upstream(
            Some("localhost:8080"),
            &[ep("http", "localhost", 8080)],
            crate::domain::model::PROTOCOL_HTTP,
            None,
            None,
            &Default::default(),
            true,
        )
        .expect("http allowed");
        assert_eq!(alias, "localhost:8080");
    }

    #[test]
    fn invalid_hostname_is_rejected() {
        assert!(validate_host("-bad.example.com").is_err());
        assert!(validate_host("bad..com").is_err());
        assert!(validate_host(&"a".repeat(254)).is_err());
        assert_eq!(validate_host("api.example.com.").unwrap(), "api.example.com");
    }

    #[test]
    fn trailing_dot_and_case_are_normalized() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn wildcard_credentials_conflict_is_rejected() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..Default::default()
        };
        assert!(validate_cors(&cors).is_err());
    }

    #[test]
    fn alias_immutability_on_derivable_upstream() {
        let eps = [ep("https", "api.openai.com", 443)];
        assert!(enforce_alias_update("api.openai.com", Some("api.openai.com"), &eps).is_ok());
        assert!(enforce_alias_update("api.openai.com", Some("other"), &eps).is_err());
    }

    #[test]
    fn alias_rejection_when_endpoints_would_change_it() {
        let eps = [ep("https", "api.other.com", 443)];
        assert!(enforce_alias_update("api.openai.com", None, &eps).is_err());
    }
}
