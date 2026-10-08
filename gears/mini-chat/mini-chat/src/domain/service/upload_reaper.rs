//! Upload reaper (OWNER: attachments).
//!
//! Fails uploads left `pending` / `uploaded` by a dropped request or a stopped process
//! (B.9.5) and hands their provider file to the attachment cleanup queue.

use std::sync::Arc;
use std::time::Duration;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;

use crate::domain::error::DomainError;
use crate::domain::outbox_payloads::{AttachmentCleanupEvent, attachment_event_types};
use crate::domain::service::Deps;
use crate::domain::service::attachments::{cleanup_status, error_codes, status};
use crate::infra::db::entity::attachment;

/// Rows per scan (not configurable).
pub const SCAN_LIMIT: u64 = 100;

/// Runs the reaper loop until `cancel` fires.
pub async fn run(deps: Arc<Deps>, cancel: CancellationToken) {
    if !deps.cfg.upload_reaper.enabled {
        cancel.cancelled().await;
        return;
    }
    let interval = Duration::from_secs(deps.cfg.upload_reaper.scan_interval_secs.max(1));
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
        match scan_once(&deps).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(reaped = n, "upload reaper failed abandoned uploads"),
            Err(e) => tracing::warn!(error = %e, "upload reaper scan failed"),
        }
    }
}

/// One scan: fails up to [`SCAN_LIMIT`] stale `pending` / `uploaded` rows, oldest first.
///
/// # Errors
/// DB failure of the scan query.
pub async fn scan_once(deps: &Deps) -> Result<usize, DomainError> {
    let now = OffsetDateTime::now_utc();
    let stale = i64::try_from(deps.cfg.upload_reaper.stale_after_secs).unwrap_or(i64::MAX);
    let cutoff = now - time::Duration::seconds(stale);
    let rows = {
        let conn = deps.db.conn()?;
        attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::Status.is_in([status::PENDING, status::UPLOADED]))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::CleanupStatus.is_null())
                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
            )
            .order_by_asc(attachment::Column::UpdatedAt)
            .limit(SCAN_LIMIT)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?
    };
    let mut reaped = 0;
    for row in rows {
        match reap_one(deps, &row, cutoff).await {
            Ok(true) => {
                reaped += 1;
                if row.secondary_file_id.is_some() {
                    tracing::warn!(attachment_id = %row.id, secondary_file_id = ?row.secondary_file_id,
                        "abandoned upload has a secondary copy that is not deleted");
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(attachment_id = %row.id, error = %e, "failed to reap abandoned upload"),
        }
    }
    Ok(reaped)
}

/// Per row, one transaction: CAS to `failed` / `upload_abandoned`, plus a cleanup event when
/// the row has a provider file. Returns false when the row changed meanwhile.
async fn reap_one(
    deps: &Deps,
    row: &attachment::Model,
    cutoff: OffsetDateTime,
) -> Result<bool, DomainError> {
    let outbox = Arc::clone(&deps.outbox);
    let row = row.clone();
    let wake = deps
        .db
        .transaction(move |tx| {
            let outbox = Arc::clone(&outbox);
            let row = row.clone();
            Box::pin(async move {
                let now = OffsetDateTime::now_utc();
                let mut q = attachment::Entity::update_many()
                    .col_expr(attachment::Column::Status, Expr::value(status::FAILED))
                    .col_expr(
                        attachment::Column::ErrorCode,
                        Expr::value(error_codes::UPLOAD_ABANDONED),
                    )
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
                if row.provider_file_id.is_some() {
                    q = q
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(cleanup_status::PENDING),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)));
                }
                let affected = q
                    .filter(
                        Condition::all()
                            .add(attachment::Column::Id.eq(row.id))
                            .add(attachment::Column::ChatId.eq(row.chat_id))
                            .add(attachment::Column::Status.eq(row.status.clone()))
                            .add(attachment::Column::DeletedAt.is_null())
                            .add(attachment::Column::CleanupStatus.is_null())
                            .add(attachment::Column::UpdatedAt.lt(cutoff)),
                    )
                    .secure()
                    .scope_with(&AccessScope::for_tenant(row.tenant_id))
                    .exec(tx)
                    .await?
                    .rows_affected;
                if affected == 0 {
                    return Ok::<_, DomainError>(None);
                }
                if row.provider_file_id.is_none() {
                    return Ok(Some(None));
                }
                let ev = AttachmentCleanupEvent {
                    event_type: attachment_event_types::UPLOAD_ABANDONED.to_owned(),
                    tenant_id: row.tenant_id,
                    chat_id: row.chat_id,
                    attachment_id: row.id,
                    provider_file_id: row.provider_file_id.clone(),
                    vector_store_id: None,
                    storage_backend: row.storage_backend.clone(),
                    attachment_kind: row.attachment_kind.clone(),
                    deleted_at: now,
                    secondary_ref: None,
                };
                let wake = outbox.enqueue_attachment_cleanup(tx, &ev).await?;
                Ok(Some(Some(wake)))
            })
        })
        .await?;
    match wake {
        None => Ok(false),
        Some(w) => {
            if let Some(w) = w {
                w.fire();
            }
            Ok(true)
        }
    }
}

#[cfg(test)]
#[path = "upload_reaper_tests.rs"]
mod upload_reaper_tests;
