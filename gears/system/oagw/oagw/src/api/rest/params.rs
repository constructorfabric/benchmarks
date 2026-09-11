//! Request parameter binding — `cpt-cf-oagw-dod-list-query-parameters`.
//!
//! The `{id}` path parameter is accepted as the resource kind's anonymous GTS
//! instance (`gts.cf.core.oagw.upstream.v1~{uuid}`) or as a bare UUID, and the
//! list query string is handed to the list algorithm untouched: the five OData
//! parameters are the list algorithm's concern, the raw string this module's.

use axum::http::Uri;
use uuid::Uuid;

use crate::control_plane::scoping;
use crate::control_plane::validation::ResourceKind;
use crate::domain::error::DomainError;
use crate::domain::plugin_contract::PluginFamily;

/// Parses the `{id}` path parameter of one addressed operation.
///
/// # Errors
///
/// Returns the same 404 row a path-addressed miss answers with when the value
/// is neither the resource kind's anonymous GTS instance nor a bare UUID: an
/// identifier that cannot be parsed addresses nothing, and the answer discloses
/// no more than a miss does.
#[allow(clippy::result_large_err)]
pub fn path_id(kind: ResourceKind, value: &str) -> Result<Uuid, DomainError> {
    let prefix = match kind {
        ResourceKind::Upstream => crate::gts::UPSTREAM_TYPE,
        ResourceKind::Route => crate::gts::ROUTE_TYPE,
    };
    let parsed = crate::gts::parse_gts_instance(prefix, value);
    parsed.ok_or_else(scoping::path_miss)
}

/// The plugin selector one `{id}` path parameter carries.
///
/// Only a family's full anonymous GTS instance is accepted
/// (`gts.cf.core.oagw.{type}_plugin.v1~{uuid}`): a bare `Uuid` names no
/// permission arm, so it addresses nothing exactly as an unparseable value
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginSelector {
    /// The family the identifier's prefix names, when it names one at all.
    pub family: Option<PluginFamily>,
    /// The identifier the full form names, when the whole value parsed.
    pub id: Option<Uuid>,
}

impl PluginSelector {
    /// Parses one `{id}` path parameter into the family its prefix names and
    /// the identifier its full form names, independently of each other: an
    /// unparseable identifier still names the arm its prefix selected, and a
    /// value that names no family at all names no arm.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        const FAMILIES: [PluginFamily; 3] = [
            PluginFamily::Auth,
            PluginFamily::Guard,
            PluginFamily::Transform,
        ];
        for family in FAMILIES {
            if let Some(tail) = value.strip_prefix(family.base_type()) {
                return Self {
                    family: Some(family),
                    id: Uuid::parse_str(tail).ok(),
                };
            }
        }
        Self {
            family: None,
            id: None,
        }
    }

    /// The family and identifier the selector addresses, or the 404 row a
    /// path-addressed miss answers with when either is absent.
    ///
    /// # Errors
    ///
    /// Returns the same 404 row every path-addressed miss answers with.
    #[allow(clippy::result_large_err)]
    pub fn addressed(self) -> Result<(PluginFamily, Uuid), DomainError> {
        match (self.family, self.id) {
            (Some(family), Some(id)) => Ok((family, id)),
            _ => Err(scoping::path_miss()),
        }
    }
}

/// The raw query string of the request, without its leading `?`.
///
/// The string is bound to the list operation verbatim and never echoed into a
/// problem document: the `instance` a list refusal names is the request path
/// only, so nothing the query carried is reflected back to the caller.
#[must_use]
pub fn raw_query(uri: &Uri) -> &str {
    uri.query().unwrap_or_default()
}
