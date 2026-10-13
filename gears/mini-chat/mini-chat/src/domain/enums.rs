//! String-valued enumerations stored as text columns (DESIGN section 3.7).

macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The text stored in the database column.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// Parse the stored text; `None` for an unknown value.
            #[must_use]
            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($text => Some(Self::$variant),)+
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

text_enum! {
    /// `messages.role`.
    MessageRole { User => "user", Assistant => "assistant", System => "system" }
}

text_enum! {
    /// `chat_turns.state`.
    TurnState {
        Running => "running",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
    }
}

impl TurnState {
    /// `completed`, `failed` and `cancelled` are immutable terminal states.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

text_enum! {
    /// `chat_turns.requester_type`.
    RequesterType { User => "user", System => "system" }
}

text_enum! {
    /// `attachments.status`.
    AttachmentStatus {
        Pending => "pending",
        Uploaded => "uploaded",
        Ready => "ready",
        Failed => "failed",
    }
}

text_enum! {
    /// `attachments.attachment_kind`.
    AttachmentKind { Document => "document", Image => "image" }
}

text_enum! {
    /// `attachments.cleanup_status`.
    CleanupStatus { Pending => "pending", Done => "done", Failed => "failed" }
}

text_enum! {
    /// `attachments.secondary_status`.
    SecondaryStatus {
        NotAttempted => "not_attempted",
        Pending => "pending",
        Uploaded => "uploaded",
        Failed => "failed",
    }
}

text_enum! {
    /// `attachments.secondary_provider_kind`.
    SecondaryProviderKind { Anthropic => "anthropic" }
}

text_enum! {
    /// `message_reactions.reaction`.
    ReactionKind { Like => "like", Dislike => "dislike" }
}

text_enum! {
    /// `quota_usage.period_type`.
    PeriodType { Daily => "daily", Monthly => "monthly" }
}

text_enum! {
    /// `quota_usage.bucket`.
    QuotaBucket {
        Total => "total",
        TierStandard => "tier:standard",
        TierPremium => "tier:premium",
    }
}

#[cfg(test)]
#[path = "enums_tests.rs"]
mod enums_tests;
