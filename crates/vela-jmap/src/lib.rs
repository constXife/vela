// SPDX-License-Identifier: MIT

//! JMAP transport layer for Vela (targeting Stalwart via RFC 8620 / 8621).

use std::fmt;

use vela_core::{Mailbox, MailboxRole, VelaError};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Authentication credentials kept in memory and wiped on drop.
///
/// Intentionally does not derive `Debug` so secrets cannot leak into logs.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub enum JmapCredentials {
    Basic { username: String, password: String },
    Bearer { token: String },
}

impl fmt::Debug for JmapCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Basic { username, .. } => f
                .debug_struct("JmapCredentials::Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::Bearer { .. } => f.write_str("JmapCredentials::Bearer(<redacted>)"),
        }
    }
}

/// Connection configuration for a single JMAP account.
#[derive(Clone, Debug)]
pub struct JmapConfig {
    /// Base URL of the Stalwart server (e.g. `https://mail.example.com`).
    pub server_url: String,
    pub credentials: JmapCredentials,
}

/// Live JMAP session handle wrapping `jmap-client`.
pub struct JmapSession {
    client: jmap_client::client::Client,
}

impl JmapSession {
    /// Discover the JMAP session (`/.well-known/jmap`) and authenticate.
    pub async fn connect(config: &JmapConfig) -> Result<Self, VelaError> {
        let builder = jmap_client::client::Client::new();
        let builder = match &config.credentials {
            JmapCredentials::Basic { username, password } => {
                builder.credentials((username.as_str(), password.as_str()))
            }
            JmapCredentials::Bearer { token } => builder.credentials(token.as_str()),
        };

        let client = builder
            .connect(&config.server_url)
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?;

        tracing::info!(server = %config.server_url, "connected to JMAP server");
        Ok(Self { client })
    }

    /// Access the underlying `jmap-client` instance for batched queries.
    pub fn raw(&self) -> &jmap_client::client::Client {
        &self.client
    }

    /// Fetch the mailbox list sorted by canonical role rank, then name.
    pub async fn list_mailboxes(&self) -> Result<Vec<Mailbox>, VelaError> {
        let ids = self
            .client
            .mailbox_query(None::<jmap_client::mailbox::query::Filter>, None::<Vec<_>>)
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?
            .take_ids();

        let mut mailboxes = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(mb) = self
                .client
                .mailbox_get(&id, None::<Vec<jmap_client::mailbox::Property>>)
                .await
                .map_err(|err| VelaError::Transport(err.to_string()))?
            {
                let role = match mb.role() {
                    jmap_client::mailbox::Role::Inbox => MailboxRole::Inbox,
                    jmap_client::mailbox::Role::Archive => MailboxRole::Archive,
                    jmap_client::mailbox::Role::Drafts => MailboxRole::Drafts,
                    jmap_client::mailbox::Role::Sent => MailboxRole::Sent,
                    jmap_client::mailbox::Role::Trash => MailboxRole::Trash,
                    jmap_client::mailbox::Role::Junk => MailboxRole::Junk,
                    jmap_client::mailbox::Role::Important
                    | jmap_client::mailbox::Role::Other(_)
                    | jmap_client::mailbox::Role::None => MailboxRole::Custom,
                };

                mailboxes.push(Mailbox {
                    id: vela_core::MailboxId(id),
                    name: mb.name().unwrap_or_default().to_string(),
                    role,
                    parent_id: mb
                        .parent_id()
                        .map(|parent| vela_core::MailboxId(parent.to_string())),
                    unread_threads: mb.unread_threads() as u32,
                    total_threads: mb.total_threads() as u32,
                });
            }
        }

        mailboxes.sort_by(|a, b| {
            a.role
                .sort_rank()
                .cmp(&b.role.sort_rank())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(mailboxes)
    }
}
