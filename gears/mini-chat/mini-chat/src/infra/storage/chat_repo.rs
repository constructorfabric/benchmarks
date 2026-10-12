//! `SeaORM` implementation of the chat repository over Secure ORM.

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::odata::sea_orm_filter::{LimitCfg, paginate_odata};
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::db_error::map_scope_err;
use super::entity::{attachments, chats, messages};
use super::odata::{ChatODataMapper, UNTITLED_DB_TITLE};
use crate::domain::chat::{Chat, ChatDetail, ChatFilterField, ChatRepository, NewChat};
use crate::domain::cleanup::ChatCleanupPayload;
use crate::domain::error::DomainError;
use crate::infra::outbox::OutboxEnqueuer;

/// Attachment `cleanup_status` meaning "provider cleanup is outstanding".
const CLEANUP_PENDING: &str = "pending";

/// Default and maximum page size of the chat list.
const LIST_LIMITS: LimitCfg = LimitCfg {
    default: 20,
    max: 100,
};

/// Chat repository over the gear's database.
pub struct ChatRepositoryImpl {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
}

impl ChatRepositoryImpl {
    /// Creates the repository.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, outbox: Arc<OutboxEnqueuer>) -> Self {
        Self { db, outbox }
    }
}

fn to_chat(model: chats::Model) -> Chat {
    Chat {
        id: model.id,
        tenant_id: model.tenant_id,
        user_id: model.user_id,
        model: model.model,
        // Untitled chats are stored as the empty title (see `UNTITLED_DB_TITLE`).
        title: model.title.filter(|title| !title.is_empty()),
        is_temporary: model.is_temporary,
        created_at: model.created_at,
        updated_at: model.updated_at,
    }
}

/// Number of live messages of a chat. The messages table has no owner column: the owner check
/// already happened on the chat, so only the tenant part of the scope applies here.
async fn message_count(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<u64, DomainError> {
    messages::Entity::find()
        .filter(messages::Column::ChatId.eq(chat_id))
        .filter(messages::Column::DeletedAt.is_null())
        .secure()
        .scope_with(&scope.tenant_only())
        .count(runner)
        .await
        .map_err(map_scope_err)
}

async fn detail_of(
    runner: &impl DBRunner,
    scope: &AccessScope,
    model: chats::Model,
) -> Result<ChatDetail, DomainError> {
    let message_count = message_count(runner, scope, model.id).await?;
    Ok(ChatDetail {
        chat: to_chat(model),
        message_count,
    })
}

async fn find_live(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chats::Model>, DomainError> {
    chats::Entity::find()
        .filter(chats::Column::Id.eq(chat_id))
        .filter(chats::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .one(runner)
        .await
        .map_err(map_scope_err)
}

/// The default order is "most recently active first": `updated_at desc`; `id desc` is added as the
/// tiebreaker by the pager.
fn with_default_order(query: &ODataQuery) -> ODataQuery {
    if query.cursor.is_some() || !query.order.0.is_empty() {
        return query.clone();
    }
    let mut ordered = query.clone();
    ordered.order = ODataOrderBy(vec![OrderKey {
        field: "updated_at".to_owned(),
        dir: SortDir::Desc,
    }]);
    ordered
}

#[async_trait]
impl ChatRepository for ChatRepositoryImpl {
    async fn insert(&self, scope: &AccessScope, chat: NewChat) -> Result<ChatDetail, DomainError> {
        let conn = self.db.conn()?;
        let active = chats::ActiveModel {
            id: ActiveValue::Set(chat.id),
            tenant_id: ActiveValue::Set(chat.tenant_id),
            user_id: ActiveValue::Set(chat.user_id),
            model: ActiveValue::Set(chat.model),
            title: ActiveValue::Set(Some(
                chat.title.unwrap_or_else(|| UNTITLED_DB_TITLE.to_owned()),
            )),
            is_temporary: ActiveValue::Set(false),
            created_at: ActiveValue::Set(chat.now),
            updated_at: ActiveValue::Set(chat.now),
            deleted_at: ActiveValue::Set(None),
        };
        let model = secure_insert::<chats::Entity>(active, scope, &conn)
            .await
            .map_err(map_scope_err)?;
        Ok(ChatDetail {
            chat: to_chat(model),
            message_count: 0,
        })
    }

    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ChatDetail>, DomainError> {
        let conn = self.db.conn()?;
        match find_live(&conn, scope, chat_id).await? {
            Some(model) => detail_of(&conn, scope, model).await.map(Some),
            None => Ok(None),
        }
    }

    async fn list(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        let conn = self.db.conn()?;
        let select = chats::Entity::find()
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope);
        let page = paginate_odata::<ChatFilterField, ChatODataMapper, _, _, _, _>(
            select,
            &conn,
            &with_default_order(query),
            ("id", SortDir::Desc),
            LIST_LIMITS,
            |model| model,
        )
        .await?;
        let mut items = Vec::with_capacity(page.items.len());
        for model in page.items {
            items.push(detail_of(&conn, scope, model).await?);
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn rename(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
        now: OffsetDateTime,
    ) -> Result<Option<ChatDetail>, DomainError> {
        let conn = self.db.conn()?;
        let updated = chats::Entity::update_many()
            .col_expr(chats::Column::Title, Expr::value(title))
            .col_expr(chats::Column::UpdatedAt, Expr::value(now))
            .filter(chats::Column::Id.eq(chat_id))
            .filter(chats::Column::DeletedAt.is_null())
            .secure()
            .scope_with(scope)
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected;
        if updated == 0 {
            return Ok(None);
        }
        match find_live(&conn, scope, chat_id).await? {
            Some(model) => detail_of(&conn, scope, model).await.map(Some),
            None => Ok(None),
        }
    }

    async fn soft_delete(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        system_request_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let outbox = Arc::clone(&self.outbox);
        let scope = scope.clone();
        in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let scope = &scope;
                let Some(chat) = find_live(tx, scope, chat_id).await? else {
                    return Ok((false, Wake::empty()));
                };
                let deleted = chats::Entity::update_many()
                    .col_expr(chats::Column::DeletedAt, Expr::value(now))
                    .col_expr(chats::Column::UpdatedAt, Expr::value(now))
                    .filter(chats::Column::Id.eq(chat_id))
                    .filter(chats::Column::DeletedAt.is_null())
                    .secure()
                    .scope_with(scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?
                    .rows_affected;
                if deleted == 0 {
                    return Ok((false, Wake::empty()));
                }
                attachments::Entity::update_many()
                    .col_expr(
                        attachments::Column::CleanupStatus,
                        Expr::value(CLEANUP_PENDING),
                    )
                    .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(now))
                    .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
                    .filter(attachments::Column::ChatId.eq(chat_id))
                    .filter(attachments::Column::CleanupStatus.is_null())
                    .secure()
                    .scope_with(&scope.tenant_only())
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?;
                // Enqueue last: an error after the enqueue would drop the wake on rollback.
                let payload = ChatCleanupPayload::for_soft_delete(
                    chat.tenant_id,
                    chat_id,
                    system_request_id,
                    now,
                );
                let wake = outbox.enqueue_chat_cleanup(tx, &payload).await?;
                Ok((true, wake))
            })
        })
        .await
    }
}
