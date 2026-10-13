//! Test support: an in-process application wired like the gear (see
//! [`TestApp`]), fixed identities, a mock PDP and catalog fixtures.
//!
//! Always compiled (integration tests use it); not part of the public API.

pub mod authz;
pub mod fake_provider;
pub mod harness;
pub mod images;
pub mod providers;
pub mod seed;
pub mod sse;
pub mod upload;
pub mod users;

pub use authz::{MockPdp, PdpMode};
pub use fake_provider::{FakeFile, FakeProvider, FakeVectorStore, RecordedRequest, ScriptedStream};
pub use harness::{TestApp, TestAppBuilder, TestResponse};
pub use sse::{DropHandle, SseCapture, parse_sse};
pub use users::TestUser;

/// Run a whole test whose expectations depend on the current UTC quota periods
/// (rows seeded for "today", read back through the service's clock) and run
/// it once more when it failed while the UTC day changed under it.
///
/// # Panics
/// When `body` fails without a day change, or fails twice.
pub async fn midnight_safe<F, Fut>(body: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let today = || crate::domain::estimation::period_starts(crate::domain::clock::now_utc()).0;
    for attempt in 1..=2 {
        let day = today();
        let Err(err) = tokio::spawn(body()).await else {
            return;
        };
        if attempt == 1 && today() != day {
            continue;
        }
        match err.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(err) => panic!("test body did not complete: {err}"),
        }
    }
}

/// Model catalog fixtures.
pub mod catalog {
    use mini_chat_sdk::{
        EstimationBudgets, ModelApiParams, ModelCatalogEntry, ModelFeatures, ModelGeneralConfig,
        ModelPreference, ModelSupportedEndpoints, ModelTier, ModelToolSupport,
    };

    /// Capability enabling image input.
    pub const VISION_INPUT: &str = "VISION_INPUT";

    fn model(id: &str, tier: ModelTier) -> ModelCatalogEntry {
        ModelCatalogEntry {
            id: id.to_owned(),
            provider_model_id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            icon: String::new(),
            provider_id: "openai".to_owned(),
            provider_display_name: "OpenAI".to_owned(),
            tier,
            enabled: true,
            multimodal_capabilities: Vec::new(),
            context_window: 128_000,
            max_output_tokens: 4096,
            max_input_tokens: 0,
            input_tokens_credit_multiplier_micro: 1_000_000,
            output_tokens_credit_multiplier_micro: 3_000_000,
            multiplier_display: String::new(),
            estimation_budgets: EstimationBudgets::default(),
            max_num_results: 5,
            web_search_context_size: "low".to_owned(),
            max_tool_calls: 2,
            general_config: ModelGeneralConfig {
                model_type: "chat".to_owned(),
                available_from: String::new(),
                max_file_size_mb: 25,
                api_params: ModelApiParams::default(),
                features: ModelFeatures {
                    streaming: true,
                    structured_output: false,
                },
                tool_support: ModelToolSupport::default(),
                supported_endpoints: ModelSupportedEndpoints {
                    responses: true,
                    ..ModelSupportedEndpoints::default()
                },
            },
            preference: None,
            system_prompt: String::new(),
            thread_summary_prompt: String::new(),
        }
    }

    /// Enabled premium model `id` (provider `openai`, no capabilities, no tools).
    #[must_use]
    pub fn premium_model(id: &str) -> ModelCatalogEntry {
        model(id, ModelTier::Premium)
    }

    /// Enabled standard model `id` (provider `openai`, no capabilities, no tools).
    #[must_use]
    pub fn standard_model(id: &str) -> ModelCatalogEntry {
        model(id, ModelTier::Standard)
    }

    /// `gpt-premium` (premium, default, `VISION_INPUT`, every tool) and
    /// `gpt-standard` (standard).
    #[must_use]
    pub fn default_catalog() -> Vec<ModelCatalogEntry> {
        let mut premium = premium_model("gpt-premium");
        premium.preference = Some(ModelPreference {
            is_default: true,
            sort_order: 0,
        });
        premium.multimodal_capabilities = vec![VISION_INPUT.to_owned()];
        premium.general_config.tool_support = ModelToolSupport {
            web_search: true,
            file_search: true,
            image_generation: true,
            code_interpreter: true,
            mcp: true,
        };
        vec![premium, standard_model("gpt-standard")]
    }
}
