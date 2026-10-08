//! System Context - runtime internals exposed to system gears

use std::sync::Arc;
use uuid::Uuid;

use crate::runtime::{GearManager, GrpcInstallerStore};

/// System-level context provided to system gears during the wiring phase.
///
/// This gives system gears access to runtime internals like the gear manager
/// and gRPC installer store. Only gears with the "system" capability receive this.
///
/// Normal user gears do not see `SystemContext` - they only get `GearCtx` during init.
pub struct SystemContext {
    /// Process-level instance ID (shared by all gears in this process)
    instance_id: Uuid,

    /// Gear instance registry and manager
    pub gear_manager: Arc<GearManager>,

    /// gRPC service installer store
    pub grpc_installers: Arc<GrpcInstallerStore>,
}

impl SystemContext {
    /// Create a new system context from runtime components
    pub fn new(
        instance_id: Uuid,
        gear_manager: Arc<GearManager>,
        grpc_installers: Arc<GrpcInstallerStore>,
    ) -> Self {
        Self {
            instance_id,
            gear_manager,
            grpc_installers,
        }
    }

    /// Returns the process-level instance ID.
    ///
    /// This is a unique identifier for this process instance, shared by all gears
    /// in the same process. It is generated once at bootstrap.
    #[inline]
    #[must_use]
    pub fn instance_id(&self) -> Uuid {
        self.instance_id
    }
}
