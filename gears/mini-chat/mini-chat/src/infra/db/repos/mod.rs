//! Repositories: intent-level queries over the secure ORM.
//!
//! User-request paths pass the PDP-compiled [`toolkit_db::secure::AccessScope`];
//! child tables of an already authorized chat are read with an explicit tenant.

pub mod attachment;
pub mod chat;
pub mod message;
pub mod quota;
pub mod reaction;
pub mod thread_summary;
pub mod turn;
pub mod vector_store;

pub use attachment::AttachmentRepo;
pub use chat::ChatRepo;
pub use message::MessageRepo;
pub use quota::QuotaRepo;
pub use reaction::ReactionRepo;
pub use thread_summary::ThreadSummaryRepo;
pub use turn::TurnRepo;
pub use vector_store::VectorStoreRepo;
