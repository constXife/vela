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

/// A correspondent seen in cached threads, with how many threads they appear
/// in as a sender (`From`) or recipient (`To`/`Cc`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownAddress {
    pub address: EmailAddress,
    pub as_sender: u32,
    pub as_recipient: u32,
}

impl KnownAddress {
    /// Count correspondents across `(senders, recipients)` of distinct
    /// threads. Addresses match case-insensitively; the first non-empty
    /// display name wins. Sorted by total thread count, then address.
    pub fn tally<'a>(
        threads: impl IntoIterator<Item = (&'a [EmailAddress], &'a [EmailAddress])>,
    ) -> Vec<Self> {
        let mut book: std::collections::HashMap<String, Self> = std::collections::HashMap::new();
        let mut add = |addr: &EmailAddress, sender: bool| {
            let key = addr.email.trim().to_lowercase();
            if key.is_empty() {
                return;
            }
            let entry = book.entry(key).or_insert_with(|| Self {
                address: EmailAddress {
                    name: None,
                    email: addr.email.trim().to_string(),
                },
                as_sender: 0,
                as_recipient: 0,
            });
            if entry.address.name.is_none() {
                entry.address.name = addr
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);
            }
            if sender {
                entry.as_sender += 1;
            } else {
                entry.as_recipient += 1;
            }
        };
        for (senders, recipients) in threads {
            for addr in senders {
                add(addr, true);
            }
            for addr in recipients {
                add(addr, false);
            }
        }
        let mut out: Vec<Self> = book.into_values().collect();
        out.sort_by(|a, b| {
            (b.as_sender + b.as_recipient)
                .cmp(&(a.as_sender + a.as_recipient))
                .then_with(|| a.address.email.cmp(&b.address.email))
        });
        out
    }
}

/// Compact representation used in the center thread list.
///
/// Built from the thread's latest email (JMAP `collapseThreads`), so `senders`
/// and `recipients` describe that email's `From` and `To` + `Cc`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub subject: String,
    pub snippet: String,
    pub senders: Vec<EmailAddress>,
    pub recipients: Vec<EmailAddress>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: Option<&str>, email: &str) -> EmailAddress {
        EmailAddress {
            name: name.map(str::to_string),
            email: email.into(),
        }
    }

    #[test]
    fn tally_counts_roles_case_insensitively_and_ranks_by_total() {
        let t1 = (
            vec![addr(None, "Alice@Example.com")],
            vec![addr(Some("Me"), "me@x.test")],
        );
        let t2 = (
            vec![addr(Some("Alice Smith"), "alice@example.com")],
            vec![addr(None, "me@x.test"), addr(Some(" "), "bob@y.test")],
        );
        let t3 = (vec![addr(None, "me@x.test")], vec![addr(None, "  ")]);
        let book = KnownAddress::tally([
            (t1.0.as_slice(), t1.1.as_slice()),
            (t2.0.as_slice(), t2.1.as_slice()),
            (t3.0.as_slice(), t3.1.as_slice()),
        ]);

        let summary: Vec<(&str, Option<&str>, u32, u32)> = book
            .iter()
            .map(|k| {
                (
                    k.address.email.as_str(),
                    k.address.name.as_deref(),
                    k.as_sender,
                    k.as_recipient,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("me@x.test", Some("Me"), 1, 2),
                ("Alice@Example.com", Some("Alice Smith"), 2, 0),
                ("bob@y.test", None, 0, 1),
            ]
        );
    }
}
