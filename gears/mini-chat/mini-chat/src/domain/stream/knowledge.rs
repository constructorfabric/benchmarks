//! Knowledge search in the send pipeline (DESIGN "Knowledge Search"): the `search_knowledge`
//! function tool, its parameters per request, and the answer to one call of the agentic loop
//! (the loop itself runs in [`super::provider_task`]).

use std::sync::Arc;
use std::time::Instant;

use opentelemetry::KeyValue;
use serde_json::{Value, json};
use uuid::Uuid;

use super::StreamService;
use crate::config::KnowledgeSearchConfig;
use crate::infra::llm::ToolSpec;
use crate::infra::storage::knowledge::{KnowledgeChunk, KnowledgeRetriever, KnowledgeTarget};
use crate::metrics::Metrics;

/// Name of the knowledge search function tool.
pub const SEARCH_KNOWLEDGE: &str = "search_knowledge";

const LIMIT_REACHED_OUTPUT: &str =
    "search limit reached: answer with the information already retrieved";
const SEARCH_FAILED_OUTPUT: &str = "knowledge search failed";
const INVALID_ARGUMENTS_OUTPUT: &str = "invalid arguments: a non-empty `query` string is required";

/// Knowledge search of one turn (built only when every enablement condition holds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeParams {
    pub target: KnowledgeTarget,
    pub vector_store_id: String,
    /// `knowledge_search.max_calls_per_message`.
    pub max_calls: u32,
    /// `knowledge_search.top_k` (cap of the model's `top_k`).
    pub top_k: usize,
}

impl KnowledgeParams {
    /// Provider requests one message may make: `max_calls_per_message + 2`.
    #[must_use]
    pub fn max_iterations(&self) -> u32 {
        self.max_calls.saturating_add(2)
    }
}

/// The knowledge search parameters of a request of `tenant_id`, or `None` (knowledge search
/// off): the feature is disabled, no retriever exists, or the configured provider cannot serve
/// the tenant (logged).
pub(super) fn params(svc: &StreamService, tenant_id: Uuid) -> Option<KnowledgeParams> {
    let cfg = &svc.cfg.knowledge_search;
    if !cfg.enabled {
        return None;
    }
    let configured = || -> Result<KnowledgeParams, String> {
        if svc.knowledge.is_none() {
            return Err("no knowledge retriever".to_owned());
        }
        let (Some(vector_store_id), Some(provider_id)) = (&cfg.vector_store_id, &cfg.provider_id)
        else {
            return Err("vector_store_id / provider_id missing".to_owned());
        };
        let target = svc.providers.knowledge_target(provider_id, tenant_id)?;
        Ok(KnowledgeParams {
            target,
            vector_store_id: vector_store_id.clone(),
            max_calls: cfg.max_calls_per_message,
            top_k: cfg.top_k,
        })
    };
    configured()
        .inspect_err(|reason| {
            tracing::warn!(%tenant_id, %reason, "knowledge search is off for this request");
        })
        .ok()
}

/// The `search_knowledge` function tool.
pub(super) fn tool(cfg: &KnowledgeSearchConfig) -> ToolSpec {
    ToolSpec::Function {
        name: SEARCH_KNOWLEDGE.to_owned(),
        description:
            "Search the organization knowledge base and return the most relevant passages."
                .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to search for."},
                "top_k": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": cfg.top_k,
                    "description": "How many passages to return.",
                },
            },
            "required": ["query"],
        }),
    }
}

/// What one `search_knowledge` call does.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CallPlan {
    /// Run a retrieval.
    Search {
        query: String,
        max_num_results: usize,
    },
    /// The per-message retrieval limit is reached.
    LimitReached,
    /// The arguments carry no usable query.
    Invalid,
}

/// Per-turn retrieval budget of the agentic loop.
pub(super) struct AgenticLoop {
    pub params: KnowledgeParams,
    /// Provider requests made so far (the first one included).
    pub requests: u32,
    retrievals: u32,
}

impl AgenticLoop {
    pub(super) fn new(params: KnowledgeParams) -> Self {
        Self {
            params,
            requests: 1,
            retrievals: 0,
        }
    }

    /// Whether another provider request stays within the iteration cap.
    pub(super) fn may_continue(&self) -> bool {
        self.requests < self.params.max_iterations()
    }

    /// Plans one call with JSON `arguments`; a planned search uses one retrieval. Only planned
    /// searches count as `search_knowledge` calls (`file_search_calls`): limit-reached and
    /// invalid-argument calls run no retrieval and count nowhere.
    pub(super) fn plan(&mut self, arguments: &str) -> CallPlan {
        let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let Some(query) = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
        else {
            return CallPlan::Invalid;
        };
        if self.retrievals >= self.params.max_calls {
            return CallPlan::LimitReached;
        }
        self.retrievals += 1;
        let requested = args
            .get("top_k")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n > 0)
            .unwrap_or(self.params.top_k);
        CallPlan::Search {
            query: query.to_owned(),
            max_num_results: requested.min(self.params.top_k),
        }
    }
}

/// The output of a call that runs no retrieval. A planned search that cannot run (no
/// retriever, which `params` rules out) answers like a failed search.
pub(super) fn static_output(plan: &CallPlan) -> String {
    let message = match plan {
        CallPlan::LimitReached => LIMIT_REACHED_OUTPUT,
        CallPlan::Invalid => INVALID_ARGUMENTS_OUTPUT,
        CallPlan::Search { .. } => SEARCH_FAILED_OUTPUT,
    };
    json!({ "error": message }).to_string()
}

/// Runs one retrieval and returns the function output and whether it succeeded; records the
/// knowledge search metrics.
pub(super) async fn search(
    retriever: &Arc<dyn KnowledgeRetriever>,
    params: &KnowledgeParams,
    metrics: &Metrics,
    query: &str,
    max_num_results: usize,
) -> (String, bool) {
    let started = Instant::now();
    let result = retriever
        .search(
            &params.target,
            &params.vector_store_id,
            query,
            max_num_results,
        )
        .await;
    #[allow(clippy::cast_precision_loss)] // histogram sample in milliseconds
    metrics
        .knowledge_search_latency_ms
        .record(started.elapsed().as_millis() as f64, &[]);
    match result {
        Ok(chunks) => {
            metrics
                .knowledge_search
                .add(1, &[KeyValue::new("result", "ok")]);
            #[allow(clippy::cast_precision_loss)] // histogram sample
            metrics
                .knowledge_search_chunks
                .record(chunks.len() as f64, &[]);
            (results_output(&chunks), true)
        }
        Err(err) => {
            tracing::warn!(error = %err, "knowledge search failed");
            metrics
                .knowledge_search
                .add(1, &[KeyValue::new("result", "error")]);
            (json!({ "error": SEARCH_FAILED_OUTPUT }).to_string(), false)
        }
    }
}

/// `{"results": [{"source", "text"}]}` (no provider file ids).
fn results_output(chunks: &[KnowledgeChunk]) -> String {
    let results: Vec<Value> = chunks
        .iter()
        .map(|c| json!({"source": c.source, "text": c.text}))
        .collect();
    json!({ "results": results }).to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::Method;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::{CallPlan, static_output};
    use crate::test_support::metrics::MetricsProbe;

    use crate::config::{DEFAULT_KNOWLEDGE_SEARCH_GUARD, MiniChatConfig};
    use crate::test_support::app::{
        AZURE_ALIAS, AZURE_API_VERSION, TestApp, anthropic_provider, azure_provider, ctx,
        test_config,
    };
    use crate::test_support::catalog::{anthropic_model, test_catalog};
    use crate::test_support::gateway::{Responder, SseScript};
    use crate::test_support::stream::{
        ANTHROPIC_MESSAGES_PATH, SeedAttachment, USAGE_QUEUE, anthropic_answer, anthropic_event,
        completed, create_chat, event_names, function_call_with, provider_calls, script_provider,
        script_provider_sequence, seed_attachment, seed_vector_store, stream_uri, text_delta,
        turns_of,
    };

    const SEARCH_PATH: &str = "/openai/vector_stores/vs_kb/search";

    fn knowledge_config(edit: impl FnOnce(&mut MiniChatConfig)) -> MiniChatConfig {
        let mut cfg = test_config();
        cfg.providers.insert("azure".to_owned(), azure_provider());
        let k = &mut cfg.knowledge_search;
        k.enabled = true;
        k.vector_store_id = Some("vs_kb".to_owned());
        k.provider_id = Some("azure".to_owned());
        edit(&mut cfg);
        cfg
    }

    fn search_call(call_id: &str, query: &str) -> SseScript {
        function_call_with(call_id, "search_knowledge", &json!({"query": query}))
    }

    fn script_search(app: &TestApp, chunks: &[(&str, &str)]) {
        let data: Vec<Value> = chunks
            .iter()
            .map(|(file, text)| {
                json!({"file_id": "file-abcdefghijklmnop", "filename": file, "score": 0.5,
                       "content": [{"type": "text", "text": text}]})
            })
            .collect();
        app.gateway.on(
            Method::POST,
            SEARCH_PATH,
            Responder::json(
                200,
                json!({"object": "vector_store.search_results.page", "data": data}),
            ),
        );
    }

    fn searches(app: &TestApp) -> Vec<crate::test_support::gateway::RecordedRequest> {
        app.gateway.requests_to(&Method::POST, SEARCH_PATH)
    }

    /// The `function_call_output` items of a provider request body.
    fn outputs(body: &Value) -> Vec<String> {
        body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["type"] == "function_call_output")
            .map(|i| i["output"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn has_search_tool(body: &Value) -> bool {
        body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["type"] == "function" && t["name"] == "search_knowledge")
    }

    #[test]
    fn knowledge_search_loop_static_outputs() {
        let search = CallPlan::Search {
            query: "q".to_owned(),
            max_num_results: 1,
        };
        // a planned search that cannot run (no retriever) is a failed search
        assert!(static_output(&search).contains("knowledge search failed"));
        assert!(static_output(&CallPlan::LimitReached).contains("search limit reached"));
        assert!(static_output(&CallPlan::Invalid).contains("invalid arguments"));
    }

    #[tokio::test]
    async fn knowledge_search_loop() {
        let probe = MetricsProbe::new();
        let app = TestApp::builder()
            .config(knowledge_config(|c| {
                c.knowledge_search.max_chunk_chars = 12;
            }))
            .metrics(Arc::clone(&probe.metrics))
            .build()
            .await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user);
        let chat = create_chat(&app, &who, None).await;
        script_search(&app, &[("handbook.pdf", "Vacation is 25 days per year.")]);
        script_provider_sequence(
            &app,
            vec![
                Responder::Sse(vec![
                    text_delta("Let me check. "),
                    search_call("call_1", "vacation"),
                    completed(5, 5),
                ]),
                Responder::Sse(vec![text_delta("25 days."), completed(20, 7)]),
            ],
        );

        let frames = app
            .stream(
                "POST",
                &stream_uri(chat),
                &who,
                json!({"content": "How much vacation?"}),
            )
            .await
            .expect("stream");

        assert_eq!(
            event_names(&frames),
            ["stream_started", "delta", "delta", "done"]
        );
        assert_eq!(frames[2].data["content"], "25 days.");
        // only the final iteration's usage is reported and settled
        assert_eq!(
            frames[3].data["usage"],
            json!({"input_tokens": 20, "output_tokens": 7})
        );

        let calls = provider_calls(&app);
        assert_eq!(calls.len(), 2);
        assert!(has_search_tool(&calls[0]), "{}", calls[0]["tools"]);
        assert!(
            calls[0]["instructions"]
                .as_str()
                .unwrap()
                .contains(DEFAULT_KNOWLEDGE_SEARCH_GUARD)
        );
        let search = searches(&app);
        assert_eq!(search.len(), 1);
        assert_eq!(
            search[0].uri,
            format!("/{AZURE_ALIAS}{SEARCH_PATH}?api-version={AZURE_API_VERSION}")
        );
        assert_eq!(
            search[0].json,
            Some(json!({"query": "vacation", "max_num_results": 5}))
        );
        // the call and its output were appended to the input of the next request
        let input = calls[1]["input"].as_array().unwrap();
        let call = &input[input.len() - 2];
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["call_id"], "call_1");
        assert_eq!(call["name"], "search_knowledge");
        let out = outputs(&calls[1]);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("Vacation is "), "{}", out[0]);
        assert!(
            !out[0].contains("25 days"),
            "trimmed to 12 chars: {}",
            out[0]
        );
        assert!(!out[0].contains("file-abcdefghijklmnop"), "{}", out[0]);

        let turn = turns_of(&app, chat).await.remove(0);
        assert_eq!(turn.state, "completed");
        assert_eq!(turn.file_search_completed_count, 1);
        TestApp::wait_until("usage event", || async {
            !app.outbox_payloads(USAGE_QUEUE).is_empty()
        })
        .await;
        let usage = &app.outbox_payloads(USAGE_QUEUE)[0];
        assert_eq!(usage["file_search_calls"], 1, "{usage}");
        assert_eq!(usage["usage"]["input_tokens"], 20, "{usage}");
        assert_eq!(probe.counter("knowledge_search", &[("result", "ok")]), 1);
        assert_eq!(probe.counter("knowledge_search", &[("result", "error")]), 0);
    }

    #[tokio::test]
    async fn knowledge_search_loop_over_anthropic() {
        let mut cfg = knowledge_config(|_| {});
        cfg.providers
            .insert("anthropic".to_owned(), anthropic_provider());
        let mut catalog = test_catalog();
        catalog.push(anthropic_model("claude"));
        let app = TestApp::builder()
            .config(cfg)
            .catalog(catalog)
            .build()
            .await;
        let who = ctx(Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &who, Some("claude")).await;
        script_search(&app, &[("a.txt", "alpha")]);
        let ev = anthropic_event;
        let tool_use = Responder::Sse(vec![
            ev(
                "message_start",
                json!({"message": {"id": "msg_1", "usage": {"input_tokens": 3, "output_tokens": 1}}}),
            ),
            ev(
                "content_block_start",
                json!({"index": 0, "content_block":
                {"type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {}}}),
            ),
            ev(
                "content_block_delta",
                json!({"index": 0, "delta":
                {"type": "input_json_delta", "partial_json": "{\"query\": \"alpha\"}"}}),
            ),
            ev("content_block_stop", json!({"index": 0})),
            ev(
                "message_delta",
                json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 4}}),
            ),
            ev("message_stop", json!({})),
        ]);
        app.gateway.on_sequence(
            Method::POST,
            ANTHROPIC_MESSAGES_PATH,
            vec![tool_use, anthropic_answer(&["Alpha it is."], 6)],
        );

        let frames = app
            .stream("POST", &stream_uri(chat), &who, json!({"content": "q"}))
            .await
            .expect("stream");

        assert_eq!(
            event_names(&frames),
            ["stream_started", "tool", "delta", "done"]
        );
        assert_eq!(
            frames[1].data,
            json!({"phase": "start", "name": "search_knowledge", "details": {}})
        );
        let calls = app
            .gateway
            .requests_to(&Method::POST, ANTHROPIC_MESSAGES_PATH);
        assert_eq!(calls.len(), 2);
        let first = calls[0].json.clone().unwrap();
        assert!(
            first["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "search_knowledge" && t.get("input_schema").is_some()),
            "{}",
            first["tools"]
        );
        let messages = calls[1].json.as_ref().unwrap()["messages"].clone();
        let n = messages.as_array().unwrap().len();
        assert_eq!(
            messages[n - 2]["content"][0],
            json!({"type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {"query": "alpha"}})
        );
        let result = &messages[n - 1]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "toolu_1");
        assert!(
            result["content"].as_str().unwrap().contains("alpha"),
            "{result}"
        );
        assert_eq!(turns_of(&app, chat).await[0].state, "completed");
    }

    #[tokio::test]
    async fn knowledge_search_loop_iteration_cap() {
        // one retrieval per message → at most 3 provider requests
        let app = TestApp::builder()
            .config(knowledge_config(|c| {
                c.knowledge_search.max_calls_per_message = 1;
            }))
            .build()
            .await;
        let who = ctx(Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &who, None).await;
        script_search(&app, &[("a.txt", "alpha")]);
        script_provider(
            &app,
            Responder::Sse(vec![search_call("call_1", "again"), completed(5, 5)]),
        );

        let frames = app
            .stream("POST", &stream_uri(chat), &who, json!({"content": "q"}))
            .await
            .expect("stream");

        assert_eq!(event_names(&frames), ["stream_started", "error"]);
        assert_eq!(frames[1].data["code"], "agentic_iterations_exceeded");
        let calls = provider_calls(&app);
        assert_eq!(calls.len(), 3);
        assert_eq!(searches(&app).len(), 1, "one retrieval, then the limit");
        let last = outputs(&calls[2]);
        assert_eq!(last.len(), 2);
        assert!(last[0].contains("alpha"), "{}", last[0]);
        assert!(last[1].contains("search limit reached"), "{}", last[1]);
        let turn = turns_of(&app, chat).await.remove(0);
        assert_eq!(turn.state, "failed");
        assert_eq!(
            turn.error_code.as_deref(),
            Some("agentic_iterations_exceeded")
        );
        // settled on the usage the last response reported (DESIGN 5.7: `actual` when known)
        TestApp::wait_until("usage event", || async {
            !app.outbox_payloads(USAGE_QUEUE).is_empty()
        })
        .await;
        let usage = &app.outbox_payloads(USAGE_QUEUE)[0];
        assert_eq!(usage["settlement_method"], "actual", "{usage}");
        assert_eq!(usage["usage"]["input_tokens"], 5, "{usage}");
        assert_eq!(usage["file_search_calls"], 1, "{usage}");
    }

    #[tokio::test]
    async fn knowledge_search_loop_other_function_is_unexpected() {
        let app = TestApp::builder()
            .config(knowledge_config(|_| {}))
            .build()
            .await;
        let who = ctx(Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &who, None).await;
        script_provider(
            &app,
            Responder::Sse(vec![
                function_call_with("call_1", "load_files", &json!({})),
                completed(5, 5),
            ]),
        );
        let frames = app
            .stream("POST", &stream_uri(chat), &who, json!({"content": "q"}))
            .await
            .expect("stream");
        assert_eq!(event_names(&frames), ["stream_started", "error"]);
        assert_eq!(frames[1].data["code"], "unexpected_tool_use");
        assert!(searches(&app).is_empty());
    }

    #[tokio::test]
    async fn knowledge_search_loop_is_off_with_file_search_or_a_misconfigured_provider() {
        // the chat has ready documents: file_search wins
        let app = TestApp::builder()
            .config(knowledge_config(|_| {}))
            .build()
            .await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user);
        let chat = create_chat(&app, &who, None).await;
        seed_attachment(&app, SeedAttachment::document(tenant, chat, user)).await;
        seed_vector_store(&app, tenant, chat).await;
        script_provider(
            &app,
            Responder::Sse(vec![text_delta("ok"), completed(1, 1)]),
        );
        let frames = app
            .stream("POST", &stream_uri(chat), &who, json!({"content": "q"}))
            .await
            .expect("stream");
        assert_eq!(frames.last().unwrap().event, "done");
        let body = &provider_calls(&app)[0];
        assert!(
            body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["type"] == "file_search")
        );
        assert!(!has_search_tool(body), "{}", body["tools"]);
        assert!(
            !body["instructions"]
                .as_str()
                .unwrap()
                .contains(DEFAULT_KNOWLEDGE_SEARCH_GUARD)
        );

        // the knowledge provider has no api_version: off for the request
        let app = TestApp::builder()
            .config(knowledge_config(|c| {
                c.providers.get_mut("azure").unwrap().api_version = None;
            }))
            .build()
            .await;
        let who = ctx(Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &who, None).await;
        script_provider(
            &app,
            Responder::Sse(vec![text_delta("ok"), completed(1, 1)]),
        );
        let frames = app
            .stream("POST", &stream_uri(chat), &who, json!({"content": "q"}))
            .await
            .expect("stream");
        assert_eq!(frames.last().unwrap().event, "done");
        let body = &provider_calls(&app)[0];
        assert!(!has_search_tool(body), "{}", body);
    }
}
