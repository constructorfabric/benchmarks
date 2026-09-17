//! Authorization resource types for OAGW (used with the `PolicyEnforcer`).

use authz_resolver_sdk::ResourceType;
use toolkit_security::pep_properties::{OWNER_TENANT_ID, RESOURCE_ID};

use crate::domain::model::PluginKind;

/// Upstream CRUD resource (`create` / `override` / `read` / `delete`).
pub const UPSTREAM_RESOURCE: ResourceType = ResourceType::from_static(
    crate::domain::gts::UPSTREAM_TYPE,
    &[OWNER_TENANT_ID, RESOURCE_ID],
);

/// Route CRUD resource.
pub const ROUTE_RESOURCE: ResourceType =
    ResourceType::from_static(crate::domain::gts::ROUTE_TYPE, &[OWNER_TENANT_ID, RESOURCE_ID]);

/// Proxy invocation resource (`invoke`).
pub const PROXY_RESOURCE: ResourceType = ResourceType::from_static(
    crate::domain::gts::PROXY_TYPE,
    &[OWNER_TENANT_ID],
);

/// Custom-plugin CRUD resource for a given kind.
#[must_use]
pub fn plugin_resource(kind: PluginKind) -> ResourceType {
    ResourceType::new(kind.base_type().to_owned(), &[OWNER_TENANT_ID])
}
