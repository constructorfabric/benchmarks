//! The chat persistence port ([`ChatStore`]) over the database provider: the owner-scoped chat
//! reads and writes of the chat-management feature and the chat delete transaction.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::chats::{
    ChatCleanupMessage, ChatCleanupQueue, ChatDetail, ChatDraft, ChatStore,
};
use crate::domain::error::DomainError;
use crate::infra::storage::chat_page::TimeStorage;
use crate::infra::storage::repo_impl::{ChatRepo, NewChat};

/// How the timestamps are stored on the engine of the provider (`SQLite` keeps text).
fn time_storage(db: &DBProvider<DomainError>) -> TimeStorage {
    TimeStorage::of_engine(db.db().db_engine())
}

#[async_trait]
impl ChatStore for DBProvider<DomainError> {
    async fn insert_chat(
        &self,
        scope: &AccessScope,
        ctx: &SecurityContext,
        draft: ChatDraft,
    ) -> Result<ChatDetail, DomainError> {
        let conn = self.conn()?;
        let new = NewChat {
            id: draft.id,
            model: draft.model,
            title: draft.title,
        };
        let record = ChatRepo::create_chat(&conn, scope, ctx, new).await?;
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-project
        // a new chat has no message
        Ok(record.into_detail(0))
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-project
    }

    async fn load_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError> {
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-404
        // a missing, soft-deleted, foreign-owner or foreign-tenant chat is the same not-found
        // (chat resource type), never a 403
        let record = ChatRepo::load_chat(&conn, scope, chat_id).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-404
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-count
        let count = ChatRepo::count_messages(&conn, &record).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-count
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-project
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-reads
        // the stored model is returned as it is, without a catalog lookup, so a chat whose model
        // was disabled later is still readable
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-reads
        // a read is not activity: nothing is written, so `updated_at` stays as it is
        Ok(record.into_detail(count))
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-activity-ordering:p1:inst-activity-reads
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-model-immutability:p1:inst-immutable-reads
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-project
    }

    async fn list_chats(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-list
        // only the caller's live chats, whatever filter or cursor the request carries: the scope
        // is applied before the filter and the cursor position
        let page = ChatRepo::list_chat_page(&conn, scope, query, time_storage(self)).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-isolation:p1:inst-isolation-list
        let mut items = Vec::with_capacity(page.items.len());
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-each-chat
        for record in page.items {
            // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-project-chat
            let count = ChatRepo::count_messages(&conn, &record).await?;
            items.push(record.into_detail(count));
            // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-project-chat
        }
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-each-chat
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-page-info
        // the effective limit and the next and previous cursors (`null` when there is no such
        // page) were built with the page
        Ok(Page {
            items,
            page_info: page.page_info,
        })
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-page-info
    }

    async fn rename_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError> {
        let conn = self.conn()?;
        let record = ChatRepo::update_title(&conn, scope, chat_id, title).await?;
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-project
        let count = ChatRepo::count_messages(&conn, &record).await?;
        Ok(record.into_detail(count))
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-project
    }

    async fn delete_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        cleanup: Arc<dyn ChatCleanupQueue>,
    ) -> Result<(), DomainError> {
        // the owner-scoped load: a missing, soft-deleted or foreign chat is a not-found and
        // nothing is written or enqueued
        let chat = {
            let conn = self.conn()?;
            ChatRepo::load_chat(&conn, scope, chat_id).await?
        };
        let scope = scope.clone();
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-begin
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-try
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-commit
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-catch
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-return-failure
        // one transaction: the soft-delete, the attachment marking and the enqueue become visible
        // together at the commit; an error of any step (or of the commit) rolls all of them back
        // and is returned as it is, which the flow maps to 404, 400 or 500
        let wake = self
            .transaction(move |tx| {
                Box::pin(async move {
                    // an already-deleted chat (another delete committed first) is a not-found
                    // and rolls back before anything is enqueued
                    let deleted_at = ChatRepo::soft_delete_chat_at(tx, &scope, chat.id).await?;
                    ChatRepo::mark_attachments_cleanup_pending(tx, &chat).await?;
                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-enqueue
                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-payload-too-large
                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-return-payload-error
                    // an oversized payload rolls the whole transaction back (the chat stays
                    // active, the attachments stay unmarked); the flow maps it to 400
                    let message = ChatCleanupMessage {
                        tenant_id: chat.tenant_id,
                        chat_id: chat.id,
                        chat_deleted_at: deleted_at,
                    };
                    cleanup.enqueue_chat_cleanup(tx, &message).await
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-return-payload-error
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-payload-too-large
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-enqueue
                })
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-return-failure
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-catch
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-commit
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-try
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-begin
        // @cpt-begin:cpt-cf-mini-chat-state-chat-management-chat-lifecycle:p2:inst-state-delete
        // the delete transaction committed: `deleted_at` is set, the attachments are marked and
        // the chat-cleanup message is enqueued
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-no-provider-call
        // no provider call, no vector store call and no wait: the chat-cleanup handler removes
        // the provider files and the chat vector store later
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-no-side-effects
        // a running turn of the chat is not touched and no audit event is emitted
        wake.fire();
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-no-side-effects
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-no-provider-call
        // @cpt-end:cpt-cf-mini-chat-state-chat-management-chat-lifecycle:p2:inst-state-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-return
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-delete-transaction:p1:inst-tx-return
    }
}
