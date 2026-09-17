//! Domain services.
pub mod management;

pub use management::{
    BUILT_IN_PLUGINS, BuiltInPlugin, ControlPlane, ListLimits, PluginSpec, ResolvedPlugin,
    RouteSpec, UpstreamSpec, built_in_plugin,
};
