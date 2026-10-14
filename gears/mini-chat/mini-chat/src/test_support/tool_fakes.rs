//! Fakes of the tool contributions and the knowledge loop of FEATURE 0005 slice S2: a counting
//! [`AttachmentFactsStore`], a scripted [`KnowledgeRetriever`], a recording [`TurnProgress`] and
//! the catalog and provider-registry builders of the contribution tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::send::{FILE_GUARD, std_model, tool_support};
use crate::config::{KnowledgeSearchConfig, MiniChatConfig};
use crate::domain::attachments::contributions::AttachmentToolSettings;
use crate::domain::attachments::facts::{AttachmentFactsStore, ReadyFile};
use crate::domain::attachments::knowledge::{KnowledgeChunk, KnowledgeRetriever, RetrievalFailure};
use crate::domain::error::DomainError;
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::streaming::{ToolKind, TurnProgress};

// ── the facts store ──────────────────────────────────────────────────────────

/// The reads a [`FakeFacts`] served, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactRead {
    DocumentCount,
    CodeInterpreterFiles,
    VectorStore,
    CitationFiles,
}

/// A configurable [`AttachmentFactsStore`] that logs every read.
pub struct FakeFacts {
    pub documents: u32,
    pub vector_store: Option<String>,
    pub ci_files: Vec<ReadyFile>,
    pub citations: Vec<ReadyFile>,
    pub fail: AtomicBool,
    pub reads: Mutex<Vec<FactRead>>,
}

impl FakeFacts {
    pub fn new() -> Self {
        Self {
            documents: 0,
            vector_store: None,
            ci_files: Vec::new(),
            citations: Vec::new(),
            fail: AtomicBool::new(false),
            reads: Mutex::new(Vec::new()),
        }
    }

    pub fn reads(&self) -> Vec<FactRead> {
        self.reads.lock().unwrap().clone()
    }

    fn log(&self, read: FactRead) -> Result<(), DomainError> {
        self.reads.lock().unwrap().push(read);
        if self.fail.load(Ordering::SeqCst) {
            return Err(DomainError::internal("injected fact read failure"));
        }
        Ok(())
    }
}

impl Default for FakeFacts {
    fn default() -> Self {
        Self::new()
    }
}

/// A ready file with a recognizable provider file id and filename.
pub fn ready_file(n: u128, provider_file_id: &str, filename: &str) -> ReadyFile {
    ReadyFile {
        attachment_id: Uuid::from_u128(0xF000 + n),
        provider_file_id: provider_file_id.to_owned(),
        filename: filename.to_owned(),
    }
}

#[async_trait]
impl AttachmentFactsStore for FakeFacts {
    async fn ready_document_count(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
    ) -> Result<u32, DomainError> {
        self.log(FactRead::DocumentCount)?;
        Ok(self.documents)
    }

    async fn ready_code_interpreter_files(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
    ) -> Result<Vec<ReadyFile>, DomainError> {
        self.log(FactRead::CodeInterpreterFiles)?;
        Ok(self.ci_files.clone())
    }

    async fn bound_vector_store_id(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        self.log(FactRead::VectorStore)?;
        Ok(self.vector_store.clone())
    }

    async fn citation_files(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
    ) -> Result<Vec<ReadyFile>, DomainError> {
        self.log(FactRead::CitationFiles)?;
        Ok(self.citations.clone())
    }
}

// ── catalog entries ──────────────────────────────────────────────────────────

/// The standard model with `max_num_results` and the given file search and code interpreter
/// support (web search stays on).
pub fn model_with_tools(
    max_num_results: u32,
    file_search: bool,
    code_interpreter: bool,
) -> ModelCatalogEntry {
    let mut entry = std_model(&json!({ "max_num_results": max_num_results }));
    let mut value = serde_json::to_value(&entry).expect("entry serializes");
    let mut tools = tool_support(true);
    tools["file_search"] = json!(file_search);
    tools["code_interpreter"] = json!(code_interpreter);
    value["general_config"]["tool_support"] = tools;
    entry = serde_json::from_value(value).expect("entry");
    entry
}

pub fn no_switches() -> KillSwitches {
    KillSwitches::default()
}

pub fn switches_off(file_search: bool, code_interpreter: bool) -> KillSwitches {
    KillSwitches {
        disable_file_search: file_search,
        disable_code_interpreter: code_interpreter,
        ..KillSwitches::default()
    }
}

// ── provider registries for the knowledge contribution ───────────────────────

/// The `kb` provider id of the knowledge tests.
pub const KB_PROVIDER: &str = "kb";
pub const KB_ALIAS: &str = "kb.example.com";
pub const KB_API_VERSION: &str = "2025-03-01-preview";
pub const KB_VECTOR_STORE: &str = "vs_kb_1";

/// A registry with the provider `kb` of kind `kind` (`openai_responses`, `anthropic_messages`,
/// `openai_chat_completions` or `vllm_responses`) and, when `api_version` is given, its
/// `api_version`.
pub fn kb_registry(kind: &str, api_version: Option<&str>) -> Arc<ProviderRegistry> {
    let azure = api_version.is_some_and(|version| !version.is_empty());
    let mut provider = json!({
        "kind": kind, "host": KB_ALIAS,
        "storage_kind": if azure { "azure" } else { "openai" }
    });
    if let Some(version) = api_version.filter(|version| !version.is_empty()) {
        provider["api_version"] = json!(version);
    }
    if kind == "anthropic_messages" {
        provider["api_path"] = json!("/v1/messages");
    }
    if kind == "openai_chat_completions" {
        provider["api_path"] = json!("/v1/chat/completions");
    }
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": { KB_PROVIDER: provider }
    }))
    .expect("valid config");
    Arc::new(ProviderRegistry::build(&config.providers))
}

/// The enabled `knowledge_search` configuration over the provider `kb`.
pub fn knowledge_config() -> KnowledgeSearchConfig {
    KnowledgeSearchConfig {
        enabled: true,
        vector_store_id: Some(KB_VECTOR_STORE.to_owned()),
        provider_id: Some(KB_PROVIDER.to_owned()),
        guard: "KB-GUARD: use search_knowledge sparingly.".to_owned(),
        ..KnowledgeSearchConfig::default()
    }
}

/// The settings of the attachment tool contributions of a send rig: the rig's file search guard,
/// the given knowledge configuration over the `kb` provider and a configured retriever.
pub fn tool_settings(knowledge: KnowledgeSearchConfig) -> AttachmentToolSettings {
    AttachmentToolSettings {
        file_search_guard: FILE_GUARD.to_owned(),
        knowledge,
        providers: kb_registry("openai_responses", Some(KB_API_VERSION)),
        retriever_configured: true,
    }
}

// ── the retriever ────────────────────────────────────────────────────────────

/// One search the retriever received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCall {
    pub tenant_id: Uuid,
    pub query: String,
    pub top_k: usize,
}

/// A scripted [`KnowledgeRetriever`]: queued answers, a default of two chunks, and a hook that
/// cancels a token while a search runs.
pub struct ScriptedRetriever {
    answers: Mutex<VecDeque<Result<Vec<KnowledgeChunk>, RetrievalFailure>>>,
    default: Mutex<Result<Vec<KnowledgeChunk>, RetrievalFailure>>,
    calls: Mutex<Vec<SearchCall>>,
    cancel_during_search: Mutex<Option<CancellationToken>>,
    /// The number of next searches that never resolve.
    hang: AtomicUsize,
}

impl ScriptedRetriever {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(VecDeque::new()),
            default: Mutex::new(Ok(vec![chunk("alpha"), chunk("beta")])),
            calls: Mutex::new(Vec::new()),
            cancel_during_search: Mutex::new(None),
            hang: AtomicUsize::new(0),
        })
    }

    /// The next `searches` searches never resolve (a retriever that hangs).
    pub fn hang_next(&self, searches: usize) {
        self.hang.store(searches, Ordering::SeqCst);
    }

    pub fn queue(&self, answer: Result<Vec<KnowledgeChunk>, RetrievalFailure>) {
        self.answers.lock().unwrap().push_back(answer);
    }

    /// Cancel `token` while the next searches run.
    pub fn cancel_during_search(&self, token: &CancellationToken) {
        *self.cancel_during_search.lock().unwrap() = Some(token.clone());
    }

    pub fn calls(&self) -> Vec<SearchCall> {
        self.calls.lock().unwrap().clone()
    }
}

pub fn chunk(text: &str) -> KnowledgeChunk {
    KnowledgeChunk {
        text: text.to_owned(),
    }
}

#[async_trait]
impl KnowledgeRetriever for ScriptedRetriever {
    async fn search(
        &self,
        tenant_id: Uuid,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<KnowledgeChunk>, RetrievalFailure> {
        self.calls.lock().unwrap().push(SearchCall {
            tenant_id,
            query: query.to_owned(),
            top_k,
        });
        if let Some(token) = self.cancel_during_search.lock().unwrap().as_ref() {
            token.cancel();
        }
        if self
            .hang
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            std::future::pending::<()>().await;
        }
        let queued = self.answers.lock().unwrap().pop_front();
        queued.unwrap_or_else(|| self.default.lock().unwrap().clone())
    }
}

// ── the progress port ────────────────────────────────────────────────────────

/// A [`TurnProgress`] that records the counter increments and can fail them.
#[derive(Default)]
pub struct RecordingProgress {
    done: Mutex<Vec<(Uuid, Uuid, ToolKind)>>,
    pub fail: AtomicBool,
}

impl RecordingProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The `(turn_id, tool)` of every counter increment.
    pub fn increments(&self) -> Vec<(Uuid, ToolKind)> {
        self.done
            .lock()
            .unwrap()
            .iter()
            .map(|(_, turn, tool)| (*turn, *tool))
            .collect()
    }
}

#[async_trait]
impl TurnProgress for RecordingProgress {
    async fn record_tool_done(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        tool: ToolKind,
    ) -> Result<(), DomainError> {
        self.done.lock().unwrap().push((tenant_id, turn_id, tool));
        if self.fail.load(Ordering::SeqCst) {
            return Err(DomainError::internal("injected progress failure"));
        }
        Ok(())
    }

    async fn touch_progress(
        &self,
        _tenant_id: Uuid,
        _turn_id: Uuid,
        _at: OffsetDateTime,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}
