// @cpt-begin:cpt-cf-oagw-dod-gts-provisioning:p1:inst-full
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{RegisterResult, TypesRegistryClient, TypesRegistryError};

use crate::domain::error::OagwError;

/// GTS type-schema draft the provisioned schemas declare.
const JSON_SCHEMA_DRAFT: &str = "https://json-schema.org/draft-07/schema#";

/// GTS type identifier of the upstream identifier family.
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// GTS type identifier of the route identifier family.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";
/// GTS type identifiers of the plugin identifier family.
pub const PLUGIN_TYPE_IDS: &[&str] = &[
    "gts.cf.core.oagw.auth_plugin.v1~",
    "gts.cf.core.oagw.guard_plugin.v1~",
    "gts.cf.core.oagw.transform_plugin.v1~",
];
/// GTS type identifier of the protocol identifier family.
pub const PROTOCOL_TYPE_ID: &str = "gts.cf.core.oagw.protocol.v1~";
/// GTS type identifier of the error identifier family.
pub const ERROR_TYPE_ID: &str = "gts.cf.core.errors.err.v1~";

/// One oagw identifier family and the GTS type identifiers it provisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentifierFamily {
    /// Short name of the family, used in diagnostics.
    pub name: &'static str,
    /// The GTS type identifiers the family covers.
    pub type_ids: &'static [&'static str],
}

/// The closed oagw identifier-family list (`inst-tp-01`).
pub const IDENTIFIER_FAMILIES: &[IdentifierFamily] = &[
    IdentifierFamily {
        name: "upstream",
        type_ids: &[UPSTREAM_TYPE_ID],
    },
    IdentifierFamily {
        name: "route",
        type_ids: &[ROUTE_TYPE_ID],
    },
    IdentifierFamily {
        name: "plugin",
        type_ids: PLUGIN_TYPE_IDS,
    },
    IdentifierFamily {
        name: "protocol",
        type_ids: &[PROTOCOL_TYPE_ID],
    },
    IdentifierFamily {
        name: "error",
        type_ids: &[ERROR_TYPE_ID],
    },
];

/// Seam over the types-registry SDK client used by the provisioning algorithm.
///
/// Kept so the algorithm is testable without a live types-registry gear; the
/// production implementation forwards to [`TypesRegistryClient`].
#[async_trait]
pub trait TypeSchemaRegistrar: Send + Sync {
    /// Registers the given GTS type-schemas, one `RegisterResult` per input.
    ///
    /// # Errors
    /// Returns `Err` only for catastrophic failures, mirroring the SDK client.
    async fn register_type_schemas(
        &self,
        type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError>;
}

#[async_trait]
impl TypeSchemaRegistrar for Arc<dyn TypesRegistryClient> {
    async fn register_type_schemas(
        &self,
        type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        TypesRegistryClient::register_type_schemas(self.as_ref(), type_schemas).await
    }
}

/// Builds the JSON type-schema document for a GTS type identifier.
fn type_schema(type_id: &str) -> Value {
    let mut schema = json!({
        "$id": format!("gts://{type_id}"),
        "$schema": JSON_SCHEMA_DRAFT,
        "type": "object",
        "properties": {},
    });
    if type_id == PROTOCOL_TYPE_ID {
        // The protocol family pins the closed value set the configuration
        // loader validates (`cpt-cf-oagw-constraint-protocols`).
        schema["properties"]["protocol"] = json!({ "enum": crate::config::UPSTREAM_PROTOCOLS });
    }
    schema
}

/// Provisions every oagw identifier family in the types-registry.
///
/// # Errors
/// Returns the typed startup error surface: a
/// [`OagwError::LinkUnavailable`] naming the family that failed when the
/// registry reports an error other than an existing registration.
pub async fn provision_identifier_families(
    registrar: &dyn TypeSchemaRegistrar,
) -> Result<Vec<&'static str>, OagwError> {
    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-01
    // Build the identifier family list: upstream, route, plugin, protocol,
    // error.
    let mut provisioned = Vec::with_capacity(IDENTIFIER_FAMILIES.len());
    // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-01

    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-02
    // FOR EACH family in the list.
    for family in IDENTIFIER_FAMILIES {
        // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-03
        // Ensure the family's GTS type identifiers are registered in the
        // types-registry through the SDK client.
        let schemas = family.type_ids.iter().map(|id| type_schema(id)).collect();
        // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-04
        let results = match registrar.register_type_schemas(schemas).await {
            Ok(results) => results,
            Err(error) => return Err(registry_failure(family.name, &error)),
        };
        for (index, type_id) in family.type_ids.iter().enumerate() {
            let Some(result) = results.get(index) else {
                return Err(OagwError::link_unavailable(format!(
                    "oagw.types: identifier family '{}' reported no result for {type_id}",
                    family.name
                )));
            };
            let Err(error) = result.as_result() else {
                continue;
            };
            // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-05
            if is_already_registered(error) {
                // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-06
                // The existing registration is success: a repeated init does
                // not fail and does not duplicate the registration.
                continue;
                // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-06
                // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-05
            }
            // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-07
            // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-08
            // Any other types-registry error aborts startup with the typed
            // startup error surface naming the family that failed.
            return Err(registry_failure(family.name, error));
            // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-08
            // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-07
        }
        // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-04
        // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-03
        provisioned.push(family.name);
    }
    // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-02

    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-09
    Ok(provisioned)
    // @cpt-end:cpt-cf-oagw-algo-type-provisioning:p1:inst-tp-09
}

/// Reports whether a registration failure means the type is already present.
fn is_already_registered(error: &CanonicalError) -> bool {
    matches!(
        TypesRegistryError::from(error.clone()),
        TypesRegistryError::AlreadyExists { .. }
    )
}

/// Builds the typed startup error for a family that could not be provisioned.
fn registry_failure(family: &str, error: &CanonicalError) -> OagwError {
    OagwError::link_unavailable(format!(
        "oagw.types: identifier family '{family}' could not be provisioned: {}",
        error.detail()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UPSTREAM_PROTOCOLS;
    use parking_lot::Mutex;
    use toolkit_canonical_errors::resource_error;

    // Test-local canonical error type: gives the suite a way to synthesize the
    // canonical categories the types-registry SDK client reports per item.
    #[resource_error(gts_id!("cf.oagw.test.service.v1~"))]
    struct TestServiceError;

    /// What the fake registrar replays for every requested schema.
    #[derive(Clone, Copy, PartialEq, Eq, Default)]
    enum Replay {
        /// Every requested id registers successfully.
        #[default]
        Ok,
        /// Every requested id is reported as already registered.
        AlreadyRegistered,
        /// The first requested id fails with a non-duplicate error.
        FirstFails,
        /// Every call fails catastrophically.
        Unavailable,
    }

    /// Registrar fake: records the schemas it is asked to register and replays
    /// a canned outcome for each of them.
    #[derive(Default)]
    struct FakeRegistrar {
        replay: Replay,
        requested: Mutex<Vec<Value>>,
    }

    impl FakeRegistrar {
        fn new(replay: Replay) -> Self {
            Self {
                replay,
                requested: Mutex::new(Vec::new()),
            }
        }

        fn requested(&self) -> Vec<Value> {
            self.requested.lock().clone()
        }

        fn requested_ids(&self) -> Vec<String> {
            self.requested()
                .iter()
                .filter_map(|schema| schema["$id"].as_str().map(str::to_owned))
                .collect()
        }

        fn schema_for(&self, type_id: &str) -> Option<Value> {
            self.requested()
                .into_iter()
                .find(|schema| schema["$id"] == format!("gts://{type_id}"))
        }
    }

    #[async_trait]
    impl TypeSchemaRegistrar for FakeRegistrar {
        async fn register_type_schemas(
            &self,
            type_schemas: Vec<Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            self.requested.lock().extend(type_schemas.iter().cloned());
            if self.replay == Replay::Unavailable {
                return Err(catastrophic());
            }
            Ok(type_schemas
                .iter()
                .map(|schema| {
                    let id = schema["$id"].as_str().unwrap_or_default().to_owned();
                    match self.replay {
                        Replay::AlreadyRegistered => RegisterResult::Err {
                            gts_id: Some(id),
                            error: duplicate(),
                        },
                        Replay::FirstFails if id.ends_with("route.v1~") => RegisterResult::Err {
                            gts_id: Some(id),
                            error: rejected(),
                        },
                        _ => RegisterResult::Ok { gts_id: id },
                    }
                })
                .collect())
        }
    }

    /// A canonical error the registry reports as duplicate-on-register.
    fn duplicate() -> CanonicalError {
        TestServiceError::already_exists("type schema already registered")
            .with_resource(ROUTE_TYPE_ID.to_owned())
            .create()
    }

    /// A canonical error that is not a duplicate registration.
    fn rejected() -> CanonicalError {
        TestServiceError::unimplemented("backend rejected the schema").create()
    }

    /// A catastrophic registry failure.
    fn catastrophic() -> CanonicalError {
        TestServiceError::unknown("types-registry backend is down").create()
    }

    #[tokio::test]
    async fn all_five_families_are_provisioned() {
        let registrar = FakeRegistrar::default();
        let provisioned = provision_identifier_families(&registrar).await.unwrap();

        assert_eq!(
            provisioned,
            vec!["upstream", "route", "plugin", "protocol", "error"]
        );
        assert_eq!(registrar.requested().len(), 7, "one schema per type id");
    }

    #[tokio::test]
    async fn family_schemas_carry_the_gts_uri_of_the_documented_type_ids() {
        let registrar = FakeRegistrar::default();
        provision_identifier_families(&registrar).await.unwrap();

        let ids = registrar.requested_ids();
        for expected in IDENTIFIER_FAMILIES
            .iter()
            .flat_map(|family| family.type_ids)
        {
            assert!(
                ids.contains(&format!("gts://{expected}")),
                "family type id {expected} was not requested"
            );
        }
    }

    #[tokio::test]
    async fn protocol_family_pins_the_closed_protocol_set() {
        let registrar = FakeRegistrar::default();
        provision_identifier_families(&registrar).await.unwrap();

        let protocol_schema = registrar
            .schema_for(PROTOCOL_TYPE_ID)
            .unwrap_or_else(|| panic!("the protocol family schema must be requested"));
        assert_eq!(
            protocol_schema["properties"]["protocol"]["enum"],
            serde_json::to_value(UPSTREAM_PROTOCOLS).unwrap()
        );
    }

    #[tokio::test]
    async fn already_registered_is_treated_as_success() {
        let registrar = FakeRegistrar::new(Replay::AlreadyRegistered);
        let provisioned = provision_identifier_families(&registrar).await.unwrap();
        assert_eq!(provisioned.len(), IDENTIFIER_FAMILIES.len());
    }

    #[tokio::test]
    async fn a_second_init_does_not_fail_or_duplicate() {
        let first = FakeRegistrar::default();
        let second = FakeRegistrar::new(Replay::AlreadyRegistered);

        let first_families = provision_identifier_families(&first).await.unwrap();
        let second_families = provision_identifier_families(&second).await.unwrap();

        assert_eq!(first_families, second_families);
        assert_eq!(
            second.requested().len(),
            7,
            "the same families are re-declared"
        );
    }

    #[tokio::test]
    async fn registry_failure_aborts_startup_naming_the_family() {
        let registrar = FakeRegistrar::new(Replay::FirstFails);
        let error = provision_identifier_families(&registrar).await.unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
        assert!(
            error.detail().contains("'route'"),
            "the failing family must be named, got: {}",
            error.detail()
        );
    }

    #[tokio::test]
    async fn catastrophic_registry_failure_aborts_startup_naming_the_family() {
        let registrar = FakeRegistrar::new(Replay::Unavailable);
        let error = provision_identifier_families(&registrar).await.unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert!(
            error.detail().contains("'upstream'"),
            "the failing family must be named, got: {}",
            error.detail()
        );
    }

    #[test]
    fn the_family_list_is_the_closed_five_family_set() {
        let names: Vec<_> = IDENTIFIER_FAMILIES
            .iter()
            .map(|family| family.name)
            .collect();
        assert_eq!(names, ["upstream", "route", "plugin", "protocol", "error"]);
        assert_eq!(
            IDENTIFIER_FAMILIES
                .iter()
                .flat_map(|family| family.type_ids.iter())
                .count(),
            7,
            "upstream, route, three plugin types, protocol and error"
        );
    }
}

// @cpt-end:cpt-cf-oagw-dod-gts-provisioning:p1:inst-full
