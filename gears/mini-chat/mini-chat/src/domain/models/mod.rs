//! Model catalog rules and the Models API service.

mod catalog;
mod service;

pub use catalog::{enabled_models, find_enabled_model, resolve_chat_model, select_default_model};
pub use service::ModelsService;
