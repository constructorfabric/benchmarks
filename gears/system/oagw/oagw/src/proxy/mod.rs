//! Data-Plane proxy engine: alias/route resolution, endpoint-pool
//! selection, header handling, hierarchical config merge, forwarding, and
//! streaming/protocol upgrades.
//!
//! Implemented by DECOMPOSITION entries 2.5 (proxy-core) and 2.6
//! (proxy-streaming). The REST surface this engine backs is
//! `crate::api::rest::proxy`.

pub(crate) mod body;
pub(crate) mod constants;
pub(crate) mod context;
pub(crate) mod endpoint;
pub(crate) mod engine;
pub(crate) mod errors;
pub(crate) mod forward;
pub(crate) mod guards;
pub(crate) mod headers;
pub(crate) mod hierarchy;
pub(crate) mod merge;
pub(crate) mod observe;
pub(crate) mod resolve;
pub(crate) mod route_match;
pub(crate) mod stream;
