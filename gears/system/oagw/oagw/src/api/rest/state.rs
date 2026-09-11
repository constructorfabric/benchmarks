//! Shared REST state and list-query parsing.

use std::sync::Arc;

use crate::config::OagwConfig;
use crate::infra::control_plane::OagwControlPlane;
use crate::infra::plugin::builtin_registries;
use crate::infra::proxy::ProxyEngine;
use crate::infra::secret::SecretResolver;

/// Everything the OAGW handlers need.
#[derive(Clone)]
pub struct OagwState {
    /// Control plane.
    pub control_plane: Arc<OagwControlPlane>,
    /// Data plane.
    pub engine: Arc<ProxyEngine>,
    /// Plugin registries, for the plugin catalogue endpoints.
    pub registries: Arc<crate::domain::plugin::PluginRegistries>,
}

/// Builds the plugin registries from the gear context.
///
/// # Errors
///
/// Returns an error when the credential-store client is unavailable.
pub fn build_registries(
    ctx: &toolkit::GearCtx,
    config: &OagwConfig,
) -> anyhow::Result<Arc<crate::domain::plugin::PluginRegistries>> {
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = ctx
        .client_hub()
        .get::<dyn credstore_sdk::CredStoreClientV1>()
        .map_err(|error| anyhow::anyhow!("oagw requires the credstore client: {error}"))?;
    let resolver = SecretResolver::new(credstore);
    Ok(Arc::new(builtin_registries(
        resolver,
        std::time::Duration::from_secs(config.token_cache_ttl_secs),
        config.token_cache_capacity,
    )))
}

/// Default page size for list endpoints.
pub const DEFAULT_TOP: usize = 50;

/// Maximum page size for list endpoints.
pub const MAX_TOP: usize = 100;

/// Parsed `OData`-style list query parameters.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Page size.
    pub top: usize,
    /// Number of entries skipped.
    pub skip: usize,
    /// Field to order by.
    pub orderby: Option<String>,
    /// Descending order.
    pub descending: bool,
    /// `field eq 'value'` filter.
    pub filter: Option<(String, String)>,
    /// Fields to project.
    pub select: Vec<String>,
}

impl ListQuery {
    /// Parses the query string.
    #[must_use]
    pub fn parse(query: &str) -> Self {
        let mut parsed = Self {
            top: DEFAULT_TOP,
            skip: 0,
            ..Self::default()
        };
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            let value = value.trim_matches('\'').to_owned();
            match key.as_ref() {
                "$top" => {
                    parsed.top = value
                        .parse()
                        .unwrap_or(DEFAULT_TOP)
                        .clamp(1, MAX_TOP);
                }
                "$skip" => {
                    parsed.skip = value.parse().unwrap_or(0);
                }
                "$orderby" => {
                    let mut parts = value.split_whitespace();
                    if let Some(field) = parts.next() {
                        parsed.orderby = Some(field.to_owned());
                        parsed.descending = parts.next().is_some_and(|direction| {
                            direction.eq_ignore_ascii_case("desc")
                        });
                    }
                }
                "$filter" => {
                    parsed.filter = parse_filter(&value);
                }
                "$select" => {
                    parsed.select = value
                        .split(',')
                        .map(str::trim)
                        .filter(|entry| !entry.is_empty())
                        .map(str::to_owned)
                        .collect();
                }
                _ => {}
            }
        }
        parsed
    }

    /// Applies paging, ordering and filtering to already-serialised rows.
    #[must_use]
    pub fn apply(&self, mut rows: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
        if let Some((field, value)) = &self.filter {
            let expected = value.trim_matches('\'');
            rows.retain(|row| {
                row.get(field)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|found| found == expected)
            });
        }
        if let Some(field) = &self.orderby {
            rows.sort_by(|left, right| {
                let key = |row: &serde_json::Value| {
                    row.get(field)
                        .map(serde_json::Value::to_string)
                        .unwrap_or_default()
                };
                let ordering = key(left).cmp(&key(right));
                if self.descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
        }
        let selected: Option<Vec<String>> = (!self.select.is_empty()).then(|| self.select.clone());
        rows.into_iter()
            .skip(self.skip)
            .take(self.top)
            .map(|row| match &selected {
                Some(fields) => project(&row, fields),
                None => row,
            })
            .collect()
    }
}

/// Parses a single `field eq 'value'` clause.
#[must_use]
pub fn parse_filter(value: &str) -> Option<(String, String)> {
    let (field, rest) = value.split_once("eq")?;
    Some((
        field.trim().to_owned(),
        rest.trim().trim_matches('\'').to_owned(),
    ))
}

/// Keeps only the requested members of an object row.
#[must_use]
fn project(row: &serde_json::Value, fields: &[String]) -> serde_json::Value {
    let Some(object) = row.as_object() else {
        return row.clone();
    };
    let projected = fields
        .iter()
        .filter_map(|field| object.get(field).map(|value| (field.clone(), value.clone())))
        .collect::<serde_json::Map<_, _>>();
    serde_json::Value::Object(projected)
}

/// Serialises rows to `JSON`.
#[must_use]
pub fn to_rows<T: serde::Serialize>(items: Vec<T>) -> Vec<serde_json::Value> {
    items
        .into_iter()
        .map(|item| serde_json::to_value(&item).unwrap_or_default())
        .collect()
}

