//! Repository tests of the message reads over in-memory `SQLite`: the message page (order,
//! filters, limits, keyset cursors), the attachment and reaction batches, the snapshot boundary,
//! the recent messages query and the thread summary.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithms
//! `message-list-query`, `message-projection` (the batches), `message-order`, `context-assembly`
//! (the selection steps); definitions of done `cpt-cf-mini-chat-dod-message-streaming-messages-api`,
//! `cpt-cf-mini-chat-dod-message-streaming-message-order` and
//! `cpt-cf-mini-chat-dod-message-streaming-context-assembly`.
//!
//! The stored text of a `SQLite` timestamp is variable length (`...:20.12Z` sorts after
//! `...:20.123Z`), so every fixture below mixes fractions of different lengths.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm_migration::MigratorTrait;
use toolkit_odata::{
    CursorV1, ODataOrderBy, ODataQuery, OrderKey, Page, SortDir, parse_filter_string,
};
use uuid::Uuid;

use super::MessageRepo;
use crate::domain::error::DomainError;
use crate::domain::messages::{MessageRow, Reaction};
use crate::domain::streaming::{ContextMessage, ContextRole, SnapshotBoundary};
use crate::infra::storage::chat_page::TimeStorage;
use crate::infra::storage::migrations::Migrator;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, at, seed_chat};
use crate::test_support::send::{
    AttachmentSeed, MessageSeed, link_att, seed_att, seed_msg, seed_reaction_of, seed_summary, t,
};
use crate::test_support::{TENANT_A, TestDb, USER_A1, USER_A2, raw_i64, sqlite_db};

const STORAGE: TimeStorage = TimeStorage::Rfc3339Text;

/// The offsets (µs after `at(0)`) of the fractional fixture, chronologically. Their stored texts
/// are `…:20Z`, `…:20.12Z`, `…:20.123Z`, `…:20.1234Z`, `…:20.123456Z`, `…:20.5Z`,
/// `…:20.999999Z` and `…:21Z`: in text order `20Z` follows `20.5Z`.
const CHRONOLOGICAL: [i64; 8] = [
    0, 120_000, 123_000, 123_400, 123_456, 500_000, 999_999, 1_000_000,
];

async fn setup() -> (TestDb, ChatRecord) {
    let db = sqlite_db(Migrator::migrations()).await;
    let chat = seed_chat(&db, &a1(), Uuid::new_v4(), Some("chat"), "m", at(0)).await;
    (db, chat)
}

/// Seed one user message per offset of [`CHRONOLOGICAL`], out of order, content = the offset.
async fn seed_fractional(db: &TestDb, chat: &ChatRecord) {
    for micros in [
        500_000, 0, 123_456, 1_000_000, 120_000, 999_999, 123_400, 123_000,
    ] {
        seed_msg(
            db,
            chat,
            MessageSeed::new("user", &micros.to_string(), t(micros)),
        )
        .await;
    }
}

fn offsets(micros: &[i64]) -> Vec<String> {
    micros.iter().map(ToString::to_string).collect()
}

fn ordered(field: &str, dir: SortDir) -> ODataQuery {
    ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
        field: field.to_owned(),
        dir,
    }]))
}

fn filtered(raw: &str) -> ODataQuery {
    ODataQuery::new().with_filter(parse_filter_string(raw).unwrap().into_expr())
}

fn literal(micros: i64) -> String {
    t(micros)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

async fn page(db: &TestDb, chat: &ChatRecord, query: &ODataQuery) -> Page<MessageRow> {
    MessageRepo::list_page(&db.provider.conn().unwrap(), chat, query, STORAGE)
        .await
        .unwrap()
}

fn contents(page: &Page<MessageRow>) -> Vec<String> {
    page.items.iter().map(|m| m.content.clone()).collect()
}

fn ids(page: &Page<MessageRow>) -> Vec<Uuid> {
    page.items.iter().map(|m| m.id).collect()
}

// ── the page: order ──────────────────────────────────────────────────────────

#[tokio::test]
async fn the_stored_text_of_the_fixture_is_the_trimmed_non_chronological_form() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    for text in [
        "2023-11-14T22:13:20Z",
        "2023-11-14T22:13:20.12Z",
        "2023-11-14T22:13:20.1234Z",
    ] {
        let count = raw_i64(
            &db.raw,
            &format!("SELECT COUNT(*) FROM messages WHERE created_at = '{text}'"),
        )
        .await;
        assert_eq!(count, 1, "{text} is stored as written by sqlx");
    }
}

#[tokio::test]
async fn the_default_order_is_chronological_whatever_the_fraction_lengths() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    let listed = page(&db, &chat, &ODataQuery::new()).await;
    assert_eq!(contents(&listed), offsets(&CHRONOLOGICAL));
}

#[tokio::test]
async fn created_at_asc_and_desc_are_chronological_and_its_inverse() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    let mut newest_first = CHRONOLOGICAL;
    newest_first.reverse();
    let asc = page(&db, &chat, &ordered("created_at", SortDir::Asc)).await;
    assert_eq!(contents(&asc), offsets(&CHRONOLOGICAL));
    let desc = page(&db, &chat, &ordered("created_at", SortDir::Desc)).await;
    assert_eq!(contents(&desc), offsets(&newest_first));
}

#[tokio::test]
async fn messages_with_the_same_created_at_are_ordered_by_id_in_the_same_direction() {
    let (db, chat) = setup().await;
    let mut seeded = Vec::new();
    for _ in 0..6 {
        seeded.push(seed_msg(&db, &chat, MessageSeed::new("user", "same", t(7))).await);
    }
    seeded.sort();
    let default = page(&db, &chat, &ODataQuery::new()).await;
    assert_eq!(ids(&default), seeded);
    let desc = page(&db, &chat, &ordered("created_at", SortDir::Desc)).await;
    let mut reversed = seeded.clone();
    reversed.reverse();
    assert_eq!(ids(&desc), reversed);
}

#[tokio::test]
async fn the_id_and_role_orders_are_total() {
    let (db, chat) = setup().await;
    for (i, role) in ["user", "assistant", "user", "system", "assistant", "user"]
        .into_iter()
        .enumerate()
    {
        seed_msg(
            &db,
            &chat,
            MessageSeed::new(role, "x", t(i64::try_from(i).unwrap())),
        )
        .await;
    }
    let by_id = page(&db, &chat, &ordered("id", SortDir::Asc)).await;
    let mut sorted = ids(&by_id);
    sorted.sort();
    assert_eq!(ids(&by_id), sorted);
    let by_id_desc = page(&db, &chat, &ordered("id", SortDir::Desc)).await;
    sorted.reverse();
    assert_eq!(ids(&by_id_desc), sorted);

    let by_role = page(&db, &chat, &ordered("role", SortDir::Asc)).await;
    let roles: Vec<&str> = by_role.items.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["assistant", "assistant", "system", "user", "user", "user"]
    );
    // equal roles fall back to the id in the same direction
    let tie: Vec<Uuid> = by_role
        .items
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.id)
        .collect();
    let mut tie_sorted = tie.clone();
    tie_sorted.sort();
    assert_eq!(tie, tie_sorted);
}

// ── the page: rows ───────────────────────────────────────────────────────────

#[tokio::test]
async fn only_live_messages_of_the_chat_are_listed_and_compressed_ones_stay() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    let live = seed_msg(&db, &chat, MessageSeed::new("user", "live", t(1))).await;
    let compressed = seed_msg(
        &db,
        &chat,
        MessageSeed::new("assistant", "compressed", t(2)).compressed(),
    )
    .await;
    seed_msg(
        &db,
        &chat,
        MessageSeed::new("user", "deleted", t(3)).deleted(t(10)),
    )
    .await;
    seed_msg(&db, &other, MessageSeed::new("user", "elsewhere", t(4))).await;
    let listed = page(&db, &chat, &ODataQuery::new()).await;
    assert_eq!(ids(&listed), vec![live, compressed]);
}

#[tokio::test]
async fn a_row_carries_its_columns_and_a_null_request_id_is_returned_as_none() {
    let (db, chat) = setup().await;
    let request_id = Uuid::new_v4();
    let assistant = seed_msg(
        &db,
        &chat,
        MessageSeed::new("assistant", "answer", t(5))
            .request(Some(request_id))
            .model("gpt-5.2")
            .tokens(120, 30),
    )
    .await;
    let keyless = seed_msg(
        &db,
        &chat,
        MessageSeed::new("user", "legacy", t(6)).request(None),
    )
    .await;
    let listed = page(&db, &chat, &ODataQuery::new()).await;
    let row = listed.items.iter().find(|m| m.id == assistant).unwrap();
    assert_eq!(row.request_id, Some(request_id));
    assert_eq!(row.role, "assistant");
    assert_eq!(row.content, "answer");
    assert_eq!(row.model.as_deref(), Some("gpt-5.2"));
    assert_eq!((row.input_tokens, row.output_tokens), (120, 30));
    assert_eq!(row.created_at, t(5));
    let legacy = listed.items.iter().find(|m| m.id == keyless).unwrap();
    assert_eq!(legacy.request_id, None);
}

// ── the page: filters ────────────────────────────────────────────────────────

async fn filtered_contents(db: &TestDb, chat: &ChatRecord, raw: &str) -> Vec<String> {
    contents(&page(db, chat, &filtered(raw)).await)
}

#[tokio::test]
async fn created_at_filters_compare_chronologically_not_as_text() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    let at_123 = literal(123_000);
    let cases: Vec<(String, Vec<i64>)> = vec![
        (
            format!("created_at ge {at_123}"),
            CHRONOLOGICAL[2..].to_vec(),
        ),
        (
            format!("created_at gt {at_123}"),
            CHRONOLOGICAL[3..].to_vec(),
        ),
        (
            format!("created_at le {at_123}"),
            CHRONOLOGICAL[..=2].to_vec(),
        ),
        (
            format!("created_at lt {at_123}"),
            CHRONOLOGICAL[..2].to_vec(),
        ),
        (format!("created_at eq {at_123}"), vec![123_000]),
        (
            format!("created_at ne {at_123}"),
            CHRONOLOGICAL
                .iter()
                .copied()
                .filter(|m| *m != 123_000)
                .collect(),
        ),
        (
            format!("created_at in ({}, {})", literal(0), literal(999_999)),
            vec![0, 999_999],
        ),
        (
            format!(
                "created_at ge {} and created_at lt {}",
                literal(120_000),
                literal(500_000)
            ),
            vec![120_000, 123_000, 123_400, 123_456],
        ),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            filtered_contents(&db, &chat, &raw).await,
            offsets(&expected),
            "{raw}"
        );
    }
}

#[tokio::test]
async fn role_and_id_filters_select_the_expected_rows() {
    let (db, chat) = setup().await;
    let mut by_role = std::collections::BTreeMap::new();
    for (i, role) in ["user", "assistant", "system", "assistant", "user"]
        .into_iter()
        .enumerate()
    {
        let id = seed_msg(
            &db,
            &chat,
            MessageSeed::new(role, &format!("{role}-{i}"), t(i64::try_from(i).unwrap())),
        )
        .await;
        by_role.entry(role).or_insert_with(Vec::new).push(id);
    }
    let role_cases = [
        ("role eq 'assistant'", vec!["assistant-1", "assistant-3"]),
        (
            "role ne 'user'",
            vec!["assistant-1", "system-2", "assistant-3"],
        ),
        ("contains(role, 'sist')", vec!["assistant-1", "assistant-3"]),
        ("startswith(role, 'sys')", vec!["system-2"]),
        ("endswith(role, 'er')", vec!["user-0", "user-4"]),
        (
            "role in ('user', 'system')",
            vec!["user-0", "system-2", "user-4"],
        ),
    ];
    for (raw, expected) in role_cases {
        assert_eq!(filtered_contents(&db, &chat, raw).await, expected, "{raw}");
    }
    let target = by_role["system"][0];
    assert_eq!(
        filtered_contents(&db, &chat, &format!("id eq {target}")).await,
        vec!["system-2"]
    );
    assert_eq!(
        filtered_contents(&db, &chat, &format!("id ne {target}"))
            .await
            .len(),
        4
    );
    let pair = [by_role["user"][0], by_role["user"][1]];
    assert_eq!(
        filtered_contents(&db, &chat, &format!("id in ({}, {})", pair[0], pair[1])).await,
        vec!["user-0", "user-4"]
    );
}

#[tokio::test]
async fn a_filter_never_widens_the_scope_to_other_chats_or_deleted_rows() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    seed_msg(&db, &chat, MessageSeed::new("user", "mine", t(1))).await;
    seed_msg(&db, &other, MessageSeed::new("user", "theirs", t(2))).await;
    seed_msg(
        &db,
        &chat,
        MessageSeed::new("user", "gone", t(3)).deleted(t(9)),
    )
    .await;
    let all = filtered_contents(&db, &chat, "role eq 'user' or role ne 'user'").await;
    assert_eq!(all, vec!["mine"]);
}

#[tokio::test]
async fn a_filter_or_order_outside_the_allowed_fields_is_an_odata_error() {
    let (db, chat) = setup().await;
    seed_msg(&db, &chat, MessageSeed::new("user", "m", t(1))).await;
    let conn = db.provider.conn().unwrap();
    for query in [
        filtered("content eq 'm'"),
        filtered("role gt 'a'"),
        filtered("contains(created_at, 'x')"),
        ordered("content", SortDir::Asc),
        ordered("model", SortDir::Desc),
    ] {
        let outcome = MessageRepo::list_page(&conn, &chat, &query, STORAGE).await;
        assert!(
            matches!(outcome, Err(DomainError::ODataQuery(_))),
            "{:?}: {outcome:?}",
            query.order
        );
    }
}

// ── the page: limit and cursors ──────────────────────────────────────────────

#[tokio::test]
async fn the_page_size_defaults_to_20_and_is_clamped_to_100() {
    let (db, chat) = setup().await;
    for i in 0..105 {
        seed_msg(
            &db,
            &chat,
            MessageSeed::new("user", &format!("{i:03}"), t(i)),
        )
        .await;
    }
    let default = page(&db, &chat, &ODataQuery::new()).await;
    assert_eq!(default.items.len(), 20);
    assert_eq!(default.page_info.limit, 20);
    assert!(default.page_info.next_cursor.is_some());
    assert!(default.page_info.prev_cursor.is_none());
    let clamped = page(&db, &chat, &ODataQuery::new().with_limit(5_000)).await;
    assert_eq!(clamped.items.len(), 100);
    assert_eq!(clamped.page_info.limit, 100);
    let small = page(&db, &chat, &ODataQuery::new().with_limit(3)).await;
    assert_eq!(small.items.len(), 3);
    assert_eq!(small.page_info.limit, 3);
}

/// Follow `next_cursor` until it is null; returns the contents in order.
async fn follow(db: &TestDb, chat: &ChatRecord, first: ODataQuery) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = page(db, chat, &first).await;
    for _ in 0..50 {
        out.extend(contents(&current));
        match current.page_info.next_cursor.clone() {
            Some(token) => {
                let query = ODataQuery::new()
                    .with_limit(first.limit.unwrap_or(20))
                    .with_cursor(CursorV1::decode(&token).unwrap());
                current = page(db, chat, &query).await;
            }
            None => return out,
        }
    }
    panic!("the cursor never ended");
}

#[tokio::test]
async fn cursor_pages_are_chronological_without_gaps_or_repeats() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    assert_eq!(
        follow(&db, &chat, ODataQuery::new().with_limit(3)).await,
        offsets(&CHRONOLOGICAL)
    );
    let mut newest_first = CHRONOLOGICAL;
    newest_first.reverse();
    assert_eq!(
        follow(
            &db,
            &chat,
            ordered("created_at", SortDir::Desc).with_limit(3)
        )
        .await,
        offsets(&newest_first)
    );
}

#[tokio::test]
async fn cursor_pages_hold_across_equal_created_at_values() {
    let (db, chat) = setup().await;
    let mut expected = Vec::new();
    for i in 0..7 {
        let id = seed_msg(&db, &chat, MessageSeed::new("user", &format!("m{i}"), t(5))).await;
        expected.push((id, format!("m{i}")));
    }
    expected.sort();
    let want: Vec<String> = expected.into_iter().map(|(_, text)| text).collect();
    assert_eq!(
        follow(&db, &chat, ODataQuery::new().with_limit(2)).await,
        want
    );
}

#[tokio::test]
async fn the_cursor_serves_previous_page_navigation() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    let first = page(&db, &chat, &ODataQuery::new().with_limit(3)).await;
    assert!(first.page_info.prev_cursor.is_none());
    let second = page(
        &db,
        &chat,
        &ODataQuery::new().with_limit(3).with_cursor(
            CursorV1::decode(first.page_info.next_cursor.as_deref().unwrap()).unwrap(),
        ),
    )
    .await;
    assert_eq!(contents(&second), offsets(&CHRONOLOGICAL[3..6]));
    let back = page(
        &db,
        &chat,
        &ODataQuery::new().with_limit(3).with_cursor(
            CursorV1::decode(second.page_info.prev_cursor.as_deref().unwrap()).unwrap(),
        ),
    )
    .await;
    assert_eq!(contents(&back), contents(&first));
}

// ── attachment and reaction batches ──────────────────────────────────────────

#[tokio::test]
async fn the_attachment_batch_joins_non_deleted_attachments_of_the_requested_messages() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    let m1 = seed_msg(&db, &chat, MessageSeed::new("user", "one", t(1))).await;
    let m2 = seed_msg(&db, &chat, MessageSeed::new("user", "two", t(2))).await;
    let m3 = seed_msg(&db, &chat, MessageSeed::new("user", "three", t(3))).await;
    let foreign_message = seed_msg(&db, &other, MessageSeed::new("user", "x", t(4))).await;

    let image = seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("image")
            .filename("cat.png")
            .thumbnail(64, 48, &[9, 8, 7]),
    )
    .await;
    let document = seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document").filename("a.pdf"),
    )
    .await;
    let gone = seed_att(&db, &chat, AttachmentSeed::ready("image").deleted(t(5))).await;
    let uploading = seed_att(
        &db,
        &chat,
        AttachmentSeed::ready("document").status("uploaded"),
    )
    .await;
    let foreign_att = seed_att(&db, &other, AttachmentSeed::ready("document")).await;
    for (message, attachment) in [
        (m1, image),
        (m1, document),
        (m1, gone),
        (m2, uploading),
        (foreign_message, foreign_att),
    ] {
        let owner = if message == foreign_message {
            &other
        } else {
            &chat
        };
        link_att(&db, owner, message, attachment).await;
    }

    let conn = db.provider.conn().unwrap();
    let rows = MessageRepo::attachment_batch(&conn, &chat, &[m1, m2, m3])
        .await
        .unwrap();
    let mut got: Vec<(Uuid, Uuid)> = rows
        .iter()
        .map(|r| (r.message_id, r.attachment_id))
        .collect();
    got.sort();
    let mut want = vec![(m1, image), (m1, document), (m2, uploading)];
    want.sort();
    assert_eq!(
        got, want,
        "the deleted attachment and the foreign chat are not listed"
    );

    let image_row = rows.iter().find(|r| r.attachment_id == image).unwrap();
    assert_eq!(image_row.kind, "image");
    assert_eq!(image_row.filename, "cat.png");
    assert_eq!(image_row.status, "ready");
    let thumb = image_row.thumbnail.as_ref().expect("stored thumbnail");
    assert_eq!((thumb.width, thumb.height), (64, 48));
    assert_eq!(thumb.data, vec![9, 8, 7]);
    let document_row = rows.iter().find(|r| r.attachment_id == document).unwrap();
    assert_eq!(document_row.kind, "document");
    assert_eq!(document_row.thumbnail, None);
    let uploading_row = rows.iter().find(|r| r.attachment_id == uploading).unwrap();
    assert_eq!(uploading_row.status, "uploaded");

    let only_m2 = MessageRepo::attachment_batch(&conn, &chat, &[m2])
        .await
        .unwrap();
    assert_eq!(only_m2.len(), 1);
    assert!(
        MessageRepo::attachment_batch(&conn, &chat, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_reaction_batch_holds_only_the_callers_reactions_of_the_requested_messages() {
    let (db, chat) = setup().await;
    let liked = seed_msg(&db, &chat, MessageSeed::new("assistant", "a", t(1))).await;
    let disliked = seed_msg(&db, &chat, MessageSeed::new("assistant", "b", t(2))).await;
    let others_only = seed_msg(&db, &chat, MessageSeed::new("assistant", "c", t(3))).await;
    let unrequested = seed_msg(&db, &chat, MessageSeed::new("assistant", "d", t(4))).await;
    seed_reaction_of(&db, TENANT_A, USER_A1, liked, "like").await;
    seed_reaction_of(&db, TENANT_A, USER_A1, disliked, "dislike").await;
    seed_reaction_of(&db, TENANT_A, USER_A2, liked, "dislike").await;
    seed_reaction_of(&db, TENANT_A, USER_A2, others_only, "like").await;
    seed_reaction_of(&db, TENANT_A, USER_A1, unrequested, "like").await;

    let conn = db.provider.conn().unwrap();
    let reactions =
        MessageRepo::reaction_batch(&conn, &chat, USER_A1, &[liked, disliked, others_only])
            .await
            .unwrap();
    assert_eq!(reactions.len(), 2, "{reactions:?}");
    assert_eq!(reactions[&liked], Reaction::Like);
    assert_eq!(reactions[&disliked], Reaction::Dislike);
    assert!(
        MessageRepo::reaction_batch(&conn, &chat, USER_A1, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

// ── the snapshot boundary ────────────────────────────────────────────────────

#[tokio::test]
async fn a_chat_without_messages_has_no_snapshot_boundary() {
    let (db, chat) = setup().await;
    let boundary = MessageRepo::snapshot_boundary(&db.provider.conn().unwrap(), &chat, STORAGE)
        .await
        .unwrap();
    assert_eq!(boundary, None);
}

#[tokio::test]
async fn the_boundary_is_the_chronologically_latest_live_message() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    seed_fractional(&db, &chat).await;
    // later messages that must not count: deleted, and in another chat
    seed_msg(
        &db,
        &chat,
        MessageSeed::new("user", "deleted", t(5_000_000)).deleted(t(9_000_000)),
    )
    .await;
    seed_msg(
        &db,
        &other,
        MessageSeed::new("user", "elsewhere", t(6_000_000)),
    )
    .await;
    let latest = page(
        &db,
        &chat,
        &ordered("created_at", SortDir::Desc).with_limit(1),
    )
    .await;
    let latest = &latest.items[0];
    assert_eq!(latest.content, "1000000");

    let boundary = MessageRepo::snapshot_boundary(&db.provider.conn().unwrap(), &chat, STORAGE)
        .await
        .unwrap()
        .expect("a boundary");
    assert_eq!(
        boundary,
        SnapshotBoundary {
            created_at: t(1_000_000),
            id: latest.id
        }
    );
}

#[tokio::test]
async fn the_boundary_compares_fractions_of_different_lengths_chronologically() {
    let (db, chat) = setup().await;
    // `…20.12Z` is the later text but the earlier instant than `…20.123Z`
    seed_msg(&db, &chat, MessageSeed::new("user", "earlier", t(120_000))).await;
    let later = seed_msg(
        &db,
        &chat,
        MessageSeed::new("assistant", "later", t(123_000)),
    )
    .await;
    let boundary = MessageRepo::snapshot_boundary(&db.provider.conn().unwrap(), &chat, STORAGE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(boundary.id, later);
    assert_eq!(boundary.created_at, t(123_000));
}

#[tokio::test]
async fn equal_timestamps_resolve_the_boundary_by_the_greatest_id() {
    let (db, chat) = setup().await;
    let mut seeded = Vec::new();
    for _ in 0..4 {
        seeded.push(seed_msg(&db, &chat, MessageSeed::new("user", "tie", t(3))).await);
    }
    let boundary = MessageRepo::snapshot_boundary(&db.provider.conn().unwrap(), &chat, STORAGE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(boundary.id, *seeded.iter().max().unwrap());
}

// ── the recent messages ──────────────────────────────────────────────────────

async fn recent(
    db: &TestDb,
    chat: &ChatRecord,
    boundary: Option<&SnapshotBoundary>,
    summary: Option<&crate::domain::streaming::ThreadSummaryRow>,
    limit: u32,
) -> Vec<ContextMessage> {
    MessageRepo::recent_messages(
        &db.provider.conn().unwrap(),
        chat,
        boundary,
        summary,
        limit,
        STORAGE,
    )
    .await
    .unwrap()
}

fn texts(messages: &[ContextMessage]) -> Vec<String> {
    messages.iter().map(|m| m.content.clone()).collect()
}

#[tokio::test]
async fn the_newest_messages_are_selected_and_returned_oldest_first() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    let all = recent(&db, &chat, None, None, 100).await;
    assert_eq!(texts(&all), offsets(&CHRONOLOGICAL));
    let newest_three = recent(&db, &chat, None, None, 3).await;
    assert_eq!(texts(&newest_three), offsets(&CHRONOLOGICAL[5..]));
}

#[tokio::test]
async fn the_limit_bounds_the_selection_and_zero_selects_nothing() {
    let (db, chat) = setup().await;
    for i in 0..105 {
        seed_msg(
            &db,
            &chat,
            MessageSeed::new("user", &format!("{i:03}"), t(i)),
        )
        .await;
    }
    assert!(recent(&db, &chat, None, None, 0).await.is_empty());
    assert_eq!(recent(&db, &chat, None, None, 100).await.len(), 100);
    let ten = recent(&db, &chat, None, None, 10).await;
    assert_eq!(ten.len(), 10);
    assert_eq!(ten[0].content, "095");
    assert_eq!(ten[9].content, "104");
}

#[tokio::test]
async fn messages_without_a_request_id_deleted_or_compressed_are_never_selected() {
    let (db, chat) = setup().await;
    seed_msg(&db, &chat, MessageSeed::new("user", "kept-1", t(1))).await;
    seed_msg(
        &db,
        &chat,
        MessageSeed::new("user", "legacy", t(2)).request(None),
    )
    .await;
    seed_msg(
        &db,
        &chat,
        MessageSeed::new("assistant", "deleted", t(3)).deleted(t(9)),
    )
    .await;
    seed_msg(
        &db,
        &chat,
        MessageSeed::new("user", "compressed", t(4)).compressed(),
    )
    .await;
    seed_msg(&db, &chat, MessageSeed::new("assistant", "kept-2", t(5))).await;
    let selected = recent(&db, &chat, None, None, 10).await;
    assert_eq!(texts(&selected), vec!["kept-1", "kept-2"]);
    assert_eq!(selected[0].role, ContextRole::User);
    assert_eq!(selected[1].role, ContextRole::Assistant);
    assert_eq!(selected[0].created_at, t(1));
}

#[tokio::test]
async fn the_boundary_is_inclusive_and_later_messages_never_enter() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    let boundary_message = {
        let listed = page(&db, &chat, &ODataQuery::new()).await;
        listed
            .items
            .iter()
            .find(|m| m.content == "123000")
            .unwrap()
            .clone()
    };
    let boundary = SnapshotBoundary {
        created_at: boundary_message.created_at,
        id: boundary_message.id,
    };
    // a message written after the boundary was computed
    seed_msg(&db, &chat, MessageSeed::new("user", "later", t(9_000_000))).await;
    let selected = recent(&db, &chat, Some(&boundary), None, 100).await;
    // `…20.12Z` (earlier instant, later text) is in; `…20.1234Z` and later are out
    assert_eq!(texts(&selected), offsets(&CHRONOLOGICAL[..=2]));
}

#[tokio::test]
async fn at_equal_timestamps_the_boundary_id_decides() {
    let (db, chat) = setup().await;
    let mut seeded = Vec::new();
    for i in 0..5 {
        let id = seed_msg(
            &db,
            &chat,
            MessageSeed::new("user", &format!("tie-{i}"), t(4)),
        )
        .await;
        seeded.push(id);
    }
    seeded.sort();
    let boundary = SnapshotBoundary {
        created_at: t(4),
        id: seeded[2],
    };
    let selected = recent(&db, &chat, Some(&boundary), None, 100).await;
    let mut got: Vec<Uuid> = selected.iter().map(|m| m.id).collect();
    got.sort();
    assert_eq!(got, seeded[..=2].to_vec());
    // chronological order with equal instants is the id order
    let ordered_ids: Vec<Uuid> = selected.iter().map(|m| m.id).collect();
    assert_eq!(ordered_ids, seeded[..=2].to_vec());
}

#[tokio::test]
async fn the_summary_frontier_is_exclusive() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    let listed = page(&db, &chat, &ODataQuery::new()).await;
    let frontier_message = listed.items.iter().find(|m| m.content == "123000").unwrap();
    seed_summary(
        &db,
        &chat,
        "summary",
        11,
        (frontier_message.created_at, frontier_message.id),
    )
    .await;
    let summary = MessageRepo::thread_summary(&db.provider.conn().unwrap(), &chat)
        .await
        .unwrap()
        .expect("a summary");
    let selected = recent(&db, &chat, None, Some(&summary), 100).await;
    // everything after the frontier message, nothing at or before it (also `…20.12Z`)
    assert_eq!(texts(&selected), offsets(&CHRONOLOGICAL[3..]));
}

#[tokio::test]
async fn without_a_summary_the_frontier_predicate_is_omitted() {
    let (db, chat) = setup().await;
    seed_fractional(&db, &chat).await;
    let selected = recent(&db, &chat, None, None, 100).await;
    assert_eq!(selected.len(), CHRONOLOGICAL.len());
}

// ── the thread summary and single messages ───────────────────────────────────

#[tokio::test]
async fn a_chat_without_a_summary_has_none_and_a_summary_row_maps_its_columns() {
    let (db, chat) = setup().await;
    let conn = db.provider.conn().unwrap();
    assert_eq!(
        MessageRepo::thread_summary(&conn, &chat).await.unwrap(),
        None
    );
    let anchor = seed_msg(&db, &chat, MessageSeed::new("assistant", "anchor", t(5))).await;
    seed_summary(&db, &chat, "what happened so far", 42, (t(5), anchor)).await;
    let summary = MessageRepo::thread_summary(&conn, &chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(summary.summary_text, "what happened so far");
    assert_eq!(summary.token_estimate, 42);
    assert_eq!(summary.frontier_created_at, t(5));
    assert_eq!(summary.frontier_message_id, anchor);
}

#[tokio::test]
async fn a_single_message_is_found_only_when_live_and_in_the_chat() {
    let (db, chat) = setup().await;
    let other = seed_chat(&db, &a1(), Uuid::new_v4(), Some("other"), "m", at(0)).await;
    let live = seed_msg(&db, &chat, MessageSeed::new("assistant", "here", t(1))).await;
    let deleted = seed_msg(
        &db,
        &chat,
        MessageSeed::new("assistant", "gone", t(2)).deleted(t(9)),
    )
    .await;
    let elsewhere = seed_msg(&db, &other, MessageSeed::new("assistant", "there", t(3))).await;
    let conn = db.provider.conn().unwrap();
    let found = MessageRepo::find_message(&conn, &chat, live)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.content, "here");
    for id in [deleted, elsewhere, Uuid::new_v4()] {
        assert_eq!(
            MessageRepo::find_message(&conn, &chat, id).await.unwrap(),
            None
        );
    }
}
