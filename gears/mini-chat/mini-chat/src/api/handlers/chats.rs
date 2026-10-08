//! Chat CRUD and list handlers: `mini_chat.{create,list,get,update,delete}_chat`.

use std::sync::Arc;

use axum::Extension;
use axum::extract::OriginalUri;
use toolkit::api::canonical_prelude::*;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::dto::chats::{ChatDetailDto, CreateChatReq, UpdateChatReq};
use crate::api::state::AppServices;
use crate::domain::chat_service::CreateChat;

/// `mini_chat.create_chat`: 201 with the chat and `Location: {request path}/{id}`.
///
/// # Errors
/// 400 invalid title / model, 403 / 503 from the PDP, 500 on policy plugin or database failures.
pub async fn create_chat(
    OriginalUri(uri): OriginalUri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Json(req): extract::Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = svc
        .chats
        .create(
            &ctx,
            CreateChat {
                title: req.title,
                model: req.model,
            },
        )
        .await?;
    let id = chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(chat), &uri, &id))
}

/// `mini_chat.list_chats`: the caller's chats, most recent activity first.
///
/// # Errors
/// 400 for an invalid `$filter`, `$orderby`, `limit` or `cursor`, 403 / 503 from the PDP.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    OData(query): OData,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = svc.chats.list(&ctx, &query).await?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

/// `mini_chat.get_chat`.
///
/// # Errors
/// 404 for a missing, deleted or foreign chat, 403 / 503 from the PDP.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(svc.chats.get(&ctx, id).await?.into()))
}

/// `mini_chat.update_chat`: renames the chat; other body fields are ignored.
///
/// # Errors
/// 400 invalid title, 404 for a missing, deleted or foreign chat, 403 / 503 from the PDP.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(svc.chats.rename(&ctx, id, req.title).await?.into()))
}

/// `mini_chat.delete_chat`: soft-deletes the chat, 204.
///
/// # Errors
/// 404 for a missing, deleted or foreign chat, 403 / 503 from the PDP.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    svc.chats.delete(&ctx, id).await?;
    Ok(no_content())
}

#[cfg(test)]
#[allow(clippy::inconsistent_struct_constructor)] // fixtures list the interesting fields first
mod tests {
    use axum::body::Body;
    use http::Request;
    use sea_orm::{ActiveValue::NotSet, ColumnTrait, EntityTrait, QueryFilter, Set};
    use serde_json::{Value, json};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use toolkit_db::secure::{AccessScope, SecureEntityExt, secure_insert};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use crate::infra::db::entity::{attachments, messages};
    use crate::infra::db::ts::db_now;
    use crate::test_support::app::{TestApp, TestResponse, ctx};
    use crate::test_support::pdp::PdpMode;

    const CHATS: &str = "/mini-chat/v1/chats";
    const CHAT_RESOURCE: &str = "gts.cf.core.mini_chat.chat.v1~";
    const ODATA_RESOURCE: &str = "gts.cf.core.odata.query.v1~";
    const CHAT_CLEANUP_QUEUE: &str = "mini-chat.chat_cleanup";

    fn user() -> SecurityContext {
        ctx(Uuid::new_v4(), Uuid::new_v4())
    }

    fn violation(res: &TestResponse) -> (&str, &str) {
        let v = &res.json["context"]["field_violations"][0];
        (v["field"].as_str().unwrap(), v["reason"].as_str().unwrap())
    }

    fn id_of(res: &TestResponse) -> String {
        res.json["id"].as_str().expect("chat id").to_owned()
    }

    async fn create(app: &TestApp, who: &SecurityContext, body: Value) -> TestResponse {
        let res = app.call("POST", CHATS, who, Some(body)).await;
        assert_eq!(res.status, 201, "{}", res.json);
        res
    }

    fn parse_ts(value: &Value) -> OffsetDateTime {
        OffsetDateTime::parse(value.as_str().expect("timestamp"), &Rfc3339).expect("rfc3339")
    }

    fn item_ids(page: &Value) -> Vec<&str> {
        page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn create_defaults_model_and_returns_location() {
        let app = TestApp::builder().build().await;
        let res = create(&app, &user(), json!({})).await;
        assert_eq!(res.json["model"], "gpt-premium");
        assert_eq!(res.json["message_count"], 0);
        assert_eq!(res.json["is_temporary"], false);
        assert!(res.json.get("title").is_none(), "{}", res.json);
        assert!(res.json.get("user_id").is_none(), "{}", res.json);
        assert_eq!(
            res.json["created_at"], res.json["updated_at"],
            "created_at == updated_at"
        );
        let id = id_of(&res);
        assert_eq!(
            res.headers["location"],
            format!("/mini-chat/v1/chats/{id}").as_str()
        );
    }

    #[tokio::test]
    async fn create_validates_title_and_model() {
        let app = TestApp::builder().build().await;
        let who = user();

        for title in ["   ".to_owned(), "\u{e9}".repeat(256), String::new()] {
            let res = app
                .call("POST", CHATS, &who, Some(json!({ "title": title })))
                .await;
            assert_eq!(res.status, 400, "{}", res.json);
            assert_eq!(violation(&res), ("title", "INVALID_TITLE"));
            assert_eq!(res.json["context"]["resource_type"], CHAT_RESOURCE);
        }

        let long = "\u{e9}".repeat(255);
        let res = create(&app, &who, json!({ "title": format!("  {long}  ") })).await;
        assert_eq!(res.json["title"], long.as_str(), "stored trimmed");

        let res = create(&app, &who, json!({ "title": null })).await;
        assert!(res.json.get("title").is_none());

        for model in ["gpt-disabled", "nope"] {
            let res = app
                .call("POST", CHATS, &who, Some(json!({ "model": model })))
                .await;
            assert_eq!(res.status, 400, "{model}: {}", res.json);
            assert_eq!(violation(&res), ("model", "INVALID_MODEL"), "{model}");
        }
        let res = create(&app, &who, json!({ "model": "gpt-standard" })).await;
        assert_eq!(res.json["model"], "gpt-standard");
    }

    #[tokio::test]
    async fn create_validates_title_before_authorization() {
        let app = TestApp::builder().pdp(PdpMode::Deny).build().await;
        let res = app
            .call("POST", CHATS, &user(), Some(json!({ "title": " " })))
            .await;
        assert_eq!(res.status, 400, "{}", res.json);
        assert!(app.pdp.requests().is_empty(), "no PDP call for a bad title");

        let res = app.call("POST", CHATS, &user(), Some(json!({}))).await;
        assert_eq!(res.status, 403, "{}", res.json);

        let app = TestApp::builder().pdp(PdpMode::Fail).build().await;
        let res = app.call("POST", CHATS, &user(), Some(json!({}))).await;
        assert_eq!(res.status, 503, "{}", res.json);
        assert_eq!(res.headers["retry-after"], "5");
    }

    #[tokio::test]
    async fn create_json_extractor_errors() {
        let app = TestApp::builder().build().await;
        let who = user();
        let raw = |content_type: Option<&str>, body: &str| {
            let mut b = Request::builder().method("POST").uri(CHATS);
            if let Some(ct) = content_type {
                b = b.header(http::header::CONTENT_TYPE, ct);
            }
            let mut req = b.body(Body::from(body.to_owned())).unwrap();
            req.extensions_mut().insert(who.clone());
            req
        };
        let send = |req| async {
            let resp = app.call_raw(req).await;
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        };

        let (status, json) = send(raw(Some("application/json"), "{not json")).await;
        assert_eq!(status, 400, "{json}");
        let v = &json["context"]["field_violations"][0];
        assert_eq!(
            (v["field"].as_str(), v["reason"].as_str()),
            (Some("body"), Some("json_syntax_error"))
        );

        let (status, json) = send(raw(Some("application/json"), r#"{"title": 5}"#)).await;
        assert_eq!(status, 422, "{json}");
        assert_eq!(
            json["context"]["field_violations"][0]["reason"],
            "invalid_json_body"
        );

        let (status, json) = send(raw(None, "{}")).await;
        assert_eq!(status, 415, "{json}");
        assert_eq!(
            json["context"]["field_violations"][0]["reason"],
            "missing_json_content_type"
        );
    }

    #[tokio::test]
    async fn get_patch_delete_lifecycle() {
        let app = TestApp::builder().build().await;
        let who = user();
        let created = create(&app, &who, json!({})).await;
        let id = id_of(&created);
        let uri = format!("{CHATS}/{id}");

        let got = app.call("GET", &uri, &who, None).await;
        assert_eq!(got.status, 200, "{}", got.json);
        assert_eq!(got.json, created.json);

        let patched = app
            .call(
                "PATCH",
                &uri,
                &who,
                Some(json!({ "title": " New ", "model": "gpt-standard" })),
            )
            .await;
        assert_eq!(patched.status, 200, "{}", patched.json);
        assert_eq!(patched.json["title"], "New");
        assert_eq!(patched.json["model"], "gpt-premium", "model is immutable");
        assert_eq!(patched.json["created_at"], created.json["created_at"]);
        assert!(
            parse_ts(&patched.json["updated_at"]) > parse_ts(&created.json["updated_at"]),
            "{} vs {}",
            patched.json["updated_at"],
            created.json["updated_at"]
        );

        for (body, status, field) in [
            (json!({ "title": "  " }), 400, Some("INVALID_TITLE")),
            (
                json!({ "title": "x".repeat(256) }),
                400,
                Some("INVALID_TITLE"),
            ),
            (json!({}), 422, None),
            (json!({ "title": null }), 422, None),
        ] {
            let res = app.call("PATCH", &uri, &who, Some(body.clone())).await;
            assert_eq!(res.status, status, "{body}: {}", res.json);
            if let Some(reason) = field {
                assert_eq!(violation(&res), ("title", reason));
            }
        }
        let still = app.call("GET", &uri, &who, None).await;
        assert_eq!(still.json["title"], "New");

        let deleted = app.call("DELETE", &uri, &who, None).await;
        assert_eq!(deleted.status, 204, "{}", deleted.json);
        let gone = app.call("GET", &uri, &who, None).await;
        assert_eq!(gone.status, 404, "{}", gone.json);
        assert_eq!(gone.json["context"]["resource_type"], CHAT_RESOURCE);
        let again = app.call("DELETE", &uri, &who, None).await;
        assert_eq!(again.status, 404, "{}", again.json);
        let patch = app
            .call("PATCH", &uri, &who, Some(json!({ "title": "x" })))
            .await;
        assert_eq!(patch.status, 404, "{}", patch.json);
    }

    #[tokio::test]
    async fn foreign_and_cross_tenant_chats_are_404() {
        let app = TestApp::builder().build().await;
        let tenant = Uuid::new_v4();
        let owner = ctx(tenant, Uuid::new_v4());
        let same_tenant = ctx(tenant, Uuid::new_v4());
        let other_tenant = ctx(Uuid::new_v4(), Uuid::new_v4());
        let id = id_of(&create(&app, &owner, json!({ "title": "mine" })).await);
        let uri = format!("{CHATS}/{id}");

        for intruder in [&same_tenant, &other_tenant] {
            for (method, body) in [
                ("GET", None),
                ("PATCH", Some(json!({ "title": "hijack" }))),
                ("DELETE", None),
            ] {
                let res = app.call(method, &uri, intruder, body).await;
                assert_eq!(res.status, 404, "{method}: {}", res.json);
                assert_eq!(res.json["context"]["resource_type"], CHAT_RESOURCE);
            }
        }
        let res = app.call("GET", &uri, &owner, None).await;
        assert_eq!(res.status, 200, "owner still sees an untouched chat");
        assert_eq!(res.json["title"], "mine");

        let res = app
            .call("GET", &format!("{CHATS}/not-a-uuid"), &owner, None)
            .await;
        assert_eq!(res.status, 400, "{}", res.json);
        assert_eq!(violation(&res), ("path", "invalid_path_params"));
    }

    async fn seed_attachment(
        app: &TestApp,
        chat: Uuid,
        tenant: Uuid,
        user: Uuid,
        cleanup_status: Option<&str>,
        deleted: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let now = db_now();
        let am = attachments::ActiveModel {
            id: Set(id),
            tenant_id: Set(tenant),
            chat_id: Set(chat),
            uploaded_by_user_id: Set(user),
            filename: Set("a.txt".into()),
            content_type: Set("text/plain".into()),
            size_bytes: Set(1),
            status: Set("ready".into()),
            attachment_kind: Set("document".into()),
            created_at: Set(now),
            cleanup_status: Set(cleanup_status.map(str::to_owned)),
            deleted_at: Set(deleted.then_some(now)),
            storage_backend: NotSet,
            provider_file_id: NotSet,
            error_code: NotSet,
            for_file_search: NotSet,
            for_code_interpreter: NotSet,
            doc_summary: NotSet,
            img_thumbnail: NotSet,
            img_thumbnail_width: NotSet,
            img_thumbnail_height: NotSet,
            summary_model: NotSet,
            summary_updated_at: NotSet,
            cleanup_attempts: NotSet,
            last_cleanup_error: NotSet,
            cleanup_updated_at: NotSet,
            updated_at: NotSet,
            secondary_file_id: NotSet,
            secondary_status: NotSet,
            secondary_provider_kind: NotSet,
        };
        let conn = app.db.conn().unwrap();
        secure_insert::<attachments::Entity>(am, &AccessScope::allow_all(), &conn)
            .await
            .unwrap();
        id
    }

    async fn attachment_row(app: &TestApp, id: Uuid) -> attachments::Model {
        let conn = app.db.conn().unwrap();
        attachments::Entity::find()
            .filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .unwrap()
            .expect("attachment row")
    }

    #[tokio::test]
    async fn delete_marks_attachments_and_enqueues_chat_cleanup() {
        let app = TestApp::builder().quiet_cleanup().build().await;
        let (tenant, user_id) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user_id);
        let created = create(&app, &who, json!({})).await;
        let chat = Uuid::parse_str(&id_of(&created)).unwrap();
        let other = Uuid::parse_str(&id_of(&create(&app, &who, json!({})).await)).unwrap();

        let fresh = seed_attachment(&app, chat, tenant, user_id, None, false).await;
        let done = seed_attachment(&app, chat, tenant, user_id, Some("done"), false).await;
        let removed = seed_attachment(&app, chat, tenant, user_id, None, true).await;
        let elsewhere = seed_attachment(&app, other, tenant, user_id, None, false).await;

        let res = app
            .call("DELETE", &format!("{CHATS}/{chat}"), &who, None)
            .await;
        assert_eq!(res.status, 204, "{}", res.json);

        let row = attachment_row(&app, fresh).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
        assert!(row.cleanup_updated_at.is_some());
        let row = attachment_row(&app, done).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("done"), "left alone");
        assert_eq!(attachment_row(&app, removed).await.cleanup_status, None);
        assert_eq!(attachment_row(&app, elsewhere).await.cleanup_status, None);

        TestApp::wait_until("chat cleanup delivered", || async {
            !app.outbox_payloads(CHAT_CLEANUP_QUEUE).is_empty()
        })
        .await;
        let payloads = app.outbox_payloads(CHAT_CLEANUP_QUEUE);
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        assert_eq!(payloads[0]["reason"], "chat_soft_delete");
        assert_eq!(payloads[0]["chat_id"], chat.to_string());
        assert_eq!(payloads[0]["tenant_id"], tenant.to_string());

        // A second delete is a 404 and enqueues nothing. Deleting another chat afterwards and
        // waiting for its event is the barrier: the delivered events are exactly the two chats'.
        let res = app
            .call("DELETE", &format!("{CHATS}/{chat}"), &who, None)
            .await;
        assert_eq!(res.status, 404);
        let res = app
            .call("DELETE", &format!("{CHATS}/{other}"), &who, None)
            .await;
        assert_eq!(res.status, 204, "{}", res.json);
        TestApp::wait_until("second chat cleanup delivered", || async {
            app.outbox_payloads(CHAT_CLEANUP_QUEUE).len() >= 2
        })
        .await;
        let mut chat_ids: Vec<String> = app
            .outbox_payloads(CHAT_CLEANUP_QUEUE)
            .iter()
            .map(|p| p["chat_id"].as_str().unwrap().to_owned())
            .collect();
        chat_ids.sort();
        let mut expected = vec![chat.to_string(), other.to_string()];
        expected.sort();
        assert_eq!(chat_ids, expected);
    }

    #[tokio::test]
    async fn message_count_ignores_soft_deleted_messages() {
        let app = TestApp::builder().build().await;
        let (tenant, user_id) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user_id);
        let chat = Uuid::parse_str(&id_of(&create(&app, &who, json!({})).await)).unwrap();
        let empty = id_of(&create(&app, &who, json!({})).await);

        for deleted in [false, false, true] {
            let now = db_now();
            let am = messages::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(tenant),
                chat_id: Set(chat),
                request_id: Set(None),
                role: Set("user".into()),
                content: Set("hi".into()),
                created_at: Set(now),
                deleted_at: Set(deleted.then_some(now)),
                content_type: NotSet,
                token_estimate: NotSet,
                provider_response_id: NotSet,
                request_kind: NotSet,
                features_used: NotSet,
                input_tokens: NotSet,
                output_tokens: NotSet,
                cache_read_input_tokens: NotSet,
                cache_write_input_tokens: NotSet,
                reasoning_tokens: NotSet,
                model: NotSet,
                is_compressed: NotSet,
            };
            let conn = app.db.conn().unwrap();
            secure_insert::<messages::Entity>(am, &AccessScope::allow_all(), &conn)
                .await
                .unwrap();
        }

        let got = app
            .call("GET", &format!("{CHATS}/{chat}"), &who, None)
            .await;
        assert_eq!(got.json["message_count"], 2);
        let list = app.call("GET", CHATS, &who, None).await;
        let counts: Vec<(String, i64)> = list.json["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["id"].as_str().unwrap().to_owned(),
                    i["message_count"].as_i64().unwrap(),
                )
            })
            .collect();
        assert!(counts.contains(&(chat.to_string(), 2)), "{counts:?}");
        assert!(counts.contains(&(empty, 0)), "{counts:?}");
    }

    #[tokio::test]
    async fn list_orders_by_activity_and_paginates() {
        let app = TestApp::builder().build().await;
        let who = user();
        let first = create(&app, &who, json!({ "title": "one" })).await;
        let second = create(&app, &who, json!({ "title": "two x" })).await;
        let third = create(&app, &who, json!({ "title": "three" })).await;
        let (first_id, second_id, third_id) = (id_of(&first), id_of(&second), id_of(&third));

        // Someone else's chats never show up.
        let stranger = user();
        create(&app, &stranger, json!({ "title": "foreign x" })).await;

        let renamed = app
            .call(
                "PATCH",
                &format!("{CHATS}/{first_id}"),
                &who,
                Some(json!({ "title": "one renamed" })),
            )
            .await;
        assert_eq!(renamed.status, 200, "{}", renamed.json);

        let all = app.call("GET", CHATS, &who, None).await;
        assert_eq!(all.status, 200, "{}", all.json);
        assert_eq!(
            item_ids(&all.json),
            [first_id.as_str(), third_id.as_str(), second_id.as_str()]
        );
        assert_eq!(all.json["page_info"]["limit"], 20);
        assert_eq!(all.json["items"][0]["title"], "one renamed");
        assert_eq!(all.json["items"][0]["message_count"], 0);

        // Pagination forward.
        let page1 = app
            .call("GET", &format!("{CHATS}?limit=2"), &who, None)
            .await;
        assert_eq!(
            item_ids(&page1.json),
            [first_id.as_str(), third_id.as_str()]
        );
        let next = page1.json["page_info"]["next_cursor"]
            .as_str()
            .expect("next_cursor")
            .to_owned();
        assert!(page1.json["page_info"]["prev_cursor"].is_null());
        let page2 = app
            .call("GET", &format!("{CHATS}?limit=2&cursor={next}"), &who, None)
            .await;
        assert_eq!(page2.status, 200, "{}", page2.json);
        assert_eq!(item_ids(&page2.json), [second_id.as_str()]);
        assert!(page2.json["page_info"]["next_cursor"].is_null());
        assert!(page2.json["page_info"]["prev_cursor"].is_string());

        // Limit clamping and rejection.
        let big = app
            .call("GET", &format!("{CHATS}?limit=500"), &who, None)
            .await;
        assert_eq!(big.json["page_info"]["limit"], 100);
        let zero = app
            .call("GET", &format!("{CHATS}?limit=0"), &who, None)
            .await;
        assert_eq!(zero.status, 400, "{}", zero.json);
        assert_eq!(violation(&zero).1, "INVALID_LIMIT");
        assert_eq!(zero.json["context"]["resource_type"], ODATA_RESOURCE);

        // Filters.
        let res = app
            .call(
                "GET",
                &format!("{CHATS}?$filter=contains(title,'x')"),
                &who,
                None,
            )
            .await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(item_ids(&res.json), [second_id.as_str()]);

        let after = second.json["updated_at"].as_str().unwrap();
        let res = app
            .call(
                "GET",
                &format!("{CHATS}?$filter=updated_at%20gt%20{after}"),
                &who,
                None,
            )
            .await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(item_ids(&res.json), [first_id.as_str(), third_id.as_str()]);
        let res = app
            .call(
                "GET",
                &format!("{CHATS}?$filter=updated_at%20le%20{after}"),
                &who,
                None,
            )
            .await;
        assert_eq!(item_ids(&res.json), [second_id.as_str()]);

        // Explicit ordering.
        let res = app
            .call("GET", &format!("{CHATS}?$orderby=title%20asc"), &who, None)
            .await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(
            item_ids(&res.json),
            [first_id.as_str(), third_id.as_str(), second_id.as_str()]
        );
        let titles: Vec<&str> = res.json["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, ["one renamed", "three", "two x"]);

        // Ordering by a datetime survives a cursor round trip.
        let p1 = app
            .call(
                "GET",
                &format!("{CHATS}?$orderby=updated_at%20asc&limit=2"),
                &who,
                None,
            )
            .await;
        assert_eq!(item_ids(&p1.json), [second_id.as_str(), third_id.as_str()]);
        let next = p1.json["page_info"]["next_cursor"].as_str().unwrap();
        let p2 = app
            .call("GET", &format!("{CHATS}?limit=2&cursor={next}"), &who, None)
            .await;
        assert_eq!(p2.status, 200, "{}", p2.json);
        assert_eq!(item_ids(&p2.json), [first_id.as_str()]);

        // Rejections.
        let res = app
            .call("GET", &format!("{CHATS}?$filter=foo%20eq%201"), &who, None)
            .await;
        assert_eq!(res.status, 400, "{}", res.json);
        assert_eq!(res.json["context"]["resource_type"], ODATA_RESOURCE);
        let res = app
            .call("GET", &format!("{CHATS}?cursor=garbage"), &who, None)
            .await;
        assert_eq!(res.status, 400, "{}", res.json);
        assert_eq!(violation(&res).1, "INVALID_CURSOR");
        let res = app
            .call("GET", &format!("{CHATS}?$skip=1"), &who, None)
            .await;
        assert_eq!(res.status, 400, "{}", res.json);
        assert_eq!(violation(&res).1, "UNSUPPORTED_QUERY_PARAM");

        // The stranger sees only their own chat.
        let theirs = app.call("GET", CHATS, &stranger, None).await;
        assert_eq!(theirs.json["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn datetime_filters_compose_and_survive_cursors() {
        let app = TestApp::builder().build().await;
        let who = user();
        let mut ids = Vec::new();
        let mut stamps = Vec::new();
        for n in 0..3 {
            let res = create(&app, &who, json!({ "title": format!("c{n}") })).await;
            ids.push(id_of(&res));
            stamps.push(res.json["updated_at"].as_str().unwrap().to_owned());
        }
        let list = |query: String| {
            let app = &app;
            let who = &who;
            async move {
                app.call("GET", &format!("{CHATS}?{query}"), who, None)
                    .await
            }
        };

        // `in`, `or` and `not` over datetimes.
        let res = list(format!(
            "$filter=updated_at%20in%20({},{})&$orderby=updated_at%20asc",
            stamps[0], stamps[2]
        ))
        .await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(item_ids(&res.json), [ids[0].as_str(), ids[2].as_str()]);

        let res = list(format!(
            "$filter=updated_at%20lt%20{}%20or%20updated_at%20gt%20{}&$orderby=updated_at%20asc",
            stamps[1], stamps[1]
        ))
        .await;
        assert_eq!(item_ids(&res.json), [ids[0].as_str(), ids[2].as_str()]);

        let res = list(format!(
            "$filter=not%20(updated_at%20eq%20{})&$orderby=updated_at%20asc",
            stamps[1]
        ))
        .await;
        assert_eq!(item_ids(&res.json), [ids[0].as_str(), ids[2].as_str()]);

        // A cursor continues the filtered list, and refuses a different filter.
        let filter = format!("$filter=updated_at%20ge%20{}", stamps[0]);
        let p1 = list(format!("{filter}&limit=2")).await;
        assert_eq!(item_ids(&p1.json), [ids[2].as_str(), ids[1].as_str()]);
        let next = p1.json["page_info"]["next_cursor"].as_str().unwrap();
        let p2 = list(format!("{filter}&limit=2&cursor={next}")).await;
        assert_eq!(p2.status, 200, "{}", p2.json);
        assert_eq!(item_ids(&p2.json), [ids[0].as_str()]);
        let other = list(format!("$filter=contains(title,'c')&limit=2&cursor={next}")).await;
        assert_eq!(other.status, 400, "{}", other.json);
        assert_eq!(violation(&other).1, "FILTER_MISMATCH");

        // A malformed datetime is an OData error.
        let res = list("$filter=updated_at%20gt%20yesterday".to_owned()).await;
        assert_eq!(res.status, 400, "{}", res.json);
        assert_eq!(res.json["context"]["resource_type"], ODATA_RESOURCE);
    }

    /// Follows `next_cursor` from the first page of `first_uri` (continuations are
    /// `{CHATS}?{follow}&cursor=..`, a cursor excludes `$orderby`), returning the ids in the
    /// order visited and the last page's response.
    async fn walk_forward(
        app: &TestApp,
        who: &SecurityContext,
        first_uri: &str,
        follow: &str,
    ) -> (Vec<String>, TestResponse) {
        let mut ids = Vec::new();
        let mut res = app.call("GET", first_uri, who, None).await;
        loop {
            assert_eq!(res.status, 200, "{}", res.json);
            ids.extend(item_ids(&res.json).into_iter().map(str::to_owned));
            let Some(next) = res.json["page_info"]["next_cursor"].as_str() else {
                return (ids, res);
            };
            assert!(ids.len() <= 50, "pagination does not terminate");
            res = app
                .call("GET", &format!("{CHATS}?{follow}&cursor={next}"), who, None)
                .await;
        }
    }

    #[tokio::test]
    async fn id_filter_accepts_quoted_and_unquoted_uuids() {
        let app = TestApp::builder().build().await;
        let who = user();
        let mine = id_of(&create(&app, &who, json!({"title": "a"})).await);
        let other = id_of(&create(&app, &who, json!({"title": "b"})).await);
        let list = |filter: String| {
            let (app, who) = (&app, &who);
            async move {
                app.call("GET", &format!("{CHATS}?$filter={filter}"), who, None)
                    .await
            }
        };

        let unquoted = list(format!("id%20eq%20{mine}")).await;
        let quoted = list(format!("id%20eq%20'{mine}'")).await;
        assert_eq!(quoted.status, 200, "{}", quoted.json);
        assert_eq!(item_ids(&quoted.json), [mine.as_str()]);
        assert_eq!(item_ids(&unquoted.json), item_ids(&quoted.json));
        let both = list(format!("id%20in%20('{mine}','{other}')")).await;
        assert_eq!(item_ids(&both.json).len(), 2, "{}", both.json);

        let bad = list("id%20eq%20'not-a-uuid'".to_owned()).await;
        assert_eq!(bad.status, 400, "{}", bad.json);
        assert_eq!(bad.json["context"]["resource_type"], ODATA_RESOURCE);
        assert_eq!(violation(&bad).1, "INVALID_FILTER");
    }

    #[tokio::test]
    async fn title_order_pages_through_untitled_chats_exactly_once() {
        let app = TestApp::builder().build().await;
        let who = user();
        for title in [Some("b"), None, Some("a"), None, Some("c")] {
            let body = title.map_or(json!({}), |t| json!({ "title": t }));
            create(&app, &who, body).await;
        }

        for dir in ["asc", "desc"] {
            let all = app
                .call(
                    "GET",
                    &format!("{CHATS}?$orderby=title%20{dir}"),
                    &who,
                    None,
                )
                .await;
            assert_eq!(all.status, 200, "{}", all.json);
            let titles: Vec<&str> = all.json["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["title"].as_str().unwrap_or(""))
                .collect();
            // Untitled chats sort as the empty string.
            let expected: &[&str] = if dir == "asc" {
                &["", "", "a", "b", "c"]
            } else {
                &["c", "b", "a", "", ""]
            };
            assert_eq!(titles, expected, "{dir}");
            let expected_ids: Vec<String> =
                item_ids(&all.json).into_iter().map(str::to_owned).collect();

            // Forward, one chat per page: every chat exactly once, in the same order.
            let uri = format!("{CHATS}?$orderby=title%20{dir}&limit=1");
            let (visited, last) = walk_forward(&app, &who, &uri, "limit=1").await;
            assert_eq!(visited, expected_ids, "{dir} forward");

            // Backward from the last page via `prev_cursor`.
            let mut back = vec![item_ids(&last.json)[0].to_owned()];
            let mut prev = last.json["page_info"]["prev_cursor"]
                .as_str()
                .map(str::to_owned);
            while let Some(cursor) = prev {
                let res = app
                    .call(
                        "GET",
                        &format!("{CHATS}?limit=1&cursor={cursor}"),
                        &who,
                        None,
                    )
                    .await;
                assert_eq!(res.status, 200, "{dir} backward: {}", res.json);
                back.push(item_ids(&res.json)[0].to_owned());
                assert!(back.len() <= 50, "backward pagination does not terminate");
                prev = res.json["page_info"]["prev_cursor"]
                    .as_str()
                    .map(str::to_owned);
            }
            back.reverse();
            assert_eq!(back, expected_ids, "{dir} backward");
        }
    }

    /// API timestamps carry at most microseconds (the storage artefact of `ts::normalize`, the
    /// trailing nanosecond, never leaks) and a create response agrees with later reads.
    #[tokio::test]
    async fn api_timestamps_are_microsecond_precision_and_stable_across_reads() {
        let app = TestApp::builder().build().await;
        let who = user();
        let created = create(&app, &who, json!({"title": "t"})).await;
        let id = id_of(&created);
        let got = app.call("GET", &format!("{CHATS}/{id}"), &who, None).await;
        let listed = app.call("GET", CHATS, &who, None).await;
        for field in ["created_at", "updated_at"] {
            let stamp = created.json[field].as_str().unwrap();
            let fraction = stamp
                .split_once('.')
                .map_or("", |(_, f)| f.trim_end_matches('Z'));
            assert!(fraction.len() <= 6, "{field}: {stamp}");
            OffsetDateTime::parse(stamp, &Rfc3339).unwrap();
            assert_eq!(got.json[field], stamp, "{field}: GET");
            assert_eq!(listed.json["items"][0][field], stamp, "{field}: list");
        }
    }

    #[tokio::test]
    async fn timestamps_within_one_second_order_and_filter_correctly() {
        use crate::infra::db::{repo, ts::normalize};
        use time::macros::datetime;

        let app = TestApp::builder().build().await;
        let (tenant, user_id) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user_id);
        let base = datetime!(2026-10-04 12:00:00 UTC);
        // Fractions whose unnormalised encoding would trim to ".5Z", ".55Z", ".05Z", ... and
        // therefore sort wrongly as text.
        let fractions_ms = [500, 550, 50, 0, 5, 505, 999];
        let mut chats = Vec::new();
        for (n, ms) in fractions_ms.into_iter().enumerate() {
            let at = normalize(base + time::Duration::milliseconds(ms));
            let id = Uuid::new_v4();
            let am = crate::infra::db::entity::chats::ActiveModel {
                id: Set(id),
                tenant_id: Set(tenant),
                user_id: Set(user_id),
                model: Set("gpt-premium".into()),
                title: Set(Some(format!("t{n}"))),
                is_temporary: Set(false),
                created_at: Set(at),
                updated_at: Set(at),
                deleted_at: Set(None),
            };
            let conn = app.db.conn().unwrap();
            repo::chats::insert(&conn, &AccessScope::allow_all(), am)
                .await
                .unwrap();
            chats.push((ms, id.to_string()));
        }
        chats.sort_by_key(|(ms, _)| *ms);
        let ascending: Vec<String> = chats.iter().map(|(_, id)| id.clone()).collect();
        let descending: Vec<String> = ascending.iter().rev().cloned().collect();

        let (visited, _) = walk_forward(
            &app,
            &who,
            &format!("{CHATS}?$orderby=updated_at%20asc&limit=2"),
            "limit=2",
        )
        .await;
        assert_eq!(visited, ascending);
        let (visited, _) = walk_forward(&app, &who, &format!("{CHATS}?limit=3"), "limit=3").await;
        assert_eq!(visited, descending, "default order is updated_at desc");

        // Cutoffs given with trailing zeros (".5", ".55") compare like the stored values.
        for cutoff_ms in [500, 550, 5, 999] {
            let cutoff = normalize(base + time::Duration::milliseconds(cutoff_ms))
                .format(&Rfc3339)
                .unwrap();
            let res = app
                .call(
                    "GET",
                    &format!(
                        "{CHATS}?$filter=updated_at%20gt%20{cutoff}&$orderby=updated_at%20asc"
                    ),
                    &who,
                    None,
                )
                .await;
            assert_eq!(res.status, 200, "{}", res.json);
            let expected: Vec<&str> = chats
                .iter()
                .filter(|(ms, _)| *ms > cutoff_ms)
                .map(|(_, id)| id.as_str())
                .collect();
            assert_eq!(item_ids(&res.json), expected, "gt .{cutoff_ms}");
        }
        // A cutoff written with fewer digits than the stored value still lines up.
        let res = app
            .call(
                "GET",
                &format!("{CHATS}?$filter=updated_at%20eq%202026-10-04T12:00:00.55Z"),
                &who,
                None,
            )
            .await;
        assert_eq!(
            item_ids(&res.json),
            [chats.iter().find(|(ms, _)| *ms == 550).unwrap().1.as_str()]
        );
    }

    #[tokio::test]
    async fn deleted_chats_are_not_listed() {
        let app = TestApp::builder().build().await;
        let who = user();
        let keep = id_of(&create(&app, &who, json!({})).await);
        let drop = id_of(&create(&app, &who, json!({})).await);
        let res = app
            .call("DELETE", &format!("{CHATS}/{drop}"), &who, None)
            .await;
        assert_eq!(res.status, 204);
        let list = app.call("GET", CHATS, &who, None).await;
        assert_eq!(item_ids(&list.json), [keep.as_str()]);
    }
}
