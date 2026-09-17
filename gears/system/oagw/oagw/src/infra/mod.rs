//! Infrastructure layer of the OAGW gear: platform integrations that the
//! domain layer does not depend on (DESIGN §3.2).

pub mod plugin;
pub mod tenant_hierarchy;
pub mod type_provisioning;
