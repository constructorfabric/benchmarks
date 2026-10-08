//! Upload reaper (DESIGN B.9.5): fails attachments left in `pending` /
//! `uploaded` by a dropped upload and schedules the provider file delete.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;

use crate::domain::error::DomainError;
use crate::domain::service::MiniChat;
use crate::infra::db::entity::attachments;
use crate::infra::db::now;
use crate::infra::outbox::{AttachmentCleanupEvent, Queue};

const SCAN_LIMIT: u64 = 100;

fn stale_cond(cutoff: OffsetDateTime) -> Condition {
    Condition::all()
        .add(attachments::Column::Status.is_in(["pending", "uploaded"]))
        .add(attachments::Column::DeletedAt.is_null())
        .add(attachments::Column::CleanupStatus.is_null())
        .add(attachments::Column::UpdatedAt.lt(cutoff))
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

impl MiniChat {
    /// One reaper scan; returns the number of reaped rows.
    ///
    /// # Errors
    /// Database failure of the candidate scan.
    pub async fn reaper_scan(&self) -> Result<usize, DomainError> {
        let stale = time::Duration::seconds(i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300));
        let cutoff = now() - stale;
        let conn = self.db.conn()?;
        let rows = attachments::Entity::find()
            .filter(stale_cond(cutoff))
            .order_by_asc(attachments::Column::UpdatedAt)
            .limit(SCAN_LIMIT)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        let mut reaped = 0;
        for a in rows {
            let outbox = self.outbox.clone();
            let a2 = a.clone();
            let res = crate::infra::db::tx_retry(&self.db, move |tx| {
                    let a2 = a2.clone();
                    let outbox = outbox.clone();
                    Box::pin(async move {
                        let ts = now();
                        let mut upd = attachments::Entity::update_many()
                            .col_expr(attachments::Column::Status, Expr::value("failed"))
                            .col_expr(attachments::Column::ErrorCode, Expr::value(Some("upload_abandoned".to_owned())))
                            .col_expr(attachments::Column::UpdatedAt, Expr::value(ts));
                        if a2.provider_file_id.is_some() {
                            upd = upd
                                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)));
                        }
                        let r = upd
                            .filter(
                                Condition::all()
                                    .add(attachments::Column::Id.eq(a2.id))
                                    .add(attachments::Column::Status.eq(a2.status.clone()))
                                    .add(stale_cond(cutoff)),
                            )
                            .secure()
                            .scope_with(&AccessScope::for_tenant(a2.tenant_id))
                            .exec(tx)
                            .await?;
                        if r.rows_affected == 0 {
                            return Ok(None);
                        }
                        if a2.provider_file_id.is_none() {
                            return Ok(Some(toolkit_db::outbox::Wake::empty()));
                        }
                        if let Some(sf) = &a2.secondary_file_id {
                            tracing::warn!(attachment_id = %a2.id, secondary_file_id = %sf, "abandoned upload: secondary copy is not deleted");
                        }
                        let ev = AttachmentCleanupEvent {
                            event_type: "attachment_upload_abandoned".into(),
                            tenant_id: a2.tenant_id,
                            chat_id: a2.chat_id,
                            attachment_id: a2.id,
                            provider_file_id: a2.provider_file_id.clone(),
                            vector_store_id: None,
                            storage_backend: a2.storage_backend.clone(),
                            attachment_kind: a2.attachment_kind.clone(),
                            deleted_at: rfc3339(ts),
                            secondary_ref: None,
                        };
                        Ok(Some(outbox.enqueue(tx, Queue::AttachmentCleanup, a2.tenant_id, &ev).await?))
                    })
                })
                .await;
            match res {
                Ok(Some(w)) => {
                    w.fire();
                    reaped += 1;
                    tracing::info!(attachment_id = %a.id, from_status = %a.status, "abandoned upload reaped");
                }
                Ok(None) => {}
                Err(e) => tracing::error!(attachment_id = %a.id, error = %e, "upload reaper failed"),
            }
        }
        Ok(reaped)
    }
}
