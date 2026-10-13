//! `llm_provider` library (DESIGN §3.2, ADR-0001/0005): provider resolution,
//! the gear's S2S identity, the SSE parser and the provider adapters
//! ([`providers::OagwLlmClient`]).

pub mod knowledge;
pub mod providers;
pub mod resolver;
pub mod sse;
pub mod storage;
pub mod types;

#[cfg(test)]
pub(crate) mod fake_gw;

use std::time::Duration;

use tokio::sync::watch;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

pub use resolver::ProviderResolver;
pub use types::{ResolvedProvider, ResolvedStorage};

/// How long [`ServiceIdentity::get`] waits for the identity (Ruling R6b).
const IDENTITY_WAIT: Duration = Duration::from_secs(10);

/// The gear's S2S `SecurityContext` (Ruling R2: every OAGW call uses it). Set
/// once at start after the client-credentials exchange.
pub struct ServiceIdentity {
    tx: watch::Sender<Option<SecurityContext>>,
}

impl Default for ServiceIdentity {
    fn default() -> Self {
        Self {
            tx: watch::Sender::new(None),
        }
    }
}

impl ServiceIdentity {
    /// The identity, waiting up to 10 s for [`Self::set`].
    ///
    /// # Errors
    /// `ProviderResolution("service identity not ready")` when it is still
    /// unset after the wait.
    pub async fn get(&self) -> Result<SecurityContext, DomainError> {
        let mut rx = self.tx.subscribe();
        let waited = tokio::time::timeout(IDENTITY_WAIT, async {
            rx.wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|v| v.clone())
        })
        .await;
        waited
            .ok()
            .flatten()
            .ok_or_else(|| DomainError::ProviderResolution("service identity not ready".to_owned()))
    }

    pub fn set(&self, ctx: SecurityContext) {
        self.tx.send_replace(Some(ctx));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::test_support::test_ctx;

    #[tokio::test(start_paused = true)]
    async fn service_identity_times_out_after_10s_when_unset() {
        let id = ServiceIdentity::default();
        let started = tokio::time::Instant::now();
        assert_eq!(
            id.get().await.unwrap_err(),
            DomainError::ProviderResolution("service identity not ready".to_owned())
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_secs(10), "waited {waited:?}");
        assert!(waited < Duration::from_secs(11), "waited {waited:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn service_identity_get_waits_for_set() {
        let id = Arc::new(ServiceIdentity::default());
        let ctx = test_ctx();
        let setter = {
            let id = Arc::clone(&id);
            let ctx = ctx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                id.set(ctx);
            })
        };
        let got = id.get().await.unwrap();
        assert_eq!(got.subject_id(), ctx.subject_id());
        assert_eq!(got.subject_tenant_id(), ctx.subject_tenant_id());
        setter.await.unwrap();
        // Already set: returns immediately.
        let again = id.get().await.unwrap();
        assert_eq!(again.subject_id(), ctx.subject_id());
    }
}
