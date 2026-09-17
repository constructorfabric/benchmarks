//! Domain service operations (DDD-Light services over the repository ports).

pub mod proxy_service;
pub mod route_service;
pub mod upstream_service;

pub use proxy_service::ProxyService;
pub use route_service::RouteService;
pub use upstream_service::UpstreamService;

/// Shared list-query parameters (`$filter`/`$top`/`$skip`/`$orderby` subset).
///
/// `toolkit-odata` is not an `oagw` dependency, so the management API accepts
/// the OData query surface on this small, well-defined subset: equality
/// filters, `asc`/`desc` ordering on a single field, and paging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// `field eq 'value'` equality filters.
    pub filters: Vec<(String, String)>,
    /// Field to order by, with direction.
    pub order_by: Option<(String, bool)>,
    /// Page size (default 50, max 100).
    pub top: Option<usize>,
    /// Offset.
    pub skip: Option<usize>,
}

impl ListQuery {
    /// Value of an equality filter, when present.
    #[must_use]
    pub fn filter_value(&self, field: &str) -> Option<&str> {
        self.filters
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.as_str())
    }
}
