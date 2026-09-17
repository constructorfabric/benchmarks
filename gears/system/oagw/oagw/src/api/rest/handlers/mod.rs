//! REST handlers of the `oagw` gear.
//!
//! Handlers are thin: they own the extractors (`Extension`, `Path`, `Query`,
//! `Json`), convert wire identifiers into bare UUIDs, and delegate to
//! [`crate::domain::services::ControlPlaneService`]. Errors travel as
//! [`OagwError`], which renders the gear's own RFC 9457 problem body.

pub mod plugins;
pub mod proxy;
pub mod routes;
pub mod upstreams;

use serde_json::Value;

use crate::api::rest::list::{ListParams, Page};
use crate::domain::error::{DomainError, ErrorKind};

/// Renders `rows` as the page envelope of a list endpoint.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for malformed list parameters and
/// [`ErrorKind::Internal`] when a row cannot be serialized.
pub(crate) fn list_page<D: serde::Serialize>(
    query: &std::collections::HashMap<String, String>,
    rows: Vec<D>,
) -> Result<Page<Value>, DomainError> {
    let params = ListParams::parse(query)
        .map_err(|detail| DomainError::new(ErrorKind::ValidationError, detail))?;
    let items: Result<Vec<Value>, DomainError> = rows
        .into_iter()
        .map(|row| {
            serde_json::to_value(row).map_err(|error| {
                DomainError::new(
                    ErrorKind::Internal,
                    format!("row serialization failed: {error}"),
                )
            })
        })
        .collect();
    Ok(params.page(params.apply(items?)))
}

/// Server-managed fields a create/replace body must never carry.
const SERVER_MANAGED_FIELDS: [&str; 2] = ["id", "tenant_id"];

/// Decodes a JSON body into a request DTO.
///
/// Malformed bodies are client errors, so they surface as
/// [`ErrorKind::ValidationError`] (400) rather than as an extractor rejection.
/// Server-managed fields (`id`, `tenant_id`) are rejected outright: the
/// specifications are flattened into the envelopes, so serde would otherwise
/// drop them silently.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] when the body carries a
/// server-managed field or does not match the DTO.
pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(body: Value) -> Result<T, DomainError> {
    if let Some(fields) = body.as_object() {
        for name in SERVER_MANAGED_FIELDS {
            if fields.contains_key(name) {
                return Err(DomainError::new(
                    ErrorKind::ValidationError,
                    format!("`{name}` is server managed and must not be sent"),
                ));
            }
        }
    }
    serde_json::from_value(body).map_err(|error| {
        DomainError::new(
            ErrorKind::ValidationError,
            format!("invalid request body: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn list_pages_reject_malformed_parameters() {
        let mut query = std::collections::HashMap::new();
        query.insert("$nonsense".to_owned(), "1".to_owned());
        let error = list_page::<Value>(&query, Vec::new()).expect_err("unknown parameter");
        assert_eq!(error.kind, ErrorKind::ValidationError);
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn list_pages_project_and_page_rows() {
        let mut query = std::collections::HashMap::new();
        query.insert("$select".to_owned(), "alias".to_owned());
        let page =
            list_page(&query, vec![json!({ "alias": "a.example.com", "id": "1" })]).expect("page");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0]["alias"], "a.example.com");
        assert_eq!(page.page_info.limit, 50);
    }

    #[test]
    fn bodies_with_server_managed_fields_are_validation_errors() {
        for name in ["id", "tenant_id"] {
            let mut body = serde_json::Map::new();
            body.insert(name.to_owned(), json!("cf.core.oagw.upstream.v1~abc"));
            body.insert("name".to_owned(), json!("plugin"));
            let error = parse_body::<crate::domain::model::PluginSpec>(Value::Object(body.clone()))
                .expect_err("server managed field");
            assert_eq!(error.kind, ErrorKind::ValidationError);
            assert_eq!(error.status(), 400);
            assert!(error.detail.contains(name), "{}", error.detail);
        }
    }

    #[test]
    fn malformed_bodies_are_validation_errors() {
        let error = parse_body::<crate::domain::model::PluginSpec>(json!({
            "name": 7, "plugin_type": "guard_plugin"
        }))
        .expect_err("wrong type");
        assert_eq!(error.kind, ErrorKind::ValidationError);
        assert_eq!(error.status(), 400);
    }
}
