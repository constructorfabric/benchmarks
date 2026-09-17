//! REST handlers for the OAGW management API and the proxy data plane.
//!
//! Handlers are thin adapters: they extract, call
//! [`crate::domain::services::ManagementService`] (management) or
//! [`crate::infra::proxy::pipeline::DataPlaneService`] (proxy), and convert the
//! result. All business rules live in the domain / infra layers.

use std::sync::Arc;

use crate::domain::services::ManagementService;
use crate::infra::proxy::pipeline::DataPlaneService;

pub(crate) mod plugins;
pub(crate) mod proxy;
pub(crate) mod routes;
pub(crate) mod upstreams;

/// Shared control-plane handle installed by [`crate::gear`].
pub type SharedService = Arc<ManagementService>;

/// Shared data-plane handle installed by [`crate::gear`].
pub type SharedDataPlane = Arc<DataPlaneService>;
