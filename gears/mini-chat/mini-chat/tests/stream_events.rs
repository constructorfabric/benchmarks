//! Translation of provider events into the stable SSE events: `tool` phases,
//! ignored provider noise, event-name fallback and the `citations` event.
//!
//! Source of truth: DESIGN 3.3 "SSE Event Definitions" (`tool`, `citations`),
//! "SSE Event Ordering", "Provider Event Translation" (mapping table, event
//! name taken from the `type` field when the `event:` line is missing or
//! `message`), 4 "Citation File ID and Title Resolution" (provider file id to
//! `attachment_id` / filename, restricted to the chat, deleted attachments
//! omitted) and the OpenAPI schemas `ToolData`, `CitationsData`, `Citation`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;
mod stream_support;

use serde_json::{Value, json};
use stream_support::{
    Opts, Piece, ProviderReply, StreamHarness, code_interpreter_pieces, ev, file_annotation,
    file_search_pieces, responses_stream, web_annotation, web_search_pieces,
};

async fn run(h: &StreamHarness, opts: Opts, chunks: &[&str]) -> stream_support::Sent {
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(chunks, &opts));
    let sent = h.say(chat, "go").await;
    sent.assert_well_formed();
    sent
}

fn tool_events(sent: &stream_support::Sent) -> Vec<Value> {
    sent.of("tool").iter().map(|e| e.data.clone()).collect()
}

// ── tool events ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn file_search_phases_become_tool_start_and_done() {
    let h = StreamHarness::new().await;
    let sent = run(
        &h,
        Opts::default().insert_after(0, file_search_pieces(None)),
        &["answer"],
    )
    .await;
    assert_eq!(
        sent.names_no_ping(),
        vec!["stream_started", "tool", "tool", "delta", "done"]
    );
    assert_eq!(
        tool_events(&sent),
        vec![
            json!({"phase": "start", "name": "file_search", "details": {}}),
            json!({"phase": "done", "name": "file_search", "details": {"files_searched": 0}}),
        ]
    );
}

#[tokio::test]
async fn files_searched_is_the_number_of_results_of_the_completed_event() {
    let h = StreamHarness::new().await;
    let sent = run(
        &h,
        Opts::default().insert_after(0, file_search_pieces(Some(3))),
        &["answer"],
    )
    .await;
    assert_eq!(
        tool_events(&sent)[1]["details"],
        json!({"files_searched": 3})
    );
}

#[tokio::test]
async fn web_search_phases_become_tool_start_and_done_with_empty_details() {
    let h = StreamHarness::new().await;
    let sent = run(
        &h,
        Opts::default().insert_after(0, web_search_pieces()),
        &["answer"],
    )
    .await;
    assert_eq!(
        tool_events(&sent),
        vec![
            json!({"phase": "start", "name": "web_search", "details": {}}),
            json!({"phase": "done", "name": "web_search", "details": {}}),
        ]
    );
}

#[tokio::test]
async fn code_interpreter_reports_a_start_and_a_done_with_the_log_output_only() {
    let h = StreamHarness::new().await;
    let sent = run(
        &h,
        Opts::default().insert_after(0, code_interpreter_pieces("42\nforty-two")),
        &["answer"],
    )
    .await;
    // `interpreting` and `completed` are ignored, `output_item.done` carries the logs
    assert_eq!(
        tool_events(&sent),
        vec![
            json!({"phase": "start", "name": "code_interpreter", "details": {}}),
            json!({"phase": "done", "name": "code_interpreter",
                   "details": {"output": "42\nforty-two"}}),
        ]
    );
}

#[tokio::test]
async fn long_code_interpreter_output_is_truncated_with_a_suffix() {
    let h = StreamHarness::new().await;
    let sent = run(
        &h,
        Opts::default().insert_after(0, code_interpreter_pieces(&"z".repeat(20_000))),
        &["answer"],
    )
    .await;
    let output = tool_events(&sent)[1]["details"]["output"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(output.ends_with("...[truncated]"));
    assert!(output.chars().count() <= 8192 + "...[truncated]".len());
}

#[tokio::test]
async fn tool_and_delta_events_interleave_in_provider_order() {
    let h = StreamHarness::new().await;
    let sent = run(
        &h,
        Opts::default()
            .insert_after(1, web_search_pieces())
            .insert_after(2, file_search_pieces(None)),
        &["one ", "two ", "three"],
    )
    .await;
    assert_eq!(
        sent.names_no_ping(),
        vec![
            "stream_started",
            "delta",
            "tool",
            "tool",
            "delta",
            "tool",
            "tool",
            "delta",
            "done"
        ]
    );
    assert_eq!(sent.text(), "one two three");
}

#[tokio::test]
async fn completed_tool_calls_are_counted_on_the_turn() {
    let h = StreamHarness::new().await;
    let mut pieces = file_search_pieces(None);
    pieces.extend(web_search_pieces());
    pieces.extend(code_interpreter_pieces("ok"));
    let sent = run(&h, Opts::default().insert_after(0, pieces), &["answer"]).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.file_search_completed_count, 1);
    assert_eq!(turn.web_search_completed_count, 1);
    assert_eq!(turn.code_interpreter_completed_count, 1);
}

#[tokio::test]
async fn provider_lifecycle_noise_produces_no_client_event() {
    let h = StreamHarness::new().await;
    let noise = vec![
        ev(
            "response.content_part.added",
            json!({"type": "response.content_part.added"}),
        ),
        ev(
            "response.output_item.added",
            json!({"type": "response.output_item.added"}),
        ),
        ev(
            "response.reasoning_summary_text.delta",
            json!({"type": "response.reasoning_summary_text.delta", "delta": "thinking"}),
        ),
        ev(
            "response.content_part.done",
            json!({"type": "response.content_part.done"}),
        ),
        ev(
            "response.output_item.done",
            json!({"type": "response.output_item.done",
                   "item": {"type": "message", "role": "assistant", "content": []}}),
        ),
        ev(
            "response.some_future_event",
            json!({"type": "response.some_future_event"}),
        ),
    ];
    let sent = run(&h, Opts::default().insert_after(0, noise), &["only text"]).await;
    assert_eq!(
        sent.names_no_ping(),
        vec!["stream_started", "delta", "done"]
    );
    assert_eq!(sent.text(), "only text");
}

#[tokio::test]
async fn the_event_name_falls_back_to_the_type_field_when_the_event_line_is_missing_or_message() {
    for line in [None, Some("message")] {
        let h = StreamHarness::new().await;
        let chat = h.seed_chat().await;
        let frames: Vec<Piece> = responses_stream(&["na", "meless"], &Opts::default())
            .into_iter()
            .map(|piece| match piece {
                Piece::Event(_, data) => Piece::Raw(match line {
                    Some(name) => format!("event: {name}\ndata: {data}\n\n"),
                    None => format!("data: {data}\n\n"),
                }),
                other => other,
            })
            .collect();
        h.oagw.push(ProviderReply::sse(frames));
        let sent = h.say(chat, "go").await;
        sent.assert_well_formed();
        assert_eq!(
            sent.names_no_ping(),
            vec!["stream_started", "delta", "delta", "done"],
            "{line:?}"
        );
        assert_eq!(sent.text(), "nameless");
        assert_eq!(sent.done()["usage"]["output_tokens"], json!(5));
    }
}

// ── citations ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_web_annotation_becomes_one_citations_event_before_done() {
    let h = StreamHarness::new().await;
    let text = "Rust 1.80 was released in July 2024.";
    let sent = run(
        &h,
        Opts::default().annotated(vec![web_annotation(
            "https://example.com/rust",
            "Rust blog",
            0,
            8,
        )]),
        &["Rust 1.80 was released ", "in July 2024."],
    )
    .await;
    assert_eq!(sent.text(), text);
    assert_eq!(
        sent.names_no_ping(),
        vec!["stream_started", "delta", "delta", "citations", "done"]
    );
    assert_eq!(
        sent.of("citations")[0].data,
        json!({"items": [{
            "source": "web",
            "title": "Rust blog",
            "url": "https://example.com/rust",
            "snippet": "Rust 1.8",
            "span": {"start": 0, "end": 8}
        }]})
    );
    // citations are not persisted
    let rows = h.chat.all_messages().await;
    assert!(rows.iter().all(|m| !m.content.contains("example.com")));
}

#[tokio::test]
async fn no_annotations_means_no_citations_event() {
    let h = StreamHarness::new().await;
    let sent = run(&h, Opts::default(), &["plain"]).await;
    assert!(sent.of("citations").is_empty());
}

#[tokio::test]
async fn a_file_annotation_maps_the_provider_file_to_the_attachment_and_its_filename() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.seed_document(chat, "Q3 Report.pdf").await;
    h.seed_vector_store(chat, "vs_seededstore0101").await;
    let provider_file = h.chat.attachment_row(doc).await.provider_file_id.unwrap();

    h.oagw.push(ProviderReply::text_with(
        &["see the report"],
        &Opts::default().annotated(vec![file_annotation(&provider_file, "provider-name.pdf")]),
    ));
    let sent = h.say(chat, "what is in it?").await;
    sent.assert_well_formed();
    assert_eq!(
        sent.of("citations")[0].data,
        json!({"items": [{
            "source": "file",
            "title": "Q3 Report.pdf",
            "attachment_id": doc.to_string(),
            "snippet": ""
        }]}),
        "title is the stored filename, no span, no url, no provider id"
    );
    assert!(!sent.blob().contains(&provider_file));
}

#[tokio::test]
async fn file_citations_of_unknown_deleted_or_foreign_files_are_omitted() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let kept = h.seed_document(chat, "kept.pdf").await;
    let deleted = h
        .chat
        .seed_attachment(chat_support::AttachmentSeed::document(chat, "deleted.pdf").deleted())
        .await;
    h.seed_vector_store(chat, "vs_seededstore0102").await;
    let other_chat = h.seed_chat().await;
    let foreign = h.seed_document(other_chat, "foreign.pdf").await;

    let provider_id = |id| {
        let rows = h.chat.all_attachments();
        async move {
            rows.await
                .into_iter()
                .find(|a| a.id == id)
                .and_then(|a| a.provider_file_id)
                .unwrap()
        }
    };
    let deleted_file = provider_id(deleted).await;
    let foreign_file = provider_id(foreign).await;
    let kept_file = provider_id(kept).await;
    h.oagw.push(ProviderReply::text_with(
        &["answer"],
        &Opts::default().annotated(vec![
            file_annotation("file-unknownunknown0000", "x.pdf"),
            file_annotation(&deleted_file, "deleted.pdf"),
            file_annotation(&foreign_file, "foreign.pdf"),
            file_annotation(&kept_file, "kept.pdf"),
        ]),
    ));
    let sent = h.say(chat, "q").await;
    let items = sent.of("citations")[0].data["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        items.len(),
        1,
        "only the live attachment of this chat is cited: {items:?}"
    );
    assert_eq!(items[0]["attachment_id"], json!(kept.to_string()));
}

#[tokio::test]
async fn when_nothing_can_be_mapped_no_citations_event_is_sent() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_document(chat, "a.pdf").await;
    h.seed_vector_store(chat, "vs_seededstore0103").await;
    h.oagw.push(ProviderReply::text_with(
        &["answer"],
        &Opts::default().annotated(vec![file_annotation("file-unknownunknown0001", "x.pdf")]),
    ));
    let sent = h.say(chat, "q").await;
    assert!(sent.of("citations").is_empty(), "{:?}", sent.names());
    assert_eq!(sent.terminal().event, "done");
}

#[tokio::test]
async fn file_citations_need_file_search_on_the_turn() {
    // the attachment lookup only exists when file_search is part of the request
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.seed_document(chat, "a.pdf").await; // ready, but no vector store row
    let provider_file = h.chat.attachment_row(doc).await.provider_file_id.unwrap();
    h.oagw.push(ProviderReply::text_with(
        &["answer"],
        &Opts::default().annotated(vec![file_annotation(&provider_file, "a.pdf")]),
    ));
    let sent = h.say(chat, "q").await;
    assert!(sent.of("citations").is_empty());
}

#[tokio::test]
async fn web_and_file_citations_share_one_event_in_annotation_order() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.seed_document(chat, "a.pdf").await;
    h.seed_vector_store(chat, "vs_seededstore0104").await;
    let provider_file = h.chat.attachment_row(doc).await.provider_file_id.unwrap();
    h.oagw.push(ProviderReply::text_with(
        &["0123456789"],
        &Opts::default().annotated(vec![
            web_annotation("https://one.example/", "One", 0, 3),
            file_annotation(&provider_file, "ignored.pdf"),
            web_annotation("https://two.example/", "Two", 3, 6),
        ]),
    ));
    let sent = h
        .send(
            chat,
            &json!({"content": "q", "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(
        sent.of("citations").len(),
        1,
        "one citations event per stream"
    );
    let items = sent.of("citations")[0].data["items"]
        .as_array()
        .unwrap()
        .clone();
    let sources: Vec<&str> = items
        .iter()
        .map(|i| i["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, vec!["web", "file", "web"]);
    assert_eq!(items[0]["snippet"], json!("012"));
    assert_eq!(items[2]["span"], json!({"start": 3, "end": 6}));
}
