//! Database layer: `SeaORM` entities, migrations and the string-valued enums stored as text.

pub mod entity;
pub mod migrations;
pub mod repo;
pub mod ts;
pub mod tx;

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            /// Value stored in the database column.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// Parses the stored value; `None` for an unknown string.
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
    /// `chat_turns.state`.
    TurnState {
        Running => "running",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
    }
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
    AttachmentKind {
        Document => "document",
        Image => "image",
    }
}

text_enum! {
    /// `messages.role`.
    MessageRole {
        User => "user",
        Assistant => "assistant",
        System => "system",
    }
}

text_enum! {
    /// `attachments.cleanup_status`.
    CleanupStatus {
        Pending => "pending",
        Done => "done",
        Failed => "failed",
    }
}

#[cfg(test)]
mod tests;
