//! Gear-local list query parameters (`$filter`, `$select`, `$orderby`,
//! `$top`, `$skip`).
//!
//! Implemented in the gear rather than through the platform's
//! `toolkit_odata` extractor, whose parameter set rejects `$skip` — a
//! parameter the OAGW list contract documents (`DESIGN.md` §3.3 "List
//! Query Parameters").
use form_urlencoded::parse;
use serde::de::DeserializeOwned;

use crate::api::rest::error::ApiError;
use crate::domain::error::DomainError;
use crate::domain::query::{ListQuery, parse_filter, parse_orderby};
use crate::domain::services::ListLimits;

/// A parsed list request: the domain query plus the `$select` projection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedList {
    /// Filter / order / page request.
    pub query: ListQuery,
    /// `$select` field names, when the caller projected the result.
    pub select: Option<Vec<String>>,
}

impl ParsedList {
    /// Parse a raw query string (`?$filter=...&$top=5`), or an absent one
    /// into the default page.
    ///
    /// # Errors
    ///
    /// [`ApiError`] with status 400 for an unknown parameter key, a
    /// duplicate key, a non-numeric `$top` / `$skip`, or a malformed
    /// `$filter` / `$orderby`.
    pub fn parse(raw: Option<&str>, limits: ListLimits) -> Result<Self, ApiError> {
        let mut filter: Option<String> = None;
        let mut select: Option<String> = None;
        let mut orderby: Option<String> = None;
        let mut top: Option<usize> = None;
        let mut skip: Option<usize> = None;

        if let Some(raw) = raw {
            for (key, value) in parse(raw.as_bytes()) {
                let key = key.into_owned();
                let value = value.into_owned();
                match key.as_str() {
                    "$filter" => set_string(&mut filter, &key, &value)?,
                    "$select" => set_string(&mut select, &key, &value)?,
                    "$orderby" => set_string(&mut orderby, &key, &value)?,
                    "$top" => set_number(&mut top, &key, &value)?,
                    "$skip" => set_number(&mut skip, &key, &value)?,
                    other => {
                        return Err(ApiError::validation(format!(
                            "unknown list parameter '{other}'"
                        )));
                    }
                }
            }
        }

        if top.is_some_and(|top| top > limits.max_top) {
            return Err(ApiError::validation(format!(
                "$top must not exceed {}",
                limits.max_top
            )));
        }

        Ok(Self {
            query: ListQuery {
                filter: filter.as_deref().map(parse_filter).transpose()?,
                orderby: orderby
                    .as_deref()
                    .map(parse_orderby)
                    .transpose()?
                    .unwrap_or_default(),
                top: Some(top.unwrap_or(limits.default_top)),
                skip,
            },
            select: select.map(|raw| {
                raw.split(',')
                    .map(|field| field.trim().to_owned())
                    .filter(|field| !field.is_empty())
                    .collect::<Vec<String>>()
            }),
        })
    }
}

/// Assign a single-valued string parameter, rejecting duplicates.
fn set_string(slot: &mut Option<String>, key: &str, value: &str) -> Result<(), ApiError> {
    if slot.is_some() {
        return Err(ApiError::validation(format!(
            "duplicate list parameter '{key}'"
        )));
    }
    *slot = Some(value.to_owned());
    Ok(())
}

/// Assign a single-valued integer parameter, rejecting duplicates and
/// non-numeric values.
fn set_number(slot: &mut Option<usize>, key: &str, value: &str) -> Result<(), ApiError> {
    if slot.is_some() {
        return Err(ApiError::validation(format!(
            "duplicate list parameter '{key}'"
        )));
    }
    match value.parse::<usize>() {
        Ok(parsed) => {
            *slot = Some(parsed);
            Ok(())
        }
        Err(_) => Err(ApiError::validation(format!(
            "{key} must be a non-negative integer, got '{value}'"
        ))),
    }
}

/// Deserialize a wire policy block into a domain configuration type.
///
/// The policy blocks (`headers`, `rate_limit`, `cors`) cross the wire as JSON
/// objects and are validated by the domain model, so the DTO layer carries
/// them opaquely.
///
/// # Errors
///
/// [`DomainError`] when the block is present but not a valid `T`.
pub fn decode_policy<T: DeserializeOwned>(
    field: &'static str,
    value: &Option<serde_json::Value>,
) -> Result<Option<T>, DomainError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(raw) => serde_json::from_value(raw.clone())
            .map(Some)
            .map_err(|error| {
                DomainError::field(field, "config.invalid", format!("{field}: {error}"))
            }),
    }
}

/// Serialize a domain policy block back onto the wire.
///
/// # Errors
///
/// [`DomainError`] when the block cannot be serialized.
pub fn encode_policy<T: serde::Serialize>(
    value: &Option<T>,
) -> Result<Option<serde_json::Value>, DomainError> {
    match value {
        None => Ok(None),
        Some(block) => serde_json::to_value(block).map(Some).map_err(|error| {
            DomainError::internal(format!("policy serialization failed: {error}"))
        }),
    }
}
