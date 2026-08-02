//! The tokio ↔ gpui bridge: one background thread runs a current-thread tokio
//! runtime that owns the daemon connection (harvester-client-core is tokio;
//! gpui's executor has no tokio reactor). The UI talks to it over tokio mpsc
//! channels, which are reactor-independent and therefore awaitable straight
//! from gpui tasks.

use std::collections::HashMap;

use harvester_client_core::{bootstrap_and_connect, DaemonConnection, DaemonWriter, SshTarget};
use harvester_proto::{HarvesterCmd, Outgoing, Request, SessionInfo};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// UI → bridge.
#[derive(Debug)]
pub enum Command {
    /// Open a session: attach if running, resume (`create_session`, same sid)
    /// then attach if exited. Ends in `Event::Attached` or a failure event.
    Open {
        sid: String,
        cwd: String,
        running: bool,
    },
    Prompt {
        sid: String,
        text: String,
    },
}

/// bridge → UI. Pi payloads stay verbatim `Value` — the client speaks Pi.
#[derive(Debug)]
pub enum Event {
    Connected {
        daemon_version: String,
    },
    ConnectFailed {
        detail: String,
    },
    Sessions(Vec<SessionInfo>),
    /// Attach completed and history was replayed via passthrough
    /// `get_entries`; `entries` is pi's list, verbatim.
    Attached {
        sid: String,
        entries: Vec<Value>,
    },
    /// One live pi event for an attached session, verbatim.
    Pi {
        sid: String,
        event: Value,
    },
    SessionExit {
        sid: String,
        exit_code: Option<i32>,
    },
    /// A request failed daemon- or pi-side. Statusbar material; the
    /// connection itself is still healthy.
    RequestFailed {
        detail: String,
    },
    Disconnected,
}

pub struct Bridge {
    pub commands: UnboundedSender<Command>,
    pub events: UnboundedReceiver<Event>,
}

/// What an in-flight request id was for; the follow-up on its response
/// depends on it (create chains into attach chains into get_entries).
enum Pending {
    ListSessions,
    Create { sid: String },
    Attach { sid: String },
    Detach,
    Entries { sid: String },
    Prompt,
}

pub fn start(dest: String, daemon_binary: Vec<u8>) -> std::io::Result<Bridge> {
    let (command_sender, command_receiver) = unbounded_channel();
    let (event_sender, event_receiver) = unbounded_channel();

    std::thread::Builder::new()
        .name("harvester-bridge".to_string())
        .spawn(move || run_bridge_thread(dest, daemon_binary, command_receiver, event_sender))?;

    Ok(Bridge {
        commands: command_sender,
        events: event_receiver,
    })
}

fn run_bridge_thread(
    dest: String,
    daemon_binary: Vec<u8>,
    commands: UnboundedReceiver<Command>,
    events: UnboundedSender<Event>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(Event::ConnectFailed {
                detail: format!("tokio runtime: {error}"),
            });

            return;
        }
    };

    runtime.block_on(bridge_main(dest, daemon_binary, commands, events));
}

async fn bridge_main(
    dest: String,
    daemon_binary: Vec<u8>,
    mut commands: UnboundedReceiver<Command>,
    events: UnboundedSender<Event>,
) {
    let target = SshTarget { dest, port: None };
    let connected = match bootstrap_and_connect(&target, &daemon_binary).await {
        Ok(connected) => connected,
        Err(error) => {
            let _ = events.send(Event::ConnectFailed {
                detail: error.to_string(),
            });

            return;
        }
    };
    let _ = events.send(Event::Connected {
        daemon_version: connected.daemon_version,
    });

    let DaemonConnection { mut reader, writer } = connected.connection;
    let mut link = Link {
        writer,
        events,
        pending: HashMap::new(),
        last_request_id: 0, // 0 was the handshake ping; ids start at 1
        attached: None,
    };

    // Reads get their own task: `select!` cancels the losing future, and a
    // cancelled `reader.next()` would drop a partially-read line. Channel
    // recvs below are cancel-safe. EOF drops the sender, closing the channel.
    let (line_sender, mut lines) = unbounded_channel::<std::io::Result<Outgoing>>();
    tokio::spawn(async move {
        loop {
            match reader.next().await {
                Ok(Some(outgoing)) => {
                    if line_sender.send(Ok(outgoing)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = line_sender.send(Err(error));

                    break;
                }
            }
        }
    });

    // The sidebar renders from this; fetch immediately.
    link.send_harvester(
        HarvesterCmd::ListSessions { cwd: None },
        Pending::ListSessions,
    )
    .await;

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                // UI dropped the bridge; dropping the connection tears down
                // the ssh forward.
                None => break,
                Some(command) => handle_ui_command(command, &mut link).await,
            },
            line = lines.recv() => match line {
                Some(Ok(outgoing)) => handle_daemon_line(outgoing, &mut link).await,
                Some(Err(error)) => {
                    eprintln!("harvester: read from daemon: {error}");
                    let _ = link.events.send(Event::Disconnected);

                    break;
                }
                None => {
                    let _ = link.events.send(Event::Disconnected);

                    break;
                }
            },
        }
    }
}

async fn handle_ui_command(command: Command, link: &mut Link) {
    match command {
        Command::Open { sid, cwd, running } => {
            // One chat pane means one attachment: leaving the previous
            // session keeps the daemon from streaming events nobody renders.
            if let Some(previous_sid) = link.attached.take() {
                if previous_sid != sid {
                    link.send_harvester(
                        HarvesterCmd::Detach { sid: previous_sid },
                        Pending::Detach,
                    )
                    .await;
                }
            }

            if running {
                link.send_harvester(
                    HarvesterCmd::Attach { sid: sid.clone() },
                    Pending::Attach { sid },
                )
                .await;
            } else {
                // Resume = create with the same sid (DAEMON.md). Follow-up
                // requests queue on pi's stdin, so no readiness probe needed.
                link.send_harvester(
                    HarvesterCmd::CreateSession {
                        sid: sid.clone(),
                        cwd,
                        provider: None,
                        model: None,
                        name: None,
                    },
                    Pending::Create { sid },
                )
                .await;
            }
        }
        Command::Prompt { sid, text } => {
            link.send_pi(
                &sid,
                json!({"type": "prompt", "message": text}),
                Pending::Prompt,
            )
            .await;
        }
    }
}

async fn handle_daemon_line(outgoing: Outgoing, link: &mut Link) {
    match outgoing {
        Outgoing::Event { sid, event } => {
            let _ = link.events.send(Event::Pi { sid, event });
        }
        Outgoing::SessionExit { sid, exit_code } => {
            let _ = link.events.send(Event::SessionExit { sid, exit_code });
        }
        Outgoing::HarvesterResponse { id, response } => {
            let Some(purpose) = link.take_pending(id.as_ref()) else {
                return;
            };

            match purpose {
                Pending::ListSessions => {
                    if response.success {
                        link.publish_sessions(response.data.as_ref());
                    } else {
                        link.report_failure("list_sessions", response.error);
                    }
                }
                Pending::Create { sid } => {
                    if response.success {
                        link.send_harvester(
                            HarvesterCmd::Attach { sid: sid.clone() },
                            Pending::Attach { sid },
                        )
                        .await;
                    } else {
                        link.report_failure("resume session", response.error);
                    }
                }
                Pending::Attach { sid } => {
                    if response.success {
                        link.attached = Some(sid.clone());
                        // History replay: attach → get_entries → live events
                        // (DAEMON.md §attach). No `since`: full history.
                        link.send_pi(
                            &sid.clone(),
                            json!({"type": "get_entries"}),
                            Pending::Entries { sid },
                        )
                        .await;
                    } else {
                        link.report_failure("attach", response.error);
                    }
                }
                Pending::Detach => {
                    if !response.success {
                        link.report_failure("detach", response.error);
                    }
                }
                Pending::Entries { .. } | Pending::Prompt => {
                    eprintln!("harvester: pi-bound request answered by daemon envelope");
                }
            }
        }
        Outgoing::PiResponse { id, response, .. } => {
            let Some(purpose) = link.take_pending(id.as_ref()) else {
                return;
            };
            let success = response
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            match purpose {
                Pending::Entries { sid } => {
                    if success {
                        let entries = response
                            .get("data")
                            .and_then(|data| data.get("entries"))
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let _ = link.events.send(Event::Attached { sid, entries });
                    } else {
                        link.report_failure("get_entries", pi_error(&response));
                    }
                }
                Pending::Prompt => {
                    // Accepted means queued, not completed; progress arrives
                    // as events. Only rejection is notable.
                    if !success {
                        link.report_failure("prompt", pi_error(&response));
                    }
                }
                Pending::ListSessions
                | Pending::Create { .. }
                | Pending::Attach { .. }
                | Pending::Detach => {
                    eprintln!("harvester: daemon-bound request answered by pi envelope");
                }
            }
        }
    }
}

fn pi_error(response: &Value) -> Option<String> {
    response
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
}

struct Link {
    writer: DaemonWriter,
    events: UnboundedSender<Event>,
    pending: HashMap<u64, Pending>,
    last_request_id: u64,
    /// The one session this client is attached to (single chat pane); the
    /// previous attachment is detached when a new one opens.
    attached: Option<String>,
}

impl Link {
    async fn send_harvester(&mut self, cmd: HarvesterCmd, purpose: Pending) {
        let id = self.next_request_id();
        self.pending.insert(id, purpose);
        let request = Request::Harvester {
            id: Some(json!(id)),
            cmd,
        };
        self.write(&request, id).await;
    }

    async fn send_pi(&mut self, sid: &str, cmd: Value, purpose: Pending) {
        let id = self.next_request_id();
        self.pending.insert(id, purpose);
        let request = Request::Pi {
            id: Some(json!(id)),
            sid: sid.to_string(),
            cmd,
        };
        self.write(&request, id).await;
    }

    async fn write(&mut self, request: &Request, id: u64) {
        if let Err(error) = self.writer.send(request).await {
            self.pending.remove(&id);
            let _ = self.events.send(Event::RequestFailed {
                detail: format!("send to daemon: {error}"),
            });
        }
    }

    fn next_request_id(&mut self) -> u64 {
        self.last_request_id += 1;

        self.last_request_id
    }

    fn take_pending(&mut self, id: Option<&Value>) -> Option<Pending> {
        self.pending.remove(&id?.as_u64()?)
    }

    fn publish_sessions(&self, data: Option<&Value>) {
        let sessions = data
            .and_then(|data| data.get("sessions"))
            .and_then(|sessions| serde_json::from_value::<Vec<SessionInfo>>(sessions.clone()).ok());

        match sessions {
            Some(sessions) => {
                let _ = self.events.send(Event::Sessions(sessions));
            }
            None => {
                let _ = self.events.send(Event::RequestFailed {
                    detail: "malformed list_sessions response".to_string(),
                });
            }
        }
    }

    fn report_failure(&self, context: &str, error: Option<String>) {
        let detail = format!(
            "{context}: {}",
            error.unwrap_or_else(|| "unknown error".to_string())
        );
        let _ = self.events.send(Event::RequestFailed { detail });
    }
}
