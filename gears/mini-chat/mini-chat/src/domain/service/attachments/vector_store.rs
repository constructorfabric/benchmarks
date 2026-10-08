//! Lazy per-chat vector store creation (DESIGN `chat_vector_stores` creation protocol).
//!
//! No DB transaction or connection is held across provider calls. The UNIQUE
//! `(tenant_id, chat_id)` placeholder insert elects the single creator.

use std::time::Duration;

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{UploadTimings, storage_unavailable};
use crate::domain::error::{DomainError, reasons, resource_types};
use crate::domain::service::Deps;
use crate::infra::db::entity::{chat, chat_vector_store};

/// Maximum protocol restarts (stale placeholder reclaim, vanished placeholder).
const MAX_RESTARTS: u32 = 3;

fn provider_mismatch() -> DomainError {
    DomainError::AlreadyExists {
        resource: resource_types::ATTACHMENT,
        name: reasons::PROVIDER_MISMATCH.to_owned(),
        detail: "The chat's vector store belongs to another storage backend".to_owned(),
    }
}

async fn find_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_vector_store::Model>, DomainError> {
    Ok(chat_vector_store::Entity::find()
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Rejects an upload into a chat whose vector store was created for another backend.
///
/// # Errors
/// 409 `provider_mismatch`, 500 on DB failure.
pub(super) async fn check_provider(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    storage_backend: &str,
) -> Result<(), DomainError> {
    match find_row(runner, scope, chat_id).await? {
        Some(row) if row.provider != storage_backend => Err(provider_mismatch()),
        _ => Ok(()),
    }
}

fn is_stale(row: &chat_vector_store::Model, timings: &UploadTimings) -> bool {
    let age = OffsetDateTime::now_utc() - row.created_at;
    age > time::Duration::try_from(timings.vs_stale_placeholder).unwrap_or(time::Duration::MAX)
}

/// Deletes a NULL placeholder row (guarded by `vector_store_id IS NULL`).
async fn delete_placeholder(
    deps: &Deps,
    scope: &AccessScope,
    chat_id: Uuid,
    row_id: Uuid,
) -> Result<u64, DomainError> {
    let conn = deps.db.conn()?;
    Ok(chat_vector_store::Entity::delete_many()
        .filter(
            Condition::all()
                .add(chat_vector_store::Column::Id.eq(row_id))
                .add(chat_vector_store::Column::ChatId.eq(chat_id))
                .add(chat_vector_store::Column::VectorStoreId.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(&conn)
        .await?
        .rows_affected)
}

enum Poll {
    Ready(String),
    Restart,
    GaveUp,
}

/// Loser path: waits for the winner to publish the store id.
async fn poll_for_store(
    deps: &Deps,
    timings: &UploadTimings,
    scope: &AccessScope,
    chat_id: Uuid,
    storage_backend: &str,
) -> Result<Poll, DomainError> {
    let mut delay = timings.vs_loser_initial;
    for _ in 0..timings.vs_loser_polls {
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2).min(Duration::from_secs(5));
        let row = {
            let conn = deps.db.conn()?;
            find_row(&conn, scope, chat_id).await?
        };
        match row {
            None => return Ok(Poll::Restart),
            Some(r) if r.provider != storage_backend => return Err(provider_mismatch()),
            Some(r) => {
                if let Some(vs) = r.vector_store_id {
                    return Ok(Poll::Ready(vs));
                }
                if is_stale(&r, timings) {
                    delete_placeholder(deps, scope, chat_id, r.id).await?;
                    return Ok(Poll::Restart);
                }
            }
        }
    }
    Ok(Poll::GaveUp)
}

/// Returns the chat's vector store id, creating the store on first use.
///
/// # Errors
/// 409 `provider_mismatch`; 503 when creation fails or the loser gives up; 500 on DB failure.
pub(super) async fn ensure(
    deps: &Deps,
    timings: &UploadTimings,
    scope: &AccessScope,
    chat: &chat::Model,
    storage_provider_id: &str,
    storage_backend: &str,
) -> Result<String, DomainError> {
    for _ in 0..=MAX_RESTARTS {
        // 1. Fast path.
        let existing = {
            let conn = deps.db.conn()?;
            find_row(&conn, scope, chat.id).await?
        };
        if let Some(row) = existing {
            if row.provider != storage_backend {
                return Err(provider_mismatch());
            }
            if let Some(vs) = row.vector_store_id {
                return Ok(vs);
            }
            if is_stale(&row, timings) {
                tracing::warn!(chat_id = %chat.id, "reclaiming stale vector store placeholder");
                delete_placeholder(deps, scope, chat.id, row.id).await?;
                continue;
            }
            match poll_for_store(deps, timings, scope, chat.id, storage_backend).await? {
                Poll::Ready(vs) => return Ok(vs),
                Poll::Restart => continue,
                Poll::GaveUp => return Err(storage_unavailable()),
            }
        }

        // 2. Placeholder insert (auto-committed).
        let row_id = Uuid::new_v4();
        let am = chat_vector_store::ActiveModel {
            id: Set(row_id),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            vector_store_id: Set(None),
            provider: Set(storage_backend.to_owned()),
            file_count: Set(0),
            created_at: Set(OffsetDateTime::now_utc()),
        };
        let inserted = {
            let conn = deps.db.conn()?;
            secure_insert::<chat_vector_store::Entity>(am, scope, &conn)
                .await
                .map_err(DomainError::from)
        };
        match inserted {
            Ok(_) => {}
            Err(e) if e.is_unique_violation() => {
                // 3. Loser path.
                match poll_for_store(deps, timings, scope, chat.id, storage_backend).await? {
                    Poll::Ready(vs) => return Ok(vs),
                    Poll::Restart => continue,
                    Poll::GaveUp => return Err(storage_unavailable()),
                }
            }
            Err(e) => return Err(e),
        }

        // 4. Winner path.
        let created = deps
            .storage
            .create_vector_store(storage_provider_id, chat.tenant_id, &format!("chat_{}", chat.id))
            .await;
        let vs = match created {
            Ok(vs) => vs,
            Err(e) => {
                tracing::warn!(chat_id = %chat.id, error = %e, "vector store creation failed");
                if let Err(de) = delete_placeholder(deps, scope, chat.id, row_id).await {
                    tracing::warn!(error = %de, "failed to delete vector store placeholder");
                }
                return Err(storage_unavailable());
            }
        };
        let cas = {
            let conn = deps.db.conn()?;
            chat_vector_store::Entity::update_many()
                .col_expr(chat_vector_store::Column::VectorStoreId, Expr::value(vs.clone()))
                .filter(
                    Condition::all()
                        .add(chat_vector_store::Column::Id.eq(row_id))
                        .add(chat_vector_store::Column::ChatId.eq(chat.id))
                        .add(chat_vector_store::Column::VectorStoreId.is_null()),
                )
                .secure()
                .scope_with(scope)
                .exec(&conn)
                .await?
                .rows_affected
        };
        if cas == 1 {
            return Ok(vs);
        }
        // The placeholder was reclaimed meanwhile: drop our store, use the chat's current one.
        tracing::warn!(chat_id = %chat.id, "vector store placeholder reclaimed during creation");
        if let Err(e) = deps
            .storage
            .delete_vector_store(storage_provider_id, chat.tenant_id, &vs)
            .await
            && !e.is_not_found()
        {
            tracing::warn!(error = %e, "failed to delete superseded vector store");
        }
        match poll_for_store(deps, timings, scope, chat.id, storage_backend).await? {
            Poll::Ready(vs) => return Ok(vs),
            Poll::Restart | Poll::GaveUp => return Err(storage_unavailable()),
        }
    }
    Err(storage_unavailable())
}
