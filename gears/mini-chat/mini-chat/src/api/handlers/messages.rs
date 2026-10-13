//! Message list handler: `mini_chat.list_messages`.

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::dto::messages::MiniChatMessageDto;
use crate::api::state::AppServices;

/// `mini_chat.list_messages`: the live messages of the chat, chronological unless ordered.
///
/// # Errors
/// 400 for an invalid `$filter`, `$orderby`, `limit` or `cursor`, 404 for a missing, deleted or
/// foreign chat, 403 / 503 from the PDP.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path(id): extract::Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<Page<MiniChatMessageDto>>> {
    let page = svc.messages.list(&ctx, id, &query).await?;
    Ok(Json(page.map_items(MiniChatMessageDto::from)))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use crate::test_support::app::{TestApp, TestResponse, ctx};
    use crate::test_support::stream::{
        CHATS, SeedAttachment, answer, create_chat, script_provider, seed_attachment,
        soft_delete_attachment, stream_uri,
    };

    const ODATA_RESOURCE: &str = "gts.cf.core.odata.query.v1~";
    const CHAT_RESOURCE: &str = "gts.cf.core.mini_chat.chat.v1~";

    struct Caller {
        tenant: Uuid,
        user: Uuid,
        who: SecurityContext,
    }

    fn new_user() -> Caller {
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        Caller {
            tenant,
            user,
            who: ctx(tenant, user),
        }
    }

    fn messages_uri(chat: Uuid) -> String {
        format!("{CHATS}/{chat}/messages")
    }

    async fn list(app: &TestApp, u: &Caller, chat: Uuid, query: &str) -> TestResponse {
        let uri = if query.is_empty() {
            messages_uri(chat)
        } else {
            format!("{}?{query}", messages_uri(chat))
        };
        app.call("GET", &uri, &u.who, None).await
    }

    async fn send(app: &TestApp, u: &Caller, chat: Uuid, body: Value) {
        let frames = app
            .stream("POST", &stream_uri(chat), &u.who, body)
            .await
            .unwrap_or_else(|r| panic!("send rejected: {} {}", r.status, r.json));
        assert_eq!(frames.last().expect("frames").event, "done");
    }

    /// A chat with two completed turns ("q1" then "q2"); returns the chat id.
    async fn two_turns(app: &TestApp, u: &Caller) -> Uuid {
        let chat = create_chat(app, &u.who, None).await;
        script_provider(app, answer(&["Hel", "lo"], 12, 5));
        for q in ["q1", "q2"] {
            send(app, u, chat, json!({"content": q})).await;
        }
        chat
    }

    fn items(res: &TestResponse) -> &Vec<Value> {
        res.json["items"].as_array().expect("items")
    }

    fn ids(res: &TestResponse) -> Vec<String> {
        items(res)
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn messages_list_contract_and_counts() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let chat = two_turns(&app, &u).await;

        let res = list(&app, &u, chat, "").await;

        assert_eq!(res.status, 200, "{}", res.json);
        let roles: Vec<_> = items(&res)
            .iter()
            .map(|i| i["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        let contents: Vec<_> = items(&res)
            .iter()
            .map(|i| i["content"].as_str().unwrap())
            .collect();
        assert_eq!(contents, ["q1", "Hello", "q2", "Hello"]);
        for item in items(&res) {
            item["request_id"]
                .as_str()
                .expect("request_id")
                .parse::<Uuid>()
                .unwrap();
            assert_eq!(item["attachments"], json!([]), "{item}");
            assert!(
                item.as_object().unwrap().contains_key("my_reaction")
                    && item["my_reaction"].is_null(),
                "{item}"
            );
            chrono_like(&item["created_at"]);
            if item["role"] == "assistant" {
                assert_eq!(item["model"], "gpt-premium", "{item}");
                assert_eq!(item["input_tokens"], 12, "{item}");
                assert_eq!(item["output_tokens"], 5, "{item}");
            } else {
                for absent in ["model", "input_tokens", "output_tokens"] {
                    assert!(item.get(absent).is_none(), "{absent} in {item}");
                }
            }
        }
        // A user message and its answer share the request id.
        assert_eq!(items(&res)[0]["request_id"], items(&res)[1]["request_id"]);
        assert_ne!(items(&res)[0]["request_id"], items(&res)[2]["request_id"]);
        assert_eq!(res.json["page_info"]["limit"], 20);

        let chat_res = app
            .call("GET", &format!("{CHATS}/{chat}"), &u.who, None)
            .await;
        assert_eq!(chat_res.json["message_count"], 4);
    }

    fn chrono_like(value: &Value) {
        let text = value.as_str().expect("created_at");
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .expect("RFC 3339 created_at");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one scenario over every OData feature of the list
    async fn messages_list_odata() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let chat = two_turns(&app, &u).await;
        let all = ids(&list(&app, &u, chat, "").await);
        assert_eq!(all.len(), 4);

        let assistants = list(&app, &u, chat, "$filter=role%20eq%20'assistant'").await;
        assert_eq!(assistants.status, 200, "{}", assistants.json);
        assert_eq!(items(&assistants).len(), 2);
        assert!(items(&assistants).iter().all(|i| i["role"] == "assistant"));

        let by_id = list(&app, &u, chat, &format!("$filter=id%20eq%20{}", all[1])).await;
        assert_eq!(ids(&by_id), [all[1].clone()]);
        // The documented form quotes the UUID; it selects the same message.
        let quoted = list(&app, &u, chat, &format!("$filter=id%20eq%20'{}'", all[1])).await;
        assert_eq!(quoted.status, 200, "{}", quoted.json);
        assert_eq!(ids(&quoted), ids(&by_id));
        let quoted_in = list(
            &app,
            &u,
            chat,
            &format!("$filter=id%20in%20('{}',%20{})", all[0], all[3]),
        )
        .await;
        assert_eq!(
            ids(&quoted_in),
            [all[0].clone(), all[3].clone()],
            "{}",
            quoted_in.json
        );
        let quoted_ne = list(&app, &u, chat, &format!("$filter=id%20ne%20'{}'", all[1])).await;
        assert_eq!(ids(&quoted_ne).len(), 3, "{}", quoted_ne.json);
        let not_uuid = list(&app, &u, chat, "$filter=id%20eq%20'not-a-uuid'").await;
        assert_eq!(not_uuid.status, 400, "{}", not_uuid.json);
        assert_eq!(not_uuid.json["context"]["resource_type"], ODATA_RESOURCE);
        assert_eq!(
            not_uuid.json["context"]["field_violations"][0]["reason"],
            "INVALID_FILTER"
        );
        // A quoted UUID on a string field stays a plain string.
        let by_role = list(&app, &u, chat, &format!("$filter=role%20eq%20'{}'", all[1])).await;
        assert_eq!(by_role.status, 200, "{}", by_role.json);
        assert!(items(&by_role).is_empty());
        // A cursor issued under the quoted filter continues it.
        let filter = format!("$filter=id%20in%20('{}','{}')", all[0], all[2]);
        let first = list(&app, &u, chat, &format!("{filter}&limit=1")).await;
        let next = first.json["page_info"]["next_cursor"]
            .as_str()
            .expect("next cursor");
        let rest = list(&app, &u, chat, &format!("{filter}&limit=1&cursor={next}")).await;
        assert_eq!(ids(&first), [all[0].clone()]);
        assert_eq!(ids(&rest), [all[2].clone()], "{}", rest.json);

        let desc = list(&app, &u, chat, "$orderby=created_at%20desc").await;
        let mut reversed = all.clone();
        reversed.reverse();
        assert_eq!(ids(&desc), reversed);

        // `created_at` filters bind stored-shape datetimes.
        let second = items(&list(&app, &u, chat, "").await)[1]["created_at"]
            .as_str()
            .unwrap()
            .to_owned();
        let after = list(
            &app,
            &u,
            chat,
            &format!("$filter=created_at%20gt%20{second}"),
        )
        .await;
        assert_eq!(ids(&after), all[2..].to_vec(), "{}", after.json);
        let upto = list(
            &app,
            &u,
            chat,
            &format!("$filter=created_at%20le%20{second}"),
        )
        .await;
        assert_eq!(ids(&upto), all[..2].to_vec(), "{}", upto.json);

        // limit=1 cursor walk visits every message once, in order, for both directions.
        for (order, expected) in [("asc", all.clone()), ("desc", reversed.clone())] {
            let mut seen = Vec::new();
            let mut query = format!("limit=1&$orderby=created_at%20{order}");
            for _ in 0..5 {
                let page = list(&app, &u, chat, &query).await;
                assert_eq!(page.status, 200, "{}", page.json);
                seen.extend(ids(&page));
                match page.json["page_info"]["next_cursor"].as_str() {
                    // The cursor carries the order; `$orderby` next to it is rejected.
                    Some(next) => query = format!("limit=1&cursor={next}"),
                    None => break,
                }
            }
            assert_eq!(seen, expected, "{order}");
        }
        // Default order cursor walk with a filter-free list, then the way back.
        let first = list(&app, &u, chat, "limit=2").await;
        let next = first.json["page_info"]["next_cursor"]
            .as_str()
            .expect("next");
        let second_page = list(&app, &u, chat, &format!("limit=2&cursor={next}")).await;
        assert_eq!(ids(&second_page), all[2..].to_vec());
        assert!(second_page.json["page_info"]["next_cursor"].is_null());
        let prev = second_page.json["page_info"]["prev_cursor"]
            .as_str()
            .expect("prev");
        let back = list(&app, &u, chat, &format!("limit=2&cursor={prev}")).await;
        assert_eq!(ids(&back), all[..2].to_vec());

        // `$select` is accepted and ignored.
        let selected = list(&app, &u, chat, "$select=id").await;
        assert_eq!(selected.status, 200, "{}", selected.json);
        assert_eq!(items(&selected).len(), 4);
        assert!(items(&selected)[0].get("content").is_some());

        let bad_order = list(&app, &u, chat, "$orderby=foo").await;
        assert_eq!(bad_order.status, 400, "{}", bad_order.json);
        assert_eq!(bad_order.json["context"]["resource_type"], ODATA_RESOURCE);
        let bad_cursor = list(&app, &u, chat, "cursor=bad").await;
        assert_eq!(bad_cursor.status, 400, "{}", bad_cursor.json);
        assert_eq!(bad_cursor.json["context"]["resource_type"], ODATA_RESOURCE);

        // Another user's chat is a 404 of the chat resource.
        let stranger = new_user();
        let foreign = list(&app, &stranger, chat, "").await;
        assert_eq!(foreign.status, 404, "{}", foreign.json);
        assert_eq!(foreign.json["context"]["resource_type"], CHAT_RESOURCE);
    }

    #[tokio::test]
    async fn message_attachments_listed_without_deleted() {
        let app = TestApp::builder().build().await;
        let u = new_user();
        let chat = create_chat(&app, &u.who, None).await;
        let image = seed_attachment(
            &app,
            SeedAttachment {
                thumbnail: Some((vec![1, 2, 3, 4], 32, 16)),
                ..SeedAttachment::image(u.tenant, chat, u.user)
            },
        )
        .await;
        let document =
            seed_attachment(&app, SeedAttachment::document(u.tenant, chat, u.user)).await;
        script_provider(&app, answer(&["ok"], 10, 2));
        send(
            &app,
            &u,
            chat,
            json!({"content": "look", "attachment_ids": [image, document]}),
        )
        .await;

        let res = list(&app, &u, chat, "").await;
        assert_eq!(res.status, 200, "{}", res.json);
        let user_message = &items(&res)[0];
        assert_eq!(user_message["role"], "user");
        let mut attachments = user_message["attachments"].as_array().unwrap().clone();
        attachments.sort_by_key(|a| a["kind"].as_str().unwrap().to_owned());
        assert_eq!(attachments.len(), 2, "{attachments:?}");
        assert_eq!(
            attachments[0],
            json!({"attachment_id": document, "kind": "document", "filename": "report.pdf",
                   "status": "ready"}),
        );
        assert_eq!(
            attachments[1],
            json!({"attachment_id": image, "kind": "image", "filename": "photo.png",
                   "status": "ready",
                   "img_thumbnail": {"content_type": "image/webp", "width": 32, "height": 16,
                                     "data_base64": "AQIDBA=="}}),
        );
        assert_eq!(
            items(&res)[1]["attachments"],
            json!([]),
            "assistant message"
        );
        let wire = res.json.to_string();
        assert!(!wire.contains("file-"), "provider file id leaked: {wire}");

        soft_delete_attachment(&app, document).await;
        let res = list(&app, &u, chat, "").await;
        let remaining = items(&res)[0]["attachments"].as_array().unwrap().clone();
        assert_eq!(remaining.len(), 1, "{remaining:?}");
        assert_eq!(remaining[0]["attachment_id"], json!(image));
    }
}
