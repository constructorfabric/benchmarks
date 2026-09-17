//! Stable machine-readable field-violation codes.
//!
//! They ride on [`crate::domain::error::DomainError::FieldViolation`] and are
//! surfaced verbatim on the wire as problem `context.field.reason`, so they
//! must never be renamed casually.

/// Endpoint scheme is not one of the supported transports.
pub const SCHEME_UNSUPPORTED: &str = "endpoint.scheme.unsupported";
/// Endpoint host failed hostname / IP syntax validation.
pub const HOST_INVALID: &str = "endpoint.host.invalid";
/// Endpoint port outside `1..=65535`.
pub const PORT_OUT_OF_RANGE: &str = "endpoint.port.out_of_range";
/// Endpoint pool empty, or an operation would empty it.
pub const ENDPOINTS_EMPTY: &str = "server.endpoints.empty";
/// Protocol GTS id not recognised.
pub const PROTOCOL_UNKNOWN: &str = "protocol.unknown";
/// Alias failed the character / length rules.
pub const ALIAS_FORMAT: &str = "alias.format";
/// Alias collides with another upstream of the same tenant.
pub const ALIAS_TAKEN: &str = "alias.taken";
/// Tag failed `[a-z0-9_-]+`.
pub const TAG_FORMAT: &str = "tags.format";
/// Plugin reference is not a known built-in nor a resolvable custom plugin.
pub const PLUGIN_UNKNOWN: &str = "plugins.items.unknown_ref";
/// Plugin is still referenced by an upstream or route.
pub const PLUGIN_IN_USE: &str = "plugin.in_use";
/// Two routes claim the same match key for one upstream.
pub const MATCH_CONFLICT: &str = "match.duplicate";
/// Field set after creation cannot change.
pub const IMMUTABLE: &str = "immutable_field";
/// A required field was absent.
pub const MISSING: &str = "missing";
/// Auth plugin reference is not bindable from the control plane.
pub const AUTH_PLUGIN_UNSUPPORTED: &str = "auth.type.unsupported";
/// CORS rule contradiction (credentials with wildcard origin).
pub const CORS_CREDENTIALS_WILDCARD: &str = "cors.allow_credentials_wildcard";
