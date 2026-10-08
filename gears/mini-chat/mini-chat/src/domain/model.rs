//! Domain enums and stable error-code strings.
//!
//! Every enum offers `as_str()` (the persisted / wire string) and
//! `parse(&str) -> Option<Self>` (its inverse).

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            /// Persisted / wire representation.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $s),+
                }
            }

            /// Inverse of [`Self::as_str`]; `None` for unknown strings.
            #[must_use]
            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($s => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum! {
    /// `chat_turns.state`.
    TurnState {
        Running => "running",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
    }
}

impl TurnState {
    /// State name exposed by the REST turn-status API.
    #[must_use]
    pub const fn api_state(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "done",
            Self::Failed => "error",
            Self::Cancelled => "cancelled",
        }
    }

    /// `true` for every state except `running`.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

string_enum! {
    /// `messages.role`.
    MessageRole {
        User => "user",
        Assistant => "assistant",
        System => "system",
    }
}

string_enum! {
    /// `message_reactions.reaction`.
    Reaction {
        Like => "like",
        Dislike => "dislike",
    }
}

string_enum! {
    /// `attachments.kind`.
    AttachmentKind {
        Document => "document",
        Image => "image",
    }
}

string_enum! {
    /// `attachments.status`.
    AttachmentStatus {
        Pending => "pending",
        Uploaded => "uploaded",
        Ready => "ready",
        Failed => "failed",
    }
}

string_enum! {
    /// Preflight outcome (`done.quota_decision`).
    QuotaDecision {
        Allow => "allow",
        Downgrade => "downgrade",
    }
}

string_enum! {
    /// Why a turn was downgraded (`done.downgrade_reason`).
    DowngradeReason {
        PremiumQuotaExhausted => "premium_quota_exhausted",
        ForceStandardTier => "force_standard_tier",
        DisablePremiumTier => "disable_premium_tier",
        ModelDisabled => "model_disabled",
    }
}

string_enum! {
    /// `billing_outcome` of usage events.
    BillingOutcome {
        Completed => "completed",
        Failed => "failed",
        Aborted => "aborted",
        SystemTask => "system_task",
    }
}

string_enum! {
    /// `settlement_method` of turns and usage events.
    SettlementMethod {
        Actual => "actual",
        Estimated => "estimated",
        Released => "released",
        None => "none",
    }
}

string_enum! {
    /// Scope of a 429 `quota_exceeded` error.
    QuotaScope {
        Tokens => "tokens",
        WebSearch => "web_search",
        CodeInterpreter => "code_interpreter",
    }
}

string_enum! {
    /// `quota_usage.period_type`.
    PeriodType {
        Daily => "daily",
        Monthly => "monthly",
    }
}

string_enum! {
    /// `quota_usage.bucket`.
    Bucket {
        Total => "total",
        TierPremium => "tier:premium",
    }
}

/// Stable `chat_turns.error_code` / SSE `error.code` values.
pub mod error_codes {
    pub const PROVIDER_ERROR: &str = "provider_error";
    pub const PROVIDER_TIMEOUT: &str = "provider_timeout";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";
    pub const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";
    pub const AGENTIC_ITERATIONS_EXCEEDED: &str = "agentic_iterations_exceeded";
    pub const UNEXPECTED_TOOL_USE: &str = "unexpected_tool_use";
    pub const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";
    pub const FINALIZATION_FAILED: &str = "finalization_failed";
    pub const STREAM_INTERRUPTED: &str = "stream_interrupted";
    pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";
    pub const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";
    pub const TURN_SETUP_FAILED: &str = "turn_setup_failed";
    pub const VALIDATION_ERROR: &str = "validation_error";
    pub const INPUT_TOO_LONG: &str = "input_too_long";
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    pub const INDEXING_FAILED: &str = "indexing_failed";
    pub const UPLOAD_FAILED: &str = "upload_failed";
    pub const VECTOR_STORE_FAILED: &str = "vector_store_failed";
    pub const UPLOAD_ABANDONED: &str = "upload_abandoned";
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod model_tests;
