//! The Secure ORM implementation of the [`OrphanStore`] port: the cross-tenant
//! scan of stale `running` turns and the orphan finalization transaction
//! (`cpt-cf-mini-chat-algo-background-workers-run-watchdog`,
//! `cpt-cf-mini-chat-algo-background-workers-finalize-orphan`).
//!
//! The watchdog is a system worker without a tenant or user context: its scan
//! and its CAS use the `allow_all` system boundary documented by the quota
//! helper, and the CAS is guarded by the turn id of the scan, the `running`
//! state, `deleted_at IS NULL` and the stale-progress predicate.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::TurnAuditEvent;
use sea_orm::sea_query::NullOrdering;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::SecureEntityExt;
use toolkit_security::AccessScope;

use super::quota::SeaQuotaRepository;
use crate::domain::error::DomainError;
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult};
use crate::domain::service::workers::orphan::OrphanCandidate;
use crate::domain::service::workers::ports::OrphanStore;
use crate::infra::db::entities::chat_turn;
use crate::infra::db::mapping::scope_error;
use crate::infra::outbox::audit::enqueue_audit_event;
use crate::infra::outbox::queues::OutboxEnqueuer;

/// Secure ORM store of the orphan watchdog.
pub struct SeaOrphanStore {
    db: Arc<DBProvider<DomainError>>,
    quota: Arc<SeaQuotaRepository>,
    outbox: Arc<OutboxEnqueuer>,
}

impl SeaOrphanStore {
    /// Creates the store over the database provider, the quota store (whose
    /// finalization transaction it extends) and the outbox enqueuer (the audit
    /// queue).
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        quota: Arc<SeaQuotaRepository>,
        outbox: Arc<OutboxEnqueuer>,
    ) -> Self {
        Self { db, quota, outbox }
    }
}

#[async_trait]
impl OrphanStore for SeaOrphanStore {
    async fn scan(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<OrphanCandidate>, DomainError> {
        use chat_turn::Column as C;
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-select-orphans
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-stream-lost
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-to-stale
        let stale = Condition::any().add(C::LastProgressAt.lte(cutoff)).add(
            Condition::all()
                .add(C::LastProgressAt.is_null())
                .add(C::StartedAt.lte(cutoff)),
        );
        let rows = chat_turn::Entity::find()
            .filter(
                Condition::all()
                    .add(C::State.eq("running"))
                    .add(C::DeletedAt.is_null())
                    .add(stale),
            )
            .order_by_with_nulls(C::LastProgressAt, Order::Asc, NullOrdering::First)
            .order_by(C::StartedAt, Order::Asc)
            .order_by(C::Id, Order::Asc)
            .limit(u64::from(limit))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-to-stale
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-stream-lost
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-select-orphans
        Ok(rows
            .into_iter()
            .map(|row| OrphanCandidate {
                id: row.id,
                tenant_id: row.tenant_id,
                chat_id: row.chat_id,
                request_id: row.request_id,
                requester_user_id: row.requester_user_id,
                effective_model: row.effective_model,
                policy_version_applied: row.policy_version_applied,
                started_at: row.started_at,
                web_search_completed_count: row.web_search_completed_count,
                file_search_completed_count: row.file_search_completed_count,
            })
            .collect())
    }

    async fn finalize(
        &self,
        request: &FinalizeRequest,
        audit: Option<&TurnAuditEvent>,
    ) -> Result<FinalizeResult, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-orphan-no-messages
        let wakes: Arc<Mutex<Vec<Wake>>> = Arc::new(Mutex::new(Vec::new()));
        let audit = Arc::new(audit.cloned());
        let outbox = Arc::clone(&self.outbox);
        let result = {
            let wakes = Arc::clone(&wakes);
            self.quota
                .finalize_turn_with(request, move |tx, won| {
                    let (audit, outbox, wakes) =
                        (Arc::clone(&audit), Arc::clone(&outbox), Arc::clone(&wakes));
                    let usage_enqueued = won.usage_event_enqueued;
                    Box::pin(async move {
                        if let Ok(mut held) = wakes.lock() {
                            held.clear();
                        }
                        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-enqueue-orphan-audit
                        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-orphan-no-requester
                        if usage_enqueued && let Some(event) = audit.as_ref() {
                            let wake = enqueue_audit_event(tx, &outbox, event).await?;
                            if let Ok(mut held) = wakes.lock() {
                                held.push(wake);
                            }
                        }
                        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-orphan-no-requester
                        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-enqueue-orphan-audit
                        Ok(())
                    })
                })
                .await?
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-orphan-no-messages
        if let Ok(mut held) = wakes.lock() {
            held.drain(..).for_each(Wake::fire);
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "orphans_tests.rs"]
mod tests;
