//! The built-in CORS handler (`ADR`-0004).
//!
//! Preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//! answered locally with a permissive `204` echoing what the browser asked for:
//! no upstream resolution, no tenant context. Origin and method enforcement
//! happens on the actual request, against the upstream's configuration, and a
//! refusal is `403`. `Vary: Origin` is always present so a shared cache cannot
//! serve one origin's headers to another.

use std::collections::BTreeSet;

use crate::domain::error::DomainError;
use crate::domain::model::CorsConfig;

/// The headers a preflight answer carries (`ADR`-0004, preflight response).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightResponse {
    /// `Access-Control-Allow-Origin`, echoed from the request.
    pub allow_origin: Option<String>,
    /// `Access-Control-Allow-Methods`, echoed from the request.
    pub allow_methods: Option<String>,
    /// `Access-Control-Allow-Headers`, echoed from the request.
    pub allow_headers: Option<String>,
    /// `Access-Control-Max-Age`, in seconds.
    pub max_age: u64,
    /// The `Vary` value.
    pub vary: String,
}

impl PreflightResponse {
    /// Every header as `(name, value)`, in the order `ADR`-0004 lists them.
    #[must_use]
    pub fn into_headers(self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        if let Some(origin) = self.allow_origin {
            headers.push(("access-control-allow-origin".to_owned(), origin));
        }
        if let Some(methods) = self.allow_methods {
            headers.push(("access-control-allow-methods".to_owned(), methods));
        }
        if let Some(allowed) = self.allow_headers {
            headers.push(("access-control-allow-headers".to_owned(), allowed));
        }
        headers.push((
            "access-control-max-age".to_owned(),
            self.max_age.to_string(),
        ));
        headers.push(("vary".to_owned(), self.vary));
        headers
    }
}

/// The headers an actual answer carries (`ADR`-0004, actual request handling).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CorsResponseHeaders {
    /// `Access-Control-Allow-Origin`, when the origin is allowed.
    pub allow_origin: Option<String>,
    /// `Access-Control-Allow-Credentials`, when configured.
    pub allow_credentials: bool,
    /// `Access-Control-Expose-Headers`, when configured.
    pub expose_headers: Option<String>,
    /// The `Vary` value.
    pub vary: String,
}

impl CorsResponseHeaders {
    /// Every header as `(name, value)`, in a stable order.
    #[must_use]
    pub fn into_headers(self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        if let Some(origin) = self.allow_origin {
            headers.push(("access-control-allow-origin".to_owned(), origin));
        }
        if self.allow_credentials {
            headers.push((
                "access-control-allow-credentials".to_owned(),
                "true".to_owned(),
            ));
        }
        if let Some(expose) = self.expose_headers {
            headers.push(("access-control-expose-headers".to_owned(), expose));
        }
        headers.push(("vary".to_owned(), self.vary));
        headers
    }
}

/// `true` when `request` is a CORS preflight (`ADR`-0004).
#[must_use]
pub fn is_preflight(method: &str, headers: &std::collections::BTreeMap<String, String>) -> bool {
    method.eq_ignore_ascii_case("options")
        && headers.contains_key("origin")
        && headers.contains_key("access-control-request-method")
}

/// The permissive preflight answer: what the browser asked for, echoed back
/// (`ADR`-0004, preflight request handling).
#[must_use]
pub fn preflight(headers: &std::collections::BTreeMap<String, String>) -> PreflightResponse {
    PreflightResponse {
        allow_origin: headers.get("origin").cloned(),
        allow_methods: headers.get("access-control-request-method").cloned(),
        allow_headers: headers.get("access-control-request-headers").cloned(),
        max_age: 86_400,
        vary: "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned(),
    }
}

/// Enforces the `cors` configuration of one upstream or route (`ADR`-0004).
#[derive(Debug, Clone)]
pub struct CorsPolicy {
    enabled: bool,
    allowed_origins: BTreeSet<String>,
    allowed_methods: BTreeSet<String>,
    expose_headers: Vec<String>,
    allow_credentials: bool,
}

impl CorsPolicy {
    /// A policy for `config`.
    #[must_use]
    pub fn new(config: &CorsConfig) -> Self {
        Self {
            enabled: config.enabled,
            allowed_origins: config
                .allowed_origins
                .iter()
                .map(|origin| origin.trim().to_owned())
                .collect(),
            allowed_methods: config
                .allowed_methods
                .iter()
                .map(|method| method.trim().to_ascii_uppercase())
                .collect(),
            expose_headers: config
                .expose_headers
                .iter()
                .map(|header| header.trim().to_owned())
                .collect(),
            allow_credentials: config.allow_credentials,
        }
    }

    /// A policy that denies every cross-origin request (`DESIGN` §2.2, secure
    /// by default).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            allowed_origins: BTreeSet::new(),
            allowed_methods: BTreeSet::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    /// `true` when the origin is admitted for an actual request.
    #[must_use]
    pub fn origin_allowed(&self, origin: Option<&str>) -> bool {
        match (self.enabled, origin) {
            (false, _) | (true, None) => true,
            (true, Some(origin)) => {
                self.allowed_origins.contains("*") || self.allowed_origins.contains(origin)
            }
        }
    }

    /// `true` when the method is admitted for an actual request.
    #[must_use]
    pub fn method_allowed(&self, method: &str) -> bool {
        !self.enabled || self.allowed_methods.contains(&method.to_ascii_uppercase())
    }

    /// The response headers of an allowed actual request; `None` when the
    /// request is same-origin (no `Origin` header) or CORS is disabled.
    #[must_use]
    pub fn response_headers(&self, origin: Option<&str>) -> Option<CorsResponseHeaders> {
        if !self.enabled || origin.is_none() {
            return None;
        }
        Some(CorsResponseHeaders {
            allow_origin: origin.map(ToOwned::to_owned),
            allow_credentials: self.allow_credentials && !self.allowed_origins.contains("*"),
            expose_headers: self
                .expose_headers
                .iter()
                .cloned()
                .reduce(|joined, header| format!("{joined}, {header}")),
            vary: "Origin".to_owned(),
        })
    }

    /// The `403` reason for an actual cross-origin request that is not
    /// admitted, or `None` when it is admitted.
    ///
    /// # Errors
    /// The returned error is the one the proxy handler emits.
    pub fn check(&self, origin: Option<&str>, method: &str) -> Result<(), DomainError> {
        if !self.enabled {
            return Ok(());
        }
        if !self.origin_allowed(origin) {
            return Err(DomainError::AccessDenied {
                detail: format!(
                    "origin '{}' is not allowed by this upstream's CORS policy",
                    origin.unwrap_or("")
                ),
            });
        }
        if !self.method_allowed(method) {
            return Err(DomainError::AccessDenied {
                detail: format!("method '{method}' is not allowed by this upstream's CORS policy"),
            });
        }
        Ok(())
    }
}
