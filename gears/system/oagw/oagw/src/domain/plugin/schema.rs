//! Binding-time instance-configuration validation against a registered
//! `config_schema` (`inst-ps-bind-14`/`-15`).
//!
//! The validator implements the documented JSON-Schema subset the OAGW plugin
//! catalog is written in — `type`, `properties`, `required`,
//! `additionalProperties: false`, and the two OAGW key-relationship
//! annotations — and nothing more. It exists so a binding whose instance
//! configuration does not validate against the resolved plugin's registered
//! schema is rejected with `400`
//! `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` **before** any
//! binding row is stored, so no invalid binding is ever persisted.
//!
//! The validator never echoes a rejected *value*: the rejection names the
//! offending key path only, honouring
//! `cpt-cf-oagw-algo-gear-foundation-credential-boundary`.

use serde_json::Value;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::repo::PluginRepository;

/// The schema annotations OAGW adds on top of the JSON-Schema subset.
pub const MUTUALLY_EXCLUSIVE: &str = "x-oagw-mutually-exclusive";
pub const REQUIRED_ALTERNATIVE: &str = "x-oagw-required-alternative";

/// Validate `config` against `schema`.
///
/// A `None` schema carries no constraint and validates anything, which is how
/// a custom plugin registered without a `config_schema` behaves.
///
/// # Errors
///
/// Returns a validation error carrying the offending field path inside the
/// binding, and never the rejected value.
pub fn validate_instance_config(
    field: &str,
    schema: Option<&Value>,
    config: Option<&Value>,
) -> Result<(), DomainError> {
    let Some(schema) = schema else {
        return Ok(());
    };
    // An absent configuration is validated as an empty object, so `required`
    // keys such as `client_id_ref` are still enforced.
    let empty = Value::Object(serde_json::Map::new());
    let config = config.unwrap_or(&empty);
    let Value::Object(object) = config else {
        return Err(reject(field, "must be a JSON object"));
    };
    check_object(field, schema, object)
}

/// The registered `config_schema` of one plugin reference
/// (`inst-ps-bind-14`).
///
/// A built-in identifier resolves through the plugin catalog the foundation
/// entry provisioned; a UUID-backed one resolves through the calling tenant's
/// own plugin row, because the management surface is strictly caller-scoped. A
/// reference that resolves to neither carries no schema and so constrains
/// nothing.
#[must_use]
pub fn config_schema_of(
    plugins: &dyn PluginRepository,
    tenant_id: Uuid,
    reference: &str,
) -> Option<Value> {
    if let Some(schema) = crate::domain::type_catalog::builtin_config_schema(reference) {
        return Some(schema);
    }
    let uuid = match crate::domain::plugin::identifier::parse_instance(reference) {
        crate::domain::plugin::identifier::PluginInstance::Uuid(uuid) => uuid,
        crate::domain::plugin::identifier::PluginInstance::Named(_) => return None,
    };
    plugins
        .get(tenant_id, uuid)
        .ok()
        .and_then(|record| record.config_schema)
}

fn check_object(field: &str, schema: &Value, object: &serde_json::Map<String, Value>) -> Result<(), DomainError> {
    if schema.get("type").and_then(Value::as_str).is_some_and(|kind| kind != "object") {
        return Err(reject(field, "must be a JSON object"));
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    // `additionalProperties: false` alongside a `properties` map makes every
    // key outside that set an unknown key.
    let closed = properties.is_some()
        && schema.get("additionalProperties").and_then(Value::as_bool) == Some(false);
    if let Some(properties) = properties {
        if closed {
            for key in object.keys() {
                if !properties.contains_key(key) {
                    return Err(reject(
                        field,
                        &format!("carries the unknown key `{key}`"),
                    ));
                }
            }
        }
        for (key, value) in object {
            if let Some(property) = properties.get(key) {
                check_value(&format!("{field}.{key}"), property, value)?;
            }
        }
    }
    for key in required_keys(schema) {
        if !object.contains_key(key) {
            return Err(reject(
                field,
                &format!("is missing the required key `{key}`"),
            ));
        }
    }
    for pair in alternative_groups(schema, MUTUALLY_EXCLUSIVE) {
        let present: Vec<&str> =
            pair.iter().filter(|key| object.contains_key(*key)).map(String::as_str).collect();
        if present.len() > 1 {
            return Err(reject(
                field,
                &format!(
                    "supplies the mutually exclusive keys `{}` together",
                    pair.join("`, `")
                ),
            ));
        }
    }
    for pair in alternative_groups(schema, REQUIRED_ALTERNATIVE) {
        let present = pair.iter().filter(|key| object.contains_key(*key)).count();
        if present != 1 {
            return Err(reject(
                field,
                &format!(
                    "must supply exactly one of the keys `{}`",
                    pair.join("`, `")
                ),
            ));
        }
    }
    Ok(())
}

fn check_value(field: &str, property: &Value, value: &Value) -> Result<(), DomainError> {
    if property.get("nullable").and_then(Value::as_bool) == Some(true)
        && value.is_null()
    {
        return Ok(());
    }
    let kind = property.get("type").and_then(Value::as_str);
    match kind {
        Some("string") => {
            let Value::String(text) = value else {
                return Err(reject(field, "must be a string"));
            };
            if property.get("format").and_then(Value::as_str) == Some("cred-reference")
                && !crate::domain::dto::is_cred_reference(text)
            {
                return Err(reject(
                    field,
                    "must hold a `cred://` reference, never secret material",
                ));
            }
        }
        Some("boolean") if !value.is_boolean() => return Err(reject(field, "must be a boolean")),
        Some("integer") if !value.is_i64() && !value.is_u64() => {
            return Err(reject(field, "must be an integer"))
        }
        Some("number") if !value.is_number() => return Err(reject(field, "must be a number")),
        Some("array") => {
            let Value::Array(items) = value else {
                return Err(reject(field, "must be an array"));
            };
            if let Some(minimum) = property.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < minimum {
                    return Err(reject(field, "must carry at least one entry"));
                }
            }
            let item_schema = property.get("items");
            let expects_string = item_schema
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "string");
            if expects_string {
                for (index, item) in items.iter().enumerate() {
                    let entry = format!("{field}[{index}]");
                    let Value::String(text) = item else {
                        return Err(reject(&entry, "must be a string"));
                    };
                    // A required-header list whose entries are all blank
                    // selects nothing; the binding is rejected rather than
                    // persisted as a guard that can never fire.
                    if text.trim().is_empty() {
                        return Err(reject(
                            &entry,
                            "must not be blank",
                        ));
                    }
                }
            }
        }
        Some("object") => {
            let Value::Object(nested) = value else {
                return Err(reject(field, "must be a JSON object"));
            };
            check_object(field, property, nested)?;
        }
        _ => {}
    }
    Ok(())
}

fn required_keys(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn alternative_groups(schema: &Value, annotation: &str) -> Vec<Vec<String>> {
    schema
        .get(annotation)
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .filter_map(Value::as_array)
                .map(|pair| {
                    pair.iter().filter_map(Value::as_str).map(str::to_owned).collect()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The binding-time rejection. It names the offending key path and never the
/// rejected value, so no secret material can reach the rendered body.
fn reject(field: &str, reason: &str) -> DomainError {
    DomainError::ValidationError {
        detail: format!("field `{field}` rejected: {reason}"),
        path: Some(field.to_owned()),
        trace_id: None,
    }
}
