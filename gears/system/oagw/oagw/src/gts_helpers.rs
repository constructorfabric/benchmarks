//! GTS identifier helpers for the `oagw` gear.
//!
//! oagw resources are identified on the wire by
//! `gts.cf.core.oagw.{upstream,route,plugin}.v1~<uuid>`; the gear stores bare
//! UUIDs internally and converts at the REST boundary. Error `type` fields use
//! `gts.cf.core.errors.err.v1~cf.oagw.<error>.v1` (see
//! [`crate::domain::error::ErrorKind`]).

use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};

/// Namespace of the gear's own resource types (`cf.core.oagw`).
pub const OAGW_NAMESPACE: &str = "cf.core.oagw";

/// The three kinds of resource the management API exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OagwResourceKind {
    /// An upstream configuration.
    Upstream,
    /// A route configuration.
    Route,
    /// A plugin configuration.
    Plugin,
}

impl OagwResourceKind {
    /// GTS type id of the resource kind (trailing `~` included, `gts.` prefix
    /// omitted).
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::Upstream => "cf.core.oagw.upstream.v1~",
            Self::Route => "cf.core.oagw.route.v1~",
            Self::Plugin => "cf.core.oagw.plugin.v1~",
        }
    }

    /// Singular resource name used in error details.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Route => "route",
            Self::Plugin => "plugin",
        }
    }
}

/// Renders a bare resource UUID as its full GTS id.
#[must_use]
pub fn resource_id_to_gts(kind: OagwResourceKind, id: Uuid) -> String {
    format!("gts.{}{id}", kind.gts_type())
}

/// Instance part of a plugin binding item.
///
/// Plugin bindings carry either a full GTS id
/// (`gts.cf.core.oagw.guard_plugin.v1~<uuid>`) or a bare UUID; both reduce to
/// the same instance string, which is what the persistence layer keys plugins
/// by.
#[must_use]
pub fn plugin_instance(item: &str) -> String {
    item.rsplit('~').next().unwrap_or(item).to_owned()
}

/// Parses a path-parameter resource reference into the bare UUID.
///
/// Accepts the full GTS id (`gts.cf.core.oagw.upstream.v1~<uuid>`), the id
/// without the `gts.` prefix, and a bare UUID.
///
/// # Errors
/// Returns [`ErrorKind::ResourceNotFound`] when the reference is not a valid
/// id of `kind`, so that an unparseable path parameter surfaces as a 404
/// rather than a 500.
pub fn gts_to_resource_id(kind: OagwResourceKind, value: &str) -> Result<Uuid, DomainError> {
    let not_found = || {
        DomainError::new(
            ErrorKind::ResourceNotFound,
            format!("invalid {} id: {value}", kind.as_str()),
        )
    };
    let bare = value.strip_prefix("gts.").unwrap_or(value);
    if let Ok(id) = bare.parse::<Uuid>() {
        return Ok(id);
    }
    let instance = bare.strip_prefix(kind.gts_type()).ok_or_else(not_found)?;
    instance.parse::<Uuid>().map_err(|_| not_found())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000001");

    #[test]
    fn round_trips_resource_ids() {
        for kind in [
            OagwResourceKind::Upstream,
            OagwResourceKind::Route,
            OagwResourceKind::Plugin,
        ] {
            let gts = resource_id_to_gts(kind, ID);
            assert_eq!(gts, format!("gts.{}{ID}", kind.gts_type()));
            assert_eq!(gts_to_resource_id(kind, &gts).ok(), Some(ID));
            assert_eq!(
                gts_to_resource_id(kind, &format!("{}{ID}", kind.gts_type())).ok(),
                Some(ID)
            );
            assert_eq!(gts_to_resource_id(kind, &ID.to_string()).ok(), Some(ID));
        }
    }

    #[test]
    fn rejects_foreign_and_malformed_ids() {
        let err = gts_to_resource_id(
            OagwResourceKind::Upstream,
            &resource_id_to_gts(OagwResourceKind::Route, ID),
        );
        let err = match err {
            Ok(_) => panic!("foreign id must not parse"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ErrorKind::ResourceNotFound);
        assert_eq!(err.status(), 404);
        let err = match gts_to_resource_id(OagwResourceKind::Upstream, "not-an-id") {
            Ok(_) => panic!("malformed id must not parse"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ErrorKind::ResourceNotFound);
    }
}
