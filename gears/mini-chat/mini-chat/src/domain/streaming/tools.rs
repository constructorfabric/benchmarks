//! Tool assembly (`cpt-cf-mini-chat-algo-message-streaming-tool-assembly`): the adapter-agnostic
//! tool set of a turn, gated by the catalog `tool_support`, the kill switches and the request
//! flag, plus the contributions registered by other features.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, WebSearchContextSize};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::contributions::ToolResultFormat;
use crate::domain::streaming::citations::FileCitationMap;

/// A tool of the set, in the order of the tool set: `file_search`, `web_search`,
/// `code_interpreter`, `search_knowledge`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolKind {
    FileSearch,
    WebSearch,
    CodeInterpreter,
    SearchKnowledge,
}

impl ToolKind {
    /// The tool name: `file_search`, `web_search`, `code_interpreter` or `search_knowledge`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileSearch => "file_search",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
            Self::SearchKnowledge => "search_knowledge",
        }
    }
}

/// An adapter-agnostic tool definition.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    pub kind: ToolKind,
    /// The catalog `web_search_context_size`, for `web_search` only.
    pub search_context_size: Option<WebSearchContextSize>,
}

/// What one tool contributes to the set.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPart {
    pub definition: ToolDefinition,
    /// The guard text appended to the system instructions.
    pub guard: Option<String>,
    /// An `include` entry of the provider request.
    pub include: Option<String>,
}

/// The facts of the turn a contribution decides on.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolTurnFacts {
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub web_search_enabled: bool,
}

/// A tool contributed by another feature (`file_search`, `code_interpreter`, `search_knowledge`
/// of `cpt-cf-mini-chat-feature-attachments-and-file-search`) under its own inclusion rules.
pub trait ToolContribution: Send + Sync {
    /// The tool this contribution may add.
    fn kind(&self) -> ToolKind;

    /// The part to add when the tool applies to this turn.
    fn contribute(&self, facts: &ToolTurnFacts) -> Option<ToolPart>;
}

/// The tool set of a turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolSet {
    /// In the order `file_search`, `web_search`, `code_interpreter`, `search_knowledge`.
    pub tools: Vec<ToolDefinition>,
    /// The guards of the set in the order of the context plan: `file_search`, `web_search`,
    /// `search_knowledge`.
    pub guards: Vec<String>,
    /// The `include` entries of the set.
    pub include: Vec<String>,
    /// `none` without a built-in tool, else the built-in tool names joined by `+` in the order
    /// `file_search`, `web_search`, `code_interpreter` (for example `file_search+web_search`).
    pub feature: String,
    /// The `file_search` tool of the attachments feature: the chat's vector store and the result
    /// bound (`None` when no contribution supplied them).
    pub file_search: Option<FileSearchSpec>,
    /// The ready code interpreter files of the `code_interpreter` container.
    pub code_interpreter_files: Vec<String>,
    /// The citation file map built by the file search contribution.
    pub citation_files: FileCitationMap,
    /// The tool-result format of `search_knowledge` for the provider kind of the turn.
    pub knowledge_format: Option<ToolResultFormat>,
}

/// The `file_search` definition of the attachments feature.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchSpec {
    pub vector_store_ids: Vec<String>,
    pub max_num_results: u32,
}

/// Assemble the tool set: `web_search` (with the catalog `web_search_context_size` and
/// `web_search_guard`) only when requested, supported by `tool_support.web_search` and not
/// disabled by the kill switch; the parts of the registered contributions that apply
/// (`file_search` and `search_knowledge` are never both added); ordered, with the `feature`
/// metadata value. The adapter kind is unknown here.
#[must_use]
pub fn assemble_tools(
    entry: &ModelCatalogEntry,
    kill_switches: &KillSwitches,
    web_search_guard: &str,
    facts: &ToolTurnFacts,
    contributions: &[std::sync::Arc<dyn ToolContribution>],
) -> ToolSet {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-empty
    let mut parts: Vec<ToolPart> = Vec::new();
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-empty
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-assemble
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-web
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-web-add
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-web-unsupported
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-web-skip
    // the gate is the catalog `tool_support` (and the kill switch), never the adapter; a model
    // without web search support proceeds without the tool and without the guard
    if facts.web_search_enabled
        && entry.general_config.tool_support.web_search
        && !kill_switches.disable_web_search
    {
        parts.push(ToolPart {
            definition: ToolDefinition {
                kind: ToolKind::WebSearch,
                search_context_size: Some(entry.web_search_context_size),
            },
            guard: Some(web_search_guard.to_owned()),
            include: None,
        });
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-web-skip
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-web-unsupported
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-web-add
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-web
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-assemble
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-contributions
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-contribution-add
    // every contribution decides under its own inclusion rules; the first part of a kind wins
    // and `web_search` is owned by this algorithm
    for contribution in contributions {
        let Some(part) = contribution.contribute(facts) else {
            continue;
        };
        let kind = part.definition.kind;
        if kind != ToolKind::WebSearch && !parts.iter().any(|p| p.definition.kind == kind) {
            parts.push(part);
        }
    }
    // `file_search` and `search_knowledge` are never both in the set
    if parts
        .iter()
        .any(|part| part.definition.kind == ToolKind::FileSearch)
    {
        parts.retain(|part| part.definition.kind != ToolKind::SearchKnowledge);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-contribution-add
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-contributions
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-order
    // definitions in the order of `ToolKind` (`file_search`, `web_search`, `code_interpreter`,
    // `search_knowledge`), whatever the registration order
    parts.sort_by_key(|part| part.definition.kind);
    let tools: Vec<ToolDefinition> = parts.iter().map(|part| part.definition.clone()).collect();
    // the guards in the order of the context plan: `file_search`, `web_search`,
    // `search_knowledge` (a tool without a guard of its own adds none)
    let guard_rank = |kind: ToolKind| match kind {
        ToolKind::FileSearch => 0,
        ToolKind::WebSearch => 1,
        ToolKind::SearchKnowledge => 2,
        ToolKind::CodeInterpreter => 3,
    };
    let mut guarded: Vec<&ToolPart> = parts.iter().filter(|part| part.guard.is_some()).collect();
    guarded.sort_by_key(|part| guard_rank(part.definition.kind));
    let guards: Vec<String> = guarded
        .into_iter()
        .filter_map(|part| part.guard.clone())
        .collect();
    let include: Vec<String> = parts
        .iter()
        .filter_map(|part| part.include.clone())
        .collect();
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-order
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-feature
    // `none` without a built-in tool, else the built-in tool names joined by `+`
    // (`search_knowledge` is not a built-in tool)
    let built_in: Vec<&str> = tools
        .iter()
        .filter(|tool| tool.kind != ToolKind::SearchKnowledge)
        .map(|tool| tool.kind.as_str())
        .collect();
    let feature = if built_in.is_empty() {
        "none".to_owned()
    } else {
        built_in.join("+")
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-feature
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-adapter-agnostic
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-return
    // decided before the adapter runs, without knowing the adapter kind; the per-request tool
    // call bound is the catalog `max_tool_calls`, applied by the adapter
    ToolSet {
        tools,
        guards,
        include,
        feature,
        ..ToolSet::default()
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-adapter-agnostic
}
