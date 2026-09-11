//! GTS instance-identifier helpers for the management API path parameters.
//!
//! `{id}` path segments carry the anonymous instance form
//! `gts.cf.core.oagw.<resource>.v1~{uuid}`; the UUID is the storage key.

use uuid::Uuid;

use crate::domain::model::gts;

/// Error raised when a path segment is not a GTS instance identifier.
#[derive(Debug, thiserror::Error)]
pub enum IdError {
    /// The segment does not start with the expected base type.
    #[error("expected a '{prefix}'-prefixed GTS identifier, got '{value}'")]
    Prefix {
        /// Expected base type prefix.
        prefix: String,
        /// The value as received.
        value: String,
    },
    /// The segment's tail is not a UUID.
    #[error("GTS identifier tail '{tail}' is not a UUID")]
    Tail {
        /// The tail as received.
        tail: String,
    },
}

/// Format a UUID as the instance identifier of `base_type`.
#[must_use]
pub fn format_id(base_type: &str, uuid: Uuid) -> String {
    format!("{base_type}{uuid}")
}

/// Parse a path segment into the base type and the instance UUID.
///
/// # Errors
/// Returns [`IdError`] when the segment is not of the expected shape.
pub fn parse_id(base_type: &str, value: &str) -> Result<Uuid, IdError> {
    let Some(tail) = value.strip_prefix(base_type) else {
        return Err(IdError::Prefix {
            prefix: base_type.to_owned(),
            value: value.to_owned(),
        });
    };
    Uuid::parse_str(tail).map_err(|_| IdError::Tail {
        tail: tail.to_owned(),
    })
}

/// Parse an upstream path segment.
///
/// # Errors
/// Returns [`IdError`] when the segment is malformed.
pub fn parse_upstream_id(value: &str) -> Result<Uuid, IdError> {
    parse_id(gts::UPSTREAM_TYPE, value)
}

/// Parse a route path segment.
///
/// # Errors
/// Returns [`IdError`] when the segment is malformed.
pub fn parse_route_id(value: &str) -> Result<Uuid, IdError> {
    parse_id(gts::ROUTE_TYPE, value)
}

/// Parse a plugin path segment, accepting any of the three plugin families.
///
/// # Errors
/// Returns [`IdError`] when the segment matches no plugin base type.
pub fn parse_plugin_id(value: &str) -> Result<Uuid, IdError> {
    for base in [
        gts::AUTH_PLUGIN_TYPE,
        gts::GUARD_PLUGIN_TYPE,
        gts::TRANSFORM_PLUGIN_TYPE,
    ] {
        if let Ok(id) = parse_id(base, value) {
            return Ok(id);
        }
    }
    Err(IdError::Prefix {
        prefix: "a plugin base type".to_owned(),
        value: value.to_owned(),
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::model::gts;

    #[test]
    fn round_trips_an_upstream_id() {
        let uuid = Uuid::new_v4();
        let formatted = format_id(gts::UPSTREAM_TYPE, uuid);
        assert_eq!(parse_upstream_id(&formatted).unwrap(), uuid);
    }

    #[test]
    fn rejects_a_foreign_prefix() {
        let err =
            parse_upstream_id("gts.cf.core.oagw.route.v1~00000000-0000-0000-0000-000000000001")
                .unwrap_err();
        assert!(matches!(err, IdError::Prefix { .. }));
    }

    #[test]
    fn rejects_a_non_uuid_tail() {
        let err = parse_upstream_id("not-an-id").unwrap_err();
        assert!(matches!(err, IdError::Prefix { .. }));
    }

    #[test]
    fn accepts_any_plugin_family() {
        let uuid = Uuid::new_v4();
        let guard = format_id(gts::GUARD_PLUGIN_TYPE, uuid);
        assert_eq!(parse_plugin_id(&guard).unwrap(), uuid);
        let custom = format!("gts.cf.core.oagw.transform_plugin.v1~{uuid}");
        assert_eq!(parse_plugin_id(&custom).unwrap(), uuid);
    }
}
