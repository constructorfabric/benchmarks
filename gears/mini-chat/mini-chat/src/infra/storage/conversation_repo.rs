//! Reads of the conversation a turn is assembled from (DESIGN "`ContextPlan` Determinism and
//! Snapshot Boundary").

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::DBProvider;
use toolkit_db::secure::SecureEntityExt;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::entity::{attachments, chat_vector_stores, messages, thread_summaries};
use super::map_scope_err;
use super::ordering::{after, at_or_before};
use crate::domain::error::DomainError;
use crate::domain::llm::{InputMessage, InputRole};
use crate::domain::stream::{AttachmentRef, ConversationRepository, SummaryView, TurnContext};

/// Conversation reads over the gear's database.
pub struct ConversationRepositoryImpl {
    db: Arc<DBProvider<DomainError>>,
}

impl ConversationRepositoryImpl {
    /// Creates the repository.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>) -> Self {
        Self { db }
    }
}

/// The live messages of the chat; the messages of `exclude` (the request of a retry/edit turn
/// whose own user message is already stored) are left out.
fn live_messages(chat_id: Uuid, exclude: Option<Uuid>) -> sea_orm::Select<messages::Entity> {
    let select = messages::Entity::find()
        .filter(messages::Column::ChatId.eq(chat_id))
        .filter(messages::Column::DeletedAt.is_null())
        .filter(messages::Column::RequestId.is_not_null());
    match exclude {
        Some(request_id) => select.filter(messages::Column::RequestId.ne(request_id)),
        None => select,
    }
}

/// The recent messages within the snapshot boundary and after the summary frontier, oldest first.
async fn recent_history(
    conn: &impl toolkit_db::secure::DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    boundary: Option<&messages::Model>,
    summary: Option<&thread_summaries::Model>,
    limit: u32,
    exclude: Option<Uuid>,
) -> Result<Vec<InputMessage>, DomainError> {
    let Some(boundary) = boundary.filter(|_| limit > 0) else {
        return Ok(Vec::new());
    };
    let mut condition = Condition::all()
        .add(messages::Column::IsCompressed.eq(false))
        .add(messages::Column::Role.is_in(["user", "assistant"]))
        .add(at_or_before(boundary.created_at, boundary.id));
    if let Some(summary) = summary {
        condition = condition.add(after(
            summary.summarized_up_to_created_at,
            summary.summarized_up_to_message_id,
        ));
    }
    let mut recent = live_messages(chat_id, exclude)
        .filter(condition)
        .secure()
        .scope_with(tenant_scope)
        .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
        .order_by(messages::Column::Id, sea_orm::Order::Desc)
        .limit(u64::from(limit))
        .all(conn)
        .await
        .map_err(map_scope_err)?;
    recent.reverse();
    Ok(recent
        .into_iter()
        .map(|message| InputMessage {
            role: if message.role == "assistant" {
                InputRole::Assistant
            } else {
                InputRole::User
            },
            text: message.content,
            image_file_ids: Vec::new(),
        })
        .collect())
}

#[async_trait]
impl ConversationRepository for ConversationRepositoryImpl {
    async fn turn_context(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        recent_messages_limit: u32,
        exclude_request: Option<Uuid>,
    ) -> Result<TurnContext, DomainError> {
        let conn = self.db.conn()?;
        let tenant_scope = scope.tenant_only();

        let prior = live_messages(chat_id, exclude_request)
            .filter(messages::Column::Role.eq("assistant"))
            .filter(
                Condition::any()
                    .add(messages::Column::InputTokens.gt(0))
                    .add(messages::Column::OutputTokens.gt(0)),
            )
            .secure()
            .scope_with(&tenant_scope)
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(messages::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .all(&conn)
            .await
            .map_err(map_scope_err)?;
        let prior_context_tokens = prior.first().map_or(0, |message| {
            message.input_tokens.saturating_add(message.output_tokens)
        });

        let summary = thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&tenant_scope)
            .one(&conn)
            .await
            .map_err(map_scope_err)?;

        let boundary = live_messages(chat_id, exclude_request)
            .secure()
            .scope_with(&tenant_scope)
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(messages::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .all(&conn)
            .await
            .map_err(map_scope_err)?
            .into_iter()
            .next();

        let history = recent_history(
            &conn,
            &tenant_scope,
            chat_id,
            boundary.as_ref(),
            summary.as_ref(),
            recent_messages_limit,
            exclude_request,
        )
        .await?;

        let ready = attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::Status.eq("ready"))
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&tenant_scope)
            .all(&conn)
            .await
            .map_err(map_scope_err)?;
        let ready_documents = ready.iter().any(|row| row.for_file_search);
        let code_interpreter_file_ids = ready
            .iter()
            .filter(|row| row.for_code_interpreter)
            .filter_map(|row| row.provider_file_id.clone())
            .collect();
        let vector_store_id = chat_vector_stores::Entity::find()
            .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
            .filter(chat_vector_stores::Column::VectorStoreId.is_not_null())
            .secure()
            .scope_with(&tenant_scope)
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .and_then(|row| row.vector_store_id);

        Ok(TurnContext {
            prior_context_tokens,
            summary: summary.map(|row| SummaryView {
                text: row.summary_text,
                token_estimate: i64::from(row.token_estimate),
            }),
            history,
            ready_documents,
            vector_store_id,
            code_interpreter_file_ids,
        })
    }

    async fn ready_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<AttachmentRef>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.db.conn()?;
        Ok(attachments::Entity::find()
            .filter(attachments::Column::Id.is_in(ids.to_vec()))
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::UploadedByUserId.eq(user_id))
            .filter(attachments::Column::Status.eq("ready"))
            .filter(attachments::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .all(&conn)
            .await
            .map_err(map_scope_err)?
            .into_iter()
            .map(|row| AttachmentRef {
                id: row.id,
                kind: row.attachment_kind,
                filename: row.filename,
                provider_file_id: row.provider_file_id,
                secondary_file_id: row.secondary_file_id,
            })
            .collect())
    }

    async fn file_citation_map(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<HashMap<String, (Uuid, String)>, DomainError> {
        let conn = self.db.conn()?;
        Ok(attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::Status.eq("ready"))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::ProviderFileId.is_not_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .all(&conn)
            .await
            .map_err(map_scope_err)?
            .into_iter()
            .filter_map(|row| Some((row.provider_file_id?, (row.id, row.filename))))
            .collect())
    }
}
