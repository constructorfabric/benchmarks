//! Thread summary and message-attachment repositories.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::{Db, MessageKey, now};
use super::super::entity::{message_attachments, thread_summaries};
use crate::error::DomainError;

/// Thread summary repository.
#[derive(Clone)]
pub struct SummaryRepo {
    #[allow(dead_code)]
    db: Db,
}

impl SummaryRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Load the summary row of a chat (tenant-scoped).
    pub async fn get(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<thread_summaries::Model>, DomainError> {
        let row = thread_summaries::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(thread_summaries::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
            .one(runner)
            .await?;
        Ok(row)
    }

    /// CAS commit of a new summary: succeeds only when the stored frontier
    /// still equals `base_frontier` (or no row exists when `base_frontier`
    /// is `None`). Returns `true` when the commit won.
    pub async fn commit_cas(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        base_frontier: Option<MessageKey>,
        new_summary: &str,
        target: MessageKey,
        token_estimate: i32,
    ) -> Result<bool, DomainError> {
        let now = now();
        let existing = self.get(runner, tenant_id, chat_id).await?;
        match (&existing, base_frontier) {
            (None, None) => {
                let am = thread_summaries::ActiveModel {
                    id: ActiveValue::Set(Uuid::new_v4()),
                    tenant_id: ActiveValue::Set(tenant_id),
                    chat_id: ActiveValue::Set(chat_id),
                    summary_text: ActiveValue::Set(new_summary.to_owned()),
                    summarized_up_to_created_at: ActiveValue::Set(target.0),
                    summarized_up_to_message_id: ActiveValue::Set(target.1),
                    token_estimate: ActiveValue::Set(token_estimate),
                    created_at: ActiveValue::Set(now),
                    updated_at: ActiveValue::Set(now),
                };
                let res =
                    toolkit_db::secure::secure_insert::<thread_summaries::Entity>(am, &AccessScope::allow_all(), runner).await;
                match res {
                    Ok(_) => Ok(true),
                    Err(e) if e.is_unique_violation() => Ok(false),
                    Err(e) => Err(e.into()),
                }
            }
            (Some(row), Some(base))
                if row.summarized_up_to_created_at == base.0
                    && row.summarized_up_to_message_id == base.1 =>
            {
                // CAS: update only if the frontier is still the base.
                let res = thread_summaries::Entity::update_many()
                    .col_expr(
                        thread_summaries::Column::SummaryText,
                        Expr::value(new_summary.to_owned()),
                    )
                    .col_expr(
                        thread_summaries::Column::SummarizedUpToCreatedAt,
                        Expr::value(target.0),
                    )
                    .col_expr(
                        thread_summaries::Column::SummarizedUpToMessageId,
                        Expr::value(target.1),
                    )
                    .col_expr(
                        thread_summaries::Column::TokenEstimate,
                        Expr::value(token_estimate),
                    )
                    .col_expr(thread_summaries::Column::UpdatedAt, Expr::value(now))
                    .filter(Condition::all().add(thread_summaries::Column::Id.eq(row.id)))
                    .filter(
                        Condition::all()
                            .add(thread_summaries::Column::SummarizedUpToCreatedAt.eq(base.0)),
                    )
                    .filter(
                        Condition::all()
                            .add(thread_summaries::Column::SummarizedUpToMessageId.eq(base.1)),
                    )
                    .secure()
                    .scope_with(&AccessScope::allow_all())
                    .exec(runner)
                    .await?;
                Ok(res.rows_affected == 1)
            }
            // Frontier moved since the work item was enqueued.
            _ => Ok(false),
        }
    }

    /// Delete the summary row of a chat (turn mutation path).
    pub async fn delete_for_chat(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<bool, DomainError> {
        let res = thread_summaries::Entity::delete_many()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(thread_summaries::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
            .exec(runner)
            .await?;
        Ok(res.rows_affected > 0)
    }
}

/// Message-attachment association repository.
#[derive(Clone)]
pub struct MessageAttachmentRepo {
    #[allow(dead_code)]
    db: Db,
}

impl MessageAttachmentRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Insert associations for one message.
    pub async fn insert_batch(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
        attachment_ids: &[Uuid],
    ) -> Result<(), DomainError> {
        if attachment_ids.is_empty() {
            return Ok(());
        }
        let now = now();
        let models: Vec<message_attachments::ActiveModel> = attachment_ids
            .iter()
            .map(|attachment_id| message_attachments::ActiveModel {
                chat_id: ActiveValue::Set(chat_id),
                message_id: ActiveValue::Set(message_id),
                attachment_id: ActiveValue::Set(*attachment_id),
                tenant_id: ActiveValue::Set(tenant_id),
                created_at: ActiveValue::Set(now),
            })
            .collect();
        toolkit_db::secure::secure_insert_many::<message_attachments::Entity>(models, &super::tenant_scope(scope), runner).await?;
        Ok(())
    }

    /// Whether any message references the attachment (within its chat).
    pub async fn is_referenced(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<bool, DomainError> {
        let rows = message_attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(message_attachments::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(message_attachments::Column::ChatId.eq(chat_id)))
            .filter(
                Condition::all()
                    .add(message_attachments::Column::AttachmentId.eq(attachment_id)),
            )
            .limit(1)
            .all(runner)
            .await?;
        Ok(!rows.is_empty())
    }

    /// Attachment ids referenced by the given messages (with the message
    /// they hang on).
    pub async fn for_messages(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<Vec<message_attachments::Model>, DomainError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = message_attachments::Entity::find()
            .secure()
            .scope_with(&super::tenant_scope(scope))
            .filter(Condition::all().add(message_attachments::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(message_attachments::Column::ChatId.eq(chat_id)))
            .filter(
                Condition::all()
                    .add(message_attachments::Column::MessageId.is_in(message_ids.iter().copied())),
            )
            .all(runner)
            .await?;
        Ok(rows)
    }

    /// Attachment ids linked to one message.
    pub async fn attachments_of(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let rows = message_attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(message_attachments::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(message_attachments::Column::ChatId.eq(chat_id)))
            .filter(Condition::all().add(message_attachments::Column::MessageId.eq(message_id)))
            .all(runner)
            .await?;
        Ok(rows.into_iter().map(|r| r.attachment_id).collect())
    }
}
