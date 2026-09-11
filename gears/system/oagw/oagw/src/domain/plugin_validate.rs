//! Field-level plugin create-request validation
//! (`cpt-cf-oagw-algo-plugin-create-validation`,
//! `cpt-cf-oagw-dod-plugin-config-schema-validation`).
//!
//! Parses a create request body against
//! [`crate::domain::model::PluginRequest`]. `serde`'s own deserialization
//! already rejects an unknown top-level property
//! (`#[serde(deny_unknown_fields)]`) and a `plugin_type` value outside
//! `auth`, `guard`, `transform` (an out-of-enum variant), since
//! [`crate::domain::model::PluginType`] is a closed `enum`. This module adds
//! the checks `serde` cannot express as types: non-empty `name`,
//! `config_schema` being a JSON object, and each declared `phases` entry
//! falling inside the set permitted for the declared `plugin_type`. Per-tenant
//! `name` uniqueness is a store-scoped check and lives in
//! `crate::domain::service`, alongside the equivalent alias-uniqueness check
//! for upstreams.
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use serde_json::Value;

use super::model::{Phase, PluginRequest};
use crate::error::OagwError;

/// Parses and validates a plugin create-request body
/// (`cpt-cf-oagw-algo-plugin-create-validation`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails to parse
/// against the schema mirror (including an out-of-enum `plugin_type`), `name`
/// is empty, `config_schema` is not a JSON object, or any declared `phases`
/// entry falls outside the set permitted for the declared `plugin_type`
/// (`cpt-cf-oagw-dod-plugin-config-schema-validation`).
// @cpt-begin:cpt-cf-oagw-algo-plugin-create-validation:p2:inst-plugin-createval-fn-01
pub fn parse_plugin_request(body: Value) -> Result<PluginRequest, OagwError> {
    let request: PluginRequest = serde_json::from_value(body).map_err(|error| {
        OagwError::validation_error(format!("request validation failed: {error}"))
    })?;

    let errors = validate_plugin_semantics(&request);
    if !errors.is_empty() {
        return Err(OagwError::validation_error(format!(
            "request validation failed: {}",
            errors.join("; ")
        )));
    }

    Ok(request)
}
// @cpt-end:cpt-cf-oagw-algo-plugin-create-validation:p2:inst-plugin-createval-fn-01

/// Collects every semantic violation `serde`'s type-level parsing cannot
/// express, returning an empty vector when the request is otherwise valid.
fn validate_plugin_semantics(request: &PluginRequest) -> Vec<String> {
    let mut errors = Vec::new();

    if request.name.trim().is_empty() {
        errors.push("name: must not be empty".to_owned());
    }

    if !request.config_schema.is_object() {
        errors.push("config_schema: must be a well-formed JSON Schema object".to_owned());
    }

    // @cpt-begin:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-plugin-phases-validate-01
    let permitted = request.plugin_type.permitted_phases();
    for phase in &request.phases {
        if !permitted.contains(phase) {
            errors.push(format!(
                "phases: '{}' is not permitted for plugin_type '{}'",
                phase_wire_str(*phase),
                request.plugin_type.wire_str()
            ));
        }
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-plugin-phases-validate-01

    errors
}

/// The wire-form spelling of a [`Phase`], used only to compose a readable
/// validation-error message.
const fn phase_wire_str(phase: Phase) -> &'static str {
    match phase {
        Phase::OnRequest => "on_request",
        Phase::OnResponse => "on_response",
        Phase::OnError => "on_error",
    }
}

#[cfg(test)]
mod tests {
    use super::parse_plugin_request;
    use serde_json::json;

    fn guard_request(phases: &[&str]) -> serde_json::Value {
        json!({
            "plugin_type": "guard",
            "name": "request_validator",
            "config_schema": {"type": "object"},
            "phases": phases,
            "source_code": "def on_request(ctx):\n    return ctx.next()",
        })
    }

    #[test]
    fn accepts_a_guard_plugin_declaring_permitted_phases() {
        let request =
            parse_plugin_request(guard_request(&["on_request", "on_response"])).expect("valid");
        assert_eq!(request.phases.len(), 2);
    }

    #[test]
    fn accepts_a_transform_plugin_declaring_all_three_phases() {
        let body = json!({
            "plugin_type": "transform",
            "name": "redact_pii",
            "config_schema": {"type": "object"},
            "phases": ["on_request", "on_response", "on_error"],
            "source_code": "def on_request(ctx):\n    return ctx.next()",
        });
        let request = parse_plugin_request(body).expect("valid");
        assert_eq!(request.phases.len(), 3);
    }

    // @cpt-begin:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-plugin-phases-auth-test-01
    #[test]
    fn rejects_an_auth_plugin_declaring_any_phase() {
        let body = json!({
            "plugin_type": "auth",
            "name": "custom_auth",
            "config_schema": {"type": "object"},
            "phases": ["on_request"],
            "source_code": "def authenticate(ctx):\n    return ctx.next()",
        });
        let error = parse_plugin_request(body).expect_err("auth permits no phases");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-plugin-phases-auth-test-01

    #[test]
    fn rejects_a_guard_plugin_declaring_on_error() {
        let error =
            parse_plugin_request(guard_request(&["on_error"])).expect_err("guard has no on_error");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    // @cpt-begin:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-plugin-schema-test-01
    #[test]
    fn rejects_a_non_object_config_schema() {
        let body = json!({
            "plugin_type": "guard",
            "name": "bad_schema",
            "config_schema": "not-an-object",
            "source_code": "def on_request(ctx):\n    return ctx.next()",
        });
        let error = parse_plugin_request(body).expect_err("config_schema must be an object");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-plugin-schema-test-01

    #[test]
    fn rejects_an_empty_name() {
        let body = json!({
            "plugin_type": "guard",
            "name": "",
            "config_schema": {"type": "object"},
            "source_code": "def on_request(ctx):\n    return ctx.next()",
        });
        assert!(parse_plugin_request(body).is_err());
    }

    // @cpt-begin:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-plugin-type-test-01
    #[test]
    fn rejects_a_plugin_type_outside_the_accepted_set() {
        let body = json!({
            "plugin_type": "logging",
            "name": "custom_logging",
            "config_schema": {"type": "object"},
            "source_code": "def on_request(ctx):\n    return ctx.next()",
        });
        let error = parse_plugin_request(body).expect_err("unknown plugin_type must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-plugin-type-test-01

    #[test]
    fn rejects_an_unknown_top_level_property() {
        let mut body = guard_request(&["on_request"]);
        body["unexpected"] = json!(true);
        assert!(parse_plugin_request(body).is_err());
    }

    #[test]
    fn rejects_a_missing_source_code_field() {
        let body = json!({
            "plugin_type": "guard",
            "name": "missing_source",
            "config_schema": {"type": "object"},
        });
        assert!(parse_plugin_request(body).is_err());
    }
}
