// SPDX-License-Identifier: MIT

//! Headless UI state machines for Vela.
//!
//! Every transition is pure: `handle(Event) -> Vec<Effect>`.
//! Stale async responses are rejected via monotonic `OpId` tokens, and triage
//! mutations update the visible list optimistically so keyboard navigation stays
//! instantaneous.

pub mod html;

use std::collections::{HashMap, HashSet, VecDeque};

use vela_core::{
    Mailbox, MailboxId, MailboxRole, Message, MessageId, RemoteImagePolicy, ThreadId, ThreadSummary,
};

const CONVERSATION_CACHE_LIMIT: usize = 128;
const INITIAL_PREFETCH_COUNT: usize = 4;
const NEIGHBOR_PREFETCH_COUNT: usize = 3;

/// Monotonic operation identifier used to discard stale async responses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThreadFilter {
    #[default]
    All,
    Unread,
}

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
    pub expanded_quotes: HashSet<MessageId>,
    pub last_undo: Option<UndoEntry>,
    pub status_banner: Option<String>,
    pub filter: ThreadFilter,
    pub search_query: String,
    pub fts_matches: Option<HashSet<ThreadId>>,
    conversation_cache: HashMap<ThreadId, Vec<Message>>,
    conversation_cache_order: VecDeque<ThreadId>,
    prefetching_threads: HashSet<ThreadId>,
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
    ConversationPrefetched {
        thread_id: ThreadId,
        messages: Vec<Message>,
    },
    PrefetchFailed {
        thread_id: ThreadId,
    },
    ArchiveSelected,
    TrashSelected,
    ToggleSelectedRead,
    UndoLastTriage,
    AllowRemoteImages(MessageId),
    ToggleQuote(MessageId),
    SetFilter(ThreadFilter),
    SetSearchQuery {
        query: String,
        fts_matches: Option<HashSet<ThreadId>>,
    },
    ClearFilters,
    ReportError(String),
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
    PrefetchConversation {
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

    pub fn has_active_filter(&self) -> bool {
        self.filter != ThreadFilter::All || !self.search_query.trim().is_empty()
    }

    /// Evict specific threads from the in-memory conversation cache when JMAP
    /// reports them as updated or destroyed.
    pub fn invalidate_cached_threads(&mut self, thread_ids: &[ThreadId]) {
        for tid in thread_ids {
            self.conversation_cache.remove(tid);
            if let Some(pos) = self
                .conversation_cache_order
                .iter()
                .position(|id| id == tid)
            {
                self.conversation_cache_order.remove(pos);
            }
        }
    }

    /// Threads matching the current `All`/`Unread` filter and search query
    /// (combining live substring matching with SQLCipher FTS5 full-body hits).
    pub fn visible_threads(&self) -> Vec<&ThreadSummary> {
        let query = self.search_query.trim().to_lowercase();
        self.threads
            .iter()
            .filter(|thread| {
                if self.filter == ThreadFilter::Unread && !thread.unread {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                if self
                    .fts_matches
                    .as_ref()
                    .is_some_and(|hits| hits.contains(&thread.id))
                {
                    return true;
                }
                thread.subject.to_lowercase().contains(&query)
                    || thread.snippet.to_lowercase().contains(&query)
                    || thread.senders.iter().any(|s| {
                        s.display_name().to_lowercase().contains(&query)
                            || s.email.to_lowercase().contains(&query)
                    })
            })
            .collect()
    }

    /// Seed the UI state from the local encrypted cache on cold start so the
    /// user sees mailboxes and messages before the JMAP network handshake ends.
    pub fn hydrate_from_cache(
        &mut self,
        mailboxes: Vec<Mailbox>,
        mailbox_id: Option<MailboxId>,
        threads: Vec<ThreadSummary>,
        conversation: Option<(ThreadId, Vec<Message>)>,
    ) {
        if mailboxes.is_empty() {
            return;
        }
        self.mailboxes = mailboxes;
        self.selected_mailbox = mailbox_id.or_else(|| {
            self.mailbox_by_role(MailboxRole::Inbox)
                .or_else(|| self.mailboxes.first())
                .map(|m| m.id.clone())
        });
        self.threads = threads;
        self.loading_threads = false;
        if let Some((tid, messages)) = conversation {
            self.cache_conversation(tid.clone(), messages.clone());
            self.selected_thread = Some(tid);
            self.conversation = messages;
            self.loading_conversation = false;
        } else if let Some(first) = self.threads.first() {
            self.selected_thread = Some(first.id.clone());
        }
    }

    /// Populate threads from the local cache immediately when switching mailboxes,
    /// keeping `active_mailbox_op` intact so the live JMAP refresh still reconciles.
    pub fn apply_cached_threads(
        &mut self,
        mailbox_id: &MailboxId,
        threads: Vec<ThreadSummary>,
        first_conversation: Option<(ThreadId, Vec<Message>)>,
    ) {
        if self.selected_mailbox.as_ref() != Some(mailbox_id) || threads.is_empty() {
            return;
        }
        self.threads = threads;
        self.loading_threads = false;
        if let Some((tid, messages)) = first_conversation {
            self.cache_conversation(tid.clone(), messages.clone());
            self.selected_thread = Some(tid);
            self.conversation = messages;
            self.loading_conversation = false;
        } else if let Some(first) = self.threads.first() {
            self.selected_thread = Some(first.id.clone());
        }
    }

    pub fn handle(&mut self, event: InboxEvent) -> Vec<InboxEffect> {
        match event {
            InboxEvent::MailboxesLoaded(mailboxes) => {
                self.mailboxes = mailboxes;
                let target = self
                    .selected_mailbox
                    .as_ref()
                    .filter(|id| self.mailboxes.iter().any(|m| &m.id == *id))
                    .cloned()
                    .or_else(|| {
                        self.mailbox_by_role(MailboxRole::Inbox)
                            .or_else(|| self.mailboxes.first())
                            .map(|m| m.id.clone())
                    });
                if let Some(id) = target {
                    // Preserve already-hydrated threads on screen while refreshing live.
                    let op_id = self.alloc_op();
                    self.selected_mailbox = Some(id.clone());
                    self.active_mailbox_op = Some(op_id);
                    if self.threads.is_empty() {
                        self.loading_threads = true;
                    }
                    return vec![InboxEffect::LoadThreads {
                        op_id,
                        mailbox_id: id,
                    }];
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
                self.prefetching_threads.clear();
                let keep_id = self
                    .selected_thread
                    .as_ref()
                    .filter(|id| self.threads.iter().any(|t| &t.id == *id))
                    .cloned();
                if let Some(id) = keep_id {
                    let idx = self.threads.iter().position(|t| t.id == id).unwrap_or(0);
                    let mut effects = self.select_thread(id);
                    effects.extend(self.prefetch_around(idx, INITIAL_PREFETCH_COUNT));
                    return effects;
                }
                self.selected_thread = None;
                self.conversation.clear();
                if let Some(first) = self.threads.first() {
                    let id = first.id.clone();
                    let mut effects = self.select_thread(id);
                    effects.extend(self.prefetch_around(0, INITIAL_PREFETCH_COUNT));
                    return effects;
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
                self.prefetching_threads.remove(&thread_id);
                if !messages.is_empty() {
                    self.cache_conversation(thread_id.clone(), messages.clone());
                }
                if self.active_thread_op != Some(op_id)
                    || self.selected_thread.as_ref() != Some(&thread_id)
                {
                    return Vec::new();
                }
                self.loading_conversation = false;
                self.conversation = messages;
                Vec::new()
            }
            InboxEvent::ConversationPrefetched {
                thread_id,
                messages,
            } => {
                self.prefetching_threads.remove(&thread_id);
                if !messages.is_empty() {
                    self.cache_conversation(thread_id.clone(), messages.clone());
                }
                if self.selected_thread.as_ref() == Some(&thread_id) && self.loading_conversation {
                    self.active_thread_op = None;
                    self.loading_conversation = false;
                    self.conversation = messages;
                }
                Vec::new()
            }
            InboxEvent::PrefetchFailed { thread_id } => {
                self.prefetching_threads.remove(&thread_id);
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
            InboxEvent::ToggleQuote(message_id) => {
                if !self.expanded_quotes.remove(&message_id) {
                    self.expanded_quotes.insert(message_id);
                }
                Vec::new()
            }
            InboxEvent::SetFilter(filter) => {
                self.filter = filter;
                self.ensure_selection_visible()
            }
            InboxEvent::SetSearchQuery { query, fts_matches } => {
                self.search_query = query;
                self.fts_matches = fts_matches;
                self.ensure_selection_visible()
            }
            InboxEvent::ClearFilters => {
                self.filter = ThreadFilter::All;
                self.search_query.clear();
                self.fts_matches = None;
                self.ensure_selection_visible()
            }
            InboxEvent::ReportError(msg) => {
                self.loading_threads = false;
                self.loading_conversation = false;
                self.status_banner = Some(format!("Error: {msg}"));
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
        self.selected_thread = Some(thread_id.clone());
        let selected_idx = self.threads.iter().position(|t| t.id == thread_id);

        let mut effects = Vec::new();
        if let Some(cached) = self.conversation_cache.get(&thread_id).cloned() {
            self.touch_cache_key(&thread_id);
            self.active_thread_op = None;
            self.loading_conversation = false;
            self.conversation = cached;
        } else {
            self.conversation.clear();
            let op_id = self.alloc_op();
            self.active_thread_op = Some(op_id);
            self.loading_conversation = true;
            effects.push(InboxEffect::LoadConversation { op_id, thread_id });
        }

        if let Some(idx) = selected_idx {
            effects.extend(self.prefetch_around(idx, NEIGHBOR_PREFETCH_COUNT));
        }

        effects
    }

    fn cache_conversation(&mut self, thread_id: ThreadId, messages: Vec<Message>) {
        self.conversation_cache.insert(thread_id.clone(), messages);
        self.touch_cache_key(&thread_id);
        while self.conversation_cache.len() > CONVERSATION_CACHE_LIMIT {
            if let Some(oldest) = self.conversation_cache_order.pop_front() {
                self.conversation_cache.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn touch_cache_key(&mut self, thread_id: &ThreadId) {
        if let Some(pos) = self
            .conversation_cache_order
            .iter()
            .position(|id| id == thread_id)
        {
            self.conversation_cache_order.remove(pos);
        }
        self.conversation_cache_order.push_back(thread_id.clone());
    }

    fn prefetch_around(&mut self, center_idx: usize, max_count: usize) -> Vec<InboxEffect> {
        if self.threads.is_empty() || max_count == 0 {
            return Vec::new();
        }

        let len = self.threads.len();
        let offsets: [isize; 5] = [1, 2, -1, 3, 4];
        let mut effects = Vec::new();

        for offset in offsets {
            if effects.len() >= max_count {
                break;
            }
            let target = center_idx as isize + offset;
            if target < 0 || target >= len as isize {
                continue;
            }
            let thread_id = self.threads[target as usize].id.clone();
            if self.selected_thread.as_ref() == Some(&thread_id)
                || self.conversation_cache.contains_key(&thread_id)
                || self.prefetching_threads.contains(&thread_id)
            {
                continue;
            }
            self.prefetching_threads.insert(thread_id.clone());
            effects.push(InboxEffect::PrefetchConversation { thread_id });
        }

        effects
    }

    fn ensure_selection_visible(&mut self) -> Vec<InboxEffect> {
        let visible_ids: Vec<ThreadId> = self
            .visible_threads()
            .into_iter()
            .map(|t| t.id.clone())
            .collect();
        if let Some(current) = &self.selected_thread
            && visible_ids.contains(current)
        {
            return Vec::new();
        }
        if let Some(first) = visible_ids.into_iter().next() {
            self.select_thread(first)
        } else {
            self.selected_thread = None;
            self.conversation.clear();
            self.loading_conversation = false;
            Vec::new()
        }
    }

    fn step_selection(&mut self, delta: isize) -> Vec<InboxEffect> {
        let visible_ids: Vec<ThreadId> = self
            .visible_threads()
            .into_iter()
            .map(|t| t.id.clone())
            .collect();
        if visible_ids.is_empty() {
            return Vec::new();
        }
        let current = self
            .selected_thread
            .as_ref()
            .and_then(|id| visible_ids.iter().position(|vid| vid == id));

        let next_idx = match current {
            Some(idx) => {
                let target = idx as isize + delta;
                target.clamp(0, (visible_ids.len() - 1) as isize) as usize
            }
            None => 0,
        };

        let next_id = visible_ids[next_idx].clone();
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

        // Returning to t-1 later should hit the cache immediately without another LoadConversation.
        let back = state.handle(InboxEvent::SelectThread(ThreadId("t-1".into())));
        assert!(!state.loading_conversation);
        assert_eq!(state.conversation.len(), 1);
        assert!(
            !back
                .iter()
                .any(|e| matches!(e, InboxEffect::LoadConversation { .. }))
        );
    }

    #[test]
    fn prefetches_neighboring_threads_and_serves_cached_selection_instantly() {
        let mut state = InboxState::new();
        state.threads = vec![
            sample_thread("t-1", "First"),
            sample_thread("t-2", "Second"),
            sample_thread("t-3", "Third"),
        ];

        let effects = state.handle(InboxEvent::SelectThread(ThreadId("t-1".into())));
        assert!(effects.iter().any(|e| matches!(
            e,
            InboxEffect::PrefetchConversation { thread_id } if thread_id == &ThreadId("t-2".into())
        )));
        assert!(effects.iter().any(|e| matches!(
            e,
            InboxEffect::PrefetchConversation { thread_id } if thread_id == &ThreadId("t-3".into())
        )));

        // Prefetch completes in the background for t-2.
        state.handle(InboxEvent::ConversationPrefetched {
            thread_id: ThreadId("t-2".into()),
            messages: vec![Message {
                id: MessageId("m-2".into()),
                thread_id: ThreadId("t-2".into()),
                subject: "Second".into(),
                from: vec![],
                to: vec![],
                cc: vec![],
                received_at: Timestamp::UNIX_EPOCH,
                unread: true,
                text_body: Some("prefetched body".into()),
                html_body: None,
                attachments: vec![],
                has_remote_images: false,
            }],
        });

        // Stepping to t-2 is instantaneous: no LoadConversation effect emitted.
        let next_effects = state.handle(InboxEvent::SelectNextThread);
        assert!(!state.loading_conversation);
        assert_eq!(state.conversation.len(), 1);
        assert_eq!(
            state.conversation[0].text_body.as_deref(),
            Some("prefetched body")
        );
        assert!(
            !next_effects
                .iter()
                .any(|e| matches!(e, InboxEffect::LoadConversation { .. }))
        );
    }

    #[test]
    fn filters_by_unread_and_search_query_including_fts_hits() {
        let mut state = InboxState::new();
        let mut t1 = sample_thread("t-1", "Invoice from Paddle");
        t1.unread = false;
        let t2 = sample_thread("t-2", "Rust weekly digest");
        let t3 = sample_thread("t-3", "Quick sync");
        state.threads = vec![t1, t2, t3];

        // Unread filter hides t-1 and advances selection to t-2.
        state.handle(InboxEvent::SetFilter(ThreadFilter::Unread));
        let visible: Vec<&str> = state
            .visible_threads()
            .into_iter()
            .map(|t| t.id.0.as_str())
            .collect();
        assert_eq!(visible, vec!["t-2", "t-3"]);
        assert_eq!(state.selected_thread, Some(ThreadId("t-2".into())));

        // Search query matches substring OR FTS5 body hit.
        let mut fts = HashSet::new();
        fts.insert(ThreadId("t-3".into()));
        state.handle(InboxEvent::SetSearchQuery {
            query: "rust".into(),
            fts_matches: Some(fts),
        });
        let visible: Vec<&str> = state
            .visible_threads()
            .into_iter()
            .map(|t| t.id.0.as_str())
            .collect();
        assert_eq!(visible, vec!["t-2", "t-3"]);

        // Stepping navigates only within visible results.
        state.handle(InboxEvent::SelectNextThread);
        assert_eq!(state.selected_thread, Some(ThreadId("t-3".into())));

        // Clearing filters restores all threads.
        state.handle(InboxEvent::ClearFilters);
        assert_eq!(state.visible_threads().len(), 3);
    }
}
