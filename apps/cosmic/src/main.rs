// SPDX-License-Identifier: MIT

//! Vela COSMIC desktop shell.
//!
//! Keeps all state transitions in `vela-flows` and runs all JMAP network I/O
//! asynchronously off the UI thread so scrolling and keyboard triage stay smooth.
//! Credentials are stored in the OS Keyring (`org.freedesktop.secrets`) with
//! non-secret account metadata in `~/.config/vela/config.toml` (`0600`).

mod sample;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use cosmic::app::{Core, Settings, Task};
use cosmic::iced::core::layout::Limits;
use cosmic::iced::{Alignment, Color, Event, Length, Size, Subscription, event, keyboard};
use cosmic::{Element, executor, widget};
use jiff::{Timestamp, Zoned, tz::TimeZone};
use vela_cache::VelaCache;
use vela_core::{
    Attachment, BlobId, EmailAddress, Mailbox, MailboxId, MailboxRole, Message as MailMessage,
    MessageId, RemoteImagePolicy, ThreadId, ThreadSummary,
};
use vela_flows::{InboxEffect, InboxEvent, InboxState, ThreadFilter, TriageAction, html};
use vela_jmap::{
    ChangesCheck, JmapConfig, JmapCredentials, JmapSession, SyncStates, ThreadMutation, store,
};
use zeroize::Zeroize;

const WINDOW_SIZE: Size = Size::new(1280.0, 760.0);
const WINDOW_MIN: Size = Size::new(960.0, 600.0);
const INITIAL_THREAD_LIMIT: usize = 50;
const DEFAULT_SERVER_URL: &str = "https://mail.example.com";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vela=info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let new_instance = args.iter().any(|a| a == "--new-instance")
        || std::env::var_os("VELA_NEW_INSTANCE").is_some();
    let sample_mode =
        args.iter().any(|a| a == "--sample") || std::env::var_os("VELA_SAMPLE").is_some();

    let settings = Settings::default().size(WINDOW_SIZE).size_limits(
        Limits::NONE
            .min_width(WINDOW_MIN.width)
            .min_height(WINDOW_MIN.height),
    );
    let flags = Flags { sample_mode };
    if new_instance {
        cosmic::app::run::<App>(settings, flags)?;
    } else {
        cosmic::app::run_single_instance::<App>(settings, flags)?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct Flags {
    sample_mode: bool,
}

impl cosmic::app::CosmicFlags for Flags {
    type SubCommand = String;
    type Args = Vec<String>;
}

enum Screen {
    Starting,
    Connect(ConnectForm),
    Inbox,
}

struct ConnectForm {
    server_url: String,
    username: String,
    password: String,
    connecting: bool,
    error: Option<String>,
}

impl ConnectForm {
    fn from_saved_or_default() -> Self {
        let saved = store::load_account_config().ok().flatten();
        Self {
            server_url: saved
                .as_ref()
                .map(|c| c.server_url.clone())
                .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string()),
            username: saved.map(|c| c.username).unwrap_or_default(),
            password: String::new(),
            connecting: false,
            error: None,
        }
    }

    fn wipe_password(&mut self) {
        self.password.zeroize();
    }
}

struct App {
    core: Core,
    screen: Screen,
    inbox: InboxState,
    session: Option<Arc<JmapSession>>,
    cache: Option<Arc<VelaCache>>,
    accounts: Vec<store::AccountConfig>,
    active_account_id: Option<String>,
    syncing: bool,
    sync_states: SyncStates,
    downloading_blobs: HashSet<BlobId>,
    last_saved_attachment: Option<PathBuf>,
    show_shortcuts: bool,
}

type ConnectedAccount = (String, Arc<JmapSession>, Vec<Mailbox>, String);

#[derive(Clone, Debug)]
struct CachedSnapshot {
    account_id: String,
    cache: Arc<VelaCache>,
    mailboxes: Vec<Mailbox>,
    mailbox_id: Option<MailboxId>,
    threads: Vec<ThreadSummary>,
    conversation: Option<(ThreadId, Vec<MailMessage>)>,
    sync_states: SyncStates,
}

#[derive(Clone, Debug)]
enum Message {
    CacheWarmed(Option<Box<CachedSnapshot>>),
    CacheReady {
        account_id: String,
        cache: Option<Arc<VelaCache>>,
    },
    StartupResolved(Result<Option<ConnectedAccount>, String>),
    ServerUrlChanged(String),
    UsernameChanged(String),
    PasswordChanged(String),
    SubmitConnect,
    ConnectFinished(Result<ConnectedAccount, String>),
    OpenSampleMode,
    OpenAddAccount,
    CancelAddAccount,
    SwitchAccount(String),
    SignOut,
    SignedOut(Option<String>),
    SelectMailbox(MailboxId),
    ThreadsLoaded {
        op_id: vela_flows::OpId,
        mailbox_id: MailboxId,
        result: Result<(Vec<ThreadSummary>, SyncStates), String>,
    },
    SelectThread(ThreadId),
    SelectNext,
    SelectPrev,
    ConversationLoaded {
        op_id: vela_flows::OpId,
        thread_id: ThreadId,
        result: Result<Vec<MailMessage>, String>,
    },
    ConversationPrefetched {
        thread_id: ThreadId,
        result: Result<Vec<MailMessage>, String>,
    },
    Archive,
    Trash,
    RestoreToInbox,
    ToggleRead,
    ToggleFlag,
    Undo,
    TriageFinished(Result<(), String>),
    AllowRemoteImages(MessageId),
    ToggleQuote(MessageId),
    FocusSearch,
    SearchChanged(String),
    SetFilter(ThreadFilter),
    ClearFilters,
    SyncNow,
    SyncChangesChecked(Result<ChangesCheck, String>),
    SyncMailboxesFinished(Result<(Vec<Mailbox>, String), String>),
    DownloadAttachment(Attachment),
    AttachmentDownloaded {
        blob_id: BlobId,
        result: Result<PathBuf, String>,
    },
    OpenSavedAttachment(PathBuf),
    ToggleShortcuts,
    CloseShortcuts,
    EscapePressed,
}

impl App {
    fn refresh_accounts_from_disk(&mut self) {
        if let Ok(cfg) = store::load_accounts_config() {
            self.accounts = cfg.accounts;
        }
    }

    fn dispatch(&mut self, event: InboxEvent) -> Task<Message> {
        let effects = self.inbox.handle(event);
        let mut tasks = Vec::new();
        for effect in effects {
            tasks.push(self.execute_effect(effect));
        }
        Task::batch(tasks)
    }

    fn execute_effect(&mut self, effect: InboxEffect) -> Task<Message> {
        match effect {
            InboxEffect::LoadThreads { op_id, mailbox_id } => {
                if let Some(cache) = &self.cache
                    && let Ok(cached_threads) = cache.load_threads(&mailbox_id)
                    && !cached_threads.is_empty()
                {
                    let first_conv = cached_threads.first().and_then(|first| {
                        cache
                            .load_conversation(&first.id)
                            .ok()
                            .flatten()
                            .map(|msgs| (first.id.clone(), msgs))
                    });
                    self.inbox
                        .apply_cached_threads(&mailbox_id, cached_threads, first_conv);
                }

                if let Some(session) = self.session.clone() {
                    let mb = mailbox_id.clone();
                    cosmic::task::future(async move {
                        let result = session
                            .list_threads_with_state(&mb, INITIAL_THREAD_LIMIT)
                            .await
                            .map_err(|e| e.to_string());
                        Message::ThreadsLoaded {
                            op_id,
                            mailbox_id: mb,
                            result,
                        }
                    })
                } else if self.cache.is_none() {
                    let threads = sample::threads_for_mailbox(&mailbox_id);
                    self.dispatch(InboxEvent::ThreadsLoaded {
                        op_id,
                        mailbox_id,
                        threads,
                    })
                } else {
                    Task::none()
                }
            }
            InboxEffect::LoadConversation { op_id, thread_id } => {
                if let Some(cache) = &self.cache
                    && let Ok(Some(messages)) = cache.load_conversation(&thread_id)
                {
                    return self.dispatch(InboxEvent::ConversationLoaded {
                        op_id,
                        thread_id,
                        messages,
                    });
                }

                if let Some(session) = self.session.clone() {
                    let tid = thread_id.clone();
                    cosmic::task::future(async move {
                        let result = session
                            .load_conversation(&tid)
                            .await
                            .map_err(|e| e.to_string());
                        Message::ConversationLoaded {
                            op_id,
                            thread_id: tid,
                            result,
                        }
                    })
                } else if self.cache.is_none() {
                    let messages = sample::messages_for_thread(&thread_id);
                    self.dispatch(InboxEvent::ConversationLoaded {
                        op_id,
                        thread_id,
                        messages,
                    })
                } else {
                    Task::none()
                }
            }
            InboxEffect::PrefetchConversation { thread_id } => {
                if let Some(cache) = &self.cache
                    && let Ok(Some(messages)) = cache.load_conversation(&thread_id)
                {
                    return self.dispatch(InboxEvent::ConversationPrefetched {
                        thread_id,
                        messages,
                    });
                }

                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let tid = thread_id.clone();
                cosmic::task::future(async move {
                    let result = session
                        .load_conversation(&tid)
                        .await
                        .map_err(|e| e.to_string());
                    Message::ConversationPrefetched {
                        thread_id: tid,
                        result,
                    }
                })
            }
            InboxEffect::ApplyTriage {
                thread_id,
                action,
                target_mailbox,
                ..
            } => {
                if let Some(cache) = self.cache.clone() {
                    let mbs = self.inbox.mailboxes.clone();
                    let active_mb = self.inbox.selected_mailbox.clone();
                    let threads = self.inbox.threads.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = cache.save_mailboxes(&mbs);
                        if let Some(mb) = active_mb {
                            let _ = cache.save_threads(&mb, &threads);
                        }
                    });
                }
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mutation = match action {
                    TriageAction::Archive | TriageAction::Trash | TriageAction::MoveToInbox => {
                        match target_mailbox {
                            Some(target) => ThreadMutation::MoveToMailbox(target),
                            None => return Task::none(),
                        }
                    }
                    TriageAction::ToggleRead { mark_unread } => {
                        ThreadMutation::SetUnread(mark_unread)
                    }
                    TriageAction::ToggleFlag { flagged } => ThreadMutation::SetFlagged(flagged),
                };
                cosmic::task::future(async move {
                    let result = session
                        .mutate_thread(&thread_id, mutation)
                        .await
                        .map_err(|e| e.to_string());
                    Message::TriageFinished(result)
                })
            }
            InboxEffect::RestoreThread {
                thread_id,
                mailbox_ids,
                ..
            } => {
                if let Some(cache) = self.cache.clone() {
                    let mbs = self.inbox.mailboxes.clone();
                    let active_mb = self.inbox.selected_mailbox.clone();
                    let threads = self.inbox.threads.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = cache.save_mailboxes(&mbs);
                        if let Some(mb) = active_mb {
                            let _ = cache.save_threads(&mb, &threads);
                        }
                    });
                }
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                cosmic::task::future(async move {
                    let result = session
                        .mutate_thread(&thread_id, ThreadMutation::RestoreMailboxes(mailbox_ids))
                        .await
                        .map_err(|e| e.to_string());
                    Message::TriageFinished(result)
                })
            }
        }
    }
}

impl cosmic::Application for App {
    type Executor = executor::Default;
    type Flags = Flags;
    type Message = Message;

    const APP_ID: &'static str = "com.rlyeh.vela.Cosmic";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Self::Message>) {
        let accounts_cfg = if flags.sample_mode {
            store::AccountsConfig::default()
        } else {
            store::load_accounts_config().unwrap_or_default()
        };
        let active_id = accounts_cfg.active().map(|a| a.id.clone());

        let mut app = Self {
            core,
            screen: Screen::Starting,
            inbox: InboxState::new(),
            session: None,
            cache: None,
            accounts: accounts_cfg.accounts,
            active_account_id: active_id.clone(),
            syncing: !flags.sample_mode && active_id.is_some(),
            sync_states: SyncStates::default(),
            downloading_blobs: HashSet::new(),
            last_saved_attachment: None,
            show_shortcuts: false,
        };

        if flags.sample_mode {
            app.screen = Screen::Inbox;
            let task = app.dispatch(InboxEvent::MailboxesLoaded(sample::mailboxes()));
            return (app, task);
        }

        if let Some(account_id) = active_id {
            let task = spawn_account_load_tasks(account_id);
            (app, task)
        } else {
            app.screen = Screen::Connect(ConnectForm::from_saved_or_default());
            (app, Task::none())
        }
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        if !matches!(self.screen, Screen::Inbox) {
            return Subscription::none();
        }

        event::listen_with(|ev, status, _window| {
            if matches!(status, event::Status::Captured) {
                return None;
            }
            let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = ev else {
                return None;
            };
            if modifiers.control() || modifiers.alt() || modifiers.logo() {
                return None;
            }
            match key {
                keyboard::Key::Named(keyboard::key::Named::ArrowDown) => Some(Message::SelectNext),
                keyboard::Key::Named(keyboard::key::Named::ArrowUp) => Some(Message::SelectPrev),
                keyboard::Key::Named(
                    keyboard::key::Named::Delete | keyboard::key::Named::Backspace,
                ) => Some(Message::Trash),
                keyboard::Key::Named(keyboard::key::Named::Escape) => Some(Message::EscapePressed),
                keyboard::Key::Character(ref c) => match c.as_str() {
                    "j" => Some(Message::SelectNext),
                    "k" => Some(Message::SelectPrev),
                    "e" => Some(Message::Archive),
                    "u" => Some(Message::ToggleRead),
                    "s" => Some(Message::ToggleFlag),
                    "z" => Some(Message::Undo),
                    "r" => Some(Message::SyncNow),
                    "/" => Some(Message::FocusSearch),
                    "?" => Some(Message::ToggleShortcuts),
                    _ => None,
                },
                _ => None,
            }
        })
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::CacheWarmed(Some(snap)) => {
                if self.active_account_id.as_deref() != Some(snap.account_id.as_str()) {
                    return Task::none();
                }
                self.cache = Some(snap.cache);
                if self.sync_states.mailbox_state.is_none() {
                    self.sync_states.mailbox_state = snap.sync_states.mailbox_state;
                }
                if self.sync_states.email_state.is_none() {
                    self.sync_states.email_state = snap.sync_states.email_state;
                }
                if self.sync_states.thread_state.is_none() {
                    self.sync_states.thread_state = snap.sync_states.thread_state;
                }
                if self.session.is_none()
                    && !snap.mailboxes.is_empty()
                    && !matches!(self.screen, Screen::Connect(_))
                {
                    self.inbox.hydrate_from_cache(
                        snap.mailboxes,
                        snap.mailbox_id,
                        snap.threads,
                        snap.conversation,
                    );
                    self.screen = Screen::Inbox;
                }
                Task::none()
            }
            Message::CacheWarmed(None) => Task::none(),
            Message::CacheReady { account_id, cache } => {
                if self.active_account_id.as_deref() != Some(account_id.as_str()) {
                    return Task::none();
                }
                if let Some(cache) = cache {
                    if !self.inbox.mailboxes.is_empty() {
                        let c = cache.clone();
                        let mbs = self.inbox.mailboxes.clone();
                        let states = self.sync_states.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = c.save_mailboxes(&mbs);
                            persist_sync_states(&c, &states);
                        });
                    }
                    self.cache = Some(cache);
                }
                Task::none()
            }
            Message::StartupResolved(Ok(Some((account_id, session, mailboxes, mb_state)))) => {
                if self.active_account_id.as_deref() != Some(account_id.as_str()) {
                    return Task::none();
                }
                self.sync_states.mailbox_state = Some(mb_state);
                if let Some(cache) = self.cache.clone() {
                    let mbs = mailboxes.clone();
                    let states = self.sync_states.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = cache.save_mailboxes(&mbs);
                        persist_sync_states(&cache, &states);
                    });
                }
                self.session = Some(session);
                self.refresh_accounts_from_disk();
                self.syncing = false;
                self.screen = Screen::Inbox;
                self.dispatch(InboxEvent::MailboxesLoaded(mailboxes))
            }
            Message::StartupResolved(Ok(None)) => {
                self.syncing = false;
                self.screen = Screen::Connect(ConnectForm::from_saved_or_default());
                Task::none()
            }
            Message::StartupResolved(Err(err)) => {
                self.syncing = false;
                if self.inbox.mailboxes.is_empty() {
                    let mut form = ConnectForm::from_saved_or_default();
                    form.error = Some(err);
                    self.screen = Screen::Connect(form);
                    Task::none()
                } else {
                    // Stay usable offline on cached mail and surface the network error in the banner.
                    self.dispatch(InboxEvent::ReportError(format!("Offline: {err}")))
                }
            }
            Message::ServerUrlChanged(val) => {
                if let Screen::Connect(form) = &mut self.screen {
                    form.server_url = val;
                }
                Task::none()
            }
            Message::UsernameChanged(val) => {
                if let Screen::Connect(form) = &mut self.screen {
                    form.username = val;
                }
                Task::none()
            }
            Message::PasswordChanged(val) => {
                if let Screen::Connect(form) = &mut self.screen {
                    form.password = val;
                }
                Task::none()
            }
            Message::SubmitConnect => {
                let Screen::Connect(form) = &mut self.screen else {
                    return Task::none();
                };
                if form.connecting {
                    return Task::none();
                }
                let server_url = form.server_url.trim().to_string();
                let username = form.username.trim().to_string();
                let password = form.password.clone();
                if server_url.is_empty() || username.is_empty() || password.is_empty() {
                    form.error = Some("Enter server URL, username, and password.".into());
                    return Task::none();
                }
                form.connecting = true;
                form.error = None;

                let config = JmapConfig {
                    server_url,
                    credentials: JmapCredentials::Basic { username, password },
                };
                cosmic::task::future(async move {
                    let result = async {
                        let session = Arc::new(JmapSession::connect(&config).await?);
                        let (mailboxes, mb_state) = session.list_mailboxes_with_state().await?;
                        let saved = store::save_jmap_config(&config).await?;
                        Ok::<_, vela_core::VelaError>((saved.id, session, mailboxes, mb_state))
                    }
                    .await
                    .map_err(|e| e.to_string());
                    Message::ConnectFinished(result)
                })
            }
            Message::ConnectFinished(Ok((account_id, session, mailboxes, mb_state))) => {
                if let Screen::Connect(form) = &mut self.screen {
                    form.wipe_password();
                }
                self.refresh_accounts_from_disk();
                self.active_account_id = Some(account_id.clone());
                self.sync_states = SyncStates {
                    mailbox_state: Some(mb_state),
                    email_state: None,
                    thread_state: None,
                };
                self.session = Some(session);
                self.cache = None;
                self.syncing = false;
                self.inbox = InboxState::new();
                self.screen = Screen::Inbox;
                let aid = account_id.clone();
                let open_cache_task = cosmic::task::future(async move {
                    let cache = match store::load_or_create_cache_key_for(&aid).await {
                        Ok(key) => tokio::task::spawn_blocking({
                            let aid = aid.clone();
                            move || VelaCache::open_for_account(&aid, &key).ok().map(Arc::new)
                        })
                        .await
                        .ok()
                        .flatten(),
                        Err(_) => None,
                    };
                    Message::CacheReady {
                        account_id: aid,
                        cache,
                    }
                });
                Task::batch([
                    open_cache_task,
                    self.dispatch(InboxEvent::MailboxesLoaded(mailboxes)),
                ])
            }
            Message::ConnectFinished(Err(err)) => {
                if let Screen::Connect(form) = &mut self.screen {
                    form.connecting = false;
                    form.error = Some(err);
                }
                Task::none()
            }
            Message::OpenSampleMode => {
                if let Screen::Connect(form) = &mut self.screen {
                    form.wipe_password();
                }
                self.session = None;
                self.cache = None;
                self.active_account_id = None;
                self.syncing = false;
                self.sync_states = SyncStates::default();
                self.inbox = InboxState::new();
                self.screen = Screen::Inbox;
                self.dispatch(InboxEvent::MailboxesLoaded(sample::mailboxes()))
            }
            Message::OpenAddAccount => {
                self.screen = Screen::Connect(ConnectForm {
                    server_url: DEFAULT_SERVER_URL.to_string(),
                    username: String::new(),
                    password: String::new(),
                    connecting: false,
                    error: None,
                });
                Task::none()
            }
            Message::CancelAddAccount => {
                if let Screen::Connect(form) = &mut self.screen {
                    form.wipe_password();
                }
                self.screen = Screen::Inbox;
                Task::none()
            }
            Message::SwitchAccount(account_id) => {
                if self.active_account_id.as_deref() == Some(account_id.as_str())
                    && self.session.is_some()
                {
                    return Task::none();
                }
                let _ = store::set_active_account(&account_id);
                self.active_account_id = Some(account_id.clone());
                self.session = None;
                self.cache = None;
                self.syncing = true;
                self.sync_states = SyncStates::default();
                self.inbox = InboxState::new();
                self.screen = Screen::Inbox;
                spawn_account_load_tasks(account_id)
            }
            Message::SignOut => {
                let removing = self.active_account_id.take();
                self.session = None;
                self.cache = None;
                self.syncing = false;
                self.sync_states = SyncStates::default();
                self.inbox = InboxState::new();
                cosmic::task::future(async move {
                    let next_active = if let Some(account_id) = removing {
                        let next = store::remove_account(&account_id).await.ok().flatten();
                        let _ = tokio::task::spawn_blocking(move || {
                            vela_cache::destroy_account_cache(&account_id)
                        })
                        .await;
                        next
                    } else {
                        let _ = store::clear_saved_secret().await;
                        let _ = store::clear_cache_key().await;
                        let _ =
                            tokio::task::spawn_blocking(vela_cache::destroy_default_cache).await;
                        None
                    };
                    Message::SignedOut(next_active)
                })
            }
            Message::SignedOut(next_account_id) => {
                self.refresh_accounts_from_disk();
                if let Some(next_id) = next_account_id {
                    self.active_account_id = Some(next_id.clone());
                    self.syncing = true;
                    self.screen = Screen::Inbox;
                    spawn_account_load_tasks(next_id)
                } else {
                    self.active_account_id = None;
                    self.screen = Screen::Connect(ConnectForm::from_saved_or_default());
                    Task::none()
                }
            }
            Message::SelectMailbox(id) => self.dispatch(InboxEvent::SelectMailbox(id)),
            Message::ThreadsLoaded {
                op_id,
                mailbox_id,
                result,
            } => match result {
                Ok((threads, states)) => {
                    if let Some(email_state) = states.email_state {
                        self.sync_states.email_state = Some(email_state);
                    }
                    if let Some(thread_state) = states.thread_state {
                        self.sync_states.thread_state = Some(thread_state);
                    }
                    if let Some(cache) = self.cache.clone() {
                        let mb = mailbox_id.clone();
                        let ts = threads.clone();
                        let st = self.sync_states.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = cache.save_threads(&mb, &ts);
                            persist_sync_states(&cache, &st);
                        });
                    }
                    self.dispatch(InboxEvent::ThreadsLoaded {
                        op_id,
                        mailbox_id,
                        threads,
                    })
                }
                Err(err) => self.dispatch(InboxEvent::ReportError(err)),
            },
            Message::SelectThread(id) => self.dispatch(InboxEvent::SelectThread(id)),
            Message::SelectNext => self.dispatch(InboxEvent::SelectNextThread),
            Message::SelectPrev => self.dispatch(InboxEvent::SelectPrevThread),
            Message::ConversationLoaded {
                op_id,
                thread_id,
                result,
            } => match result {
                Ok(messages) => {
                    if let Some(cache) = self.cache.clone() {
                        let tid = thread_id.clone();
                        let msgs = messages.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = cache.save_conversation(&tid, &msgs);
                        });
                    }
                    self.dispatch(InboxEvent::ConversationLoaded {
                        op_id,
                        thread_id,
                        messages,
                    })
                }
                Err(err) => self.dispatch(InboxEvent::ReportError(err)),
            },
            Message::ConversationPrefetched { thread_id, result } => match result {
                Ok(messages) => {
                    if let Some(cache) = self.cache.clone() {
                        let tid = thread_id.clone();
                        let msgs = messages.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = cache.save_conversation(&tid, &msgs);
                        });
                    }
                    self.dispatch(InboxEvent::ConversationPrefetched {
                        thread_id,
                        messages,
                    })
                }
                Err(_) => self.dispatch(InboxEvent::PrefetchFailed { thread_id }),
            },
            Message::Archive => self.dispatch(InboxEvent::ArchiveSelected),
            Message::Trash => self.dispatch(InboxEvent::TrashSelected),
            Message::RestoreToInbox => self.dispatch(InboxEvent::MoveSelectedToInbox),
            Message::ToggleRead => self.dispatch(InboxEvent::ToggleSelectedRead),
            Message::ToggleFlag => self.dispatch(InboxEvent::ToggleSelectedFlag),
            Message::Undo => self.dispatch(InboxEvent::UndoLastTriage),
            Message::TriageFinished(Ok(())) => Task::none(),
            Message::TriageFinished(Err(err)) => self.dispatch(InboxEvent::ReportError(err)),
            Message::AllowRemoteImages(id) => self.dispatch(InboxEvent::AllowRemoteImages(id)),
            Message::ToggleQuote(id) => self.dispatch(InboxEvent::ToggleQuote(id)),
            Message::FocusSearch => widget::text_input::focus(search_input_id()),
            Message::SearchChanged(query) => {
                let fts_matches = if query.trim().is_empty() {
                    None
                } else {
                    self.cache
                        .as_ref()
                        .and_then(|c| c.search_threads(&query, 100).ok())
                        .map(|hits| hits.into_iter().collect::<HashSet<_>>())
                };
                self.dispatch(InboxEvent::SetSearchQuery { query, fts_matches })
            }
            Message::SetFilter(filter) => self.dispatch(InboxEvent::SetFilter(filter)),
            Message::ClearFilters => self.dispatch(InboxEvent::ClearFilters),
            Message::SyncNow => {
                if self.syncing {
                    return Task::none();
                }
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                self.syncing = true;
                let since = self.sync_states.clone();
                cosmic::task::future(async move {
                    let result = session
                        .check_changes(&since)
                        .await
                        .map_err(|e| e.to_string());
                    Message::SyncChangesChecked(result)
                })
            }
            Message::SyncChangesChecked(Ok(check)) => {
                if !check.invalidated_threads.is_empty() {
                    self.inbox
                        .invalidate_cached_threads(&check.invalidated_threads);
                    if let Some(cache) = self.cache.clone() {
                        let invalid = check.invalidated_threads.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = cache.invalidate_threads(&invalid);
                        });
                    }
                }
                if !check.mailboxes_changed && !check.emails_changed && !check.threads_changed {
                    self.syncing = false;
                    return Task::none();
                }
                self.sync_states = check.new_states;
                let Some(session) = self.session.clone() else {
                    self.syncing = false;
                    return Task::none();
                };
                cosmic::task::future(async move {
                    let result = session
                        .list_mailboxes_with_state()
                        .await
                        .map_err(|e| e.to_string());
                    Message::SyncMailboxesFinished(result)
                })
            }
            Message::SyncChangesChecked(Err(err)) => {
                self.syncing = false;
                self.dispatch(InboxEvent::ReportError(format!("Sync failed: {err}")))
            }
            Message::SyncMailboxesFinished(Ok((mailboxes, mb_state))) => {
                self.syncing = false;
                self.sync_states.mailbox_state = Some(mb_state);
                if let Some(cache) = self.cache.clone() {
                    let mbs = mailboxes.clone();
                    let st = self.sync_states.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = cache.save_mailboxes(&mbs);
                        persist_sync_states(&cache, &st);
                    });
                }
                self.dispatch(InboxEvent::MailboxesLoaded(mailboxes))
            }
            Message::SyncMailboxesFinished(Err(err)) => {
                self.syncing = false;
                self.dispatch(InboxEvent::ReportError(format!("Sync failed: {err}")))
            }
            Message::DownloadAttachment(attachment) => {
                if !self.downloading_blobs.insert(attachment.blob_id.clone()) {
                    return Task::none();
                }
                let blob_id = attachment.blob_id.clone();
                let filename = attachment
                    .name
                    .clone()
                    .unwrap_or_else(|| "attachment".to_string());
                let session = self.session.clone();
                cosmic::task::future(async move {
                    let result = async {
                        let bytes = if let Some(session) = session {
                            session
                                .download_attachment(&blob_id)
                                .await
                                .map_err(|e| e.to_string())?
                        } else {
                            format!("Sample attachment placeholder for {filename}\n").into_bytes()
                        };
                        tokio::task::spawn_blocking(move || save_to_downloads(&filename, &bytes))
                            .await
                            .map_err(|e| e.to_string())?
                    }
                    .await;
                    Message::AttachmentDownloaded { blob_id, result }
                })
            }
            Message::AttachmentDownloaded { blob_id, result } => {
                self.downloading_blobs.remove(&blob_id);
                match result {
                    Ok(path) => {
                        self.last_saved_attachment = Some(path.clone());
                        self.inbox.last_undo = None;
                        self.inbox.status_banner =
                            Some(format!("Saved attachment to {}", path.display()));
                        Task::none()
                    }
                    Err(err) => self.dispatch(InboxEvent::ReportError(format!(
                        "Attachment download failed: {err}"
                    ))),
                }
            }
            Message::OpenSavedAttachment(path) => {
                if let Err(err) = std::process::Command::new("xdg-open").arg(&path).spawn() {
                    return self.dispatch(InboxEvent::ReportError(format!(
                        "Failed to open {}: {err}",
                        path.display()
                    )));
                }
                Task::none()
            }
            Message::ToggleShortcuts => {
                self.show_shortcuts = !self.show_shortcuts;
                Task::none()
            }
            Message::CloseShortcuts => {
                self.show_shortcuts = false;
                Task::none()
            }
            Message::EscapePressed => {
                if self.show_shortcuts {
                    self.show_shortcuts = false;
                    Task::none()
                } else if self.inbox.has_active_filter() {
                    self.dispatch(InboxEvent::ClearFilters)
                } else {
                    Task::none()
                }
            }
        }
    }

    fn dialog(&self) -> Option<Element<'_, Self::Message>> {
        if self.show_shortcuts {
            Some(self.view_shortcuts_dialog())
        } else {
            None
        }
    }

    fn view(&self) -> Element<'_, Self::Message> {
        match &self.screen {
            Screen::Starting => widget::container(widget::text::body("Connecting to account…"))
                .center(Length::Fill)
                .into(),
            Screen::Connect(form) => self.view_connect(form),
            Screen::Inbox => self.view_inbox(),
        }
    }
}

impl App {
    fn view_shortcuts_dialog(&self) -> Element<'_, Message> {
        let content = widget::column::with_capacity(6)
            .spacing(12)
            .push(shortcut_section(
                "Navigation",
                &[
                    ("j  /  ↓", "Next conversation"),
                    ("k  /  ↑", "Previous conversation"),
                ],
            ))
            .push(widget::divider::horizontal::light())
            .push(shortcut_section(
                "Triage",
                &[
                    ("E", "Archive conversation"),
                    ("Del  /  Backspace", "Move to Trash"),
                    ("U", "Toggle Read / Unread"),
                    ("S", "Flag / Unflag conversation"),
                    ("Z", "Undo last triage action"),
                ],
            ))
            .push(widget::divider::horizontal::light())
            .push(shortcut_section(
                "Search & Global",
                &[
                    ("/", "Focus search bar"),
                    ("R", "Sync mailboxes & threads"),
                    ("Esc", "Close dialog / clear filters"),
                    ("?", "Toggle this shortcuts overlay"),
                ],
            ));

        widget::dialog()
            .title("Keyboard Shortcuts")
            .control(content)
            .primary_action(widget::button::suggested("Close").on_press(Message::CloseShortcuts))
            .into()
    }
    fn view_connect<'a>(&self, form: &'a ConnectForm) -> Element<'a, Message> {
        let mut card = widget::column::with_capacity(8)
            .spacing(12)
            .push(widget::text::title2("Sign in to Vela"))
            .push(widget::text::caption(
                "Password is stored securely in the OS Keyring; only server URL and username are saved in ~/.config/vela/config.toml.",
            ))
            .push(
                widget::text_input("Server URL (e.g. https://mail.example.com)", &form.server_url)
                    .on_input(Message::ServerUrlChanged)
                    .on_submit(|_| Message::SubmitConnect),
            )
            .push(
                widget::text_input("Username or email", &form.username)
                    .on_input(Message::UsernameChanged)
                    .on_submit(|_| Message::SubmitConnect),
            )
            .push(
                widget::text_input("Password", &form.password)
                    .password()
                    .on_input(Message::PasswordChanged)
                    .on_submit(|_| Message::SubmitConnect),
            );

        if let Some(err) = &form.error {
            card = card.push(widget::text::caption(format!("Error: {err}")));
        }

        let connect_label = if form.connecting {
            "Connecting…"
        } else {
            "Connect"
        };
        let mut connect_btn = widget::button::suggested(connect_label);
        if !form.connecting {
            connect_btn = connect_btn.on_press(Message::SubmitConnect);
        }

        let secondary_action = if !self.accounts.is_empty() {
            widget::button::text("Cancel").on_press(Message::CancelAddAccount)
        } else {
            widget::button::text("Preview sample data").on_press(Message::OpenSampleMode)
        };

        let actions = widget::row::with_capacity(2)
            .push(connect_btn)
            .push(secondary_action)
            .spacing(12)
            .align_y(Alignment::Center);

        card = card.push(actions);

        let boxed = widget::container(card)
            .padding(24)
            .width(Length::Fixed(440.0))
            .class(cosmic::theme::Container::Card);

        widget::container(boxed).center(Length::Fill).into()
    }

    fn view_inbox(&self) -> Element<'_, Message> {
        let mailboxes_col = self.view_mailboxes();
        let threads_col = self.view_threads();
        let conversation_col = self.view_conversation();

        let panes = widget::row::with_capacity(5)
            .push(mailboxes_col)
            .push(widget::divider::vertical::default())
            .push(threads_col)
            .push(widget::divider::vertical::default())
            .push(conversation_col)
            .width(Length::Fill)
            .height(Length::Fill);

        let mut root = widget::column::with_capacity(3)
            .push(panes)
            .width(Length::Fill)
            .height(Length::Fill);

        if let Some(banner) = &self.inbox.status_banner {
            let mut row = widget::row::with_capacity(3)
                .push(widget::text::body(banner))
                .push(widget::space::horizontal())
                .align_y(Alignment::Center)
                .spacing(12);
            if self.inbox.last_undo.is_some() {
                row = row.push(widget::button::text("Undo (Z)").on_press(Message::Undo));
            } else if let Some(path) = &self.last_saved_attachment {
                row = row.push(
                    widget::button::text("Open")
                        .on_press(Message::OpenSavedAttachment(path.clone())),
                );
            }
            let status_bar = widget::container(row).padding([8, 16]).width(Length::Fill);
            root = root
                .push(widget::divider::horizontal::default())
                .push(status_bar);
        }

        root.into()
    }

    fn view_mailboxes(&self) -> Element<'_, Message> {
        let is_syncing = self.syncing || self.inbox.loading_threads;
        let sync_tooltip = if is_syncing {
            "Syncing…"
        } else if self.session.is_some() {
            "Sync now (R)"
        } else {
            "Sample mode"
        };
        let mut sync_btn =
            widget::button::icon(widget::icon::from_name("view-refresh-symbolic")).padding(6);
        if self.session.is_some() && !is_syncing {
            sync_btn = sync_btn.on_press(Message::SyncNow);
        }

        let header = widget::container(
            widget::row::with_capacity(3)
                .push(widget::text::title4("Mailboxes"))
                .push(widget::space::horizontal())
                .push(widget::tooltip(
                    sync_btn,
                    widget::text::caption(sync_tooltip),
                    widget::tooltip::Position::Bottom,
                ))
                .align_y(Alignment::Center),
        )
        .padding([0, 12, 0, 16])
        .align_y(Alignment::Center)
        .height(Length::Fixed(52.0))
        .width(Length::Fill);

        let mut list = widget::column::with_capacity(self.inbox.mailboxes.len())
            .spacing(4)
            .padding([8, 10]);

        for mb in &self.inbox.mailboxes {
            let selected = self.inbox.selected_mailbox.as_ref() == Some(&mb.id);
            let mut row = widget::row::with_capacity(4)
                .spacing(10)
                .align_y(Alignment::Center)
                .push(widget::icon::from_name(mailbox_icon_name(mb.role)).size(16))
                .push(widget::text::body(&mb.name));

            if mb.unread_threads > 0 {
                row = row.push(widget::space::horizontal()).push(
                    widget::text::caption(mb.unread_threads.to_string()).class(if selected {
                        cosmic::theme::Text::Accent
                    } else {
                        cosmic::theme::Text::Custom(muted_text_style)
                    }),
                );
            }

            let btn = widget::button::custom(row)
                .class(cosmic::theme::Button::ListItem([8.0; 4]))
                .selected(selected)
                .padding([8, 10])
                .width(Length::Fill)
                .on_press(Message::SelectMailbox(mb.id.clone()));
            list = list.push(btn);
        }

        let mut footer_col = widget::column::with_capacity(self.accounts.len() + 2).spacing(4);
        if !self.accounts.is_empty() {
            for acc in &self.accounts {
                let is_active = self.active_account_id.as_deref() == Some(acc.id.as_str());
                let sub = if is_active && is_syncing {
                    "Syncing…".to_string()
                } else {
                    acc.host()
                };
                let item = widget::column::with_capacity(2)
                    .spacing(1)
                    .push(
                        widget::text::caption(html::clean_snippet(&acc.username, 22))
                            .class(cosmic::theme::Text::Custom(primary_text_style)),
                    )
                    .push(
                        widget::text::caption(html::clean_snippet(&sub, 22))
                            .class(cosmic::theme::Text::Custom(muted_text_style)),
                    );
                let acc_btn = widget::button::custom(item)
                    .class(cosmic::theme::Button::ListItem([6.0; 4]))
                    .selected(is_active)
                    .padding([6, 8])
                    .width(Length::Fill)
                    .on_press(Message::SwitchAccount(acc.id.clone()));
                footer_col = footer_col.push(acc_btn);
            }

            let footer_actions = widget::row::with_capacity(3)
                .spacing(4)
                .align_y(Alignment::Center)
                .push(widget::tooltip(
                    widget::button::icon(widget::icon::from_name("list-add-symbolic"))
                        .padding(6)
                        .on_press(Message::OpenAddAccount),
                    widget::text::caption("Add JMAP account"),
                    widget::tooltip::Position::Top,
                ))
                .push(widget::space::horizontal())
                .push(widget::button::text("Sign out").on_press(Message::SignOut));
            footer_col = footer_col.push(footer_actions);
        } else {
            footer_col = footer_col
                .push(
                    widget::container(
                        widget::text::caption("Sample mode")
                            .class(cosmic::theme::Text::Custom(muted_text_style)),
                    )
                    .padding([0, 10]),
                )
                .push(
                    widget::button::text("Sign in…")
                        .width(Length::Fill)
                        .on_press(Message::OpenAddAccount),
                );
        }

        let sidebar = widget::column::with_capacity(5)
            .push(header)
            .push(widget::divider::horizontal::default())
            .push(widget::scrollable(list).height(Length::Fill))
            .push(widget::divider::horizontal::default())
            .push(widget::container(footer_col).padding(8))
            .width(Length::Fixed(220.0))
            .height(Length::Fill);

        sidebar.into()
    }

    fn view_threads(&self) -> Element<'_, Message> {
        let active_mailbox = self
            .inbox
            .selected_mailbox
            .as_ref()
            .and_then(|id| self.inbox.mailboxes.iter().find(|mb| &mb.id == id));

        let header_title = active_mailbox
            .map(|mb| mb.name.as_str())
            .unwrap_or("Messages");
        let unread_count = self.inbox.threads.iter().filter(|t| t.unread).count();
        let unread_label = if unread_count > 0 {
            format!("Unread ({unread_count})")
        } else {
            "Unread".to_string()
        };

        let filter_row = widget::row::with_capacity(3)
            .spacing(4)
            .align_y(Alignment::Center)
            .push(
                widget::button::custom(widget::text::caption("All"))
                    .class(cosmic::theme::Button::ListItem([6.0; 4]))
                    .selected(self.inbox.filter == ThreadFilter::All)
                    .padding([4, 8])
                    .on_press(Message::SetFilter(ThreadFilter::All)),
            )
            .push(
                widget::button::custom(widget::text::caption(unread_label))
                    .class(cosmic::theme::Button::ListItem([6.0; 4]))
                    .selected(self.inbox.filter == ThreadFilter::Unread)
                    .padding([4, 8])
                    .on_press(Message::SetFilter(ThreadFilter::Unread)),
            )
            .push(widget::tooltip(
                widget::button::custom(widget::icon::from_name("insert-star-symbolic").size(14))
                    .class(cosmic::theme::Button::ListItem([6.0; 4]))
                    .selected(self.inbox.filter == ThreadFilter::Flagged)
                    .padding([5, 7])
                    .on_press(Message::SetFilter(ThreadFilter::Flagged)),
                widget::text::caption("Flagged"),
                widget::tooltip::Position::Bottom,
            ));

        let header = widget::container(
            widget::row::with_capacity(3)
                .push(widget::text::title4(header_title))
                .push(widget::space::horizontal())
                .push(filter_row)
                .align_y(Alignment::Center),
        )
        .padding([0, 14])
        .align_y(Alignment::Center)
        .height(Length::Fixed(52.0))
        .width(Length::Fill);

        let search_bar = widget::container(
            widget::search_input("Search mail…  (/)", &self.inbox.search_query)
                .id(search_input_id())
                .on_input(Message::SearchChanged)
                .on_clear(Message::ClearFilters)
                .width(Length::Fill),
        )
        .padding([8, 12, 4, 12])
        .width(Length::Fill);

        let now = Zoned::now();
        let visible_threads = self.inbox.visible_threads();
        let mut list = widget::column::with_capacity(visible_threads.len() * 2 + 1)
            .spacing(4)
            .padding([6, 10]);

        if self.inbox.loading_threads && self.inbox.threads.is_empty() {
            list = list.push(
                widget::container(
                    widget::text::body("Loading conversations…")
                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                )
                .padding(12),
            );
        } else if self.inbox.threads.is_empty() {
            list = list.push(
                widget::container(
                    widget::text::body("No conversations in this mailbox.")
                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                )
                .padding(12),
            );
        } else if visible_threads.is_empty() {
            let empty_msg = if !self.inbox.search_query.trim().is_empty() {
                format!("No matches for «{}».", self.inbox.search_query.trim())
            } else if self.inbox.filter == ThreadFilter::Flagged {
                "No flagged conversations.".to_string()
            } else {
                "No unread conversations.".to_string()
            };
            list = list.push(
                widget::container(
                    widget::column::with_capacity(2)
                        .spacing(10)
                        .align_x(Alignment::Center)
                        .push(
                            widget::text::body(empty_msg)
                                .class(cosmic::theme::Text::Custom(muted_text_style)),
                        )
                        .push(
                            widget::button::standard("Clear filters")
                                .on_press(Message::ClearFilters),
                        ),
                )
                .padding([24, 12])
                .center_x(Length::Fill),
            );
        } else {
            let thread_count = visible_threads.len();
            for (idx, thread) in visible_threads.iter().enumerate() {
                let selected = self.inbox.selected_thread.as_ref() == Some(&thread.id);
                let next_selected = visible_threads
                    .get(idx + 1)
                    .is_some_and(|next| self.inbox.selected_thread.as_ref() == Some(&next.id));
                let sender = thread
                    .senders
                    .first()
                    .map(|s| s.display_name())
                    .unwrap_or("Unknown");
                let sender_display = html::clean_snippet(sender, 28);
                let subject_display = html::clean_snippet(&thread.subject, 68);
                let snippet_display = html::clean_snippet(&thread.snippet, 96);
                let time_label = format_thread_time(thread.received_at, &now);

                let mut top_row = widget::row::with_capacity(6)
                    .spacing(6)
                    .align_y(Alignment::Center);

                if thread.unread {
                    top_row =
                        top_row.push(widget::text::caption("●").class(cosmic::theme::Text::Accent));
                }

                let sender_widget = if thread.unread {
                    widget::text::body(sender_display)
                        .font(cosmic::font::bold())
                        .class(cosmic::theme::Text::Custom(primary_text_style))
                } else {
                    widget::text::body(sender_display)
                        .class(cosmic::theme::Text::Custom(primary_text_style))
                };
                top_row = top_row.push(sender_widget);

                if thread.message_count > 1 {
                    top_row = top_row.push(
                        widget::text::caption(format!("({})", thread.message_count))
                            .class(cosmic::theme::Text::Custom(muted_text_style)),
                    );
                }

                top_row = top_row.push(widget::space::horizontal());

                if thread.flagged {
                    top_row =
                        top_row.push(widget::icon::from_name("insert-star-symbolic").size(14));
                }
                if thread.has_attachments {
                    top_row =
                        top_row.push(widget::icon::from_name("mail-attachment-symbolic").size(14));
                }

                top_row = top_row.push(widget::text::caption(time_label).class(if thread.unread {
                    cosmic::theme::Text::Accent
                } else {
                    cosmic::theme::Text::Custom(muted_text_style)
                }));

                let subject_widget = if thread.unread {
                    widget::text::body(subject_display)
                        .class(cosmic::theme::Text::Custom(primary_text_style))
                } else {
                    widget::text::body(subject_display)
                        .class(cosmic::theme::Text::Custom(secondary_text_style))
                };

                let mut content = widget::column::with_capacity(3)
                    .push(top_row)
                    .push(subject_widget)
                    .spacing(3);

                if !snippet_display.is_empty() {
                    content = content.push(
                        widget::text::caption(snippet_display)
                            .class(cosmic::theme::Text::Custom(muted_text_style)),
                    );
                }

                let btn = widget::button::custom(content)
                    .class(cosmic::theme::Button::ListItem([8.0; 4]))
                    .selected(selected)
                    .padding([10, 12])
                    .width(Length::Fill)
                    .on_press(Message::SelectThread(thread.id.clone()));

                list = list.push(btn);

                if idx + 1 < thread_count {
                    let sep: Element<'_, Message> = if !selected && !next_selected {
                        widget::container(widget::divider::horizontal::light())
                            .padding([0, 12])
                            .height(Length::Fixed(1.0))
                            .into()
                    } else {
                        widget::space::vertical().height(Length::Fixed(1.0)).into()
                    };
                    list = list.push(sep);
                }
            }
        }

        widget::column::with_capacity(4)
            .push(header)
            .push(widget::divider::horizontal::default())
            .push(search_bar)
            .push(widget::scrollable(list).height(Length::Fill))
            .width(Length::Fixed(370.0))
            .height(Length::Fill)
            .into()
    }

    fn view_conversation(&self) -> Element<'_, Message> {
        let selected_summary = self
            .inbox
            .selected_thread
            .as_ref()
            .and_then(|id| self.inbox.threads.iter().find(|t| &t.id == id));
        let selected_unread = selected_summary.map(|t| t.unread).unwrap_or(false);
        let selected_flagged = selected_summary.map(|t| t.flagged).unwrap_or(false);
        let active_role = self
            .inbox
            .selected_mailbox
            .as_ref()
            .and_then(|id| self.inbox.mailboxes.iter().find(|mb| &mb.id == id))
            .map(|mb| mb.role);

        if self.inbox.conversation.is_empty() && !self.inbox.loading_conversation {
            return widget::container(
                widget::text::body("Select a conversation to read.")
                    .class(cosmic::theme::Text::Custom(muted_text_style)),
            )
            .center(Length::Fill)
            .into();
        }

        let read_label = if selected_unread {
            "Mark read"
        } else {
            "Mark unread"
        };
        let read_icon = if selected_unread {
            "mail-mark-read-symbolic"
        } else {
            "mail-mark-unread-symbolic"
        };
        let flag_label = if selected_flagged { "Unflag" } else { "Flag" };

        let mut toolbar_row = widget::row::with_capacity(6)
            .spacing(8)
            .align_y(Alignment::Center);

        match active_role {
            Some(MailboxRole::Trash | MailboxRole::Junk) => {
                toolbar_row = toolbar_row.push(widget::tooltip(
                    widget::button::suggested("Restore")
                        .leading_icon(
                            widget::icon::from_name("mail-folder-inbox-symbolic").handle(),
                        )
                        .on_press(Message::RestoreToInbox),
                    widget::text::caption("Restore to Inbox"),
                    widget::tooltip::Position::Bottom,
                ));
            }
            Some(MailboxRole::Archive) => {
                toolbar_row = toolbar_row.push(widget::tooltip(
                    widget::button::standard("Move to Inbox")
                        .leading_icon(
                            widget::icon::from_name("mail-folder-inbox-symbolic").handle(),
                        )
                        .on_press(Message::RestoreToInbox),
                    widget::text::caption("Move back to Inbox"),
                    widget::tooltip::Position::Bottom,
                ));
            }
            _ => {
                toolbar_row = toolbar_row.push(widget::tooltip(
                    widget::button::standard("Archive")
                        .leading_icon(widget::icon::from_name("mail-archive-symbolic").handle())
                        .on_press(Message::Archive),
                    widget::text::caption("Archive (E)"),
                    widget::tooltip::Position::Bottom,
                ));
            }
        }

        toolbar_row = toolbar_row
            .push(widget::tooltip(
                widget::button::standard(read_label)
                    .leading_icon(widget::icon::from_name(read_icon).handle())
                    .on_press(Message::ToggleRead),
                widget::text::caption("Toggle Read/Unread (U)"),
                widget::tooltip::Position::Bottom,
            ))
            .push(widget::tooltip(
                widget::button::standard(flag_label)
                    .leading_icon(widget::icon::from_name("insert-star-symbolic").handle())
                    .on_press(Message::ToggleFlag),
                widget::text::caption("Flag / Unflag (S)"),
                widget::tooltip::Position::Bottom,
            ))
            .push(widget::space::horizontal());

        if active_role != Some(MailboxRole::Trash) {
            toolbar_row = toolbar_row.push(widget::tooltip(
                widget::button::standard("Trash")
                    .leading_icon(widget::icon::from_name("user-trash-symbolic").handle())
                    .on_press(Message::Trash),
                widget::text::caption("Move to Trash (Del)"),
                widget::tooltip::Position::Bottom,
            ));
        }

        let toolbar = widget::container(toolbar_row)
            .padding([0, 20])
            .align_y(Alignment::Center)
            .height(Length::Fixed(52.0))
            .width(Length::Fill);

        if self.inbox.conversation.is_empty() {
            let preview_body: Element<'_, Message> = if let Some(summary) = selected_summary {
                let subject = if summary.subject.trim().is_empty() {
                    "(No subject)"
                } else {
                    summary.subject.as_str()
                };
                let primary_sender = summary.senders.first();
                let sender_name = primary_sender
                    .map(|s| s.display_name())
                    .unwrap_or("Unknown");
                let initials = sender_initials(sender_name);
                let timestamp = format_full_time(summary.received_at);
                let snippet = html::clean_snippet(&summary.snippet, 240);

                let avatar = widget::container(
                    widget::text::caption(initials)
                        .class(cosmic::theme::Text::Custom(primary_text_style)),
                )
                .center(Length::Fixed(32.0))
                .class(cosmic::theme::Container::custom(avatar_container_style));

                let header_row = widget::row::with_capacity(2)
                    .push(avatar)
                    .push(
                        widget::row::with_capacity(3)
                            .push(
                                widget::text::heading(sender_name)
                                    .class(cosmic::theme::Text::Custom(primary_text_style)),
                            )
                            .push(widget::space::horizontal())
                            .push(
                                widget::text::caption(timestamp)
                                    .class(cosmic::theme::Text::Custom(muted_text_style)),
                            )
                            .spacing(12)
                            .align_y(Alignment::Center),
                    )
                    .spacing(12)
                    .align_y(Alignment::Center);

                let mut card = widget::column::with_capacity(4)
                    .spacing(12)
                    .push(header_row)
                    .push(widget::divider::horizontal::light());

                if !snippet.is_empty() {
                    card = card.push(
                        widget::text::body(snippet)
                            .class(cosmic::theme::Text::Custom(secondary_text_style)),
                    );
                }
                card = card.push(
                    widget::text::caption("Loading full message…")
                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                );

                widget::column::with_capacity(2)
                    .spacing(14)
                    .padding(20)
                    .max_width(840.0)
                    .push(widget::text::title3(subject))
                    .push(
                        widget::container(card)
                            .padding(16)
                            .class(cosmic::theme::Container::Card)
                            .width(Length::Fill),
                    )
                    .into()
            } else {
                widget::container(
                    widget::text::body("Loading conversation…")
                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                )
                .center(Length::Fill)
                .into()
            };

            return widget::column::with_capacity(3)
                .push(toolbar)
                .push(widget::divider::horizontal::default())
                .push(
                    widget::scrollable(preview_body)
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }

        let thread_subject = self
            .inbox
            .conversation
            .first()
            .map(|m| m.subject.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("(No subject)");

        let mut col = widget::column::with_capacity(self.inbox.conversation.len() + 1)
            .spacing(14)
            .padding(20)
            .max_width(840.0)
            .push(widget::text::title3(thread_subject));

        for msg in &self.inbox.conversation {
            let primary_sender = msg.from.first();
            let sender_name = primary_sender
                .map(|a| a.display_name())
                .unwrap_or("Unknown");
            let initials = sender_initials(sender_name);
            let to = format_address_list(&msg.to);
            let cc = format_address_list(&msg.cc);
            let timestamp = format_full_time(msg.received_at);
            let prepared = html::prepare_body(msg.text_body.as_deref(), msg.html_body.as_deref());
            let has_remote = msg.has_remote_images || prepared.analysis.has_remote_images;

            let avatar = widget::container(
                widget::text::caption(initials)
                    .class(cosmic::theme::Text::Custom(primary_text_style)),
            )
            .center(Length::Fixed(32.0))
            .class(cosmic::theme::Container::custom(avatar_container_style));

            let mut meta_col = widget::column::with_capacity(3).spacing(2).push(
                widget::row::with_capacity(3)
                    .push(
                        widget::text::heading(sender_name)
                            .class(cosmic::theme::Text::Custom(primary_text_style)),
                    )
                    .push(widget::space::horizontal())
                    .push(
                        widget::text::caption(timestamp)
                            .class(cosmic::theme::Text::Custom(muted_text_style)),
                    )
                    .spacing(12)
                    .align_y(Alignment::Center),
            );

            let from_subtitle = if msg.from.len() == 1 {
                primary_sender
                    .map(|a| a.email.clone())
                    .filter(|email| !email.eq_ignore_ascii_case(sender_name))
                    .unwrap_or_default()
            } else {
                format_address_list(&msg.from)
            };
            if !from_subtitle.is_empty() {
                meta_col = meta_col.push(
                    widget::text::caption(from_subtitle)
                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                );
            }

            let mut recipient_parts = Vec::new();
            if !to.is_empty() {
                recipient_parts.push(format!("To: {to}"));
            }
            if !cc.is_empty() {
                recipient_parts.push(format!("Cc: {cc}"));
            }
            if !recipient_parts.is_empty() {
                meta_col = meta_col.push(
                    widget::text::caption(recipient_parts.join("  ·  "))
                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                );
            }

            let header_row = widget::row::with_capacity(2)
                .push(avatar)
                .push(meta_col)
                .spacing(12)
                .align_y(Alignment::Start);

            let mut card = widget::column::with_capacity(8)
                .spacing(12)
                .push(header_row)
                .push(widget::divider::horizontal::light());

            if has_remote {
                let policy = self.inbox.remote_image_policy(&msg.id);
                let banner_content: Element<'_, Message> = if policy == RemoteImagePolicy::Block {
                    widget::row::with_capacity(3)
                        .push(widget::icon::from_name("image-x-generic-symbolic").size(16))
                        .push(
                            widget::text::caption("External images are blocked for privacy.")
                                .class(cosmic::theme::Text::Custom(muted_text_style)),
                        )
                        .push(widget::space::horizontal())
                        .push(
                            widget::button::text("Load images")
                                .on_press(Message::AllowRemoteImages(msg.id.clone())),
                        )
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                } else {
                    widget::text::caption("External images enabled for this message.")
                        .class(cosmic::theme::Text::Custom(muted_text_style))
                        .into()
                };

                card = card.push(
                    widget::container(banner_content)
                        .padding([6, 10])
                        .width(Length::Fill)
                        .class(cosmic::theme::Container::Primary),
                );
            }

            let body = if prepared.main_text.is_empty() {
                "(Empty message body)".to_string()
            } else {
                prepared.main_text
            };
            card = card.push(widget::text::body(body));

            if let Some(quoted) = prepared.quoted_text {
                let expanded = self.inbox.expanded_quotes.contains(&msg.id);
                let toggle_label = if expanded {
                    "Hide quoted text"
                } else {
                    "⋯ Show quoted text"
                };
                card = card.push(
                    widget::button::text(toggle_label)
                        .on_press(Message::ToggleQuote(msg.id.clone())),
                );
                if expanded {
                    card = card.push(
                        widget::text::caption(quoted)
                            .class(cosmic::theme::Text::Custom(muted_text_style)),
                    );
                }
            }

            let regular_attachments: Vec<_> =
                msg.attachments.iter().filter(|a| !a.is_inline).collect();
            if !regular_attachments.is_empty() {
                let mut chips: Vec<Element<'_, Message>> =
                    Vec::with_capacity(regular_attachments.len());
                for att in regular_attachments {
                    let name = html::clean_snippet(att.name.as_deref().unwrap_or("attachment"), 36);
                    let downloading = self.downloading_blobs.contains(&att.blob_id);
                    let size_label = if downloading {
                        "Downloading…".to_string()
                    } else {
                        format_file_size(att.size)
                    };

                    let chip_content = widget::row::with_capacity(3)
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .push(
                            widget::icon::from_name(attachment_icon_name(&att.mime_type)).size(16),
                        )
                        .push(
                            widget::column::with_capacity(2)
                                .spacing(1)
                                .push(
                                    widget::text::body(name)
                                        .class(cosmic::theme::Text::Custom(primary_text_style)),
                                )
                                .push(
                                    widget::text::caption(size_label)
                                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                                ),
                        )
                        .push(widget::icon::from_name("document-save-symbolic").size(16));

                    let mut btn = widget::button::custom(chip_content)
                        .class(cosmic::theme::Button::ListItem([8.0; 4]))
                        .padding([8, 12]);
                    if !downloading {
                        btn = btn.on_press(Message::DownloadAttachment(att.clone()));
                    }

                    chips.push(
                        widget::tooltip(
                            widget::container(btn).class(cosmic::theme::Container::Primary),
                            widget::text::caption("Save to Downloads"),
                            widget::tooltip::Position::Bottom,
                        )
                        .into(),
                    );
                }

                card = card
                    .push(widget::divider::horizontal::light())
                    .push(widget::flex_row(chips).row_spacing(8).column_spacing(8));
            }

            col = col.push(
                widget::container(card)
                    .padding(16)
                    .class(cosmic::theme::Container::Card)
                    .width(Length::Fill),
            );
        }

        widget::column::with_capacity(3)
            .push(toolbar)
            .push(widget::divider::horizontal::default())
            .push(
                widget::scrollable(col)
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

fn persist_sync_states(cache: &VelaCache, states: &SyncStates) {
    if let Some(s) = &states.mailbox_state {
        let _ = cache.set_sync_state("mailbox_state", s);
    }
    if let Some(s) = &states.email_state {
        let _ = cache.set_sync_state("email_state", s);
    }
    if let Some(s) = &states.thread_state {
        let _ = cache.set_sync_state("thread_state", s);
    }
}

fn spawn_account_load_tasks(account_id: String) -> Task<Message> {
    let warm_id = account_id.clone();
    let warm_cache_task = cosmic::task::future(async move {
        let Ok(key) = store::load_or_create_cache_key_for(&warm_id).await else {
            return Message::CacheWarmed(None);
        };
        let snap = tokio::task::spawn_blocking(move || {
            let cache = Arc::new(VelaCache::open_for_account(&warm_id, &key).ok()?);
            let mailboxes = cache.load_mailboxes().ok().unwrap_or_default();
            let initial_mb = mailboxes
                .iter()
                .find(|m| m.role == MailboxRole::Inbox)
                .or_else(|| mailboxes.first())
                .map(|m| m.id.clone());
            let threads = initial_mb
                .as_ref()
                .and_then(|id| cache.load_threads(id).ok())
                .unwrap_or_default();
            let conversation = threads.first().and_then(|t| {
                cache
                    .load_conversation(&t.id)
                    .ok()
                    .flatten()
                    .map(|msgs| (t.id.clone(), msgs))
            });
            let sync_states = SyncStates {
                mailbox_state: cache.get_sync_state("mailbox_state").ok().flatten(),
                email_state: cache.get_sync_state("email_state").ok().flatten(),
                thread_state: cache.get_sync_state("thread_state").ok().flatten(),
            };
            Some(Box::new(CachedSnapshot {
                account_id: warm_id,
                cache,
                mailboxes,
                mailbox_id: initial_mb,
                threads,
                conversation,
                sync_states,
            }))
        })
        .await
        .ok()
        .flatten();
        Message::CacheWarmed(snap)
    });

    let connect_id = account_id;
    let connect_task = cosmic::task::future(async move {
        let result = async {
            let Some(config) = store::load_jmap_config_for(&connect_id).await? else {
                return Ok(None);
            };
            let session = Arc::new(JmapSession::connect(&config).await?);
            let (mailboxes, mb_state) = session.list_mailboxes_with_state().await?;
            Ok::<_, vela_core::VelaError>(Some((connect_id, session, mailboxes, mb_state)))
        }
        .await
        .map_err(|e| e.to_string());
        Message::StartupResolved(result)
    });

    Task::batch([warm_cache_task, connect_task])
}

fn search_input_id() -> widget::Id {
    widget::Id::new("vela-thread-search")
}

fn mailbox_icon_name(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "mail-folder-inbox-symbolic",
        MailboxRole::Drafts => "edit-symbolic",
        MailboxRole::Sent => "mail-send-symbolic",
        MailboxRole::Archive => "mail-archive-symbolic",
        MailboxRole::Junk => "mail-mark-junk-symbolic",
        MailboxRole::Trash => "user-trash-symbolic",
        MailboxRole::Custom => "folder-symbolic",
    }
}

fn format_address(addr: &EmailAddress) -> String {
    match addr.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() && !name.eq_ignore_ascii_case(&addr.email) => {
            format!("{name} <{}>", addr.email)
        }
        _ => addr.email.clone(),
    }
}

fn format_address_list(addrs: &[EmailAddress]) -> String {
    addrs
        .iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_thread_time(ts: Timestamp, now: &Zoned) -> String {
    let local = ts.to_zoned(TimeZone::system());
    let today = now.date();
    let msg_date = local.date();

    if msg_date == today {
        local.strftime("%H:%M").to_string()
    } else if msg_date.tomorrow().ok() == Some(today) {
        "Yesterday".to_string()
    } else if msg_date.year() == today.year() {
        local.strftime("%b %-d").to_string()
    } else {
        local.strftime("%b %-d, %Y").to_string()
    }
}

fn format_full_time(ts: Timestamp) -> String {
    let local = ts.to_zoned(TimeZone::system());
    local.strftime("%b %-d, %Y · %H:%M").to_string()
}

fn primary_text_style(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    let color: Color = theme.cosmic().on_bg_color().into();
    cosmic::iced::widget::text::Style {
        color: Some(color),
        ..Default::default()
    }
}

fn secondary_text_style(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    let mut color: Color = theme.cosmic().on_bg_color().into();
    color.a = 0.82;
    cosmic::iced::widget::text::Style {
        color: Some(color),
        ..Default::default()
    }
}

fn muted_text_style(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    let mut color: Color = theme.cosmic().on_bg_color().into();
    color.a = 0.60;
    cosmic::iced::widget::text::Style {
        color: Some(color),
        ..Default::default()
    }
}

fn sender_initials(name: &str) -> String {
    let parts: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    match parts.as_slice() {
        [] => "?".to_string(),
        [only] => only
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string()),
        [first, .., last] => {
            let a = first.chars().next().unwrap_or('?');
            let b = last.chars().next().unwrap_or('?');
            format!("{}{}", a.to_uppercase(), b.to_uppercase())
        }
    }
}

fn avatar_container_style(theme: &cosmic::Theme) -> widget::container::Style {
    let cosmic = theme.cosmic();
    let mut bg: Color = cosmic.accent.base.into();
    bg.a = 0.22;
    widget::container::Style {
        background: Some(bg.into()),
        border: cosmic::iced::Border {
            radius: 16.0.into(),
            width: 1.0,
            color: cosmic.accent.base.into(),
        },
        ..Default::default()
    }
}

fn attachment_icon_name(mime: &str) -> &'static str {
    let lower = mime.to_ascii_lowercase();
    if lower.starts_with("image/") {
        "image-x-generic-symbolic"
    } else if lower.starts_with("text/") {
        "text-x-generic-symbolic"
    } else {
        "mail-attachment-symbolic"
    }
}

fn format_file_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{} KB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn save_to_downloads(raw_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let base_dir = dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
        .ok_or_else(|| "unable to determine Downloads directory".to_string())?;
    std::fs::create_dir_all(&base_dir).map_err(|e| e.to_string())?;

    // Sanitize filename to prevent path traversal from untrusted headers.
    let safe_name: String = raw_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("attachment")
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | '\0'))
        .collect();
    let safe_name = safe_name.trim_matches('.');
    let safe_name = if safe_name.is_empty() {
        "attachment"
    } else {
        safe_name
    };

    let candidate = base_dir.join(safe_name);
    let dest = if !candidate.exists() {
        candidate
    } else {
        let stem = candidate
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("attachment");
        let ext = candidate.extension().and_then(|e| e.to_str());
        let mut chosen = None;
        for n in 1..1000 {
            let numbered = match ext {
                Some(ext) => format!("{stem} ({n}).{ext}"),
                None => format!("{stem} ({n})"),
            };
            let path = base_dir.join(numbered);
            if !path.exists() {
                chosen = Some(path);
                break;
            }
        }
        chosen.ok_or_else(|| "too many duplicate attachment files".to_string())?
    };

    std::fs::write(&dest, bytes).map_err(|e| e.to_string())?;
    Ok(dest)
}

fn shortcut_section<'a>(title: &'a str, entries: &[(&'a str, &'a str)]) -> Element<'a, Message> {
    let mut col = widget::column::with_capacity(entries.len() + 1)
        .spacing(6)
        .push(
            widget::text::caption_heading(title)
                .class(cosmic::theme::Text::Custom(muted_text_style)),
        );

    for &(keys, desc) in entries {
        let keycap = widget::container(
            widget::text::caption(keys).class(cosmic::theme::Text::Custom(primary_text_style)),
        )
        .padding([3, 8])
        .class(cosmic::theme::Container::Primary);

        let row = widget::row::with_capacity(3)
            .push(widget::text::body(desc))
            .push(widget::space::horizontal())
            .push(keycap)
            .align_y(Alignment::Center)
            .spacing(16);
        col = col.push(row);
    }

    col.into()
}
