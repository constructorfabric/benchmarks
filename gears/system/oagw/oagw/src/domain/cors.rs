//! CORS policy evaluation for the data plane (per ADR 0004).
//!
//! `CorsGate` decides whether a proxied request is CORS-eligible, whether a
//! preflight (`OPTIONS`) should be answered directly, and what
//! `Access-Control-*` headers to attach to allowed responses.

use crate::domain::error::DomainError;
use crate::domain::models::CorsConfig;

/// Decides how to act on the `Origin` of an inbound proxied request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsDecision {
    /// CORS is not in play (disabled or no `Origin` header) — proceed normally.
    Passthrough,
    /// The origin is not allowed — reject with `403`.
    OriginDenied,
    /// Preflight request that must be answered with CORS headers.
    Preflight {
        /// Response headers to attach to the `204` preflight response.
        headers: Vec<(String, String)>,
    },
    /// Allowed actual request — proceed and attach these response headers.
    Allowed {
        /// `Access-Control-*` headers to attach to the proxied response.
        headers: Vec<(String, String)>,
    },
}

/// Evaluator over a [`CorsConfig`].
#[derive(Debug, Clone)]
pub struct CorsGate {
    config: Option<CorsConfig>,
}

const WILDCARD: &str = "*";

impl CorsGate {
    /// Builds a gate from the effective CORS config (disabled policy → off).
    #[must_use]
    pub const fn new(config: Option<CorsConfig>) -> Self {
        Self { config }
    }

    /// Evaluates an inbound request for CORS handling.
    ///
    /// # Errors
    ///
    /// Returns `SsrfBlocked`-free `Cors`-typed errors only via the
    /// `OriginDenied` decision; this method itself is infallible and returns
    /// the decision to act on.
    #[must_use]
    pub fn evaluate(&self, method: &str, origin: Option<&str>) -> CorsDecision {
        let Some(cfg) = &self.config else {
            return CorsDecision::Passthrough;
        };
        if !cfg.enabled {
            return CorsDecision::Passthrough;
        }
        let Some(origin) = origin else {
            return CorsDecision::Passthrough;
        };

        let allowed = Self::origin_allowed(&cfg.allowed_origins, origin);
        if !allowed {
            return CorsDecision::OriginDenied;
        }

        let allow_origin =
            if cfg.allowed_origins.iter().any(|o| o == WILDCARD) && !cfg.allow_credentials {
                WILDCARD.to_owned()
            } else {
                origin.to_owned()
            };

        let methods = cfg
            .allowed_methods
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let request_headers = cfg
            .allowed_headers
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let expose = cfg
            .expose_headers
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");

        let mut headers = vec![
            ("Access-Control-Allow-Origin".to_owned(), allow_origin),
            ("Vary".to_owned(), "Origin".to_owned()),
        ];
        if cfg.allow_credentials {
            headers.push((
                "Access-Control-Allow-Credentials".to_owned(),
                "true".to_owned(),
            ));
        }
        if !expose.is_empty() {
            headers.push(("Access-Control-Expose-Headers".to_owned(), expose));
        }

        if method.eq_ignore_ascii_case("OPTIONS") {
            headers.push(("Access-Control-Allow-Methods".to_owned(), methods));
            headers.push(("Access-Control-Allow-Headers".to_owned(), request_headers));
            headers.push((
                "Access-Control-Max-Age".to_owned(),
                cfg.max_age_secs.to_string(),
            ));
            CorsDecision::Preflight { headers }
        } else {
            CorsDecision::Allowed { headers }
        }
    }

    /// Decides whether `origin` matches an entry in `allowed`.
    ///
    /// Only three shapes match: the exact wildcard `*`, an exact origin, or
    /// a scoped subdomain wildcard of the form `SCHEME://*.DOMAIN`. The
    /// scoped form covers exactly one subdomain label of an origin sharing
    /// the same scheme — with a dot boundary around the label, the apex
    /// domain itself is never matched, and no over-broad prefix match is
    /// possible (e.g. `https://*.example.com` does not match
    /// `https://example.com.evil.net`).
    fn origin_allowed(allowed: &[String], origin: &str) -> bool {
        allowed
            .iter()
            .any(|o| o == WILDCARD || o == origin || Self::scoped_wildcard(o, origin))
    }

    fn scoped_wildcard(pattern: &str, origin: &str) -> bool {
        let Some((scheme, rest)) = pattern.split_once("://") else {
            return false;
        };
        let Some(domain) = rest.strip_prefix("*.") else {
            return false;
        };
        let Some(origin_rest) = origin.strip_prefix(&format!("{scheme}://")) else {
            return false;
        };
        // Drop any explicit port so host matching is port-agnostic.
        let host = origin_rest.split(':').next().unwrap_or(origin_rest);
        // The first dot separates the wildcard-covered label from the apex
        // (`split_once`, not `rsplit_once`: the apex is everything after the
        // first label). Deeper labels push dots into the apex, which then
        // fails to equal the pattern's domain; a bare apex has no label.
        let Some((label, apex)) = host.split_once('.') else {
            return false;
        };
        !label.is_empty() && apex == domain
    }

    /// Converts an `OriginDenied` decision into a domain error (403 source).
    #[must_use]
    pub fn denied_error() -> DomainError {
        DomainError::PepDenied("CORS: origin not allowed".to_owned())
    }

    /// Rejects a CORS policy that combines a wildcard `*` origin with
    /// credentialed requests. Browsers refuse to attach credentials when the
    /// policy reflects `Access-Control-Allow-Origin: *`, so the combination
    /// is either silently broken or a misconfiguration; fail it loudly at
    /// config/route-validation time.
    ///
    /// # Errors
    ///
    /// Returns a validation error when credentials and a wildcard origin
    /// coexist in the policy.
    pub fn validate_policy(cfg: &CorsConfig) -> Result<(), DomainError> {
        if cfg.allow_credentials && cfg.allowed_origins.iter().any(|o| o == WILDCARD) {
            return Err(DomainError::validation(
                "CORS `allow_credentials=true` cannot be combined with a wildcard `*` origin",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CorsConfig {
        CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            allowed_headers: vec!["authorization".to_owned()],
            expose_headers: vec!["x-request-id".to_owned()],
            max_age_secs: 300,
            allow_credentials: true,
        }
    }

    #[test]
    fn disabled_policy_passes_through() {
        let gate = CorsGate::new(Some(CorsConfig::default()));
        assert_eq!(
            gate.evaluate("GET", Some("https://evil.example")),
            CorsDecision::Passthrough
        );
    }

    #[test]
    fn unknown_origin_denied() {
        let gate = CorsGate::new(Some(cfg()));
        assert_eq!(
            gate.evaluate("GET", Some("https://evil.example")),
            CorsDecision::OriginDenied
        );
    }

    #[test]
    fn allowed_actual_request_gets_headers() {
        let gate = CorsGate::new(Some(cfg()));
        let d = gate.evaluate("GET", Some("https://app.example.com"));
        let CorsDecision::Allowed { headers } = d else {
            panic!("expected Allowed");
        };
        assert!(
            headers
                .iter()
                .any(|(k, _)| k == "Access-Control-Allow-Origin")
        );
    }

    #[test]
    fn preflight_gets_methods_and_max_age() {
        let gate = CorsGate::new(Some(cfg()));
        let d = gate.evaluate("OPTIONS", Some("https://app.example.com"));
        let CorsDecision::Preflight { headers } = d else {
            panic!("expected Preflight");
        };
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Access-Control-Allow-Methods" && v == "GET, POST")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Access-Control-Max-Age" && v == "300")
        );
    }

    #[test]
    fn scoped_subdomain_wildcard_matches_exactly_one_label() {
        let gate = CorsGate::new(Some(CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://*.example.com".to_owned()],
            ..cfg()
        }));
        assert!(
            matches!(
                gate.evaluate("GET", Some("https://app.example.com")),
                CorsDecision::Allowed { .. }
            ),
            "single-label subdomain must match"
        );
        assert!(
            matches!(
                gate.evaluate("GET", Some("https://app.example.com:8443")),
                CorsDecision::Allowed { .. }
            ),
            "port-suffixed subdomain must match"
        );
        assert_eq!(
            gate.evaluate("GET", Some("https://example.com")),
            CorsDecision::OriginDenied,
            "apex domain must not match a subdomain wildcard"
        );
        assert_eq!(
            gate.evaluate("GET", Some("https://a.b.example.com")),
            CorsDecision::OriginDenied,
            "deeper than one label must not match"
        );
        assert_eq!(
            gate.evaluate("GET", Some("https://example.com.evil.net")),
            CorsDecision::OriginDenied,
            "no over-broad prefix match"
        );
        assert_eq!(
            gate.evaluate("GET", Some("http://app.example.com")),
            CorsDecision::OriginDenied,
            "different scheme must not match"
        );
    }

    #[test]
    fn credentials_with_wildcard_origin_rejected_at_validation() {
        let bad = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..cfg()
        };
        assert!(CorsGate::validate_policy(&bad).is_err());
        let good = CorsConfig {
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allow_credentials: true,
            ..cfg()
        };
        assert!(CorsGate::validate_policy(&good).is_ok());
        let wildcard_no_creds = CorsConfig {
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: false,
            ..cfg()
        };
        assert!(CorsGate::validate_policy(&wildcard_no_creds).is_ok());
    }
}
