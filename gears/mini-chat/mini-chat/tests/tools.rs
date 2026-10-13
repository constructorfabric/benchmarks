//! T054: attachments made available to provider tools; citations mapping; tool guidance.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

fn tool<'a>(req: &'a Value, ty: &str) -> Option<&'a Value> {
    req["tools"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["type"] == ty))
}

#[tokio::test]
async fn no_tools_without_ready_documents() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.send(chat, "plain").await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(
        tool(&req, "file_search").is_none() && tool(&req, "code_interpreter").is_none(),
        "{req}"
    );
    assert_eq!(req["metadata"]["feature"], "none");
    assert!(
        !req["instructions"]
            .as_str()
            .unwrap()
            .contains("file_search")
    );
}

#[tokio::test]
async fn file_search_tool_with_ready_documents_and_citations() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let doc = h
        .upload_ok(chat, "report.pdf", "application/pdf", b"%PDF-1.4 fake")
        .await;
    let row = db::attachment(&h, doc).await;
    let file_id = row.provider_file_id.clone().unwrap();
    let vs = h
        .provider
        .requests()
        .into_iter()
        .filter(|r| r.path.contains("/vector_stores/"))
        .map(|r| r.path)
        .next()
        .unwrap();
    let vs_id = vs
        .split("/vector_stores/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();

    let output = json!([{"type": "message", "content": [{"type": "output_text", "text": "From the report.",
    "annotations": [
        {"type": "file_citation", "file_id": file_id, "filename": "provider-name.pdf", "index": 3},
        {"type": "file_citation", "file_id": "file-unknownunknown123", "filename": "x", "index": 3}
    ]}]}]);
    h.provider.push(Script::Ok {
        parts: vec!["From the report.".into()],
        usage: (10, 5),
        before_done: vec![
            (
                "response.file_search_call.searching".into(),
                json!({"type": "response.file_search_call.searching"}),
            ),
            (
                "response.file_search_call.completed".into(),
                json!({"type": "response.file_search_call.completed", "results": [{}, {}]}),
            ),
        ],
        output: Some(output),
    });
    let r = h.send(chat, "summarize the report").await;
    let req = h.provider.chat_requests().pop().unwrap();
    let fs = tool(&req, "file_search").expect("file_search tool");
    assert_eq!(fs["vector_store_ids"], json!([vs_id]));
    assert_eq!(fs["max_num_results"], 4);
    assert_eq!(req["metadata"]["feature"], "file_search");
    assert!(
        req["instructions"].as_str().unwrap().len() > "You are prem.".len(),
        "file search guard appended"
    );

    let tools: Vec<&Value> = r
        .events
        .iter()
        .filter(|(n, _)| n == "tool")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(tools[0]["name"], "file_search");
    assert_eq!(tools[0]["phase"], "start");
    assert_eq!(tools[1]["phase"], "done");
    assert_eq!(tools[1]["details"]["files_searched"], 2);
    let items = r.first("citations").expect("citations")["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        items.len(),
        1,
        "unknown provider files are omitted: {items:?}"
    );
    assert_eq!(items[0]["source"], "file");
    assert_eq!(items[0]["attachment_id"], doc.to_string());
    assert_eq!(items[0]["title"], "report.pdf");
    assert_eq!(items[0]["snippet"], "");
    assert!(items[0].get("url").is_none());

    // kill switch removes the tool
    let mut p = default_policy();
    p["kill_switches"] = json!({"disable_file_search": true});
    h.set_policy(p);
    h.send(chat, "again").await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(tool(&req, "file_search").is_none());
}

#[tokio::test]
async fn images_are_sent_as_input_image_file_ids() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let a = h.upload_ok(chat, "a.png", "image/png", &png(4, 4)).await;
    let b = h.upload_ok(chat, "b.jpg", "image/png", &png(4, 4)).await;
    let fa = db::attachment(&h, a).await.provider_file_id.unwrap();
    let fb = db::attachment(&h, b).await.provider_file_id.unwrap();
    h.send_body(
        chat,
        json!({"content": "compare", "attachment_ids": [a, b]}),
    )
    .await;
    let req = h.provider.chat_requests().pop().unwrap();
    let user = req["input"].as_array().unwrap().last().unwrap().clone();
    let content = user["content"].as_array().unwrap();
    assert_eq!(content[0], json!({"type": "input_text", "text": "compare"}));
    let ids: Vec<&str> = content
        .iter()
        .filter(|c| c["type"] == "input_image")
        .map(|c| c["file_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![fa.as_str(), fb.as_str()]);
    // images alone do not enable file_search
    assert!(tool(&req, "file_search").is_none());
}

#[tokio::test]
async fn code_interpreter_container_with_xlsx_files() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let x = h.upload_ok(chat, "t.xlsx", XLSX, b"PK").await;
    let fid = db::attachment(&h, x).await.provider_file_id.unwrap();
    h.provider.push(Script::Ok {
        parts: vec!["done".into()],
        usage: (10, 5),
        before_done: vec![
            ("response.code_interpreter_call.in_progress".into(), json!({"type": "response.code_interpreter_call.in_progress"})),
            ("response.output_item.done".into(), json!({"type": "response.output_item.done", "item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "a"}, {"type": "logs", "logs": "b"}]}})),
        ],
        output: None,
    });
    let r = h.send(chat, "analyze").await;
    let req = h.provider.chat_requests().pop().unwrap();
    let ci = tool(&req, "code_interpreter").expect("code_interpreter tool");
    assert_eq!(ci["container"], json!({"type": "auto", "file_ids": [fid]}));
    assert!(
        req["include"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i == "code_interpreter_call.outputs")
    );
    assert!(
        tool(&req, "file_search").is_none(),
        "xlsx is code-interpreter only"
    );
    let tools: Vec<&Value> = r
        .events
        .iter()
        .filter(|(n, _)| n == "tool")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(tools[0]["name"], "code_interpreter");
    assert_eq!(tools[1]["phase"], "done");
    assert_eq!(tools[1]["details"]["output"], "a\nb");
    let turns = db::turns(&h, chat).await;
    assert_eq!(turns[0].code_interpreter_completed_count, 1);
}

#[tokio::test]
async fn code_interpreter_per_message_limit() {
    let mut cfg = default_config();
    cfg["quota"] = json!({"code_interpreter_max_calls_per_message": 1});
    let h = Harness::with(Opts {
        config: cfg,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    h.upload_ok(chat, "t.xlsx", XLSX, b"PK").await;
    let start = (
        "response.code_interpreter_call.in_progress".to_owned(),
        json!({"type": "response.code_interpreter_call.in_progress"}),
    );
    h.provider.push(Script::Ok {
        parts: vec![],
        usage: (1, 1),
        before_done: vec![start.clone(), start],
        output: None,
    });
    let r = h.send(chat, "x").await;
    assert_eq!(
        r.first("error").unwrap()["code"],
        "code_interpreter_calls_exceeded"
    );
    let t = h.wait_turn_terminal(chat).await;
    assert_eq!(
        t[0].error_code.as_deref(),
        Some("code_interpreter_calls_exceeded")
    );
}

#[tokio::test]
async fn unexpected_function_tool_use_fails_turn() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec![],
        usage: (1, 1),
        before_done: vec![("response.output_item.added".into(), json!({"type": "response.output_item.added", "item": {"type": "function_call", "name": "x"}}))],
        output: None,
    });
    let r = h.send(chat, "x").await;
    assert_eq!(r.first("error").unwrap()["code"], "unexpected_tool_use");
    let _ = Uuid::nil();
}
