//! REST DTOs for the OAGW management API.
//!
//! The domain model doubles as the wire representation (DESIGN §3.1), so these
//! DTOs only add the wrapper shapes the API publishes: GTS identifiers in path
//! parameters, list envelopes and creation responses that echo the stored
//! resource with its server-generated identity.

use serde::Deserialize;
use uuid::Uuid;

use crate::domain::model::{Plugin, Route, Upstream};

// The domain model doubles as the wire representation (DESIGN §3.1), so the
// request/response DTO markers are implemented directly on it. The
// `#[toolkit_macros::api_dto]` macro cannot be used there: these types already
// derive `serde` (with field-level skip/default rules the wire contract depends
// on) and `utoipa::ToSchema`, which the macro would derive a second time.
impl toolkit::api::api_dto::RequestApiDto for Upstream {}
impl toolkit::api::api_dto::ResponseApiDto for Upstream {}
impl toolkit::api::api_dto::RequestApiDto for Route {}
impl toolkit::api::api_dto::ResponseApiDto for Route {}
impl toolkit::api::api_dto::RequestApiDto for Plugin {}
impl toolkit::api::api_dto::ResponseApiDto for Plugin {}

/// Query parameters of `GET /oagw/v1/routes`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListRoutesQuery {
    /// Restrict the listing to routes of this upstream.
    #[serde(default)]
    pub upstream_id: Option<Uuid>,
    /// Restrict the listing to this plugin type (`GET /oagw/v1/plugins`).
    #[serde(default)]
    pub plugin_type: Option<String>,
}

/// Envelope of `GET /oagw/v1/upstreams`.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamList {
    /// The caller's own upstreams.
    pub items: Vec<Upstream>,
}

/// Envelope of `GET /oagw/v1/routes`.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct RouteList {
    /// The caller's own routes.
    pub items: Vec<Route>,
}

/// Envelope of `GET /oagw/v1/plugins`.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct PluginList {
    /// The caller's own plugins.
    pub items: Vec<Plugin>,
}

/// Response of `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSource {
    /// Plugin the source belongs to.
    pub plugin_id: Uuid,
    /// Starlark source.
    pub source_code: String,
}

/// Extracts the UUID of a GTS path parameter (`gts.…~<uuid>`).
///
/// Accepts the bare UUID too, because path parameters arrive percent-decoded
/// and clients may quote only the instance part.
#[must_use]
pub fn parse_gts_path_id(raw: &str, base: &str) -> Option<Uuid> {
    let instance = raw.strip_prefix(base).unwrap_or(raw);
    uuid::Uuid::parse_str(instance).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gts_path_ids_accept_both_spellings() {
        let id = Uuid::new_v4();
        let raw = format!("{}{id}", crate::domain::model::gts::UPSTREAM);
        assert_eq!(
            parse_gts_path_id(&raw, crate::domain::model::gts::UPSTREAM),
            Some(id)
        );
        assert_eq!(
            parse_gts_path_id(&id.to_string(), crate::domain::model::gts::UPSTREAM),
            Some(id)
        );
        assert_eq!(
            parse_gts_path_id("not-a-uuid", crate::domain::model::gts::UPSTREAM),
            None
        );
    }
}
