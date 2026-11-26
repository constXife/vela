// SPDX-License-Identifier: MIT

//! Domain models, identifiers, and pure policies for Vela.
//!
//! This crate is intentionally free of UI, network, and OS dependencies.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Opaque server-assigned mailbox identifier (JMAP `Mailbox.id`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MailboxId(pub String);

/// Opaque server-assigned thread identifier (JMAP `Thread.id`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub String);

/// Opaque server-assigned email identifier (JMAP `Email.id`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

/// Opaque server-assigned blob identifier (JMAP `blobId`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobId(pub String);

/// Canonical JMAP mailbox roles (RFC 8621 / IANA IMAP Mailbox Name Attributes).
///
/// Actions like Archive and Trash always target roles rather than localized
/// folder names so Vela stays consistent with Thunderbird on the same server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MailboxRole {
    Inbox,
    Archive,
    Drafts,
    Sent,
    Trash,
    Junk,
    Custom,
}

impl MailboxRole {
    pub fn from_jmap_role(role: Option<&str>) -> Self {
        match role {
            Some("inbox") => Self::Inbox,
            Some("archive") => Self::Archive,
            Some("drafts") => Self::Drafts,
            Some("sent") => Self::Sent,
            Some("trash") => Self::Trash,
            Some("junk") => Self::Junk,
            _ => Self::Custom,
        }
    }

    pub fn sort_rank(self) -> u8 {
        match self {
            Self::Inbox => 0,
            Self::Drafts => 1,
            Self::Sent => 2,
            Self::Archive => 3,
            Self::Junk => 4,
            Self::Trash => 5,
            Self::Custom => 6,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: MailboxId,
    pub name: String,
    pub role: MailboxRole,
    pub parent_id: Option<MailboxId>,
    pub unread_threads: u32,
    pub total_threads: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: String,
}

impl EmailAddress {
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.email)
    }
}

/// Compact representation used in the center thread list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub subject: String,
    pub snippet: String,
    pub senders: Vec<EmailAddress>,
    pub received_at: Timestamp,
    pub message_count: u32,
    pub unread: bool,
    pub flagged: bool,
    pub has_attachments: bool,
    pub mailbox_ids: Vec<MailboxId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub blob_id: BlobId,
    pub name: Option<String>,
    pub mime_type: String,
    pub size: u64,
    pub cid: Option<String>,
    pub is_inline: bool,
}

/// Full message payload loaded on demand for the conversation pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub subject: String,
    pub from: Vec<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub received_at: Timestamp,
    pub unread: bool,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub attachments: Vec<Attachment>,
    pub has_remote_images: bool,
}

/// Policy controlling external `http(s)://` image loading for a message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteImagePolicy {
    #[default]
    Block,
    AllowOnce,
}

#[derive(Debug, thiserror::Error)]
pub enum VelaError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("network or protocol error: {0}")]
    Transport(String),
    #[error("mailbox with role {0:?} not found on server")]
    MissingRole(MailboxRole),
    #[error("{0}")]
    Other(String),
}
