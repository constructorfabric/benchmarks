#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError};
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use super::{BaseLicense, register_routes};
use crate::api::rest::RestState;
use crate::config::MiniChatConfig;
use crate::domain::attachment_service::tests::{FakeStorage, fast_timing};
use crate::domain::attachment_service::{AttachmentPorts, AttachmentService, AttachmentSettings};
use crate::domain::authz::AuthzService;
use crate::domain::chat_service::ChatService;
use crate::domain::chat_service::tests::{OwnerPdp, a1, ctx_of, owner_authz};
use crate::domain::message_service::MessageService;
use crate::domain::models_service::ModelsService;
use crate::domain::models_service::tests::{
    FakePolicy, allow_all_authz, deny_all_authz, sample_catalog, test_ctx,
};
use crate::domain::ports::{
    AttachmentKind, AttachmentStatus, MessageRole, QuotaRepository, Reaction, TurnState,
};
use crate::domain::quota_service::{QuotaService, QuotaSettings};
use crate::domain::stream::service::tests::{FakeProvider, FakeResolver, test_catalog};
use crate::domain::stream::{StreamService, StreamSettings};
use crate::domain::turn_service::TurnService;
use crate::infra::db::repo::tests::{
    AttachmentSeed, MessageSeed, TENANT_A, TENANT_B, TestStore, TurnSeed, USER_A1, USER_B1,
    link_attachment, seed_attachment, seed_chat, seed_message, seed_reaction, seed_turn,
};
use crate::infra::thumbnail::{ThumbnailLimits, WebpThumbnailer};

const PREFIX: &str = "/mini-chat";
const MODEL_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.model.v1~";
const CHAT_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.chat.v1~";
const CHATS: &str = "/mini-chat/v1/chats";
const ATTACHMENT_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.attachment.v1~";
/// Document size limit of the router fixture's attachment service.
const UPLOAD_DOCUMENT_MAX_BYTES: u64 = 1024;

/// REST state over `store`: the Models API over `policy` / `models_authz`,
/// chats and messages over an owner-constraint PDP.
fn state_with(
    store: &TestStore,
    policy: Arc<FakePolicy>,
    models_authz: Arc<AuthzService>,
) -> RestState {
    let models = Arc::new(ModelsService::new(
        policy as Arc<dyn MiniChatModelPolicyPluginClientV1>,
        models_authz,
    ));
    let owner = owner_authz(&OwnerPdp::new());
    let chat_models = Arc::new(ModelsService::new(
        FakePolicy::new(sample_catalog()) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
        Arc::clone(&owner),
    ));
    let repos = store.repo.repositories();
    let config = MiniChatConfig::default();
    let quota = Arc::new(QuotaService::new(
        Arc::clone(&store.repo) as Arc<dyn QuotaRepository>,
        crate::domain::stream::service::tests::FakePolicy::new(test_catalog())
            as Arc<dyn MiniChatModelPolicyPluginClientV1>,
        Arc::clone(&owner),
        QuotaSettings::from_config(&config),
    ));
    let stream = Arc::new(StreamService::new(
        store.db.clone(),
        repos.clone(),
        Arc::clone(&quota),
        Arc::clone(&owner),
        Arc::new(FakeResolver),
        Arc::new(FakeProvider::default()),
        StreamSettings::from_config(&config),
    ));
    let turns = Arc::new(TurnService::new(
        store.db.clone(),
        repos.clone(),
        Arc::clone(&owner),
        Arc::clone(&stream),
    ));
    let storage = FakeStorage::new();
    let attachments = Arc::new(AttachmentService::new(
        AttachmentPorts {
            chats: repos.chats.clone(),
            attachments: Arc::clone(&store.repo) as _,
            vector_stores: Arc::clone(&store.repo) as _,
            files: Arc::clone(&storage) as _,
            stores: storage as _,
            thumbnails: Arc::new(WebpThumbnailer::new(ThumbnailLimits::from_config(
                &config.thumbnail,
            ))),
        },
        Arc::clone(&chat_models),
        Arc::clone(&owner),
        AttachmentSettings {
            document_max_bytes: UPLOAD_DOCUMENT_MAX_BYTES,
            timing: fast_timing(),
            ..AttachmentSettings::from_config(&config)
        },
        tokio_util::sync::CancellationToken::new(),
    ));
    RestState {
        models,
        chats: Arc::new(ChatService::new(
            repos.chats.clone(),
            chat_models,
            Arc::clone(&owner),
        )),
        messages: Arc::new(MessageService::new(repos, owner)),
        stream,
        turns,
        quota,
        attachments,
        sse_ping_interval: Duration::from_secs(15),
    }
}

/// Router plus the store backing its chat and message services.
struct Fixture {
    store: TestStore,
    router: Router,
}

impl Fixture {
    async fn with(policy: Arc<FakePolicy>, models_authz: Arc<AuthzService>) -> Self {
        Self::with_prefix(policy, models_authz, PREFIX).await
    }

    async fn with_prefix(
        policy: Arc<FakePolicy>,
        models_authz: Arc<AuthzService>,
        prefix: &str,
    ) -> Self {
        let store = TestStore::new().await;
        let state = state_with(&store, policy, models_authz);
        let openapi = OpenApiRegistryImpl::new();
        let router = register_routes(Router::new(), &openapi, state, prefix);
        Self { store, router }
    }

    async fn new() -> Self {
        Self::with(FakePolicy::new(sample_catalog()), allow_all_authz()).await
    }

    async fn call(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let header_of = |name: header::HeaderName| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        let content_type = header_of(header::CONTENT_TYPE);
        let location = header_of(header::LOCATION);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply {
            status,
            content_type,
            location,
            raw: bytes.to_vec(),
            body,
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.call(request("GET", uri, a1(), None)).await
    }

    async fn send_json(&self, method: &str, uri: &str, body: &Value) -> Reply {
        self.call(request(
            method,
            uri,
            a1(),
            Some(("application/json", body.to_string().into_bytes())),
        ))
        .await
    }

    async fn create(&self, body: &Value) -> Value {
        let reply = self.send_json("POST", CHATS, body).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body
    }
}

/// Response status, selected headers and body.
struct Reply {
    status: StatusCode,
    content_type: Option<String>,
    location: Option<String>,
    raw: Vec<u8>,
    body: Value,
}

/// Request with the `SecurityContext` injected the way the api-gateway does
/// (request extension) and an optional `(content type, body)`.
fn request(
    method: &str,
    uri: &str,
    ctx: SecurityContext,
    body: Option<(&str, Vec<u8>)>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some((content_type, bytes)) => {
            if !content_type.is_empty() {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            Body::from(bytes)
        }
        None => Body::empty(),
    };
    let mut request = builder.body(body).unwrap();
    request.extensions_mut().insert(ctx);
    request
}

/// GET request as the Models API tests send it.
fn get(uri: &str, ctx: SecurityContext) -> Request<Body> {
    request("GET", uri, ctx, None)
}

async fn router_with(policy: Arc<FakePolicy>, authz: Arc<AuthzService>) -> Fixture {
    Fixture::with(policy, authz).await
}

async fn router() -> Fixture {
    Fixture::new().await
}

async fn send(fixture: Fixture, uri: &str) -> (StatusCode, Option<String>, Value) {
    let reply = fixture.call(get(uri, test_ctx())).await;
    (reply.status, reply.content_type, reply.body)
}

#[tokio::test]
async fn list_models_returns_the_enabled_projection_in_catalog_order() {
    let (status, content_type, body) = send(router().await, "/mini-chat/v1/models").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(content_type.unwrap().starts_with("application/json"));
    assert_eq!(
        body,
        json!({
            "items": [
                {
                    "model_id": "premium",
                    "display_name": "Model premium",
                    "tier": "premium",
                    "multiplier_display": "3x",
                    "description": "Best for complex reasoning",
                    "multimodal_capabilities": ["VISION_INPUT", "RAG"],
                    "context_window": 128_000
                },
                {
                    "model_id": "standard",
                    "display_name": "Model standard",
                    "tier": "standard",
                    "multiplier_display": "1x",
                    "multimodal_capabilities": [],
                    "context_window": 128_000
                },
                {
                    "model_id": "default",
                    "display_name": "Model default",
                    "tier": "standard",
                    "multiplier_display": "1x",
                    "multimodal_capabilities": [],
                    "context_window": 128_000
                }
            ]
        })
    );
}

#[tokio::test]
async fn list_models_never_exposes_provider_or_credit_internals() {
    let (_, _, body) = send(router().await, "/mini-chat/v1/models").await;

    let raw = body.to_string();
    for forbidden in [
        "provider",
        "credit_multiplier",
        "policy_version",
        "max_output",
        "is_default",
        "preference",
        "OpenAI",
    ] {
        assert!(!raw.contains(forbidden), "{forbidden} exposed: {raw}");
    }
}

#[tokio::test]
async fn get_model_returns_the_same_projection_as_the_list() {
    let (_, _, list) = send(router().await, "/mini-chat/v1/models").await;

    let (status, _, body) = send(router().await, "/mini-chat/v1/models/premium").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, list["items"][0]);
}

#[tokio::test]
async fn get_model_without_description_omits_the_field() {
    let (status, _, body) = send(router().await, "/mini-chat/v1/models/standard").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("description").is_none(), "{body}");
}

#[tokio::test]
async fn get_disabled_or_unknown_model_is_a_model_not_found_problem() {
    let mut problems = Vec::new();
    for id in ["hidden", "missing"] {
        let (status, content_type, body) =
            send(router().await, &format!("/mini-chat/v1/models/{id}")).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(
            content_type
                .unwrap()
                .starts_with("application/problem+json"),
            "{id}"
        );
        assert_eq!(body["status"], json!(404));
        assert_eq!(body["context"]["resource_type"], json!(MODEL_RESOURCE_TYPE));
        problems.push(body);
    }
    assert_eq!(problems[0]["type"], problems[1]["type"]);
    assert_eq!(problems[0]["title"], problems[1]["title"]);
}

#[tokio::test]
async fn pdp_denial_is_a_403_authz_denied_problem() {
    let router = router_with(FakePolicy::new(sample_catalog()), deny_all_authz()).await;

    let (status, _, body) = send(router, "/mini-chat/v1/models").await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["context"]["reason"], json!("AUTHZ_DENIED"));
}

#[tokio::test]
async fn plugin_failure_is_a_500_problem_without_the_diagnostic() {
    let router = router_with(
        FakePolicy::failing(MiniChatModelPolicyPluginError::Internal(
            "secret plugin detail".to_owned(),
        )),
        allow_all_authz(),
    )
    .await;

    let (status, _, body) = send(router, "/mini-chat/v1/models/premium").await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(!body.to_string().contains("secret plugin detail"));
}

#[tokio::test]
async fn routes_honour_a_trailing_slash_in_the_prefix() {
    let fixture = Fixture::with_prefix(
        FakePolicy::new(sample_catalog()),
        allow_all_authz(),
        "/chat-api/",
    )
    .await;

    let created = fixture
        .call(request(
            "POST",
            "/chat-api/v1/chats",
            a1(),
            Some(("application/json", b"{}".to_vec())),
        ))
        .await;
    let (status, _, body) = send(fixture, "/chat-api/v1/models").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(
        created.location.unwrap(),
        format!(
            "/chat-api/v1/chats/{}",
            created.body["id"].as_str().unwrap()
        )
    );
}

#[test]
fn routes_require_the_platform_base_license_feature() {
    assert_eq!(
        BaseLicense.as_ref(),
        "gts.cf.core.lic.feat.v1~cf.core.global.base.v1"
    );
}

/// The `OpenAPI` document of the registered routes.
async fn openapi_doc() -> Value {
    let store = TestStore::without_outbox().await;
    let openapi = OpenApiRegistryImpl::new();
    let state = state_with(&store, FakePolicy::new(Vec::new()), allow_all_authz());
    let _router = register_routes(Router::new(), &openapi, state, PREFIX);
    serde_json::to_value(
        openapi
            .build_openapi(&toolkit::api::OpenApiInfo::default())
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn openapi_declares_the_models_operations() {
    let doc = openapi_doc().await;
    let list = &doc["paths"]["/mini-chat/v1/models"]["get"];
    assert_eq!(list["operationId"], json!("mini_chat.list_models"));
    assert_eq!(
        list["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        json!("#/components/schemas/ModelListDto")
    );
    assert!(list["responses"]["503"]["headers"]["Retry-After"].is_object());
    let get = &doc["paths"]["/mini-chat/v1/models/{id}"]["get"];
    assert_eq!(get["operationId"], json!("mini_chat.get_model"));
    assert_eq!(
        get["responses"]["404"]["content"]["application/problem+json"]["schema"]["$ref"],
        json!("#/components/schemas/Problem")
    );
    let model = &doc["components"]["schemas"]["ModelDto"];
    assert_eq!(
        model["required"],
        json!([
            "model_id",
            "display_name",
            "tier",
            "multiplier_display",
            "multimodal_capabilities",
            "context_window"
        ])
    );
    assert_eq!(
        doc["components"]["schemas"]["ModelTierDto"]["enum"],
        json!(["standard", "premium"])
    );
}

// ── Chats ────────────────────────────────────────────────────────────────

/// `(content type, body, expected status, expected field-violation reason)`.
type ExtractorCase = (&'static str, &'static [u8], StatusCode, &'static str);

fn field_violation_reason(body: &Value) -> Option<&str> {
    body["context"]["field_violations"][0]["reason"].as_str()
}

#[tokio::test]
async fn create_chat_is_201_with_location_and_omits_the_absent_title() {
    let fixture = Fixture::new().await;

    let reply = fixture.send_json("POST", CHATS, &json!({})).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert!(reply.content_type.unwrap().starts_with("application/json"));
    let id = reply.body["id"].as_str().unwrap();
    assert_eq!(
        reply.location.as_deref(),
        Some(format!("/mini-chat/v1/chats/{id}").as_str())
    );
    let keys: Vec<&str> = reply
        .body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys.iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "id",
            "model",
            "is_temporary",
            "message_count",
            "created_at",
            "updated_at"
        ]
        .into_iter()
        .collect(),
        "{}",
        reply.body
    );
    assert_eq!(reply.body["message_count"], json!(0));
    assert_eq!(reply.body["is_temporary"], json!(false));
}

#[tokio::test]
async fn created_titled_chat_is_returned_by_get_and_list() {
    let fixture = Fixture::new().await;
    let created = fixture
        .create(&json!({ "title": "  Hello  ", "model": "standard" }))
        .await;
    let id = created["id"].as_str().unwrap();

    let got = fixture.get(&format!("{CHATS}/{id}")).await;
    let listed = fixture.get(CHATS).await;

    assert_eq!(created["title"], json!("Hello"));
    assert_eq!(created["model"], json!("standard"));
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(got.body, created);
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.body["items"], json!([created]));
    assert_eq!(listed.body["page_info"]["limit"], json!(20));
}

#[tokio::test]
async fn json_extractor_rejections_are_canonical_problems() {
    let fixture = Fixture::new().await;
    let cases: [ExtractorCase; 3] = [
        (
            "application/json",
            b"{\"title\": \"broken\"",
            StatusCode::BAD_REQUEST,
            "json_syntax_error",
        ),
        (
            "application/json",
            b"{\"title\": 5}",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_json_body",
        ),
        (
            "",
            b"{\"title\": \"no content type\"}",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "missing_json_content_type",
        ),
    ];

    for (content_type, body, status, reason) in cases {
        let reply = fixture
            .call(request(
                "POST",
                CHATS,
                a1(),
                Some((content_type, body.to_vec())),
            ))
            .await;

        assert_eq!(reply.status, status, "{reason}: {}", reply.body);
        assert!(
            reply
                .content_type
                .unwrap()
                .starts_with("application/problem+json")
        );
        assert_eq!(field_violation_reason(&reply.body), Some(reason));
        assert_eq!(
            reply.body["context"]["field_violations"][0]["field"],
            json!("body")
        );
    }
}

#[tokio::test]
async fn invalid_title_is_a_400_field_violation() {
    let fixture = Fixture::new().await;

    let reply = fixture
        .send_json("POST", CHATS, &json!({ "title": "   " }))
        .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(field_violation_reason(&reply.body), Some("INVALID_TITLE"));
}

#[tokio::test]
async fn non_uuid_path_ids_are_invalid_path_params() {
    let fixture = Fixture::new().await;
    let chat = Uuid::new_v4();
    let uris = [
        ("GET", format!("{CHATS}/not-a-uuid")),
        ("DELETE", format!("{CHATS}/12345")),
        ("GET", format!("{CHATS}/not-a-uuid/messages")),
        ("DELETE", format!("{CHATS}/{chat}/messages/nope/reaction")),
        ("GET", format!("{CHATS}/{chat}/turns/nope")),
    ];

    for (method, uri) in uris {
        let reply = fixture.call(request(method, &uri, a1(), None)).await;

        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            reply.body
        );
        assert_eq!(
            field_violation_reason(&reply.body),
            Some("invalid_path_params"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn update_chat_renames_and_requires_a_string_title() {
    let fixture = Fixture::new().await;
    let created = fixture.create(&json!({ "title": "Before" })).await;
    let uri = format!("{CHATS}/{}", created["id"].as_str().unwrap());

    let renamed = fixture
        .send_json(
            "PATCH",
            &uri,
            &json!({ "title": " After ", "model": "premium", "foo": 1 }),
        )
        .await;
    let missing = fixture.send_json("PATCH", &uri, &json!({})).await;
    let null = fixture
        .send_json("PATCH", &uri, &json!({ "title": null }))
        .await;

    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    assert_eq!(renamed.body["title"], json!("After"));
    assert_eq!(renamed.body["model"], created["model"]);
    for reply in [missing, null] {
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.body
        );
        assert_eq!(
            field_violation_reason(&reply.body),
            Some("invalid_json_body")
        );
    }
}

#[tokio::test]
async fn delete_chat_is_204_then_404() {
    let fixture = Fixture::new().await;
    let created = fixture.create(&json!({})).await;
    let uri = format!("{CHATS}/{}", created["id"].as_str().unwrap());

    let deleted = fixture.call(request("DELETE", &uri, a1(), None)).await;
    let again = fixture.call(request("DELETE", &uri, a1(), None)).await;
    let get = fixture.get(&uri).await;

    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(deleted.raw.is_empty());
    for reply in [again, get] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
        assert_eq!(
            reply.body["context"]["resource_type"],
            json!(CHAT_RESOURCE_TYPE)
        );
    }
}

#[tokio::test]
async fn another_tenants_chat_is_not_found() {
    let fixture = Fixture::new().await;
    let created = fixture.create(&json!({ "title": "Mine" })).await;
    let uri = format!("{CHATS}/{}", created["id"].as_str().unwrap());

    let reply = fixture
        .call(request("GET", &uri, ctx_of(TENANT_B, USER_B1), None))
        .await;

    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
}

#[tokio::test]
async fn list_chats_odata_errors_are_400() {
    let fixture = Fixture::new().await;

    for query in [
        "limit=0",
        "$filter=model%20eq%20'x'",
        "cursor=nope",
        "$skip=1",
    ] {
        let reply = fixture.get(&format!("{CHATS}?{query}")).await;

        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{query}: {}",
            reply.body
        );
    }
}

// ── Messages, reactions, turns ───────────────────────────────────────────

#[tokio::test]
async fn list_messages_applies_the_dto_omission_rules() {
    let fixture = Fixture::new().await;
    let store = &fixture.store;
    let chat = seed_chat(store, TENANT_A, USER_A1, None).await;
    let request_id = Uuid::new_v4();
    let mut user = MessageSeed::new(MessageRole::User, -20);
    user.request_id = Some(request_id);
    user.model = Some("leaked".to_owned());
    user.input_tokens = 9;
    let user = seed_message(store, chat, TENANT_A, user).await;
    let mut assistant = MessageSeed::new(MessageRole::Assistant, -10);
    assistant.request_id = Some(request_id);
    assistant.input_tokens = 21;
    let assistant = seed_message(store, chat, TENANT_A, assistant).await;
    let image = seed_attachment(
        store,
        chat,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "cat.png"),
    )
    .await;
    let document = seed_attachment(
        store,
        chat,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Document, AttachmentStatus::Ready, "a.pdf"),
    )
    .await;
    link_attachment(store, chat, TENANT_A, user, image, -20).await;
    link_attachment(store, chat, TENANT_A, user, document, -19).await;

    let reply = fixture.get(&format!("{CHATS}/{chat}/messages")).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let items = reply.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0],
        json!({
            "id": user.to_string(),
            "request_id": request_id.to_string(),
            "role": "user",
            "content": "user message",
            "attachments": [
                {
                    "attachment_id": image.to_string(),
                    "kind": "image",
                    "filename": "cat.png",
                    "status": "ready",
                    "img_thumbnail": {
                        "content_type": "image/webp",
                        "width": 64,
                        "height": 48,
                        "data_base64": "UklGRg=="
                    }
                },
                {
                    "attachment_id": document.to_string(),
                    "kind": "document",
                    "filename": "a.pdf",
                    "status": "ready"
                }
            ],
            "my_reaction": null,
            "created_at": items[0]["created_at"]
        })
    );
    let assistant_item = &items[1];
    assert_eq!(assistant_item["id"], json!(assistant.to_string()));
    assert_eq!(assistant_item["model"], json!("standard"));
    assert_eq!(assistant_item["input_tokens"], json!(21));
    assert!(
        assistant_item.get("output_tokens").is_none(),
        "{assistant_item}"
    );
    assert_eq!(assistant_item["attachments"], json!([]));
    assert_eq!(assistant_item["my_reaction"], Value::Null);
    let raw = reply.body.to_string();
    for hidden in ["chat_id", "tenant_id", "user_id", "provider", "file-secret"] {
        assert!(!raw.contains(hidden), "{hidden} exposed: {raw}");
    }
}

#[tokio::test]
async fn list_messages_accepts_a_quoted_uuid_id_filter() {
    let fixture = Fixture::new().await;
    let store = &fixture.store;
    let chat = seed_chat(store, TENANT_A, USER_A1, None).await;
    seed_message(
        store,
        chat,
        TENANT_A,
        MessageSeed::new(MessageRole::User, -20),
    )
    .await;
    let assistant = seed_message(
        store,
        chat,
        TENANT_A,
        MessageSeed::new(MessageRole::Assistant, -10),
    )
    .await;
    seed_reaction(store, assistant, TENANT_A, USER_A1, Reaction::Dislike).await;

    let reply = fixture
        .get(&format!(
            "{CHATS}/{chat}/messages?$filter=id%20eq%20'{assistant}'"
        ))
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let items = reply.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], json!(assistant.to_string()));
    assert_eq!(items[0]["my_reaction"], json!("dislike"));
}

#[tokio::test]
async fn reactions_are_set_and_removed() {
    let fixture = Fixture::new().await;
    let store = &fixture.store;
    let chat = seed_chat(store, TENANT_A, USER_A1, None).await;
    let assistant = seed_message(
        store,
        chat,
        TENANT_A,
        MessageSeed::new(MessageRole::Assistant, -10),
    )
    .await;
    let uri = format!("{CHATS}/{chat}/messages/{assistant}/reaction");

    let set = fixture
        .send_json("PUT", &uri, &json!({ "reaction": "like" }))
        .await;
    let invalid = fixture
        .send_json("PUT", &uri, &json!({ "reaction": "love" }))
        .await;
    let removed = fixture.call(request("DELETE", &uri, a1(), None)).await;

    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert_eq!(set.body["message_id"], json!(assistant.to_string()));
    assert_eq!(set.body["reaction"], json!("like"));
    assert!(set.body["created_at"].is_string());
    assert_eq!(set.body.as_object().unwrap().len(), 3, "{}", set.body);
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST, "{}", invalid.body);
    assert_eq!(
        field_violation_reason(&invalid.body),
        Some("INVALID_REACTION")
    );
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert!(removed.raw.is_empty());
}

#[tokio::test]
async fn turn_status_omits_absent_optional_fields() {
    let fixture = Fixture::new().await;
    let store = &fixture.store;
    let chat = seed_chat(store, TENANT_A, USER_A1, None).await;
    let running = TurnSeed::new(TurnState::Running);
    let running_id = running.request_id;
    seed_turn(store, chat, TENANT_A, running).await;
    let mut failed = TurnSeed::new(TurnState::Failed);
    failed.error_code = Some("provider_error".to_owned());
    let failed_id = failed.request_id;
    seed_turn(store, chat, TENANT_A, failed).await;

    let running = fixture
        .get(&format!("{CHATS}/{chat}/turns/{running_id}"))
        .await;
    let failed = fixture
        .get(&format!("{CHATS}/{chat}/turns/{failed_id}"))
        .await;

    assert_eq!(running.status, StatusCode::OK, "{}", running.body);
    assert_eq!(
        running.body,
        json!({
            "request_id": running_id.to_string(),
            "state": "running",
            "updated_at": running.body["updated_at"]
        })
    );
    assert!(running.body["updated_at"].is_string());
    assert_eq!(failed.body["state"], json!("error"));
    assert_eq!(failed.body["error_code"], json!("provider_error"));
    assert!(failed.body.get("assistant_message_id").is_none());
}

// ── OpenAPI ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn openapi_declares_the_chat_operations() {
    let doc = openapi_doc().await;
    let paths = &doc["paths"];

    let expected = [
        ("/mini-chat/v1/chats", "post", "mini_chat.create_chat"),
        ("/mini-chat/v1/chats", "get", "mini_chat.list_chats"),
        ("/mini-chat/v1/chats/{id}", "get", "mini_chat.get_chat"),
        ("/mini-chat/v1/chats/{id}", "patch", "mini_chat.update_chat"),
        (
            "/mini-chat/v1/chats/{id}",
            "delete",
            "mini_chat.delete_chat",
        ),
        (
            "/mini-chat/v1/chats/{id}/messages",
            "get",
            "mini_chat.list_messages",
        ),
        (
            "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction",
            "put",
            "mini_chat.put_reaction",
        ),
        (
            "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction",
            "delete",
            "mini_chat.delete_reaction",
        ),
        (
            "/mini-chat/v1/chats/{id}/turns/{request_id}",
            "get",
            "mini_chat.get_turn",
        ),
    ];
    for (path, method, operation_id) in expected {
        let operation = &paths[path][method];
        assert_eq!(
            operation["operationId"],
            json!(operation_id),
            "{path} {method}"
        );
        assert!(
            operation["responses"]["503"]["headers"]["Retry-After"].is_object(),
            "{operation_id}"
        );
    }

    let create = &paths["/mini-chat/v1/chats"]["post"];
    assert_eq!(
        create["responses"]["201"]["content"]["application/json"]["schema"]["$ref"],
        json!("#/components/schemas/ChatDetailDto")
    );
    assert!(create["responses"]["201"]["headers"]["Location"].is_object());
    assert_eq!(
        create["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        json!("#/components/schemas/CreateChatReq")
    );
    assert_eq!(
        create["x-throttling-rate-limit-zone"],
        json!("rl_mini_chat_chat")
    );
    assert_eq!(
        create["x-throttling-in-flight-limit-zone"],
        json!("ifl_mini_chat_chat")
    );
    let mut statuses: Vec<&String> = create["responses"].as_object().unwrap().keys().collect();
    statuses.sort();
    assert_eq!(statuses, ["201", "400", "401", "403", "422", "500", "503"]);
    assert_eq!(
        paths["/mini-chat/v1/chats/{id}/messages"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["$ref"],
        json!("#/components/schemas/Page_MiniChatMessageDto")
    );
    assert_eq!(
        paths["/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction"]["put"]["responses"]["200"]["content"]
            ["application/json"]["schema"]["$ref"],
        json!("#/components/schemas/MiniChatReactionDto")
    );

    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["ChatDetailDto"]["required"],
        json!([
            "id",
            "model",
            "is_temporary",
            "message_count",
            "created_at",
            "updated_at"
        ])
    );
    assert_eq!(
        schemas["MiniChatMessageDto"]["required"],
        json!([
            "id",
            "request_id",
            "role",
            "content",
            "attachments",
            "my_reaction",
            "created_at"
        ])
    );
    assert_eq!(
        schemas["ImgThumbnailDto"]["required"],
        json!(["content_type", "width", "height", "data_base64"])
    );
    assert_eq!(
        schemas["TurnStatusState"]["enum"],
        json!(["running", "done", "error", "cancelled"])
    );
    assert_eq!(
        schemas["ReactionKindDto"]["enum"],
        json!(["like", "dislike"])
    );
    assert!(schemas["Page_ChatDetailDto"].is_object());
    assert!(schemas["TurnStatusResponse"].is_object());
    assert!(schemas.get("MessageDto").is_none());
}

// ── Streaming and quota status ───────────────────────────────────────────

/// `(event, data)` pairs of an SSE body.
fn sse_events(raw: &[u8]) -> Vec<(String, Value)> {
    let text = std::str::from_utf8(raw).unwrap();
    text.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut name = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    value.clone_into(&mut name);
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push_str(value);
                }
            }
            (name, serde_json::from_str(&data).unwrap())
        })
        .collect()
}

fn stream_uri(chat_id: Uuid) -> String {
    format!("{CHATS}/{chat_id}/messages:stream")
}

#[tokio::test]
async fn stream_message_relays_the_turn_as_sse() {
    let fixture = router().await;
    let chat_id = seed_chat(&fixture.store, TENANT_A, USER_A1, None).await;
    let request_id = Uuid::new_v4();

    let reply = fixture
        .send_json(
            "POST",
            &stream_uri(chat_id),
            &json!({ "content": "Hello", "request_id": request_id }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type.as_deref(), Some("text/event-stream"));
    let events = sse_events(&reply.raw);
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["stream_started", "delta", "done"]);
    let started = &events[0].1;
    assert_eq!(started["request_id"], json!(request_id));
    assert_eq!(started["is_new_turn"], json!(true));
    assert!(started.get("thread_summary_applied").is_none(), "{started}");
    assert_eq!(events[1].1, json!({ "type": "text", "content": "Fine." }));
    let done = &events[2].1;
    assert_eq!(
        done["usage"],
        json!({ "input_tokens": 10, "output_tokens": 5 })
    );
    assert_eq!(done["effective_model"], json!("standard"));
    assert_eq!(done["selected_model"], json!("standard"));
    assert_eq!(done["quota_decision"], json!("allow"));
    assert!(done.get("downgrade_from").is_none(), "{done}");
    assert!(done.get("downgrade_reason").is_none(), "{done}");

    // Same request id again: idempotent replay of the stored answer.
    let second = fixture
        .send_json(
            "POST",
            &stream_uri(chat_id),
            &json!({ "content": "Hello", "request_id": request_id }),
        )
        .await;
    assert_eq!(second.status, StatusCode::OK);
    let events = sse_events(&second.raw);
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["stream_started", "delta", "done"]);
    assert_eq!(events[0].1["is_new_turn"], json!(false));
    assert_eq!(events[0].1["message_id"], started["message_id"]);
    assert_eq!(events[1].1["content"], json!("Fine."));
    assert!(events[2].1.get("quota_warnings").is_none());
}

#[tokio::test]
async fn stream_message_rejections_are_json_problems() {
    let fixture = router().await;
    let chat_id = seed_chat(&fixture.store, TENANT_A, USER_A1, None).await;

    let empty = fixture
        .send_json("POST", &stream_uri(chat_id), &json!({ "content": "  " }))
        .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.body);
    assert!(empty.content_type.unwrap().contains("json"));

    let unknown = fixture
        .send_json(
            "POST",
            &stream_uri(Uuid::new_v4()),
            &json!({ "content": "hi" }),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);
    assert_eq!(
        unknown.body["context"]["resource_type"],
        json!(CHAT_RESOURCE_TYPE)
    );

    let missing = fixture
        .send_json("POST", &stream_uri(chat_id), &json!({}))
        .await;
    assert_eq!(
        missing.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        missing.body
    );
}

#[tokio::test]
async fn quota_status_reports_both_tiers_and_periods() {
    let fixture = router().await;

    let reply = fixture.get("/mini-chat/v1/quota/status").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = &reply.body;
    assert_eq!(body["warning_threshold_pct"], json!(80));
    let tiers: Vec<&Value> = body["tiers"].as_array().unwrap().iter().collect();
    let names: Vec<&Value> = tiers.iter().map(|tier| &tier["tier"]).collect();
    assert_eq!(names, [&json!("premium"), &json!("total")]);
    for tier in tiers {
        let periods = tier["periods"].as_array().unwrap();
        let names: Vec<&Value> = periods.iter().map(|period| &period["period"]).collect();
        assert_eq!(names, [&json!("daily"), &json!("monthly")]);
        for period in periods {
            assert_eq!(period["used_credits_micro"], json!(0));
            assert_eq!(period["remaining_percentage"], json!(100));
            assert_eq!(period["warning"], json!(false));
            assert_eq!(period["exhausted"], json!(false));
            assert!(period["next_reset"].as_str().unwrap().ends_with('Z'));
            assert_eq!(
                period["remaining_credits_micro"],
                period["limit_credits_micro"]
            );
        }
    }
}

#[tokio::test]
async fn openapi_declares_the_stream_and_quota_operations() {
    let doc = openapi_doc().await;
    let stream = &doc["paths"]["/mini-chat/v1/chats/{id}/messages:stream"]["post"];
    assert_eq!(stream["operationId"], json!("mini_chat.stream_message"));
    assert_eq!(
        stream["responses"]["200"]["content"]["text/event-stream"]["schema"]["$ref"],
        json!("#/components/schemas/MiniChatSseEvent")
    );
    for status in [
        "400", "401", "403", "404", "409", "422", "429", "500", "503",
    ] {
        assert!(stream["responses"][status].is_object(), "missing {status}");
    }
    let quota = &doc["paths"]["/mini-chat/v1/quota/status"]["get"];
    assert_eq!(quota["operationId"], json!("mini_chat.get_quota_status"));
    assert_eq!(
        quota["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        json!("#/components/schemas/QuotaStatusResponse")
    );
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["MiniChatSseEvent"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    assert_eq!(
        schemas["StreamStartedData"]["required"],
        json!(["request_id", "message_id", "is_new_turn"])
    );
    assert_eq!(schemas["DeltaData"]["required"], json!(["type", "content"]));
    assert_eq!(
        schemas["QuotaStatusResponse"]["required"],
        json!(["tiers", "warning_threshold_pct"])
    );
}

// ── Turn mutations ───────────────────────────────────────────────────────

fn turn_uri(chat_id: Uuid, request_id: &Value) -> String {
    format!("{CHATS}/{chat_id}/turns/{}", request_id.as_str().unwrap())
}

fn names_of(events: &[(String, Value)]) -> Vec<&str> {
    events.iter().map(|(name, _)| name.as_str()).collect()
}

#[tokio::test]
async fn turn_mutation_routes_stream_retry_and_edit_and_delete_with_204() {
    let fixture = router().await;
    let chat_id = seed_chat(&fixture.store, TENANT_A, USER_A1, None).await;
    let first = json!(Uuid::new_v4());
    let sent = fixture
        .send_json(
            "POST",
            &stream_uri(chat_id),
            &json!({ "content": "Hello", "request_id": first }),
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK);

    let retry = fixture
        .call(request(
            "POST",
            &format!("{}/retry", turn_uri(chat_id, &first)),
            a1(),
            None,
        ))
        .await;
    assert_eq!(retry.status, StatusCode::OK, "{:?}", retry.raw);
    assert_eq!(retry.content_type.as_deref(), Some("text/event-stream"));
    let events = sse_events(&retry.raw);
    assert_eq!(names_of(&events), ["stream_started", "delta", "done"]);
    let retried = events[0].1["request_id"].clone();
    assert_ne!(retried, first);
    assert_eq!(events[0].1["is_new_turn"], json!(true));

    let invalid = fixture
        .send_json(
            "PATCH",
            &turn_uri(chat_id, &retried),
            &json!({ "text": "x" }),
        )
        .await;
    assert_eq!(
        invalid.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        invalid.body
    );
    let blank = fixture
        .send_json(
            "PATCH",
            &turn_uri(chat_id, &retried),
            &json!({ "content": " " }),
        )
        .await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST, "{}", blank.body);
    assert_eq!(field_violation_reason(&blank.body), Some("EMPTY_CONTENT"));

    let edit = fixture
        .send_json(
            "PATCH",
            &turn_uri(chat_id, &retried),
            &json!({ "content": "Hello again" }),
        )
        .await;
    assert_eq!(edit.status, StatusCode::OK, "{:?}", edit.raw);
    let events = sse_events(&edit.raw);
    assert_eq!(names_of(&events), ["stream_started", "delta", "done"]);
    let edited = events[0].1["request_id"].clone();

    let stale = fixture
        .call(request(
            "POST",
            &format!("{}/retry", turn_uri(chat_id, &retried)),
            a1(),
            None,
        ))
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["context"]["reason"], json!("NOT_LATEST_TURN"));
    assert!(stale.content_type.unwrap().contains("json"));

    let deleted = fixture
        .call(request("DELETE", &turn_uri(chat_id, &edited), a1(), None))
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);
    assert!(deleted.raw.is_empty());
    let status = fixture.get(&turn_uri(chat_id, &edited)).await;
    assert_eq!(status.status, StatusCode::NOT_FOUND, "{}", status.body);
}

#[tokio::test]
async fn turn_mutation_routes_reject_non_uuid_paths_and_unknown_turns() {
    let fixture = router().await;
    let chat_id = seed_chat(&fixture.store, TENANT_A, USER_A1, None).await;

    let bad = fixture
        .call(request(
            "DELETE",
            &format!("{CHATS}/{chat_id}/turns/not-a-uuid"),
            a1(),
            None,
        ))
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    let unknown = fixture
        .call(request(
            "POST",
            &format!("{}/retry", turn_uri(chat_id, &json!(Uuid::new_v4()))),
            a1(),
            None,
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);
    assert_eq!(
        unknown.body["context"]["resource_type"],
        json!("gts.cf.core.mini_chat.turn.v1~")
    );
}

#[tokio::test]
async fn openapi_declares_the_turn_mutation_operations() {
    let doc = openapi_doc().await;
    let paths = &doc["paths"];
    let turn = "/mini-chat/v1/chats/{id}/turns/{request_id}";
    let expected = [
        (
            format!("{turn}/retry"),
            "post",
            "mini_chat.retry_turn",
            vec![
                "200", "400", "401", "403", "404", "409", "429", "500", "503",
            ],
        ),
        (
            turn.to_owned(),
            "patch",
            "mini_chat.edit_turn",
            vec![
                "200", "400", "401", "403", "404", "409", "422", "429", "500", "503",
            ],
        ),
        (
            turn.to_owned(),
            "delete",
            "mini_chat.delete_turn",
            vec!["204", "400", "401", "403", "404", "409", "500", "503"],
        ),
    ];
    for (path, method, operation_id, statuses) in expected {
        let operation = &paths[path.as_str()][method];
        assert_eq!(operation["operationId"], json!(operation_id), "{path}");
        let mut declared: Vec<&String> =
            operation["responses"].as_object().unwrap().keys().collect();
        declared.sort();
        assert_eq!(declared, statuses, "{operation_id}");
        assert!(operation["responses"]["503"]["headers"]["Retry-After"].is_object());
    }
    for method in ["post", "patch"] {
        let path = if method == "post" {
            format!("{turn}/retry")
        } else {
            turn.to_owned()
        };
        assert_eq!(
            paths[path.as_str()][method]["responses"]["200"]["content"]["text/event-stream"]["schema"]
                ["$ref"],
            json!("#/components/schemas/MiniChatSseEvent")
        );
    }
    assert_eq!(
        paths[turn]["patch"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        json!("#/components/schemas/EditTurnRequest")
    );
    assert_eq!(
        doc["components"]["schemas"]["EditTurnRequest"]["required"],
        json!(["content"])
    );
}

// ── Attachments ──────────────────────────────────────────────────────────

const BOUNDARY: &str = "mc-router-test";

/// One multipart part: name, filename, content type, data; `None` leaves the
/// header parameter or line out.
type Part<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

/// `multipart/form-data` body of `parts`.
fn multipart(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, filename, content_type, data) in parts {
        let disposition = match filename {
            Some(filename) => format!("form-data; name=\"{name}\"; filename=\"{filename}\""),
            None => format!("form-data; name=\"{name}\""),
        };
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: {disposition}\r\n").as_bytes(),
        );
        if let Some(content_type) = content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn multipart_type() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

fn attachments_uri(chat_id: &str) -> String {
    format!("{CHATS}/{chat_id}/attachments")
}

impl Fixture {
    async fn upload_raw(&self, chat_id: &str, content_type: &str, body: Vec<u8>) -> Reply {
        self.call(request(
            "POST",
            &attachments_uri(chat_id),
            a1(),
            Some((content_type, body)),
        ))
        .await
    }

    async fn new_chat(&self) -> String {
        let chat = self
            .create(&json!({ "title": "files", "model": "standard" }))
            .await;
        chat["id"].as_str().unwrap().to_owned()
    }
}

fn assert_field_violation(reply: &Reply, status: StatusCode, field: &str, reason: &str) {
    assert_eq!(reply.status, status, "{}", reply.body);
    let violations = reply.body["context"]["field_violations"]
        .as_array()
        .unwrap();
    assert!(
        violations
            .iter()
            .any(|v| v["field"] == json!(field) && v["reason"] == json!(reason)),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn upload_without_a_multipart_boundary_is_boundary_required() {
    let fx = Fixture::new().await;
    let chat_id = fx.new_chat().await;
    let file = multipart(&[("file", Some("a.txt"), Some("text/plain"), b"hello")]);
    for content_type in [
        "",
        "multipart/form-data",
        "application/json",
        "application/octet-stream",
    ] {
        let reply = fx.upload_raw(&chat_id, content_type, file.clone()).await;
        assert_field_violation(
            &reply,
            StatusCode::BAD_REQUEST,
            "content_type",
            "BOUNDARY_REQUIRED",
        );
    }
}

#[tokio::test]
async fn unreadable_multipart_body_is_multipart_error() {
    let fx = Fixture::new().await;
    let chat_id = fx.new_chat().await;
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition form-data name file\r\n\r\nx\r\n--{BOUNDARY}--\r\n"
    );
    let reply = fx
        .upload_raw(&chat_id, &multipart_type(), body.into_bytes())
        .await;
    assert_field_violation(
        &reply,
        StatusCode::BAD_REQUEST,
        "multipart",
        "MULTIPART_ERROR",
    );

    // A body cut inside the file part is unreadable too.
    let mut truncated = multipart(&[("file", Some("a.txt"), Some("text/plain"), b"hello")]);
    truncated.truncate(truncated.len() - BOUNDARY.len() - 8);
    let reply = fx.upload_raw(&chat_id, &multipart_type(), truncated).await;
    assert_field_violation(
        &reply,
        StatusCode::BAD_REQUEST,
        "multipart",
        "MULTIPART_ERROR",
    );
}

#[tokio::test]
async fn upload_without_a_file_part_is_missing_file() {
    let fx = Fixture::new().await;
    let chat_id = fx.new_chat().await;
    for body in [
        multipart(&[
            ("document", Some("a.txt"), Some("text/plain"), b"hello"),
            ("note", None, None, b"text"),
        ]),
        multipart(&[]),
    ] {
        let reply = fx.upload_raw(&chat_id, &multipart_type(), body).await;
        assert_field_violation(&reply, StatusCode::BAD_REQUEST, "file", "MISSING_FILE");
    }
}

#[tokio::test]
async fn file_part_without_a_content_type_is_missing_content_type() {
    let fx = Fixture::new().await;
    let chat_id = fx.new_chat().await;
    let body = multipart(&[("file", Some("a.txt"), None, b"hello")]);
    let reply = fx.upload_raw(&chat_id, &multipart_type(), body).await;
    assert_field_violation(
        &reply,
        StatusCode::BAD_REQUEST,
        "content_type",
        "MISSING_CONTENT_TYPE",
    );
    assert_eq!(
        reply.body["context"]["resource_type"],
        json!(ATTACHMENT_RESOURCE_TYPE)
    );
}

#[tokio::test]
async fn file_above_the_limit_is_file_too_large() {
    let fx = Fixture::new().await;
    let chat_id = fx.new_chat().await;
    let data = vec![b'a'; usize::try_from(UPLOAD_DOCUMENT_MAX_BYTES).unwrap() + 1];
    let body = multipart(&[("file", Some("big.txt"), Some("text/plain"), &data)]);
    let reply = fx.upload_raw(&chat_id, &multipart_type(), body).await;
    assert_field_violation(
        &reply,
        StatusCode::BAD_REQUEST,
        "content_length",
        "FILE_TOO_LARGE",
    );
}

#[tokio::test]
async fn unsupported_file_type_is_rejected_from_the_part_headers() {
    let fx = Fixture::new().await;
    let chat_id = fx.new_chat().await;
    let body = multipart(&[("file", Some("a.bin"), Some("application/zip"), b"PK")]);
    let reply = fx.upload_raw(&chat_id, &multipart_type(), body).await;
    assert_field_violation(
        &reply,
        StatusCode::BAD_REQUEST,
        "content_type",
        "UNSUPPORTED_CONTENT_TYPE",
    );
}

#[tokio::test]
async fn chat_is_resolved_before_the_body_is_read() {
    let fx = Fixture::new().await;
    // An unreadable body: only the chat lookup can produce the 404.
    let reply = fx
        .upload_raw(
            &Uuid::new_v4().to_string(),
            &multipart_type(),
            b"garbage".to_vec(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert_eq!(
        reply.body["context"]["resource_type"],
        json!(CHAT_RESOURCE_TYPE)
    );

    let reply = fx
        .upload_raw("not-a-uuid", &multipart_type(), multipart(&[]))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
}

#[tokio::test]
async fn upload_get_and_delete_round_trip() {
    let fx = Fixture::new().await;
    let chat_id = fx.new_chat().await;
    let body = multipart(&[
        ("note", None, None, b"ignored"),
        (
            "file",
            Some("notes.txt"),
            Some("text/plain; charset=utf-8"),
            b"hello",
        ),
    ]);
    let reply = fx.upload_raw(&chat_id, &multipart_type(), body).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    let created = reply.body;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["filename"], json!("notes.txt"));
    assert_eq!(created["content_type"], json!("text/plain"));
    assert_eq!(created["size_bytes"], json!(5));
    assert_eq!(created["status"], json!("ready"));
    assert_eq!(created["kind"], json!("document"));
    let keys: Vec<&String> = created.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec![
            "content_type",
            "created_at",
            "filename",
            "id",
            "kind",
            "size_bytes",
            "status"
        ],
        "absent optionals are omitted, no provider ids: {created}"
    );

    let uri = format!("{}/{id}", attachments_uri(&chat_id));
    let got = fx.get(&uri).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(got.body, created);

    let deleted = fx.call(request("DELETE", &uri, a1(), None)).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let again = fx.call(request("DELETE", &uri, a1(), None)).await;
    assert_eq!(again.status, StatusCode::NO_CONTENT);

    let gone = fx.get(&uri).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.body);
    assert_eq!(
        gone.body["context"]["resource_type"],
        json!(ATTACHMENT_RESOURCE_TYPE)
    );
}

#[tokio::test]
async fn openapi_declares_the_attachment_operations() {
    let doc = openapi_doc().await;
    let paths = &doc["paths"];
    let collection = "/mini-chat/v1/chats/{id}/attachments";
    let item = "/mini-chat/v1/chats/{id}/attachments/{attachment_id}";
    let expected = [
        (
            collection,
            "post",
            "mini_chat.upload_attachment",
            vec![
                "201", "400", "401", "403", "404", "409", "429", "500", "503",
            ],
        ),
        (
            item,
            "get",
            "mini_chat.get_attachment",
            vec!["200", "400", "401", "403", "404", "500", "503"],
        ),
        (
            item,
            "delete",
            "mini_chat.delete_attachment",
            vec!["204", "400", "401", "403", "404", "409", "500", "503"],
        ),
    ];
    for (path, method, operation_id, statuses) in expected {
        let operation = &paths[path][method];
        assert_eq!(operation["operationId"], json!(operation_id), "{path}");
        let mut declared: Vec<&String> =
            operation["responses"].as_object().unwrap().keys().collect();
        declared.sort();
        assert_eq!(declared, statuses, "{operation_id}");
        assert!(operation["responses"]["503"]["headers"]["Retry-After"].is_object());
    }
    let upload = &paths[collection]["post"];
    assert_eq!(
        upload["requestBody"]["content"]["multipart/form-data"]["schema"]["required"],
        json!(["file"])
    );
    assert_eq!(
        upload["responses"]["201"]["content"]["application/json"]["schema"]["$ref"],
        json!("#/components/schemas/AttachmentDetailDto")
    );
    assert_eq!(
        doc["components"]["schemas"]["AttachmentDetailDto"]["required"],
        json!([
            "id",
            "filename",
            "content_type",
            "size_bytes",
            "status",
            "kind",
            "created_at"
        ])
    );
}

#[tokio::test]
async fn upload_route_leaves_content_type_validation_to_the_handler() {
    let store = TestStore::without_outbox().await;
    let openapi = OpenApiRegistryImpl::new();
    let state = state_with(&store, FakePolicy::new(Vec::new()), allow_all_authz());
    let _router = register_routes(Router::new(), &openapi, state, PREFIX);
    let upload = openapi
        .operation_specs
        .get("POST:/mini-chat/v1/chats/{id}/attachments")
        .unwrap();
    assert!(upload.allowed_request_content_types.is_none());
    assert_eq!(
        upload.request_body.as_ref().unwrap().content_type,
        "multipart/form-data"
    );
}
