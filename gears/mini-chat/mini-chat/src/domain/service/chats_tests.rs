#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::Arc;

use time::OffsetDateTime;
use uuid::Uuid;

use super::test_rows::{
    attachment_rows, chat_row, create_chat, enc, env_with_pdp, insert_attachment, insert_message,
    message_am, odata, problem,
};
use super::{MAX_TITLE_CHARS, validate_title};
use crate::api::rest::dto::CreateChatReq;
use crate::domain::error::{DomainError, reasons, resource_types};
use crate::domain::service::test_support::{
    DenyPdp, FailingPdp, TENANT_A, TENANT_B, TestEnv, USER_A1, USER_A2, USER_B, ctx, ctx_a1,
};

fn req(title: Option<&str>, model: Option<&str>) -> CreateChatReq {
    CreateChatReq {
        model: model.map(str::to_owned),
        title: title.map(str::to_owned),
    }
}

fn assert_invalid(e: DomainError, field: &str, reason: &str) {
    let p = problem(e);
    assert_eq!(p["status"], 400, "{p}");
    assert_eq!(p["context"]["field_violations"][0]["field"], field, "{p}");
    assert_eq!(p["context"]["field_violations"][0]["reason"], reason, "{p}");
}

fn assert_not_found(e: DomainError, resource: &str) {
    let p = problem(e);
    assert_eq!(p["status"], 404, "{p}");
    assert_eq!(p["context"]["resource_type"], resource, "{p}");
}

// ── title validation ───────────────────────────────────────────────────────

#[test]
fn title_validation_rules() {
    assert_eq!(validate_title("  Hello  ").unwrap(), "Hello");
    assert_eq!(validate_title("a").unwrap(), "a");
    let max = "é".repeat(MAX_TITLE_CHARS);
    assert_eq!(
        validate_title(&max).unwrap(),
        max,
        "255 multi-byte chars are fine"
    );
    for bad in ["", "   ", "\t\n", &"x".repeat(MAX_TITLE_CHARS + 1)] {
        assert_invalid(
            validate_title(bad).unwrap_err(),
            "title",
            reasons::INVALID_TITLE,
        );
    }
    // Trimmed length counts.
    let padded = format!("  {}  ", "x".repeat(MAX_TITLE_CHARS));
    assert!(validate_title(&padded).is_ok());
}

// ── create ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_uses_default_model_and_trims_title() {
    let env = TestEnv::default_env().await;
    let dto = env
        .services
        .chats
        .create(&ctx_a1(), req(Some("  My chat "), None))
        .await
        .unwrap();
    assert_eq!(dto.model, "gpt-premium");
    assert_eq!(dto.title.as_deref(), Some("My chat"));
    assert_eq!(dto.message_count, 0);
    assert!(!dto.is_temporary);
    assert_eq!(dto.created_at, dto.updated_at);

    let row = chat_row(&env, dto.id).await;
    assert_eq!(row.tenant_id, TENANT_A);
    assert_eq!(row.user_id, USER_A1);
    assert_eq!(row.title.as_deref(), Some("My chat"));
    assert!(row.deleted_at.is_none());

    let json = serde_json::to_value(&dto).unwrap();
    assert!(json.get("user_id").is_none());
    assert!(json.get("tenant_id").is_none());
    env.shutdown().await;
}

#[tokio::test]
async fn create_without_title_omits_title() {
    let env = TestEnv::default_env().await;
    let dto = env
        .services
        .chats
        .create(&ctx_a1(), req(None, None))
        .await
        .unwrap();
    assert!(dto.title.is_none());
    let json = serde_json::to_value(&dto).unwrap();
    assert!(json.get("title").is_none(), "title must be omitted: {json}");
    assert_eq!(json["message_count"], 0);
    assert_eq!(chat_row(&env, dto.id).await.title, None);
    env.shutdown().await;
}

#[tokio::test]
async fn create_with_explicit_model() {
    let env = TestEnv::default_env().await;
    let dto = env
        .services
        .chats
        .create(&ctx_a1(), req(None, Some("gpt-standard")))
        .await
        .unwrap();
    assert_eq!(dto.model, "gpt-standard");
    env.shutdown().await;
}

#[tokio::test]
async fn create_rejects_unknown_and_disabled_models() {
    let env = TestEnv::default_env().await;
    for m in ["gpt-disabled", "nope"] {
        let e = env
            .services
            .chats
            .create(&ctx_a1(), req(None, Some(m)))
            .await
            .unwrap_err();
        assert_invalid(e, "model", reasons::INVALID_MODEL);
    }
    env.shutdown().await;
}

#[tokio::test]
async fn create_without_enabled_model_is_invalid_model() {
    let mut d = crate::domain::service::test_support::model("only-disabled", "standard");
    d.enabled = false;
    let env = TestEnv::new(crate::domain::service::test_support::TestOptions {
        catalog: vec![d],
        ..Default::default()
    })
    .await;
    let e = env
        .services
        .chats
        .create(&ctx_a1(), req(None, None))
        .await
        .unwrap_err();
    assert_invalid(e, "model", reasons::INVALID_MODEL);
    env.shutdown().await;
}

#[tokio::test]
async fn create_default_falls_back_to_first_enabled() {
    let mut a = crate::domain::service::test_support::model("first-disabled", "standard");
    a.enabled = false;
    let b = crate::domain::service::test_support::model("second", "standard");
    let c = crate::domain::service::test_support::model("third", "premium");
    let env = TestEnv::new(crate::domain::service::test_support::TestOptions {
        catalog: vec![a, b, c],
        ..Default::default()
    })
    .await;
    let dto = env
        .services
        .chats
        .create(&ctx_a1(), req(None, None))
        .await
        .unwrap();
    assert_eq!(dto.model, "second");
    env.shutdown().await;
}

#[tokio::test]
async fn create_title_is_validated_before_pep_and_model() {
    // Deny PDP: an invalid title still yields 400, not 403.
    let env = env_with_pdp(Arc::new(DenyPdp)).await;
    let e = env
        .services
        .chats
        .create(&ctx_a1(), req(Some("   "), Some("nope")))
        .await
        .unwrap_err();
    assert_invalid(e, "title", reasons::INVALID_TITLE);
    // A valid title reaches the PEP.
    let e = env
        .services
        .chats
        .create(&ctx_a1(), req(Some("ok"), Some("nope")))
        .await
        .unwrap_err();
    assert_eq!(problem(e)["status"], 403);
    env.shutdown().await;

    // Allowing PDP: invalid title + invalid model -> title wins.
    let env = TestEnv::default_env().await;
    let long = "x".repeat(256);
    let e = env
        .services
        .chats
        .create(&ctx_a1(), req(Some(&long), Some("nope")))
        .await
        .unwrap_err();
    assert_invalid(e, "title", reasons::INVALID_TITLE);
    env.shutdown().await;
}

// ── get ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_counts_non_deleted_messages() {
    let env = TestEnv::default_env().await;
    let id = create_chat(&env, USER_A1, TENANT_A, Some("t")).await;
    let now = OffsetDateTime::now_utc();
    let rid = Uuid::new_v4();
    insert_message(&env, message_am(TENANT_A, id, "user", Some(rid), now)).await;
    insert_message(&env, message_am(TENANT_A, id, "assistant", Some(rid), now)).await;
    insert_message(
        &env,
        message_am(TENANT_A, id, "system", Some(Uuid::new_v4()), now),
    )
    .await;
    let mut deleted = message_am(TENANT_A, id, "user", Some(Uuid::new_v4()), now);
    deleted.deleted_at = sea_orm::ActiveValue::Set(Some(now));
    insert_message(&env, deleted).await;

    let dto = env.services.chats.get(&ctx_a1(), id).await.unwrap();
    assert_eq!(dto.id, id);
    assert_eq!(dto.message_count, 3);
    assert_eq!(dto.title.as_deref(), Some("t"));
    env.shutdown().await;
}

#[tokio::test]
async fn get_unknown_chat_is_404() {
    let env = TestEnv::default_env().await;
    let e = env
        .services
        .chats
        .get(&ctx_a1(), Uuid::new_v4())
        .await
        .unwrap_err();
    assert_not_found(e, resource_types::CHAT);
    env.shutdown().await;
}

// ── update ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn update_title_renames_and_bumps_updated_at() {
    let env = TestEnv::default_env().await;
    let created = env
        .services
        .chats
        .create(&ctx_a1(), req(Some("old"), Some("gpt-standard")))
        .await
        .unwrap();
    let dto = env
        .services
        .chats
        .update_title(&ctx_a1(), created.id, "  Renamed ")
        .await
        .unwrap();
    assert_eq!(dto.title.as_deref(), Some("Renamed"));
    assert_eq!(dto.model, "gpt-standard", "model is immutable");
    assert!(dto.updated_at > created.updated_at);
    assert_eq!(dto.created_at, created.created_at);
    let row = chat_row(&env, created.id).await;
    assert_eq!(row.title.as_deref(), Some("Renamed"));
    assert_eq!(row.model, "gpt-standard");
    assert_eq!(row.updated_at, dto.updated_at);
    env.shutdown().await;
}

#[tokio::test]
async fn update_validates_title_before_loading_chat() {
    let env = TestEnv::default_env().await;
    // Unknown chat + bad title -> 400 (title first).
    let e = env
        .services
        .chats
        .update_title(&ctx_a1(), Uuid::new_v4(), "  ")
        .await
        .unwrap_err();
    assert_invalid(e, "title", reasons::INVALID_TITLE);
    // Unknown chat + good title -> 404.
    let e = env
        .services
        .chats
        .update_title(&ctx_a1(), Uuid::new_v4(), "fine")
        .await
        .unwrap_err();
    assert_not_found(e, resource_types::CHAT);
    env.shutdown().await;
}

// ── delete ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_soft_deletes_marks_attachments_and_enqueues_cleanup() {
    let env = TestEnv::default_env().await;
    let id = create_chat(&env, USER_A1, TENANT_A, None).await;
    let a_live = insert_attachment(&env, TENANT_A, id, "document", "ready", None, false).await;
    let a_deleted = insert_attachment(&env, TENANT_A, id, "image", "ready", None, true).await;

    env.services.chats.delete(&ctx_a1(), id).await.unwrap();

    let row = chat_row(&env, id).await;
    let deleted_at = row.deleted_at.expect("soft-deleted");
    assert_eq!(row.updated_at, deleted_at);

    let atts = attachment_rows(&env, id).await;
    let live = atts.iter().find(|a| a.id == a_live.id).unwrap();
    assert_eq!(live.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(live.cleanup_updated_at, Some(deleted_at));
    let gone = atts.iter().find(|a| a.id == a_deleted.id).unwrap();
    assert!(
        gone.cleanup_status.is_none(),
        "deleted attachments are untouched"
    );

    let queue = env.deps.cfg.outbox.chat_cleanup_queue_name.clone();
    let delivered = env.delivered_to(&queue, 1).await;
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    let ev = &delivered[0];
    assert_eq!(ev["chat_id"], id.to_string());
    assert_eq!(ev["tenant_id"], TENANT_A.to_string());
    assert_eq!(ev["reason"], "chat_soft_delete");
    assert!(Uuid::parse_str(ev["system_request_id"].as_str().unwrap()).is_ok());
    assert!(ev["chat_deleted_at"].is_string());

    // Second delete and reads -> 404.
    let e = env.services.chats.delete(&ctx_a1(), id).await.unwrap_err();
    assert_not_found(e, resource_types::CHAT);
    let e = env.services.chats.get(&ctx_a1(), id).await.unwrap_err();
    assert_not_found(e, resource_types::CHAT);
    let e = env
        .services
        .chats
        .update_title(&ctx_a1(), id, "x")
        .await
        .unwrap_err();
    assert_not_found(e, resource_types::CHAT);
    env.shutdown().await;
}

#[tokio::test]
async fn delete_keeps_existing_cleanup_status() {
    let env = TestEnv::default_env().await;
    let id = create_chat(&env, USER_A1, TENANT_A, None).await;
    let a = insert_attachment(&env, TENANT_A, id, "document", "failed", None, false).await;
    {
        use sea_orm::sea_query::Expr;
        use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
        use toolkit_db::secure::{AccessScope, SecureUpdateExt};
        let conn = env.deps.db.conn().unwrap();
        crate::infra::db::entity::attachment::Entity::update_many()
            .col_expr(
                crate::infra::db::entity::attachment::Column::CleanupStatus,
                Expr::value("done"),
            )
            .filter(Condition::all().add(crate::infra::db::entity::attachment::Column::Id.eq(a.id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }
    env.services.chats.delete(&ctx_a1(), id).await.unwrap();
    let rows = attachment_rows(&env, id).await;
    assert_eq!(rows[0].cleanup_status.as_deref(), Some("done"));
    env.shutdown().await;
}

// ── list ───────────────────────────────────────────────────────────────────

async fn list(
    env: &TestEnv,
    user: Uuid,
    tenant: Uuid,
    qs: &str,
) -> toolkit_odata::Page<crate::api::rest::dto::ChatDetailDto> {
    env.services
        .chats
        .list(&ctx(user, tenant), odata(qs).await.unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn list_orders_by_updated_at_desc_and_counts_messages() {
    let env = TestEnv::default_env().await;
    let c1 = create_chat(&env, USER_A1, TENANT_A, Some("one")).await;
    let c2 = create_chat(&env, USER_A1, TENANT_A, Some("two")).await;
    let c3 = create_chat(&env, USER_A1, TENANT_A, Some("three")).await;
    // Rename c1 -> most recent activity.
    env.services
        .chats
        .update_title(&ctx_a1(), c1, "one!")
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    insert_message(
        &env,
        message_am(TENANT_A, c2, "user", Some(Uuid::new_v4()), now),
    )
    .await;
    insert_message(
        &env,
        message_am(TENANT_A, c2, "user", Some(Uuid::new_v4()), now),
    )
    .await;

    let page = list(&env, USER_A1, TENANT_A, "").await;
    let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![c1, c3, c2]);
    assert_eq!(page.page_info.limit, 20);
    assert!(page.page_info.next_cursor.is_none());
    assert!(page.page_info.prev_cursor.is_none());
    let counts: Vec<i64> = page.items.iter().map(|c| c.message_count).collect();
    assert_eq!(counts, vec![0, 0, 2]);

    let json = serde_json::to_value(&page).unwrap();
    assert!(json["items"].is_array());
    assert!(json["page_info"].get("limit").is_some());
    env.shutdown().await;
}

#[tokio::test]
async fn list_excludes_deleted_and_foreign_chats() {
    let env = TestEnv::default_env().await;
    let mine = create_chat(&env, USER_A1, TENANT_A, None).await;
    let deleted = create_chat(&env, USER_A1, TENANT_A, None).await;
    env.services.chats.delete(&ctx_a1(), deleted).await.unwrap();
    let other_user = create_chat(&env, USER_A2, TENANT_A, None).await;
    let other_tenant = create_chat(&env, USER_B, TENANT_B, None).await;

    let page = list(&env, USER_A1, TENANT_A, "").await;
    let ids: Vec<Uuid> = page.items.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![mine]);
    let page = list(&env, USER_A2, TENANT_A, "").await;
    assert_eq!(
        page.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![other_user]
    );
    let page = list(&env, USER_B, TENANT_B, "").await;
    assert_eq!(
        page.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![other_tenant]
    );
    env.shutdown().await;
}

async fn page_through(env: &TestEnv, base_qs: &str, limit: u64) -> Vec<Uuid> {
    let mut seen = Vec::new();
    let mut qs = format!("limit={limit}{base_qs}");
    for _ in 0..20 {
        let page = list(env, USER_A1, TENANT_A, &qs).await;
        assert!(page.items.len() as u64 <= limit);
        seen.extend(page.items.iter().map(|c| c.id));
        match page.page_info.next_cursor {
            Some(c) => {
                // Filter must be repeated with the cursor; $orderby must not.
                let filter_part: String = base_qs
                    .split('&')
                    .filter(|p| p.starts_with("%24filter") || p.starts_with("$filter"))
                    .map(|p| format!("&{p}"))
                    .collect();
                qs = format!("limit={limit}&cursor={c}{filter_part}");
            }
            None => return seen,
        }
    }
    panic!("pagination did not terminate");
}

#[tokio::test]
async fn list_pagination_has_no_duplicates_or_gaps() {
    let env = TestEnv::default_env().await;
    let mut expected = Vec::new();
    for i in 0..5 {
        expected.push(create_chat(&env, USER_A1, TENANT_A, Some(&format!("chat {i}"))).await);
    }
    expected.reverse(); // updated_at desc

    for limit in [1_u64, 2, 3] {
        let seen = page_through(&env, "", limit).await;
        assert_eq!(seen, expected, "limit={limit}");
    }

    // Explicit $orderby on updated_at asc.
    let seen = page_through(
        &env,
        &format!("&{}={}", enc("$orderby"), enc("updated_at asc")),
        2,
    )
    .await;
    let mut asc = expected.clone();
    asc.reverse();
    assert_eq!(seen, asc);

    // Explicit $orderby on title.
    let seen = page_through(
        &env,
        &format!("&{}={}", enc("$orderby"), enc("title desc")),
        2,
    )
    .await;
    assert_eq!(seen, expected, "titles are 'chat 0'..'chat 4'");

    // Backward navigation: page 2 -> prev cursor -> page 1.
    let p1 = list(&env, USER_A1, TENANT_A, "limit=2").await;
    let p2 = list(
        &env,
        USER_A1,
        TENANT_A,
        &format!(
            "limit=2&cursor={}",
            p1.page_info.next_cursor.clone().unwrap()
        ),
    )
    .await;
    assert_eq!(
        p2.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        expected[2..4].to_vec()
    );
    let back = list(
        &env,
        USER_A1,
        TENANT_A,
        &format!(
            "limit=2&cursor={}",
            p2.page_info.prev_cursor.clone().unwrap()
        ),
    )
    .await;
    assert_eq!(
        back.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        p1.items.iter().map(|c| c.id).collect::<Vec<_>>()
    );
    env.shutdown().await;
}

#[tokio::test]
async fn list_filters_by_title_contains() {
    let env = TestEnv::default_env().await;
    let a = create_chat(&env, USER_A1, TENANT_A, Some("alpha x")).await;
    let _b = create_chat(&env, USER_A1, TENANT_A, Some("beta")).await;
    let c = create_chat(&env, USER_A1, TENANT_A, Some("x gamma")).await;
    let _d = create_chat(&env, USER_A1, TENANT_A, None).await;
    let f = format!("{}={}", enc("$filter"), enc("contains(title,'x')"));
    let page = list(&env, USER_A1, TENANT_A, &f).await;
    let ids: HashSet<Uuid> = page.items.iter().map(|c| c.id).collect();
    assert_eq!(ids, HashSet::from([a, c]));

    // Paged with the filter (cursor must carry the same filter).
    let seen = page_through(&env, &format!("&{f}"), 1).await;
    assert_eq!(seen, vec![c, a]);

    // Cursor with a different filter -> FILTER_MISMATCH.
    let p1 = list(&env, USER_A1, TENANT_A, &format!("limit=1&{f}")).await;
    let cursor = p1.page_info.next_cursor.unwrap();
    let e = env
        .services
        .chats
        .list(
            &ctx_a1(),
            odata(&format!("limit=1&cursor={cursor}")).await.unwrap(),
        )
        .await
        .unwrap_err();
    let p = problem(e);
    assert_eq!(p["status"], 400);
    assert_eq!(p["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
    env.shutdown().await;
}

#[tokio::test]
async fn list_filters_by_updated_at_and_id() {
    let env = TestEnv::default_env().await;
    let old = create_chat(&env, USER_A1, TENANT_A, Some("old")).await;
    let boundary = OffsetDateTime::now_utc();
    let new = create_chat(&env, USER_A1, TENANT_A, Some("new")).await;
    let ts = boundary
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();

    let f = format!("{}={}", enc("$filter"), enc(&format!("updated_at gt {ts}")));
    let page = list(&env, USER_A1, TENANT_A, &f).await;
    assert_eq!(
        page.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![new]
    );

    let f = format!("{}={}", enc("$filter"), enc(&format!("updated_at lt {ts}")));
    let page = list(&env, USER_A1, TENANT_A, &f).await;
    assert_eq!(
        page.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![old]
    );

    let f = format!("{}={}", enc("$filter"), enc(&format!("id eq '{old}'")));
    let page = list(&env, USER_A1, TENANT_A, &f).await;
    assert_eq!(
        page.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![old]
    );
    env.shutdown().await;
}

#[tokio::test]
async fn list_odata_errors() {
    let env = TestEnv::default_env().await;
    create_chat(&env, USER_A1, TENANT_A, None).await;

    // limit=0 is rejected by the extractor.
    let e = odata("limit=0").await.unwrap_err();
    let p = super::test_rows::canonical_problem(e);
    assert_eq!(p["status"], 400);
    assert_eq!(
        p["context"]["field_violations"][0]["reason"],
        "INVALID_LIMIT"
    );

    // Unknown filter field -> 400 with the OData resource type.
    let q = odata(&format!("{}={}", enc("$filter"), enc("bogus eq 1")))
        .await
        .unwrap();
    let e = env.services.chats.list(&ctx_a1(), q).await.unwrap_err();
    let p = problem(e);
    assert_eq!(p["status"], 400, "{p}");
    assert_eq!(p["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
    assert_eq!(
        p["context"]["field_violations"][0]["reason"],
        "INVALID_FILTER"
    );

    // Unknown orderby field.
    let q = odata(&format!("{}={}", enc("$orderby"), enc("model asc")))
        .await
        .unwrap();
    let e = env.services.chats.list(&ctx_a1(), q).await.unwrap_err();
    let p = problem(e);
    assert_eq!(p["status"], 400, "{p}");
    assert_eq!(
        p["context"]["field_violations"][0]["reason"],
        "INVALID_ORDERBY_FIELD"
    );

    // Garbage cursor is rejected by the extractor.
    let e = odata("cursor=not-a-cursor").await.unwrap_err();
    let p = super::test_rows::canonical_problem(e);
    assert_eq!(
        p["context"]["field_violations"][0]["reason"],
        "INVALID_CURSOR"
    );
    env.shutdown().await;
}

#[tokio::test]
async fn list_limit_is_clamped_and_select_ignored() {
    let env = TestEnv::default_env().await;
    create_chat(&env, USER_A1, TENANT_A, Some("t")).await;
    let page = list(&env, USER_A1, TENANT_A, "limit=1000").await;
    assert_eq!(page.page_info.limit, 100);
    let page = list(&env, USER_A1, TENANT_A, &format!("{}=id", enc("$select"))).await;
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items[0].title.as_deref(),
        Some("t"),
        "$select is ignored"
    );
    env.shutdown().await;
}

// ── isolation / PEP ────────────────────────────────────────────────────────

#[tokio::test]
async fn foreign_users_get_404() {
    let env = TestEnv::default_env().await;
    let id = create_chat(&env, USER_A1, TENANT_A, Some("secret")).await;
    for (user, tenant) in [(USER_A2, TENANT_A), (USER_B, TENANT_B)] {
        let c = ctx(user, tenant);
        assert_not_found(
            env.services.chats.get(&c, id).await.unwrap_err(),
            resource_types::CHAT,
        );
        assert_not_found(
            env.services
                .chats
                .update_title(&c, id, "hijack")
                .await
                .unwrap_err(),
            resource_types::CHAT,
        );
        assert_not_found(
            env.services.chats.delete(&c, id).await.unwrap_err(),
            resource_types::CHAT,
        );
    }
    let row = chat_row(&env, id).await;
    assert_eq!(row.title.as_deref(), Some("secret"));
    assert!(row.deleted_at.is_none());
    env.shutdown().await;
}

#[tokio::test]
async fn deny_pdp_is_403() {
    let env = env_with_pdp(Arc::new(DenyPdp)).await;
    let c = ctx_a1();
    let id = Uuid::new_v4();
    let errs = vec![
        env.services
            .chats
            .create(&c, req(None, None))
            .await
            .unwrap_err(),
        env.services.chats.get(&c, id).await.unwrap_err(),
        env.services
            .chats
            .update_title(&c, id, "x")
            .await
            .unwrap_err(),
        env.services.chats.delete(&c, id).await.unwrap_err(),
        env.services
            .chats
            .list(&c, odata("").await.unwrap())
            .await
            .unwrap_err(),
    ];
    for e in errs {
        let p = problem(e);
        assert_eq!(p["status"], 403, "{p}");
        assert_eq!(p["context"]["reason"], "AUTHZ_DENIED");
    }
    env.shutdown().await;
}

#[tokio::test]
async fn failing_pdp_is_503() {
    let env = env_with_pdp(Arc::new(FailingPdp)).await;
    let c = ctx_a1();
    let id = Uuid::new_v4();
    let errs = vec![
        env.services
            .chats
            .create(&c, req(None, None))
            .await
            .unwrap_err(),
        env.services.chats.get(&c, id).await.unwrap_err(),
        env.services
            .chats
            .update_title(&c, id, "x")
            .await
            .unwrap_err(),
        env.services.chats.delete(&c, id).await.unwrap_err(),
        env.services
            .chats
            .list(&c, odata("").await.unwrap())
            .await
            .unwrap_err(),
    ];
    for e in errs {
        let p = problem(e);
        assert_eq!(p["status"], 503, "{p}");
        assert_eq!(p["context"]["retry_after_seconds"], 5);
    }
    env.shutdown().await;
}

#[tokio::test]
async fn list_orders_untitled_chats_by_title_with_working_cursors() {
    let env = TestEnv::default_env().await;
    let b = create_chat(&env, USER_A1, TENANT_A, Some("b")).await;
    let u1 = create_chat(&env, USER_A1, TENANT_A, None).await;
    let a = create_chat(&env, USER_A1, TENANT_A, Some("a")).await;
    let u2 = create_chat(&env, USER_A1, TENANT_A, None).await;
    let c = create_chat(&env, USER_A1, TENANT_A, Some("c")).await;
    // Untitled chats order as '' (first ascending), ties broken by id desc.
    let mut untitled = vec![u1, u2];
    untitled.sort();
    untitled.reverse();
    let mut asc = untitled.clone();
    asc.extend([a, b, c]);

    let one_page = list(
        &env,
        USER_A1,
        TENANT_A,
        &format!("{}={}", enc("$orderby"), enc("title asc")),
    )
    .await;
    assert_eq!(one_page.items.iter().map(|c| c.id).collect::<Vec<_>>(), asc);
    assert!(one_page.items[0].title.is_none());

    for limit in [1_u64, 2, 3] {
        let seen = page_through(
            &env,
            &format!("&{}={}", enc("$orderby"), enc("title asc")),
            limit,
        )
        .await;
        assert_eq!(seen, asc, "asc limit={limit}");
    }
    let seen = page_through(
        &env,
        &format!("&{}={}", enc("$orderby"), enc("title desc")),
        2,
    )
    .await;
    let mut desc = vec![c, b, a];
    desc.extend(untitled.iter().copied());
    assert_eq!(seen, desc);

    // Backward navigation over the untitled boundary.
    let qs = format!("limit=2&{}={}", enc("$orderby"), enc("title asc"));
    let p1 = list(&env, USER_A1, TENANT_A, &qs).await;
    let p2 = list(
        &env,
        USER_A1,
        TENANT_A,
        &format!("limit=2&cursor={}", p1.page_info.next_cursor.clone().unwrap()),
    )
    .await;
    assert_eq!(p2.items.iter().map(|c| c.id).collect::<Vec<_>>(), asc[2..4].to_vec());
    let back = list(
        &env,
        USER_A1,
        TENANT_A,
        &format!("limit=2&cursor={}", p2.page_info.prev_cursor.clone().unwrap()),
    )
    .await;
    assert_eq!(
        back.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        asc[..2].to_vec()
    );

    // With a filter: the cursor carries its hash.
    let seen = page_through(
        &env,
        &format!(
            "&{}={}&{}={}",
            enc("$orderby"),
            enc("title asc"),
            enc("$filter"),
            enc("updated_at ge 2000-01-01T00:00:00Z")
        ),
        2,
    )
    .await;
    assert_eq!(seen, asc);
    env.shutdown().await;
}
