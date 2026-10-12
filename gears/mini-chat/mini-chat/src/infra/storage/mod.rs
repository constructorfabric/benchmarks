//! Persistence: scoped entities, migrations and repositories.

mod attachment_repo;
mod chat_repo;
mod conversation_repo;
mod db_error;
pub mod entity;
mod message_repo;
pub mod migrations;
mod mutation_repo;
mod odata;
mod ordering;
mod quota_repo;
mod rows;
mod summary_repo;
mod turn_repo;

pub use attachment_repo::AttachmentRepositoryImpl;
pub use chat_repo::ChatRepositoryImpl;
pub use conversation_repo::ConversationRepositoryImpl;
pub(crate) use db_error::map_scope_err;
pub use message_repo::MessageRepositoryImpl;
pub use mutation_repo::MutationRepositoryImpl;
pub use quota_repo::QuotaRepositoryImpl;
pub use summary_repo::SummaryRepositoryImpl;
pub use turn_repo::TurnRepositoryImpl;
