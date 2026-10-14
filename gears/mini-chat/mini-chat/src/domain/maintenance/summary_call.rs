//! The summary provider call
//! (`cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call` and the OAGW part of
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`): the transport port, the
//! single call within the time left of the provider phase, and the bounded prompt-too-long
//! retries.
//!
//! Deadlines run on the injected [`Timer`]: an attempt is raced against `timer.sleep(time_left)`
//! with the call polled first, so a test timer decides when a hung call ends. The OAGW adapter is
//! `infra::oagw::summary_client::OagwSummaryClient`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::ModelCatalogEntry;
use serde_json::Value;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::domain::attachments::timing::Timer;
use crate::domain::maintenance::settings::{MAX_PROMPT_TOO_LONG_RETRIES, SummaryWorkerSettings};
use crate::domain::maintenance::summary_prompt::{
    SummaryPrompt, drop_oldest_fifth, render_user_prompt,
};
use crate::domain::maintenance::summary_request::{
    SummaryRequest, SummaryRequestInput, build_summary_request,
};
use crate::domain::maintenance::summary_response::{
    SummaryCallFailure, SummaryCompletion, classify_error_response, parse_completion,
};
use crate::domain::provider_registry::EffectiveProvider;

/// A provider answer as the transport read it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpAnswer {
    pub status: u16,
    /// The body: at most 16 KiB of an error answer, at most 1 MiB of a success answer.
    pub body: Vec<u8>,
    /// A success body larger than 1 MiB (the body is then empty).
    pub too_large: bool,
}

/// Why no provider answer exists.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    /// The gateway answered with its own error: an `Err` of `proxy_request`, or a response
    /// marked `ErrorSource::Gateway` (a 502, 503 or 504 Problem included).
    Gateway,
    /// The transport failed: the S2S context is not bound, or the response body could not be
    /// read.
    Transport,
}

/// The one provider call of the summary: `POST` of a JSON body through OAGW under the S2S
/// context with `Content-Type: application/json`, `Accept: application/json` and no credential
/// header; the call is made without retry.
#[async_trait]
pub trait SummaryTransport: Send + Sync {
    /// Send `request` and read the answer (bounded as [`HttpAnswer`] says).
    ///
    /// # Errors
    ///
    /// [`TransportFailure`] when the gateway or the transport failed.
    async fn post_json(&self, uri: &str, body: &Value) -> Result<HttpAnswer, TransportFailure>;
}

/// One attempt: the request, the call within `time_left` and the reading of the answer
/// (`inst-ad-no-time` to `inst-ad-return`).
///
/// # Errors
///
/// [`SummaryCallFailure`] of the class `PromptTooLong` or `Other`.
pub async fn call_once(
    transport: &dyn SummaryTransport,
    timer: &dyn Timer,
    request: &SummaryRequest,
    kind: ProviderKind,
    time_left: Duration,
) -> Result<SummaryCompletion, SummaryCallFailure> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-no-content-logs
    // nothing is logged here: the request body, the prompt, the response text and the error body
    // never leave this function; the caller logs the chat id and the failure class only
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-no-content-logs
    let other = |cause: &str| SummaryCallFailure::Other {
        cause: cause.to_owned(),
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-no-time
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-no-time
    if time_left.is_zero() {
        return Err(other("the provider phase deadline was reached"));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-no-time
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-no-time
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-post
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-transport-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-transport
    // the call is raced against the time left, polled first, and made without retry
    let answer = tokio::select! {
        biased;
        answer = transport.post_json(&request.uri, &request.body) => answer,
        () = timer.sleep(time_left) => {
            return Err(other("the provider call exceeded its deadline"));
        }
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-transport
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-transport-failed
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-post
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-transport-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-transport
    let answer = match answer {
        Ok(answer) => answer,
        Err(TransportFailure::Gateway) => return Err(other("the gateway reported an error")),
        Err(TransportFailure::Transport) => return Err(other("the provider transport failed")),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-transport
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-transport-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-non-2xx
    if !(200..300).contains(&answer.status) {
        return Err(classify_error_response(answer.status, &answer.body));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-non-2xx
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-malformed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-malformed
    if answer.too_large {
        return Err(other("the provider response is malformed"));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-malformed
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-malformed
    parse_completion(kind, &answer.body)
}

/// What the provider-call algorithm needs.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ProviderCall<'a> {
    pub provider: &'a EffectiveProvider,
    pub model: &'a ModelCatalogEntry,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub settings: &'a SummaryWorkerSettings,
    /// The stored summary of the base frontier, when the base is not empty.
    pub base_summary: Option<&'a str>,
    /// The fitted prompt of the frozen range.
    pub prompt: &'a SummaryPrompt,
}

/// The provider-call algorithm over the transport and the timer.
#[domain_model]
pub struct SummaryCaller {
    pub transport: Arc<dyn SummaryTransport>,
    pub timer: Arc<dyn Timer>,
}

impl SummaryCaller {
    /// Call the summary model: the provider phase is bounded by
    /// `settings.provider_phase_bound()` from the entry of this method (retries included), every
    /// attempt gets the time left, an attempt without time left fails without a call, and a
    /// prompt-too-long rejection is retried at most twice with the oldest fifth of the kept
    /// messages dropped (at least two kept).
    ///
    /// # Errors
    ///
    /// [`SummaryCallFailure`] after the retries are exhausted or for any other failure.
    pub async fn call(
        &self,
        call: &ProviderCall<'_>,
    ) -> Result<SummaryCompletion, SummaryCallFailure> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-deadline
        // the provider phase, prompt-too-long retries included, ends `claim_timeout_secs` minus
        // 10 s after its start; every attempt gets the time left of this single bound
        let started = self.timer.monotonic();
        let bound = call.settings.provider_phase_bound();
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-deadline
        let mut kept = call.prompt.kept.clone();
        let mut user = call.prompt.user.clone();
        let mut retries = 0;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-try
        // at most `MAX_PROMPT_TOO_LONG_RETRIES` retries, each with fewer messages: the loop ends
        loop {
            let left = bound.saturating_sub(self.timer.monotonic().saturating_sub(started));
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-build
            // the system prompt as the system instructions, the user prompt as one user message,
            // the output cap of the catalog entry, no tools and no previous response reference
            let request = build_summary_request(&SummaryRequestInput {
                provider: call.provider,
                model: call.model,
                tenant_id: call.tenant_id,
                chat_id: call.chat_id,
                system_prompt: &call.prompt.system,
                user_prompt: &user,
            });
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-build
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-post
            let attempt = call_once(
                self.transport.as_ref(),
                self.timer.as_ref(),
                &request,
                call.provider.kind,
                left,
            )
            .await;
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-post
            match attempt {
                // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-catch-ptl
                // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-ptl-retry
                Err(SummaryCallFailure::PromptTooLong)
                    if retries < MAX_PROMPT_TOO_LONG_RETRIES && kept.len() > 2 =>
                {
                    kept = drop_oldest_fifth(&kept).to_vec();
                    user = render_user_prompt(
                        call.base_summary,
                        &kept,
                        call.settings.message_content_limit,
                    );
                    retries += 1;
                }
                // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-ptl-retry
                // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-catch-ptl
                // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-ptl-exhausted
                // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-catch-other
                // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-return-failure
                // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-return
                // a failure holds its sanitized cause only; a rejected prompt-too-long attempt
                // contributes no usage to the success
                other => return other,
                // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-return
                // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-return-failure
                // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-catch-other
                // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-ptl-exhausted
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-try
    }
}

#[cfg(test)]
#[path = "summary_call_tests.rs"]
mod summary_call_tests;
