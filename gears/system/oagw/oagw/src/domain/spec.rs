// Created: 2026-08-31 by Constructor Tech
//! Write-path payloads (DESIGN §3.3 "CRUD Semantics").
//!
//! These are the domain-level shapes of the create/replace bodies: the REST
//! layer owns the serde/OpenAPI annotations, the domain layer owns the
//! semantics. Because every nested member is a shared [`crate::domain::model`]
//! type, DTO → spec is a field move and cannot lose data.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use crate::domain::model::PluginKind;
use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, PluginsConfig, RateLimitConfig, RouteMatch,
};
use crate::domain::validation::MAX_SOURCE_BYTES;

/// Endpoint as written on the wire (`server.endpoints[]` of the upstream
/// schema, where only `scheme` and `host` are required).
///
/// `port` is optional: `None` means "use the scheme default". An explicit
/// `0` is preserved rather than substituted, so port validation still rejects
/// it with the configured problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EndpointSpec {
    /// Connection scheme.
    pub scheme: crate::domain::model::Scheme,
    /// Hostname or IP address.
    pub host: String,
    /// Port; omitted on the wire when it equals the scheme default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl From<Endpoint> for EndpointSpec {
    fn from(endpoint: Endpoint) -> Self {
        Self {
            scheme: endpoint.scheme,
            host: endpoint.host,
            port: Some(endpoint.port),
        }
    }
}

/// Endpoint pool wrapper (`server` in the upstream schema).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServerSpec {
    /// Endpoints of the pool.
    #[serde(default)]
    pub endpoints: Vec<EndpointSpec>,
}

/// Upstream create/replace payload (upstream schema).
#[derive(Debug, Clone, PartialEq, Deserialize, utoipa::ToSchema)]
pub struct UpstreamSpec {
    /// Explicit alias; derived from the endpoints when omitted.
    #[serde(default)]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Wire protocol (canonical GTS id).
    pub protocol: crate::domain::model::Protocol,
    /// Endpoint pool.
    pub server: ServerSpec,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Auth plugin binding.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// Route create/replace payload (route schema).
///
/// `upstream_id` is required on create and absent on replace (immutable).
#[derive(Debug, Clone, PartialEq, Deserialize, utoipa::ToSchema)]
pub struct RouteSpec {
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Match rule (`match` on the wire).
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy (ADR-0004 "Configuration Schema": a first-class field of
    /// the route as well as of the upstream).
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// Route replace payload (route schema, PUT semantics).
///
/// `upstream_id` is immutable and therefore absent: moving a route to another
/// upstream is a delete plus a create (DESIGN §3.3 "PUT (Replace)").
#[derive(Debug, Clone, PartialEq, Deserialize, utoipa::ToSchema)]
pub struct RouteUpdateSpec {
    /// Whether the route participates in matching.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Match rule (`match` on the wire).
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy (ADR-0004).
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// Plugin create payload (ADR-0002 appendix A "Definition").
#[derive(Debug, Clone, Default, PartialEq, Deserialize, utoipa::ToSchema)]
pub struct PluginSpec {
    /// Unique name within the tenant.
    #[serde(default)]
    pub name: Option<String>,
    /// Plugin family (`auth` | `guard` | `transform`).
    #[serde(rename = "plugin_type", alias = "type", default)]
    pub plugin_type: Option<PluginKind>,
    /// Whether the plugin is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Plugin configuration.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// JSON Schema describing `config`.
    #[serde(default)]
    pub config_schema: Option<serde_json::Value>,
    /// Free-text description.
    #[serde(default)]
    pub description: Option<String>,
    /// Starlark source.
    #[serde(alias = "source", default)]
    pub source_code: Option<String>,
}

impl PluginSpec {
    /// Name, required for the create path.
    ///
    /// # Errors
    /// 400 when `name` is missing or only whitespace.
    pub fn name(&self) -> Result<&str, crate::error::OagwError> {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| crate::error::OagwError::validation("plugin name is required"))
    }

    /// Plugin family, required for the create path.
    ///
    /// # Errors
    /// 400 when `plugin_type` is missing or not a known family.
    pub fn plugin_type(&self) -> Result<PluginKind, crate::error::OagwError> {
        self.plugin_type.ok_or_else(|| {
            crate::error::OagwError::validation("plugin_type must be one of auth, guard, transform")
        })
    }

    /// Starlark source, required for the create path.
    ///
    /// An empty script has nothing to run and an oversized one would dominate
    /// the record, so both are rejected here rather than at compile time in
    /// slice 2 (DESIGN §3.2 body validation).
    ///
    /// # Errors
    /// 400 when `source_code` is missing, empty or longer than
    /// [`MAX_SOURCE_BYTES`].
    pub fn source_code(&self) -> Result<&str, crate::error::OagwError> {
        let source = self
            .source_code
            .as_deref()
            .ok_or_else(|| crate::error::OagwError::validation("plugin source_code is required"))?;
        if source.trim().is_empty() {
            return Err(crate::error::OagwError::validation(
                "plugin source_code must not be empty",
            )
            .with_extension(|ext| ext.invalid_value = Some(source.to_owned())));
        }
        if source.len() > MAX_SOURCE_BYTES {
            return Err(crate::error::OagwError::validation(format!(
                "plugin source_code exceeds {MAX_SOURCE_BYTES} bytes"
            ))
            .with_extension(|ext| ext.invalid_value = Some(source.len().to_string())));
        }
        Ok(source)
    }
}

impl ServerSpec {
    /// Endpoints with every omitted port materialised to the scheme default,
    /// validated by the caller.
    ///
    /// An endpoint may be written without a `port`; storing `0` would fail
    /// port validation, so the default of the declared scheme is filled in
    /// here (DESIGN §3.2 "Standard ports"). An explicit `0` survives the
    /// conversion and is rejected downstream.
    #[must_use]
    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.endpoints
            .iter()
            .map(|spec| Endpoint {
                scheme: spec.scheme,
                host: spec.host.clone(),
                port: spec.port.unwrap_or(spec.scheme.default_port()),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::model::Scheme;
    use crate::error::OagwErrorKind;

    use super::{EndpointSpec, MAX_SOURCE_BYTES, PluginSpec, ServerSpec};

    fn endpoint(scheme: Scheme, host: &str, port: Option<u16>) -> EndpointSpec {
        EndpointSpec {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn an_omitted_port_takes_the_scheme_default() {
        let spec = ServerSpec {
            endpoints: vec![
                endpoint(Scheme::Https, "api.vendor.com", None),
                endpoint(Scheme::Http, "10.0.0.1", None),
            ],
        };
        let ports: Vec<u16> = spec.endpoints().iter().map(|e| e.port).collect();
        assert_eq!(ports, vec![443, 80]);
    }

    #[test]
    fn a_declared_port_is_kept() {
        let spec = ServerSpec {
            endpoints: vec![endpoint(Scheme::Https, "api.vendor.com", Some(8443))],
        };
        assert_eq!(spec.endpoints()[0].port, 8443);
    }

    #[test]
    fn an_explicit_zero_is_preserved_for_validation() {
        let spec = ServerSpec {
            endpoints: vec![endpoint(Scheme::Https, "api.vendor.com", Some(0))],
        };
        assert_eq!(spec.endpoints()[0].port, 0);
    }

    #[test]
    fn an_empty_source_is_rejected() {
        for source in ["", "   \n"] {
            let spec = PluginSpec {
                source_code: Some(source.to_owned()),
                ..PluginSpec::default()
            };
            let error = spec.source_code().unwrap_err();
            assert_eq!(error.kind(), &OagwErrorKind::Validation);
        }
    }

    #[test]
    fn an_oversized_source_is_rejected() {
        let size = MAX_SOURCE_BYTES + 1;
        let spec = PluginSpec {
            source_code: Some("x".repeat(size)),
            ..PluginSpec::default()
        };
        let error = spec.source_code().unwrap_err();
        let reported = size.to_string();
        assert_eq!(
            error.extensions().invalid_value.as_deref(),
            Some(reported.as_str())
        );
    }

    #[test]
    fn a_source_at_the_cap_is_accepted() {
        let spec = PluginSpec {
            source_code: Some("x".repeat(MAX_SOURCE_BYTES)),
            ..PluginSpec::default()
        };
        assert_eq!(
            spec.source_code().unwrap_or_default().len(),
            MAX_SOURCE_BYTES
        );
    }
}
