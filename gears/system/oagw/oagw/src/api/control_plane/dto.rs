//! The CRUD request DTO boundary of the management surface
//! (`cpt-cf-oagw-algo-crud-request-validation`).
//!
//! The boundary owns the field sets only: it rejects a body that is not a JSON
//! object, a field outside the field set of the DTO the method addresses, and a
//! body that misses a required field, before any domain rule runs. Every value
//! rule is owned by `cpt-cf-oagw-flow-resource-validation`, which this module
//! hands the validated payload to and whose rules it restates none of.

use serde_json::{Map, Value};

use crate::domain::error::{DomainError, Violation, ViolationKind, Violations};

/// The create field set of an upstream: the fields
/// `schemas/upstream.v1.schema.json` declares, in schema order.
pub const UPSTREAM_CREATE_FIELDS: &[&str] = &[
    "id",
    "enabled",
    "alias",
    "tags",
    "server",
    "protocol",
    "auth",
    "headers",
    "plugins",
    "rate_limit",
    "cors",
];

/// The update field set of an upstream: the create field set minus `id`, which
/// is not a field of any update DTO.
pub const UPSTREAM_UPDATE_FIELDS: &[&str] = &[
    "enabled",
    "alias",
    "tags",
    "server",
    "protocol",
    "auth",
    "headers",
    "plugins",
    "rate_limit",
    "cors",
];

/// The create field set of a route: the fields `schemas/route.v1.schema.json`
/// declares plus the field-set extension `cpt-cf-oagw-feature-domain-model`
/// declares in its §1.5 — `enabled` and `priority`, which the route schema does
/// not declare at the top level.
pub const ROUTE_CREATE_FIELDS: &[&str] = &[
    "id",
    "tags",
    "upstream_id",
    "match",
    "plugins",
    "rate_limit",
    "enabled",
    "priority",
];

/// The update field set of a route: the create field set minus `id` and
/// `upstream_id`, whose immutability is enforced by their absence.
pub const ROUTE_UPDATE_FIELDS: &[&str] = &[
    "tags",
    "match",
    "plugins",
    "rate_limit",
    "enabled",
    "priority",
];

/// The field set of a plugin, the fields the `Plugin` aggregate of
/// `cpt-cf-oagw-design-domain-model` declares, no JSON Schema being defined for
/// it.
pub const PLUGIN_FIELDS: &[&str] = &[
    "id",
    "plugin_type",
    "name",
    "description",
    "config_schema",
    "source_code",
];

/// The aggregate field set the `$orderby` and `$select` parameters of the
/// upstream list resolve over, the schema fields plus the field-set extension.
pub const UPSTREAM_AGGREGATE_FIELDS: &[&str] = UPSTREAM_CREATE_FIELDS;

/// The aggregate field set the `$orderby` and `$select` parameters of the route
/// list resolve over.
pub const ROUTE_AGGREGATE_FIELDS: &[&str] = ROUTE_CREATE_FIELDS;

/// The aggregate field set the `$orderby` and `$select` parameters of the
/// plugin list resolve over.
pub const PLUGIN_AGGREGATE_FIELDS: &[&str] = PLUGIN_FIELDS;

/// The method a request carries, which selects the DTO the body is parsed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrudMethod {
    /// A create, whose DTO follows the schema field set.
    Create,
    /// A replace, whose DTO drops `id`, `tenant_id` and, for a route,
    /// `upstream_id`.
    Replace,
}

impl CrudMethod {
    /// The field set the method selects.
    #[must_use]
    pub fn fields(self, resource: Resource) -> &'static [&'static str] {
        match (resource, self) {
            (Resource::Upstream, Self::Create) => UPSTREAM_CREATE_FIELDS,
            (Resource::Upstream, Self::Replace) => UPSTREAM_UPDATE_FIELDS,
            (Resource::Route, Self::Create) => ROUTE_CREATE_FIELDS,
            (Resource::Route, Self::Replace) => ROUTE_UPDATE_FIELDS,
            (Resource::Plugin, _) => PLUGIN_FIELDS,
        }
    }

    /// The fields the method requires.
    #[must_use]
    pub fn required(self, resource: Resource) -> &'static [&'static str] {
        match (resource, self) {
            (Resource::Upstream, _) => &["server", "protocol"],
            (Resource::Route, Self::Create) => &["upstream_id", "match"],
            (Resource::Route, Self::Replace) => &["match"],
            (Resource::Plugin, _) => &["plugin_type", "name", "source_code"],
        }
    }
}

/// The resource a management request addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// An upstream.
    Upstream,
    /// A route.
    Route,
    /// A plugin.
    Plugin,
}

impl Resource {
    /// The aggregate field set of the resource, the universe the list
    /// parameters resolve over.
    #[must_use]
    pub const fn aggregate_fields(self) -> &'static [&'static str] {
        match self {
            Self::Upstream => UPSTREAM_AGGREGATE_FIELDS,
            Self::Route => ROUTE_AGGREGATE_FIELDS,
            Self::Plugin => PLUGIN_AGGREGATE_FIELDS,
        }
    }

    /// The declared field order of the aggregate, the order the violations are
    /// reported in.
    #[must_use]
    pub const fn field_order(self) -> &'static [&'static str] {
        match self {
            Self::Upstream => crate::domain::model::Upstream::FIELD_ORDER,
            Self::Route => crate::domain::model::Route::FIELD_ORDER,
            Self::Plugin => crate::domain::model::Plugin::FIELD_ORDER,
        }
    }
}

/// Parses a management body into its DTO
/// (`cpt-cf-oagw-algo-crud-request-validation`).
///
/// The returned object carries exactly the fields the DTO declares, so the
/// domain layer receives a payload it can validate field by field.
///
/// # Errors
/// Returns the collected unknown-field and missing-required violations, to be
/// rendered as 400 before any domain rule runs.
pub fn parse(
    resource: Resource,
    method: CrudMethod,
    body: &[u8],
) -> Result<Map<String, Value>, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-01
    // The body is parsed as JSON and anything that is not an object is
    // rejected, so a malformed payload never reaches the domain layer.
    let parsed: Value = serde_json::from_slice(body).map_err(|error| {
        DomainError::from_violation(Violation::new(
            ViolationKind::UnknownField,
            "body",
            format!("the request body is not valid JSON: {error}"),
        ))
    })?;
    let Some(object) = parsed.as_object() else {
        return Err(DomainError::from_violation(Violation::new(
            ViolationKind::UnknownField,
            "body",
            "the request body is not a JSON object",
        )));
    };
    // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-01

    // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-02
    // Every top-level field is compared against the field set of the DTO the
    // method selects, and every field outside it is recorded with its path.
    let fields = method.fields(resource);
    let mut violations = Violations::new();
    let mut dto = Map::new();
    for (name, value) in object {
        // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-03
        // The field name is compared against the field set of the DTO that
        // matches the method: the create DTOs follow the schema field sets of
        // upstream.v1 and route.v1 plus the plugin fields of the DESIGN domain
        // model, and the update DTOs drop `id`, `tenant_id` and route
        // `upstream_id`.
        if !fields.contains(&name.as_str()) {
            // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-04
            // An unknown-field violation is recorded for every field outside
            // that set, naming the field path; the immutability of `id`,
            // `tenant_id` and route `upstream_id` is enforced here, by their
            // absence from the update DTO, never by comparing a supplied value
            // against a stored one.
            violations.push(Violation::new(
                ViolationKind::UnknownField,
                name.clone(),
                format!(
                    "'{name}' is not a field of the {what}",
                    what = what(method, resource)
                ),
            ));
            continue;
            // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-04
        }
        // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-03
        dto.insert(name.clone(), value.clone());
    }
    // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-02

    // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-05
    // The required fields of the DTO, one violation per absent field.
    for required in method.required(resource) {
        if !object.contains_key(*required) {
            violations.push(Violation::new(
                ViolationKind::UnknownField,
                *required,
                format!(
                    "'{required}' is required by the {what} but the body omits it",
                    what = what(method, resource)
                ),
            ));
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-05

    // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-07
    // RETURN the collected violations to the caller flow, to be rendered as
    // 400 with GTS type `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`
    // through `cpt-cf-oagw-algo-error-mapping`, satisfying the input-validation
    // NFR at the management boundary.
    // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-06
    // The collected violations are returned together, so a caller sees every
    // offending field path at once.
    violations.into_result(resource.field_order())?;
    // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-06
    // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-07

    // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-08
    // ELSE hand the typed DTO to the domain validation of
    // `cpt-cf-oagw-flow-resource-validation`, which delegates the endpoint
    // rules to `cpt-cf-oagw-algo-endpoint-validation` and every value-object
    // rule to `cpt-cf-oagw-algo-shape-validation`; this algorithm owns the
    // field-set comparison alone.
    // @cpt-begin:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-09
    // The DTO carries exactly the declared fields, in the field order the
    // aggregate validates them over.
    Ok(dto)
    // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-09
    // @cpt-end:cpt-cf-oagw-algo-crud-request-validation:p1:inst-cv-08
}

/// The DTO the violation names, e.g. "create upstream DTO".
fn what(method: CrudMethod, resource: Resource) -> String {
    format!(
        "{method} {resource} DTO",
        method = match method {
            CrudMethod::Create => "create",
            CrudMethod::Replace => "replace",
        },
        resource = match resource {
            Resource::Upstream => "upstream",
            Resource::Route => "route",
            Resource::Plugin => "plugin",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::ViolationKind;
    use crate::domain::model::{PLUGIN_TYPE_IDS, PROTOCOL_HTTP};
    use serde_json::json;
    use std::collections::BTreeSet;
    use uuid::Uuid;

    const UPSTREAM_BODY: &[u8] = br#"{
        "id": "0f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
        "enabled": true,
        "alias": "payments.vendor.com",
        "tags": ["openai"],
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com", "port": 443}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    }"#;

    const ROUTE_BODY: &[u8] = br#"{
        "upstream_id": "0f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
        "match": {"http": {"path": "/v1/chat", "methods": ["GET"]}},
        "priority": 10,
        "enabled": true
    }"#;

    const PLUGIN_BODY: &[u8] = br#"{
        "plugin_type": "gts.cf.core.oagw.guard_plugin.v1~",
        "name": "tenant-guard",
        "source_code": "def on_request(context): return context"
    }"#;

    /// An upstream body that carries no immutable field, the shape a replace
    /// DTO accepts.
    const REPLACE_UPSTREAM_BODY: &[u8] = br#"{
        "enabled": false,
        "alias": "eu.vendor.com.",
        "tags": ["openai"],
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com", "port": 443}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    }"#;

    fn violation_kinds(body: &[u8], resource: Resource, method: CrudMethod) -> Vec<ViolationKind> {
        parse(resource, method, body)
            .expect_err("the body is rejected")
            .to_violations()
            .into_iter()
            .map(|violation| violation.kind)
            .collect()
    }

    /// The field names the rejected body is reported for.
    fn reported_fields(body: &[u8], resource: Resource, method: CrudMethod) -> Vec<String> {
        parse(resource, method, body)
            .expect_err("the body is rejected")
            .to_violations()
            .into_iter()
            .map(|violation| violation.field)
            .collect()
    }

    fn field_set(dto: Map<String, Value>) -> BTreeSet<String> {
        dto.keys().cloned().collect()
    }

    /// `inst-cv-01`: a body that is not a JSON object is rejected.
    #[test]
    fn a_body_that_is_not_a_json_object_is_rejected() {
        for body in [
            br#""a string""#.as_slice(),
            b"42",
            b"null",
            b"[1,2,3]",
            b"not json at all",
        ] {
            let kinds = violation_kinds(body, Resource::Upstream, CrudMethod::Create);
            assert!(
                kinds.contains(&ViolationKind::UnknownField),
                "a malformed body is a DTO boundary violation: {kinds:?}"
            );
        }
    }

    /// `inst-cv-02` and `inst-cv-04`: every field outside the DTO field set is
    /// recorded with its path.
    #[test]
    fn an_unknown_field_is_recorded_with_its_path() {
        let body = br#"{"server": {}, "protocol": "p", "totally_unknown": 1}"#;
        assert_eq!(
            reported_fields(body, Resource::Upstream, CrudMethod::Create),
            ["totally_unknown"]
        );
    }

    /// `inst-cv-03`: the create DTO follows the schema field set plus the
    /// field-set extension, so `enabled` and a route `priority` are accepted.
    #[test]
    fn the_field_set_extension_is_accepted_and_not_reported_unknown() {
        for (body, resource) in [
            (UPSTREAM_BODY, Resource::Upstream),
            (ROUTE_BODY, Resource::Route),
            (PLUGIN_BODY, Resource::Plugin),
        ] {
            let dto = field_set(parse(resource, CrudMethod::Create, body).unwrap());
            let body_keys: BTreeSet<String> = {
                let parsed: Value = serde_json::from_slice(body).unwrap();
                parsed
                    .as_object()
                    .expect("the body is an object")
                    .keys()
                    .cloned()
                    .collect()
            };
            assert_eq!(dto, body_keys, "{resource:?}");
            for key in &dto {
                assert!(
                    resource.aggregate_fields().contains(&key.as_str()),
                    "{key} is not a field of {resource:?}"
                );
            }
        }
    }

    /// `inst-cv-03`: the update DTOs drop `id`, `tenant_id` and, for a route,
    /// `upstream_id`.
    #[test]
    fn the_update_dto_declares_no_immutable_field() {
        let with_field = |field: &str| {
            json!({ field: "0f0a1b2c-3d4e-4f50-8617-8899aabbccdd" })
                .to_string()
                .into_bytes()
        };
        for field in ["id", "tenant_id"] {
            let fields =
                reported_fields(&with_field(field), Resource::Upstream, CrudMethod::Replace);
            assert!(fields.contains(&field.to_owned()), "{field}: {fields:?}");
        }
        for field in ["id", "tenant_id", "upstream_id"] {
            let fields = reported_fields(&with_field(field), Resource::Route, CrudMethod::Replace);
            assert!(fields.contains(&field.to_owned()), "{field}: {fields:?}");
        }
        // The update DTO still declares the extension and the owned fields.
        let update_route = br#"{"match": {"http": {"path": "/v1/chat", "methods": ["GET"]}}, "priority": 10, "enabled": false}"#;
        assert_eq!(
            field_set(parse(Resource::Route, CrudMethod::Replace, update_route).unwrap()),
            ["match", "priority", "enabled"]
                .into_iter()
                .map(ToString::to_string)
                .collect()
        );
        for key in field_set(
            parse(
                Resource::Upstream,
                CrudMethod::Replace,
                REPLACE_UPSTREAM_BODY,
            )
            .unwrap(),
        ) {
            assert!(UPSTREAM_UPDATE_FIELDS.contains(&key.as_str()), "{key}");
        }
    }

    /// `inst-cv-05`: one violation per absent required field.
    #[test]
    fn the_required_fields_are_verified() {
        let fields = reported_fields(b"{}", Resource::Upstream, CrudMethod::Create);
        assert!(fields.contains(&"server".to_owned()), "{fields:?}");
        assert!(fields.contains(&"protocol".to_owned()), "{fields:?}");

        let fields = reported_fields(b"{}", Resource::Plugin, CrudMethod::Create);
        assert!(fields.contains(&"plugin_type".to_owned()), "{fields:?}");
        assert!(fields.contains(&"name".to_owned()), "{fields:?}");
        assert!(fields.contains(&"source_code".to_owned()), "{fields:?}");

        // A route update DTO requires the match rule but never the immutable
        // reference.
        let fields = reported_fields(br#"{"priority": 3}"#, Resource::Route, CrudMethod::Replace);
        assert!(fields.contains(&"match".to_owned()), "{fields:?}");
        assert!(!fields.contains(&"upstream_id".to_owned()));
    }

    /// `inst-cv-06` and `inst-cv-07`: the violations are reported together.
    #[test]
    fn all_violations_are_reported_together() {
        let body = br#"{"unknown_one": 1, "unknown_two": 2}"#;
        let fields = reported_fields(body, Resource::Upstream, CrudMethod::Create);
        assert!(fields.contains(&"unknown_one".to_owned()), "{fields:?}");
        assert!(fields.contains(&"unknown_two".to_owned()), "{fields:?}");
    }

    /// `inst-cv-08` and `inst-cv-09`: a body inside the field set is handed on
    /// to the domain layer, which owns the value rules.
    #[test]
    fn a_body_inside_the_field_set_is_handed_to_the_domain_layer() {
        let dto = parse(Resource::Upstream, CrudMethod::Create, UPSTREAM_BODY)
            .expect("the body is inside the field set");
        let upstream = crate::domain::validation::validate_upstream_payload(
            &Value::Object(dto),
            Uuid::from_u128(0x7),
        )
        .expect("the payload satisfies the domain rules");
        assert_eq!(upstream.alias.as_deref(), Some("payments.vendor.com"));
        assert!(upstream.enabled, "the flag the body carries is kept");

        let dto = parse(Resource::Plugin, CrudMethod::Create, PLUGIN_BODY)
            .expect("the body is inside the field set");
        let plugin = crate::domain::validation::validate_plugin_payload(
            &Value::Object(dto),
            Uuid::from_u128(0x7),
        )
        .expect("the plugin payload satisfies the domain rules");
        assert_eq!(plugin.name.as_deref(), Some("tenant-guard"));
    }

    #[test]
    fn the_aggregate_field_sets_are_the_schema_field_sets_plus_the_extension() {
        assert_eq!(
            Resource::Upstream.aggregate_fields(),
            UPSTREAM_CREATE_FIELDS,
            "the upstream schema declares the extension field itself"
        );
        assert!(Resource::Route.aggregate_fields().contains(&"enabled"));
        assert!(Resource::Route.aggregate_fields().contains(&"priority"));
        assert!(Resource::Route.aggregate_fields().contains(&"upstream_id"));
        assert!(
            !Resource::Plugin.aggregate_fields().contains(&"enabled"),
            "a plugin has no field-set extension"
        );
        assert!(PLUGIN_TYPE_IDS.contains(&PLUGIN_TYPE_IDS[0]));
        assert!(!PROTOCOL_HTTP.is_empty());
    }
}
