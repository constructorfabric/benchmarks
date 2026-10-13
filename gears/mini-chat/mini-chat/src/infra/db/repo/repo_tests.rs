#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Repository tests on in-memory `SQLite` and the shared fixtures of the
//! domain service tests: a migrated database, a running outbox with every
//! mini-chat queue (a recording handler acknowledges each message), the
//! repositories, and seeding / reading helpers that go through the Secure
//! ORM with an unconstrained scope.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::outbox::{
    LeasedMessageHandler, MessageResult, Outbox, OutboxHandle, OutboxMessage, OutboxProfile,
};
use toolkit_db::secure::{DbConn, SecureEntityExt, secure_insert};
use toolkit_db::{ConnectOpts, DBProvider, Db, connect_db};
use toolkit_odata::{CursorV1, Error as ODataError, ODataOrderBy, ODataQuery, OrderKey, SortDir};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use uuid::Uuid;

use super::{
    DbRepository, chats as chat_repo, messages as message_repo, reactions as reaction_repo,
};
use crate::domain::error::{DomainError, ResourceKind, conflict};
use crate::domain::ports::{
    AttachmentKind, AttachmentStatus, MessageRole, NewChat, NewReaction, Reaction,
    ReactionRepository, TurnState,
};
use crate::infra::db::entities::{
    DbTimestamp, TimestampForm, attachments, chat_turns, chats, message_attachments,
    message_reactions, messages,
};
use crate::infra::db::error::{UniqueTarget, unique_violation_target};
use crate::infra::outbox::{MiniChatQueue, OutboxEnqueuer, OutboxQueues};

// ── Fixture ──────────────────────────────────────────────────────────────

/// One message delivered to the recording handler.
#[derive(Debug, Clone)]
pub struct Delivered {
    pub queue: String,
    pub payload_type: String,
    pub payload: serde_json::Value,
}

/// Acknowledges every message and records it.
struct RecordingHandler {
    queue: String,
    delivered: Arc<Mutex<Vec<Delivered>>>,
}

#[async_trait]
impl LeasedMessageHandler for RecordingHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload = serde_json::from_slice(&msg.payload).unwrap_or(serde_json::Value::Null);
        self.delivered.lock().unwrap().push(Delivered {
            queue: self.queue.clone(),
            payload_type: msg.payload_type.clone(),
            payload,
        });
        MessageResult::Ok
    }
}

/// Migrated in-memory database, repositories and (optionally) a running
/// outbox with every mini-chat queue.
pub struct TestStore {
    pub db: Db,
    pub repo: Arc<DbRepository>,
    pub enqueuer: Arc<OutboxEnqueuer>,
    delivered: Arc<Mutex<Vec<Delivered>>>,
    handle: Option<OutboxHandle>,
}

impl TestStore {
    /// Store with a running outbox.
    pub async fn new() -> Self {
        Self::build(true).await
    }

    /// Store whose outbox producer is never attached.
    pub async fn without_outbox() -> Self {
        Self::build(false).await
    }

    async fn build(with_outbox: bool) -> Self {
        let url = format!(
            "sqlite:file:mini_chat_{}?mode=memory&cache=shared",
            Uuid::new_v4().simple()
        );
        let opts = ConnectOpts {
            max_conns: Some(1),
            ..Default::default()
        };
        let db = connect_db(&url, opts).await.expect("connect sqlite");
        run_migrations_for_testing(&db, crate::infra::db::migrations())
            .await
            .expect("migrations");

        let queues = OutboxQueues::default();
        let enqueuer = Arc::new(OutboxEnqueuer::new(queues.clone()));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let handle = if with_outbox {
            let handle = start_outbox(&db, &queues, &delivered).await;
            assert!(enqueuer.attach(Arc::clone(handle.outbox())));
            Some(handle)
        } else {
            None
        };
        let provider = Arc::new(DBProvider::new(db.clone()));
        let repo = Arc::new(DbRepository::new(
            Arc::clone(&provider),
            Arc::clone(&enqueuer),
        ));
        Self {
            db,
            repo,
            enqueuer,
            delivered,
            handle,
        }
    }

    /// A non-transactional runner.
    pub fn conn(&self) -> DbConn<'_> {
        self.db.conn().expect("conn")
    }

    /// How timestamps are bound on the test database.
    pub fn ts(&self) -> TimestampForm {
        self.repo.timestamps()
    }

    /// `at` bound for the test database.
    pub fn stamp(&self, at: OffsetDateTime) -> DbTimestamp {
        self.ts().stamp(at)
    }

    /// The running outbox.
    pub fn outbox(&self) -> &Arc<Outbox> {
        self.handle.as_ref().expect("outbox started").outbox()
    }

    /// Payloads delivered on `queue` so far.
    pub fn delivered(&self, queue: &str) -> Vec<Delivered> {
        self.delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.queue == queue)
            .cloned()
            .collect()
    }

    /// Waits until at least `count` messages were delivered on `queue`, then
    /// a little longer so that extra messages would show up too.
    pub async fn wait_delivered(&self, queue: &str, count: usize) -> Vec<Delivered> {
        for _ in 0..200 {
            if self.delivered(queue).len() >= count {
                tokio::time::sleep(Duration::from_millis(100)).await;
                return self.delivered(queue);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.delivered(queue)
    }
}

async fn start_outbox(
    db: &Db,
    queues: &OutboxQueues,
    delivered: &Arc<Mutex<Vec<Delivered>>>,
) -> OutboxHandle {
    let handler = |queue: MiniChatQueue| RecordingHandler {
        queue: queues.name(queue).to_owned(),
        delivered: Arc::clone(delivered),
    };
    let mut builder = Outbox::builder(db.clone()).profile(OutboxProfile::low_latency());
    for queue in MiniChatQueue::ALL {
        builder = builder
            .queue(queues.name(queue), queues.partitions())
            .leased(handler(queue))
            .done();
    }
    builder.start().await.expect("start outbox")
}

// ── Identities and scopes ────────────────────────────────────────────────

pub const TENANT_A: Uuid = Uuid::from_u128(0x7e7a);
pub const TENANT_B: Uuid = Uuid::from_u128(0x7e7b);
pub const USER_A1: Uuid = Uuid::from_u128(0xa1);
pub const USER_A2: Uuid = Uuid::from_u128(0xa2);
pub const USER_B1: Uuid = Uuid::from_u128(0xb1);

/// The scope the PEP returns for `(tenant, user)`.
pub fn subject_scope(tenant: Uuid, user: Uuid) -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
        ScopeFilter::eq(pep_properties::OWNER_ID, user),
    ]))
}

// ── Seeding ──────────────────────────────────────────────────────────────

/// Current time at microsecond precision, shifted by `offset_ms`.
pub fn at(offset_ms: i64) -> OffsetDateTime {
    crate::domain::ports::storage_now() + time::Duration::milliseconds(offset_ms)
}

/// Inserts a live chat owned by `(tenant, user)`.
pub async fn seed_chat(store: &TestStore, tenant: Uuid, user: Uuid, title: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    chat_repo::insert(
        &store.conn(),
        store.ts(),
        &AccessScope::allow_all(),
        NewChat {
            id,
            tenant_id: tenant,
            user_id: user,
            model: "standard".to_owned(),
            title: title.map(str::to_owned),
            now: at(0),
        },
    )
    .await
    .expect("seed chat");
    id
}

/// A message to seed.
#[derive(Debug, Clone)]
pub struct MessageSeed {
    pub role: MessageRole,
    pub request_id: Option<Uuid>,
    pub content: String,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub created_at: OffsetDateTime,
    pub deleted: bool,
}

impl MessageSeed {
    pub fn new(role: MessageRole, offset_ms: i64) -> Self {
        Self {
            role,
            request_id: Some(Uuid::new_v4()),
            content: format!("{} message", role.as_str()),
            model: (role == MessageRole::Assistant).then(|| "standard".to_owned()),
            input_tokens: 0,
            output_tokens: 0,
            created_at: at(offset_ms),
            deleted: false,
        }
    }
}

/// Inserts a message of `chat_id`.
pub async fn seed_message(
    store: &TestStore,
    chat_id: Uuid,
    tenant: Uuid,
    seed: MessageSeed,
) -> Uuid {
    let id = Uuid::new_v4();
    let model = messages::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        request_id: Set(seed.request_id),
        role: Set(seed.role.as_str().to_owned()),
        content: Set(seed.content),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(seed.input_tokens),
        output_tokens: Set(seed.output_tokens),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(seed.model),
        is_compressed: Set(false),
        created_at: Set(store.stamp(seed.created_at)),
        deleted_at: Set(seed.deleted.then(|| store.stamp(at(0)))),
    };
    secure_insert::<messages::Entity>(model, &AccessScope::allow_all(), &store.conn())
        .await
        .expect("seed message");
    id
}

/// A turn to seed.
#[derive(Debug, Clone)]
pub struct TurnSeed {
    pub request_id: Uuid,
    pub state: TurnState,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
    pub deleted: bool,
}

impl TurnSeed {
    pub fn new(state: TurnState) -> Self {
        Self {
            request_id: Uuid::new_v4(),
            state,
            error_code: None,
            assistant_message_id: None,
            updated_at: at(0),
            deleted: false,
        }
    }
}

/// Inserts a turn of `chat_id`.
pub async fn seed_turn(store: &TestStore, chat_id: Uuid, tenant: Uuid, seed: TurnSeed) -> Uuid {
    let id = Uuid::new_v4();
    let model = chat_turns::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        request_id: Set(seed.request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(USER_A1)),
        state: Set(seed.state.as_str().to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(Some("resp_internal".to_owned())),
        assistant_message_id: Set(seed.assistant_message_id),
        error_code: Set(seed.error_code),
        reserve_tokens: Set(Some(2_000)),
        max_output_tokens_applied: Set(Some(1_000)),
        reserved_credits_micro: Set(Some(1)),
        policy_version_applied: Set(Some(1)),
        effective_model: Set(Some("standard".to_owned())),
        minimal_generation_floor_applied: Set(Some(50)),
        error_detail: Set(None),
        deleted_at: Set(seed.deleted.then(|| store.stamp(at(0)))),
        replaced_by_request_id: Set(None),
        started_at: Set(store.stamp(at(-10))),
        last_progress_at: Set(Some(store.stamp(seed.updated_at))),
        web_search_enabled: Set(false),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(store.stamp(seed.updated_at)),
    };
    secure_insert::<chat_turns::Entity>(model, &AccessScope::allow_all(), &store.conn())
        .await
        .expect("seed turn");
    id
}

/// An attachment to seed.
#[derive(Debug, Clone)]
pub struct AttachmentSeed {
    pub kind: AttachmentKind,
    pub status: AttachmentStatus,
    pub filename: String,
    pub thumbnail: Option<(Vec<u8>, i32, i32)>,
    pub cleanup_status: Option<String>,
    pub deleted: bool,
}

impl AttachmentSeed {
    pub fn new(kind: AttachmentKind, status: AttachmentStatus, filename: &str) -> Self {
        Self {
            kind,
            status,
            filename: filename.to_owned(),
            thumbnail: (kind == AttachmentKind::Image)
                .then(|| (vec![0x52, 0x49, 0x46, 0x46], 64, 48)),
            cleanup_status: None,
            deleted: false,
        }
    }
}

/// Inserts an attachment of `chat_id`.
pub async fn seed_attachment(
    store: &TestStore,
    chat_id: Uuid,
    tenant: Uuid,
    seed: AttachmentSeed,
) -> Uuid {
    let id = Uuid::new_v4();
    let (thumbnail, width, height) = match seed.thumbnail {
        Some((data, width, height)) => (Some(data), Some(width), Some(height)),
        None => (None, None, None),
    };
    let model = attachments::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(USER_A1),
        filename: Set(seed.filename),
        content_type: Set(Some("application/octet-stream".to_owned())),
        size_bytes: Set(Some(1024)),
        storage_backend: Set("openai".to_owned()),
        provider_file_id: Set(Some(format!("file-secret-{}", id.simple()))),
        status: Set(seed.status.as_str().to_owned()),
        error_code: Set(None),
        attachment_kind: Set(seed.kind.as_str().to_owned()),
        for_file_search: Set(seed.kind == AttachmentKind::Document),
        for_code_interpreter: Set(false),
        doc_summary: Set(None),
        img_thumbnail: Set(thumbnail),
        img_thumbnail_width: Set(width),
        img_thumbnail_height: Set(height),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(seed.cleanup_status),
        cleanup_attempts: Set(0),
        last_cleanup_error: Set(None),
        cleanup_updated_at: Set(None),
        created_at: Set(store.stamp(at(0))),
        updated_at: Set(store.stamp(at(0))),
        deleted_at: Set(seed.deleted.then(|| store.stamp(at(0)))),
        secondary_file_id: Set(None),
        secondary_status: Set("not_attempted".to_owned()),
        secondary_provider_kind: Set(None),
        secondary_provider_id: Set(None),
    };
    secure_insert::<attachments::Entity>(model, &AccessScope::allow_all(), &store.conn())
        .await
        .expect("seed attachment");
    id
}

/// Links `attachment_id` to `message_id` (both of `chat_id`).
pub async fn link_attachment(
    store: &TestStore,
    chat_id: Uuid,
    tenant: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
    offset_ms: i64,
) {
    let model = message_attachments::ActiveModel {
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        message_id: Set(message_id),
        attachment_id: Set(attachment_id),
        created_at: Set(store.stamp(at(offset_ms))),
    };
    secure_insert::<message_attachments::Entity>(model, &AccessScope::allow_all(), &store.conn())
        .await
        .expect("link attachment");
}

/// Inserts a reaction of `user` on `message_id`.
pub async fn seed_reaction(
    store: &TestStore,
    message_id: Uuid,
    tenant: Uuid,
    user: Uuid,
    reaction: Reaction,
) {
    reaction_repo::upsert(
        &store.conn(),
        store.ts(),
        &subject_scope(tenant, user),
        NewReaction {
            id: Uuid::new_v4(),
            message_id,
            tenant_id: tenant,
            user_id: user,
            reaction,
            created_at: at(0),
        },
    )
    .await
    .expect("seed reaction");
}

// ── Reading ──────────────────────────────────────────────────────────────

pub async fn chat_row(store: &TestStore, chat_id: Uuid) -> chats::Model {
    chats::Entity::find()
        .filter(chats::Column::Id.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&store.conn())
        .await
        .expect("read chat")
        .expect("chat row")
}

pub async fn attachment_row(store: &TestStore, attachment_id: Uuid) -> attachments::Model {
    attachments::Entity::find()
        .filter(attachments::Column::Id.eq(attachment_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&store.conn())
        .await
        .expect("read attachment")
        .expect("attachment row")
}

pub async fn reaction_rows(store: &TestStore, message_id: Uuid) -> Vec<message_reactions::Model> {
    message_reactions::Entity::find()
        .filter(message_reactions::Column::MessageId.eq(message_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&store.conn())
        .await
        .expect("read reactions")
}

pub async fn message_rows(store: &TestStore, chat_id: Uuid) -> Vec<messages::Model> {
    messages::Entity::find()
        .filter(messages::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&store.conn())
        .await
        .expect("read messages")
}

// ── Repository tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn chat_queries_are_confined_to_the_scope() {
    let store = TestStore::without_outbox().await;
    let chat = seed_chat(&store, TENANT_A, USER_A1, Some("mine")).await;

    let own = chat_repo::find_active(&store.conn(), &subject_scope(TENANT_A, USER_A1), chat)
        .await
        .unwrap();
    assert_eq!(own.map(|c| c.id), Some(chat));
    for (tenant, user) in [
        (TENANT_A, USER_A2),
        (TENANT_B, USER_B1),
        (TENANT_B, USER_A1),
    ] {
        let foreign = chat_repo::find_active(&store.conn(), &subject_scope(tenant, user), chat)
            .await
            .unwrap();
        assert!(foreign.is_none(), "{tenant}/{user} must not see the chat");
    }
}

#[tokio::test]
async fn chat_insert_outside_the_scope_is_rejected() {
    let store = TestStore::without_outbox().await;

    let err = chat_repo::insert(
        &store.conn(),
        store.ts(),
        &subject_scope(TENANT_A, USER_A1),
        NewChat {
            id: Uuid::new_v4(),
            tenant_id: TENANT_B,
            user_id: USER_A1,
            model: "standard".to_owned(),
            title: None,
            now: at(0),
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
}

#[tokio::test]
async fn deleted_chat_is_not_active() {
    let store = TestStore::without_outbox().await;
    let scope = subject_scope(TENANT_A, USER_A1);
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;

    assert!(
        chat_repo::mark_deleted(&store.conn(), store.ts(), &scope, chat, at(0))
            .await
            .unwrap()
    );
    assert!(
        !chat_repo::mark_deleted(&store.conn(), store.ts(), &scope, chat, at(0))
            .await
            .unwrap()
    );
    assert!(
        chat_repo::find_active(&store.conn(), &scope, chat)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn message_count_is_one_grouped_query_over_live_messages() {
    let store = TestStore::without_outbox().await;
    let scope = subject_scope(TENANT_A, USER_A1);
    let busy = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let quiet = seed_chat(&store, TENANT_A, USER_A1, None).await;
    for offset in 0..3 {
        seed_message(
            &store,
            busy,
            TENANT_A,
            MessageSeed::new(MessageRole::User, offset),
        )
        .await;
    }
    let mut gone = MessageSeed::new(MessageRole::Assistant, 5);
    gone.deleted = true;
    seed_message(&store, busy, TENANT_A, gone).await;

    let counts = chat_repo::count_active_messages(&store.conn(), &scope, &[busy, quiet])
        .await
        .unwrap();

    assert_eq!(counts.get(&busy), Some(&3));
    assert_eq!(counts.get(&quiet), None);
}

#[tokio::test]
async fn reaction_upsert_keeps_one_row_per_user_and_message() {
    let store = TestStore::without_outbox().await;
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let message = seed_message(
        &store,
        chat,
        TENANT_A,
        MessageSeed::new(MessageRole::Assistant, 0),
    )
    .await;

    seed_reaction(&store, message, TENANT_A, USER_A1, Reaction::Like).await;
    seed_reaction(&store, message, TENANT_A, USER_A1, Reaction::Dislike).await;
    seed_reaction(&store, message, TENANT_A, USER_A2, Reaction::Like).await;

    let rows = reaction_rows(&store, message).await;
    assert_eq!(rows.len(), 2);
    let mine = rows.iter().find(|r| r.user_id == USER_A1).unwrap();
    assert_eq!(mine.reaction, "dislike");
}

#[tokio::test]
async fn reaction_put_echoes_its_own_write_under_concurrent_put_and_delete() {
    let store = TestStore::without_outbox().await;
    let scope = subject_scope(TENANT_A, USER_A1);
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let message = seed_message(
        &store,
        chat,
        TENANT_A,
        MessageSeed::new(MessageRole::Assistant, 0),
    )
    .await;
    let reaction_of = |reaction| NewReaction {
        id: Uuid::new_v4(),
        message_id: message,
        tenant_id: TENANT_A,
        user_id: USER_A1,
        reaction,
        created_at: at(0),
    };
    let repo: &dyn ReactionRepository = store.repo.as_ref();

    // Each `PUT` runs its write and read-back in one transaction: a `DELETE`
    // or another `PUT` of the same reaction interleaved with it never makes it
    // fail nor echo the other request's value.
    for _ in 0..25 {
        let (liked, deleted, disliked) = tokio::join!(
            repo.upsert(&scope, reaction_of(Reaction::Like)),
            repo.delete(&scope, message, USER_A1),
            repo.upsert(&scope, reaction_of(Reaction::Dislike)),
        );
        let liked = liked.expect("like upsert");
        assert_eq!(liked.reaction, Reaction::Like);
        assert_eq!((liked.message_id, liked.user_id), (message, USER_A1));
        deleted.expect("delete");
        assert_eq!(
            disliked.expect("dislike upsert").reaction,
            Reaction::Dislike
        );
    }
}

#[tokio::test]
async fn reaction_upsert_in_a_transaction_returns_its_write_before_a_later_delete() {
    let store = TestStore::without_outbox().await;
    let scope = subject_scope(TENANT_A, USER_A1);
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let message = seed_message(
        &store,
        chat,
        TENANT_A,
        MessageSeed::new(MessageRole::Assistant, 0),
    )
    .await;
    seed_reaction(&store, message, TENANT_A, USER_A1, Reaction::Dislike).await;
    let ts = store.ts();

    let stored = store
        .db
        .transaction_ref_mapped(move |tx| {
            Box::pin(async move {
                let stored = reaction_repo::upsert(
                    tx,
                    ts,
                    &scope,
                    NewReaction {
                        id: Uuid::new_v4(),
                        message_id: message,
                        tenant_id: TENANT_A,
                        user_id: USER_A1,
                        reaction: Reaction::Like,
                        created_at: at(0),
                    },
                )
                .await?;
                reaction_repo::delete(tx, &scope, message, USER_A1).await?;
                Ok::<_, DomainError>(stored)
            })
        })
        .await
        .unwrap();

    assert_eq!(stored.reaction, Reaction::Like);
    assert!(reaction_rows(&store, message).await.is_empty());
}

#[tokio::test]
async fn real_sqlite_unique_violations_are_identified_from_the_driver_error() {
    let store = TestStore::without_outbox().await;
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let request_id = Uuid::new_v4();
    let mut first = TurnSeed::new(TurnState::Completed);
    first.request_id = request_id;
    seed_turn(&store, chat, TENANT_A, first).await;

    let duplicate = chat_turns::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat),
        request_id: Set(request_id),
        requester_type: Set("user".to_owned()),
        state: Set("failed".to_owned()),
        started_at: Set(store.stamp(at(0))),
        updated_at: Set(store.stamp(at(0))),
        ..Default::default()
    };
    let err =
        secure_insert::<chat_turns::Entity>(duplicate, &AccessScope::allow_all(), &store.conn())
            .await
            .unwrap_err();

    assert_eq!(
        unique_violation_target(&err),
        Some(UniqueTarget::TurnRequestId)
    );
    assert!(matches!(
        DomainError::from(err),
        DomainError::AlreadyExists {
            resource: ResourceKind::Turn,
            resource_name: conflict::UNIQUE_VIOLATION,
        }
    ));
}

#[tokio::test]
async fn foreign_key_failures_map_to_internal_without_driver_text() {
    let store = TestStore::without_outbox().await;
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let dangling = message_attachments::ActiveModel {
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat),
        message_id: Set(Uuid::new_v4()),
        attachment_id: Set(Uuid::new_v4()),
        created_at: Set(store.stamp(at(0))),
    };
    let result = secure_insert::<message_attachments::Entity>(
        dangling,
        &AccessScope::allow_all(),
        &store.conn(),
    )
    .await;

    // `SQLite` enforces foreign keys only when the pragma is on; either way
    // nothing about the failure may leak into the domain error.
    if let Err(err) = result {
        assert_eq!(unique_violation_target(&err), None);
        match DomainError::from(err) {
            DomainError::Internal(diagnostic) => {
                assert!(!diagnostic.contains("message_attachments"), "{diagnostic}");
            }
            other => panic!("expected internal, got {other:?}"),
        }
    }
}

// ── Ordering and paging ──────────────────────────────────────────────────

/// `2026-01-01T00:00:00Z` plus `micros` microseconds.
fn instant(micros: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap()
        + time::Duration::microseconds(micros)
}

/// `instant(micros)` as the API echoes it (RFC 3339, trailing zeros trimmed).
fn echoed(micros: i64) -> String {
    instant(micros).format(&Rfc3339).unwrap()
}

/// A first-page query with the `$filter` `raw` (parsed and hashed as the
/// `OData` extractor does).
fn filtered(raw: &str) -> ODataQuery {
    let expr = toolkit_odata::parse_filter_string(raw).unwrap().into_expr();
    let hash = toolkit_odata::short_filter_hash(Some(&expr)).unwrap();
    ODataQuery::new().with_filter(expr).with_filter_hash(hash)
}

fn ordered(query: ODataQuery, field: &str, dir: SortDir, limit: u64) -> ODataQuery {
    query
        .with_order(ODataOrderBy(vec![OrderKey {
            field: field.to_owned(),
            dir,
        }]))
        .with_limit(limit)
}

/// The continuation of `first` at the cursor `token`.
fn continued(first: &ODataQuery, token: &str) -> ODataQuery {
    let mut next = ODataQuery::new().with_cursor(CursorV1::decode(token).unwrap());
    if let Some(limit) = first.limit {
        next = next.with_limit(limit);
    }
    if let Some(filter) = first.filter() {
        next = next.with_filter(filter.clone());
    }
    if let Some(hash) = &first.filter_hash {
        next = next.with_filter_hash(hash.clone());
    }
    next
}

async fn chat_page(store: &TestStore, query: &ODataQuery) -> toolkit_odata::Page<Uuid> {
    let page = chat_repo::list_active(
        &store.conn(),
        store.ts(),
        &subject_scope(TENANT_A, USER_A1),
        query,
    )
    .await
    .unwrap();
    toolkit_odata::Page {
        items: page.items.into_iter().map(|chat| chat.id).collect(),
        page_info: page.page_info,
    }
}

async fn message_page(
    store: &TestStore,
    chat_id: Uuid,
    query: &ODataQuery,
) -> toolkit_odata::Page<Uuid> {
    let page = message_repo::list_active(
        &store.conn(),
        store.ts(),
        &subject_scope(TENANT_A, USER_A1),
        chat_id,
        query,
    )
    .await
    .unwrap();
    toolkit_odata::Page {
        items: page.items.into_iter().map(|message| message.id).collect(),
        page_info: page.page_info,
    }
}

/// The items of every page from `first`, following the next cursors.
async fn chat_pages(store: &TestStore, first: &ODataQuery) -> Vec<Vec<Uuid>> {
    let mut pages = Vec::new();
    let mut query = first.clone();
    loop {
        let page = chat_page(store, &query).await;
        pages.push(page.items);
        assert!(pages.len() <= 10, "chat paging does not terminate");
        match page.page_info.next_cursor {
            Some(token) => query = continued(first, &token),
            None => return pages,
        }
    }
}

/// The items of every page from `first`, following the next cursors.
async fn message_pages(store: &TestStore, chat_id: Uuid, first: &ODataQuery) -> Vec<Vec<Uuid>> {
    let mut pages = Vec::new();
    let mut query = first.clone();
    loop {
        let page = message_page(store, chat_id, &query).await;
        pages.push(page.items);
        assert!(pages.len() <= 10, "message paging does not terminate");
        match page.page_info.next_cursor {
            Some(token) => query = continued(first, &token),
            None => return pages,
        }
    }
}

/// Inserts an untitled chat of `(TENANT_A, USER_A1)` updated at `at`.
async fn seed_chat_at(store: &TestStore, at: OffsetDateTime) -> Uuid {
    let id = Uuid::new_v4();
    chat_repo::insert(
        &store.conn(),
        store.ts(),
        &AccessScope::allow_all(),
        NewChat {
            id,
            tenant_id: TENANT_A,
            user_id: USER_A1,
            model: "standard".to_owned(),
            title: None,
            now: at,
        },
    )
    .await
    .expect("seed chat");
    id
}

#[tokio::test]
async fn untitled_chats_page_exactly_once_in_title_order() {
    let store = TestStore::without_outbox().await;
    let mut chats = Vec::new();
    for title in [None, Some("beta"), None, Some("alpha"), None] {
        let id = seed_chat(&store, TENANT_A, USER_A1, title).await;
        chats.push((title.unwrap_or_default().to_owned(), id));
    }
    seed_chat(&store, TENANT_A, USER_A2, None).await;

    for dir in [SortDir::Asc, SortDir::Desc] {
        // Untitled chats sort as the empty title; ties fall to `id desc`.
        let mut expected = chats.clone();
        expected.sort_by(|a, b| {
            let by_title = match dir {
                SortDir::Asc => a.0.cmp(&b.0),
                SortDir::Desc => b.0.cmp(&a.0),
            };
            by_title.then(b.1.cmp(&a.1))
        });
        let expected: Vec<Uuid> = expected.into_iter().map(|(_, id)| id).collect();

        let first = ordered(ODataQuery::new(), "title", dir, 2);
        let pages = chat_pages(&store, &first).await;
        assert_eq!(pages.len(), 3, "{dir:?}: {pages:?}");
        assert_eq!(pages.concat(), expected, "{dir:?}");

        // Backward from the second page returns exactly the first page.
        let page_one = chat_page(&store, &first).await;
        let page_two = chat_page(
            &store,
            &continued(&first, &page_one.page_info.next_cursor.unwrap()),
        )
        .await;
        let back = chat_page(
            &store,
            &continued(&first, &page_two.page_info.prev_cursor.unwrap()),
        )
        .await;
        assert_eq!(back.items, page_one.items, "{dir:?}");
        assert!(back.page_info.prev_cursor.is_none(), "{dir:?}");
    }
}

#[tokio::test]
async fn message_timestamps_order_filter_and_page_chronologically() {
    let store = TestStore::without_outbox().await;
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let mut seeded = Vec::new();
    for micros in [100_000, 150_000, 0] {
        let mut seed = MessageSeed::new(MessageRole::User, 0);
        seed.created_at = instant(micros);
        seeded.push(seed_message(&store, chat, TENANT_A, seed).await);
    }
    let (at_100, at_150, at_0) = (seeded[0], seeded[1], seeded[2]);
    let chronological = vec![at_0, at_100, at_150];

    for (dir, expected) in [
        (SortDir::Asc, chronological.clone()),
        (SortDir::Desc, chronological.iter().rev().copied().collect()),
    ] {
        let first = ordered(ODataQuery::new(), "created_at", dir, 1);
        let pages = message_pages(&store, chat, &first).await;
        assert_eq!(pages.concat(), expected, "{dir:?}");
    }

    assert_eq!(echoed(100_000), "2026-01-01T00:00:00.1Z");
    assert_eq!(echoed(0), "2026-01-01T00:00:00Z");
    for (raw, expected) in [
        (format!("created_at eq {}", echoed(100_000)), vec![at_100]),
        (format!("created_at gt {}", echoed(100_000)), vec![at_150]),
        (format!("created_at eq {}", echoed(0)), vec![at_0]),
        (
            format!("created_at ge {}", echoed(0)),
            chronological.clone(),
        ),
        (
            format!("created_at lt {}", echoed(150_000)),
            vec![at_0, at_100],
        ),
        (
            format!("created_at le {}", echoed(150_000)),
            chronological.clone(),
        ),
    ] {
        let first = ordered(filtered(&raw), "created_at", SortDir::Asc, 1);
        let pages = message_pages(&store, chat, &first).await;
        assert_eq!(pages.concat(), expected, "{raw}");
    }
}

#[tokio::test]
async fn chat_timestamps_order_filter_and_page_chronologically() {
    let store = TestStore::without_outbox().await;
    let at_100 = seed_chat_at(&store, instant(100_000)).await;
    let at_150 = seed_chat_at(&store, instant(150_000)).await;
    let at_0 = seed_chat_at(&store, instant(0)).await;

    // Default order: `updated_at desc`, one chat per page.
    let pages = chat_pages(&store, &ODataQuery::new().with_limit(1)).await;
    assert_eq!(pages.concat(), [at_150, at_100, at_0]);
    let first = ordered(ODataQuery::new(), "updated_at", SortDir::Asc, 2);
    assert_eq!(
        chat_pages(&store, &first).await.concat(),
        [at_0, at_100, at_150]
    );

    for (raw, expected) in [
        (format!("updated_at eq {}", echoed(0)), vec![at_0]),
        (format!("updated_at gt {}", echoed(0)), vec![at_100, at_150]),
        (format!("updated_at lt {}", echoed(100_000)), vec![at_0]),
    ] {
        let first = ordered(filtered(&raw), "updated_at", SortDir::Asc, 1);
        assert_eq!(chat_pages(&store, &first).await.concat(), expected, "{raw}");
    }
}

// ── Cursor validation ────────────────────────────────────────────────────

async fn chat_page_err(store: &TestStore, query: &ODataQuery) -> DomainError {
    chat_repo::list_active(
        &store.conn(),
        store.ts(),
        &subject_scope(TENANT_A, USER_A1),
        query,
    )
    .await
    .unwrap_err()
}

async fn message_page_err(store: &TestStore, chat_id: Uuid, query: &ODataQuery) -> DomainError {
    message_repo::list_active(
        &store.conn(),
        store.ts(),
        &subject_scope(TENANT_A, USER_A1),
        chat_id,
        query,
    )
    .await
    .unwrap_err()
}

/// The decoded next cursor of `page`.
fn next_cursor(page: &toolkit_odata::Page<Uuid>) -> CursorV1 {
    CursorV1::decode(page.page_info.next_cursor.as_deref().expect("next cursor")).unwrap()
}

fn assert_invalid_cursor(err: &DomainError, case: &str) {
    assert!(
        matches!(err, DomainError::InvalidQuery(ODataError::InvalidCursor)),
        "{case}: {err:?}"
    );
}

#[tokio::test]
async fn chat_cursor_issued_for_another_filter_is_a_filter_mismatch() {
    let store = TestStore::without_outbox().await;
    for _ in 0..2 {
        seed_chat(&store, TENANT_A, USER_A1, Some("same")).await;
    }
    let first = filtered("title eq 'same'").with_limit(1);
    let page = chat_page(&store, &first).await;
    let token = page.page_info.next_cursor.expect("next cursor");

    let other = filtered("title eq 'other'").with_limit(1);
    let err = chat_page_err(&store, &continued(&other, &token)).await;

    assert!(
        matches!(err, DomainError::InvalidQuery(ODataError::FilterMismatch)),
        "{err:?}"
    );
}

#[tokio::test]
async fn malformed_chat_cursor_keys_are_invalid_cursors() {
    let store = TestStore::without_outbox().await;
    seed_chat_at(&store, instant(0)).await;
    seed_chat_at(&store, instant(100_000)).await;
    // Default order: `updated_at desc`, then `id desc`.
    let cursor = next_cursor(&chat_page(&store, &ODataQuery::new().with_limit(1)).await);
    assert_eq!(cursor.s, "-updated_at,-id");
    assert!(
        chat_page(
            &store,
            &ODataQuery::new().with_cursor(cursor.clone()).with_limit(1)
        )
        .await
        .page_info
        .prev_cursor
        .is_some()
    );

    let mut missing_key = cursor.clone();
    missing_key.k.pop();
    let mut extra_key = cursor.clone();
    extra_key.k.push(Uuid::new_v4().to_string());
    let mut bad_id = cursor.clone();
    bad_id.k[1] = "not-a-uuid".to_owned();
    let mut bad_timestamp = cursor.clone();
    bad_timestamp.k[0] = "not-a-timestamp".to_owned();
    let mut loose_timestamp = cursor.clone();
    loose_timestamp.k[0] = "2026-01-01T00:00:00Z".to_owned();
    for (case, cursor) in [
        ("missing key", missing_key),
        ("extra key", extra_key),
        ("non-UUID id key", bad_id),
        ("non-timestamp updated_at key", bad_timestamp),
        ("non-fixed-width updated_at key", loose_timestamp),
    ] {
        let err = chat_page_err(&store, &ODataQuery::new().with_cursor(cursor)).await;
        assert_invalid_cursor(&err, case);
    }
}

#[tokio::test]
async fn chat_cursor_with_an_unorderable_field_is_an_invalid_order_field() {
    let store = TestStore::without_outbox().await;
    seed_chat_at(&store, instant(0)).await;
    seed_chat_at(&store, instant(100_000)).await;
    let mut cursor = next_cursor(&chat_page(&store, &ODataQuery::new().with_limit(1)).await);
    cursor.s = "+model".to_owned();
    cursor.k = vec!["standard".to_owned()];

    let err = chat_page_err(&store, &ODataQuery::new().with_cursor(cursor)).await;

    assert!(
        matches!(&err, DomainError::InvalidQuery(ODataError::InvalidOrderByField(field)) if field == "model"),
        "{err:?}"
    );
}

#[tokio::test]
async fn malformed_message_cursor_timestamp_keys_are_invalid_cursors() {
    let store = TestStore::without_outbox().await;
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;
    for micros in [0, 100_000] {
        let mut seed = MessageSeed::new(MessageRole::User, 0);
        seed.created_at = instant(micros);
        seed_message(&store, chat, TENANT_A, seed).await;
    }
    // Default order: `created_at asc`, then `id asc`.
    let cursor = next_cursor(&message_page(&store, chat, &ODataQuery::new().with_limit(1)).await);
    assert_eq!(cursor.s, "+created_at,+id");
    let page = message_page(&store, chat, &ODataQuery::new().with_cursor(cursor.clone())).await;
    assert_eq!(page.items.len(), 1);

    let mut bad_timestamp = cursor.clone();
    bad_timestamp.k[0] = "not-a-timestamp".to_owned();
    let mut loose_timestamp = cursor.clone();
    loose_timestamp.k[0] = "2026-01-01T00:00:00Z".to_owned();
    for (case, cursor) in [
        ("non-timestamp created_at key", bad_timestamp),
        ("non-fixed-width created_at key", loose_timestamp),
    ] {
        let err = message_page_err(&store, chat, &ODataQuery::new().with_cursor(cursor)).await;
        assert_invalid_cursor(&err, case);
    }
}

// ── Attachment deletion: `secondary_ref` from the stored target ──────────

/// Alias resolver recording each lookup; a stored provider entry maps to
/// `alias-of-{entry}`, a row without one to `alias-by-kind`.
#[derive(Default)]
struct RecordingAliases {
    lookups: Mutex<Vec<AliasLookup>>,
}

/// One alias lookup: tenant, provider kind, stored provider entry.
type AliasLookup = (Uuid, String, Option<String>);

impl crate::domain::ports::SecondaryAliasResolver for RecordingAliases {
    fn upstream_alias(
        &self,
        tenant_id: Uuid,
        provider_kind: &str,
        provider_id: Option<&str>,
    ) -> Option<String> {
        self.lookups.lock().unwrap().push((
            tenant_id,
            provider_kind.to_owned(),
            provider_id.map(str::to_owned),
        ));
        Some(provider_id.map_or_else(|| "alias-by-kind".to_owned(), |id| format!("alias-of-{id}")))
    }
}

/// Soft-deletes an image of a new chat whose secondary copy was recorded
/// with `provider_id` (`None`: a row stored without the entry) and returns
/// the `secondary_ref` of its cleanup message and the resolver lookups.
async fn secondary_ref_after_delete(
    provider_id: Option<&str>,
) -> (serde_json::Value, Vec<AliasLookup>) {
    use crate::domain::ports::{
        AttachmentDeletion, AttachmentStore, SecondaryAliasResolver, SecondaryCopy,
    };
    use toolkit_db::secure::SecureUpdateExt;

    let store = TestStore::new().await;
    let chat = seed_chat(&store, TENANT_A, USER_A1, None).await;
    let id = seed_attachment(
        &store,
        chat,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "a.png"),
    )
    .await;
    let scope = subject_scope(TENANT_A, USER_A1);
    let copy = SecondaryCopy::Uploaded {
        file_id: "file_copy".to_owned(),
        provider_kind: "anthropic".to_owned(),
        provider_id: provider_id.unwrap_or("unused").to_owned(),
    };
    assert!(
        AttachmentStore::mark_secondary(&*store.repo, &scope, chat, id, copy, at(1))
            .await
            .unwrap()
    );
    let row = attachment_row(&store, id).await;
    assert_eq!(
        row.secondary_provider_id.as_deref(),
        Some(provider_id.unwrap_or("unused"))
    );
    if provider_id.is_none() {
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::SecondaryProviderId,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&store.conn())
            .await
            .unwrap();
    }

    let aliases = Arc::new(RecordingAliases::default());
    let deletion = AttachmentStore::soft_delete(
        &*store.repo,
        &scope,
        chat,
        id,
        at(2),
        Some(Arc::clone(&aliases) as Arc<dyn SecondaryAliasResolver>),
    )
    .await
    .unwrap();
    assert_eq!(deletion, AttachmentDeletion::Deleted);

    let delivered = store
        .wait_delivered(&OutboxQueues::default().attachment_cleanup, 1)
        .await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].payload["attachment_id"], id.to_string());
    let lookups = aliases.lookups.lock().unwrap().clone();
    (delivered[0].payload["secondary_ref"].clone(), lookups)
}

#[tokio::test]
async fn attachment_cleanup_payload_carries_the_stored_secondary_target() {
    let (secondary_ref, lookups) = secondary_ref_after_delete(Some("claude-second")).await;
    assert_eq!(
        secondary_ref,
        serde_json::json!({
            "file_id": "file_copy",
            "provider_kind": "anthropic",
            "upstream_alias": "alias-of-claude-second",
        })
    );
    assert_eq!(
        lookups,
        [(
            TENANT_A,
            "anthropic".to_owned(),
            Some("claude-second".to_owned())
        )],
        "the alias is resolved once, from the soft-deleted row's stored entry"
    );
}

#[tokio::test]
async fn attachment_cleanup_payload_without_a_stored_target_resolves_by_kind() {
    let (secondary_ref, lookups) = secondary_ref_after_delete(None).await;
    assert_eq!(secondary_ref["upstream_alias"], "alias-by-kind");
    assert_eq!(lookups, [(TENANT_A, "anthropic".to_owned(), None)]);
}
