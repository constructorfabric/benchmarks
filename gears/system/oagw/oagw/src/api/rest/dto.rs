// Updated: 2026-09-01 by Constructor Tech
//! Wire DTOs.
//!
//! The domain models in [`crate::domain::dto`] are themselves API DTOs — the
//! `#[api_dto]` macro gives them the schema and the request/response markers —
//! so this module only adds the shapes the API has and the domain has not:
//! a paginated envelope and the OData query container.

use serde::{Deserialize, Serialize};

use toolkit::api::api_dto::{RequestApiDto, ResponseApiDto};

use crate::domain::dto::{Plugin, Route, Upstream};

/// A page of results, following the platform's `Page` shape.
///
/// Offset-based rather than cursor-based: the OData `$skip`/`$top` pair this
/// API's list contract uses is what the caller asks for, and a cursor the
/// caller cannot compute would not answer it.
///
/// The utoipa impls are hand-written rather than derived, for the same reason
/// `toolkit_odata::Page` carries them: the derive omits the generic parameter
/// from `schemas()`, leaving the item's `$ref` dangling, and it names every
/// instantiation `Page` — which collides with the platform's own `Page`
/// component. `Page_{item}` is the platform's own disambiguated form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

impl<T> utoipa::PartialSchema for Page<T>
where
    T: utoipa::ToSchema + utoipa::PartialSchema,
{
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder};

        ObjectBuilder::new()
            .property(
                "items",
                ArrayBuilder::new().items(utoipa::openapi::Ref::from_schema_name(
                    T::name().to_string(),
                )),
            )
            .required("items")
            .property(
                "total",
                utoipa::openapi::schema::Object::with_type(utoipa::openapi::schema::Type::Integer),
            )
            .required("total")
            .property(
                "offset",
                utoipa::openapi::schema::Object::with_type(utoipa::openapi::schema::Type::Integer),
            )
            .required("offset")
            .property(
                "limit",
                utoipa::openapi::schema::Object::with_type(utoipa::openapi::schema::Type::Integer),
            )
            .required("limit")
            .into()
    }
}

impl<T> utoipa::ToSchema for Page<T>
where
    T: utoipa::ToSchema + utoipa::PartialSchema,
{
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Owned(format!("Page_{}", T::name()))
    }

    fn schemas(
        schemas: &mut Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) {
        // The item type — what the derive omits for a generic parameter.
        schemas.push((
            <T as utoipa::ToSchema>::name().to_string(),
            <T as utoipa::PartialSchema>::schema(),
        ));
        <T as utoipa::ToSchema>::schemas(schemas);
    }
}

impl<T> Page<T> {
    #[must_use]
    pub fn new(items: Vec<T>, total: usize, offset: usize, limit: usize) -> Self {
        Self {
            items,
            total,
            offset,
            limit,
        }
    }

    /// Build a page from a slice already paginated by the caller.
    #[must_use]
    pub fn slice(all: &[T], offset: usize, limit: usize) -> Self
    where
        T: Clone,
    {
        Self {
            items: all.iter().skip(offset).take(limit).cloned().collect(),
            total: all.len(),
            offset,
            limit,
        }
    }
}

/// Query string for the list endpoints.
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub struct ListQuery {
    #[serde(rename = "$filter", default)]
    pub filter: Option<String>,
    #[serde(rename = "$select", default)]
    pub select: Option<String>,
    #[serde(rename = "$orderby", default)]
    pub orderby: Option<String>,
    #[serde(rename = "$top", default)]
    pub top: Option<usize>,
    #[serde(rename = "$skip", default)]
    pub skip: Option<usize>,
}

impl From<ListQuery> for crate::api::rest::odata::ListOptions {
    fn from(q: ListQuery) -> Self {
        Self {
            filter: q.filter,
            select: q.select,
            orderby: q.orderby,
            top: q.top,
            skip: q.skip,
        }
    }
}

/// Request body for creating a plugin.
#[toolkit_macros::api_dto(request)]
pub struct CreatePluginRequest {
    #[serde(rename = "type")]
    pub kind: crate::domain::dto::PluginKind,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub source: Option<String>,
}

/// A plugin as the wire sees it: the domain model plus its GTS instance id.
#[toolkit_macros::api_dto(response)]
pub struct PluginView {
    #[serde(flatten)]
    pub plugin: Plugin,
    pub gts_id: String,
}

impl PluginView {
    #[must_use]
    pub fn new(plugin: Plugin, kind: crate::domain::dto::PluginKind) -> Self {
        let id = plugin.id.unwrap_or_default();
        Self {
            gts_id: crate::gts::instance_id(kind.base_type(), id),
            plugin,
        }
    }
}

impl From<CreatePluginRequest> for Plugin {
    fn from(req: CreatePluginRequest) -> Self {
        Self {
            id: None,
            kind: req.kind,
            name: req.name,
            description: req.description,
            tags: req.tags,
            config: req.config,
            source: req.source,
        }
    }
}

/// An upstream as the wire sees it.
#[toolkit_macros::api_dto(response)]
pub struct UpstreamView {
    #[serde(flatten)]
    pub upstream: Upstream,
    pub gts_id: String,
}

impl UpstreamView {
    #[must_use]
    pub fn new(upstream: Upstream) -> Self {
        let id = upstream.id.unwrap_or_default();
        Self {
            gts_id: crate::gts::instance_id(crate::gts::UPSTREAM_TYPE, id),
            upstream,
        }
    }
}

/// A route as the wire sees it.
#[toolkit_macros::api_dto(response)]
pub struct RouteView {
    #[serde(flatten)]
    pub route: Route,
    pub gts_id: String,
}

impl RouteView {
    #[must_use]
    pub fn new(route: Route) -> Self {
        let id = route.id.unwrap_or_default();
        Self {
            gts_id: crate::gts::instance_id(crate::gts::ROUTE_TYPE, id),
            route,
        }
    }
}

/// The declaration a custom plugin was created with.
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceView {
    pub source: String,
}

impl<T> RequestApiDto for Page<T> {}
impl<T> ResponseApiDto for Page<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_query_accepts_the_odata_parameters() {
        let q: ListQuery = serde_json::from_value(serde_json::json!({
            "$filter": "tags eq 'ai'",
            "$top": 10,
            "$skip": 5,
            "$orderby": "alias desc",
        }))
        .unwrap();
        let o: crate::api::rest::odata::ListOptions = q.into();
        assert_eq!(o.tag_filter().as_deref(), Some("ai"));
        assert_eq!(o.limit(), 10);
        assert_eq!(o.offset(), 5);
        assert!(o.descending());
    }

    #[test]
    fn a_view_carries_the_gts_instance_id() {
        let u = Upstream {
            id: Some(uuid::Uuid::from_u128(0x1234)),
            ..Upstream::default()
        };
        let view = UpstreamView::new(u);
        assert!(view.gts_id.starts_with("gts.cf.core.oagw.upstream.v1~"));
        assert!(view.gts_id.ends_with("00000000000000000000000000001234"));
    }

    #[test]
    fn a_page_slices_correctly() {
        let items: Vec<u32> = (0..10).collect();
        let page = Page::slice(&items, 3, 4);
        assert_eq!(page.items, vec![3, 4, 5, 6]);
        assert_eq!(page.total, 10);
        assert_eq!(page.offset, 3);
        assert_eq!(page.limit, 4);
    }
}
