//! Route domain/DTO types, modeled against
//! `docs/schemas/route.v1.schema.json`.
//!
//! Extended by `cpt-cf-oagw-feature-route-management` (2.3) with the CRUD
//! surface's tenant-scoping field (`tenant_id`, a management-API-only
//! addition never part of the wire schema) and the two documented
//! schema-versus-domain-model discrepancies `enabled`/`priority`
//! (`cpt-cf-oagw-dod-route-schema-validation`).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::upstream::{RateLimitConfig, Sharing};

/// Anonymous GTS-identifier prefix for a Route resource
/// (`DESIGN.md`'s Resource Identification Pattern). The `{id}` path
/// parameter on `GET`/`PUT`/`DELETE /oagw/v1/routes/{id}` accepts a value in
/// this form as an alternative to the bare UUID `route.v1.schema.json`
/// declares for the `id` field
/// (`cpt-cf-oagw-algo-route-tenant-scope-resolve`).
pub const ROUTE_GTS_ID_PREFIX: &str = "gts.cf.core.oagw.route.v1~";

/// HTTP method allowlist entry for an HTTP route match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

/// How to treat `/{path_suffix}` from the proxy URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    Disabled,
    #[default]
    Append,
}

/// `match.http` sub-config (used when the upstream protocol is HTTP).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HttpMatch {
    #[serde(default)]
    pub methods: Vec<HttpMethod>,
    pub path: String,
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// `match.grpc` sub-config (used when the upstream protocol is gRPC).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// `match` sub-config: exactly one of `http`/`grpc` must be present
/// (enforced by `cpt-cf-oagw-feature-route-management`'s, 2.3, validation,
/// not by this skeleton type).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct RouteMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// `plugins` sub-config: route-level plugin chain binding.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct RoutePluginsBinding {
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub items: Vec<String>,
}

fn default_route_enabled() -> bool {
    true
}

/// Route configuration resource (`gts.cf.core.oagw.route.v1~`), belonging
/// to an Upstream.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant. A management-API-only addition: never part of
    /// `route.v1.schema.json`'s wire shape (never read from the request
    /// body and never serialized back to the client) -- the handler stamps
    /// it from the calling `SecurityContext`
    /// (`cpt-cf-oagw-algo-route-tenant-scope-resolve`).
    #[serde(default, skip_serializing, skip_deserializing)]
    pub tenant_id: Uuid,
    #[serde(default)]
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub route_match: RouteMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<RoutePluginsBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Schema-versus-domain-model discrepancy #1
    /// (`cpt-cf-oagw-dod-route-schema-validation`): not enumerated among
    /// `route.v1.schema.json`'s excerpted top-level properties, but legal
    /// under the root object's default permissiveness and required by the
    /// Route domain-model entity's `+Boolean enabled`. Defaults to `true`.
    // @cpt-dod:cpt-cf-oagw-dod-route-enable-disable:p1
    #[serde(default = "default_route_enabled")]
    pub enabled: bool,
    /// Schema-versus-domain-model discrepancy #2: the same kind of
    /// addition as `enabled` above, for the Route domain-model entity's
    /// `+Int priority`. Required alongside `match.http` (it participates in
    /// the `(path, priority, method)` uniqueness key); meaningless for a
    /// `match.grpc`-only route, so left absent (`None`) there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
}

impl Route {
    /// Normalize an `{id}` path parameter to its bare-UUID form, accepting
    /// either the bare UUID `route.v1.schema.json` declares for `id`, or the
    /// anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}` `DESIGN.md`'s
    /// Resource Identification Pattern documents for path parameters
    /// (`cpt-cf-oagw-algo-route-tenant-scope-resolve` step
    /// `inst-tenantscope-query`).
    #[must_use]
    pub fn normalize_id_param(raw: &str) -> Option<Uuid> {
        let candidate = raw.strip_prefix(ROUTE_GTS_ID_PREFIX).unwrap_or(raw);
        Uuid::parse_str(candidate).ok()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_a_minimal_http_route() {
        let upstream_id = Uuid::new_v4();
        let json = serde_json::json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
        });
        let route: Route = serde_json::from_value(json).unwrap();
        assert_eq!(route.upstream_id, upstream_id);
        assert!(route.route_match.http.is_some());
        assert!(route.route_match.grpc.is_none());
    }

    #[test]
    fn enabled_defaults_to_true_when_omitted() {
        let json = serde_json::json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
        });
        let route: Route = serde_json::from_value(json).unwrap();
        assert!(route.enabled);
    }

    #[test]
    fn tenant_id_is_never_read_from_or_serialized_to_the_wire() {
        let json = serde_json::json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
            "tenant_id": Uuid::new_v4(),
        });
        let route: Route = serde_json::from_value(json).unwrap();
        assert_eq!(route.tenant_id, Uuid::nil());

        let mut stamped = route;
        stamped.tenant_id = Uuid::new_v4();
        stamped.id = Some(Uuid::new_v4());
        let value = serde_json::to_value(&stamped).unwrap();
        assert!(value.get("tenant_id").is_none());
    }

    #[test]
    fn normalize_id_param_accepts_a_bare_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(Route::normalize_id_param(&id.to_string()), Some(id));
    }

    #[test]
    fn normalize_id_param_accepts_the_anonymous_gts_form() {
        let id = Uuid::new_v4();
        let gts_form = format!("{ROUTE_GTS_ID_PREFIX}{id}");
        assert_eq!(Route::normalize_id_param(&gts_form), Some(id));
    }

    #[test]
    fn normalize_id_param_rejects_garbage() {
        assert_eq!(Route::normalize_id_param("not-a-uuid"), None);
    }
}
