//! Knowledge search: the `search_knowledge` function tool and its retrieval
//! (DESIGN §4 "Knowledge Search").

use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::{KnowledgeSearchConfig, ProviderKind};
use crate::infra::llm::{LlmGateway, ResolvedProvider};

/// Function tool name.
pub const TOOL_NAME: &str = "search_knowledge";

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-turn knowledge search parameters (built only when every prerequisite holds).
#[derive(Debug, Clone)]
pub struct KnowledgeParams {
    pub provider: ResolvedProvider,
    pub vector_store_id: String,
    pub top_k: usize,
    pub max_chunk_chars: usize,
    pub max_calls: u32,
}

impl KnowledgeParams {
    /// Builds the parameters, or `None` (with a warning) when a prerequisite is missing.
    #[must_use]
    // Linear prerequisite checks, each with its own warning.
    #[allow(clippy::cognitive_complexity)]
    pub fn build(cfg: &KnowledgeSearchConfig, llm: &LlmGateway, tenant_id: Uuid) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        let provider_id = cfg
            .provider_id
            .as_deref()
            .filter(|p| !p.trim().is_empty())?;
        let vector_store_id = cfg
            .vector_store_id
            .clone()
            .filter(|v| !v.trim().is_empty())?;
        let Some(entry) = llm.providers().get(provider_id) else {
            tracing::warn!(provider = %provider_id, "knowledge search provider is not configured; knowledge search is off");
            return None;
        };
        if !matches!(
            entry.kind,
            ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
        ) {
            tracing::warn!(provider = %provider_id, "knowledge search provider kind is not supported; knowledge search is off");
            return None;
        }
        if entry
            .api_version
            .as_deref()
            .is_none_or(|v| v.trim().is_empty())
        {
            tracing::warn!(provider = %provider_id, "knowledge search provider has no api_version; knowledge search is off");
            return None;
        }
        let provider = match llm.resolve(provider_id, tenant_id) {
            Ok(p) if !p.alias.is_empty() => p,
            Ok(_) | Err(_) => {
                tracing::warn!(provider = %provider_id, "knowledge search provider has no upstream alias; knowledge search is off");
                return None;
            }
        };
        Some(Self {
            provider,
            vector_store_id,
            top_k: cfg.top_k.max(1),
            max_chunk_chars: cfg.max_chunk_chars.max(1),
            max_calls: cfg.max_calls_per_message.max(1),
        })
    }
}

/// Function tool definition (Responses API format).
#[must_use]
pub fn tool_definition() -> Value {
    json!({
        "type": "function",
        "name": TOOL_NAME,
        "description": "Search the organization knowledge base and return the most relevant text passages.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query in natural language."},
                "top_k": {"type": "integer", "minimum": 1, "description": "Maximum number of passages to return."}
            },
            "required": ["query"],
            "additionalProperties": false
        }
    })
}

/// Arguments supplied by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchArgs {
    pub query: String,
    pub top_k: Option<usize>,
}

/// Parses the function-call arguments (`{"query": ..., "top_k": ...}`).
#[must_use]
pub fn parse_args(arguments: &str) -> Option<SearchArgs> {
    let v: Value = serde_json::from_str(arguments).ok()?;
    let query = v.get("query")?.as_str()?.trim().to_owned();
    if query.is_empty() {
        return None;
    }
    let top_k = v
        .get("top_k")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0);
    Some(SearchArgs { query, top_k })
}

/// One retrieved passage.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub filename: String,
    pub text: String,
    pub score: Option<f64>,
}

fn trim_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

/// Extracts the passages of a vector-store search result page.
#[must_use]
pub fn parse_results(v: &Value, max_chunk_chars: usize, limit: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    for item in v
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let filename = item
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let score = item.get("score").and_then(Value::as_f64);
        let text: Vec<&str> = item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect();
        out.push(Chunk {
            filename,
            text: trim_chars(&text.join("\n"), max_chunk_chars),
            score,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// `function_call_output` payload for retrieved passages.
#[must_use]
pub fn results_output(chunks: &[Chunk]) -> String {
    let items: Vec<Value> = chunks
        .iter()
        .map(|c| json!({"source": c.filename, "text": c.text}))
        .collect();
    json!({"results": items}).to_string()
}

/// `function_call_output` payload once the per-message search limit is reached.
#[must_use]
pub fn limit_output() -> String {
    json!({"error": "search limit reached for this message; answer with the information you already have"}).to_string()
}

/// `function_call_output` payload of a failed retrieval.
#[must_use]
pub fn failure_output() -> String {
    json!({"error": "knowledge search failed; answer without it"}).to_string()
}

/// `function_call_output` payload for unusable arguments.
#[must_use]
pub fn invalid_args_output() -> String {
    json!({"error": "invalid arguments: a non-empty 'query' string is required"}).to_string()
}

/// Runs one retrieval against the configured vector store.
///
/// # Errors
/// Provider failures (the caller reports them to the model).
pub async fn search(
    llm: &LlmGateway,
    p: &KnowledgeParams,
    args: &SearchArgs,
) -> Result<Vec<Chunk>, crate::infra::llm::ProviderCallError> {
    let k = args.top_k.unwrap_or(p.top_k).min(p.top_k);
    let uri = p
        .provider
        .rag_uri(&format!("/vector_stores/{}/search", p.vector_store_id));
    let body = json!({"query": args.query, "max_num_results": k});
    let v = llm
        .send_json(http::Method::POST, &uri, Some(&body), SEARCH_TIMEOUT)
        .await?;
    Ok(parse_results(&v, p.max_chunk_chars, k))
}

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod knowledge_tests;
