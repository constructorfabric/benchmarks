//! Turn model and the turn status API (DESIGN "Turn Status API", section 3.9).

mod model;
mod service;

pub use model::{TurnRecord, TurnState, TurnStatus};
pub use service::TurnService;
