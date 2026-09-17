// Created: 2026-09-04 by Constructor Tech
//! Plugin identification model.
//!
//! Implements `docs/PRD.md` `cpt-cf-oagw-fr-plugin-system` and
//! `cpt-cf-oagw-fr-builtin-plugins`: three plugin types (auth, guard,
//! transform), referenced either by a built-in GTS identifier or by the UUID
//! of an immutable custom (Starlark) plugin. Execution order is
//! Auth → Guards → Transform(request) → upstream → Transform(response/error).

use std::borrow::Cow;
use std::fmt;

use toolkit_gts::gts_id;
use uuid::Uuid;

use crate::error::OagwError;

/// GTS type of auth plugins (credential injection).
pub const AUTH_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
/// GTS type of guard plugins (validation / policy enforcement).
pub const GUARD_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
/// GTS type of transform plugins (request / response mutation).
pub const TRANSFORM_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");

/// No authentication.
pub const AUTH_NOOP: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
/// API key injection (header or query parameter).
pub const AUTH_APIKEY: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
/// OAuth2 client credentials flow.
pub const AUTH_OAUTH2_CLIENT_CRED: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
/// OAuth2 client credentials with HTTP Basic authentication.
pub const AUTH_OAUTH2_CLIENT_CRED_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");
/// HTTP Basic authentication (catalog identifier only).
pub const AUTH_BASIC: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
/// Bearer token injection (catalog identifier only).
pub const AUTH_BEARER: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");
/// Required header enforcement — the only guard bindable via
/// `plugins.items[].plugin_ref`.
pub const GUARD_REQUIRED_HEADERS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
/// Request timeout enforcement (core data-plane configuration).
pub const GUARD_TIMEOUT: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");
/// CORS preflight validation (core data-plane configuration).
pub const GUARD_CORS: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");
/// `X-Request-ID` propagation.
pub const TRANSFORM_REQUEST_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");
/// Request/response logging (core data-plane instrumentation).
pub const TRANSFORM_LOGGING: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// Prometheus metrics collection (core data-plane instrumentation).
pub const TRANSFORM_METRICS: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

/// Built-in plugins that are catalog identifiers only and therefore not
/// bindable in `plugins.items[].plugin_ref`.
pub const CATALOG_ONLY_PLUGINS: [&str; 6] = [
    AUTH_BASIC,
    AUTH_BEARER,
    GUARD_TIMEOUT,
    GUARD_CORS,
    TRANSFORM_LOGGING,
    TRANSFORM_METRICS,
];

/// Built-in plugins that may be referenced from `plugins.items[].plugin_ref`.
pub const BINDABLE_PLUGINS: [&str; 6] = [
    AUTH_NOOP,
    AUTH_APIKEY,
    AUTH_OAUTH2_CLIENT_CRED,
    AUTH_OAUTH2_CLIENT_CRED_BASIC,
    GUARD_REQUIRED_HEADERS,
    TRANSFORM_REQUEST_ID,
];

/// Kind of a plugin (`docs/PRD.md` `cpt-cf-oagw-fr-plugin-system`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginKind {
    /// Credential injection; runs first.
    Auth,
    /// Validation / policy enforcement; may reject the request.
    Guard,
    /// Request and response mutation.
    Transform,
}

impl PluginKind {
    /// Every plugin kind.
    pub const ALL: [PluginKind; 3] = [Self::Auth, Self::Guard, Self::Transform];

    /// GTS type of the plugin kind.
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::Auth => AUTH_PLUGIN_TYPE,
            Self::Guard => GUARD_PLUGIN_TYPE,
            Self::Transform => TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Short kind name used by the REST surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// Position in the execution order Auth → Guard → Transform.
    #[must_use]
    pub const fn execution_rank(self) -> u8 {
        match self {
            Self::Auth => 0,
            Self::Guard => 1,
            Self::Transform => 2,
        }
    }

    /// Parses a kind token or a GTS type identifier.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown kind.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auth" | AUTH_PLUGIN_TYPE => Ok(Self::Auth),
            "guard" | GUARD_PLUGIN_TYPE => Ok(Self::Guard),
            "transform" | TRANSFORM_PLUGIN_TYPE => Ok(Self::Transform),
            _ => Err(OagwError::Validation {
                detail: format!("unknown plugin kind '{raw}' (expected auth, guard or transform)"),
            }),
        }
    }
}

impl fmt::Display for PluginKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for PluginKind {
    type Err = OagwError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

/// Phase of the request lifecycle a plugin participates in
/// (`docs/PRD.md` `cpt-cf-oagw-fr-plugin-system`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginPhase {
    /// Credential injection.
    Auth,
    /// Validation; may reject the request.
    Guard,
    /// Outbound request mutation.
    RequestTransform,
    /// Inbound response mutation.
    ResponseTransform,
    /// Error path mutation.
    ErrorTransform,
}

impl PluginPhase {
    /// Stable execution order of the phases.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Auth => 0,
            Self::Guard => 1,
            Self::RequestTransform => 2,
            Self::ResponseTransform => 3,
            Self::ErrorTransform => 4,
        }
    }
}

/// A plugin reference: a built-in GTS identifier or the UUID of a custom
/// (Starlark) plugin (`docs/schemas/upstream.v1.schema.json`
/// `plugins.items[].plugin_ref`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PluginInstance {
    /// A built-in plugin identified by its GTS instance id.
    Named(String),
    /// A custom plugin identified by its UUID.
    Custom(Uuid),
}

/// A typed plugin reference inside a [`crate::domain::upstream::PluginChain`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginRef {
    kind: PluginKind,
    instance: PluginInstance,
}

impl PluginRef {
    /// Builds a plugin reference from its parts.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when `raw` is neither a GTS instance
    /// id of `kind` nor a UUID.
    pub fn parse(kind: PluginKind, raw: &str) -> Result<Self, OagwError> {
        let trimmed = raw.trim();
        if trimmed.starts_with(kind.gts_type()) {
            if gts::GtsInstanceId::try_new(trimmed).is_err() {
                return Err(OagwError::Validation {
                    detail: format!("'{raw}' is not a valid GTS instance identifier"),
                });
            }
            return Ok(Self {
                kind,
                instance: PluginInstance::Named(trimmed.to_owned()),
            });
        }
        let id = Uuid::parse_str(trimmed).map_err(|_| OagwError::Validation {
            detail: format!(
                "'{raw}' must be a built-in {} plugin GTS id or a custom plugin UUID",
                kind.as_str()
            ),
        })?;
        Ok(Self {
            kind,
            instance: PluginInstance::Custom(id),
        })
    }

    /// Builds a reference to a built-in plugin by its GTS instance id.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when `raw` is not a GTS instance id
    /// of `kind`.
    pub fn builtin(kind: PluginKind, raw: &str) -> Result<Self, OagwError> {
        let parsed = Self::parse(kind, raw)?;
        match parsed.instance {
            PluginInstance::Named(_) => Ok(parsed),
            PluginInstance::Custom(_) => Err(OagwError::Validation {
                detail: format!("'{raw}' is not a built-in {} plugin", kind.as_str()),
            }),
        }
    }

    /// Plugin kind.
    #[must_use]
    pub const fn kind(&self) -> PluginKind {
        self.kind
    }

    /// Plugin instance (GTS id or custom UUID).
    #[must_use]
    pub const fn instance(&self) -> &PluginInstance {
        &self.instance
    }

    /// UUID of a custom plugin, `None` for a built-in one.
    #[must_use]
    pub const fn plugin_uuid(&self) -> Option<Uuid> {
        match &self.instance {
            PluginInstance::Custom(id) => Some(*id),
            PluginInstance::Named(_) => None,
        }
    }

    /// `true` when this reference identifies `id` (useful for binding checks
    /// before a custom plugin is deleted).
    #[must_use]
    pub fn uuid_matches(&self, id: Uuid) -> bool {
        match &self.instance {
            PluginInstance::Custom(uuid) => *uuid == id,
            PluginInstance::Named(_) => false,
        }
    }

    /// `true` for a built-in (GTS-identified) plugin.
    #[must_use]
    pub const fn is_builtin(&self) -> bool {
        matches!(&self.instance, PluginInstance::Named(_))
    }

    /// `true` when the plugin may appear in `plugins.items[]`
    /// (`docs/PRD.md` `cpt-cf-oagw-fr-builtin-plugins`: catalog-only
    /// identifiers are not bindable).
    #[must_use]
    pub fn is_bindable(&self) -> bool {
        match &self.instance {
            PluginInstance::Custom(_) => true,
            PluginInstance::Named(id) => CATALOG_ONLY_PLUGINS.iter().all(|only| only != id),
        }
    }

    /// Identifier as rendered on the wire: a GTS instance id for a built-in
    /// plugin, the canonical UUID form for a custom one.
    #[must_use]
    pub fn as_ref_str(&self) -> Cow<'_, str> {
        match &self.instance {
            PluginInstance::Named(id) => Cow::Borrowed(id.as_str()),
            PluginInstance::Custom(id) => Cow::Owned(id.as_simple().to_string()),
        }
    }
}

impl fmt::Display for PluginRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_ref_str())
    }
}

/// Descriptor of a custom (Starlark) plugin
/// (`docs/DESIGN.md` §3.1 `Plugin`, GTS types `gts.cf.core.oagw.*_plugin.v1~`).
///
/// Custom plugins are immutable after creation: an update creates a new
/// plugin and re-binds the references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    /// System-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Plugin kind (also selects the GTS type).
    pub kind: PluginKind,
    /// Human-readable name.
    pub name: String,
    /// Starlark source; never returned together with a credential.
    pub source: String,
    /// `true` once no upstream or route references the plugin anymore.
    pub gc_eligible: bool,
}

impl Plugin {
    /// Creates a plugin descriptor.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the name or the source is
    /// empty.
    pub fn new(
        id: uuid::Uuid,
        tenant_id: uuid::Uuid,
        kind: PluginKind,
        name: String,
        source: String,
    ) -> Result<Self, OagwError> {
        if name.trim().is_empty() {
            return Err(OagwError::Validation {
                detail: String::from("the plugin name must not be empty"),
            });
        }
        if source.trim().is_empty() {
            return Err(OagwError::Validation {
                detail: String::from("the plugin source must not be empty"),
            });
        }
        Ok(Self {
            id,
            tenant_id,
            kind,
            name,
            source,
            gc_eligible: false,
        })
    }

    /// GTS type of the plugin.
    #[must_use]
    pub const fn gts_type(&self) -> &'static str {
        self.kind.gts_type()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "plugin_tests.rs"]
mod plugin_tests;
