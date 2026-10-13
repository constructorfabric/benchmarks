#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use mini_chat_sdk::MiniChatModelPolicyPluginClientV1;
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

use super::*;
use crate::domain::authz::AuthzService;
use crate::domain::error::{DomainError, InvalidArgumentReason, ResourceKind};
use crate::domain::models_service::ModelsService;
use crate::domain::models_service::tests::{FakePolicy, deny_all_authz, sample_catalog};
use crate::domain::ports::{AttachmentKind, AttachmentStatus, MessageRole};
use crate::infra::db::repo::tests::{
    AttachmentSeed, MessageSeed, TENANT_A, TENANT_B, TestStore, USER_A1, USER_A2, USER_B1,
    attachment_row, chat_row, message_rows, seed_attachment, seed_message,
};
use crate::infra::outbox::OutboxQueues;

// ── Shared fixtures (also used by the message service tests) ─────────────

/// PDP allowing every request with the subject's tenant + owner constraint
/// (the shape a real PDP returns for owner-only chat access); counts calls.
pub struct OwnerPdp {
    calls: AtomicUsize,
}

impl OwnerPdp {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for OwnerPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let tenant = request
            .context
            .tenant_context
            .as_ref()
            .and_then(|tenant| tenant.root_id)
            .expect("tenant context");
        let constraints = if request.context.require_constraints {
            vec![Constraint {
                predicates: vec![
                    Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, tenant)),
                    Predicate::Eq(EqPredicate::new(
                        pep_properties::OWNER_ID,
                        request.subject.id,
                    )),
                ],
            }]
        } else {
            Vec::new()
        };
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints,
                ..Default::default()
            },
        })
    }
}

pub fn owner_authz(pdp: &Arc<OwnerPdp>) -> Arc<AuthzService> {
    Arc::new(AuthzService::from_client(
        Arc::clone(pdp) as Arc<dyn AuthZResolverApi>
    ))
}

pub fn ctx_of(tenant: Uuid, user: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .build()
        .unwrap()
}

pub fn a1() -> SecurityContext {
    ctx_of(TENANT_A, USER_A1)
}

struct Fixture {
    store: TestStore,
    pdp: Arc<OwnerPdp>,
    policy: Arc<FakePolicy>,
    service: ChatService,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_store(TestStore::new().await)
    }

    async fn without_outbox() -> Self {
        Self::with_store(TestStore::without_outbox().await)
    }

    fn with_store(store: TestStore) -> Self {
        let pdp = OwnerPdp::new();
        let authz = owner_authz(&pdp);
        let policy = FakePolicy::new(sample_catalog());
        let models = Arc::new(ModelsService::new(
            Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
            Arc::clone(&authz),
        ));
        let repos = store.repo.repositories();
        let service = ChatService::new(repos.chats, models, authz);
        Self {
            store,
            pdp,
            policy,
            service,
        }
    }

    async fn create(&self, title: &str) -> ChatDetail {
        self.service
            .create(&a1(), Some(title.to_owned()), None)
            .await
            .unwrap()
    }
}

fn is_invalid(err: &DomainError, expected: InvalidArgumentReason) -> bool {
    matches!(err, DomainError::InvalidArgument { reason, .. } if *reason == expected)
}

fn is_chat_not_found(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::NotFound {
            resource: ResourceKind::Chat,
            ..
        }
    )
}

fn title_filter(title: &str) -> ODataQuery {
    let expr = ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("title".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String(title.to_owned()))),
    );
    ODataQuery::new()
        .with_filter(expr)
        .with_filter_hash(format!("title-{title}"))
}

fn ids(page: &Page<ChatDetail>) -> Vec<Uuid> {
    page.items.iter().map(|chat| chat.id).collect()
}

async fn pause() {
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
}

// ── Title validation ─────────────────────────────────────────────────────

#[test]
fn title_is_trimmed_and_bounded_in_characters() {
    assert_eq!(validate_title("  Hello \t").unwrap(), "Hello");
    assert_eq!(validate_title(&"t".repeat(255)).unwrap().len(), 255);
    assert_eq!(
        validate_title(&format!("  {}  ", "\u{e9}".repeat(255)))
            .unwrap()
            .chars()
            .count(),
        255
    );
    for invalid in [
        String::new(),
        "   ".to_owned(),
        " \t\n ".to_owned(),
        "x".repeat(256),
    ] {
        let err = validate_title(&invalid).unwrap_err();
        assert!(
            is_invalid(&err, InvalidArgumentReason::InvalidTitle),
            "{invalid:?}"
        );
    }
}

// ── Create ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_without_fields_uses_the_default_model_and_the_callers_identity() {
    let fx = Fixture::without_outbox().await;

    let chat = fx.service.create(&a1(), None, None).await.unwrap();

    assert_eq!(chat.model, "default");
    assert_eq!(chat.title, None);
    assert!(!chat.is_temporary);
    assert_eq!(chat.message_count, 0);
    assert_eq!(chat.created_at, chat.updated_at);
    let row = chat_row(&fx.store, chat.id).await;
    assert_eq!((row.tenant_id, row.user_id), (TENANT_A, USER_A1));
    assert_eq!(row.deleted_at, None);
}

#[tokio::test]
async fn create_keeps_an_enabled_requested_model_and_trims_the_title() {
    let fx = Fixture::without_outbox().await;

    let chat = fx
        .service
        .create(
            &a1(),
            Some("  Hello  ".to_owned()),
            Some("premium".to_owned()),
        )
        .await
        .unwrap();

    assert_eq!(chat.model, "premium");
    assert_eq!(chat.title.as_deref(), Some("Hello"));
    assert_eq!(
        chat_row(&fx.store, chat.id).await.title.as_deref(),
        Some("Hello")
    );
}

#[tokio::test]
async fn create_rejects_unknown_disabled_and_empty_models() {
    let fx = Fixture::without_outbox().await;

    for model in ["missing", "hidden", ""] {
        let err = fx
            .service
            .create(&a1(), None, Some(model.to_owned()))
            .await
            .unwrap_err();
        assert!(
            is_invalid(&err, InvalidArgumentReason::InvalidModel),
            "{model}: {err:?}"
        );
    }
}

#[tokio::test]
async fn create_validates_the_title_before_authorization_and_model_lookup() {
    let fx = Fixture::without_outbox().await;

    let err = fx
        .service
        .create(&a1(), Some("   ".to_owned()), Some("missing".to_owned()))
        .await
        .unwrap_err();

    assert!(
        is_invalid(&err, InvalidArgumentReason::InvalidTitle),
        "{err:?}"
    );
    assert_eq!(fx.pdp.calls(), 0, "no PDP call for an invalid title");
    assert_eq!(
        fx.policy.snapshot_reads(),
        0,
        "no model lookup for an invalid title"
    );
}

#[tokio::test]
async fn create_is_denied_when_the_pdp_denies() {
    let store = TestStore::without_outbox().await;
    let policy = FakePolicy::new(sample_catalog());
    let authz = deny_all_authz();
    let models = Arc::new(ModelsService::new(
        Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
        Arc::clone(&authz),
    ));
    let service = ChatService::new(store.repo.repositories().chats, models, authz);

    let err = service.create(&a1(), None, None).await.unwrap_err();

    assert!(matches!(
        err,
        DomainError::PermissionDenied {
            resource: ResourceKind::Chat
        }
    ));
}

// ── Get / isolation ──────────────────────────────────────────────────────

#[tokio::test]
async fn get_returns_the_created_chat() {
    let fx = Fixture::without_outbox().await;
    let created = fx.create("Get me").await;

    let fetched = fx.service.get(&a1(), created.id).await.unwrap();

    assert_eq!(fetched, created);
}

#[tokio::test]
async fn other_users_and_tenants_get_not_found() {
    let fx = Fixture::without_outbox().await;
    let created = fx.create("Private").await;

    for ctx in [ctx_of(TENANT_A, USER_A2), ctx_of(TENANT_B, USER_B1)] {
        assert!(is_chat_not_found(
            &fx.service.get(&ctx, created.id).await.unwrap_err()
        ));
        assert!(is_chat_not_found(
            &fx.service
                .update_title(&ctx, created.id, "Stolen")
                .await
                .unwrap_err()
        ));
        assert!(is_chat_not_found(
            &fx.service.delete(&ctx, created.id).await.unwrap_err()
        ));
        let listed = fx
            .service
            .list(&ctx, &title_filter("Private"))
            .await
            .unwrap();
        assert!(listed.items.is_empty());
    }
    assert_eq!(fx.service.get(&a1(), created.id).await.unwrap(), created);
}

#[tokio::test]
async fn unknown_chat_is_not_found_with_the_requested_id() {
    let fx = Fixture::without_outbox().await;
    let id = Uuid::new_v4();

    match fx.service.get(&a1(), id).await.unwrap_err() {
        DomainError::NotFound {
            resource: ResourceKind::Chat,
            id: reported,
        } => assert_eq!(reported, id.to_string()),
        other => panic!("expected chat not_found, got {other:?}"),
    }
}

#[tokio::test]
async fn message_count_counts_only_live_messages() {
    let fx = Fixture::without_outbox().await;
    let chat = fx.create("Counting").await;
    for offset in 0..3 {
        seed_message(
            &fx.store,
            chat.id,
            TENANT_A,
            MessageSeed::new(MessageRole::User, offset),
        )
        .await;
    }
    let mut gone = MessageSeed::new(MessageRole::User, 4);
    gone.deleted = true;
    seed_message(&fx.store, chat.id, TENANT_A, gone).await;

    assert_eq!(
        fx.service.get(&a1(), chat.id).await.unwrap().message_count,
        3
    );
    let listed = fx
        .service
        .list(&a1(), &title_filter("Counting"))
        .await
        .unwrap();
    assert_eq!(listed.items[0].message_count, 3);
}

// ── Update ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn rename_changes_only_the_title_and_bumps_updated_at() {
    let fx = Fixture::without_outbox().await;
    let created = fx
        .service
        .create(&a1(), Some("Before".to_owned()), Some("premium".to_owned()))
        .await
        .unwrap();
    pause().await;

    let renamed = fx
        .service
        .update_title(&a1(), created.id, "  After  ")
        .await
        .unwrap();

    assert_eq!(renamed.title.as_deref(), Some("After"));
    assert_eq!(renamed.model, "premium");
    assert_eq!(renamed.created_at, created.created_at);
    assert!(renamed.updated_at > created.updated_at);
    assert_eq!(fx.service.get(&a1(), created.id).await.unwrap(), renamed);
}

#[tokio::test]
async fn rename_validates_the_title_first() {
    let fx = Fixture::without_outbox().await;
    let created = fx.create("Unchanged").await;
    let calls = fx.pdp.calls();

    for title in ["", "    ", &"x".repeat(256)] {
        let err = fx
            .service
            .update_title(&a1(), created.id, title)
            .await
            .unwrap_err();
        assert!(is_invalid(&err, InvalidArgumentReason::InvalidTitle));
    }

    assert_eq!(fx.pdp.calls(), calls);
    assert_eq!(
        chat_row(&fx.store, created.id).await.title.as_deref(),
        Some("Unchanged")
    );
}

// ── Delete ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_soft_deletes_marks_attachments_and_enqueues_one_cleanup_message() {
    let fx = Fixture::new().await;
    let chat = fx.create("Soft delete").await;
    let neighbour = fx.create("Neighbour").await;
    let message = seed_message(
        &fx.store,
        chat.id,
        TENANT_A,
        MessageSeed::new(MessageRole::User, 0),
    )
    .await;
    let document = seed_attachment(
        &fx.store,
        chat.id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "a.pdf"),
    )
    .await;
    let image = seed_attachment(
        &fx.store,
        chat.id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "b.png"),
    )
    .await;
    let mut already =
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "c.pdf");
    already.deleted = true;
    already.cleanup_status = Some("done".to_owned());
    let already = seed_attachment(&fx.store, chat.id, TENANT_A, already).await;
    let untouched = seed_attachment(
        &fx.store,
        neighbour.id,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "d.pdf"),
    )
    .await;

    fx.service.delete(&a1(), chat.id).await.unwrap();

    let row = chat_row(&fx.store, chat.id).await;
    let deleted_at = row.deleted_at.expect("soft-deleted");
    assert_eq!(row.updated_at, deleted_at);
    assert_eq!(row.title.as_deref(), Some("Soft delete"));
    for attachment in [document, image] {
        let row = attachment_row(&fx.store, attachment).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
        assert_eq!(row.cleanup_updated_at, Some(deleted_at));
        assert_eq!(row.deleted_at, None, "attachment rows are not deleted");
    }
    assert_eq!(
        attachment_row(&fx.store, already)
            .await
            .cleanup_status
            .as_deref(),
        Some("done")
    );
    assert_eq!(
        attachment_row(&fx.store, untouched).await.cleanup_status,
        None
    );
    let messages = message_rows(&fx.store, chat.id).await;
    assert_eq!(messages.len(), 1);
    assert_eq!((messages[0].id, messages[0].deleted_at), (message, None));

    let queue = OutboxQueues::default().chat_cleanup;
    let delivered = fx.store.wait_delivered(&queue, 1).await;
    assert_eq!(delivered.len(), 1, "exactly one chat-cleanup message");
    let payload = &delivered[0].payload;
    assert_eq!(delivered[0].payload_type, "application/json");
    assert_eq!(payload["tenant_id"], TENANT_A.to_string());
    assert_eq!(payload["chat_id"], chat.id.to_string());
    assert_eq!(payload["reason"], "chat_soft_delete");
    let system_request_id: Uuid = payload["system_request_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(system_request_id.get_version_num(), 4);
    let reported = time::OffsetDateTime::parse(
        payload["chat_deleted_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!(reported, deleted_at);
}

#[tokio::test]
async fn second_delete_is_not_found_and_enqueues_nothing() {
    let fx = Fixture::new().await;
    let chat = fx.create("Twice").await;
    fx.service.delete(&a1(), chat.id).await.unwrap();

    let err = fx.service.delete(&a1(), chat.id).await.unwrap_err();

    assert!(is_chat_not_found(&err));
    let queue = OutboxQueues::default().chat_cleanup;
    assert_eq!(fx.store.wait_delivered(&queue, 2).await.len(), 1);
    assert!(is_chat_not_found(
        &fx.service.get(&a1(), chat.id).await.unwrap_err()
    ));
    assert!(is_chat_not_found(
        &fx.service
            .update_title(&a1(), chat.id, "Revived")
            .await
            .unwrap_err()
    ));
}

#[tokio::test]
async fn delete_rolls_back_when_the_outbox_is_not_running() {
    let fx = Fixture::without_outbox().await;
    let chat = fx.create("Rollback").await;

    let err = fx.service.delete(&a1(), chat.id).await.unwrap_err();

    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
    assert_eq!(chat_row(&fx.store, chat.id).await.deleted_at, None);
}

// ── List ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_defaults_to_updated_at_desc_with_page_size_20() {
    let fx = Fixture::without_outbox().await;
    let first = fx.create("order").await;
    pause().await;
    let second = fx.create("order").await;
    pause().await;
    let third = fx.create("order").await;

    let page = fx
        .service
        .list(&a1(), &title_filter("order"))
        .await
        .unwrap();
    assert_eq!(page.page_info.limit, 20);
    assert_eq!(ids(&page), [third.id, second.id, first.id]);

    pause().await;
    fx.service
        .update_title(&a1(), first.id, "order")
        .await
        .unwrap();
    let page = fx
        .service
        .list(&a1(), &title_filter("order"))
        .await
        .unwrap();
    assert_eq!(ids(&page), [first.id, third.id, second.id]);
}

#[tokio::test]
async fn list_excludes_deleted_chats() {
    let fx = Fixture::new().await;
    let kept = fx.create("mine").await;
    let deleted = fx.create("mine").await;
    fx.service.delete(&a1(), deleted.id).await.unwrap();

    let page = fx.service.list(&a1(), &title_filter("mine")).await.unwrap();

    assert_eq!(ids(&page), [kept.id]);
}

#[tokio::test]
async fn list_pages_forward_and_back_with_cursors() {
    let fx = Fixture::without_outbox().await;
    let mut created = Vec::new();
    for _ in 0..3 {
        created.push(fx.create("page").await.id);
        pause().await;
    }
    created.reverse();

    let first = fx
        .service
        .list(&a1(), &title_filter("page").with_limit(2))
        .await
        .unwrap();
    assert_eq!(ids(&first), created[..2]);
    assert_eq!(first.page_info.prev_cursor, None);
    let next = first.page_info.next_cursor.clone().expect("next cursor");

    let second_query = title_filter("page")
        .with_limit(2)
        .with_cursor(CursorV1::decode(&next).unwrap());
    let second = fx.service.list(&a1(), &second_query).await.unwrap();
    assert_eq!(ids(&second), created[2..]);
    assert_eq!(second.page_info.next_cursor, None);
    let prev = second.page_info.prev_cursor.clone().expect("prev cursor");

    let back_query = title_filter("page")
        .with_limit(2)
        .with_cursor(CursorV1::decode(&prev).unwrap());
    let back = fx.service.list(&a1(), &back_query).await.unwrap();
    assert_eq!(ids(&back), created[..2]);
}

#[tokio::test]
async fn list_clamps_the_limit_and_rejects_zero() {
    let fx = Fixture::without_outbox().await;

    let page = fx
        .service
        .list(&a1(), &ODataQuery::new().with_limit(500))
        .await
        .unwrap();
    assert_eq!(page.page_info.limit, 100);

    let err = fx
        .service
        .list(&a1(), &ODataQuery::new().with_limit(0))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DomainError::InvalidQuery(toolkit_odata::Error::InvalidLimit)
    ));
}

#[tokio::test]
async fn list_filters_by_id_and_orders_by_title() {
    let fx = Fixture::without_outbox().await;
    let b = fx.create("sort-b").await;
    let c = fx.create("sort-c").await;
    let a = fx.create("sort-a").await;

    let by_id = ODataQuery::new().with_filter(ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(c.id))),
    ));
    assert_eq!(ids(&fx.service.list(&a1(), &by_id).await.unwrap()), [c.id]);

    let starts = ODataQuery::new().with_filter(ast::Expr::Function(
        "startswith".to_owned(),
        vec![
            ast::Expr::Identifier("title".to_owned()),
            ast::Expr::Value(ast::Value::String("sort-".to_owned())),
        ],
    ));
    for (dir, expected) in [
        (SortDir::Asc, [a.id, b.id, c.id]),
        (SortDir::Desc, [c.id, b.id, a.id]),
    ] {
        let query = starts.clone().with_order(ODataOrderBy(vec![OrderKey {
            field: "title".to_owned(),
            dir,
        }]));
        assert_eq!(
            ids(&fx.service.list(&a1(), &query).await.unwrap()),
            expected
        );
    }
}

#[tokio::test]
async fn list_rejects_unknown_fields() {
    let fx = Fixture::without_outbox().await;

    let filter = ODataQuery::new().with_filter(ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("model".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String("standard".to_owned()))),
    ));
    let err = fx.service.list(&a1(), &filter).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::InvalidQuery(toolkit_odata::Error::InvalidFilter(_))
        ),
        "{err:?}"
    );

    let order = ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
        field: "model".to_owned(),
        dir: SortDir::Asc,
    }]));
    let err = fx.service.list(&a1(), &order).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::InvalidQuery(toolkit_odata::Error::InvalidOrderByField(_))
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn list_counts_messages_per_chat() {
    let fx = Fixture::without_outbox().await;
    let busy = fx.create("count").await;
    let empty = fx.create("count").await;
    for offset in 0..2 {
        seed_message(
            &fx.store,
            busy.id,
            TENANT_A,
            MessageSeed::new(MessageRole::User, offset),
        )
        .await;
    }

    let page = fx
        .service
        .list(&a1(), &title_filter("count"))
        .await
        .unwrap();

    let counts: Vec<(Uuid, u64)> = page.items.iter().map(|c| (c.id, c.message_count)).collect();
    assert!(counts.contains(&(busy.id, 2)));
    assert!(counts.contains(&(empty.id, 0)));
}
