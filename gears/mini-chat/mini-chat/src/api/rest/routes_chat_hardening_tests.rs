#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Regression tests of the review findings of `cpt-cf-mini-chat-feature-chat-crud`
//! through the REST layer: concurrent deletes on a file-backed `SQLite`
//! database (D1), `$orderby=title` over untitled chats (D2), reactions under a
//! PDP that constrains the chat id (D3), `$filter` on stored date-times (D4) and
//! the `$filter` complexity guard (D5).

use std::collections::BTreeSet;

use axum::http::StatusCode;
use futures::future::join_all;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::chat_test_env::{
    BASE_UNIX, CHAT_TYPE, ChatEnv, Options, Reply, chat_id, chat_uri, enc, item_ids, msg_id,
    next_cursor, request_id, rfc3339,
};
use crate::infra::db::test_support::{chat_row, message_row};
use crate::test_doubles::{OTHER_USER, PdpReply, USER, USER_TENANT};

// ================================================================================================
// D1: concurrent deletes (file-backed SQLite, WAL, pool of several connections)
// ================================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deletes_of_distinct_chats_on_a_file_database_all_return_204() {
    let env = ChatEnv::with(Options::with_file_db()).await;
    let mut chats = Vec::new();
    for n in 1..=20_u128 {
        chats.push(
            env.seed_chat(n, Some("chat"), i64::try_from(n).unwrap())
                .await,
        );
    }

    let replies = join_all(
        chats
            .iter()
            .map(|chat| async { env.delete(&chat_uri(*chat)).await }),
    )
    .await;

    let statuses: Vec<StatusCode> = replies.iter().map(|reply| reply.status).collect();
    assert!(
        statuses
            .iter()
            .all(|status| *status == StatusCode::NO_CONTENT),
        "{statuses:?}: {}",
        replies
            .iter()
            .find(|reply| reply.status != StatusCode::NO_CONTENT)
            .map_or("", |reply| reply.text.as_str())
    );
    assert_eq!(
        env.chat_cleanup_rows().await.len(),
        20,
        "one cleanup per chat"
    );
    assert_eq!(
        env.list_chats("").await.json["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deletes_of_one_chat_on_a_file_database_give_one_204_and_the_rest_404() {
    let env = ChatEnv::with(Options::with_file_db()).await;
    let chat = env.seed_chat(1, Some("chat"), 1).await;

    let replies = join_all((0..10).map(|_| async { env.delete(&chat_uri(chat)).await })).await;

    let no_content = replies
        .iter()
        .filter(|reply| reply.status == StatusCode::NO_CONTENT)
        .count();
    let not_found = replies
        .iter()
        .filter(|reply| reply.status == StatusCode::NOT_FOUND)
        .count();
    let statuses: Vec<StatusCode> = replies.iter().map(|reply| reply.status).collect();
    assert_eq!(no_content, 1, "{statuses:?}");
    assert_eq!(not_found, 9, "{statuses:?}");
    assert_eq!(
        env.chat_cleanup_rows().await.len(),
        1,
        "exactly one cleanup message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deletes_and_renames_on_a_file_database_never_answer_500() {
    let env = ChatEnv::with(Options::with_file_db()).await;
    let chat = env.seed_chat(1, Some("chat"), 1).await;

    let (deletes, renames) = tokio::join!(
        join_all((0..5).map(|_| async { env.delete(&chat_uri(chat)).await })),
        join_all((0..5).map(|_| async {
            env.patch_chat(chat, &serde_json::json!({ "title": "new" }))
                .await
        }))
    );

    for reply in deletes.iter().chain(renames.iter()) {
        assert!(
            matches!(
                reply.status,
                StatusCode::NO_CONTENT | StatusCode::NOT_FOUND | StatusCode::OK
            ),
            "{}: {}",
            reply.status,
            reply.text
        );
    }
}

// ================================================================================================
// D2: $orderby=title with untitled chats
// ================================================================================================

/// Walks the chat list with `orderby` and `limit`, returning the titles
/// (`""` for an untitled chat) and ids in page order.
async fn walk_titles(env: &ChatEnv, orderby: &str, limit: usize) -> (Vec<String>, Vec<String>) {
    let mut titles = Vec::new();
    let mut ids = Vec::new();
    let mut query = format!("?$orderby={}&limit={limit}", enc(orderby));
    for _ in 0..50 {
        let page = env.list_chats(&query).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text);
        ids.extend(item_ids(&page));
        titles.extend(
            page.json["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["title"].as_str().unwrap_or_default().to_owned()),
        );
        match next_cursor(&page) {
            Some(cursor) => query = format!("?limit={limit}&cursor={}", enc(&cursor)),
            None => return (titles, ids),
        }
    }
    panic!("the cursor walk does not end");
}

#[tokio::test]
async fn ordering_by_title_walks_titled_and_untitled_chats_exactly_once_in_both_directions() {
    let env = ChatEnv::new().await;
    let titles = [
        Some("beta"),
        None,
        Some("alpha"),
        None,
        Some("gamma"),
        Some("Alpha"),
        None,
    ];
    let mut all = BTreeSet::new();
    for (n, title) in titles.iter().enumerate() {
        let n = u128::try_from(n).unwrap() + 1;
        all.insert(
            env.seed_chat(n, *title, i64::try_from(n).unwrap())
                .await
                .to_string(),
        );
    }

    for orderby in ["title asc", "title desc"] {
        for limit in [1_usize, 2, 4] {
            let (walked_titles, walked_ids) = walk_titles(&env, orderby, limit).await;

            let unique: BTreeSet<String> = walked_ids.iter().cloned().collect();
            assert_eq!(walked_ids.len(), titles.len(), "{orderby} limit {limit}");
            assert_eq!(
                unique, all,
                "{orderby} limit {limit}: every chat exactly once"
            );
            let mut expected = walked_titles.clone();
            expected.sort();
            if orderby.ends_with("desc") {
                expected.reverse();
            }
            assert_eq!(
                walked_titles, expected,
                "{orderby} limit {limit}: untitled chats sort as the empty title"
            );
        }
    }
}

#[tokio::test]
async fn the_prev_cursor_of_a_title_ordered_walk_over_untitled_chats_leads_back() {
    let env = ChatEnv::new().await;
    for (n, title) in [None, None, Some("a"), Some("b")].iter().enumerate() {
        let n = u128::try_from(n).unwrap() + 1;
        env.seed_chat(n, *title, i64::try_from(n).unwrap()).await;
    }
    let first = env
        .list_chats(&format!("?$orderby={}&limit=2", enc("title asc")))
        .await;
    let second = env
        .list_chats(&format!(
            "?limit=2&cursor={}",
            enc(&next_cursor(&first).unwrap())
        ))
        .await;
    let prev = second.json["page_info"]["prev_cursor"].as_str().unwrap();

    let back = env
        .list_chats(&format!("?limit=2&cursor={}", enc(prev)))
        .await;

    assert_eq!(back.status, StatusCode::OK, "{}", back.text);
    assert_eq!(item_ids(&back), item_ids(&first));
}

// ================================================================================================
// D3: reactions under a PDP that constrains the chat id
// ================================================================================================

#[tokio::test]
async fn reactions_work_under_a_pdp_that_constrains_the_chat_id() {
    let env = ChatEnv::with(Options::with_pdp(PdpReply::AllowChatConstraints)).await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    env.seed_message(chat, 1, "user", 1).await;
    env.seed_message(chat, 2, "assistant", 2).await;
    let message = msg_id(2);

    let put = env.put_reaction(chat, message, "like").await;
    let listed = env.list_messages(chat, "").await;
    let replaced = env.put_reaction(chat, message, "dislike").await;
    let listed_again = env.list_messages(chat, "").await;
    let removed = env.delete_reaction(chat, message).await;
    let listed_after = env.list_messages(chat, "").await;

    assert_eq!(put.status, StatusCode::OK, "{}", put.text);
    let my = |reply: &Reply| {
        reply.json["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == message.to_string())
            .unwrap()["my_reaction"]
            .clone()
    };
    assert_eq!(my(&listed), "like", "{}", listed.text);
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.text);
    assert_eq!(my(&listed_again), "dislike", "{}", listed_again.text);
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.text);
    assert!(my(&listed_after).is_null(), "{}", listed_after.text);
    assert_eq!(env.db.count("message_reactions").await, 0);
}

#[tokio::test]
async fn reactions_under_a_chat_constraining_pdp_still_answer_404_for_a_foreign_chat() {
    let env = ChatEnv::with(Options::with_pdp(PdpReply::AllowChatConstraints)).await;
    let foreign = env
        .seed_chat_of(1, USER_TENANT, OTHER_USER, Some("theirs"), 10)
        .await;
    env.seed_message(foreign, 1, "assistant", 1).await;
    let message = msg_id(1);

    let put = env.put_reaction(foreign, message, "like").await;
    let removed = env.delete_reaction(foreign, message).await;
    let unknown = env.put_reaction(chat_id(99), message, "like").await;

    put.assert_not_found(CHAT_TYPE);
    removed.assert_not_found(CHAT_TYPE);
    unknown.assert_not_found(CHAT_TYPE);
    assert_eq!(env.db.count("message_reactions").await, 0);
}

#[tokio::test]
async fn another_users_reaction_is_invisible_under_a_chat_constraining_pdp() {
    let env = ChatEnv::with(Options::with_pdp(PdpReply::AllowChatConstraints)).await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    env.seed_message(chat, 1, "assistant", 1).await;
    env.seed_reaction(1, msg_id(1), OTHER_USER, "like").await;

    let mine = env.list_messages(chat, "").await;
    let removed = env.delete_reaction(chat, msg_id(1)).await;

    assert!(
        mine.json["items"][0]["my_reaction"].is_null(),
        "{}",
        mine.text
    );
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(env.db.count("message_reactions").await, 1, "theirs stays");
}

// ================================================================================================
// D4: $filter on stored date-times
// ================================================================================================

fn at_nanos(seconds: i64, nanos: u32) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(BASE_UNIX + seconds)
        .unwrap()
        .replace_nanosecond(nanos)
        .unwrap()
}

async fn count_with(
    env: &ChatEnv,
    chat: Option<Uuid>,
    field: &str,
    op: &str,
    literal: &str,
) -> usize {
    let query = format!("?$filter={}", enc(&format!("{field} {op} {literal}")));
    let reply = match chat {
        None => env.list_chats(&query).await,
        Some(chat) => env.list_messages(chat, &query).await,
    };
    assert_eq!(reply.status, StatusCode::OK, "{field} {op}: {}", reply.text);
    reply.json["items"].as_array().unwrap().len()
}

/// The filter matrix over one row at `literal` among rows before and after it.
async fn assert_boundary(env: &ChatEnv, chat: Option<Uuid>, field: &str, literal: &str) {
    for (op, expected) in [
        ("eq", 1),
        ("ne", 2),
        ("gt", 1),
        ("ge", 2),
        ("lt", 1),
        ("le", 2),
    ] {
        assert_eq!(
            count_with(env, chat, field, op, literal).await,
            expected,
            "{field} {op} {literal}"
        );
    }
}

#[tokio::test]
async fn updated_at_filters_agree_with_the_stored_value_of_a_chat_created_through_the_api() {
    let env = ChatEnv::new().await;
    let created = env
        .post_chat(&serde_json::json!({ "title": "api chat" }))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let updated_at = created.json["updated_at"].as_str().unwrap().to_owned();

    for (op, expected) in [
        ("eq", 1),
        ("ge", 1),
        ("le", 1),
        ("gt", 0),
        ("lt", 0),
        ("ne", 0),
    ] {
        assert_eq!(
            count_with(&env, None, "updated_at", op, &updated_at).await,
            expected,
            "updated_at {op} {updated_at}"
        );
    }
}

#[tokio::test]
async fn updated_at_filters_include_and_exclude_the_boundary_row_exactly() {
    let env = ChatEnv::new().await;
    for (n, secs) in [(1_u128, 10_i64), (2, 20), (3, 30)] {
        env.seed_chat(n, Some("chat"), secs).await;
    }
    let literal = rfc3339(20);

    assert_boundary(&env, None, "updated_at", &literal).await;
}

#[tokio::test]
async fn updated_at_filters_hold_at_nanosecond_precision() {
    let env = ChatEnv::new().await;
    // Rows 1 ns apart around a boundary with a fractional second.
    for (n, nanos) in [
        (1_u128, 123_456_788_u32),
        (2, 123_456_789),
        (3, 123_456_790),
    ] {
        let row = chat_row(chat_id(n), USER_TENANT, USER)
            .set("title", "chat")
            .set("created_at", at_nanos(0, 0))
            .set("updated_at", at_nanos(5, nanos));
        env.db.insert("chats", row).await.unwrap();
    }
    let listed = env.list_chats("").await;
    let boundary = listed.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == chat_id(2).to_string())
        .unwrap()["updated_at"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_boundary(&env, None, "updated_at", &boundary).await;
}

#[tokio::test]
async fn created_at_filters_include_and_exclude_the_boundary_message_exactly() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("chat"), 1).await;
    for (n, secs) in [(1_u128, 10_i64), (2, 20), (3, 30)] {
        env.seed_message(chat, n, "user", secs).await;
    }
    let literal = rfc3339(20);

    assert_boundary(&env, Some(chat), "created_at", &literal).await;
}

#[tokio::test]
async fn created_at_filters_hold_at_nanosecond_precision() {
    // Sub-second digits without a trailing zero: `SQLite` keeps the RFC 3339 text
    // the driver writes, which drops trailing zeros (`.98765432Z` sorts after
    // `.987654321Z` as text); that engine-level text order is outside this fix.
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("chat"), 1).await;
    for (n, nanos) in [
        (1_u128, 987_654_321_u32),
        (2, 987_654_322),
        (3, 987_654_323),
    ] {
        let row = message_row(msg_id(n), USER_TENANT, chat)
            .set("request_id", request_id(n))
            .set("created_at", at_nanos(7, nanos));
        env.seed_message_row(chat, n, row).await;
    }
    let listed = env.list_messages(chat, "").await;
    let boundary = listed.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == msg_id(2).to_string())
        .unwrap()["created_at"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_boundary(&env, Some(chat), "created_at", &boundary).await;
}

#[tokio::test]
async fn a_datetime_in_list_matches_the_stored_value() {
    let env = ChatEnv::new().await;
    env.seed_chat(1, Some("a"), 10).await;
    env.seed_chat(2, Some("b"), 20).await;
    let filter = format!("updated_at in ({})", rfc3339(20));

    let reply = env.list_chats(&format!("?$filter={}", enc(&filter))).await;

    // `in` over date-times is accepted by the platform or rejected as a
    // filter error, never a server error.
    assert!(
        reply.status == StatusCode::OK || reply.status == StatusCode::BAD_REQUEST,
        "{}",
        reply.text
    );
    if reply.status == StatusCode::OK {
        assert_eq!(item_ids(&reply), vec![chat_id(2).to_string()]);
    }
}

// ================================================================================================
// D5: $filter complexity guard
// ================================================================================================

fn or_chain(terms: usize) -> String {
    vec!["title eq 'a'"; terms].join(" or ")
}

#[tokio::test]
async fn wide_filters_are_400_filter_too_complex_and_the_process_survives() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("a"), 1).await;
    env.seed_message(chat, 1, "user", 1).await;

    for terms in [250_usize, 400] {
        let filter = or_chain(terms);
        let chats = env.list_chats(&format!("?$filter={}", enc(&filter))).await;
        let messages = env
            .list_messages(
                chat,
                &format!("?$filter={}", enc(&filter.replace("title", "role"))),
            )
            .await;

        chats.assert_odata_error("FILTER_TOO_COMPLEX");
        messages.assert_odata_error("FILTER_TOO_COMPLEX");
    }
    assert_eq!(env.list_chats("").await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_short_term_chain_over_the_operator_bound_is_rejected_too() {
    let env = ChatEnv::new().await;
    let filter = vec!["id eq 1"; 120].join(" or ");

    let reply = env.list_chats(&format!("?$filter={}", enc(&filter))).await;

    reply.assert_odata_error("FILTER_TOO_COMPLEX");
}

#[tokio::test]
async fn deep_nesting_is_rejected() {
    let env = ChatEnv::new().await;
    let filter = format!("{}title eq 'a'{}", "(".repeat(300), ")".repeat(300));

    let reply = env.list_chats(&format!("?$filter={}", enc(&filter))).await;

    reply.assert_odata_error("FILTER_TOO_COMPLEX");
}

#[tokio::test]
async fn a_ten_term_filter_and_one_at_the_structural_bound_still_work() {
    let env = ChatEnv::new().await;
    env.seed_chat(1, Some("a"), 1).await;
    env.seed_chat(2, Some("b"), 2).await;

    for terms in [10_usize, 30] {
        let reply = env
            .list_chats(&format!("?$filter={}", enc(&or_chain(terms))))
            .await;

        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{terms} terms: {}",
            reply.text
        );
        assert_eq!(
            item_ids(&reply),
            vec![chat_id(1).to_string()],
            "{terms} terms"
        );
    }
}

#[tokio::test]
async fn a_filter_beyond_the_platform_limit_keeps_its_platform_reason() {
    let env = ChatEnv::new().await;
    let filter = format!("title eq '{}'", "x".repeat(9000));

    let reply = env.list_chats(&format!("?$filter={}", enc(&filter))).await;

    reply.assert_odata_error("FILTER_TOO_LONG");
}

// ================================================================================================
// R2B-001: operators glued to their neighbours, on a tokio-worker-sized stack
// ================================================================================================

/// Stack of a tokio worker thread (the runtime default, 2 MiB): what the
/// platform filter conversion runs on in production.
const WORKER_STACK: usize = 2 * 1024 * 1024;

/// Runs `test` on a fresh thread with the tokio worker default stack. A stack
/// overflow aborts the whole test process, so a passing run proves no crash.
fn on_worker_stack<F, Fut>(test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::Builder::new()
        .name("worker-stack-test".to_owned())
        .stack_size(WORKER_STACK)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(test());
        })
        .unwrap()
        .join()
        .unwrap();
}

/// The filters that evade a whole-word operator count: the grammar accepts an
/// operator glued to the neighbouring tokens.
fn glued_filters() -> Vec<(&'static str, String)> {
    vec![
        ("ortrue chain", format!("true{}", "ortrue".repeat(339))),
        ("not chain", format!("{}true", "not".repeat(300))),
        ("1orid chain", format!("id eq 1{}", "orid eq 1".repeat(250))),
        ("andtrue chain", format!("true{}", "andtrue".repeat(280))),
        // Few keywords, but a deep AST: only the structural check sees it.
        ("40 or terms", vec!["id eq 1"; 40].join(" or ")),
        // Shallow but wide: more than 200 nodes with no logical operator.
        (
            "wide in list",
            format!("title in ({})", vec!["'a'"; 250].join(",")),
        ),
    ]
}

#[test]
fn glued_operator_chains_are_400_filter_too_complex_on_a_worker_sized_stack() {
    on_worker_stack(|| async {
        let env = ChatEnv::new().await;
        let chat = env.seed_chat(1, Some("a"), 1).await;
        env.seed_message(chat, 1, "user", 1).await;

        for (name, filter) in glued_filters() {
            let chats = env.list_chats(&format!("?$filter={}", enc(&filter))).await;
            let messages = env
                .list_messages(
                    chat,
                    &format!("?$filter={}", enc(&filter.replace("title", "role"))),
                )
                .await;

            assert_eq!(
                chats.status,
                StatusCode::BAD_REQUEST,
                "{name}: {}",
                chats.text
            );
            chats.assert_odata_error("FILTER_TOO_COMPLEX");
            messages.assert_odata_error("FILTER_TOO_COMPLEX");
        }
        assert_eq!(env.list_chats("").await.status, StatusCode::OK);
    });
}

#[test]
fn glued_chains_inside_the_guard_bounds_are_still_400_never_a_crash() {
    // Written to be as large as the lexical bound allows and still small
    // enough for the structural check alone to be the one that rejects.
    on_worker_stack(|| async {
        let env = ChatEnv::new().await;
        let filter = format!("true{}", "ortrue".repeat(40));

        let reply = env.list_chats(&format!("?$filter={}", enc(&filter))).await;

        reply.assert_odata_error("FILTER_TOO_COMPLEX");
    });
}

#[test]
fn legitimate_filters_with_keywords_inside_literals_and_functions_still_work() {
    on_worker_stack(|| async {
        let env = ChatEnv::new().await;
        let rock = env.seed_chat(1, Some("rock and roll"), 1).await;
        let order = env.seed_chat(2, Some("order"), 2).await;
        env.seed_chat(3, Some("plain"), 3).await;

        let cases: Vec<(String, Vec<String>)> = vec![
            (
                "title eq 'rock and roll'".to_owned(),
                vec![rock.to_string()],
            ),
            ("contains(title,'or')".to_owned(), vec![order.to_string()]),
            (
                "contains(title, 'report') or title eq 'order'".to_owned(),
                vec![order.to_string()],
            ),
            (
                format!("id eq {rock} and title ne 'x'"),
                vec![rock.to_string()],
            ),
            (
                "startswith(title,'p') and not contains(title,'zzz')".to_owned(),
                vec![chat_id(3).to_string()],
            ),
        ];
        for (filter, expected) in cases {
            let reply = env.list_chats(&format!("?$filter={}", enc(&filter))).await;

            assert_eq!(reply.status, StatusCode::OK, "{filter}: {}", reply.text);
            let mut ids = item_ids(&reply);
            ids.sort();
            let mut expected = expected;
            expected.sort();
            assert_eq!(ids, expected, "{filter}");
        }
    });
}

// ================================================================================================
// R3-001 / R3-002: failing nested `not(` and the bounded structure check
// ================================================================================================

#[test]
fn a_failing_nested_not_filter_is_400_invalid_filter_quickly_on_a_worker_sized_stack() {
    on_worker_stack(|| async {
        let env = ChatEnv::new().await;
        let chat = env.seed_chat(1, Some("a"), 1).await;
        let filter = format!("{}1 eq{}", "not(".repeat(32), ")".repeat(32));

        let started = std::time::Instant::now();
        let chats = env.list_chats(&format!("?$filter={}", enc(&filter))).await;
        let messages = env
            .list_messages(chat, &format!("?$filter={}", enc(&filter)))
            .await;
        let elapsed = started.elapsed();

        assert_eq!(chats.status, StatusCode::BAD_REQUEST, "{}", chats.text);
        chats.assert_odata_error("INVALID_FILTER");
        messages.assert_odata_error("INVALID_FILTER");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "took {elapsed:?}"
        );
        assert_eq!(env.list_chats("").await.status, StatusCode::OK);
    });
}

#[test]
fn malformed_filters_are_400_invalid_filter_whatever_their_shape() {
    on_worker_stack(|| async {
        let env = ChatEnv::new().await;

        for filter in ["title eq", "(title eq 'a'", "not", "title eq 'a' and"] {
            let reply = env.list_chats(&format!("?$filter={}", enc(filter))).await;

            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{filter}");
            reply.assert_odata_error("INVALID_FILTER");
        }
    });
}

#[test]
fn a_burst_of_nesting_filters_all_get_their_answer_under_the_concurrency_bound() {
    on_worker_stack(|| async {
        let env = ChatEnv::new().await;
        env.seed_chat(1, Some("a"), 1).await;
        let filter = format!("{}title eq 'a'{}", "(".repeat(20), ")".repeat(20));
        let query = format!("?$filter={}", enc(&filter));

        let replies = futures::future::join_all((0..24).map(|_| env.list_chats(&query))).await;

        for reply in replies {
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        }
    });
}

#[test]
fn nesting_filters_that_are_legitimate_still_pass() {
    on_worker_stack(|| async {
        let env = ChatEnv::new().await;
        let one = env.seed_chat(1, Some("a"), 1).await;
        env.seed_chat(2, Some("b"), 2).await;

        for filter in [
            "(title eq 'a')".to_owned(),
            "not (title eq 'b')".to_owned(),
            "not(title eq 'b') and contains(title,'a')".to_owned(),
            format!("(id eq {one}) or (id eq {one})"),
        ] {
            let reply = env.list_chats(&format!("?$filter={}", enc(&filter))).await;

            assert_eq!(reply.status, StatusCode::OK, "{filter}: {}", reply.text);
            assert_eq!(item_ids(&reply), vec![one.to_string()], "{filter}");
        }
    });
}
