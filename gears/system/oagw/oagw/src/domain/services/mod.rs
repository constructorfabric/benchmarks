//! Domain service layer.
//!
//! * [`management`] — the control-plane [`ControlPlaneService`]: tenant-scoped
//!   CRUD with alias enforcement, alias-update rules, ancestor-bind checks,
//!   plugin-reference resolution and route match uniqueness.
//! * [`DataPlaneService`] — trait abstraction over the outbound proxy flow
//!   (implemented by `infra/proxy`).
//!
//! The tenant-hierarchy walk used by both planes lives here: `resolve_chain`
//! produces the ancestor-merged [`crate::domain::merge::ResolvedChain`] from
//! an alias.

pub mod management;

pub use management::{ControlPlaneService, PluginValidator};

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::domain::dto::{ProxyIdentity, ProxyRequest, ProxyResponse};
use crate::domain::error::OagwResult;

/// Data-plane service abstraction (DESIGN §3.2 "Internal Services").
///
/// `execute_proxy` runs the full proxy flow: CORS → rate limit → auth →
/// guards → transform(request) → upstream call → transform(response/error).
#[async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Execute the proxy flow for an inbound request targeting `alias`.
    ///
    /// # Errors
    ///
    /// Every failure maps to an [`OagwError`] with the DESIGN GTS type.
    ///
    /// [`OagwError`]: crate::domain::error::OagwError
    async fn execute_proxy(
        &self,
        ctx: &SecurityContext,
        identity: ProxyIdentity,
        alias: &str,
        req: ProxyRequest,
    ) -> OagwResult<ProxyResponse>;
}
