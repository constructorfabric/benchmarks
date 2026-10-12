//! REST API of the gear. Routes live under the configured `url_prefix`
//! (default `/mini-chat`) and are registered through `OperationBuilder`.

mod attachment_dto;
mod attachment_handlers;
mod chat_handlers;
pub mod dto;
mod handlers;
mod quota_dto;
mod quota_handlers;
mod routes;
mod stream_dto;
mod stream_handlers;

pub use routes::{
    MiniChatLicense, register_attachment_routes, register_chat_routes, register_models_routes,
    register_quota_routes, register_stream_routes, register_turn_mutation_routes,
};
