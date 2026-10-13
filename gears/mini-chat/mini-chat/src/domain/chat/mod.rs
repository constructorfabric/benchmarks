//! The chat aggregate: model, repository port and service.

mod model;
mod query;
mod repo;
mod service;
mod title;

pub use model::{Chat, ChatDetail, NewChat};
pub use query::ChatFilterField;
pub use repo::ChatRepository;
pub use service::ChatService;
pub use title::normalize_title;
