#![cfg(feature = "utoipa")]

use toolkit_canonical_errors::Problem;
use utoipa::{PartialSchema, ToSchema};

#[test]
fn problem_to_schema_reports_expected_name() {
    assert_eq!(<Problem as ToSchema>::name(), "Problem");
}

#[test]
fn problem_schema_contains_top_level_fields() {
    let schema = <Problem as PartialSchema>::schema();
    let json = serde_json::to_value(&schema).unwrap();

    let properties = json
        .get("properties")
        .expect("schema should have properties");

    for field in [
        "type", "title", "status", "detail", "instance", "trace_id", "context",
    ] {
        assert!(
            properties.get(field).is_some(),
            "missing property {field} in schema: {json}"
        );
    }
}

#[test]
fn problem_schema_marks_core_fields_required() {
    let schema = <Problem as PartialSchema>::schema();
    let json = serde_json::to_value(&schema).unwrap();

    let required = json
        .get("required")
        .and_then(|v| v.as_array())
        .expect("schema should declare required fields");
    let required: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

    for field in ["type", "title", "status", "detail", "context"] {
        assert!(
            required.contains(&field),
            "expected {field} to be required, required = {required:?}"
        );
    }
}

#[test]
fn problem_schema_types_status_as_integer() {
    let schema = <Problem as PartialSchema>::schema();
    let json = serde_json::to_value(&schema).unwrap();

    let status = &json["properties"]["status"];
    assert_eq!(status["type"], "integer");
}
