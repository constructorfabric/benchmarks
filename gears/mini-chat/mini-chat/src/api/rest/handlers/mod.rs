//! REST handlers, one module per resource area.

mod prelude;

mod chats;
mod messages;
mod stream;
mod turns;
mod attachments;
mod reactions;
mod models;

pub use chats::*;
pub use messages::*;
pub use stream::*;
pub use turns::*;
pub use attachments::*;
pub use reactions::*;
pub use models::*;
