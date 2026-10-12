//! Messages API and reactions (DESIGN "List Messages", "Message Reaction API").

mod model;
mod query;
mod service;

pub use model::{AttachmentSummary, MessageRole, MessageView, Reaction, ReactionView, Thumbnail};
pub use query::MessageFilterField;
pub use service::{MessageRepository, MessageService};
