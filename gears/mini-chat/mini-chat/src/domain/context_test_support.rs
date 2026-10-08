//! Test helpers for context / thread-summary tests: direct row inserts and reads.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::clock;
use crate::domain::services::AppServices;
use crate::infra::db::entities::{chat, message, thread_summary};

/// Inserts a chat row and returns its id.
pub async fn insert_chat(app: &AppServices, tenant_id: Uuid, user_id: Uuid, model: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = clock::now();
    let am = chat::ActiveModel {
        id: ActiveValue::Set(id),
        tenant_id: ActiveValue::Set(tenant_id),
        user_id: ActiveValue::Set(user_id),
        model: ActiveValue::Set(model.to_owned()),
        title: ActiveValue::Set(None),
        is_temporary: ActiveValue::Set(false),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
        deleted_at: ActiveValue::Set(None),
    };
    let conn = app.db.conn().unwrap();
    secure_insert::<chat::Entity>(am, &AccessScope::allow_all(), &conn).await.unwrap();
    id
}

/// Inserts a message with an explicit timestamp and id.
#[allow(clippy::too_many_arguments)]
pub async fn insert_message_at(
    app: &AppServices,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Option<Uuid>,
    role: &str,
    content: &str,
    id: Uuid,
    created_at: OffsetDateTime,
) -> message::Model {
    let am = message::ActiveModel {
        id: ActiveValue::Set(id),
        tenant_id: ActiveValue::Set(tenant_id),
        chat_id: ActiveValue::Set(chat_id),
        request_id: ActiveValue::Set(request_id),
        role: ActiveValue::Set(role.to_owned()),
        content: ActiveValue::Set(content.to_owned()),
        content_type: ActiveValue::Set("text".to_owned()),
        token_estimate: ActiveValue::Set(0),
        provider_response_id: ActiveValue::Set(None),
        request_kind: ActiveValue::Set("chat".to_owned()),
        features_used: ActiveValue::Set(serde_json::json!([])),
        input_tokens: ActiveValue::Set(0),
        output_tokens: ActiveValue::Set(0),
        cache_read_input_tokens: ActiveValue::Set(0),
        cache_write_input_tokens: ActiveValue::Set(0),
        reasoning_tokens: ActiveValue::Set(0),
        model: ActiveValue::Set(None),
        is_compressed: ActiveValue::Set(false),
        created_at: ActiveValue::Set(created_at),
        deleted_at: ActiveValue::Set(None),
    };
    let conn = app.db.conn().unwrap();
    secure_insert::<message::Entity>(am, &AccessScope::allow_all(), &conn).await.unwrap()
}

/// Inserts a message stamped with `clock::now()`.
pub async fn insert_message(
    app: &AppServices,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Option<Uuid>,
    role: &str,
    content: &str,
) -> message::Model {
    insert_message_at(app, tenant_id, chat_id, request_id, role, content, Uuid::new_v4(), clock::now()).await
}

/// Inserts a completed turn (user + assistant message with the same request id).
pub async fn insert_turn(
    app: &AppServices,
    tenant_id: Uuid,
    chat_id: Uuid,
    user: &str,
    assistant: &str,
) -> (Uuid, message::Model, message::Model) {
    let rid = Uuid::new_v4();
    let u = insert_message(app, tenant_id, chat_id, Some(rid), "user", user).await;
    let a = insert_message(app, tenant_id, chat_id, Some(rid), "assistant", assistant).await;
    (rid, u, a)
}

/// Sets `deleted_at` / `is_compressed` of a message.
pub async fn set_message_flags(app: &AppServices, id: Uuid, deleted: bool, compressed: bool) {
    let conn = app.db.conn().unwrap();
    message::Entity::update_many()
        .col_expr(message::Column::DeletedAt, Expr::value(if deleted { Some(clock::now()) } else { None }))
        .col_expr(message::Column::IsCompressed, Expr::value(compressed))
        .filter(Condition::all().add(message::Column::Id.eq(id)))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

/// Inserts a thread summary row.
pub async fn insert_summary(
    app: &AppServices,
    tenant_id: Uuid,
    chat_id: Uuid,
    text: &str,
    frontier: (OffsetDateTime, Uuid),
    token_estimate: i32,
) -> thread_summary::Model {
    let now = clock::now();
    let am = thread_summary::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(tenant_id),
        chat_id: ActiveValue::Set(chat_id),
        summary_text: ActiveValue::Set(text.to_owned()),
        summarized_up_to_created_at: ActiveValue::Set(frontier.0),
        summarized_up_to_message_id: ActiveValue::Set(frontier.1),
        token_estimate: ActiveValue::Set(token_estimate),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    };
    let conn = app.db.conn().unwrap();
    secure_insert::<thread_summary::Entity>(am, &AccessScope::allow_all(), &conn).await.unwrap()
}

/// The chat's summary row.
pub async fn get_summary(app: &AppServices, chat_id: Uuid) -> Option<thread_summary::Model> {
    let conn = app.db.conn().unwrap();
    thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
}

/// All messages of the chat in `(created_at, id)` order.
pub async fn get_messages(app: &AppServices, chat_id: Uuid) -> Vec<message::Model> {
    let conn = app.db.conn().unwrap();
    message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(message::Column::CreatedAt, Order::Asc)
        .order_by(message::Column::Id, Order::Asc)
        .all(&conn)
        .await
        .unwrap()
}

/// Read-only views of the shared outbox tables (black-box checks of enqueued messages).
pub mod outbox_view {
    pub mod body {
        use sea_orm::entity::prelude::*;
        use toolkit_db::secure::Scopable;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
        #[sea_orm(table_name = "toolkit_outbox_body")]
        #[secure(unrestricted)]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i64,
            pub payload: Vec<u8>,
            pub payload_type: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    pub mod incoming {
        use sea_orm::entity::prelude::*;
        use toolkit_db::secure::Scopable;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
        #[sea_orm(table_name = "toolkit_outbox_incoming")]
        #[secure(unrestricted)]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i64,
            pub partition_id: i64,
            pub body_id: i64,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    pub mod partitions {
        use sea_orm::entity::prelude::*;
        use toolkit_db::secure::Scopable;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
        #[sea_orm(table_name = "toolkit_outbox_partitions")]
        #[secure(unrestricted)]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i64,
            pub queue: String,
            pub partition: i64,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }
}

/// A pending (not yet sequenced) outbox message: `(queue, partition, payload_type, json)`.
#[derive(Debug, Clone)]
pub struct PendingMessage {
    pub queue: String,
    pub partition: i64,
    pub payload_type: String,
    pub json: serde_json::Value,
}

/// Pending outbox messages visible to `runner` (call inside the enqueuing transaction).
pub async fn pending_messages(runner: &impl toolkit_db::secure::DBRunner) -> Vec<PendingMessage> {
    use outbox_view::{body, incoming, partitions};
    let all = AccessScope::allow_all();
    let incoming = incoming::Entity::find().secure().scope_with(&all).all(runner).await.unwrap();
    let mut out = Vec::new();
    for i in incoming {
        let b = body::Entity::find()
            .filter(body::Column::Id.eq(i.body_id))
            .secure()
            .scope_with(&all)
            .one(runner)
            .await
            .unwrap()
            .unwrap();
        let p = partitions::Entity::find()
            .filter(partitions::Column::Id.eq(i.partition_id))
            .secure()
            .scope_with(&all)
            .one(runner)
            .await
            .unwrap()
            .unwrap();
        out.push(PendingMessage {
            queue: p.queue,
            partition: p.partition,
            payload_type: b.payload_type,
            json: serde_json::from_slice(&b.payload).unwrap(),
        });
    }
    out
}
