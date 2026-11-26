// SPDX-License-Identifier: MIT

//! Vela COSMIC desktop shell.
//!
//! Keeps all state transitions in `vela-flows` and runs all JMAP network I/O
//! asynchronously off the UI thread so scrolling and keyboard triage stay smooth.

mod sample;

use std::sync::Arc;

use cosmic::app::{Core, Settings, Task};
use cosmic::iced::core::layout::Limits;
use cosmic::iced::{Event, Length, Size, Subscription, event, keyboard};
use cosmic::{Element, executor, widget};
use vela_core::{
    Mailbox, MailboxId, Message as MailMessage, MessageId, RemoteImagePolicy, ThreadId,
    ThreadSummary,
};
use vela_flows::{InboxEffect, InboxEvent, InboxState, TriageAction, html};
use vela_jmap::{JmapConfig, JmapSession, ThreadMutation};

const WINDOW_SIZE: Size = Size::new(1280.0, 760.0);
const WINDOW_MIN: Size = Size::new(960.0, 600.0);
const INITIAL_THREAD_LIMIT: usize = 50;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vela=info")),
        )
        .init();

    let settings = Settings::default().size(WINDOW_SIZE).size_limits(
        Limits::NONE
            .min_width(WINDOW_MIN.width)
            .min_height(WINDOW_MIN.height),
    );
    cosmic::app::run_single_instance::<App>(settings, Flags)?;
    Ok(())
}

#[derive(Clone, Debug)]
struct Flags;

impl cosmic::app::CosmicFlags for Flags {
    type SubCommand = String;
    type Args = Vec<String>;
}

struct App {
    core: Core,
    inbox: InboxState,
    session: Option<Arc<JmapSession>>,
}

#[derive(Clone, Debug)]
enum Message {
    Connected(Result<(Arc<JmapSession>, Vec<Mailbox>), String>),
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

    fn init(core: Core, _flags: Self::Flags) -> (Self, Task<Self::Message>) {
        let mut app = Self {
            core,
            inbox: InboxState::new(),
            session: None,
        };

        match JmapConfig::from_env() {
            Ok(Some(config)) => {
                app.inbox.status_banner = Some(format!("Connecting to {}…", config.server_url));
                let task = cosmic::task::future(async move {
                    let result = async {
                        let session = Arc::new(JmapSession::connect(&config).await?);
                        let mailboxes = session.list_mailboxes().await?;
                        Ok::<_, vela_core::VelaError>((session, mailboxes))
                    }
                    .await
                    .map_err(|e| e.to_string());
                    Message::Connected(result)
                });
                (app, task)
            }
            Ok(None) => {
                let task = app.dispatch(InboxEvent::MailboxesLoaded(sample::mailboxes()));
                (app, task)
            }
            Err(err) => {
                app.inbox.status_banner =
                    Some(format!("Config error: {err} (showing sample data)"));
                let task = app.dispatch(InboxEvent::MailboxesLoaded(sample::mailboxes()));
                (app, task)
            }
        }
    }

    fn subscription(&self) -> Subscription<Self::Message> {
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
            Message::Connected(Ok((session, mailboxes))) => {
                self.session = Some(session);
                self.inbox.status_banner = None;
                self.dispatch(InboxEvent::MailboxesLoaded(mailboxes))
            }
            Message::Connected(Err(err)) => self.dispatch(InboxEvent::ReportError(format!(
                "JMAP connect failed: {err}"
            ))),
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
                .align_y(cosmic::iced::Alignment::Center)
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
}

impl App {
    fn view_mailboxes(&self) -> Element<'_, Message> {
        let mut list = widget::column::with_capacity(self.inbox.mailboxes.len() + 1)
            .spacing(4)
            .padding(12);
        list = list.push(widget::text::title4("Mailboxes"));

        for mb in &self.inbox.mailboxes {
            let selected = self.inbox.selected_mailbox.as_ref() == Some(&mb.id);
            let label = if mb.unread_threads > 0 {
                format!("{} ({})", mb.name, mb.unread_threads)
            } else {
                mb.name.clone()
            };
            let btn = if selected {
                widget::button::suggested(label)
            } else {
                widget::button::text(label)
            }
            .width(Length::Fill)
            .on_press(Message::SelectMailbox(mb.id.clone()));
            list = list.push(btn);
        }

        widget::scrollable(list)
            .width(Length::Fixed(210.0))
            .height(Length::Fill)
            .into()
    }

    fn view_threads(&self) -> Element<'_, Message> {
        let mut list = widget::column::with_capacity(self.inbox.threads.len() + 1)
            .spacing(6)
            .padding(12);

        if self.inbox.loading_threads {
            list = list.push(widget::text::body("Loading conversations…"));
        } else if self.inbox.threads.is_empty() {
            list = list.push(widget::text::body("No conversations in this mailbox."));
        } else {
            for thread in &self.inbox.threads {
                let selected = self.inbox.selected_thread.as_ref() == Some(&thread.id);
                let sender = thread
                    .senders
                    .first()
                    .map(|s| s.display_name())
                    .unwrap_or("Unknown");
                let prefix = if thread.unread { "● " } else { "" };

                let content = widget::column::with_capacity(3)
                    .push(widget::text::body(format!(
                        "{prefix}{sender} ({})",
                        thread.message_count
                    )))
                    .push(widget::text::heading(&thread.subject))
                    .push(widget::text::caption(&thread.snippet))
                    .spacing(2);

                let btn = if selected {
                    widget::button::custom(content).class(cosmic::theme::Button::Suggested)
                } else {
                    widget::button::custom(content).class(cosmic::theme::Button::Text)
                }
                .padding(10)
                .width(Length::Fill)
                .on_press(Message::SelectThread(thread.id.clone()));

                list = list.push(btn);
            }
        }

        widget::scrollable(list)
            .width(Length::Fixed(360.0))
            .height(Length::Fill)
            .into()
    }

    fn view_conversation(&self) -> Element<'_, Message> {
        let mut col = widget::column::with_capacity(self.inbox.conversation.len() + 2)
            .spacing(16)
            .padding(20);

        if self.inbox.loading_conversation && self.inbox.conversation.is_empty() {
            col = col.push(widget::text::body("Loading conversation…"));
        } else if self.inbox.conversation.is_empty() {
            col = col.push(widget::text::body("Select a conversation to read."));
        } else {
            let toolbar = widget::row::with_capacity(3)
                .push(widget::button::standard("Archive (E)").on_press(Message::Archive))
                .push(widget::button::standard("Trash (Del)").on_press(Message::Trash))
                .push(widget::button::standard("Read/Unread (U)").on_press(Message::ToggleRead))
                .spacing(8);
            col = col.push(toolbar);

            for msg in &self.inbox.conversation {
                let from = msg
                    .from
                    .iter()
                    .map(|a| a.display_name())
                    .collect::<Vec<_>>()
                    .join(", ");
                let prepared =
                    html::prepare_body(msg.text_body.as_deref(), msg.html_body.as_deref());
                let has_remote = msg.has_remote_images || prepared.analysis.has_remote_images;

                let mut card = widget::column::with_capacity(7)
                    .spacing(8)
                    .push(widget::text::title4(&msg.subject))
                    .push(widget::text::caption(format!("From: {from}")));

                if prepared.analysis.is_rich_layout {
                    card = card.push(widget::text::caption(
                        "HTML newsletter detected (rich layout renderer pending Blitz integration)",
                    ));
                }

                if has_remote {
                    let policy = self.inbox.remote_image_policy(&msg.id);
                    if policy == RemoteImagePolicy::Block {
                        card = card.push(
                            widget::row::with_capacity(2)
                                .push(widget::text::caption("External images are blocked."))
                                .push(
                                    widget::button::text("Load external images")
                                        .on_press(Message::AllowRemoteImages(msg.id.clone())),
                                )
                                .spacing(8)
                                .align_y(cosmic::iced::Alignment::Center),
                        );
                    } else {
                        card = card.push(widget::text::caption(
                            "External images enabled for this message.",
                        ));
                    }
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
                        card = card.push(widget::text::caption(quoted));
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
                    card = card.push(widget::text::caption(format!("Attachments: {summary}")));
                }

                col = col.push(
                    widget::container(card)
                        .padding(14)
                        .class(cosmic::theme::Container::Card)
                        .width(Length::Fill),
                );
            }
        }

        widget::scrollable(col)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}
