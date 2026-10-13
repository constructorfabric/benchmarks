//! The gear's late-bound runtime handles: the background task registry and the
//! S2S security context exist only after `start`, while the services are built
//! at `init`.

use std::sync::{Arc, OnceLock};

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

use crate::domain::service::attachments::ports::BackgroundRuntime;
use crate::infra::background_tasks::BackgroundTasks;

/// Slot the gear fills at `start`.
#[derive(Default)]
pub struct GearRuntime {
    tasks: OnceLock<Arc<BackgroundTasks>>,
    s2s: OnceLock<SecurityContext>,
    unbound_stop: CancellationToken,
}

impl GearRuntime {
    /// An unbound runtime.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the registry and the S2S context; `false` when already bound.
    pub fn bind(&self, tasks: &Arc<BackgroundTasks>, s2s: SecurityContext) -> bool {
        if self.tasks.set(Arc::clone(tasks)).is_err() {
            return false;
        }
        self.s2s.set(s2s).is_ok()
    }
}

impl BackgroundRuntime for GearRuntime {
    fn spawn(&self, name: &'static str, task: BoxFuture<'static, ()>) -> bool {
        match self.tasks.get() {
            Some(tasks) => {
                tasks.spawn(name, task);
                true
            }
            None => false,
        }
    }

    fn stop_token(&self) -> CancellationToken {
        self.tasks
            .get()
            .map_or_else(|| self.unbound_stop.clone(), |tasks| tasks.token())
    }

    fn s2s_context(&self) -> Option<SecurityContext> {
        self.s2s.get().cloned()
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
