//! Upload reaper: fails `pending` / `uploaded` rows abandoned by a dropped request or a stopped
//! process, and hands their provider files to the attachment cleanup (DESIGN B.9.5).

use std::sync::Arc;
use std::time::Duration;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;

use crate::clock;
use crate::domain::attachments::{
    CLEANUP_PENDING, ERR_UPLOAD_ABANDONED, EVENT_UPLOAD_ABANDONED, STATUS_FAILED, STATUS_PENDING, STATUS_UPLOADED,
    cleanup_payload,
};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::attachment;

/// Rows handled per scan (not configurable).
pub const SCAN_LIMIT: u64 = 100;

fn stale_condition(statuses: Vec<String>, cutoff: OffsetDateTime) -> Condition {
    Condition::all()
        .add(attachment::Column::Status.is_in(statuses))
        .add(attachment::Column::DeletedAt.is_null())
        .add(attachment::Column::CleanupStatus.is_null())
        .add(attachment::Column::UpdatedAt.lt(cutoff))
}

/// One scan at `now`: returns the number of rows marked `upload_abandoned`.
///
/// # Errors
/// DB errors of the selection query (per-row failures are logged and skipped).
pub async fn scan_once(app: &AppServices, now: OffsetDateTime) -> Result<usize, DomainError> {
    let stale_after = i64::try_from(app.cfg.upload_reaper.stale_after_secs).unwrap_or(i64::MAX);
    let cutoff = clock::normalize(now - time::Duration::seconds(stale_after));
    let now = clock::normalize(now);
    let statuses = vec![STATUS_PENDING.to_owned(), STATUS_UPLOADED.to_owned()];
    let rows = {
        let conn = app.db.conn()?;
        attachment::Entity::find()
            .filter(stale_condition(statuses, cutoff))
            .order_by(attachment::Column::UpdatedAt, Order::Asc)
            .limit(SCAN_LIMIT)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?
    };
    let mut reaped = 0;
    for row in rows {
        let from_status = row.status.clone();
        let id = row.id;
        match reap_row(app, row, cutoff, now).await {
            Ok(true) => {
                reaped += 1;
                tracing::info!(attachment_id = %id, %from_status, "upload abandoned");
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(attachment_id = %id, error = %e, "reaping an abandoned upload failed"),
        }
    }
    Ok(reaped)
}

async fn reap_row(app: &AppServices, row: attachment::Model, cutoff: OffsetDateTime, now: OffsetDateTime) -> Result<bool, DomainError> {
    let outbox = Arc::clone(&app.outbox);
    let wake = app
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                let has_file = row.provider_file_id.is_some();
                let mut q = attachment::Entity::update_many()
                    .col_expr(attachment::Column::Status, Expr::value(STATUS_FAILED))
                    .col_expr(attachment::Column::ErrorCode, Expr::value(Some(ERR_UPLOAD_ABANDONED.to_owned())))
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
                if has_file {
                    q = q
                        .col_expr(attachment::Column::CleanupStatus, Expr::value(Some(CLEANUP_PENDING.to_owned())))
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)));
                }
                let res = q
                    .filter(stale_condition(vec![row.status.clone()], cutoff).add(attachment::Column::Id.eq(row.id)))
                    .secure()
                    .scope_with(&AccessScope::for_tenant(row.tenant_id))
                    .exec(tx)
                    .await?;
                if res.rows_affected == 0 {
                    return Ok(None);
                }
                if !has_file {
                    return Ok(Some(None));
                }
                if let Some(sec) = &row.secondary_file_id {
                    tracing::warn!(attachment_id = %row.id, secondary_file_id = %sec, "secondary file of an abandoned upload is not deleted");
                }
                let payload = cleanup_payload(EVENT_UPLOAD_ABANDONED, &row, now);
                let w = enqueue_with(&outbox, tx, &payload).await?;
                Ok(Some(Some(w)))
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

async fn enqueue_with(
    outbox: &crate::infra::outbox::OutboxPort,
    tx: &toolkit_db::DbTx<'_>,
    payload: &crate::infra::outbox::payloads::AttachmentCleanupPayload,
) -> Result<toolkit_db::outbox::Wake, DomainError> {
    outbox
        .enqueue_json(
            tx,
            outbox.attachment_cleanup_queue(),
            payload.tenant_id,
            crate::infra::outbox::PAYLOAD_ATTACHMENT_CLEANUP,
            payload,
        )
        .await
        .map_err(|e| match e {
            DomainError::InvalidFormat(m) => DomainError::internal(m),
            other => other,
        })
}

/// Runs a scan every `upload_reaper.scan_interval_secs` until `cancel` fires.
pub async fn run(app: Arc<AppServices>, cancel: CancellationToken) {
    let interval = Duration::from_secs(app.cfg.upload_reaper.scan_interval_secs.max(1));
    loop {
        if let Err(e) = scan_once(&app, clock::now()).await {
            tracing::warn!(error = %e, "upload reaper scan failed");
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
#[path = "upload_reaper_tests.rs"]
mod tests;
