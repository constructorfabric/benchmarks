//! Leader-run background workers: orphan watchdog (DESIGN §4) and upload
//! reaper (Appendix B.9.5). Without the `k8s` feature the elector is a no-op
//! (every instance is leader); the CAS guards prevent double processing.

use std::sync::Arc;
use std::time::Duration;

use chrono::Duration as ChronoDuration;
use mini_chat_sdk::ModelTier;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;

use crate::domain::error::DomainError;
use crate::domain::service::attachments::AttachmentCleanupPayload;
use crate::domain::service::finalize::finalize_orphan;
use crate::domain::service::{AppServices, now, policy};
use crate::infra::db::entity::{attachments, chat_turns};
use crate::infra::outbox::Wakes;

const SCAN_LIMIT: u64 = 100;

/// One orphan-watchdog scan; returns the number of finalized turns.
///
/// # Errors
/// Database failure.
pub async fn orphan_scan(svc: &AppServices) -> Result<usize, DomainError> {
    let cutoff = now() - ChronoDuration::seconds(i64::try_from(svc.cfg.orphan_watchdog.timeout_secs).unwrap_or(300));
    let conn = svc.conn()?;
    let candidates = chat_turns::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(chat_turns::Column::State.eq("running"))
                .add(chat_turns::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(chat_turns::Column::LastProgressAt.lte(cutoff))
                        .add(
                            Condition::all()
                                .add(chat_turns::Column::LastProgressAt.is_null())
                                .add(chat_turns::Column::StartedAt.lte(cutoff)),
                        ),
                ),
        )
        .limit(SCAN_LIMIT)
        .all(&conn)
        .await?;
    drop(conn);
    let mut finalized = 0;
    for turn in candidates {
        tracing::info!(turn_id = %turn.id, "orphan turn detected (stale_progress)");
        let mut mult = None;
        if let (Some(v), Some(model)) = (turn.policy_version_applied, turn.effective_model.clone()) {
            let user = turn.requester_user_id.unwrap_or(DEFAULT_SUBJECT_ID);
            if let Ok(snap) =
                policy::snapshot_version(svc.policy.as_ref(), user, u64::try_from(v).unwrap_or(0)).await
                && let Some(m) = snap.model(&model)
            {
                mult = Some((
                    m.input_tokens_credit_multiplier_micro,
                    m.output_tokens_credit_multiplier_micro,
                    m.tier == ModelTier::Premium,
                ));
            }
        }
        match finalize_orphan(svc, turn, cutoff, mult).await {
            Ok(true) => finalized += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "orphan finalization failed"),
        }
    }
    Ok(finalized)
}

/// One upload-reaper scan; returns the number of reaped rows.
///
/// # Errors
/// Database failure.
pub async fn upload_reaper_scan(svc: &AppServices) -> Result<usize, DomainError> {
    let cutoff = now() - ChronoDuration::seconds(i64::try_from(svc.cfg.upload_reaper.stale_after_secs).unwrap_or(300));
    let conn = svc.conn()?;
    let rows = attachments::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(attachments::Column::Status.is_in(["pending", "uploaded"]))
                .add(attachments::Column::DeletedAt.is_null())
                .add(attachments::Column::CleanupStatus.is_null())
                .add(attachments::Column::UpdatedAt.lt(cutoff)),
        )
        .order_by(attachments::Column::UpdatedAt, sea_orm::Order::Asc)
        .limit(SCAN_LIMIT)
        .all(&conn)
        .await?;
    drop(conn);
    let mut reaped = 0;
    for row in rows {
        let outbox = Arc::clone(&svc.outbox);
        let res = svc
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    let ts = now();
                    let mut q = attachments::Entity::update_many()
                        .col_expr(attachments::Column::Status, Expr::value("failed"))
                        .col_expr(attachments::Column::ErrorCode, Expr::value(Some("upload_abandoned")))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts));
                    if row.provider_file_id.is_some() {
                        q = q
                            .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)));
                    }
                    let n = q
                        .filter(
                            Condition::all()
                                .add(attachments::Column::Id.eq(row.id))
                                .add(attachments::Column::Status.eq(row.status.clone()))
                                .add(attachments::Column::DeletedAt.is_null())
                                .add(attachments::Column::CleanupStatus.is_null())
                                .add(attachments::Column::UpdatedAt.lt(cutoff)),
                        )
                        .secure()
                        .scope_with(&AccessScope::for_tenant(row.tenant_id))
                        .exec(tx)
                        .await?
                        .rows_affected;
                    let mut w = Wakes::default();
                    if n == 0 {
                        return Ok::<_, DomainError>((false, w));
                    }
                    if let Some(fid) = &row.provider_file_id {
                        if row.secondary_file_id.is_some() {
                            tracing::warn!(attachment_id = %row.id, "secondary file of an abandoned upload is not deleted");
                        }
                        let payload = AttachmentCleanupPayload {
                            event_type: "attachment_upload_abandoned".to_owned(),
                            tenant_id: row.tenant_id,
                            chat_id: row.chat_id,
                            attachment_id: row.id,
                            provider_file_id: Some(fid.clone()),
                            vector_store_id: None,
                            storage_backend: row.storage_backend.clone(),
                            attachment_kind: row.attachment_kind.clone(),
                            deleted_at: ts,
                            secondary_ref: None,
                        };
                        w.push(outbox.attachment_cleanup(tx, row.tenant_id, &payload).await?);
                    }
                    Ok((true, w))
                })
            })
            .await;
        match res {
            Ok((true, w)) => {
                w.fire();
                reaped += 1;
            }
            Ok((false, _)) => {}
            Err(e) => tracing::warn!(error = %e, "upload reaper update failed"),
        }
    }
    Ok(reaped)
}

/// Runs a periodic worker until cancelled.
pub async fn run_periodic<F, Fut>(name: &'static str, interval: Duration, cancel: CancellationToken, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<usize, DomainError>>,
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = tick.tick() => {
                match f().await {
                    Ok(n) if n > 0 => tracing::info!(worker = name, processed = n, "worker scan"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(worker = name, error = %e, "worker scan failed"),
                }
            }
        }
    }
}
