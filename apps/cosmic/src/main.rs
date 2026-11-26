// SPDX-License-Identifier: MIT

//! Vela COSMIC desktop shell.
//!
//! Keeps all state transitions in `vela-flows` and runs all JMAP network I/O
//! asynchronously off the UI thread so scrolling and keyboard triage stay smooth.
//! Credentials are stored in the OS Keyring (`org.freedesktop.secrets`) with
//! non-secret account metadata in `~/.config/vela/config.toml` (`0600`).

mod sample;

use std::sync::Arc;

use cosmic::app::{Core, Settings, Task};
use cosmic::iced::core::layout::Limits;
use cosmic::iced::{Alignment, Color, Event, Length, Size, Subscription, event, keyboard};
use cosmic::{Element, executor, widget};
use jiff::{Timestamp, Zoned, tz::TimeZone};
use vela_core::{
    EmailAddress, Mailbox, MailboxId, MailboxRole, Message as MailMessage, MessageId,
    RemoteImagePolicy, ThreadId, ThreadSummary,
};
use vela_flows::{InboxEffect, InboxEvent, InboxState, TriageAction, html};
use vela_jmap::{JmapConfig, JmapCredentials, JmapSession, ThreadMutation, store};
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
}

type ConnectedAccount = (Arc<JmapSession>, Vec<Mailbox>);

#[derive(Clone, Debug)]
enum Message {
    StartupResolved(Result<Option<ConnectedAccount>, String>),
    ServerUrlChanged(String),
    UsernameChanged(String),
    PasswordChanged(String),
    SubmitConnect,
    ConnectFinished(Result<ConnectedAccount, String>),
    OpenSampleMode,
    SignOut,
    SignedOut,
    SelectMailbox(MailboxId),
    ThreadsLoaded {
        op_id: vela_flows::OpId,
        mailbox_id: MailboxId,
        result: Result<Vec<ThreadSummary>, String>,
    },
    SelectThread(ThreadId),
    SelectNext,
    SelectPrev,
    ConversationLoaded {
        op_id: vela_flows::OpId,
        thread_id: ThreadId,
        result: Result<Vec<MailMessage>, String>,
    },
    Archive,
    Trash,
    ToggleRead,
    Undo,
    TriageFinished(Result<(), String>),
    AllowRemoteImages(MessageId),
    ToggleQuote(MessageId),
}

impl App {
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
                if let Some(session) = self.session.clone() {
                    let mb = mailbox_id.clone();
                    cosmic::task::future(async move {
                        let result = session
                            .list_threads(&mb, INITIAL_THREAD_LIMIT)
                            .await
                            .map_err(|e| e.to_string());
                        Message::ThreadsLoaded {
                            op_id,
                            mailbox_id: mb,
                            result,
                        }
                    })
                } else {
                    let threads = sample::threads_for_mailbox(&mailbox_id);
                    self.dispatch(InboxEvent::ThreadsLoaded {
                        op_id,
                        mailbox_id,
                        threads,
                    })
                }
            }
            InboxEffect::LoadConversation { op_id, thread_id } => {
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
                } else {
                    let messages = sample::messages_for_thread(&thread_id);
                    self.dispatch(InboxEvent::ConversationLoaded {
                        op_id,
                        thread_id,
                        messages,
                    })
                }
            }
            InboxEffect::ApplyTriage {
                thread_id,
                action,
                target_mailbox,
                ..
            } => {
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mutation = match action {
                    TriageAction::Archive | TriageAction::Trash => match target_mailbox {
                        Some(target) => ThreadMutation::MoveToMailbox(target),
                        None => return Task::none(),
                    },
                    TriageAction::ToggleRead { mark_unread } => {
                        ThreadMutation::SetUnread(mark_unread)
                    }
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
        let mut app = Self {
            core,
            screen: Screen::Starting,
            inbox: InboxState::new(),
            session: None,
        };

        if flags.sample_mode {
            app.screen = Screen::Inbox;
            let task = app.dispatch(InboxEvent::MailboxesLoaded(sample::mailboxes()));
            return (app, task);
        }

        let task = cosmic::task::future(async move {
            let result = async {
                let Some(config) = store::load_saved_jmap_config().await? else {
                    return Ok(None);
                };
                let session = Arc::new(JmapSession::connect(&config).await?);
                let mailboxes = session.list_mailboxes().await?;
                Ok::<_, vela_core::VelaError>(Some((session, mailboxes)))
            }
            .await
            .map_err(|e| e.to_string());
            Message::StartupResolved(result)
        });

        (app, task)
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
                keyboard::Key::Character(ref c) => match c.as_str() {
                    "j" => Some(Message::SelectNext),
                    "k" => Some(Message::SelectPrev),
                    "e" => Some(Message::Archive),
                    "u" => Some(Message::ToggleRead),
                    "z" => Some(Message::Undo),
                    _ => None,
                },
                _ => None,
            }
        })
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::StartupResolved(Ok(Some((session, mailboxes)))) => {
                self.session = Some(session);
                self.screen = Screen::Inbox;
                self.dispatch(InboxEvent::MailboxesLoaded(mailboxes))
            }
            Message::StartupResolved(Ok(None)) => {
                self.screen = Screen::Connect(ConnectForm::from_saved_or_default());
                Task::none()
            }
            Message::StartupResolved(Err(err)) => {
                let mut form = ConnectForm::from_saved_or_default();
                form.error = Some(err);
                self.screen = Screen::Connect(form);
                Task::none()
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
                        let mailboxes = session.list_mailboxes().await?;
                        store::save_jmap_config(&config).await?;
                        Ok::<_, vela_core::VelaError>((session, mailboxes))
                    }
                    .await
                    .map_err(|e| e.to_string());
                    Message::ConnectFinished(result)
                })
            }
            Message::ConnectFinished(Ok((session, mailboxes))) => {
                if let Screen::Connect(form) = &mut self.screen {
                    form.wipe_password();
                }
                self.session = Some(session);
                self.inbox = InboxState::new();
                self.screen = Screen::Inbox;
                self.dispatch(InboxEvent::MailboxesLoaded(mailboxes))
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
                self.inbox = InboxState::new();
                self.screen = Screen::Inbox;
                self.dispatch(InboxEvent::MailboxesLoaded(sample::mailboxes()))
            }
            Message::SignOut => {
                self.session = None;
                self.inbox = InboxState::new();
                cosmic::task::future(async move {
                    let _ = store::clear_saved_secret().await;
                    Message::SignedOut
                })
            }
            Message::SignedOut => {
                self.screen = Screen::Connect(ConnectForm::from_saved_or_default());
                Task::none()
            }
            Message::SelectMailbox(id) => self.dispatch(InboxEvent::SelectMailbox(id)),
            Message::ThreadsLoaded {
                op_id,
                mailbox_id,
                result,
            } => match result {
                Ok(threads) => self.dispatch(InboxEvent::ThreadsLoaded {
                    op_id,
                    mailbox_id,
                    threads,
                }),
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
                Ok(messages) => self.dispatch(InboxEvent::ConversationLoaded {
                    op_id,
                    thread_id,
                    messages,
                }),
                Err(err) => self.dispatch(InboxEvent::ReportError(err)),
            },
            Message::Archive => self.dispatch(InboxEvent::ArchiveSelected),
            Message::Trash => self.dispatch(InboxEvent::TrashSelected),
            Message::ToggleRead => self.dispatch(InboxEvent::ToggleSelectedRead),
            Message::Undo => self.dispatch(InboxEvent::UndoLastTriage),
            Message::TriageFinished(Ok(())) => Task::none(),
            Message::TriageFinished(Err(err)) => self.dispatch(InboxEvent::ReportError(err)),
            Message::AllowRemoteImages(id) => self.dispatch(InboxEvent::AllowRemoteImages(id)),
            Message::ToggleQuote(id) => self.dispatch(InboxEvent::ToggleQuote(id)),
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

        let actions = widget::row::with_capacity(2)
            .push(connect_btn)
            .push(widget::button::text("Preview sample data").on_press(Message::OpenSampleMode))
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
            }
            let status_bar = widget::container(row).padding([8, 16]).width(Length::Fill);
            root = root
                .push(widget::divider::horizontal::default())
                .push(status_bar);
        }

        root.into()
    }

    fn view_mailboxes(&self) -> Element<'_, Message> {
        let header = widget::container(widget::text::title4("Mailboxes"))
            .padding([0, 16])
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

        let footer_label = if self.session.is_some() {
            "Sign out"
        } else {
            "Sign in…"
        };
        let mut footer_col = widget::column::with_capacity(2).spacing(2);
        if self.session.is_none() {
            footer_col = footer_col.push(
                widget::container(
                    widget::text::caption("Sample mode")
                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                )
                .padding([0, 10]),
            );
        }
        footer_col = footer_col.push(
            widget::button::text(footer_label)
                .width(Length::Fill)
                .on_press(Message::SignOut),
        );

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
        let header_meta = match active_mailbox {
            Some(mb) if mb.unread_threads > 0 => format!("{} unread", mb.unread_threads),
            Some(_) => format!("{}", self.inbox.threads.len()),
            None => String::new(),
        };

        let header = widget::container(
            widget::row::with_capacity(3)
                .push(widget::text::title4(header_title))
                .push(widget::space::horizontal())
                .push(
                    widget::text::caption(header_meta)
                        .class(cosmic::theme::Text::Custom(muted_text_style)),
                )
                .align_y(Alignment::Center),
        )
        .padding([0, 16])
        .align_y(Alignment::Center)
        .height(Length::Fixed(52.0))
        .width(Length::Fill);

        let now = Zoned::now();
        let mut list = widget::column::with_capacity(self.inbox.threads.len() + 1)
            .spacing(4)
            .padding([8, 10]);

        if self.inbox.loading_threads {
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
        } else {
            let thread_count = self.inbox.threads.len();
            for (idx, thread) in self.inbox.threads.iter().enumerate() {
                let selected = self.inbox.selected_thread.as_ref() == Some(&thread.id);
                let next_selected = self
                    .inbox
                    .threads
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

        widget::column::with_capacity(3)
            .push(header)
            .push(widget::divider::horizontal::default())
            .push(widget::scrollable(list).height(Length::Fill))
            .width(Length::Fixed(370.0))
            .height(Length::Fill)
            .into()
    }

    fn view_conversation(&self) -> Element<'_, Message> {
        let selected_unread = self
            .inbox
            .selected_thread
            .as_ref()
            .and_then(|id| self.inbox.threads.iter().find(|t| &t.id == id))
            .map(|t| t.unread)
            .unwrap_or(false);

        if self.inbox.loading_conversation && self.inbox.conversation.is_empty() {
            return widget::container(
                widget::text::body("Loading conversation…")
                    .class(cosmic::theme::Text::Custom(muted_text_style)),
            )
            .center(Length::Fill)
            .into();
        }

        if self.inbox.conversation.is_empty() {
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

        let toolbar = widget::container(
            widget::row::with_capacity(4)
                .push(widget::tooltip(
                    widget::button::standard("Archive")
                        .leading_icon(widget::icon::from_name("mail-archive-symbolic").handle())
                        .on_press(Message::Archive),
                    widget::text::caption("Archive (E)"),
                    widget::tooltip::Position::Bottom,
                ))
                .push(widget::tooltip(
                    widget::button::standard(read_label)
                        .leading_icon(widget::icon::from_name(read_icon).handle())
                        .on_press(Message::ToggleRead),
                    widget::text::caption("Toggle Read/Unread (U)"),
                    widget::tooltip::Position::Bottom,
                ))
                .push(widget::space::horizontal())
                .push(widget::tooltip(
                    widget::button::standard("Trash")
                        .leading_icon(widget::icon::from_name("user-trash-symbolic").handle())
                        .on_press(Message::Trash),
                    widget::text::caption("Move to Trash (Del)"),
                    widget::tooltip::Position::Bottom,
                ))
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .padding([0, 20])
        .align_y(Alignment::Center)
        .height(Length::Fixed(52.0))
        .width(Length::Fill);

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
                let summary = regular_attachments
                    .iter()
                    .map(|a| {
                        let name = a.name.as_deref().unwrap_or("attachment");
                        format!("{name} ({} KB)", (a.size / 1024).max(1))
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                card = card.push(widget::divider::horizontal::light()).push(
                    widget::row::with_capacity(2)
                        .push(widget::icon::from_name("mail-attachment-symbolic").size(16))
                        .push(
                            widget::text::caption(summary)
                                .class(cosmic::theme::Text::Custom(muted_text_style)),
                        )
                        .spacing(8)
                        .align_y(Alignment::Center),
                );
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
