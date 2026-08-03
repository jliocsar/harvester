//! Top-level UI state and layout (docs/SPEC.md §UI, slice-4 cut): sidebar
//! (machine + sessions), content (chat or empty state), statusbar. Owns the
//! bridge: commands out synchronously, events in on a gpui task awaiting the
//! bridge's channel.

use gpui::{div, prelude::*, px, rgb, Context, Entity, SharedString, Window};
use harvester_proto::{SessionInfo, SessionStatus};
use tokio::sync::mpsc::UnboundedSender;

use crate::bridge::{Bridge, Command, Event as BridgeEvent};
use crate::chat::Chat;
use crate::theme;
use crate::ui;

const SIDEBAR_WIDTH: f32 = 240.;
/// Enough of a uuidv7 to tell sessions apart at a glance.
const SID_PREVIEW_CHARS: usize = 8;

/// Connection lifecycle as the statusbar tells it.
enum Link {
    Connecting,
    Connected { daemon_version: String },
    Failed { detail: String },
    Lost,
}

pub struct Workbench {
    dest: String,
    link: Link,
    sessions: Vec<SessionInfo>,
    /// Sid we asked the bridge to open, until `Attached` promotes it to a
    /// chat (also the "don't double-open" guard).
    opening: Option<String>,
    chat: Option<Entity<Chat>>,
    /// Last request failure; sticks in the statusbar until the next one.
    last_error: Option<String>,
    commands: UnboundedSender<Command>,
}

impl Workbench {
    pub fn new(dest: String, bridge: Bridge, cx: &mut Context<Self>) -> Self {
        let Bridge {
            commands,
            mut events,
        } = bridge;

        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |workbench, cx| workbench.handle_bridge_event(event, cx))
                    .is_err()
                {
                    // Workbench dropped — window closed; stop pumping.
                    break;
                }
            }
        })
        .detach();

        Self {
            dest,
            link: Link::Connecting,
            sessions: Vec::new(),
            opening: None,
            chat: None,
            last_error: None,
            commands,
        }
    }

    fn handle_bridge_event(&mut self, event: BridgeEvent, cx: &mut Context<Self>) {
        match event {
            BridgeEvent::Connected { daemon_version } => {
                self.link = Link::Connected { daemon_version };
            }
            BridgeEvent::ConnectFailed { detail } => self.link = Link::Failed { detail },
            BridgeEvent::Sessions(mut sessions) => {
                // Newest first; uuidv7 sids sort chronologically, so the sid
                // itself is the created-at key.
                sessions.sort_by(|a, b| b.sid.cmp(&a.sid));
                self.sessions = sessions;
            }
            BridgeEvent::Attached { sid, entries } => {
                // The daemon's list may predate a resume we just did; disk on
                // the VM stays authoritative, this is display state only.
                self.set_session_status(&sid, SessionStatus::Running);
                self.opening = None;
                let commands = self.commands.clone();
                self.chat = Some(cx.new(|cx| Chat::new(sid, &entries, commands, cx)));
            }
            BridgeEvent::Pi { sid, event } => {
                if let Some(chat) = self.chat_for(&sid, cx) {
                    chat.update(cx, |chat, cx| chat.handle_pi_event(&event, cx));
                }
            }
            BridgeEvent::SessionExit { sid, exit_code } => {
                self.set_session_status(&sid, SessionStatus::Exited);

                if let Some(chat) = self.chat_for(&sid, cx) {
                    chat.update(cx, |chat, cx| chat.handle_session_exit(exit_code, cx));
                }
            }
            BridgeEvent::RequestFailed { detail } => {
                self.opening = None;
                self.last_error = Some(detail);
            }
            BridgeEvent::Disconnected => self.link = Link::Lost,
        }

        cx.notify();
    }

    fn chat_for(&self, sid: &str, cx: &Context<Self>) -> Option<Entity<Chat>> {
        self.chat
            .as_ref()
            .filter(|chat| chat.read(cx).sid() == sid)
            .cloned()
    }

    fn set_session_status(&mut self, sid: &str, status: SessionStatus) {
        for session in &mut self.sessions {
            if session.sid == sid {
                session.status = status;
            }
        }
    }

    fn open_session(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.opening.is_some() {
            return;
        }

        let Some(session) = self.sessions.get(index) else {
            return;
        };
        let already_open = self
            .chat_for(&session.sid, cx)
            .is_some_and(|_| session.status == SessionStatus::Running);

        if already_open {
            return;
        }

        self.opening = Some(session.sid.clone());

        if self
            .commands
            .send(Command::Open {
                sid: session.sid.clone(),
                cwd: session.cwd.clone(),
                running: session.status == SessionStatus::Running,
            })
            .is_err()
        {
            // Bridge gone; the Disconnected event drives the visible state.
            self.opening = None;
        }

        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(theme::PANEL_BG))
            .border_r_1()
            .border_color(rgb(theme::BORDER))
            .child(ui::section_header("machine"))
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .flex()
                    .flex_col()
                    .child(div().child(self.dest.clone()))
                    .child(self.render_link_line()),
            )
            .child(ui::section_header("sessions"))
            .child(
                div()
                    .id("sessions")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(
                        self.sessions
                            .iter()
                            .enumerate()
                            .map(|(index, session)| self.render_session_row(index, session, cx)),
                    )
                    .when(self.sessions.is_empty(), |sidebar| {
                        sidebar.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_FAINT))
                                .child("no sessions yet"),
                        )
                    }),
            )
    }

    fn render_link_line(&self) -> impl IntoElement {
        let (text, color) = match &self.link {
            Link::Connecting => ("connecting…".to_string(), theme::TEXT_FAINT),
            Link::Connected { daemon_version } => {
                (format!("harvesterd {daemon_version}"), theme::TEXT_FAINT)
            }
            Link::Failed { .. } => ("connect failed".to_string(), theme::STATUS_ERROR),
            Link::Lost => ("connection lost".to_string(), theme::STATUS_ERROR),
        };

        div().text_xs().text_color(rgb(color)).child(text)
    }

    fn render_session_row(
        &self,
        index: usize,
        session: &SessionInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self
            .chat
            .as_ref()
            .is_some_and(|chat| chat.read(cx).sid() == session.sid);
        let opening = self.opening.as_deref() == Some(session.sid.as_str());
        let dot_color = match session.status {
            SessionStatus::Running => theme::STATUS_RUNNING,
            SessionStatus::Exited => theme::TEXT_FAINT,
        };
        let title: SharedString = match &session.name {
            Some(name) => name.clone().into(),
            None => sid_preview(&session.sid).into(),
        };
        let workspace_name: SharedString = session
            .cwd
            .rsplit('/')
            .next()
            .unwrap_or(session.cwd.as_str())
            .to_string()
            .into();

        ui::session_row(
            ("session", index),
            title,
            workspace_name,
            dot_color,
            selected,
            opening,
        )
        .on_click(cx.listener(move |workbench, _, _, cx| workbench.open_session(index, cx)))
    }

    fn render_content(&self) -> impl IntoElement {
        let content = div().flex_1().min_w(px(0.)).h_full();

        if let Some(chat) = &self.chat {
            return content.child(chat.clone());
        }

        let (hint, color) = match &self.link {
            Link::Connecting => (format!("connecting to {}…", self.dest), theme::TEXT_FAINT),
            Link::Connected { .. } => ("select a session".to_string(), theme::TEXT_FAINT),
            Link::Failed { detail } => (detail.clone(), theme::STATUS_ERROR),
            Link::Lost => ("connection lost".to_string(), theme::STATUS_ERROR),
        };

        content.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(rgb(color))
                .child(hint),
        )
    }

    fn render_statusbar(&self) -> impl IntoElement {
        let link_text = match &self.link {
            Link::Connecting => "connecting…".to_string(),
            Link::Connected { daemon_version } => {
                format!("{} · harvesterd {daemon_version}", self.dest)
            }
            Link::Failed { .. } => "connect failed".to_string(),
            Link::Lost => "connection lost".to_string(),
        };
        let link_color = match &self.link {
            Link::Failed { .. } | Link::Lost => theme::STATUS_ERROR,
            _ => theme::TEXT_FAINT,
        };

        ui::statusbar_frame()
            .child(div().text_color(rgb(link_color)).child(link_text))
            .child(div().flex_1())
            .children(self.last_error.clone().map(|error| {
                div()
                    .text_color(rgb(theme::STATUS_ERROR))
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(error)
            }))
    }
}

impl Render for Workbench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::WINDOW_BG))
            .text_color(rgb(theme::TEXT))
            .text_size(px(13.))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(self.render_sidebar(cx))
                    .child(self.render_content()),
            )
            .child(self.render_statusbar())
    }
}

fn sid_preview(sid: &str) -> String {
    sid.chars().take(SID_PREVIEW_CHARS).collect()
}
