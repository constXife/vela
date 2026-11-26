// SPDX-License-Identifier: MIT

//! JMAP transport layer for Vela (targeting Stalwart via RFC 8620 / 8621).
//!
//! Every list/load path uses JMAP result references so mailbox listing,
//! thread listing, and conversation loading each complete in a single HTTP
//! roundtrip.

pub mod store;

use std::collections::HashMap;
use std::fmt;

use jiff::Timestamp;
use jmap_client::email::{self, Property as EmailProperty};
use jmap_client::thread::Property as ThreadProperty;
use vela_core::{
    Attachment, BlobId, EmailAddress, Mailbox, MailboxId, MailboxRole, Message, MessageId,
    ThreadId, ThreadSummary, VelaError,
};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Maximum inline blob size fetched automatically (10 MiB).
const MAX_INLINE_BLOB_BYTES: usize = 10 * 1024 * 1024;
/// Maximum explicit attachment download size (50 MiB).
const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

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
    /// Base URL of the Stalwart server (e.g. `https://yogg.example.com`).
    pub server_url: String,
    pub credentials: JmapCredentials,
}

impl JmapConfig {
    /// Load connection details from environment variables for prototyping:
    /// - `VELA_SERVER_URL`
    /// - `VELA_TOKEN` (Bearer) or `VELA_USERNAME` + `VELA_PASSWORD` (Basic)
    ///
    /// Returns `Ok(None)` when `VELA_SERVER_URL` is not set.
    pub fn from_env() -> Result<Option<Self>, VelaError> {
        let Ok(server_url) = std::env::var("VELA_SERVER_URL") else {
            return Ok(None);
        };
        let server_url = server_url.trim().to_string();
        if server_url.is_empty() {
            return Ok(None);
        }

        let credentials = if let Ok(token) = std::env::var("VELA_TOKEN") {
            JmapCredentials::Bearer { token }
        } else {
            let username = std::env::var("VELA_USERNAME").map_err(|_| {
                VelaError::Auth("VELA_USERNAME or VELA_TOKEN must be set".to_string())
            })?;
            let password = std::env::var("VELA_PASSWORD").map_err(|_| {
                VelaError::Auth("VELA_PASSWORD must be set when using VELA_USERNAME".to_string())
            })?;
            JmapCredentials::Basic { username, password }
        };

        Ok(Some(Self {
            server_url,
            credentials,
        }))
    }
}

/// Triage mutation requested by the presentation layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadMutation {
    MoveToMailbox(MailboxId),
    SetUnread(bool),
    SetFlagged(bool),
    RestoreMailboxes(Vec<MailboxId>),
}

/// JMAP state tokens tracked for incremental `/changes` synchronization.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncStates {
    pub mailbox_state: Option<String>,
    pub email_state: Option<String>,
    pub thread_state: Option<String>,
}

/// Result of a batched `Mailbox/changes` + `Email/changes` + `Thread/changes` check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesCheck {
    pub mailboxes_changed: bool,
    pub emails_changed: bool,
    pub threads_changed: bool,
    pub invalidated_threads: Vec<ThreadId>,
    pub new_states: SyncStates,
}

/// Live JMAP session handle wrapping `jmap-client`.
pub struct JmapSession {
    client: jmap_client::client::Client,
}

impl fmt::Debug for JmapSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JmapSession { .. }")
    }
}

impl JmapSession {
    /// Discover the JMAP session (`/.well-known/jmap`) and authenticate.
    pub async fn connect(config: &JmapConfig) -> Result<Self, VelaError> {
        let mut builder = jmap_client::client::Client::new();
        if let Some(host) = extract_host(&config.server_url) {
            builder = builder.follow_redirects([host]);
        }
        builder = match &config.credentials {
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

    /// Access the underlying `jmap-client` instance for advanced calls.
    pub fn raw(&self) -> &jmap_client::client::Client {
        &self.client
    }

    /// Fetch all mailboxes in a single `Mailbox/get` request, sorted by role.
    pub async fn list_mailboxes(&self) -> Result<Vec<Mailbox>, VelaError> {
        let (mailboxes, _) = self.list_mailboxes_with_state().await?;
        Ok(mailboxes)
    }

    /// Fetch all mailboxes along with the server's `Mailbox` JMAP state token.
    pub async fn list_mailboxes_with_state(&self) -> Result<(Vec<Mailbox>, String), VelaError> {
        let mut request = self.client.build();
        request.get_mailbox();
        let mut resp = request
            .send_get_mailbox()
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?;
        let state = resp.state().to_string();
        let list = resp.take_list();

        let mut mailboxes: Vec<Mailbox> = list
            .into_iter()
            .filter_map(|mut mb| {
                let id = mb.take_id();
                if id.is_empty() {
                    return None;
                }
                let role = map_role(mb.role());
                Some(Mailbox {
                    id: MailboxId(id),
                    name: mb.name().unwrap_or_default().to_string(),
                    role,
                    parent_id: mb.parent_id().map(|p| MailboxId(p.to_string())),
                    unread_threads: mb.unread_threads() as u32,
                    total_threads: mb.total_threads() as u32,
                })
            })
            .collect();

        mailboxes.sort_by(|a, b| {
            a.role
                .sort_rank()
                .cmp(&b.role.sort_rank())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok((mailboxes, state))
    }

    /// Fetch collapsed thread summaries for a mailbox in a single batched HTTP
    /// roundtrip (`Email/query` -> `Email/get` -> `Thread/get`).
    pub async fn list_threads(
        &self,
        mailbox_id: &MailboxId,
        limit: usize,
    ) -> Result<Vec<ThreadSummary>, VelaError> {
        let (threads, _) = self.list_threads_with_state(mailbox_id, limit).await?;
        Ok(threads)
    }

    /// Fetch collapsed thread summaries along with `Email` and `Thread` JMAP state tokens.
    pub async fn list_threads_with_state(
        &self,
        mailbox_id: &MailboxId,
        limit: usize,
    ) -> Result<(Vec<ThreadSummary>, SyncStates), VelaError> {
        let mut request = self.client.build();

        let query = request.query_email();
        query
            .filter(email::query::Filter::in_mailbox(&mailbox_id.0))
            .sort([email::query::Comparator::received_at().descending()])
            .limit(limit);
        query.arguments().collapse_threads(true);
        let email_ids_ref = query.result_reference();

        let get_emails = request.get_email();
        get_emails.ids_ref(email_ids_ref).properties([
            EmailProperty::Id,
            EmailProperty::ThreadId,
            EmailProperty::Subject,
            EmailProperty::Preview,
            EmailProperty::From,
            EmailProperty::ReceivedAt,
            EmailProperty::Keywords,
            EmailProperty::HasAttachment,
            EmailProperty::MailboxIds,
        ]);
        let thread_ids_ref = get_emails.result_reference(EmailProperty::ThreadId);

        request.get_thread().ids_ref(thread_ids_ref);

        let response = request
            .send()
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?;

        // Responses come back in call order: [0] Email/query, [1] Email/get, [2] Thread/get.
        let mut responses = response.unwrap_method_responses();
        if responses.len() < 3 {
            return Err(VelaError::Transport(
                "incomplete JMAP response for list_threads".into(),
            ));
        }

        let threads_resp = responses
            .pop()
            .unwrap()
            .unwrap_get_thread()
            .map_err(|err| VelaError::Transport(err.to_string()))?;
        let emails_resp = responses
            .pop()
            .unwrap()
            .unwrap_get_email()
            .map_err(|err| VelaError::Transport(err.to_string()))?;
        let query_resp = responses
            .pop()
            .unwrap()
            .unwrap_query_email()
            .map_err(|err| VelaError::Transport(err.to_string()))?;

        let thread_counts: HashMap<String, u32> = threads_resp
            .list()
            .iter()
            .map(|t| (t.id().to_string(), t.email_ids().len() as u32))
            .collect();

        let mut email_map: HashMap<String, jmap_client::email::Email> = emails_resp
            .list()
            .iter()
            .filter_map(|e| e.id().map(|id| (id.to_string(), e.clone())))
            .collect();

        let mut summaries = Vec::with_capacity(query_resp.ids().len());
        for email_id in query_resp.ids() {
            let Some(email) = email_map.remove(email_id) else {
                continue;
            };
            let thread_id = email
                .thread_id()
                .map(str::to_string)
                .unwrap_or_else(|| email_id.clone());
            let message_count = thread_counts.get(&thread_id).copied().unwrap_or(1);
            let keywords = email.keywords();
            let unread = !keywords.contains(&"$seen");
            let flagged = keywords.contains(&"$flagged");

            summaries.push(ThreadSummary {
                id: ThreadId(thread_id),
                subject: email
                    .subject()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("(No subject)")
                    .to_string(),
                snippet: email.preview().unwrap_or_default().to_string(),
                senders: map_addresses(email.from()),
                received_at: map_timestamp(email.received_at()),
                message_count,
                unread,
                flagged,
                has_attachments: email.has_attachment(),
                mailbox_ids: email
                    .mailbox_ids()
                    .into_iter()
                    .map(|id| MailboxId(id.to_string()))
                    .collect(),
            });
        }

        let states = SyncStates {
            mailbox_state: None,
            email_state: Some(emails_resp.state().to_string()),
            thread_state: Some(threads_resp.state().to_string()),
        };

        Ok((summaries, states))
    }

    /// Check for incremental server-side changes since `since` using a single
    /// batched `Mailbox/changes` + `Email/changes` + `Thread/changes` roundtrip.
    ///
    /// If any stored state token is missing or rejected by the server
    /// (`cannotCalculateChanges`), that resource is flagged as changed so the
    /// caller can fall back to a clean full refresh.
    pub async fn check_changes(&self, since: &SyncStates) -> Result<ChangesCheck, VelaError> {
        let mut request = self.client.build();
        let mut requested_mb = false;
        let mut requested_email = false;
        let mut requested_thread = false;

        if let Some(state) = since.mailbox_state.as_deref().filter(|s| !s.is_empty()) {
            request.changes_mailbox(state);
            requested_mb = true;
        }
        if let Some(state) = since.email_state.as_deref().filter(|s| !s.is_empty()) {
            request.changes_email(state);
            requested_email = true;
        }
        if let Some(state) = since.thread_state.as_deref().filter(|s| !s.is_empty()) {
            request.changes_thread(state);
            requested_thread = true;
        }

        if !requested_mb && !requested_email && !requested_thread {
            return Ok(ChangesCheck {
                mailboxes_changed: true,
                emails_changed: true,
                threads_changed: true,
                invalidated_threads: Vec::new(),
                new_states: SyncStates::default(),
            });
        }

        let response = request
            .send()
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?;
        let mut responses = response.unwrap_method_responses().into_iter();

        let mut mailboxes_changed = !requested_mb;
        let mut emails_changed = !requested_email;
        let mut threads_changed = !requested_thread;
        let mut invalidated_threads = Vec::new();
        let mut new_states = since.clone();

        if requested_mb {
            match responses
                .next()
                .and_then(|r| r.unwrap_changes_mailbox().ok())
            {
                Some(ch) => {
                    mailboxes_changed = ch.has_more_changes()
                        || ch.total_changes() > 0
                        || ch.old_state() != ch.new_state();
                    new_states.mailbox_state = Some(ch.new_state().to_string());
                }
                None => {
                    mailboxes_changed = true;
                    new_states.mailbox_state = None;
                }
            }
        }

        if requested_email {
            match responses.next().and_then(|r| r.unwrap_changes_email().ok()) {
                Some(ch) => {
                    emails_changed = ch.has_more_changes()
                        || ch.total_changes() > 0
                        || ch.old_state() != ch.new_state();
                    new_states.email_state = Some(ch.new_state().to_string());
                }
                None => {
                    emails_changed = true;
                    new_states.email_state = None;
                }
            }
        }

        if requested_thread {
            match responses
                .next()
                .and_then(|r| r.unwrap_changes_thread().ok())
            {
                Some(ch) => {
                    threads_changed = ch.has_more_changes()
                        || ch.total_changes() > 0
                        || ch.old_state() != ch.new_state();
                    for id in ch
                        .created()
                        .iter()
                        .chain(ch.updated())
                        .chain(ch.destroyed())
                    {
                        invalidated_threads.push(ThreadId(id.clone()));
                    }
                    new_states.thread_state = Some(ch.new_state().to_string());
                }
                None => {
                    threads_changed = true;
                    new_states.thread_state = None;
                }
            }
        }

        Ok(ChangesCheck {
            mailboxes_changed,
            emails_changed,
            threads_changed,
            invalidated_threads,
            new_states,
        })
    }

    /// Fetch all messages in a thread with text & HTML body values in a single
    /// batched HTTP roundtrip (`Thread/get` -> `Email/get`).
    pub async fn load_conversation(&self, thread_id: &ThreadId) -> Result<Vec<Message>, VelaError> {
        let mut request = self.client.build();

        let get_thread = request.get_thread();
        get_thread.ids([thread_id.0.as_str()]);
        let email_ids_ref = get_thread.result_reference(ThreadProperty::EmailIds);

        let get_emails = request.get_email();
        get_emails.ids_ref(email_ids_ref).properties([
            EmailProperty::Id,
            EmailProperty::ThreadId,
            EmailProperty::Subject,
            EmailProperty::From,
            EmailProperty::To,
            EmailProperty::Cc,
            EmailProperty::ReceivedAt,
            EmailProperty::Keywords,
            EmailProperty::TextBody,
            EmailProperty::HtmlBody,
            EmailProperty::BodyValues,
            EmailProperty::Attachments,
        ]);
        get_emails
            .arguments()
            .fetch_text_body_values(true)
            .fetch_html_body_values(true)
            .max_body_value_bytes(512 * 1024);

        let response = request
            .send()
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?;

        let mut responses = response.unwrap_method_responses();
        if responses.len() < 2 {
            return Err(VelaError::Transport(
                "incomplete JMAP response for load_conversation".into(),
            ));
        }

        let emails_resp = responses
            .pop()
            .unwrap()
            .unwrap_get_email()
            .map_err(|err| VelaError::Transport(err.to_string()))?;
        let thread_resp = responses
            .pop()
            .unwrap()
            .unwrap_get_thread()
            .map_err(|err| VelaError::Transport(err.to_string()))?;

        let ordered_ids: Vec<String> = thread_resp
            .list()
            .first()
            .map(|t| t.email_ids().iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();

        let mut by_id: HashMap<String, jmap_client::email::Email> = emails_resp
            .list()
            .iter()
            .filter_map(|e| e.id().map(|id| (id.to_string(), e.clone())))
            .collect();

        let mut messages = Vec::with_capacity(ordered_ids.len());
        for id in ordered_ids {
            let Some(email) = by_id.remove(&id) else {
                continue;
            };
            let text_body = join_body_parts(&email, email.text_body());
            let html_body = join_body_parts(&email, email.html_body());
            let has_remote_images = html_body
                .as_deref()
                .map(quick_has_remote_images)
                .unwrap_or(false);

            let attachments = email
                .attachments()
                .unwrap_or(&[])
                .iter()
                .filter_map(|part| {
                    let blob_id = part.blob_id()?.to_string();
                    Some(Attachment {
                        blob_id: BlobId(blob_id),
                        name: part.name().map(str::to_string),
                        mime_type: part
                            .content_type()
                            .unwrap_or("application/octet-stream")
                            .to_string(),
                        size: part.size() as u64,
                        cid: part
                            .content_id()
                            .map(|c| c.trim_matches(|ch| ch == '<' || ch == '>').to_string()),
                        is_inline: part
                            .content_disposition()
                            .is_some_and(|d| d.eq_ignore_ascii_case("inline"))
                            || part.content_id().is_some(),
                    })
                })
                .collect();

            messages.push(Message {
                id: MessageId(id),
                thread_id: thread_id.clone(),
                subject: email
                    .subject()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("(No subject)")
                    .to_string(),
                from: map_addresses(email.from()),
                to: map_addresses(email.to()),
                cc: map_addresses(email.cc()),
                received_at: map_timestamp(email.received_at()),
                unread: !email.keywords().contains(&"$seen"),
                text_body,
                html_body,
                attachments,
                has_remote_images,
            });
        }

        Ok(messages)
    }

    /// Apply a triage mutation to every message in a thread.
    pub async fn mutate_thread(
        &self,
        thread_id: &ThreadId,
        mutation: ThreadMutation,
    ) -> Result<(), VelaError> {
        let Some(thread) = self
            .client
            .thread_get(&thread_id.0)
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?
        else {
            return Ok(());
        };

        let email_ids = thread.email_ids();
        if email_ids.is_empty() {
            return Ok(());
        }

        let mut request = self.client.build();
        let set = request.set_email();
        for email_id in email_ids {
            let patch = set.update(email_id);
            match &mutation {
                ThreadMutation::MoveToMailbox(target) => {
                    patch.mailbox_ids([target.0.as_str()]);
                }
                ThreadMutation::SetUnread(unread) => {
                    patch.keyword("$seen", !unread);
                }
                ThreadMutation::SetFlagged(flagged) => {
                    patch.keyword("$flagged", *flagged);
                }
                ThreadMutation::RestoreMailboxes(mailboxes) => {
                    patch.mailbox_ids(mailboxes.iter().map(|m| m.0.as_str()));
                }
            }
        }

        request
            .send_set_email()
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?;
        Ok(())
    }

    /// Download a blob (inline image or attachment) with a size ceiling.
    pub async fn download_blob(&self, blob_id: &BlobId) -> Result<Vec<u8>, VelaError> {
        self.download_with_limit(blob_id, MAX_INLINE_BLOB_BYTES)
            .await
    }

    /// Download an explicit user-requested attachment up to 50 MiB.
    pub async fn download_attachment(&self, blob_id: &BlobId) -> Result<Vec<u8>, VelaError> {
        self.download_with_limit(blob_id, MAX_ATTACHMENT_BYTES)
            .await
    }

    async fn download_with_limit(
        &self,
        blob_id: &BlobId,
        limit: usize,
    ) -> Result<Vec<u8>, VelaError> {
        let bytes = self
            .client
            .download(&blob_id.0)
            .await
            .map_err(|err| VelaError::Transport(err.to_string()))?;
        if bytes.len() > limit {
            return Err(VelaError::Other(format!(
                "blob {} exceeds download limit ({} bytes)",
                blob_id.0,
                bytes.len()
            )));
        }
        Ok(bytes)
    }
}

fn map_role(role: jmap_client::mailbox::Role) -> MailboxRole {
    match role {
        jmap_client::mailbox::Role::Inbox => MailboxRole::Inbox,
        jmap_client::mailbox::Role::Archive => MailboxRole::Archive,
        jmap_client::mailbox::Role::Drafts => MailboxRole::Drafts,
        jmap_client::mailbox::Role::Sent => MailboxRole::Sent,
        jmap_client::mailbox::Role::Trash => MailboxRole::Trash,
        jmap_client::mailbox::Role::Junk => MailboxRole::Junk,
        jmap_client::mailbox::Role::Important
        | jmap_client::mailbox::Role::Other(_)
        | jmap_client::mailbox::Role::None => MailboxRole::Custom,
    }
}

fn map_addresses(addrs: Option<&[jmap_client::email::EmailAddress]>) -> Vec<EmailAddress> {
    addrs
        .unwrap_or(&[])
        .iter()
        .map(|a| EmailAddress {
            name: a.name().map(str::to_string),
            email: a.email().to_string(),
        })
        .collect()
}

fn map_timestamp(secs: Option<i64>) -> Timestamp {
    secs.and_then(|s| Timestamp::from_second(s).ok())
        .unwrap_or(Timestamp::UNIX_EPOCH)
}

fn join_body_parts(
    email: &jmap_client::email::Email,
    parts: Option<&[jmap_client::email::EmailBodyPart]>,
) -> Option<String> {
    let parts = parts?;
    let mut joined = String::new();
    for part in parts {
        if let Some(part_id) = part.part_id()
            && let Some(val) = email.body_value(part_id)
        {
            if !joined.is_empty() {
                joined.push('\n');
            }
            joined.push_str(val.value());
        }
    }
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn extract_host(url: &str) -> Option<String> {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = without_scheme.split('/').next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if host_port.starts_with('[') {
        host_port.split(']').next()?.trim_start_matches('[')
    } else {
        host_port.split(':').next()?
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn quick_has_remote_images(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.contains("src=\"http://")
        || lower.contains("src=\"https://")
        || lower.contains("src='http://")
        || lower.contains("src='https://")
        || lower.contains("url(http://")
        || lower.contains("url(https://")
}
