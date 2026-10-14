//! `GET /v1/chats` over the real router, SQLite and the mock PDP: the page shape, the default
//! and explicit orders, `$filter`, every `limit` variant, cursor paging without gaps or repeats,
//! the `OData` error reasons and activity ordering.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: flow
//! `cpt-cf-mini-chat-flow-chat-management-list-chats`; algorithms `list-query`,
//! `activity-ordering`, `message-count`, `chat-detail-projection`; definitions of done
//! `cpt-cf-mini-chat-dod-chat-management-list` and
//! `cpt-cf-mini-chat-dod-chat-management-activity-ordering`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::fmt::Write as _;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::infra::storage::chat_page::TimeStorage;
use crate::infra::storage::repo_impl::ChatRepo;
use crate::test_support::Pdp;
use crate::test_support::chats::{
    CHATS, ChatRig, a1, assert_odata_error, assert_problem, at, at_rfc3339, detail_keys, item_ids,
    keys, owner_scope, parse_ts, rig, seed_message, violation,
};

/// Percent-encode a query component.
pub(super) fn enc(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            write!(out, "%{byte:02X}").unwrap();
        }
    }
    out
}

/// `GET /chats?{query}` where `query` is already encoded.
pub(super) async fn list(rig: &ChatRig, query: &str) -> crate::test_support::chats::Reply {
    let uri = if query.is_empty() {
        CHATS.to_owned()
    } else {
        format!("{CHATS}?{query}")
    };
    rig.get(&uri, &a1()).await
}

pub(super) async fn filtered(rig: &ChatRig, filter: &str) -> Vec<Uuid> {
    let reply = list(rig, &format!("$filter={}", enc(filter))).await;
    assert_eq!(reply.status, StatusCode::OK, "{filter}: {}", reply.text);
    item_ids(&reply.json())
}

/// Seed `n` chats at distinct times `at(0)..at(n)`; returns the ids oldest first.
async fn seed_many(rig: &ChatRig, n: i64) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for i in 0..n {
        let chat = rig
            .seed_at(&a1(), Uuid::new_v4(), Some("c"), "gpt-standard", at(i))
            .await;
        ids.push(chat.id);
    }
    ids
}

// ── page shape and default order ─────────────────────────────────────────────

#[tokio::test]
async fn a_user_without_chats_gets_an_empty_page() {
    let rig = rig(Pdp::TenantOnly).await;
    let reply = list(&rig, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("application/json"),
        "{}",
        reply.content_type
    );
    assert_eq!(
        reply.json(),
        json!({
            "items": [],
            "page_info": { "limit": 20, "next_cursor": null, "prev_cursor": null }
        })
    );
}

#[tokio::test]
async fn the_page_carries_items_and_page_info_with_null_cursors_when_there_is_no_other_page() {
    let rig = rig(Pdp::TenantOnly).await;
    let titled = rig
        .seed_at(
            &a1(),
            Uuid::new_v4(),
            Some("titled"),
            "gpt-standard",
            at(20),
        )
        .await;
    let untitled = rig
        .seed_at(&a1(), Uuid::new_v4(), None, "gpt-premium", at(10))
        .await;
    let reply = list(&rig, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = reply.json();
    assert_eq!(keys(&body), BTreeSet::from(["items", "page_info"]));
    assert_eq!(
        keys(&body["page_info"]),
        BTreeSet::from(["limit", "next_cursor", "prev_cursor"])
    );
    assert!(body["page_info"]["next_cursor"].is_null());
    assert!(body["page_info"]["prev_cursor"].is_null());

    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(keys(&items[0]), detail_keys(true));
    assert_eq!(
        keys(&items[1]),
        detail_keys(false),
        "untitled: no title key"
    );
    assert_eq!(items[0]["id"], titled.id.to_string());
    assert_eq!(items[1]["id"], untitled.id.to_string());
    assert_eq!(items[1]["model"], "gpt-premium");
    assert_eq!(items[0]["is_temporary"], false);
    assert_eq!(parse_ts(&items[0]["updated_at"]), at(20));
}

#[tokio::test]
async fn each_item_carries_its_own_message_count() {
    let rig = rig(Pdp::TenantOnly).await;
    let busy = rig
        .seed_at(&a1(), Uuid::new_v4(), Some("busy"), "m", at(2))
        .await;
    let quiet = rig
        .seed_at(&a1(), Uuid::new_v4(), Some("quiet"), "m", at(1))
        .await;
    seed_message(&rig.db, &busy, "user").await;
    seed_message(&rig.db, &busy, "assistant").await;
    let body = list(&rig, "").await.json();
    let counts: Vec<(String, i64)> = body["items"]
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
    assert_eq!(
        counts,
        vec![(busy.id.to_string(), 2), (quiet.id.to_string(), 0)]
    );
}

#[tokio::test]
async fn the_default_order_is_most_recently_updated_first() {
    let rig = rig(Pdp::TenantOnly).await;
    let ids = seed_many(&rig, 4).await;
    let listed = item_ids(&list(&rig, "").await.json());
    let mut expected = ids.clone();
    expected.reverse();
    assert_eq!(listed, expected);
}

#[tokio::test]
async fn equal_updated_at_values_are_ordered_by_id_in_the_primary_direction() {
    let rig = rig(Pdp::TenantOnly).await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        let id = Uuid::new_v4();
        rig.seed_at(&a1(), id, Some("same"), "m", at(7)).await;
        ids.push(id);
    }
    ids.sort();
    let mut descending = ids.clone();
    descending.reverse();

    let cases = vec![
        ("", descending.clone()),
        ("$orderby=updated_at%20desc", descending.clone()),
        ("$orderby=updated_at%20asc", ids.clone()),
        ("$orderby=title%20desc", descending.clone()),
        ("$orderby=title%20asc", ids.clone()),
        ("$orderby=id%20desc", descending.clone()),
        ("$orderby=id%20asc", ids.clone()),
    ];
    for (query, expected) in cases {
        let reply = list(&rig, query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.text);
        assert_eq!(item_ids(&reply.json()), expected, "{query}");
    }
}

// ── activity ordering ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_chat_bumped_by_a_message_activity_write_is_listed_before_a_newer_one() {
    let rig = rig(Pdp::TenantOnly).await;
    let older = rig
        .seed_at(&a1(), Uuid::new_v4(), Some("older"), "m", at(10))
        .await;
    let newer = rig
        .seed_at(&a1(), Uuid::new_v4(), Some("newer"), "m", at(20))
        .await;
    assert_eq!(
        item_ids(&list(&rig, "").await.json()),
        vec![newer.id, older.id]
    );

    // a sent message, a retry and an edit move updated_at forward in the transaction of the
    // message write; the same rule through the repository
    let (scope, chat) = (owner_scope(&a1()), older.clone());
    rig.db
        .provider
        .transaction(move |tx| {
            Box::pin(async move {
                crate::test_support::chats::seed_message_in(tx, &chat).await?;
                ChatRepo::touch_chat(tx, &scope, chat.id).await
            })
        })
        .await
        .unwrap();
    assert_eq!(
        item_ids(&list(&rig, "").await.json()),
        vec![older.id, newer.id]
    );
}

#[tokio::test]
async fn an_explicit_orderby_overrides_the_activity_order() {
    let rig = rig(Pdp::TenantOnly).await;
    let ids = seed_many(&rig, 3).await;
    let listed = item_ids(&list(&rig, "$orderby=updated_at%20asc").await.json());
    assert_eq!(listed, ids);
}

// ── $orderby ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_six_allowed_orders_sort_by_updated_at_id_and_title() {
    let rig = rig(Pdp::TenantOnly).await;
    let mut chats = Vec::new();
    for (title, secs) in [("pear", 30), ("apple", 10), ("mango", 20), ("kiwi", 40)] {
        let id = Uuid::new_v4();
        rig.seed_at(&a1(), id, Some(title), "m", at(secs)).await;
        chats.push((id, title, secs));
    }
    let sorted_by = |key: fn(&(Uuid, &str, i64)) -> String, descending: bool| -> Vec<Uuid> {
        let mut rows = chats.clone();
        rows.sort_by_key(key);
        if descending {
            rows.reverse();
        }
        rows.into_iter().map(|(id, _, _)| id).collect()
    };
    let by_updated = |c: &(Uuid, &str, i64)| format!("{:05}", c.2);
    let by_title = |c: &(Uuid, &str, i64)| c.1.to_owned();
    let by_id = |c: &(Uuid, &str, i64)| c.0.to_string();
    let cases = vec![
        ("updated_at asc", sorted_by(by_updated, false)),
        ("updated_at desc", sorted_by(by_updated, true)),
        ("title asc", sorted_by(by_title, false)),
        ("title desc", sorted_by(by_title, true)),
        ("id asc", sorted_by(by_id, false)),
        ("id desc", sorted_by(by_id, true)),
    ];
    for (orderby, expected) in cases {
        let reply = list(&rig, &format!("$orderby={}", enc(orderby))).await;
        assert_eq!(reply.status, StatusCode::OK, "{orderby}: {}", reply.text);
        assert_eq!(item_ids(&reply.json()), expected, "{orderby}");
    }
}

#[tokio::test]
async fn an_orderby_outside_the_allowed_values_is_invalid_orderby_field() {
    let rig = rig(Pdp::TenantOnly).await;
    for orderby in [
        "model asc",
        "created_at desc",
        "message_count desc",
        "title sideways",
        "unknown",
    ] {
        let reply = list(&rig, &format!("$orderby={}", enc(orderby))).await;
        assert_odata_error(&reply, "$orderby", "INVALID_ORDERBY_FIELD");
    }
}

// ── $filter ──────────────────────────────────────────────────────────────────

/// Five chats: alpha(10), beta(20), alphabet(30), gamma(40), delta alpha(50).
async fn filter_fixture() -> (ChatRig, Vec<(Uuid, &'static str, i64)>) {
    let rig = rig(Pdp::TenantOnly).await;
    let mut rows = Vec::new();
    for (title, secs) in [
        ("alpha", 10),
        ("beta", 20),
        ("alphabet", 30),
        ("gamma", 40),
        ("delta alpha", 50),
    ] {
        let id = Uuid::new_v4();
        rig.seed_at(&a1(), id, Some(title), "m", at(secs)).await;
        rows.push((id, title, secs));
    }
    (rig, rows)
}

/// The ids of the fixture rows with the given titles, in the default order (newest first).
fn ids_of(rows: &[(Uuid, &'static str, i64)], titles: &[&str]) -> Vec<Uuid> {
    let mut picked: Vec<_> = rows.iter().filter(|r| titles.contains(&r.1)).collect();
    picked.sort_by_key(|r| std::cmp::Reverse(r.2));
    picked.into_iter().map(|r| r.0).collect()
}

#[tokio::test]
async fn title_filters_support_eq_ne_contains_startswith_endswith_and_in() {
    let (rig, rows) = filter_fixture().await;
    let cases = vec![
        ("title eq 'alpha'", vec!["alpha"]),
        (
            "title ne 'alpha'",
            vec!["beta", "alphabet", "gamma", "delta alpha"],
        ),
        (
            "contains(title,'alpha')",
            vec!["alpha", "alphabet", "delta alpha"],
        ),
        ("startswith(title,'alpha')", vec!["alpha", "alphabet"]),
        ("endswith(title,'alpha')", vec!["alpha", "delta alpha"]),
        ("title in ('beta','gamma')", vec!["beta", "gamma"]),
        ("title eq 'missing'", vec![]),
    ];
    for (filter, titles) in cases {
        assert_eq!(
            filtered(&rig, filter).await,
            ids_of(&rows, &titles),
            "{filter}"
        );
    }
}

#[tokio::test]
async fn id_filters_support_eq_ne_and_in() {
    let (rig, rows) = filter_fixture().await;
    let (a, b) = (rows[0].0, rows[1].0);
    assert_eq!(filtered(&rig, &format!("id eq {a}")).await, vec![a]);
    let not_a = filtered(&rig, &format!("id ne {a}")).await;
    assert_eq!(not_a.len(), 4);
    assert!(!not_a.contains(&a));
    let mut both = filtered(&rig, &format!("id in ({a}, {b})")).await;
    both.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(both, expected);
}

#[tokio::test]
async fn updated_at_filters_support_eq_ne_gt_ge_lt_le_and_in() {
    let (rig, rows) = filter_fixture().await;
    let t = |secs: i64| at_rfc3339(secs);
    let cases = vec![
        (
            format!("updated_at gt {}", t(30)),
            vec!["gamma", "delta alpha"],
        ),
        (
            format!("updated_at ge {}", t(30)),
            vec!["alphabet", "gamma", "delta alpha"],
        ),
        (format!("updated_at lt {}", t(30)), vec!["alpha", "beta"]),
        (
            format!("updated_at le {}", t(30)),
            vec!["alpha", "beta", "alphabet"],
        ),
        (format!("updated_at eq {}", t(30)), vec!["alphabet"]),
        (
            format!("updated_at ne {}", t(30)),
            vec!["alpha", "beta", "gamma", "delta alpha"],
        ),
        (
            format!("updated_at in ({}, {})", t(10), t(50)),
            vec!["alpha", "delta alpha"],
        ),
    ];
    for (filter, titles) in cases {
        assert_eq!(
            filtered(&rig, &filter).await,
            ids_of(&rows, &titles),
            "{filter}"
        );
    }
}

#[tokio::test]
async fn filters_combine_with_logical_operators_and_with_the_order() {
    let (rig, rows) = filter_fixture().await;
    let both = format!(
        "startswith(title,'alpha') and updated_at gt {}",
        at_rfc3339(10)
    );
    assert_eq!(filtered(&rig, &both).await, ids_of(&rows, &["alphabet"]));
    assert_eq!(
        filtered(&rig, "title eq 'beta' or title eq 'gamma'").await,
        ids_of(&rows, &["beta", "gamma"])
    );
    let reply = list(
        &rig,
        &format!(
            "$filter={}&$orderby={}",
            enc("contains(title,'alpha')"),
            enc("title asc")
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let titles: Vec<String> = reply.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(titles, vec!["alpha", "alphabet", "delta alpha"]);
}

#[tokio::test]
async fn a_filter_on_an_unknown_field_or_with_an_operator_outside_the_set_is_invalid_filter() {
    let rig = rig(Pdp::TenantOnly).await;
    let id = Uuid::new_v4();
    let bad = vec![
        "model eq 'gpt-5.2'".to_owned(),
        "user_id eq 'x'".to_owned(),
        "tenant_id eq 'x'".to_owned(),
        "deleted_at eq 2026-01-01T00:00:00Z".to_owned(),
        "message_count gt 1".to_owned(),
        // operators outside the field's set
        format!("id gt {id}"),
        format!("id lt {id}"),
        "title gt 'a'".to_owned(),
        "title lt 'a'".to_owned(),
        "contains(updated_at,'2026')".to_owned(),
        format!("startswith(id,'{id}')"),
        // malformed
        "title eq".to_owned(),
        "((title eq 'a')".to_owned(),
        "title eq 'a' and".to_owned(),
        "not a filter at all".to_owned(),
    ];
    for filter in bad {
        let reply = list(&rig, &format!("$filter={}", enc(&filter))).await;
        assert_odata_error(&reply, "$filter", "INVALID_FILTER");
    }
}

#[tokio::test]
async fn a_filter_above_the_length_limit_is_filter_too_long() {
    let rig = rig(Pdp::TenantOnly).await;
    let filter = format!("title eq '{}'", "a".repeat(9 * 1024));
    let reply = list(&rig, &format!("$filter={}", enc(&filter))).await;
    assert_odata_error(&reply, "$filter", "FILTER_TOO_LONG");
}

#[tokio::test]
async fn a_filter_above_the_complexity_limit_is_filter_too_complex() {
    let rig = rig(Pdp::TenantOnly).await;
    // a conjunction of many comparisons: below the length limit, above the node budget
    let mut terms = 400;
    let filter = loop {
        let candidate = vec!["id eq 1"; terms].join(" and ");
        let parsed = toolkit_odata::parse_filter_string(&candidate).unwrap();
        if parsed.node_count() > 2000 {
            break candidate;
        }
        terms += 100;
    };
    assert!(filter.len() <= 8 * 1024, "{} bytes", filter.len());
    let reply = list(&rig, &format!("$filter={}", enc(&filter))).await;
    assert_odata_error(&reply, "$filter", "FILTER_TOO_COMPLEX");
}

// ── limit ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn limit_defaults_to_20_accepts_1_to_100_and_clamps_larger_values_to_100() {
    let rig = rig(Pdp::TenantOnly).await;
    seed_many(&rig, 105).await;
    let cases: Vec<(&str, usize, u64)> = vec![
        ("", 20, 20),
        ("limit=1", 1, 1),
        ("limit=7", 7, 7),
        ("limit=100", 100, 100),
        ("limit=101", 100, 100),
        ("limit=1000000", 100, 100),
        ("limit=18446744073709551615", 100, 100),
        ("%24top=5", 5, 5),
    ];
    for (query, items, effective) in cases {
        let reply = list(&rig, query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.text);
        let body = reply.json();
        assert_eq!(body["items"].as_array().unwrap().len(), items, "{query}");
        assert_eq!(body["page_info"]["limit"], effective, "{query}");
        assert!(
            body["page_info"]["next_cursor"].is_string(),
            "{query}: more chats exist"
        );
    }
}

#[tokio::test]
async fn a_limit_of_zero_is_invalid_limit_keyed_to_top() {
    let rig = rig(Pdp::TenantOnly).await;
    for query in ["limit=0", "%24top=0"] {
        let reply = list(&rig, query).await;
        assert_odata_error(&reply, "$top", "INVALID_LIMIT");
    }
}

#[tokio::test]
async fn an_unparsable_limit_is_invalid_query_params_keyed_to_query() {
    let rig = rig(Pdp::TenantOnly).await;
    for query in [
        "limit=-1",
        "limit=abc",
        "limit=1.5",
        "limit=",
        "limit=18446744073709551616",
        "limit=1e2",
        "limit=%2B5x",
        "%24top=abc",
    ] {
        let reply = list(&rig, query).await;
        assert_odata_error(&reply, "query", "INVALID_QUERY_PARAMS");
    }
}

#[tokio::test]
async fn limit_together_with_top_or_repeated_is_invalid_query_params() {
    let rig = rig(Pdp::TenantOnly).await;
    for query in ["limit=2&%24top=3", "limit=1&limit=2", "%24top=1&%24top=2"] {
        let reply = list(&rig, query).await;
        assert_odata_error(&reply, "query", "INVALID_QUERY_PARAMS");
    }
}

// ── other query options ──────────────────────────────────────────────────────

#[tokio::test]
async fn an_unsupported_dollar_option_is_unsupported_query_param() {
    let rig = rig(Pdp::TenantOnly).await;
    for option in [
        "$skip=1",
        "$count=true",
        "$expand=x",
        "$search=a",
        "$format=json",
        "$foo=1",
    ] {
        let name = option.split('=').next().unwrap();
        let reply = list(&rig, &enc(option).replace("%3D", "=")).await;
        assert_odata_error(&reply, name, "UNSUPPORTED_QUERY_PARAM");
    }
}

#[tokio::test]
async fn select_is_validated_and_ignored() {
    let rig = rig(Pdp::TenantOnly).await;
    rig.seed(&a1(), Some("t"), "gpt-standard").await;
    for select in ["id", "id,title", "model,unknown_field"] {
        let reply = list(&rig, &format!("$select={}", enc(select))).await;
        assert_eq!(reply.status, StatusCode::OK, "{select}: {}", reply.text);
        let item = &reply.json()["items"][0];
        assert_eq!(
            keys(item),
            detail_keys(true),
            "{select}: the full item is returned"
        );
    }
    for select in ["", "id,id", " , "] {
        let reply = list(&rig, &format!("$select={}", enc(select))).await;
        assert_odata_error(&reply, "$select", "INVALID_SELECT");
    }
}

// ── cursor ───────────────────────────────────────────────────────────────────

/// Follow `next_cursor` from the first request until it is null; returns the ids in order and
/// asserts that no page exceeds `limit`.
pub(super) async fn walk(rig: &ChatRig, first_query: &str, limit: usize, extra: &str) -> Vec<Uuid> {
    let mut collected = Vec::new();
    let mut query = first_query.to_owned();
    for _ in 0..50 {
        let reply = list(rig, &query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.text);
        let body = reply.json();
        let page = item_ids(&body);
        assert!(page.len() <= limit, "{query}");
        collected.extend(page);
        match body["page_info"]["next_cursor"].as_str() {
            Some(cursor) => query = format!("limit={limit}&cursor={}{extra}", enc(cursor)),
            None => return collected,
        }
    }
    panic!("the cursor never ended");
}

#[tokio::test]
async fn cursor_pages_have_no_gaps_or_repeats_under_every_order_even_with_equal_values() {
    let rig = rig(Pdp::TenantOnly).await;
    // equal timestamps and equal titles for most chats: only the id tiebreak separates them
    for i in 0..9 {
        let title = if i < 6 { "same" } else { "other" };
        let secs = if i < 6 { 5 } else { 15 };
        rig.seed_at(&a1(), Uuid::new_v4(), Some(title), "m", at(secs))
            .await;
    }
    for orderby in [
        None,
        Some("updated_at asc"),
        Some("updated_at desc"),
        Some("title asc"),
        Some("title desc"),
        Some("id asc"),
        Some("id desc"),
    ] {
        let order = orderby.map_or(String::new(), |o| format!("&$orderby={}", enc(o)));
        let all = item_ids(&list(&rig, &format!("limit=100{order}")).await.json());
        assert_eq!(all.len(), 9, "{orderby:?}");
        let paged = walk(&rig, &format!("limit=2{order}"), 2, "").await;
        assert_eq!(paged, all, "{orderby:?}: paging equals the unpaged order");
        let distinct: BTreeSet<Uuid> = paged.iter().copied().collect();
        assert_eq!(distinct.len(), 9, "{orderby:?}: no repeats");
    }
}

#[tokio::test]
async fn the_cursor_serves_previous_page_navigation() {
    let rig = rig(Pdp::TenantOnly).await;
    seed_many(&rig, 7).await;
    let first = list(&rig, "limit=3").await.json();
    assert!(
        first["page_info"]["prev_cursor"].is_null(),
        "no page before the first"
    );
    let second = list(
        &rig,
        &format!(
            "limit=3&cursor={}",
            enc(first["page_info"]["next_cursor"].as_str().unwrap())
        ),
    )
    .await
    .json();
    let prev = second["page_info"]["prev_cursor"]
        .as_str()
        .expect("a page before the second");
    let back = list(&rig, &format!("limit=3&cursor={}", enc(prev)))
        .await
        .json();
    assert_eq!(item_ids(&back), item_ids(&first));
}

#[tokio::test]
async fn a_cursor_continues_a_filtered_query_only_with_the_same_filter() {
    let rig = rig(Pdp::TenantOnly).await;
    for i in 0..6 {
        rig.seed_at(&a1(), Uuid::new_v4(), Some("keep"), "m", at(i))
            .await;
    }
    rig.seed_at(&a1(), Uuid::new_v4(), Some("skip"), "m", at(9))
        .await;
    let filter = format!("&$filter={}", enc("title eq 'keep'"));
    let first = list(&rig, &format!("limit=2{filter}")).await.json();
    let cursor = first["page_info"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();

    // the same filter continues
    let walked = walk(&rig, &format!("limit=2{filter}"), 2, &filter).await;
    assert_eq!(walked.len(), 6);

    // another filter, or none, is a mismatch
    for other in [
        format!("&$filter={}", enc("title eq 'skip'")),
        String::new(),
    ] {
        let reply = list(&rig, &format!("limit=2&cursor={}{other}", enc(&cursor))).await;
        assert_odata_error(&reply, "cursor", "FILTER_MISMATCH");
    }
}

#[tokio::test]
async fn a_cursor_together_with_orderby_is_order_with_cursor() {
    let rig = rig(Pdp::TenantOnly).await;
    seed_many(&rig, 3).await;
    let first = list(&rig, "limit=1").await.json();
    let cursor = first["page_info"]["next_cursor"].as_str().unwrap();
    for orderby in ["updated_at desc", "title asc"] {
        let reply = list(
            &rig,
            &format!("cursor={}&$orderby={}", enc(cursor), enc(orderby)),
        )
        .await;
        let body = assert_odata_error(&reply, "$orderby", "ORDER_WITH_CURSOR");
        assert_eq!(violation(&body, "cursor")["reason"], "ORDER_WITH_CURSOR");
    }
}

#[tokio::test]
async fn a_cursor_that_cannot_be_decoded_is_invalid_cursor() {
    let rig = rig(Pdp::TenantOnly).await;
    seed_many(&rig, 2).await;
    for cursor in ["garbage", "!!!", "e30", "bm90IGpzb24"] {
        let reply = list(&rig, &format!("cursor={}", enc(cursor))).await;
        assert_odata_error(&reply, "cursor", "INVALID_CURSOR");
    }
}

#[tokio::test]
async fn a_cursor_of_another_order_is_an_order_mismatch_at_the_repository_boundary() {
    // Over HTTP the extractor answers ORDER_WITH_CURSOR before a mismatch can arise; the
    // repository still reports ORDER_MISMATCH when a query carries a cursor and a different order.
    let rig = rig(Pdp::TenantOnly).await;
    seed_many(&rig, 5).await;
    let conn = rig.db.provider.conn().unwrap();
    let scope = owner_scope(&a1());
    let first = ChatRepo::list_chat_page(
        &conn,
        &scope,
        &toolkit_odata::ODataQuery::new().with_limit(2),
        TimeStorage::Rfc3339Text,
    )
    .await
    .unwrap();
    let cursor =
        toolkit_odata::CursorV1::decode(first.page_info.next_cursor.as_deref().unwrap()).unwrap();
    let query = toolkit_odata::ODataQuery::new()
        .with_cursor(cursor)
        .with_order(toolkit_odata::ODataOrderBy(vec![toolkit_odata::OrderKey {
            field: "title".to_owned(),
            dir: toolkit_odata::SortDir::Asc,
        }]));
    let outcome = ChatRepo::list_chat_page(&conn, &scope, &query, TimeStorage::Rfc3339Text).await;
    assert!(
        matches!(
            outcome,
            Err(crate::domain::error::DomainError::ODataQuery(
                toolkit_odata::Error::OrderMismatch
            ))
        ),
        "{outcome:?}"
    );
}

// ── admission ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_is_authorized_as_the_list_action_without_a_resource_id() {
    let rig = rig(Pdp::TenantOnly).await;
    let reply = list(&rig, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "list");
    assert_eq!(request.resource.id, None);
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn a_pdp_deny_or_an_unusable_decision_is_a_403_and_a_pdp_failure_a_503() {
    for behavior in [Pdp::Deny, Pdp::NoConstraints, Pdp::UnknownProperty] {
        let rig = rig(behavior).await;
        let reply = list(&rig, "").await;
        let body = assert_problem(&reply, 403, "permission_denied");
        assert_eq!(body["context"]["reason"], "AUTHZ_DENIED", "{behavior:?}");
        // admission precedes the repository query validation
        let reply = list(&rig, &format!("$filter={}", enc("model eq 'x'"))).await;
        assert_problem(&reply, 403, "permission_denied");
    }
    let rig = rig(Pdp::Unreachable).await;
    let reply = list(&rig, "").await;
    let body = assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
    assert_eq!(body["context"]["retry_after_seconds"], 5);
}

#[tokio::test]
async fn listing_does_not_change_any_updated_at() {
    let rig = rig(Pdp::TenantOnly).await;
    let ids = seed_many(&rig, 3).await;
    for query in ["", "limit=1"] {
        let reply = list(&rig, query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.text);
    }
    for (index, id) in ids.iter().enumerate() {
        let secs = i64::try_from(index).unwrap();
        let row = crate::test_support::chats::chat_row(&rig.db, *id)
            .await
            .unwrap();
        assert_eq!(row.updated_at, at(secs));
    }
}
