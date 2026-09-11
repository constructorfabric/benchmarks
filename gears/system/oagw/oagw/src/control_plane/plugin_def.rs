//! Plugin definition validation and the wire phase vocabulary.
//!
//! The plugin create body carries no shipped schema of its own: no
//! `plugin.v1.schema.json` is frozen beside the upstream and route schemas, so
//! the five properties the create flow accepts are checked here in code. The
//! check is a closed one — the root admits exactly the members the flow names,
//! `plugin_type` names one of the three plugin families, the declared phases
//! are a subset of the phases that family supports, the configuration schema
//! is an object, the description is a string, and the source is present and
//! non-empty. The Starlark source is never parsed, compiled, or executed here:
//! create-time validation covers the declared fields only (§1.5).
//!
//! Every failing property is accumulated into **one**
//! `DomainError::gateway(ErrorKind::ValidationError, ..)` whose detail names
//! the failing properties, comma-separated, and never carries a request body
//! value — the same shape the upstream and route validators produce.

use serde_json::Value;
use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::plugin_contract::{PluginFamily, PluginPhase};
use crate::gts;

/// The root members a plugin create body may carry.
const PLUGIN_ROOT: [&str; 6] = [
    "plugin_type",
    "name",
    "description",
    "config_schema",
    "phases",
    "source_code",
];

/// The wire literal of one phase a plugin may declare.
///
/// The three on-phase literals DESIGN §3.1 names for the transform family —
/// `on_request`, `on_response`, `on_error` — are the vocabulary every family
/// declares in: the auth family's single credential-injection phase and the
/// guard family's two evaluation phases are request- and response-phase work
/// in the same sense, and the error phase is the transform contract's alone.
const PHASE_ON_REQUEST: &str = "on_request";
const PHASE_ON_RESPONSE: &str = "on_response";
const PHASE_ON_ERROR: &str = "on_error";

/// The wire literals one family admits, in contract order.
#[must_use]
pub const fn wire_phases(family: PluginFamily) -> &'static [&'static str] {
    match family {
        PluginFamily::Auth => &[PHASE_ON_REQUEST],
        PluginFamily::Guard => &[PHASE_ON_REQUEST, PHASE_ON_RESPONSE],
        PluginFamily::Transform => &[PHASE_ON_REQUEST, PHASE_ON_RESPONSE, PHASE_ON_ERROR],
    }
}

/// The plugin phase one wire literal names within one family.
#[must_use]
pub fn phase_of(family: PluginFamily, literal: &str) -> Option<PluginPhase> {
    match (family, literal) {
        (PluginFamily::Auth, PHASE_ON_REQUEST) => Some(PluginPhase::Auth),
        (PluginFamily::Guard, PHASE_ON_REQUEST) => Some(PluginPhase::GuardRequest),
        (PluginFamily::Guard, PHASE_ON_RESPONSE) => Some(PluginPhase::GuardResponse),
        (PluginFamily::Transform, PHASE_ON_REQUEST) => Some(PluginPhase::TransformRequest),
        (PluginFamily::Transform, PHASE_ON_RESPONSE) => Some(PluginPhase::TransformResponse),
        (PluginFamily::Transform, PHASE_ON_ERROR) => Some(PluginPhase::TransformError),
        _ => None,
    }
}

/// The validated body of a plugin create.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedPlugin {
    /// The family the body's `plugin_type` named.
    pub family: PluginFamily,
    /// The plugin name, as submitted.
    pub name: String,
    /// The description, when the body carried one.
    pub description: Option<String>,
    /// The configuration schema, when the body carried one.
    pub config_schema: Option<Value>,
    /// The declared phases, as the wire literals the body carried.
    pub phases: Vec<String>,
    /// The Starlark source, verbatim.
    pub source_code: String,
}

/// Accumulated failing properties of one plugin create body.
#[derive(Debug, Default)]
struct Defects(Vec<String>);

impl Defects {
    /// Adds one failing property by name, dropping a repeat.
    fn add(&mut self, property: &str) {
        if !self.0.contains(&property.to_owned()) {
            self.0.push(property.to_owned());
        }
    }

    /// Whether every property passed.
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The single validation error naming every failing property.
    fn into_error(self) -> DomainError {
        let detail = if self.0.is_empty() {
            String::from("the request body is not valid for the resource kind")
        } else {
            self.0.join(", ")
        };
        DomainError::gateway(ErrorKind::ValidationError, detail)
    }
}

/// Validates one plugin create body against the families and the phase
/// vocabulary the contracts declare.
///
/// The family is what the caller needs before the permission arm is selected,
/// so it is returned with the validated definition.
///
/// # Errors
///
/// Returns one gateway validation error naming every failing property.
#[allow(clippy::result_large_err)]
pub fn validate_plugin(body: &Value) -> Result<ValidatedPlugin, DomainError> {
    let mut defects = Defects::default();

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-type
    // The requested plugin_type names one of the three plugin base types, and
    // the declared phases are a subset of the phases that type supports: both
    // checks read the contracts the registries were built from, so a body that
    // names no family or a phase the family does not expose is refused before
    // any row is written.
    let fields = match body.as_object() {
        Some(fields) => fields,
        None => {
            defects.add("plugin_type");
            defects.add("name");
            defects.add("source_code");
            return Err(defects.into_error());
        }
    };
    for key in fields.keys() {
        if !PLUGIN_ROOT.contains(&key.as_str()) {
            defects.add(&format!("unknown property '{key}' at root"));
        }
    }

    let family = fields
        .get("plugin_type")
        .and_then(Value::as_str)
        .and_then(PluginFamily::from_type_literal);
    if family.is_none() {
        // A submitted value the catalogue names but no family backs is told
        // apart from one the catalogue does not know at all, so an operator
        // can tell a reserved-but-unimplemented type from a typo. Neither
        // detail carries the submitted value.
        match fields.get("plugin_type").and_then(Value::as_str) {
            Some(value) if gts::plugin_catalog::is_known_identifier(value) => {
                defects.add("plugin_type names a catalogue identifier no plugin family backs");
            }
            _ => defects.add("plugin_type"),
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-type

    let name = fields.get("name").and_then(Value::as_str);
    if name.is_none_or(str::is_empty) {
        defects.add("name");
    }
    if let Some(description) = fields.get("description")
        && description.as_str().is_none()
    {
        defects.add("description");
    }
    let config_schema = fields.get("config_schema");
    if let Some(schema) = config_schema
        && schema.as_object().is_none()
    {
        defects.add("config_schema");
    }
    let phases = declared_phases(fields, family, &mut defects);
    let source = fields.get("source_code").and_then(Value::as_str);
    if source.is_none_or(str::is_empty) {
        defects.add("source_code");
    }

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-validate-if
    if !defects.is_empty() {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-validate-return
        // RETURN 400 naming every failing property; no row is written, and the
        // source is never parsed or executed at create time.
        return Err(defects.into_error());
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-validate-return
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-validate-if

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-validate-else
    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-validate-continue
    // Continue with the validated definition and the verbatim source. A field
    // the checks above recorded a defect for has already answered, so each
    // binding below holds the value the body carried.
    let Some(family) = family else {
        return Err(defects.into_error());
    };
    let Some(name) = name else {
        return Err(defects.into_error());
    };
    let Some(source) = source else {
        return Err(defects.into_error());
    };
    Ok(ValidatedPlugin {
        family,
        name: name.to_owned(),
        description: fields
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        config_schema: config_schema.cloned(),
        phases,
        source_code: source.to_owned(),
    })
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-validate-continue
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-validate-else
}

/// The declared phases one body carries, checked against the family's set.
fn declared_phases(
    fields: &serde_json::Map<String, Value>,
    family: Option<PluginFamily>,
    defects: &mut Defects,
) -> Vec<String> {
    let Some(phases) = fields.get("phases") else {
        return Vec::new();
    };
    let Some(carried) = phases.as_array() else {
        defects.add("phases");
        return Vec::new();
    };
    let mut declared = Vec::new();
    for (index, phase) in carried.iter().enumerate() {
        let Some(literal) = phase.as_str() else {
            defects.add(&format!("phases[{index}]"));
            continue;
        };
        // A phase outside the family's supported set is refused by name.
        let admitted = family.is_some_and(|family| wire_phases(family).contains(&literal));
        if !admitted {
            defects.add(&format!("phases[{index}]"));
            continue;
        }
        if !declared.iter().any(|held| held == literal) {
            declared.push(literal.to_owned());
        }
    }
    declared
}

/// The anonymous GTS instance identifier of one plugin row, derived from the
/// family its `plugin_type` names.
#[must_use]
pub fn plugin_instance(family: PluginFamily, id: Uuid) -> String {
    gts::gts_instance(family.base_type(), id)
}

/// The family one create body's `plugin_type` names, read before any
/// validation runs so the body selects the permission arm the handler
/// enforces.
#[must_use]
pub fn declared_family(body: &Value) -> Option<PluginFamily> {
    body.get("plugin_type")
        .and_then(Value::as_str)
        .and_then(PluginFamily::from_type_literal)
}

/// The 400 the duplicate plugin name answers with: a name taken within the
/// calling tenant is a validation failure naming the property, because no
/// catalogue row answers it and the detail discloses nothing about another
/// tenant's catalogue.
#[allow(clippy::result_large_err)]
pub fn name_taken() -> DomainError {
    DomainError::gateway(
        ErrorKind::ValidationError,
        "name is already held by another plugin of the calling tenant",
    )
}

/// The 409 the in-use protection answers with.
#[allow(clippy::result_large_err)]
pub fn plugin_in_use() -> DomainError {
    DomainError::gateway(
        ErrorKind::PluginInUse,
        "the plugin is still referenced and cannot be deleted",
    )
}