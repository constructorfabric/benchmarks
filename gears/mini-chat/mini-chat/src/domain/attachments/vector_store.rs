//! Lazy per-chat vector store creation protocol (DESIGN §3.7 `chat_vector_stores` "Creation
//! protocol" and "Stale placeholder reclaim"). No transaction or connection is held across a
//! provider call.

use std::sync::Arc;
use std::time::Duration;

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::domain::error::{DomainError, Resource};
use crate::domain::services::AppServices;
use crate::infra::db::entities::chat_vector_store;
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::storage;

use super::PROVIDER_RETRY_AFTER_SECS;

/// A NULL placeholder older than this is reclaimed (its creator died).
pub const STALE_PLACEHOLDER_SECS: i64 = 120;
/// Loser-path polls before giving up with 503.
pub const LOSER_POLLS: u32 = 5;
const LOSER_BASE_DELAY: Duration = Duration::from_millis(250);
const MAX_RESTARTS: usize = 3;

/// 409 `already_exists` `provider_mismatch`.
#[must_use]
pub fn provider_mismatch() -> DomainError {
    DomainError::AlreadyExists {
        resource: Resource::Attachment,
        name: "provider_mismatch".to_owned(),
        detail: "The chat's vector store belongs to another provider backend".to_owned(),
    }
}

fn unavailable(detail: &str) -> DomainError {
    DomainError::unavailable(PROVIDER_RETRY_AFTER_SECS, detail)
}

/// The chat's vector store row, if any.
///
/// # Errors
/// DB errors.
pub async fn find(app: &AppServices, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<chat_vector_store::Model>, DomainError> {
    let conn = app.db.conn()?;
    Ok(chat_vector_store::Entity::find()
        .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(&conn)
        .await?)
}

/// Rejects an upload whose provider backend differs from the chat's existing store.
///
/// # Errors
/// 409 `provider_mismatch`, DB errors.
pub async fn check_provider(app: &AppServices, tenant_id: Uuid, chat_id: Uuid, provider: &ResolvedProvider) -> Result<(), DomainError> {
    match find(app, tenant_id, chat_id).await? {
        Some(row) if row.provider != provider.storage_backend => Err(provider_mismatch()),
        _ => Ok(()),
    }
}

fn is_stale(row: &chat_vector_store::Model) -> bool {
    row.vector_store_id.is_none() && row.created_at < clock::now() - time::Duration::seconds(STALE_PLACEHOLDER_SECS)
}

/// Deletes a row only while it is still a NULL placeholder.
async fn delete_placeholder(app: &AppServices, tenant_id: Uuid, row_id: Uuid) -> Result<(), DomainError> {
    let conn = app.db.conn()?;
    chat_vector_store::Entity::delete_many()
        .filter(
            Condition::all()
                .add(chat_vector_store::Column::Id.eq(row_id))
                .add(chat_vector_store::Column::VectorStoreId.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(&conn)
        .await?;
    Ok(())
}

/// Returns the chat's vector store id, creating the store on first use.
///
/// # Errors
/// 409 `provider_mismatch`, 503 provider failure or creation not converging, DB errors.
pub async fn ensure(
    app: &Arc<AppServices>,
    ctx: &SecurityContext,
    tenant_id: Uuid,
    chat_id: Uuid,
    provider: &ResolvedProvider,
) -> Result<String, DomainError> {
    for _ in 0..MAX_RESTARTS {
        if let Some(row) = find(app, tenant_id, chat_id).await? {
            if row.provider != provider.storage_backend {
                return Err(provider_mismatch());
            }
            if let Some(id) = row.vector_store_id {
                return Ok(id);
            }
            if is_stale(&row) {
                tracing::warn!(%chat_id, "reclaiming a stale vector store placeholder");
                delete_placeholder(app, tenant_id, row.id).await?;
                continue;
            }
            return poll_existing(app, tenant_id, chat_id, provider).await;
        }
        let row_id = Uuid::new_v4();
        let am = chat_vector_store::ActiveModel {
            id: Set(row_id),
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            vector_store_id: Set(None),
            provider: Set(provider.storage_backend.clone()),
            file_count: Set(0),
            created_at: Set(clock::now()),
        };
        let inserted = {
            let conn = app.db.conn()?;
            secure_insert::<chat_vector_store::Entity>(am, &AccessScope::for_tenant(tenant_id), &conn)
                .await
                .map_err(DomainError::from)
        };
        match inserted {
            Ok(_) => return create_as_winner(app, ctx, tenant_id, chat_id, row_id, provider).await,
            Err(e) if e.is_unique_violation() => return poll_existing(app, tenant_id, chat_id, provider).await,
            Err(e) => return Err(e),
        }
    }
    Err(unavailable("vector store creation did not converge"))
}

async fn create_as_winner(
    app: &Arc<AppServices>,
    ctx: &SecurityContext,
    tenant_id: Uuid,
    chat_id: Uuid,
    row_id: Uuid,
    provider: &ResolvedProvider,
) -> Result<String, DomainError> {
    let name = format!("mini-chat-{chat_id}");
    let vs_id = match storage::create_vector_store(app.transport.as_ref(), ctx, provider, &name).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%chat_id, error = %e, "vector store creation failed");
            if let Err(de) = delete_placeholder(app, tenant_id, row_id).await {
                tracing::warn!(%chat_id, error = %de, "failed to delete the vector store placeholder");
            }
            return Err(unavailable("vector store creation failed"));
        }
    };
    let res = {
        let conn = app.db.conn()?;
        chat_vector_store::Entity::update_many()
            .col_expr(chat_vector_store::Column::VectorStoreId, Expr::value(Some(vs_id.clone())))
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::Id.eq(row_id))
                    .add(chat_vector_store::Column::VectorStoreId.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await?
    };
    if res.rows_affected == 1 {
        return Ok(vs_id);
    }
    // The placeholder was reclaimed while the store was being created: drop the new store.
    let app2 = Arc::clone(app);
    let ctx2 = ctx.clone();
    let provider2 = provider.clone();
    tokio::spawn(async move {
        if let Err(e) = storage::delete_vector_store(app2.transport.as_ref(), &ctx2, &provider2, &vs_id).await {
            tracing::warn!(error = %e, "best-effort delete of a superseded vector store failed");
        }
    });
    poll_existing(app, tenant_id, chat_id, provider).await
}

/// Loser path: waits for another request to populate the store id.
async fn poll_existing(app: &AppServices, tenant_id: Uuid, chat_id: Uuid, provider: &ResolvedProvider) -> Result<String, DomainError> {
    let mut delay = LOSER_BASE_DELAY;
    for _ in 0..LOSER_POLLS {
        tokio::time::sleep(delay).await;
        delay *= 2;
        if let Some(row) = find(app, tenant_id, chat_id).await? {
            if row.provider != provider.storage_backend {
                return Err(provider_mismatch());
            }
            if let Some(id) = row.vector_store_id {
                return Ok(id);
            }
        }
    }
    Err(unavailable("vector store is being created by another request"))
}
