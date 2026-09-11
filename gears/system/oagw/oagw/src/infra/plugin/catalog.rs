//! The builtin plugin catalog
//! (`cpt-cf-oagw-flow-plugin-catalog-bootstrap`).
//!
//! The catalog is the set the gear serves and the data plane of entry 2.5
//! resolves from: the three registries of [`super::registry`], the reserved
//! catalog-only identifiers that are registered but never resolvable
//! (`inst-pcrg-05`) and the type schemas registered with
//! `cpt-cf-oagw-actor-types-registry` (`inst-pcat-02`).
//!
//! Construction is fallible on purpose: a duplicate identifier or a rejected
//! registration fails the gear init step with the offending identifier named, so
//! host startup aborts instead of serving a plugin contract whose catalog is
//! incomplete (`inst-pcat-05`, `inst-pcrg-12`).

// @cpt-begin:cpt-cf-oagw-flow-plugin-catalog-bootstrap:p2:inst-pcat-11
// The resolution of a registry entry into an executable plugin instance and the
// execution order of a chain are data-plane behaviour (entry 2.5), never a
// control-plane action: this module holds identity, type and declared phases
// only, and exposes no way to run a plugin.
// @cpt-end:cpt-cf-oagw-flow-plugin-catalog-bootstrap:p2:inst-pcat-11

use std::collections::BTreeSet;

use types_registry_sdk::TypesRegistryClient;

use crate::domain::plugin::{PluginType, AUTH_PLUGIN_BASE, NAMED_INSTANCE_PREFIX};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};

/// The declared JSON body of a plugin type schema.
///
/// The `$schema` field is what makes the entity a schema at all: the GTS store
/// classifies a body as a schema iff it carries `$schema`, so a body that only
/// declares `$id` with a type identifier is stored as an instance and every
/// ready-commit validation that resolves the type fails. The required field set
/// is deliberately absent — the create path enforces it (`inst-pdef-01`) and the
/// reserved catalog-only identifiers, which are not plugin definitions, must
/// validate against this body too.
const SCHEMA_BODY: &str = r#"{
    "$schema": "http://json-schema.org/draft-07/schema#",
    "type": "object",
    "properties": {
        "id": { "type": "string" },
        "tenant_id": { "type": "string" },
        "plugin_type": { "type": "string" },
        "name": { "type": "string" },
        "description": { "type": "string" },
        "config_schema": { "type": "object" },
        "phases": { "type": "array", "items": { "type": "string" } },
        "source_code": { "type": "string" },
        "last_used_at": { "type": "string" },
        "gc_eligible_at": { "type": "string" }
    },
    "additionalProperties": false
}"#;

/// Why a plugin catalog could not be built.
///
/// Every row names the offending identifier, which is what the gear init step
/// reports when it aborts host startup (`inst-pcrg-11`, `inst-pcrg-12`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginCatalogError {
    /// Two registry entries carry the same identifier.
    #[error("duplicate plugin identifier `{identifier}`")]
    DuplicateIdentifier {
        /// The identifier both entries carry.
        identifier: String,
    },

    /// The types-registry refused a type schema or a reserved identifier.
    #[error("the plugin catalog entry `{identifier}` could not be registered: {cause}")]
    Registration {
        /// The identifier the registry refused.
        identifier: String,
        /// The registry's own failure reason.
        cause: String,
    },
}

/// The builtin plugin catalog of the gear.
///
/// Read-only for the lifetime of the process (`inst-pcrg-10`): no management
/// operation mutates it, because builtin plugins are not stored, not addressable
/// and not subject to deletion.
pub struct PluginCatalog {
    /// Resolvable auth plugins.
    auth: AuthPluginRegistry,
    /// Resolvable guard plugins.
    guard: GuardPluginRegistry,
    /// Resolvable transform plugins.
    transform: TransformPluginRegistry,
    /// Reserved identifiers registered with the types-registry and held outside
    /// every registry (`inst-pcrg-05`, `inst-pcrg-06`), as full GTS identifiers.
    catalog_only: BTreeSet<String>,
    /// The identifiers this catalog submitted to the types-registry.
    registered: Vec<String>,
}

impl PluginCatalog {
    /// Build the catalog over the builtin declarations, without registering
    /// anything.
    ///
    /// This is the catalog of a deployment with no in-process types-registry
    /// client: the reserved identifiers are still recorded, so a caller that
    /// binds one is refused, and the absence of the registration is logged at
    /// startup rather than silently assumed.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCatalogError::DuplicateIdentifier`] when two builtin
    /// declarations collide.
    pub fn with_builtins() -> Result<Self, PluginCatalogError> {
        Self::assemble(Vec::new())
    }

    /// Register the plugin type schemas and the reserved catalog-only
    /// identifiers with the types-registry, then build the catalog
    /// (`inst-pcat-02`, `inst-pcat-03`).
    ///
    /// # Errors
    ///
    /// Returns [`PluginCatalogError::Registration`] naming the offending
    /// identifier when the registry refuses one of the submissions or is
    /// unreachable, and [`PluginCatalogError::DuplicateIdentifier`] when two
    /// builtin declarations collide.
    pub async fn bootstrap(
        client: &dyn TypesRegistryClient,
    ) -> Result<Self, PluginCatalogError> {
        let schemas = Self::type_schema_entities();
        let results = client
            .register_type_schemas(schemas.clone())
            .await
            .map_err(|cause| PluginCatalogError::Registration {
                identifier: AUTH_PLUGIN_BASE.to_owned(),
                cause: cause.to_string(),
            })?;
        Self::confirm(results, &schemas)?;

        let reserved = Self::reserved_entities();
        let identifiers = reserved
            .iter()
            .filter_map(|entity| entity.get("id").and_then(|id| id.as_str()))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let results = client
            .register(reserved.clone())
            .await
            .map_err(|cause| PluginCatalogError::Registration {
                identifier: identifiers.first().cloned().unwrap_or_default(),
                cause: cause.to_string(),
            })?;
        Self::confirm(results, &reserved)?;

        Self::assemble(identifiers)
    }

    /// The registered plugin type schemas, as the flow returns them
    /// (`inst-pcat-12`).
    #[must_use]
    pub fn type_schema_entities() -> Vec<serde_json::Value> {
        PluginType::all()
            .iter()
            .map(|plugin_type| {
                // The declared body is a fixed string, so a parse failure is a
                // programming error rather than an input error; the default keeps
                // the registration total.
                let mut schema = serde_json::from_str::<serde_json::Value>(SCHEMA_BODY)
                    .unwrap_or(serde_json::Value::Null);
                if let Some(object) = schema.as_object_mut() {
                    object.insert(
                        "$id".to_owned(),
                        serde_json::json!(format!("gts://{}~", plugin_type.base_identifier())),
                    );
                    object.insert(
                        "title".to_owned(),
                        serde_json::json!(format!("{} plugin", plugin_type.as_str())),
                    );
                    object.insert(
                        "description".to_owned(),
                        serde_json::json!(format!(
                            "Schema of a `{}` plugin definition of the oagw gear",
                            plugin_type.as_str()
                        )),
                    );
                }
                schema
            })
            .collect()
    }

    /// The six reserved catalog-only identifiers, as instance entities
    /// (`inst-pcrg-05`, `inst-pcrg-06`).
    ///
    /// Each is registered as a reserved GTS catalog entry with no backing
    /// implementation, so a caller that uses one as `auth.plugin_type` fails
    /// with `unknown auth plugin` and a caller that binds one through
    /// `plugins.items[].plugin_ref` is refused.
    #[must_use]
    pub fn reserved_entities() -> Vec<serde_json::Value> {
        let mut entities = Vec::new();
        // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-06
        // Every catalog-only identifier is registered as a reserved GTS catalog
        // entry with no backing implementation, so a caller that uses one as
        // `auth.plugin_type` fails with `unknown auth plugin` and a caller that
        // binds one through `plugins.items[].plugin_ref` is refused.
        for plugin_type in PluginType::all() {
            for name in plugin_type.catalog_only_names() {
                let identifier =
                    format!("{}~{NAMED_INSTANCE_PREFIX}{name}.v1", plugin_type.base_identifier());
                // The body is an instance of the plugin type schema, so it may
                // carry only the properties that schema declares: `$id` would be
                // an undeclared property and fail ready-commit validation, so the
                // identifier goes into `id`, which the registry reads as the
                // entity id field.
                entities.push(serde_json::json!({
                    "id": identifier,
                    "plugin_type": plugin_type.as_str(),
                    "name": name,
                    "description": format!(
                        "Reserved catalog-only {} identifier of the oagw gear: registered, \
                         never resolvable and never bindable",
                        plugin_type.as_str()
                    ),
                }));
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-06
        entities
    }

    /// Fail on any per-item registration result that is not an `Ok`.
    fn confirm(
        results: Vec<types_registry_sdk::RegisterResult>,
        submitted: &[serde_json::Value],
    ) -> Result<(), PluginCatalogError> {
        for result in results {
            if let types_registry_sdk::RegisterResult::Err { gts_id, error } = result {
                let identifier = gts_id
                    .or_else(|| {
                        submitted.first().and_then(|entity| {
                            entity
                                .get("id")
                                .or_else(|| entity.get("$id"))
                                .and_then(|id| id.as_str())
                                .map(str::to_owned)
                        })
                    })
                    .unwrap_or_default();
                return Err(PluginCatalogError::Registration {
                    identifier,
                    cause: error.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Build the registries over the builtin declarations.
    fn assemble(registered: Vec<String>) -> Result<Self, PluginCatalogError> {
        let catalog_only = PluginType::all()
            .iter()
            .flat_map(|plugin_type| {
                plugin_type.catalog_only_names().iter().map(|name| {
                    format!("{}~{NAMED_INSTANCE_PREFIX}{name}.v1", plugin_type.base_identifier())
                })
            })
            .collect::<BTreeSet<_>>();

        // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-13
        // The returned catalog is the three registries, the catalog-only
        // identifier set and the identifiers the types-registry accepted.
        Ok(Self {
            // `inst-pcrg-02` to `inst-pcrg-04`: the four auth plugins, the one
            // guard plugin and the one transform plugin.
            auth: AuthPluginRegistry::with_builtins()?,
            guard: GuardPluginRegistry::with_builtins()?,
            transform: TransformPluginRegistry::with_builtins()?,
            catalog_only,
            registered,
        })
        // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-13
    }

    /// The resolvable auth plugins (`inst-pcrg-02`).
    #[must_use]
    pub const fn auth(&self) -> &AuthPluginRegistry {
        &self.auth
    }

    /// The resolvable guard plugins (`inst-pcrg-03`).
    #[must_use]
    pub const fn guard(&self) -> &GuardPluginRegistry {
        &self.guard
    }

    /// The resolvable transform plugins (`inst-pcrg-04`).
    #[must_use]
    pub const fn transform(&self) -> &TransformPluginRegistry {
        &self.transform
    }

    /// The registry of one plugin type.
    #[must_use]
    pub fn registry_of(&self, plugin_type: PluginType) -> RegistryRef<'_> {
        match plugin_type {
            PluginType::Auth => RegistryRef::Auth(&self.auth),
            PluginType::Guard => RegistryRef::Guard(&self.guard),
            PluginType::Transform => RegistryRef::Transform(&self.transform),
        }
    }

    /// The reserved catalog-only identifiers, as full GTS identifiers
    /// (`inst-pcrg-05`).
    #[must_use]
    pub fn catalog_only_identifiers(&self) -> Vec<String> {
        self.catalog_only.iter().cloned().collect()
    }

    /// Whether `identifier` is one of the reserved catalog-only identifiers.
    #[must_use]
    pub fn is_catalog_only(&self, identifier: &str) -> bool {
        self.catalog_only.contains(identifier)
    }

    /// The full GTS identifier of every resolvable plugin, in name order per
    /// registry, with the registry key each identifier maps to (`inst-pcrg-07`,
    /// `inst-pcrg-08`).
    ///
    /// Classification and resolution agree by construction: the registries are
    /// built from exactly the name sets [`crate::domain::plugin`] fixes.
    #[must_use]
    pub fn resolvable(&self) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        for plugin_type in PluginType::all() {
            for identifier in self.registry_of(plugin_type).identifiers() {
                let name = identifier
                    .split_once('~')
                    .and_then(|(_, instance)| crate::domain::plugin::named_name(instance))
                    .map(str::to_owned);
                if let Some(name) = name {
                    entries.push((identifier, name));
                }
            }
        }
        entries
    }

    /// The identifiers this catalog submitted to the types-registry.
    #[must_use]
    pub fn registered_identifiers(&self) -> &[String] {
        &self.registered
    }
}

/// The registry of one plugin type.
#[derive(Clone, Copy)]
pub enum RegistryRef<'a> {
    /// The auth registry.
    Auth(&'a AuthPluginRegistry),
    /// The guard registry.
    Guard(&'a GuardPluginRegistry),
    /// The transform registry.
    Transform(&'a TransformPluginRegistry),
}

impl RegistryRef<'_> {
    /// The full GTS identifiers of the registry's entries, in name order.
    #[must_use]
    pub fn identifiers(&self) -> Vec<String> {
        match self {
            Self::Auth(registry) => registry.identifiers(),
            Self::Guard(registry) => registry.identifiers(),
            Self::Transform(registry) => registry.identifiers(),
        }
    }

    /// Whether the registry holds an entry named `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        match self {
            Self::Auth(registry) => registry.contains(name),
            Self::Guard(registry) => registry.contains(name),
            Self::Transform(registry) => registry.contains(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use toolkit_canonical_errors::CanonicalError;
    use types_registry_sdk::RegisterResult;

    use super::*;
    use crate::domain::plugin::{AUTH_PLUGIN_BASE, GUARD_PLUGIN_BASE, TRANSFORM_PLUGIN_BASE};

    /// Recorder over the types-registry contract: it records the submissions and
    /// optionally refuses one identifier, which [`MockTypesRegistryClient`]
    /// cannot express because it asserts an empty batch.
    struct RecordingClient {
        schemas: Mutex<Vec<String>>,
        instances: Mutex<Vec<String>>,
        /// Identifier a submission of which is refused.
        refuse: Option<String>,
    }

    impl RecordingClient {
        fn refusing(identifier: &str) -> Self {
            Self {
                schemas: Mutex::new(Vec::new()),
                instances: Mutex::new(Vec::new()),
                refuse: Some(identifier.to_owned()),
            }
        }

        fn submitted_schemas(&self) -> Vec<String> {
            self.schemas.lock().expect("schemas lock").clone()
        }

        fn submitted_instances(&self) -> Vec<String> {
            self.instances.lock().expect("instances lock").clone()
        }
    }

    fn identifier_of(entity: &serde_json::Value) -> String {
        entity
            .get("id")
            .or_else(|| entity.get("$id"))
            .and_then(|id| id.as_str())
            .unwrap_or_default()
            .trim_start_matches("gts://")
            .to_owned()
    }

    #[async_trait]
    impl TypesRegistryClient for RecordingClient {
        async fn register(
            &self,
            entities: Vec<serde_json::Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            let mut results = Vec::new();
            for entity in entities {
                let id = identifier_of(&entity);
                self.instances.lock().expect("instances lock").push(id.clone());
                results.push(match self.refuse.as_ref() {
                    Some(refused) if *refused == id => RegisterResult::Err {
                        gts_id: Some(id.clone()),
                        error: toolkit_canonical_errors::CanonicalError::internal(
                            format!("refused `{id}`"),
                        )
                        .create(),
                    },
                    _ => RegisterResult::Ok { gts_id: id },
                });
            }
            Ok(results)
        }

        async fn register_type_schemas(
            &self,
            type_schemas: Vec<serde_json::Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            let mut results = Vec::new();
            for entity in type_schemas {
                let id = identifier_of(&entity);
                self.schemas.lock().expect("schemas lock").push(id.clone());
                results.push(RegisterResult::Ok { gts_id: id });
            }
            Ok(results)
        }

        async fn get_type_schema(
            &self,
            _type_id: &str,
        ) -> Result<types_registry_sdk::GtsTypeSchema, CanonicalError> {
            Err(unused())
        }

        async fn get_type_schema_by_uuid(
            &self,
            _uuid: uuid::Uuid,
        ) -> Result<types_registry_sdk::GtsTypeSchema, CanonicalError> {
            Err(unused())
        }

        async fn get_type_schemas(
            &self,
            type_ids: Vec<String>,
        ) -> std::collections::HashMap<String, Result<types_registry_sdk::GtsTypeSchema, CanonicalError>>
        {
            type_ids
                .into_iter()
                .map(|id| (id, Err(unused())))
                .collect()
        }

        async fn get_type_schemas_by_uuid(
            &self,
            uuids: Vec<uuid::Uuid>,
        ) -> std::collections::HashMap<uuid::Uuid, Result<types_registry_sdk::GtsTypeSchema, CanonicalError>>
        {
            uuids.into_iter().map(|uuid| (uuid, Err(unused()))).collect()
        }

        async fn list_type_schemas(
            &self,
            _query: types_registry_sdk::TypeSchemaQuery,
        ) -> Result<Vec<types_registry_sdk::GtsTypeSchema>, CanonicalError> {
            Ok(Vec::new())
        }

        async fn register_instances(
            &self,
            instances: Vec<serde_json::Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            self.register(instances).await
        }

        async fn get_instance(
            &self,
            _id: &str,
        ) -> Result<types_registry_sdk::GtsInstance, CanonicalError> {
            Err(unused())
        }

        async fn get_instance_by_uuid(
            &self,
            _uuid: uuid::Uuid,
        ) -> Result<types_registry_sdk::GtsInstance, CanonicalError> {
            Err(unused())
        }

        async fn get_instances(
            &self,
            ids: Vec<String>,
        ) -> std::collections::HashMap<String, Result<types_registry_sdk::GtsInstance, CanonicalError>>
        {
            ids.into_iter().map(|id| (id, Err(unused()))).collect()
        }

        async fn get_instances_by_uuid(
            &self,
            uuids: Vec<uuid::Uuid>,
        ) -> std::collections::HashMap<uuid::Uuid, Result<types_registry_sdk::GtsInstance, CanonicalError>>
        {
            uuids.into_iter().map(|uuid| (uuid, Err(unused()))).collect()
        }

        async fn list_instances(
            &self,
            _query: types_registry_sdk::InstanceQuery,
        ) -> Result<Vec<types_registry_sdk::GtsInstance>, CanonicalError> {
            Ok(Vec::new())
        }
    }

    fn unused() -> CanonicalError {
        CanonicalError::internal("not exercised by the catalog bootstrap").create()
    }

    /// The catalog of a bootstrap over `client`: a `match` because `src/` forbids
    /// `expect` even under `cfg(test)`.
    async fn bootstrapped(client: &RecordingClient) -> PluginCatalog {
        match PluginCatalog::bootstrap(client).await {
            Ok(catalog) => catalog,
            Err(cause) => panic!("the bootstrap failed: {cause}"),
        }
    }

    /// The catalog of a builtin-only assembly: a `match` because `src/` forbids
    /// `expect` even under `cfg(test)`.
    fn builtins() -> PluginCatalog {
        match PluginCatalog::with_builtins() {
            Ok(catalog) => catalog,
            Err(cause) => panic!("the builtin assembly failed: {cause}"),
        }
    }

    #[tokio::test]
    async fn bootstrap_registers_the_type_schemas_and_the_reserved_identifiers() {
        let client = RecordingClient {
            schemas: Mutex::new(Vec::new()),
            instances: Mutex::new(Vec::new()),
            refuse: None,
        };
        let catalog = bootstrapped(&client).await;

        // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-01
        let schemas = client.submitted_schemas();
        assert_eq!(schemas.len(), 3, "{schemas:?}");
        for plugin_type in PluginType::all() {
            let type_id = format!("{}~", plugin_type.base_identifier());
            assert!(
                schemas.contains(&type_id),
                "{type_id} missing: {schemas:?}"
            );
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-01

        // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-05
        let instances = client.submitted_instances();
        assert_eq!(instances.len(), 6, "{instances:?}");
        // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-05
        assert_eq!(catalog.registered_identifiers().len(), 6);
    }

    #[test]
    fn the_type_schema_bodies_are_schemas_by_the_gts_classification_rule() {
        // The GTS store classifies a body as a schema iff it carries `$schema`,
        // whatever its `$id` looks like; a schema submitted without it is stored
        // as an instance and every ready-commit validation of the type and of
        // its instances fails. The `$id` must be the `gts://` URI form of the
        // type identifier, which is the only form the schema id extraction
        // accepts.
        for schema in PluginCatalog::type_schema_entities() {
            let id = identifier_of(&schema);
            assert!(
                schema
                    .get("$schema")
                    .and_then(|value| value.as_str())
                    .is_some_and(|value| !value.is_empty()),
                "{id} declares no `$schema` and is stored as an instance"
            );
            let declared = schema
                .get("$id")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            assert!(
                declared.starts_with("gts://") && declared.ends_with('~'),
                "{declared} is not the `gts://` URI form of a type identifier"
            );
        }
    }

    #[test]
    fn the_reserved_bodies_are_instances_that_validate_against_the_type_schema() {
        // Every reserved body is validated against its plugin type schema at
        // ready-commit, where `additionalProperties: false` rejects any member
        // the schema does not declare — including `$id`, which is why the
        // identifier is carried in `id`.
        let schemas = PluginCatalog::type_schema_entities();
        for reserved in PluginCatalog::reserved_entities() {
            let id = identifier_of(&reserved);
            let plugin_type = reserved
                .get("plugin_type")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let schema = schemas
                .iter()
                .find(|schema| {
                    schema
                        .get("title")
                        .and_then(|value| value.as_str())
                        .is_some_and(|title| title.starts_with(plugin_type))
                })
                .unwrap_or_else(|| panic!("no type schema for `{plugin_type}`"));
            let properties = schema
                .get("properties")
                .and_then(|value| value.as_object())
                .unwrap_or_else(|| panic!("the schema of `{plugin_type}` declares no properties"));
            let members = reserved
                .as_object()
                .unwrap_or_else(|| panic!("`{id}` is not an object"));
            for member in members.keys() {
                assert!(
                    properties.contains_key(member.as_str()),
                    "`{member}` of `{id}` is not declared by the plugin type schema"
                );
            }
            assert!(
                !members.contains_key("$schema"),
                "`{id}` declares `$schema` and is stored as a schema"
            );
        }
    }

    #[tokio::test]
    async fn a_refused_registration_names_the_offending_identifier() {
        let reserved = format!("{AUTH_PLUGIN_BASE}~{NAMED_INSTANCE_PREFIX}bearer.v1");
        let client = RecordingClient::refusing(&reserved);
        let outcome = PluginCatalog::bootstrap(&client).await;
        match outcome {
            Err(PluginCatalogError::Registration { identifier, .. }) => {
                assert_eq!(identifier, reserved);
            }
            Ok(_) => panic!("the refused registration must fail the bootstrap"),
            Err(PluginCatalogError::DuplicateIdentifier { identifier }) => {
                panic!("unexpected duplicate `{identifier}`");
            }
        }
    }

    #[test]
    fn the_builtins_resolve_and_the_reserved_identifiers_stay_outside_every_registry() {
        // @cpt-begin:cpt-cf-oagw-dod-builtin-registries:p1:inst-full
        let catalog = builtins();

        let resolvable = catalog
            .resolvable()
            .into_iter()
            .map(|(identifier, _)| identifier)
            .collect::<Vec<_>>();
        for identifier in [
            format!("{AUTH_PLUGIN_BASE}~{NAMED_INSTANCE_PREFIX}noop.v1"),
            format!("{AUTH_PLUGIN_BASE}~{NAMED_INSTANCE_PREFIX}apikey.v1"),
            format!("{AUTH_PLUGIN_BASE}~{NAMED_INSTANCE_PREFIX}oauth2_client_cred.v1"),
            format!("{AUTH_PLUGIN_BASE}~{NAMED_INSTANCE_PREFIX}oauth2_client_cred_basic.v1"),
            format!("{GUARD_PLUGIN_BASE}~{NAMED_INSTANCE_PREFIX}required_headers.v1"),
            format!("{TRANSFORM_PLUGIN_BASE}~{NAMED_INSTANCE_PREFIX}request_id.v1"),
        ] {
            assert!(
                resolvable.contains(&identifier),
                "{identifier} missing: {resolvable:?}"
            );
        }
        for identifier in catalog.catalog_only_identifiers() {
            assert!(
                !resolvable.contains(&identifier),
                "{identifier} is catalog-only and must not resolve"
            );
            assert!(catalog.is_catalog_only(&identifier));
        }
        // @cpt-end:cpt-cf-oagw-dod-builtin-registries:p1:inst-full
    }

    #[test]
    fn every_resolvable_identifier_is_a_valid_gts_identifier() {
        let catalog = builtins();
        for (identifier, name) in catalog.resolvable() {
            assert!(
                gts::GtsId::try_new(&identifier).is_ok(),
                "{identifier} is not a GTS identifier"
            );
            assert!(!identifier.ends_with('~'), "{identifier} is an instance id");
            assert!(
                !catalog.is_catalog_only(&identifier),
                "{name} must not be catalog-only"
            );
        }
    }

    #[test]
    fn every_reserved_identifier_is_a_gts_instance_identifier() {
        for identifier in builtins().catalog_only_identifiers() {
            assert!(
                gts::GtsId::try_new(&identifier).is_ok(),
                "{identifier} is not a GTS identifier"
            );
            assert!(!identifier.ends_with('~'));
        }
    }

    /// A second declaration of the `required_headers` name, for the duplicate
    /// check the registry construction performs.
    struct CollidingGuard;

    impl crate::domain::plugin::PluginDeclaration for CollidingGuard {
        fn identifier(&self) -> &str {
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        }

        fn plugin_type(&self) -> PluginType {
            PluginType::Guard
        }

        fn name(&self) -> &str {
            "required_headers"
        }

        fn phases(&self) -> &'static [crate::domain::plugin::Phase] {
            &[crate::domain::plugin::Phase::OnRequest]
        }
    }

    impl crate::domain::plugin::GuardPlugin for CollidingGuard {}

    #[test]
    fn a_duplicate_declaration_fails_the_registry_construction() {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-11
        let entries: Vec<std::sync::Arc<dyn crate::domain::plugin::GuardPlugin>> = vec![
            std::sync::Arc::new(CollidingGuard),
            std::sync::Arc::new(CollidingGuard),
        ];
        let outcome = crate::infra::plugin::registry::PluginRegistry::new(entries);
        match outcome {
            Err(PluginCatalogError::DuplicateIdentifier { identifier }) => {
                assert_eq!(
                    identifier,
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
                );
            }
            Ok(_) => panic!("the duplicate identifier must be refused"),
            Err(PluginCatalogError::Registration { identifier, cause }) => {
                panic!("unexpected registration failure `{identifier}`: {cause}");
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-catalog-register:p1:inst-pcrg-11
    }

}