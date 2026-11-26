// SPDX-License-Identifier: MIT

//! Headless UI state machines for Vela.
//!
//! Every transition is pure: `handle(Event) -> Vec<Effect>`.
//! Stale async responses are rejected via monotonic `OpId` tokens, and triage
//! mutations update the visible list optimistically so keyboard navigation stays
//! instantaneous.

use std::collections::HashMap;

use vela_core::{
    Mailbox, MailboxId, MailboxRole, Message, MessageId, RemoteImagePolicy, ThreadId, ThreadSummary,
};

/// Monotonic operation identifier used to discard stale async responses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriageAction {
    Archive,
    Trash,
    ToggleRead { mark_unread: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoEntry {
    pub action: TriageAction,
    pub thread: ThreadSummary,
    pub index: usize,
}

#[derive(Clone, Debug, Default)]
pub struct InboxState {
    pub mailboxes: Vec<Mailbox>,
    pub selected_mailbox: Option<MailboxId>,
    pub threads: Vec<ThreadSummary>,
    pub selected_thread: Option<ThreadId>,
    pub conversation: Vec<Message>,
    pub loading_threads: bool,
    pub loading_conversation: bool,
    pub remote_image_policies: HashMap<MessageId, RemoteImagePolicy>,
    pub last_undo: Option<UndoEntry>,
    pub status_banner: Option<String>,
    next_op: u64,
    active_mailbox_op: Option<OpId>,
    active_thread_op: Option<OpId>,
}

#[derive(Clone, Debug)]
pub enum InboxEvent {
    MailboxesLoaded(Vec<Mailbox>),
    SelectMailbox(MailboxId),
    ThreadsLoaded {
        op_id: OpId,
        mailbox_id: MailboxId,
        threads: Vec<ThreadSummary>,
    },
    SelectThread(ThreadId),
    SelectNextThread,
    SelectPrevThread,
    ConversationLoaded {
        op_id: OpId,
        thread_id: ThreadId,
        messages: Vec<Message>,
    },
    ArchiveSelected,
    TrashSelected,
    ToggleSelectedRead,
    UndoLastTriage,
    AllowRemoteImages(MessageId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxEffect {
    LoadThreads {
        op_id: OpId,
        mailbox_id: MailboxId,
    },
    LoadConversation {
        op_id: OpId,
        thread_id: ThreadId,
    },
    ApplyTriage {
        op_id: OpId,
        thread_id: ThreadId,
        action: TriageAction,
        target_mailbox: Option<MailboxId>,
    },
    RestoreThread {
        op_id: OpId,
        thread_id: ThreadId,
        mailbox_ids: Vec<MailboxId>,
    },
}

impl InboxState {
    pub fn new() -> Self {
        Self::default()
    }

    fn alloc_op(&mut self) -> OpId {
        self.next_op += 1;
        OpId(self.next_op)
    }

    pub fn mailbox_by_role(&self, role: MailboxRole) -> Option<&Mailbox> {
        self.mailboxes.iter().find(|m| m.role == role)
    }

    pub fn remote_image_policy(&self, message_id: &MessageId) -> RemoteImagePolicy {
        self.remote_image_policies
            .get(message_id)
            .copied()
            .unwrap_or(RemoteImagePolicy::Block)
    }

    pub fn handle(&mut self, event: InboxEvent) -> Vec<InboxEffect> {
        match event {
            InboxEvent::MailboxesLoaded(mailboxes) => {
                self.mailboxes = mailboxes;
                if self.selected_mailbox.is_none()
                    && let Some(initial) = self
                        .mailbox_by_role(MailboxRole::Inbox)
                        .or_else(|| self.mailboxes.first())
                {
                    let id = initial.id.clone();
                    return self.select_mailbox(id);
                }
                Vec::new()
            }
            InboxEvent::SelectMailbox(mailbox_id) => self.select_mailbox(mailbox_id),
            InboxEvent::ThreadsLoaded {
                op_id,
                mailbox_id,
                threads,
            } => {
                if self.active_mailbox_op != Some(op_id)
                    || self.selected_mailbox.as_ref() != Some(&mailbox_id)
                {
                    return Vec::new();
                }
                self.loading_threads = false;
                self.threads = threads;
                self.selected_thread = None;
                self.conversation.clear();
                if let Some(first) = self.threads.first() {
                    let id = first.id.clone();
                    return self.select_thread(id);
                }
                Vec::new()
            }
            InboxEvent::SelectThread(thread_id) => self.select_thread(thread_id),
            InboxEvent::SelectNextThread => self.step_selection(1),
            InboxEvent::SelectPrevThread => self.step_selection(-1),
            InboxEvent::ConversationLoaded {
                op_id,
                thread_id,
                messages,
            } => {
                if self.active_thread_op != Some(op_id)
                    || self.selected_thread.as_ref() != Some(&thread_id)
                {
                    return Vec::new();
                }
                self.loading_conversation = false;
                self.conversation = messages;
                Vec::new()
            }
            InboxEvent::ArchiveSelected => {
                self.move_selected(TriageAction::Archive, MailboxRole::Archive)
            }
            InboxEvent::TrashSelected => {
                self.move_selected(TriageAction::Trash, MailboxRole::Trash)
            }
            InboxEvent::ToggleSelectedRead => self.toggle_selected_read(),
            InboxEvent::UndoLastTriage => self.undo_last_triage(),
            InboxEvent::AllowRemoteImages(message_id) => {
                self.remote_image_policies
                    .insert(message_id, RemoteImagePolicy::AllowOnce);
                Vec::new()
            }
        }
    }

    fn select_mailbox(&mut self, mailbox_id: MailboxId) -> Vec<InboxEffect> {
        let op_id = self.alloc_op();
        self.selected_mailbox = Some(mailbox_id.clone());
        self.active_mailbox_op = Some(op_id);
        self.loading_threads = true;
        self.threads.clear();
        self.selected_thread = None;
        self.conversation.clear();
        self.status_banner = None;
        vec![InboxEffect::LoadThreads { op_id, mailbox_id }]
    }

    fn select_thread(&mut self, thread_id: ThreadId) -> Vec<InboxEffect> {
        if self.selected_thread.as_ref() == Some(&thread_id) && !self.conversation.is_empty() {
            return Vec::new();
        }
        let op_id = self.alloc_op();
        self.selected_thread = Some(thread_id.clone());
        self.active_thread_op = Some(op_id);
        self.loading_conversation = true;
        vec![InboxEffect::LoadConversation { op_id, thread_id }]
    }

    fn step_selection(&mut self, delta: isize) -> Vec<InboxEffect> {
        if self.threads.is_empty() {
            return Vec::new();
        }
        let current = self
            .selected_thread
            .as_ref()
            .and_then(|id| self.threads.iter().position(|t| &t.id == id));

        let next_idx = match current {
            Some(idx) => {
                let target = idx as isize + delta;
                target.clamp(0, (self.threads.len() - 1) as isize) as usize
            }
            None => 0,
        };

        let next_id = self.threads[next_idx].id.clone();
        self.select_thread(next_id)
    }

    fn move_selected(&mut self, action: TriageAction, role: MailboxRole) -> Vec<InboxEffect> {
        let Some(selected_id) = self.selected_thread.clone() else {
            return Vec::new();
        };
        let Some(idx) = self.threads.iter().position(|t| t.id == selected_id) else {
            return Vec::new();
        };

        let target_mailbox = self.mailbox_by_role(role).map(|m| m.id.clone());
        let removed = self.threads.remove(idx);
        let op_id = self.alloc_op();

        self.last_undo = Some(UndoEntry {
            action: action.clone(),
            thread: removed.clone(),
            index: idx,
        });
        self.status_banner = Some(match action {
            TriageAction::Archive => format!("Archived «{}» — press Z to undo", removed.subject),
            TriageAction::Trash => {
                format!("Moved «{}» to Trash — press Z to undo", removed.subject)
            }
            TriageAction::ToggleRead { .. } => String::new(),
        });

        let mut effects = vec![InboxEffect::ApplyTriage {
            op_id,
            thread_id: removed.id,
            action,
            target_mailbox,
        }];

        // Immediately advance selection to the next remaining thread.
        if self.threads.is_empty() {
            self.selected_thread = None;
            self.conversation.clear();
            self.loading_conversation = false;
        } else {
            let next_idx = idx.min(self.threads.len() - 1);
            let next_id = self.threads[next_idx].id.clone();
            effects.extend(self.select_thread(next_id));
        }

        effects
    }

    fn toggle_selected_read(&mut self) -> Vec<InboxEffect> {
        let Some(selected_id) = self.selected_thread.clone() else {
            return Vec::new();
        };
        let Some(thread) = self.threads.iter_mut().find(|t| t.id == selected_id) else {
            return Vec::new();
        };
        thread.unread = !thread.unread;
        let mark_unread = thread.unread;
        let op_id = self.alloc_op();
        vec![InboxEffect::ApplyTriage {
            op_id,
            thread_id: selected_id,
            action: TriageAction::ToggleRead { mark_unread },
            target_mailbox: None,
        }]
    }

    fn undo_last_triage(&mut self) -> Vec<InboxEffect> {
        let Some(entry) = self.last_undo.take() else {
            return Vec::new();
        };
        let insert_at = entry.index.min(self.threads.len());
        let thread_id = entry.thread.id.clone();
        let mailbox_ids = entry.thread.mailbox_ids.clone();
        self.threads.insert(insert_at, entry.thread);
        self.status_banner = Some("Action undone".to_string());

        let op_id = self.alloc_op();
        let mut effects = vec![InboxEffect::RestoreThread {
            op_id,
            thread_id: thread_id.clone(),
            mailbox_ids,
        }];
        effects.extend(self.select_thread(thread_id));
        effects
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use vela_core::{
        EmailAddress, Mailbox, MailboxId, MailboxRole, Message, MessageId, ThreadId, ThreadSummary,
    };

    use super::*;

    fn sample_mailboxes() -> Vec<Mailbox> {
        vec![
            Mailbox {
                id: MailboxId("mb-inbox".into()),
                name: "Inbox".into(),
                role: MailboxRole::Inbox,
                parent_id: None,
                unread_threads: 2,
                total_threads: 2,
            },
            Mailbox {
                id: MailboxId("mb-archive".into()),
                name: "Archive".into(),
                role: MailboxRole::Archive,
                parent_id: None,
                unread_threads: 0,
                total_threads: 0,
            },
            Mailbox {
                id: MailboxId("mb-trash".into()),
                name: "Trash".into(),
                role: MailboxRole::Trash,
                parent_id: None,
                unread_threads: 0,
                total_threads: 0,
            },
        ]
    }

    fn sample_thread(id: &str, subject: &str) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId(id.into()),
            subject: subject.into(),
            snippet: "Snippet".into(),
            senders: vec![EmailAddress {
                name: Some("Alice".into()),
                email: "alice@example.com".into(),
            }],
            received_at: Timestamp::UNIX_EPOCH,
            message_count: 1,
            unread: true,
            flagged: false,
            has_attachments: false,
            mailbox_ids: vec![MailboxId("mb-inbox".into())],
        }
    }

    #[test]
    fn optimistic_archive_advances_selection_and_supports_undo() {
        let mut state = InboxState::new();
        let effects = state.handle(InboxEvent::MailboxesLoaded(sample_mailboxes()));
        let InboxEffect::LoadThreads { op_id, mailbox_id } = &effects[0] else {
            panic!("expected LoadThreads effect");
        };

        state.handle(InboxEvent::ThreadsLoaded {
            op_id: *op_id,
            mailbox_id: mailbox_id.clone(),
            threads: vec![
                sample_thread("t-1", "First"),
                sample_thread("t-2", "Second"),
            ],
        });
        assert_eq!(state.selected_thread, Some(ThreadId("t-1".into())));

        let archive_effects = state.handle(InboxEvent::ArchiveSelected);
        assert_eq!(state.threads.len(), 1);
        assert_eq!(state.selected_thread, Some(ThreadId("t-2".into())));
        assert!(matches!(
            archive_effects.first(),
            Some(InboxEffect::ApplyTriage {
                action: TriageAction::Archive,
                target_mailbox: Some(MailboxId(id)),
                ..
            }) if id == "mb-archive"
        ));

        let undo_effects = state.handle(InboxEvent::UndoLastTriage);
        assert_eq!(state.threads.len(), 2);
        assert_eq!(state.selected_thread, Some(ThreadId("t-1".into())));
        assert!(matches!(
            undo_effects.first(),
            Some(InboxEffect::RestoreThread { thread_id, .. }) if thread_id == &ThreadId("t-1".into())
        ));
    }

    #[test]
    fn stale_conversation_load_is_rejected() {
        let mut state = InboxState::new();
        state.threads = vec![
            sample_thread("t-1", "First"),
            sample_thread("t-2", "Second"),
        ];

        let first = state.handle(InboxEvent::SelectThread(ThreadId("t-1".into())));
        let InboxEffect::LoadConversation {
            op_id: stale_op, ..
        } = first[0]
        else {
            panic!("expected LoadConversation");
        };

        let second = state.handle(InboxEvent::SelectNextThread);
        let InboxEffect::LoadConversation {
            op_id: fresh_op, ..
        } = second[0]
        else {
            panic!("expected LoadConversation");
        };
        assert_ne!(stale_op, fresh_op);

        // Stale response for t-1 arrives after user moved to t-2: must be ignored.
        state.handle(InboxEvent::ConversationLoaded {
            op_id: stale_op,
            thread_id: ThreadId("t-1".into()),
            messages: vec![Message {
                id: MessageId("m-1".into()),
                thread_id: ThreadId("t-1".into()),
                subject: "First".into(),
                from: vec![],
                to: vec![],
                cc: vec![],
                received_at: Timestamp::UNIX_EPOCH,
                unread: true,
                text_body: Some("stale body".into()),
                html_body: None,
                attachments: vec![],
                has_remote_images: false,
            }],
        });

        assert!(state.conversation.is_empty());
        assert!(state.loading_conversation);
    }
}
