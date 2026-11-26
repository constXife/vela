// SPDX-License-Identifier: MIT

//! Sample in-memory mailbox fixture used before live JMAP is wired up.

use jiff::Timestamp;
use vela_core::{
    EmailAddress, Mailbox, MailboxId, MailboxRole, Message, MessageId, ThreadId, ThreadSummary,
};

pub fn mailboxes() -> Vec<Mailbox> {
    vec![
        Mailbox {
            id: MailboxId("inbox".into()),
            name: "Inbox".into(),
            role: MailboxRole::Inbox,
            parent_id: None,
            unread_threads: 2,
            total_threads: 3,
        },
        Mailbox {
            id: MailboxId("drafts".into()),
            name: "Drafts".into(),
            role: MailboxRole::Drafts,
            parent_id: None,
            unread_threads: 0,
            total_threads: 0,
        },
        Mailbox {
            id: MailboxId("sent".into()),
            name: "Sent".into(),
            role: MailboxRole::Sent,
            parent_id: None,
            unread_threads: 0,
            total_threads: 0,
        },
        Mailbox {
            id: MailboxId("archive".into()),
            name: "Archive".into(),
            role: MailboxRole::Archive,
            parent_id: None,
            unread_threads: 0,
            total_threads: 0,
        },
        Mailbox {
            id: MailboxId("trash".into()),
            name: "Trash".into(),
            role: MailboxRole::Trash,
            parent_id: None,
            unread_threads: 0,
            total_threads: 0,
        },
    ]
}

pub fn threads_for_mailbox(mailbox_id: &MailboxId) -> Vec<ThreadSummary> {
    if mailbox_id.0 != "inbox" {
        return Vec::new();
    }

    vec![
        ThreadSummary {
            id: ThreadId("t-welcome".into()),
            subject: "Welcome to Vela — 3-pane COSMIC prototype".into(),
            snippet: "Use j/k or Up/Down to move, E to archive, Del to trash, U to toggle read, Z to undo.".into(),
            senders: vec![EmailAddress {
                name: Some("Vela Prototype".into()),
                email: "vela@yogg.local".into(),
            }],
            received_at: Timestamp::UNIX_EPOCH,
            message_count: 2,
            unread: true,
            flagged: false,
            has_attachments: false,
            mailbox_ids: vec![MailboxId("inbox".into())],
        },
        ThreadSummary {
            id: ThreadId("t-newsletter".into()),
            subject: "Weekly Rust & COSMIC Digest".into(),
            snippet: "External images in this newsletter are blocked by default until you opt in.".into(),
            senders: vec![EmailAddress {
                name: Some("COSMIC Digest".into()),
                email: "digest@example.org".into(),
            }],
            received_at: Timestamp::UNIX_EPOCH,
            message_count: 1,
            unread: true,
            flagged: false,
            has_attachments: false,
            mailbox_ids: vec![MailboxId("inbox".into())],
        },
        ThreadSummary {
            id: ThreadId("t-stalwart".into()),
            subject: "Stalwart JMAP sync check on yogg".into(),
            snippet: "Next step: wire vela-jmap to the test mailbox and test live EventSource updates.".into(),
            senders: vec![EmailAddress {
                name: Some("Postmaster".into()),
                email: "postmaster@yogg.local".into(),
            }],
            received_at: Timestamp::UNIX_EPOCH,
            message_count: 1,
            unread: false,
            flagged: true,
            has_attachments: false,
            mailbox_ids: vec![MailboxId("inbox".into())],
        },
    ]
}

pub fn messages_for_thread(thread_id: &ThreadId) -> Vec<Message> {
    match thread_id.0.as_str() {
        "t-welcome" => vec![
            Message {
                id: MessageId("m-welcome-1".into()),
                thread_id: thread_id.clone(),
                subject: "Welcome to Vela — 3-pane COSMIC prototype".into(),
                from: vec![EmailAddress {
                    name: Some("Vela Prototype".into()),
                    email: "vela@yogg.local".into(),
                }],
                to: vec![EmailAddress {
                    name: Some("You".into()),
                    email: "you@yogg.local".into(),
                }],
                cc: vec![],
                received_at: Timestamp::UNIX_EPOCH,
                unread: false,
                text_body: Some(
                    "Three-pane layout is live:\n\
                     - Left: mailboxes sorted by JMAP role\n\
                     - Center: thread list with optimistic keyboard triage\n\
                     - Right: conversation view\n\n\
                     Try pressing j/k (or arrows), E (archive), Backspace/Delete (trash), U (unread), and Z (undo)."
                        .into(),
                ),
                html_body: None,
                attachments: vec![],
                has_remote_images: false,
            },
            Message {
                id: MessageId("m-welcome-2".into()),
                thread_id: thread_id.clone(),
                subject: "Re: Welcome to Vela — 3-pane COSMIC prototype".into(),
                from: vec![EmailAddress {
                    name: Some("You".into()),
                    email: "you@yogg.local".into(),
                }],
                to: vec![EmailAddress {
                    name: Some("Vela Prototype".into()),
                    email: "vela@yogg.local".into(),
                }],
                cc: vec![],
                received_at: Timestamp::UNIX_EPOCH,
                unread: true,
                text_body: Some(
                    "Looks smooth! Next up: connect to Stalwart on yogg and run the real HTML/newsletter corpus spike."
                        .into(),
                ),
                html_body: None,
                attachments: vec![],
                has_remote_images: false,
            },
        ],
        "t-newsletter" => vec![Message {
            id: MessageId("m-news-1".into()),
            thread_id: thread_id.clone(),
            subject: "Weekly Rust & COSMIC Digest".into(),
            from: vec![EmailAddress {
                name: Some("COSMIC Digest".into()),
                email: "digest@example.org".into(),
            }],
            to: vec![EmailAddress {
                name: Some("You".into()),
                email: "you@yogg.local".into(),
            }],
            cc: vec![],
            received_at: Timestamp::UNIX_EPOCH,
            unread: true,
            text_body: Some(
                "This sample message simulates an HTML newsletter with external images.\n\n\
                 External images stay blocked until you click «Load external images» in the message header."
                    .into(),
            ),
            html_body: None,
            attachments: vec![],
            has_remote_images: true,
        }],
        "t-stalwart" => vec![Message {
            id: MessageId("m-stalwart-1".into()),
            thread_id: thread_id.clone(),
            subject: "Stalwart JMAP sync check on yogg".into(),
            from: vec![EmailAddress {
                name: Some("Postmaster".into()),
                email: "postmaster@yogg.local".into(),
            }],
            to: vec![EmailAddress {
                name: Some("You".into()),
                email: "you@yogg.local".into(),
            }],
            cc: vec![],
            received_at: Timestamp::UNIX_EPOCH,
            unread: false,
            text_body: Some(
                "Ready for live JMAP spike: mailbox_query, thread_get, email_get, and EventSource state updates."
                    .into(),
            ),
            html_body: None,
            attachments: vec![],
            has_remote_images: false,
        }],
        _ => Vec::new(),
    }
}
