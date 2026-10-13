//! Domain layer: business rules independent of transport and storage.
//!
//! - [`error`] — the domain error taxonomy and its canonical REST mapping
//!   (ADR-0004);
//! - [`quota`] — credit arithmetic, UTC quota periods and token estimation;
//! - [`authz`] — the PEP: PDP evaluation and subject-confined access scopes
//!   (DESIGN section 3.8);
//! - [`models_service`] — Models API visibility and model resolution;
//! - [`ports`] — persistence ports, records and value sets;
//! - [`chat_service`] — chat CRUD;
//! - [`message_service`] — message history, reactions and turn status;
//! - [`quota_service`] — quota preflight (downgrade cascade), reserve,
//!   settlement, quota status and warnings (DESIGN sections 3.2, 5.4);
//! - [`knowledge`] — knowledge search: the retriever port, the
//!   `search_knowledge` tool, its per-request parameters and call handling
//!   (DESIGN section 4 "Knowledge Search");
//! - [`billing`] — the shared billing outcome derivation and the usage /
//!   turn audit event builders (DESIGN sections 5.7-5.9, 3.9);
//! - [`stream`] — streaming turns: setup, replay, provider task and
//!   finalization (DESIGN sections 3.3, 3.6, 5.7);
//! - [`summary_service`] — thread-summary execution: prompt, provider call,
//!   parsing and the CAS commit (DESIGN section 3.6);
//! - [`turn_service`] — retry, edit and delete of the latest turn (DESIGN
//!   section 3.9);
//! - [`attachment_service`] — attachment upload (MIME rules, purposes,
//!   limits, vector store creation, indexing wait and background indexing),
//!   read and deletion (DESIGN sections 3.3, 3.6, 3.7).

pub mod attachment_service;
pub mod authz;
pub mod billing;
pub mod chat_service;
pub mod context;
pub mod error;
pub mod knowledge;
pub mod llm;
pub mod message_service;
pub mod models_service;
pub mod orphan;
pub mod ports;
pub mod quota;
pub mod quota_service;
pub mod sanitize;
pub mod stream;
pub mod summary_service;
pub mod turn_service;
