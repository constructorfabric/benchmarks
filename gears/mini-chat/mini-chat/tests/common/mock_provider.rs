//! In-process mock of an OpenAI-compatible provider (Responses API, Files, Vector Stores).
//!
//! The streamed answer is controlled by markers in the last user message:
//! `[[error]]`, `[[429]]`, `[[500]]`, `[[web_search]]`, `[[web_search3]]`, `[[file_search]]`,
//! `[[code]]`, `[[slow]]`, `[[hang]]`, `[[incomplete]]`, `[[empty]]`, `[[ctx_error]]`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use uuid::Uuid;

/// A recorded request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

/// Behavior knobs.
#[derive(Debug, Clone)]
pub struct MockConfig {
    /// Status reported for vector store files (`completed`, `in_progress`, `failed`).
    pub index_status: String,
    /// HTTP status of `POST /files`.
    pub file_upload_status: u16,
    /// HTTP status of `DELETE /files/{id}` and `DELETE /vector_stores/{id}` (200 = normal).
    pub delete_status: u16,
    /// Text of non-streaming responses (thread summary).
    pub summary_text: String,
    /// HTTP status of non-streaming responses.
    pub summary_status: u16,
    /// Delay between `[[slow]]` deltas.
    pub slow_delay: Duration,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            index_status: "completed".into(),
            file_upload_status: 200,
            delete_status: 200,
            summary_text: "<analysis>thinking</analysis>\n<summary>\nSummary of the conversation.\n</summary>".into(),
            summary_status: 200,
            slow_delay: Duration::from_millis(300),
        }
    }
}

#[derive(Default)]
struct Inner {
    requests: Vec<Recorded>,
    files: Vec<String>,
    cfg: MockConfig,
}

/// Mock provider handle.
#[derive(Clone)]
pub struct MockProvider {
    inner: Arc<Mutex<Inner>>,
    pub addr: SocketAddr,
}

impl MockProvider {
    /// Starts the mock on an ephemeral port.
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let app = Router::new().fallback(handle).with_state(inner.clone());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!(error = %e, "mock provider server stopped");
            }
        });
        Self { inner, addr }
    }

    /// All recorded requests.
    pub fn requests(&self) -> Vec<Recorded> {
        self.inner.lock().unwrap().requests.clone()
    }

    /// Recorded chat (`/responses`) requests.
    pub fn responses_requests(&self) -> Vec<Recorded> {
        self.requests().into_iter().filter(|r| r.path.ends_with("/responses")).collect()
    }

    /// Clears recorded requests.
    pub fn reset(&self) {
        self.inner.lock().unwrap().requests.clear();
    }

    /// Updates the behavior.
    pub fn configure(&self, f: impl FnOnce(&mut MockConfig)) {
        f(&mut self.inner.lock().unwrap().cfg);
    }

    fn cfg(&self) -> MockConfig {
        self.inner.lock().unwrap().cfg.clone()
    }
}

fn json_resp(status: u16, v: &Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn route(path: &str) -> String {
    for prefix in ["/openai/v1", "/openai", "/v1"] {
        if let Some(rest) = path.strip_prefix(prefix)
            && rest.starts_with('/')
        {
            return rest.to_owned();
        }
    }
    path.to_owned()
}

fn last_user_text(body: &Value) -> String {
    let Some(items) = body.get("input").and_then(Value::as_array) else { return String::new() };
    for item in items.iter().rev() {
        if item.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        return match item.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
    }
    String::new()
}

async fn handle(State(inner): State<Arc<Mutex<Inner>>>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().map(str::to_owned);
    let headers: HashMap<String, String> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or_default().to_owned()))
        .collect();
    let is_json = header_is(req.headers(), "application/json");
    let raw: Bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap_or_default();
    let body = if is_json && !raw.is_empty() {
        serde_json::from_slice(&raw).unwrap_or(Value::Null)
    } else {
        json!({"_bytes": raw.len()})
    };
    let cfg = {
        let mut g = inner.lock().unwrap();
        g.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            query,
            headers,
            body: body.clone(),
        });
        g.cfg.clone()
    };
    let r = route(&path);
    let parts: Vec<&str> = r.trim_matches('/').split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("POST", ["files"]) => {
            if cfg.file_upload_status != 200 {
                return json_resp(cfg.file_upload_status, &json!({"error": {"message": "upload failed"}}));
            }
            let id = format!("file-{}", &Uuid::new_v4().simple().to_string()[..24]);
            inner.lock().unwrap().files.push(id.clone());
            json_resp(200, &json!({"id": id, "object": "file", "bytes": raw.len(), "purpose": "assistants"}))
        }
        ("POST", ["vector_stores"]) => {
            let id = format!("vs_{}", &Uuid::new_v4().simple().to_string()[..24]);
            json_resp(200, &json!({"id": id, "object": "vector_store"}))
        }
        ("POST", ["vector_stores", _, "files"]) => json_resp(
            200,
            &json!({"id": body.get("file_id"), "object": "vector_store.file", "status": cfg.index_status}),
        ),
        ("GET", ["vector_stores", _, "files", fid]) => {
            json_resp(200, &json!({"id": fid, "object": "vector_store.file", "status": cfg.index_status}))
        }
        ("DELETE", ["files" | "vector_stores", _]) => {
            if cfg.delete_status != 200 {
                return json_resp(cfg.delete_status, &json!({"error": {"message": "delete failed"}}));
            }
            json_resp(200, &json!({"deleted": true}))
        }
        ("POST", [.., "responses"]) => responses(&inner, &body, &cfg),
        _ => json_resp(404, &json!({"error": {"message": "not found"}})),
    }
}

fn header_is(h: &HeaderMap, ct: &str) -> bool {
    h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with(ct))
}

fn frame(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

fn responses(inner: &Arc<Mutex<Inner>>, body: &Value, cfg: &MockConfig) -> Response {
    let text = last_user_text(body);
    if text.contains("[[429]]") {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::CONTENT_TYPE, "application/json"), (header::RETRY_AFTER, "7")],
            json!({"error": {"message": "Rate limit"}}).to_string(),
        )
            .into_response();
    }
    if text.contains("[[500]]") {
        return json_resp(500, &json!({"error": {"message": "boom file-abcdef0123456789abcd"}}));
    }
    if !body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        if cfg.summary_status != 200 {
            return json_resp(cfg.summary_status, &json!({"error": {"message": "summary failed"}}));
        }
        if text.contains("[[ctx_error]]") {
            return json_resp(400, &json!({"error": {"code": "context_length_exceeded", "message": "maximum context length"}}));
        }
        return json_resp(
            200,
            &json!({
                "id": format!("resp_{}", Uuid::new_v4().simple()),
                "object": "response",
                "status": "completed",
                "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": cfg.summary_text, "annotations": []}]}],
                "usage": {"input_tokens": 50, "output_tokens": 20, "output_tokens_details": {"reasoning_tokens": 0}},
            }),
        );
    }
    let resp_id = format!("resp_{}", Uuid::new_v4().simple());
    let first_file = inner.lock().unwrap().files.first().cloned();
    let mut frames: Vec<(Bytes, Duration)> = Vec::new();
    let mut push = |ev: &str, data: Value, d: Duration| frames.push((frame(ev, &data), d));
    let zero = Duration::ZERO;
    push("response.created", json!({"type": "response.created", "response": {"id": resp_id}}), zero);
    if text.contains("[[error]]") {
        push(
            "response.failed",
            json!({"type": "response.failed", "response": {"id": resp_id, "error": {
                "code": "server_error", "message": "Upstream failed for file-abcdef0123456789abcd"}}}),
            zero,
        );
        return sse(frames, false);
    }
    if text.contains("[[hang]]") {
        push("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": "partial"}), zero);
        return sse(frames, true);
    }
    let mut annotations = Vec::new();
    if text.contains("[[web_search3]]") {
        for i in 0..3 {
            push("response.web_search_call.searching", json!({"type": "response.web_search_call.searching", "item_id": format!("ws_{i}")}), zero);
            push("response.web_search_call.completed", json!({"type": "response.web_search_call.completed", "item_id": format!("ws_{i}")}), zero);
        }
    }
    if text.contains("[[web_search]]") {
        push("response.web_search_call.searching", json!({"type": "response.web_search_call.searching", "item_id": "ws_1"}), zero);
        push("response.web_search_call.completed", json!({"type": "response.web_search_call.completed", "item_id": "ws_1"}), zero);
        annotations.push(json!({"type": "url_citation", "url": "https://example.com/a", "title": "Example", "start_index": 0, "end_index": 5}));
    }
    if text.contains("[[file_search]]") {
        push("response.file_search_call.searching", json!({"type": "response.file_search_call.searching", "item_id": "fs_1"}), zero);
        push("response.file_search_call.completed", json!({"type": "response.file_search_call.completed", "item_id": "fs_1"}), zero);
        let fid = first_file.unwrap_or_else(|| "file-unknown000000000000".into());
        annotations.push(json!({"type": "file_citation", "file_id": fid, "filename": "x", "index": 0}));
    }
    if text.contains("[[code]]") {
        push("response.code_interpreter_call.in_progress", json!({"type": "response.code_interpreter_call.in_progress", "item_id": "ci_1"}), zero);
        push("response.output_item.done", json!({"type": "response.output_item.done", "item": {"type": "code_interpreter_call", "id": "ci_1",
            "outputs": [{"type": "logs", "logs": "42"}]}}), zero);
    }
    let mut answer = String::new();
    if text.contains("[[slow]]") {
        for i in 0..10 {
            let d = format!("tok{i} ");
            answer.push_str(&d);
            push("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": d}), cfg.slow_delay);
        }
    } else if !text.contains("[[empty]]") {
        for piece in ["Hello", " from", " mock."] {
            answer.push_str(piece);
            push("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": piece}), zero);
        }
    }
    let mut fin = json!({
        "id": resp_id,
        "status": "completed",
        "output": [{"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": answer, "annotations": annotations}]}],
        "usage": {"input_tokens": 120, "output_tokens": 30,
            "input_tokens_details": {"cached_tokens": 10},
            "output_tokens_details": {"reasoning_tokens": 0}},
    });
    if text.contains("[[incomplete]]") {
        fin["status"] = json!("incomplete");
        fin["incomplete_details"] = json!({"reason": "max_output_tokens"});
        push("response.incomplete", json!({"type": "response.incomplete", "response": fin}), zero);
    } else {
        push("response.completed", json!({"type": "response.completed", "response": fin}), zero);
    }
    sse(frames, false)
}

fn sse(frames: Vec<(Bytes, Duration)>, hang: bool) -> Response {
    let stream = async_stream::stream! {
        for (bytes, delay) in frames {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            yield Ok::<_, Infallible>(bytes);
        }
        if hang {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    };
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}
