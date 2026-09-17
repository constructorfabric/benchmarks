//! Built-in plugin implementations and registries.
//!
//! Named (built-in) plugins are resolved in-process; custom UUID-backed
//! plugins are stored in the control plane and executed through the same
//! trait surface (DESIGN "Plugin Identification Model").

mod apikey;
mod noop;
mod oauth2_client_cred;
mod registry;
mod request_id;
mod required_headers;
mod resolver;

pub use registry::{
    AuthPluginRegistry, BuiltinPluginAvailability, GuardPluginRegistry, TransformPluginRegistry,
};
pub use resolver::{ResolvingPluginValidator, family_of};
