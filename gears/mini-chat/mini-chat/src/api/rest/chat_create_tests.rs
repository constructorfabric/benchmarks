//! `POST /v1/chats` over the real router, SQLite and the mock PDP.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: flow
//! `cpt-cf-mini-chat-flow-chat-management-create-chat`; algorithms `title-validation`,
//! `model-resolution`, `chat-detail-projection`, `activity-ordering` (create); definition of done
//! `cpt-cf-mini-chat-dod-chat-management-create`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::StatusCode;
use mini_chat_sdk::{ModelTier, PolicySnapshot};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::test_support::chats::{
    CHAT_RESOURCE, CHATS, ChatRig, RigOptions, a1, assert_invalid_argument, assert_problem,
    chat_row, chat_uri, detail_keys, keys, parse_ts, request_with_body, rig, rig_with, violation,
};
use crate::test_support::{FakeGateway, Pdp, TENANT_A, USER_A1, entry, raw_i64, snapshot};

async fn rig_with_catalog(pdp: Pdp, catalog: PolicySnapshot) -> ChatRig {
    let mut options = RigOptions::new(pdp);
    options.gateway = FakeGateway::serving(catalog);
    rig_with(options).await
}

async fn chat_rows(rig: &ChatRig) -> i64 {
    raw_i64(&rig.db.raw, "SELECT COUNT(*) FROM chats").await
}

fn chars(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

// ── success ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_returns_201_with_the_chat_detail_and_the_location_header() {
    let rig = rig(Pdp::TenantOnly).await;
    let before = OffsetDateTime::now_utc();
    let reply = rig
        .post_json(
            CHATS,
            &a1(),
            r#"{"title":"Q3 plan","model":"gpt-standard"}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("application/json"),
        "{}",
        reply.content_type
    );
    let body = reply.json();
    assert_eq!(keys(&body), detail_keys(true));
    let id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(body["title"], "Q3 plan");
    assert_eq!(body["model"], "gpt-standard");
    assert_eq!(body["is_temporary"], false);
    assert_eq!(body["message_count"], 0);
    assert_eq!(
        reply.location.as_deref(),
        Some(format!("/mini-chat/v1/chats/{id}").as_str()),
        "the request path as the gear sees it plus the new id"
    );

    // created_at and updated_at are the creation time
    let (created, updated) = (parse_ts(&body["created_at"]), parse_ts(&body["updated_at"]));
    assert_eq!(created, updated);
    assert!(created >= before - time::Duration::seconds(1));
    assert!(created <= OffsetDateTime::now_utc() + time::Duration::seconds(1));
}

#[tokio::test]
async fn the_new_chat_is_stored_for_the_caller_with_the_resolved_model() {
    let rig = rig(Pdp::TenantOnly).await;
    let reply = rig
        .post_json(CHATS, &a1(), r#"{"title":"mine","model":"gpt-premium"}"#)
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let id = Uuid::parse_str(reply.json()["id"].as_str().unwrap()).unwrap();
    let row = chat_row(&rig.db, id).await.expect("stored");
    assert_eq!(row.tenant_id, TENANT_A);
    assert_eq!(row.user_id, USER_A1);
    assert_eq!(row.model, "gpt-premium");
    assert_eq!(row.title.as_deref(), Some("mine"));
    assert!(!row.is_temporary);
    assert_eq!(row.deleted_at, None);
    assert_eq!(row.created_at, row.updated_at);
}

#[tokio::test]
async fn a_body_without_title_and_model_creates_an_untitled_chat_on_the_default_model() {
    for body in [
        r"{}",
        r#"{"title":null}"#,
        r#"{"model":null}"#,
        r#"{"title":null,"model":null}"#,
    ] {
        let rig = rig(Pdp::TenantOnly).await;
        let reply = rig.post_json(CHATS, &a1(), body).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{body}: {}", reply.text);
        let json = reply.json();
        assert_eq!(
            keys(&json),
            detail_keys(false),
            "{body}: title is omitted, never null"
        );
        assert_eq!(
            json["model"], "gpt-premium",
            "{body}: the flagged default, not the first"
        );
        let id = Uuid::parse_str(json["id"].as_str().unwrap()).unwrap();
        assert_eq!(chat_row(&rig.db, id).await.unwrap().title, None, "{body}");
    }
}

#[tokio::test]
async fn the_title_is_trimmed_and_accepted_from_1_to_255_characters() {
    let at_limit = chars('t', 255);
    let cases = vec![
        ("  padded  ".to_owned(), "padded".to_owned()),
        ("x".to_owned(), "x".to_owned()),
        (at_limit.clone(), at_limit.clone()),
        (format!("  {at_limit}\t"), at_limit),
        (chars('\u{00E9}', 255), chars('\u{00E9}', 255)),
    ];
    let rig = rig(Pdp::TenantOnly).await;
    for (sent, stored) in cases {
        let reply = rig
            .post_json(CHATS, &a1(), &json!({ "title": sent }).to_string())
            .await;
        assert_eq!(
            reply.status,
            StatusCode::CREATED,
            "{sent:?}: {}",
            reply.text
        );
        assert_eq!(reply.json()["title"], stored.as_str());
        let id = Uuid::parse_str(reply.json()["id"].as_str().unwrap()).unwrap();
        assert_eq!(chat_row(&rig.db, id).await.unwrap().title, Some(stored));
    }
}

#[tokio::test]
async fn each_create_makes_a_distinct_chat() {
    let rig = rig(Pdp::TenantOnly).await;
    let first = rig.post_json(CHATS, &a1(), "{}").await.json();
    let second = rig.post_json(CHATS, &a1(), "{}").await.json();
    assert_ne!(first["id"], second["id"]);
    assert_eq!(chat_rows(&rig).await, 2);
}

#[tokio::test]
async fn the_created_chat_can_be_read_back_with_the_same_detail() {
    let rig = rig(Pdp::TenantOnly).await;
    let created = rig.post_json(CHATS, &a1(), r#"{"title":"again"}"#).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let created = created.json();
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let read = rig.get(&chat_uri(id), &a1()).await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.text);
    assert_eq!(read.json(), created);
}

// ── identity comes from the security context ─────────────────────────────────

#[tokio::test]
async fn owner_and_tenant_come_from_the_security_context_never_from_the_body() {
    let rig = rig(Pdp::TenantOnly).await;
    let other_user = Uuid::new_v4();
    let other_tenant = Uuid::new_v4();
    let body = json!({
        "title": "sneaky",
        "user_id": other_user,
        "tenant_id": other_tenant,
        "deleted_at": "2020-01-01T00:00:00Z",
        "is_temporary": true
    })
    .to_string();
    let reply = rig.post_json(CHATS, &a1(), &body).await;
    match reply.status {
        StatusCode::CREATED => {
            let id = Uuid::parse_str(reply.json()["id"].as_str().unwrap()).unwrap();
            let row = chat_row(&rig.db, id).await.unwrap();
            assert_eq!((row.tenant_id, row.user_id), (TENANT_A, USER_A1));
            assert_eq!(row.deleted_at, None);
            assert!(!row.is_temporary, "is_temporary is always false in P1");
            assert_eq!(reply.json()["is_temporary"], false);
        }
        StatusCode::UNPROCESSABLE_ENTITY => {
            assert_invalid_argument(&reply, 422, "body", "invalid_json_body");
            assert_eq!(chat_rows(&rig).await, 0);
        }
        other => panic!("unexpected status {other}: {}", reply.text),
    }
    assert_eq!(
        raw_i64(
            &rig.db.raw,
            &format!(
                "SELECT COUNT(*) FROM chats WHERE user_id = {} OR tenant_id = {}",
                crate::test_support::chats::blob(other_user),
                crate::test_support::chats::blob(other_tenant)
            )
        )
        .await,
        0
    );
}

// ── title validation ─────────────────────────────────────────────────────────

#[tokio::test]
async fn an_invalid_title_is_a_400_invalid_title_and_creates_nothing() {
    let rig = rig(Pdp::TenantOnly).await;
    for title in [
        String::new(),
        "   ".to_owned(),
        "\t\r\n".to_owned(),
        chars('a', 256),
        format!("  {}  ", chars('a', 256)),
        chars('\u{1F600}', 256),
    ] {
        let reply = rig
            .post_json(CHATS, &a1(), &json!({ "title": title }).to_string())
            .await;
        let body = assert_invalid_argument(&reply, 400, "title", "INVALID_TITLE");
        assert_eq!(body["context"]["resource_type"], CHAT_RESOURCE);
    }
    assert_eq!(chat_rows(&rig).await, 0);
}

#[tokio::test]
async fn the_title_is_validated_before_the_authorization_check_and_the_model_lookup() {
    // a PDP that would deny, and a model that would be rejected: the title error wins and
    // neither the PDP nor the catalog is consulted
    let rig = rig(Pdp::Deny).await;
    let reply = rig
        .post_json(CHATS, &a1(), r#"{"title":"   ","model":"never-existed"}"#)
        .await;
    assert_invalid_argument(&reply, 400, "title", "INVALID_TITLE");
    assert!(rig.pdp.requests().is_empty(), "no PDP call");
    assert_eq!(rig.gateway.call_count(), 0, "no catalog read");
}

// ── model resolution ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_disabled_unknown_or_empty_model_is_a_400_invalid_model_and_creates_nothing() {
    let rig = rig(Pdp::TenantOnly).await;
    for model in ["gpt-hidden", "never-existed", ""] {
        let reply = rig
            .post_json(CHATS, &a1(), &json!({ "model": model }).to_string())
            .await;
        let body = assert_invalid_argument(&reply, 400, "model", "INVALID_MODEL");
        assert_eq!(body["context"]["resource_type"], CHAT_RESOURCE, "{model:?}");
    }
    assert_eq!(chat_rows(&rig).await, 0);
}

#[tokio::test]
async fn an_absent_model_resolves_to_the_first_enabled_entry_when_no_entry_is_flagged() {
    let catalog = snapshot(vec![
        entry("off", ModelTier::Premium, false, &json!({})),
        entry("standard-first", ModelTier::Standard, true, &json!({})),
        entry("premium-second", ModelTier::Premium, true, &json!({})),
    ]);
    let rig = rig_with_catalog(Pdp::TenantOnly, catalog).await;
    let reply = rig.post_json(CHATS, &a1(), "{}").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(
        reply.json()["model"],
        "standard-first",
        "the tier is not considered"
    );
}

#[tokio::test]
async fn without_an_enabled_model_creation_fails_with_invalid_model() {
    let catalog = snapshot(vec![entry("off", ModelTier::Standard, false, &json!({}))]);
    let rig = rig_with_catalog(Pdp::TenantOnly, catalog).await;
    for body in ["{}", r#"{"model":"off"}"#, r#"{"model":"other"}"#] {
        let reply = rig.post_json(CHATS, &a1(), body).await;
        assert_invalid_argument(&reply, 400, "model", "INVALID_MODEL");
    }
    let empty = rig_with_catalog(Pdp::TenantOnly, snapshot(vec![])).await;
    let reply = empty.post_json(CHATS, &a1(), "{}").await;
    assert_invalid_argument(&reply, 400, "model", "INVALID_MODEL");
}

#[tokio::test]
async fn the_catalog_is_read_for_every_create_and_never_cached() {
    let rig = rig(Pdp::TenantOnly).await;
    for expected in 1..=3 {
        let reply = rig.post_json(CHATS, &a1(), "{}").await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
        assert_eq!(rig.gateway.call_count(), expected);
    }
}

#[tokio::test]
async fn a_policy_plugin_failure_is_a_generic_500_and_creates_nothing() {
    let mut options = RigOptions::new(Pdp::TenantOnly);
    options.gateway = FakeGateway::failing();
    let rig = rig_with(options).await;
    for body in ["{}", r#"{"model":"gpt-standard"}"#] {
        let reply = rig.post_json(CHATS, &a1(), body).await;
        let problem = assert_problem(&reply, 500, "internal");
        assert!(
            !reply.text.contains("provider-secret-detail"),
            "cause leaked: {problem}"
        );
    }
    assert_eq!(chat_rows(&rig).await, 0);
}

// ── admission ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_is_authorized_as_the_create_action_without_a_resource_id() {
    let rig = rig(Pdp::TenantOnly).await;
    let reply = rig.post_json(CHATS, &a1(), "{}").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "create");
    assert_eq!(request.resource.id, None);
    assert_eq!(
        request.resource.resource_type,
        "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"
    );
    assert!(request.context.require_constraints);
    assert_eq!(
        request.resource.properties["owner_tenant_id"],
        json!(TENANT_A.to_string())
    );
    assert_eq!(
        request.resource.properties["owner_id"],
        json!(USER_A1.to_string())
    );
}

#[tokio::test]
async fn a_pdp_deny_or_an_unusable_decision_is_a_403_and_the_catalog_is_not_read() {
    for behavior in [
        Pdp::Deny,
        Pdp::NoConstraints,
        Pdp::UnknownProperty,
        Pdp::UnadvertisedPredicate,
    ] {
        let rig = rig(behavior).await;
        let reply = rig
            .post_json(CHATS, &a1(), r#"{"model":"gpt-standard"}"#)
            .await;
        let body = assert_problem(&reply, 403, "permission_denied");
        assert_eq!(body["context"]["reason"], "AUTHZ_DENIED", "{behavior:?}");
        assert_eq!(
            rig.gateway.call_count(),
            0,
            "{behavior:?}: authorization precedes the lookup"
        );
        assert_eq!(chat_rows(&rig).await, 0, "{behavior:?}");
    }
}

#[tokio::test]
async fn a_pdp_failure_is_a_503_with_retry_after_5_and_creates_nothing() {
    let rig = rig(Pdp::Unreachable).await;
    let reply = rig.post_json(CHATS, &a1(), "{}").await;
    let body = assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
    assert_eq!(body["context"]["retry_after_seconds"], 5);
    assert!(
        !reply.text.contains("pdp unreachable"),
        "cause leaked: {}",
        reply.text
    );
    assert_eq!(chat_rows(&rig).await, 0);
}

// ── body errors ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_body_without_a_json_content_type_is_a_415() {
    let rig = rig(Pdp::TenantOnly).await;
    for content_type in [None, Some("text/plain")] {
        let reply = rig
            .send(request_with_body(
                "POST",
                CHATS,
                &a1(),
                content_type,
                r#"{"title":"x"}"#,
            ))
            .await;
        assert_invalid_argument(&reply, 415, "body", "missing_json_content_type");
    }
    assert!(rig.pdp.requests().is_empty());
    assert_eq!(chat_rows(&rig).await, 0);
}

#[tokio::test]
async fn malformed_json_is_a_400_json_syntax_error() {
    let rig = rig(Pdp::TenantOnly).await;
    for body in ["{", r#"{"title": }"#, "not json", r#"{"title":"x""#] {
        let reply = rig.post_json(CHATS, &a1(), body).await;
        assert_invalid_argument(&reply, 400, "body", "json_syntax_error");
    }
    assert!(rig.pdp.requests().is_empty());
    assert_eq!(chat_rows(&rig).await, 0);
}

#[tokio::test]
async fn a_body_that_does_not_match_the_schema_is_a_422_invalid_json_body() {
    let rig = rig(Pdp::TenantOnly).await;
    for body in [
        r#"{"title":5}"#,
        r#"{"title":true}"#,
        r#"{"title":["x"]}"#,
        r#"{"model":5}"#,
        r#"{"model":{"id":"gpt-standard"}}"#,
        "[]",
        r#""just a string""#,
    ] {
        let reply = rig.post_json(CHATS, &a1(), body).await;
        let problem = assert_invalid_argument(&reply, 422, "body", "invalid_json_body");
        assert_eq!(
            violation(&problem, "body")["reason"],
            "invalid_json_body",
            "{body}"
        );
    }
    assert!(rig.pdp.requests().is_empty());
    assert_eq!(chat_rows(&rig).await, 0);
}

#[tokio::test]
async fn the_problem_of_a_failed_create_has_the_instance_and_the_request_trace_id() {
    let rig = rig(Pdp::TenantOnly).await;
    let reply = rig.post_json(CHATS, &a1(), r#"{"title":""}"#).await;
    let problem: Value = assert_problem(&reply, 400, "invalid_argument");
    assert_eq!(problem["instance"], CHATS);
    assert_eq!(problem["trace_id"], "req-42");
}

// ── activity ordering and the lifecycle ──────────────────────────────────────

#[tokio::test]
async fn a_created_chat_is_the_most_recently_active_and_is_listed_first() {
    let rig = rig(Pdp::TenantOnly).await;
    let older = rig
        .seed_at(
            &a1(),
            Uuid::new_v4(),
            Some("older"),
            "gpt-standard",
            crate::test_support::chats::at(10),
        )
        .await;
    let created = rig.post_json(CHATS, &a1(), r#"{"title":"fresh"}"#).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let id = Uuid::parse_str(created.json()["id"].as_str().unwrap()).unwrap();
    let listed = crate::test_support::chats::item_ids(&rig.get(CHATS, &a1()).await.json());
    assert_eq!(listed, vec![id, older.id]);
}

#[tokio::test]
async fn the_chat_lifecycle_runs_from_create_to_a_terminal_deleted_state() {
    let rig = rig(Pdp::TenantOnly).await;
    let me = a1();

    // Active: created
    let created = rig
        .post_json(CHATS, &me, r#"{"title":"  Plan  ","model":"gpt-standard"}"#)
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let created = created.json();
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    assert_eq!(created["title"], "Plan");

    // Active: read and listed
    let read = rig.get(&chat_uri(id), &me).await;
    assert_eq!(read.json(), created);
    let listed = crate::test_support::chats::item_ids(&rig.get(CHATS, &me).await.json());
    assert_eq!(listed, vec![id]);

    // Active to Active: renamed
    let renamed = rig
        .patch_json(
            &chat_uri(id),
            &me,
            r#"{"title":"Plan v2","model":"gpt-premium"}"#,
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
    let renamed = renamed.json();
    assert_eq!(renamed["title"], "Plan v2");
    assert_eq!(
        renamed["model"], "gpt-standard",
        "the model is locked at creation"
    );
    assert!(parse_ts(&renamed["updated_at"]) > parse_ts(&created["updated_at"]));

    // Active to Deleted
    assert_eq!(
        rig.delete(&chat_uri(id), &me).await.status,
        StatusCode::NO_CONTENT
    );

    // Deleted is terminal: every operation answers 404 and changes nothing
    for reply in [
        rig.get(&chat_uri(id), &me).await,
        rig.delete(&chat_uri(id), &me).await,
        rig.patch_json(&chat_uri(id), &me, r#"{"title":"again"}"#)
            .await,
    ] {
        crate::test_support::chats::assert_chat_not_found(&reply);
    }
    assert!(crate::test_support::chats::item_ids(&rig.get(CHATS, &me).await.json()).is_empty());
    assert_eq!(
        rig.cleanup_rows().await.len(),
        1,
        "exactly one cleanup message"
    );
    assert_eq!(
        chat_row(&rig.db, id).await.unwrap().title.as_deref(),
        Some("Plan v2"),
        "the row is kept as it was"
    );
}
