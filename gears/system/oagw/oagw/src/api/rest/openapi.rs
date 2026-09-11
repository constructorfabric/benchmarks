//! OpenAPI registration of the management surface and of the data plane.
//!
//! The gear contributes the ten management operations of entry 2.2, the five
//! plugin-definition operations of entry 2.3, the twelve proxy operations of
//! entry 2.4 and their document schemas to the host registry in the same mount
//! step that merges the routes, so a caller can discover the surface
//! (`cpt-cf-oagw-dod-upstream-endpoints`, `cpt-cf-oagw-dod-route-endpoints`,
//! `cpt-cf-oagw-dod-plugin-endpoints`, `cpt-cf-oagw-dod-proxy-endpoint`). The
//! paths are absolute — `/oagw/v1/...` — while the axum routes are
//! mount-relative, which is why the operations are registered from a spec rather
//! than from [`crate::api::rest::routes`]' router.

use toolkit::api::operation_builder::{
    OperationSpec, ParamLocation, ParamSpec, RequestBodySchema, RequestBodySpec, ResponseSchema,
    ResponseSpec, VendorExtensions,
};
use toolkit::api::OpenApiRegistry;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{
    AdditionalProperties, ArrayBuilder, KnownFormat, ObjectBuilder, OneOfBuilder, Schema,
    SchemaFormat, SchemaType, Type,
};

use crate::api::rest::routes::{MOUNT_ROOT, PROBLEM_SCHEMA_NAME};

/// Component name of the stored upstream representation.
const UPSTREAM_SCHEMA: &str = "OagwUpstream";
/// Component name of the stored route representation.
const ROUTE_SCHEMA: &str = "OagwRoute";
/// Component name of the upstream create/replace body.
const UPSTREAM_BODY_SCHEMA: &str = "OagwUpstreamBody";
/// Component name of the route create/replace body.
const ROUTE_BODY_SCHEMA: &str = "OagwRouteBody";
/// Component name of the stored plugin definition.
const PLUGIN_SCHEMA: &str = "OagwPlugin";
/// Component name of the plugin create body.
const PLUGIN_BODY_SCHEMA: &str = "OagwPluginBody";

/// Tag shared by every management operation.
const MANAGEMENT_TAG: &str = "oagw-management";

/// Tag of the plugin definition operations.
const PLUGIN_TAG: &str = "oagw-plugins";

/// Tag of the data-plane proxy operations.
const PROXY_TAG: &str = "oagw-proxy";

/// Register the management operations and their schemas in the host registry.
pub fn register(openapi: &dyn OpenApiRegistry) {
    // @cpt-begin:cpt-cf-oagw-dod-upstream-endpoints:p1:inst-full
    register_schemas(openapi);

    for spec in operations() {
        openapi.register_operation(&spec);
    }
    // @cpt-end:cpt-cf-oagw-dod-upstream-endpoints:p1:inst-full

    // @cpt-begin:cpt-cf-oagw-dod-plugin-endpoints:p1:inst-full
    // The five plugin operations and their document schemas are registered in
    // the same step, so the host registry describes the whole surface the gear
    // serves. No replace operation is registered for a definition, because a
    // definition is immutable for its lifetime.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-catalog-bootstrap:p2:inst-pcat-09
    register_plugin_schemas(openapi);

    for spec in plugin_operations() {
        openapi.register_operation(&spec);
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-catalog-bootstrap:p2:inst-pcat-09
    // @cpt-end:cpt-cf-oagw-dod-plugin-endpoints:p1:inst-full

    // The twelve proxy operations are registered in the same mount step: every
    // forwarded method on both proxy paths, plus the `OPTIONS` preflight pair
    // the ADR 0004 ordering dispatches before authentication. They belong to
    // `cpt-cf-oagw-dod-proxy-endpoint`, whose marker stands at the router half
    // of the registration in `crate::api::rest::routes`.
    for spec in proxy_operations() {
        openapi.register_operation(&spec);
    }
}

/// The ten management operations, with their absolute paths.
fn operations() -> Vec<OperationSpec> {
    vec![
        operation(
            http::Method::POST,
            &[MOUNT_ROOT, "/upstreams"],
            "oagw.upstreams.create",
            "Create an upstream",
        )
        .with_body(UPSTREAM_BODY_SCHEMA, "Upstream declaration to store")
        .with_response(201, "The stored upstream representation", UPSTREAM_SCHEMA)
        .build(),
        operation(
            http::Method::GET,
            &[MOUNT_ROOT, "/upstreams"],
            "oagw.upstreams.list",
            "List the calling tenant's upstreams",
        )
        .with_query_parameters()
        .with_response(200, "The requested page of upstreams", UPSTREAM_SCHEMA)
        .build(),
        operation(
            http::Method::GET,
            &[MOUNT_ROOT, "/upstreams/{id}"],
            "oagw.upstreams.read",
            "Read one upstream",
        )
        .with_identifier()
        .with_response(200, "The stored upstream representation", UPSTREAM_SCHEMA)
        .build(),
        operation(
            http::Method::PUT,
            &[MOUNT_ROOT, "/upstreams/{id}"],
            "oagw.upstreams.replace",
            "Replace one upstream",
        )
        .with_identifier()
        .with_body(UPSTREAM_BODY_SCHEMA, "Replacement upstream declaration")
        .with_response(200, "The replaced upstream representation", UPSTREAM_SCHEMA)
        .build(),
        operation(
            http::Method::DELETE,
            &[MOUNT_ROOT, "/upstreams/{id}"],
            "oagw.upstreams.delete",
            "Delete one upstream and cascade its routes",
        )
        .with_identifier()
        .without_body()
        .build(),
        operation(
            http::Method::POST,
            &[MOUNT_ROOT, "/routes"],
            "oagw.routes.create",
            "Create a route",
        )
        .with_body(ROUTE_BODY_SCHEMA, "Route declaration to store")
        .with_response(201, "The stored route representation", ROUTE_SCHEMA)
        .build(),
        operation(
            http::Method::GET,
            &[MOUNT_ROOT, "/routes"],
            "oagw.routes.list",
            "List the calling tenant's routes",
        )
        .with_query_parameters()
        .with_response(200, "The requested page of routes", ROUTE_SCHEMA)
        .build(),
        operation(
            http::Method::GET,
            &[MOUNT_ROOT, "/routes/{id}"],
            "oagw.routes.read",
            "Read one route",
        )
        .with_identifier()
        .with_response(200, "The stored route representation", ROUTE_SCHEMA)
        .build(),
        operation(
            http::Method::PUT,
            &[MOUNT_ROOT, "/routes/{id}"],
            "oagw.routes.replace",
            "Replace one route",
        )
        .with_identifier()
        .with_body(ROUTE_BODY_SCHEMA, "Replacement route declaration")
        .with_response(200, "The replaced route representation", ROUTE_SCHEMA)
        .build(),
        operation(
            http::Method::DELETE,
            &[MOUNT_ROOT, "/routes/{id}"],
            "oagw.routes.delete",
            "Delete one route",
        )
        .with_identifier()
        .without_body()
        .build(),
    ]
}

/// The five plugin definition operations, with their absolute paths.
///
/// There is no `PUT /oagw/v1/plugins/{id}`: a definition is immutable, so the
/// host registry describes no replace operation and a `PUT` request resolves to
/// the router's method-not-allowed response.
fn plugin_operations() -> Vec<OperationSpec> {
    vec![
        operation_with_tag(
            http::Method::POST,
            &[MOUNT_ROOT, "/plugins"],
            "oagw.plugins.create",
            "Create a plugin definition",
            PLUGIN_TAG,
        )
        .with_body(PLUGIN_BODY_SCHEMA, "Plugin definition to store")
        .with_response(201, "The stored plugin definition", PLUGIN_SCHEMA)
        .build(),
        operation_with_tag(
            http::Method::GET,
            &[MOUNT_ROOT, "/plugins"],
            "oagw.plugins.list",
            "List the calling tenant's plugin definitions",
            PLUGIN_TAG,
        )
        .with_query_parameters()
        .with_response(200, "The requested page of plugin definitions", PLUGIN_SCHEMA)
        .build(),
        operation_with_tag(
            http::Method::GET,
            &[MOUNT_ROOT, "/plugins/{id}"],
            "oagw.plugins.read",
            "Read one plugin definition",
            PLUGIN_TAG,
        )
        .with_identifier()
        .with_response(200, "The stored plugin definition", PLUGIN_SCHEMA)
        .build(),
        operation_with_tag(
            http::Method::DELETE,
            &[MOUNT_ROOT, "/plugins/{id}"],
            "oagw.plugins.delete",
            "Delete one unreferenced plugin definition",
            PLUGIN_TAG,
        )
        .with_identifier()
        .without_body()
        .build(),
        operation_with_tag(
            http::Method::GET,
            &[MOUNT_ROOT, "/plugins/{id}/source"],
            "oagw.plugins.source",
            "Read the stored source of one plugin definition",
            PLUGIN_TAG,
        )
        .with_identifier()
        // The stored source goes out verbatim, with no JSON envelope. The
        // registry records the pure media type, since an OpenAPI media type key
        // carries no parameter; the handler sends `text/plain; charset=utf-8`.
        .with_text_response(
            200,
            "The stored plugin source, verbatim",
            "text/plain",
        )
        .with_conflict_response()
        .build(),
    ]
}

/// The twelve proxy operations of the data plane, with their absolute paths.
///
/// Every method the route model admits (`GET`, `POST`, `PUT`, `DELETE`, `PATCH`)
/// is registered on both proxy paths, plus the `OPTIONS` pair the preflight is
/// answered with. The preflight operations are registered unauthenticated: ADR
/// 0004 dispatches a CORS preflight before the Bearer surface, so the document
/// describes the endpoint the way a caller reaches it, while every other method
/// requires `gts.cf.core.oagw.proxy.v1~:invoke`. The response body of a proxy
/// call is the upstream's own response, so no JSON component is attached to the
/// passthrough status.
fn proxy_operations() -> Vec<OperationSpec> {
    let mut specs = Vec::new();
    for (method, id, summary) in [
        (
            http::Method::GET,
            "oagw.proxy.forward.get",
            "Forward a GET request to the upstream the alias names",
        ),
        (
            http::Method::POST,
            "oagw.proxy.forward.post",
            "Forward a POST request to the upstream the alias names",
        ),
        (
            http::Method::PUT,
            "oagw.proxy.forward.put",
            "Forward a PUT request to the upstream the alias names",
        ),
        (
            http::Method::DELETE,
            "oagw.proxy.forward.delete",
            "Forward a DELETE request to the upstream the alias names",
        ),
        (
            http::Method::PATCH,
            "oagw.proxy.forward.patch",
            "Forward a PATCH request to the upstream the alias names",
        ),
        (
            http::Method::OPTIONS,
            "oagw.proxy.preflight",
            "Answer a CORS preflight for the aliased upstream",
        ),
    ] {
        let actual = method != http::Method::OPTIONS;
        for (suffix, tail) in [
            ("/{alias}", ""),
            ("/{alias}/{path_suffix}", "/{path_suffix}"),
        ] {
            specs.push(proxy_operation(
                method.clone(),
                suffix,
                &format!(
                    "{id}.{}",
                    if tail.is_empty() { "bare" } else { "suffixed" }
                ),
                &format!("{summary}{tail}"),
                actual,
            ));
        }
    }
    specs
}

/// One proxy operation spec over `method` and the path suffix after `/proxy`.
fn proxy_operation(
    method: http::Method,
    suffix: &str,
    id: &str,
    summary: &str,
    authenticated: bool,
) -> OperationSpec {
    let path = format!("{MOUNT_ROOT}/proxy{suffix}");
    let mut spec = OperationSpec {
        handler_id: format!("{}:{path}", method.as_str().to_lowercase()),
        method,
        path,
        operation_id: Some(id.to_owned()),
        summary: Some(summary.to_owned()),
        description: None,
        tags: vec![PROXY_TAG.to_owned()],
        params: vec![ParamSpec {
            name: "alias".to_owned(),
            location: ParamLocation::Path,
            required: true,
            description: Some(
                "The upstream alias the request addresses, resolved over the \
                 calling tenant's ancestor chain"
                    .to_owned(),
            ),
            param_type: "string".to_owned(),
            array: false,
        }],
        request_body: None,
        responses: Vec::new(),
        authenticated,
        exposed: true,
        rate_limit: None,
        allowed_request_content_types: None,
        vendor_extensions: VendorExtensions::default(),
        license_requirement: None,
    };
    if suffix.ends_with("{path_suffix}") {
        spec.params.push(ParamSpec {
            name: "path_suffix".to_owned(),
            location: ParamLocation::Path,
            required: true,
            description: Some(
                "The path the request is forwarded with, matched against the \
                 routes stored for the upstream"
                    .to_owned(),
            ),
            param_type: "string".to_owned(),
            array: false,
        });
    }
    if authenticated {
        // ADR 0007: a multi-endpoint pool with a hostname-derived alias asks the
        // caller to name the endpoint it wants; every other pool ignores it.
        spec.params.push(ParamSpec {
            name: "X-OAGW-Target-Host".to_owned(),
            location: ParamLocation::Header,
            required: false,
            description: Some(
                "The endpoint host of the selected upstream's pool the request \
                 is forwarded to; required only when the pool has more than one \
                 endpoint and its alias is hostname-derived"
                    .to_owned(),
            ),
            param_type: "string".to_owned(),
            array: false,
        });
        spec.responses.push(ResponseSpec {
            status: 200,
            content_type: "",
            description: "The upstream response, passed through verbatim"
                .to_owned(),
            schema: None,
        });
    } else {
        spec.responses.push(ResponseSpec {
            status: 204,
            content_type: "",
            description: "The preflight answer, with the echoed CORS headers"
                .to_owned(),
            schema: None,
        });
    }
    for (status, description) in [
        (400, "A target-host, validation or framing failure"),
        (401, "The request carries no resolvable tenant, token or permission"),
        (403, "An origin, method or permission the configuration refuses"),
        (404, "No route matches the request in the calling tenant"),
        (413, "The request body exceeds the configured limit"),
        (502, "The upstream is unreachable or unusable"),
        (503, "The plaintext gate refused the endpoint or the breaker is open"),
        (504, "The upstream did not answer within the proxy timeout"),
    ] {
        spec.responses.push(ResponseSpec {
            status,
            content_type: toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON,
            description: description.to_owned(),
            schema: Some(ResponseSchema::Ref {
                schema_name: PROBLEM_SCHEMA_NAME.to_owned(),
            }),
        });
    }
    spec
}

/// Partially built operation, assembled by the fluent helpers below.
struct OperationDraft {
    spec: OperationSpec,
}

impl OperationDraft {
    /// Attach a JSON request body referencing a registered component.
    fn with_body(mut self, schema: &'static str, description: &str) -> Self {
        self.spec.request_body = Some(RequestBodySpec {
            content_type: "application/json",
            description: Some(description.to_owned()),
            schema: RequestBodySchema::Ref {
                schema_name: schema.to_owned(),
            },
            required: true,
        });
        self
    }

    /// Attach the `204` of a delete.
    fn without_body(mut self) -> Self {
        self.spec.responses.push(ResponseSpec {
            status: 204,
            content_type: "",
            description: "The record was removed".to_owned(),
            schema: None,
        });
        self
    }

    /// Attach a JSON response referencing a registered component.
    fn with_response(mut self, status: u16, description: &str, schema: &'static str) -> Self {
        self.spec.responses.push(ResponseSpec {
            status,
            content_type: "application/json",
            description: description.to_owned(),
            schema: Some(ResponseSchema::Ref {
                schema_name: schema.to_owned(),
            }),
        });
        self
    }

    /// Attach the verbatim-text response of the plugin source read.
    fn with_text_response(
        mut self,
        status: u16,
        description: &str,
        content_type: &'static str,
    ) -> Self {
        self.spec.responses.push(ResponseSpec {
            status,
            content_type,
            description: description.to_owned(),
            schema: None,
        });
        self
    }

    /// Attach the `409` of a definition a binding still references.
    fn with_conflict_response(mut self) -> Self {
        self.spec.responses.push(ResponseSpec {
            status: 409,
            content_type: toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON,
            description: "The definition is still referenced".to_owned(),
            schema: Some(ResponseSchema::Ref {
                schema_name: PROBLEM_SCHEMA_NAME.to_owned(),
            }),
        });
        self
    }

    /// Attach the `{id}` path parameter.
    fn with_identifier(mut self) -> Self {
        self.spec.params.push(ParamSpec {
            name: "id".to_owned(),
            location: ParamLocation::Path,
            required: true,
            description: Some(
                "Anonymous GTS identifier or bare UUID of the record, \
                 addressed inside the calling tenant"
                    .to_owned(),
            ),
            param_type: "string".to_owned(),
            array: false,
        });
        self
    }

    /// Attach the five OData list parameters the list contract supports.
    fn with_query_parameters(mut self) -> Self {
        for (name, description) in [
            ("$filter", "Comparison filter over the model's fields"),
            ("$orderby", "Ordering field with an optional direction"),
            ("$select", "Comma-separated list of projected fields"),
            ("$top", "Page size, default 50, capped at 100"),
            ("$skip", "Non-negative page offset"),
        ] {
            self.spec.params.push(ParamSpec {
                name: name.to_owned(),
                location: ParamLocation::Query,
                required: false,
                description: Some(description.to_owned()),
                param_type: "string".to_owned(),
                array: false,
            });
        }
        self
    }

    /// Finish the draft with the responses every operation may return.
    fn build(mut self) -> OperationSpec {
        for (status, description) in [
            (400, "A validation failure or an unsupported query expression"),
            (401, "The request carries no resolvable tenant"),
            (403, "A required permission or bind gate is not granted"),
            (404, "The identifier does not resolve in the calling tenant"),
            (409, "An alias or a route match conflict"),
        ] {
            self.spec.responses.push(ResponseSpec {
                status,
                content_type: toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON,
                description: description.to_owned(),
                schema: Some(ResponseSchema::Ref {
                    schema_name: PROBLEM_SCHEMA_NAME.to_owned(),
                }),
            });
        }
        self.spec
    }
}

/// Start an operation spec for one management endpoint.
fn operation(method: http::Method, path: &[&str], id: &str, summary: &str) -> OperationDraft {
    operation_with_tag(method, path, id, summary, MANAGEMENT_TAG)
}

/// Start an operation spec for one endpoint, with the tag the surface groups under.
fn operation_with_tag(
    method: http::Method,
    path: &[&str],
    id: &str,
    summary: &str,
    tag: &str,
) -> OperationDraft {
    let path = path.concat();
    OperationDraft {
        spec: OperationSpec {
            handler_id: format!("{}:{path}", method.as_str().to_lowercase()),
            method,
            path,
            operation_id: Some(id.to_owned()),
            summary: Some(summary.to_owned()),
            description: None,
            tags: vec![tag.to_owned()],
            params: Vec::new(),
            request_body: None,
            responses: Vec::new(),
            authenticated: true,
            exposed: true,
            rate_limit: None,
            allowed_request_content_types: None,
            vendor_extensions: VendorExtensions::default(),
            license_requirement: None,
        },
    }
}

/// Register the document schemas of the management surface.
fn register_schemas(openapi: &dyn OpenApiRegistry) {
    openapi.ensure_schema_raw(
        UPSTREAM_SCHEMA,
        vec![
            (UPSTREAM_SCHEMA.to_owned(), upstream_schema()),
            (UPSTREAM_BODY_SCHEMA.to_owned(), upstream_body_schema()),
            (ROUTE_SCHEMA.to_owned(), route_schema()),
            (ROUTE_BODY_SCHEMA.to_owned(), route_body_schema()),
        ],
    );
}

/// Schema of the stored upstream representation.
fn upstream_schema() -> RefOr<Schema> {
    let string_property = || ObjectBuilder::new().schema_type(SchemaType::Type(Type::String));
    ObjectBuilder::new()
        .property("id", string_property())
        .required("id")
        .property("tenant_id", string_property())
        .required("tenant_id")
        .property("enabled", boolean_property())
        .property("alias", string_property())
        .required("alias")
        .property("tags", array_property())
        .property("server", server_schema())
        .required("server")
        .property("protocol", string_property())
        .required("protocol")
        .property("auth", override_schema())
        .property("headers", override_schema())
        .property("rate_limit", override_schema())
        .property("cors", override_schema())
        .property("plugins", override_schema())
        .property("created_at", string_property())
        .required("created_at")
        .description(Some(
            "Stored upstream; `auth.config` carries `cred://` references only.",
        ))
        .into()
}

/// Schema of the upstream create/replace body.
fn upstream_body_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property("id", string_property())
        .property("enabled", boolean_property())
        .property("alias", string_property())
        .property("tags", array_property())
        .property("server", server_schema())
        .required("server")
        .property("protocol", string_property())
        .required("protocol")
        .property("auth", override_schema())
        .property("headers", override_schema())
        .property("rate_limit", override_schema())
        .property("cors", override_schema())
        .property("plugins", override_schema())
        .description(Some(
            "Upstream declaration; unknown properties are rejected per \
             `additionalProperties: false` and `alias` defaults to the value derived from \
             `server.endpoints`.",
        ))
        .into()
}

/// Schema of the stored route representation.
fn route_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property("id", string_property())
        .required("id")
        .property("tenant_id", string_property())
        .required("tenant_id")
        .property("upstream_id", string_property())
        .required("upstream_id")
        .property("enabled", boolean_property())
        .property("match", match_schema())
        .required("match")
        .property("match_type", string_property())
        .required("match_type")
        .property("priority", integer_property())
        .required("priority")
        .property("tags", array_property())
        .property("plugins", override_schema())
        .property("rate_limit", override_schema())
        .property("cors", override_schema())
        .property("created_at", string_property())
        .required("created_at")
        .into()
}

/// Schema of the route create/replace body.
fn route_body_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property("id", string_property())
        .property("upstream_id", string_property())
        .required("upstream_id")
        .property("match", match_schema())
        .required("match")
        .property("enabled", boolean_property())
        .property("priority", integer_property())
        .property("tags", array_property())
        .property("plugins", override_schema())
        .property("rate_limit", override_schema())
        .property("cors", override_schema())
        .description(Some(
            "Route declaration; exactly one of `match.http` and `match.grpc` is allowed.",
        ))
        .into()
}

/// Register the document schemas of the plugin surface.
fn register_plugin_schemas(openapi: &dyn OpenApiRegistry) {
    openapi.ensure_schema_raw(
        PLUGIN_SCHEMA,
        vec![
            (PLUGIN_SCHEMA.to_owned(), plugin_schema()),
            (PLUGIN_BODY_SCHEMA.to_owned(), plugin_body_schema()),
        ],
    );
}

/// Schema of the stored plugin definition.
fn plugin_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property("id", string_property())
        .required("id")
        .property("tenant_id", string_property())
        .required("tenant_id")
        .property("plugin_type", string_property())
        .required("plugin_type")
        .property("name", string_property())
        .required("name")
        .property("description", string_property())
        .property("config_schema", config_schema_schema())
        .property("phases", array_property())
        .property("source_code", string_property())
        .required("source_code")
        .property("last_used_at", string_property())
        .property("gc_eligible_at", string_property())
        .description(Some(
            "Stored plugin definition; `id` is the anonymous GTS identifier              `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` and the definition is immutable.              `last_used_at` and `gc_eligible_at` stay unset: no usage tracking and no GC job              exist in this deployment. The definition carries configuration metadata and script              source only, never credential material.",
        ))
        .into()
}

/// Schema of the plugin create body.
fn plugin_body_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property("id", string_property())
        .property("tenant_id", string_property())
        .property("plugin_type", string_property())
        .required("plugin_type")
        .property("name", string_property())
        .required("name")
        .property("description", string_property())
        .property("config_schema", config_schema_schema())
        .property("phases", array_property())
        .property("source_code", string_property())
        .property("last_used_at", string_property())
        .property("gc_eligible_at", string_property())
        .description(Some(
            "Plugin definition to store; unknown properties are rejected per              `additionalProperties: false`. The read-only storage metadata members (`id`,              `tenant_id`, `last_used_at`, `gc_eligible_at`) are stamped by the server, so a              supplied value is ignored.",
        ))
        .into()
}

/// `config_schema`: the declared configuration contract, a JSON object.
fn config_schema_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::Object))
        .additional_properties(Some(AdditionalProperties::FreeForm(true)))
        .description(Some(
            "Declared configuration contract of the plugin, stored verbatim and never              interpreted on the management path.",
        ))
        .into()
}

/// `server` member: the endpoint pool.
fn server_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property(
            "endpoints",
            ArrayBuilder::new().items(endpoint_schema()),
        )
        .required("endpoints")
        .into()
}

/// One `server.endpoints[]` entry.
fn endpoint_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property("scheme", string_property())
        .required("scheme")
        .property("host", string_property())
        .required("host")
        .property(
            "port",
            ObjectBuilder::new()
                .schema_type(SchemaType::Type(Type::Integer))
                .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int32))),
        )
        .required("port")
        .into()
}

/// `match` member: exactly one of `http` and `grpc`.
fn match_schema() -> RefOr<Schema> {
    let http = ObjectBuilder::new()
        .property("methods", array_property())
        .required("methods")
        .property("path", string_property())
        .required("path")
        .property("query_allowlist", array_property())
        .property("path_suffix_mode", string_property());
    let grpc = ObjectBuilder::new()
        .property("service", string_property())
        .required("service")
        .property("method", string_property())
        .required("method");
    OneOfBuilder::new()
        .item(http)
        .item(grpc)
        .into()
}

/// An override member (`auth`, `plugins`, `rate_limit` or `cors`): a free-form
/// object carrying the declaration and, where the schema allows it, `sharing`.
fn override_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::Object))
        .additional_properties(Some(AdditionalProperties::FreeForm(true)))
        .into()
}

fn boolean_property() -> RefOr<Schema> {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::Boolean))
        .into()
}

fn integer_property() -> RefOr<Schema> {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::Integer))
        .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int32)))
        .into()
}

fn string_property() -> RefOr<Schema> {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::String))
        .into()
}

fn array_property() -> RefOr<Schema> {
    ArrayBuilder::new()
        .items(string_property())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorder implementing the host OpenAPI registry contract.
    #[derive(Default)]
    struct RecordingRegistry {
        schemas: std::sync::Mutex<Vec<String>>,
        operations: std::sync::Mutex<Vec<String>>,
    }

    impl OpenApiRegistry for RecordingRegistry {
        fn register_operation(&self, spec: &toolkit::api::operation_builder::OperationSpec) {
            self.operations
                .lock()
                .expect("operations lock")
                .push(format!("{} {}", spec.method, spec.path));
        }

        fn ensure_schema_raw(
            &self,
            name: &str,
            schemas: Vec<(String, RefOr<Schema>)>,
        ) -> String {
            self.schemas
                .lock()
                .expect("schemas lock")
                .extend(schemas.into_iter().map(|(name, _)| name));
            name.to_owned()
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn twenty_seven_operations_are_registered_with_absolute_paths() {
        let registry = RecordingRegistry::default();
        register(&registry);

        let operations = registry
            .operations
            .lock()
            .expect("operations lock")
            .clone();
        assert_eq!(operations.len(), 27, "{operations:?}");
        for (method, path) in [
            ("POST", "/oagw/v1/upstreams"),
            ("GET", "/oagw/v1/upstreams"),
            ("GET", "/oagw/v1/upstreams/{id}"),
            ("PUT", "/oagw/v1/upstreams/{id}"),
            ("DELETE", "/oagw/v1/upstreams/{id}"),
            ("POST", "/oagw/v1/routes"),
            ("GET", "/oagw/v1/routes"),
            ("GET", "/oagw/v1/routes/{id}"),
            ("PUT", "/oagw/v1/routes/{id}"),
            ("DELETE", "/oagw/v1/routes/{id}"),
            ("POST", "/oagw/v1/plugins"),
            ("GET", "/oagw/v1/plugins"),
            ("GET", "/oagw/v1/plugins/{id}"),
            ("DELETE", "/oagw/v1/plugins/{id}"),
            ("GET", "/oagw/v1/plugins/{id}/source"),
        ] {
            let expected = format!("{method} {path}");
            assert!(
                operations.contains(&expected),
                "{expected} missing: {operations:?}"
            );
        }
    }

    #[test]
    fn the_proxy_operations_cover_both_paths_and_the_forwarded_methods() {
        // @cpt-begin:cpt-cf-oagw-dod-proxy-endpoint:p1:inst-full
        let registry = RecordingRegistry::default();
        register(&registry);

        let operations = registry
            .operations
            .lock()
            .expect("operations lock")
            .clone();
        for method in ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
            for path in [
                "/oagw/v1/proxy/{alias}",
                "/oagw/v1/proxy/{alias}/{path_suffix}",
            ] {
                let expected = format!("{method} {path}");
                assert!(
                    operations.contains(&expected),
                    "{expected} missing: {operations:?}"
                );
            }
        }
        // The management surface is untouched: the proxy paths are the only
        // additions and no management path is re-registered here.
        assert!(operations.iter().all(|operation| {
            let (_, path) = operation.split_once(' ').unwrap_or(("", ""));
            !path.starts_with("/oagw/v1/proxy") || path.contains("{alias}")
        }));
        // @cpt-end:cpt-cf-oagw-dod-proxy-endpoint:p1:inst-full
    }

    #[test]
    fn only_the_preflight_proxy_operations_are_unauthenticated() {
        let specs = proxy_operations();
        assert_eq!(specs.len(), 12, "{:?}", specs.len());
        for spec in &specs {
            let is_preflight = spec.method == http::Method::OPTIONS;
            assert_eq!(
                spec.authenticated, !is_preflight,
                "{}",
                spec.summary.clone().unwrap_or_default()
            );
            // A passthrough response is the upstream's own response, so the
            // document names no JSON component for it.
            let success = spec
                .responses
                .iter()
                .find(|response| response.status < 400)
                .expect("a success response");
            assert!(
                success.schema.is_none(),
                "{} carries no document schema",
                spec.summary.clone().unwrap_or_default()
            );
            for status in [400, 401, 403, 404, 413, 502, 503, 504] {
                assert!(
                    spec.responses.iter().any(|response| response.status == status),
                    "{:?} {} misses the {status} response",
                    spec.method,
                    spec.path
                );
            }
        }
        // The target-host routing header is documented on the actual calls only:
        // a preflight resolves no upstream at all.
        for spec in &specs {
            let documented = spec
                .params
                .iter()
                .any(|param| param.name == "X-OAGW-Target-Host");
            assert_eq!(
                documented,
                spec.method != http::Method::OPTIONS,
                "{}",
                spec.path
            );
        }
    }

    #[test]
    fn every_operation_requires_authentication_and_carries_the_problem_contract() {
        let operations = operations();
        for spec in &operations {
            let summary = spec.summary.clone().unwrap_or_default();
            assert!(spec.authenticated, "{summary}");
            for status in [400, 401, 403, 404, 409] {
                assert!(
                    spec.responses.iter().any(|response| response.status == status),
                    "{summary} misses the {status} response"
                );
            }
        }
    }

    #[test]
    fn no_replace_operation_is_registered_for_a_plugin_definition() {
        // @cpt-begin:cpt-cf-oagw-dod-plugin-immutability:p1:inst-full
        let operations = plugin_operations();
        assert_eq!(operations.len(), 5, "{operations:?}");
        assert!(
            operations
                .iter()
                .all(|spec| spec.method != http::Method::PUT),
            "a definition is immutable, so the registry carries no replace operation"
        );
        // @cpt-end:cpt-cf-oagw-dod-plugin-immutability:p1:inst-full
    }

    #[test]
    fn the_source_operation_records_the_verbatim_media_type() {
        // @cpt-begin:cpt-cf-oagw-dod-plugin-source-endpoint:p1:inst-full
        let spec = plugin_operations()
            .into_iter()
            .find(|spec| spec.path.ends_with("/source"))
            .expect("the source operation is registered");
        let response = spec
            .responses
            .iter()
            .find(|response| response.status == 200)
            .expect("the source operation carries a 200");
        assert_eq!(response.content_type, "text/plain");
        assert!(response.schema.is_none(), "the body is not a JSON document");
        // @cpt-end:cpt-cf-oagw-dod-plugin-source-endpoint:p1:inst-full
    }

    #[test]
    fn the_document_schemas_are_registered() {
        let registry = RecordingRegistry::default();
        register(&registry);

        let schemas = registry.schemas.lock().expect("schemas lock").clone();
        for name in [
            UPSTREAM_SCHEMA,
            UPSTREAM_BODY_SCHEMA,
            ROUTE_SCHEMA,
            ROUTE_BODY_SCHEMA,
            PLUGIN_SCHEMA,
            PLUGIN_BODY_SCHEMA,
        ] {
            assert!(schemas.contains(&name.to_owned()), "{name} missing");
        }
    }

    #[test]
    fn the_plugin_schema_covers_the_stored_definition() {
        let rendered = serde_json::to_value(plugin_schema())
            .expect("schema is serializable")
            .to_string();
        for member in [
            "id", "tenant_id", "plugin_type", "name", "description", "config_schema", "phases",
            "source_code", "last_used_at", "gc_eligible_at",
        ] {
            assert!(rendered.contains(member), "{member} missing: {rendered}");
        }
    }

    #[test]
    fn the_upstream_schema_covers_the_stored_representation() {
        let rendered = serde_json::to_value(upstream_schema())
            .expect("schema is serializable")
            .to_string();
        for member in [
            "\"id\"", "\"tenant_id\"", "\"enabled\"", "\"alias\"", "\"tags\"", "\"server\"",
            "\"protocol\"", "\"auth\"", "\"headers\"", "\"rate_limit\"", "\"cors\"",
            "\"plugins\"", "\"created_at\"",
        ] {
            assert!(rendered.contains(member), "{member} missing: {rendered}");
        }
    }

    #[test]
    fn the_route_schema_covers_the_match_rule() {
        let rendered = serde_json::to_value(route_schema())
            .expect("schema is serializable")
            .to_string();
        for member in [
            "upstream_id",
            "match",
            "match_type",
            "priority",
            "methods",
            "path_suffix_mode",
            "query_allowlist",
        ] {
            assert!(rendered.contains(member), "{member} missing: {rendered}");
        }
    }
}
