//! The sort keys of the chat list on `SQLite` over the real router: untitled chats under the
//! title orders, and fractional-second timestamps whose stored RFC 3339 text is not in
//! chronological order (`sqlx` strips the trailing zeros of the fraction and omits it when
//! zero) under the default order, `$orderby`, `$filter` and cursor paging.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: algorithms `list-query` and
//! `activity-ordering`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use axum::http::StatusCode;
use time::OffsetDateTime;
use uuid::Uuid;

use super::chat_list_tests::{enc, filtered, list, walk};
use crate::test_support::chats::{ChatRig, a1, at, chat_row, item_ids, rig};
use crate::test_support::{Pdp, raw_i64};

// ── untitled chats under the title orders ────────────────────────────────────

#[tokio::test]
async fn untitled_chats_are_never_dropped_by_cursor_paging_under_the_title_orders() {
    let rig = rig(Pdp::TenantOnly).await;
    let mut untitled = Vec::new();
    for i in 0..4 {
        let chat = rig.seed_at(&a1(), Uuid::new_v4(), None, "m", at(i)).await;
        untitled.push(chat.id);
    }
    let mut titled = Vec::new();
    for (i, title) in ["b", "c", "a"].into_iter().enumerate() {
        let chat = rig
            .seed_at(
                &a1(),
                Uuid::new_v4(),
                Some(title),
                "m",
                at(10 + i64::try_from(i).unwrap()),
            )
            .await;
        titled.push((title, chat.id));
    }
    // the stored column stays NULL: the empty title is only the sort key
    assert_eq!(chat_row(&rig.db, untitled[0]).await.unwrap().title, None);

    untitled.sort();
    let by_title = |letters: &[&str]| -> Vec<Uuid> {
        letters
            .iter()
            .map(|l| titled.iter().find(|(t, _)| t == l).unwrap().1)
            .collect()
    };
    // an untitled chat is the empty title: first when ascending, last when descending, and the
    // id tiebreaker is in the direction of the order
    let mut asc = untitled.clone();
    asc.extend(by_title(&["a", "b", "c"]));
    let mut desc = by_title(&["c", "b", "a"]);
    desc.extend(untitled.iter().rev().copied());

    for (orderby, expected) in [("title asc", asc), ("title desc", desc)] {
        let order = format!("&$orderby={}", enc(orderby));
        let unpaged = item_ids(&list(&rig, &format!("limit=100{order}")).await.json());
        assert_eq!(unpaged, expected, "{orderby}: unpaged order");
        for limit in [1, 2, 3] {
            let paged = walk(&rig, &format!("limit={limit}{order}"), limit, "").await;
            assert_eq!(paged, expected, "{orderby}, limit {limit}: no chat lost");
            let distinct: BTreeSet<Uuid> = paged.iter().copied().collect();
            assert_eq!(distinct.len(), 7, "{orderby}, limit {limit}: no repeats");
        }
    }
}

#[tokio::test]
async fn the_previous_page_of_a_title_order_returns_the_untitled_chats_it_followed() {
    let rig = rig(Pdp::TenantOnly).await;
    for i in 0..3 {
        rig.seed_at(&a1(), Uuid::new_v4(), None, "m", at(i)).await;
    }
    for (i, title) in ["a", "b", "c"].into_iter().enumerate() {
        rig.seed_at(
            &a1(),
            Uuid::new_v4(),
            Some(title),
            "m",
            at(10 + i64::try_from(i).unwrap()),
        )
        .await;
    }
    let first = list(&rig, &format!("limit=2&$orderby={}", enc("title asc")))
        .await
        .json();
    let second = list(
        &rig,
        &format!(
            "limit=2&cursor={}",
            enc(first["page_info"]["next_cursor"].as_str().unwrap())
        ),
    )
    .await
    .json();
    let back = list(
        &rig,
        &format!(
            "limit=2&cursor={}",
            enc(second["page_info"]["prev_cursor"].as_str().unwrap())
        ),
    )
    .await
    .json();
    assert_eq!(item_ids(&back), item_ids(&first));
    // every untitled chat has no `title` on the wire
    let titles: Vec<_> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.get("title").is_some())
        .collect();
    assert_eq!(titles, vec![false, false]);
}

// ── fractional-second timestamps ─────────────────────────────────────────────

/// `at(0)` plus `micros` microseconds.
fn at_micros(micros: i64) -> OffsetDateTime {
    at(0) + time::Duration::microseconds(micros)
}

/// The offsets (µs after `at(0)`) of the chats of the fixture, in chronological order. Their
/// stored texts are `…:20Z`, `…:20.12Z`, `…:20.123Z`, `…:20.1234Z`, `…:20.123456Z`,
/// `…:20.5Z`, `…:20.999999Z` and `…:21Z`: in text order `20Z` follows `20.5Z`, `20.12Z`
/// follows `20.123456Z`.
const CHRONOLOGICAL: [i64; 8] = [
    0, 120_000, 123_000, 123_400, 123_456, 500_000, 999_999, 1_000_000,
];

/// A chat for each offset of [`CHRONOLOGICAL`], seeded out of order and titled with its offset.
async fn fractional_fixture() -> ChatRig {
    let rig = rig(Pdp::TenantOnly).await;
    for micros in [
        500_000, 0, 123_456, 1_000_000, 120_000, 999_999, 123_400, 123_000,
    ] {
        rig.seed_at(
            &a1(),
            Uuid::new_v4(),
            Some(&micros.to_string()),
            "m",
            at_micros(micros),
        )
        .await;
    }
    // the premise: the stored text really is the trimmed, non-chronological form
    for text in [
        "2023-11-14T22:13:20Z",
        "2023-11-14T22:13:20.12Z",
        "2023-11-14T22:13:20.1234Z",
    ] {
        let count = raw_i64(
            &rig.db.raw,
            &format!("SELECT COUNT(*) FROM chats WHERE updated_at = '{text}'"),
        )
        .await;
        assert_eq!(count, 1, "{text} is stored as written by sqlx");
    }
    rig
}

fn offsets(titles: &[i64]) -> Vec<String> {
    titles.iter().map(ToString::to_string).collect()
}

async fn titles_of(rig: &ChatRig, query: &str) -> Vec<String> {
    let reply = list(rig, query).await;
    assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.text);
    reply.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["title"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_default_and_explicit_updated_at_orders_are_chronological() {
    let rig = fractional_fixture().await;
    let mut newest_first = CHRONOLOGICAL;
    newest_first.reverse();
    assert_eq!(titles_of(&rig, "").await, offsets(&newest_first));
    for (orderby, expected) in [
        ("updated_at desc", offsets(&newest_first)),
        ("updated_at asc", offsets(&CHRONOLOGICAL)),
    ] {
        assert_eq!(
            titles_of(&rig, &format!("$orderby={}", enc(orderby))).await,
            expected,
            "{orderby}"
        );
    }
}

#[tokio::test]
async fn updated_at_filters_compare_chronologically() {
    let rig = fractional_fixture().await;
    // the literal 20.1234 equals the offset 123_400 whatever digits it is written with
    for literal in ["2023-11-14T22:13:20.1234Z", "2023-11-14T22:13:20.123400Z"] {
        let cases = [
            (format!("updated_at gt {literal}"), &CHRONOLOGICAL[4..]),
            (format!("updated_at ge {literal}"), &CHRONOLOGICAL[3..]),
            (format!("updated_at lt {literal}"), &CHRONOLOGICAL[..3]),
            (format!("updated_at le {literal}"), &CHRONOLOGICAL[..4]),
            (format!("updated_at eq {literal}"), &CHRONOLOGICAL[3..4]),
        ];
        for (filter, expected) in cases {
            let query = format!(
                "$filter={}&$orderby={}",
                enc(&filter),
                enc("updated_at asc")
            );
            assert_eq!(titles_of(&rig, &query).await, offsets(expected), "{filter}");
        }
    }
    let ne = titles_of(
        &rig,
        &format!(
            "$filter={}&$orderby={}",
            enc("updated_at ne 2023-11-14T22:13:20.1234Z"),
            enc("updated_at asc")
        ),
    )
    .await;
    assert_eq!(ne.len(), 7);
    assert!(!ne.contains(&"123400".to_owned()));

    // a whole second and a fraction that text-sorts after it
    for (filter, expected) in [
        ("updated_at gt 2023-11-14T22:13:20.5Z", &CHRONOLOGICAL[6..]),
        ("updated_at lt 2023-11-14T22:13:21Z", &CHRONOLOGICAL[..7]),
        ("updated_at ge 2023-11-14T22:13:21Z", &CHRONOLOGICAL[7..]),
        ("updated_at lt 2023-11-14T22:13:20.12Z", &CHRONOLOGICAL[..1]),
    ] {
        let query = format!("$filter={}&$orderby={}", enc(filter), enc("updated_at asc"));
        assert_eq!(titles_of(&rig, &query).await, offsets(expected), "{filter}");
    }

    let in_list = "updated_at in (2023-11-14T22:13:20.12Z, 2023-11-14T22:13:21Z, \
                   2023-11-14T22:13:20.123456Z) and updated_at ge 2023-11-14T22:13:20.12Z";
    assert_eq!(
        filtered(&rig, in_list).await.len(),
        3,
        "in-lists and conjunctions compare on the same key"
    );
}

#[tokio::test]
async fn cursor_paging_by_updated_at_is_chronological_without_gaps_or_repeats() {
    let rig = fractional_fixture().await;
    let mut newest_first = CHRONOLOGICAL;
    newest_first.reverse();
    for (order, expected) in [
        (String::new(), offsets(&newest_first)),
        (
            format!("&$orderby={}", enc("updated_at asc")),
            offsets(&CHRONOLOGICAL),
        ),
        (
            format!("&$orderby={}", enc("updated_at desc")),
            offsets(&newest_first),
        ),
    ] {
        let all: Vec<Uuid> = item_ids(&list(&rig, &format!("limit=100{order}")).await.json());
        assert_eq!(all.len(), 8);
        for limit in [1, 3] {
            let paged = walk(&rig, &format!("limit={limit}{order}"), limit, "").await;
            assert_eq!(paged, all, "{order}, limit {limit}");
        }
        // the unpaged order is the chronological one
        let titles = titles_of(&rig, &format!("limit=100{order}")).await;
        assert_eq!(titles, expected, "{order}");
    }
    // a filtered walk keeps its position across the pages
    let after = format!(
        "&$filter={}&$orderby={}",
        enc("updated_at gt 2023-11-14T22:13:20.123Z"),
        enc("updated_at asc")
    );
    let first = list(&rig, &format!("limit=2{after}")).await.json();
    assert_eq!(
        first["items"][0]["title"], "123400",
        "the first chat after 20.123"
    );
    let cursor = first["page_info"]["next_cursor"].as_str().unwrap();
    let rest = list(
        &rig,
        &format!(
            "limit=100&cursor={}&$filter={}",
            enc(cursor),
            enc("updated_at gt 2023-11-14T22:13:20.123Z")
        ),
    )
    .await;
    assert_eq!(rest.status, StatusCode::OK, "{}", rest.text);
    assert_eq!(
        rest.json()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        offsets(&CHRONOLOGICAL[5..]),
    );
}
