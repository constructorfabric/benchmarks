//! Wire shapes for the OAGW management API.
//!
//! Write DTOs mirror the frozen JSON Schemas, with the deliberate scheme
//! widening recorded in the FEATURE documents. Read shapes are the domain
//! entities themselves, whose `tenant_id` is not serialized.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, Headers, PluginBindings, PluginKind, RateLimit, RouteMatch, Server,
};

/// Create or replace an upstream.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamWrite {
    /// Ignored on write; the server owns identifiers.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Whether the upstream serves traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing alias. Derived from the endpoint host when omitted.
    #[serde(default)]
    pub alias: Option<String>,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: Server,
    /// Application protocol identifier.
    pub protocol: String,
    /// Authentication configuration.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Headers,
    /// Guard and transform plugin bindings.
    #[serde(default)]
    pub plugins: PluginBindings,
    /// Rate-limit configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// Create a route.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCreate {
    /// Ignored on write.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Parent upstream.
    pub upstream_id: Uuid,
    /// Match criteria.
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    /// Guard and transform plugin bindings.
    #[serde(default)]
    pub plugins: PluginBindings,
    /// Rate-limit configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
}

/// Replace a route. `upstream_id` is immutable and therefore absent.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteReplace {
    /// Ignored on write.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Match criteria.
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    /// Guard and transform plugin bindings.
    #[serde(default)]
    pub plugins: PluginBindings,
    /// Rate-limit configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
}

/// Create a custom plugin definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCreate {
    /// Ignored on write.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Name, unique within the tenant.
    pub name: String,
    /// Optional description.
    #[serde(default)]
    pub description: String,
    /// Which phase the plugin participates in.
    pub plugin_type: PluginKind,
    /// Optional configuration schema.
    #[serde(default)]
    pub config_schema: serde_json::Value,
    /// Plugin source text.
    #[serde(default)]
    pub source_code: String,
}

/// A page of results.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    /// The items on this page.
    pub items: Vec<T>,
    /// How many items the tenant owns in total.
    pub total: usize,
}

/// The `referenced_by` body of a 409 `PluginInUse`.
#[derive(Debug, Clone, Serialize)]
pub struct ReferencedBy {
    /// Identifiers of referencing upstreams.
    pub upstreams: Vec<String>,
    /// Identifiers of referencing routes.
    pub routes: Vec<String>,
}

/// OData-style paging parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct ListParams {
    /// Page size. Defaults to 50, capped at 100.
    #[serde(rename = "$top", default)]
    pub top: Option<usize>,
    /// Offset into the collection.
    #[serde(rename = "$skip", default)]
    pub skip: Option<usize>,
}

/// Default page size.
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size.
pub const MAX_TOP: usize = 100;

impl ListParams {
    /// The effective page size, defaulted and capped.
    #[must_use]
    pub fn effective_top(&self) -> usize {
        self.top.unwrap_or(DEFAULT_TOP).clamp(1, MAX_TOP)
    }

    /// The effective offset.
    #[must_use]
    pub fn effective_skip(&self) -> usize {
        self.skip.unwrap_or(0)
    }

    /// Apply paging to a collection.
    #[must_use]
    pub fn paginate<T: Clone>(&self, all: &[T]) -> Page<T> {
        let items = all
            .iter()
            .skip(self.effective_skip())
            .take(self.effective_top())
            .cloned()
            .collect();
        Page {
            items,
            total: all.len(),
        }
    }
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_defaults_to_fifty_and_caps_at_one_hundred() {
        let p = ListParams {
            top: None,
            skip: None,
        };
        assert_eq!(p.effective_top(), 50);
        assert_eq!(p.effective_skip(), 0);

        let p = ListParams {
            top: Some(1000),
            skip: Some(3),
        };
        assert_eq!(p.effective_top(), 100);
        assert_eq!(p.effective_skip(), 3);
    }

    #[test]
    fn paging_slices_and_reports_the_full_total() {
        let all: Vec<u32> = (0..10).collect();
        let p = ListParams {
            top: Some(3),
            skip: Some(8),
        };
        let page = p.paginate(&all);
        assert_eq!(page.items, vec![8, 9]);
        assert_eq!(page.total, 10);
    }

    #[test]
    fn an_upstream_write_accepts_a_plaintext_endpoint() {
        let w: UpstreamWrite = serde_json::from_value(serde_json::json!({
            "server": {"endpoints": [{"scheme": "http", "host": "example.com", "port": 80}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }))
        .unwrap();
        assert!(w.enabled);
        assert!(w.alias.is_none());
        assert_eq!(w.server.endpoints.len(), 1);
    }

    #[test]
    fn an_upstream_write_rejects_an_unknown_field() {
        let r: Result<UpstreamWrite, _> = serde_json::from_value(serde_json::json!({
            "server": {"endpoints": [{"scheme": "http", "host": "e"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "surprise": 1
        }));
        assert!(r.is_err());
    }

    #[test]
    fn a_route_replace_has_no_upstream_id_field() {
        let r: Result<RouteReplace, _> = serde_json::from_value(serde_json::json!({
            "upstream_id": "00000000-0000-0000-0000-000000000000",
            "match": {"http": {"methods": ["GET"], "path": "/"}}
        }));
        assert!(r.is_err(), "upstream_id is immutable and must be rejected");
    }
}
