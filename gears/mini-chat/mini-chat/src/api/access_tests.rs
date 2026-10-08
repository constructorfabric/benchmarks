//! Authorization and isolation across every REST operation (DESIGN 3.8): each operation asks
//! the PDP with its action and resource type and fails closed, and a foreign caller (another
//! user of the tenant, or another tenant) gets a 404 from every chat-scoped operation and
//! changes nothing.

use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::test_support::app::{TestApp, TestResponse, ctx};
use crate::test_support::attachments::{Part, multipart_body, png, post_upload, script_files};
use crate::test_support::pdp::PdpMode;
use crate::test_support::stream::{
    CHATS, SeedAttachment, answer, create_chat, messages_of, provider_calls, reactions_of,
    script_provider, seed_attachment, stream_uri, turns_of,
};

const MULTIPART: &str = "multipart/form-data; boundary=mini-chat-test-boundary";
const CHAT_RESOURCE: &str = "gts.cf.core.mini_chat.chat.v1~";

/// Ids an operation's path refers to.
#[derive(Clone, Copy)]
struct Ids {
    chat: Uuid,
    attachment: Uuid,
    request: Uuid,
    message: Uuid,
}

impl Ids {
    fn random() -> Self {
        Self {
            chat: Uuid::new_v4(),
            attachment: Uuid::new_v4(),
            request: Uuid::new_v4(),
            message: Uuid::new_v4(),
        }
    }
}

enum Body {
    None,
    Json(Value),
    Upload,
}

/// One REST operation: method, path, body, and the PDP action / resource type DESIGN 3.8 names
/// for it (`chat_scoped`: the evaluation carries the chat id as `resource.id`).
struct Op {
    method: &'static str,
    path: String,
    body: Body,
    action: &'static str,
    resource_type: &'static str,
    chat_scoped: bool,
}

#[allow(clippy::too_many_lines)] // the 19 operations of DESIGN 3.8, one entry each
fn operations(ids: Ids) -> Vec<Op> {
    let chat_type = mini_chat_sdk::CHAT_RESOURCE_TYPE;
    let Ids {
        chat,
        attachment,
        request,
        message,
    } = ids;
    let op = |method, path: String, body, action, chat_scoped| Op {
        method,
        path,
        body,
        action,
        resource_type: chat_type,
        chat_scoped,
    };
    vec![
        op(
            "POST",
            CHATS.to_owned(),
            Body::Json(json!({})),
            "create",
            false,
        ),
        op("GET", CHATS.to_owned(), Body::None, "list", false),
        op("GET", format!("{CHATS}/{chat}"), Body::None, "read", true),
        op(
            "PATCH",
            format!("{CHATS}/{chat}"),
            Body::Json(json!({"title": "hijack"})),
            "update",
            true,
        ),
        op(
            "DELETE",
            format!("{CHATS}/{chat}"),
            Body::None,
            "delete",
            true,
        ),
        op(
            "GET",
            format!("{CHATS}/{chat}/messages"),
            Body::None,
            "list_messages",
            true,
        ),
        op(
            "POST",
            stream_uri(chat),
            Body::Json(json!({"content": "hi"})),
            "send_message",
            true,
        ),
        op(
            "POST",
            format!("{CHATS}/{chat}/attachments"),
            Body::Upload,
            "upload_attachment",
            true,
        ),
        op(
            "GET",
            format!("{CHATS}/{chat}/attachments/{attachment}"),
            Body::None,
            "read_attachment",
            true,
        ),
        op(
            "DELETE",
            format!("{CHATS}/{chat}/attachments/{attachment}"),
            Body::None,
            "delete_attachment",
            true,
        ),
        op(
            "GET",
            format!("{CHATS}/{chat}/turns/{request}"),
            Body::None,
            "read_turn",
            true,
        ),
        op(
            "POST",
            format!("{CHATS}/{chat}/turns/{request}/retry"),
            Body::None,
            "retry_turn",
            true,
        ),
        op(
            "PATCH",
            format!("{CHATS}/{chat}/turns/{request}"),
            Body::Json(json!({"content": "edited"})),
            "edit_turn",
            true,
        ),
        op(
            "DELETE",
            format!("{CHATS}/{chat}/turns/{request}"),
            Body::None,
            "delete_turn",
            true,
        ),
        op(
            "PUT",
            format!("{CHATS}/{chat}/messages/{message}/reaction"),
            Body::Json(json!({"reaction": "like"})),
            "set_reaction",
            true,
        ),
        op(
            "DELETE",
            format!("{CHATS}/{chat}/messages/{message}/reaction"),
            Body::None,
            "delete_reaction",
            true,
        ),
        Op {
            method: "GET",
            path: "/mini-chat/v1/models".to_owned(),
            body: Body::None,
            action: "list",
            resource_type: mini_chat_sdk::MODEL_RESOURCE_TYPE,
            chat_scoped: false,
        },
        Op {
            method: "GET",
            path: "/mini-chat/v1/models/gpt-standard".to_owned(),
            body: Body::None,
            action: "read",
            resource_type: mini_chat_sdk::MODEL_RESOURCE_TYPE,
            chat_scoped: false,
        },
        Op {
            method: "GET",
            path: "/mini-chat/v1/quota/status".to_owned(),
            body: Body::None,
            action: "read",
            resource_type: mini_chat_sdk::USER_QUOTA_RESOURCE_TYPE,
            chat_scoped: false,
        },
    ]
}

async fn invoke(app: &TestApp, who: &SecurityContext, op: &Op, chat: Uuid) -> TestResponse {
    match &op.body {
        Body::None => app.call(op.method, &op.path, who, None).await,
        Body::Json(body) => app.call(op.method, &op.path, who, Some(body.clone())).await,
        Body::Upload => {
            let image = png(4, 4);
            let body = multipart_body(&[Part::file(Some("p.png"), Some("image/png"), &image)]);
            post_upload(app, who, chat, MULTIPART, body).await
        }
    }
}

#[tokio::test]
async fn every_operation_asks_the_pdp_and_fails_closed() {
    for (mode, status) in [(PdpMode::Deny, 403), (PdpMode::Fail, 503)] {
        let app = TestApp::builder().pdp(mode).build().await;
        let who = ctx(Uuid::new_v4(), Uuid::new_v4());
        let ids = Ids::random();
        let ops = operations(ids);
        assert_eq!(ops.len(), 19, "every operation of the published contract");

        for op in &ops {
            let before = app.pdp.requests().len();
            let res = invoke(&app, &who, op, ids.chat).await;
            let name = format!("{} {} ({mode:?})", op.method, op.path);

            assert_eq!(res.status, status, "{name}: {}", res.json);
            if status == 403 {
                assert_eq!(res.json["context"]["reason"], "AUTHZ_DENIED", "{name}");
            } else {
                assert_eq!(res.headers["retry-after"], "5", "{name}");
            }
            let asked = app.pdp.requests();
            let first = asked
                .get(before)
                .unwrap_or_else(|| panic!("{name}: no PDP call"));
            assert_eq!(first.action.name, op.action, "{name}");
            assert_eq!(first.resource.resource_type, op.resource_type, "{name}");
            let expected_id = op.chat_scoped.then_some(ids.chat);
            assert_eq!(first.resource.id, expected_id, "{name}");
        }
        assert!(
            provider_calls(&app).is_empty(),
            "nothing reached a provider"
        );
    }
}

/// A chat of `owner` with one completed turn and a ready document; the ids of its parts.
async fn owned_chat(app: &TestApp, owner: &SecurityContext) -> Ids {
    let (tenant, user) = (owner.subject_tenant_id(), owner.subject_id());
    let chat = create_chat(app, owner, None).await;
    let attachment = seed_attachment(app, SeedAttachment::document(tenant, chat, user)).await;
    let request = Uuid::new_v4();
    app.stream(
        "POST",
        &stream_uri(chat),
        owner,
        json!({"content": "secret", "request_id": request}),
    )
    .await
    .unwrap_or_else(|r| panic!("send rejected: {}", r.json));
    let message = messages_of(app, chat)
        .await
        .iter()
        .find(|m| m.role == "assistant")
        .expect("assistant message")
        .id;
    Ids {
        chat,
        attachment,
        request,
        message,
    }
}

/// With the PDP allowing, each operation evaluates only its own action on its own resource:
/// `messages:stream` never asks for `read`, retry and edit are evaluated exactly once.
#[tokio::test]
async fn allowed_operations_evaluate_only_their_own_action() {
    let app = TestApp::builder().quiet_cleanup().build().await;
    let owner = ctx(Uuid::new_v4(), Uuid::new_v4());
    script_provider(&app, answer(&["ok"], 3, 2));
    script_files(&app, &["file-img1"]);

    for n in 0..operations(Ids::random()).len() {
        let ids = owned_chat(&app, &owner).await;
        let op = &operations(ids)[n];
        let name = format!("{} {}", op.method, op.path);
        let before = app.pdp.requests().len();

        let res = invoke(&app, &owner, op, ids.chat).await;

        assert!(
            res.status.is_success(),
            "{name}: {} {}",
            res.status,
            res.json
        );
        let asked: Vec<(String, String, Option<Uuid>)> = app.pdp.requests()[before..]
            .iter()
            .map(|r| {
                (
                    r.action.name.clone(),
                    r.resource.resource_type.clone(),
                    r.resource.id,
                )
            })
            .collect();
        let expected = (
            op.action.to_owned(),
            op.resource_type.to_owned(),
            op.chat_scoped.then_some(ids.chat),
        );
        assert_eq!(asked, [expected], "{name}: exactly one evaluation, its own");
    }
}

#[tokio::test]
async fn foreign_callers_get_404_from_every_chat_operation_and_change_nothing() {
    let app = TestApp::builder().quiet_cleanup().build().await;
    let (tenant, owner_id) = (Uuid::new_v4(), Uuid::new_v4());
    let owner = ctx(tenant, owner_id);
    let chat = create_chat(&app, &owner, None).await;
    let attachment = seed_attachment(&app, SeedAttachment::document(tenant, chat, owner_id)).await;
    script_provider(&app, answer(&["mine"], 3, 2));
    let request = Uuid::new_v4();
    app.stream(
        "POST",
        &stream_uri(chat),
        &owner,
        json!({"content": "secret", "request_id": request}),
    )
    .await
    .unwrap_or_else(|r| panic!("send rejected: {}", r.json));
    let messages = messages_of(&app, chat).await;
    let message = messages
        .iter()
        .find(|m| m.role == "assistant")
        .expect("assistant message")
        .id;
    let ids = Ids {
        chat,
        attachment,
        request,
        message,
    };
    let provider_calls_before = provider_calls(&app).len();

    let same_tenant = ctx(tenant, Uuid::new_v4());
    let other_tenant = ctx(Uuid::new_v4(), Uuid::new_v4());
    for intruder in [&same_tenant, &other_tenant] {
        for op in operations(ids).iter().filter(|op| op.chat_scoped) {
            let res = invoke(&app, intruder, op, chat).await;
            let name = format!("{} {}", op.method, op.path);
            assert_eq!(res.status, 404, "{name}: {}", res.json);
            assert_eq!(
                res.json["context"]["resource_type"], CHAT_RESOURCE,
                "{name}: {}",
                res.json
            );
        }
        // The intruder's own lists do not show the owner's chat.
        let listed = app.call("GET", CHATS, intruder, None).await;
        assert_eq!(listed.json["items"], json!([]), "{}", listed.json);
    }

    // The owner's chat is untouched.
    let got = app
        .call("GET", &format!("{CHATS}/{chat}"), &owner, None)
        .await;
    assert_eq!(got.status, 200, "{}", got.json);
    assert!(got.json.get("title").is_none(), "{}", got.json);
    assert_eq!(got.json["message_count"], 2);
    let turns = turns_of(&app, chat).await;
    assert_eq!(turns.len(), 1);
    assert!(turns[0].deleted_at.is_none());
    assert_eq!(messages_of(&app, chat).await.len(), messages.len());
    assert!(reactions_of(&app, message).await.is_empty());
    let att = app
        .call(
            "GET",
            &format!("{CHATS}/{chat}/attachments/{attachment}"),
            &owner,
            None,
        )
        .await;
    assert_eq!(att.status, 200, "{}", att.json);
    assert_eq!(provider_calls(&app).len(), provider_calls_before);
}
