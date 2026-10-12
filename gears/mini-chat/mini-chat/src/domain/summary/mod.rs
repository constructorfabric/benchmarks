//! Thread summaries: the trigger evaluated at finalization, the durable work item and the
//! outbox-driven handler (DESIGN section 3.6 "Thread Summary Update").

mod model;
mod parse;
mod ports;
mod prompt;
mod service;
mod trigger;

pub use model::{
    CommitOutcome, CommitSummary, Frontier, RangeMessage, SYSTEM_SUBJECT_ID, SYSTEM_TASK_TYPE,
    StoredSummary, SummaryOutcome, SummaryTrigger, SummaryWorkItem,
};
pub use parse::{parse_summary, token_estimate};
pub use ports::SummaryRepository;
pub use prompt::{drop_step, fit, input_budget, user_prompt};
pub use service::{SummaryService, SummaryServiceParts};
pub use trigger::{evaluate, frozen_target};
