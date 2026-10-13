//! Azure `OpenAI` storage: the same API as `OpenAI`, served under `/openai` with an
//! `api-version` query parameter. Both are added by `StorageTarget::uri`, so the `OpenAI`
//! implementation is used as is.

pub use super::openai::OpenAiStorage as AzureStorage;
