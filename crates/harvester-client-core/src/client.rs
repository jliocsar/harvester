//! Request/response correlation for one daemon connection (docs/DAEMON.md:
//! requests carry an `id`, the response echoes it, arrival order is free).
//! Requests resolve at their `await` point via a pending map — the same shape
//! pi's own RPC client uses — so every command flow reads as straight-line
//! async/await instead of a hand-rolled continuation state machine.
//! Unsolicited lines (events, session_exit) flow out on their own channel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use harvester_proto::{HarvesterCmd, HarvesterResponse, Outgoing, Request};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::connection::{DaemonConnection, DaemonWriter};

/// An unsolicited daemon line: something happened to a session we watch.
#[derive(Debug)]
pub enum SessionEvent {
    /// One live pi event for an attached session, verbatim.
    Pi {
        sid: String,
        event: Value,
    },
    SessionExit {
        sid: String,
        exit_code: Option<i32>,
    },
    /// The daemon dropped events for this connection (we read too slowly).
    /// Whatever we hold for `sid` is now incomplete and must be refetched.
    EventGap {
        sid: String,
        skipped: u64,
    },
    /// The connection died (EOF or read error); no more lines will arrive
    /// and every in-flight request has been failed.
    Disconnected,
}

/// A response resolved against the wrong envelope would be a daemon bug;
/// typed so the two request kinds cannot cross.
enum CorrelatedResponse {
    Harvester(HarvesterResponse),
    Pi(Value),
}

/// Cheap-clone handle: every command flow (spawned task) holds one.
#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    /// Async mutex: the guard is held across the socket write.
    writer: AsyncMutex<DaemonWriter>,
    /// In-flight waiters by request id. std::sync — never held across an await.
    pending: Mutex<HashMap<u64, oneshot::Sender<CorrelatedResponse>>>,
    /// 0 was the handshake ping; ids start at 1.
    last_request_id: AtomicU64,
}

impl DaemonClient {
    /// Split `connection` into a routing task (reader) and a client handle
    /// (writer + pending map). Dropping the client and the event receiver
    /// tears the connection (and any ssh forward) down.
    pub fn start(connection: DaemonConnection) -> (DaemonClient, UnboundedReceiver<SessionEvent>) {
        let DaemonConnection { mut reader, writer } = connection;
        let (event_sender, event_receiver) = unbounded_channel();
        let client = DaemonClient {
            inner: Arc::new(ClientInner {
                writer: AsyncMutex::new(writer),
                pending: Mutex::new(HashMap::new()),
                last_request_id: AtomicU64::new(0),
            }),
        };

        let routing_client = client.clone();

        tokio::spawn(async move {
            loop {
                match reader.next().await {
                    Ok(Some(line)) => routing_client.route_line(line, &event_sender),
                    Ok(None) => break,
                    Err(error) => {
                        eprintln!("daemon client: read failed: {error}");

                        break;
                    }
                }
            }

            routing_client.fail_all_pending();
            let _ = event_sender.send(SessionEvent::Disconnected);
        });

        (client, event_receiver)
    }

    /// One daemon-native command, response at the await point.
    pub async fn harvester(&self, cmd: HarvesterCmd) -> std::io::Result<HarvesterResponse> {
        match self.request(|id| Request::Harvester { id, cmd }).await? {
            CorrelatedResponse::Harvester(response) => Ok(response),
            CorrelatedResponse::Pi(_) => Err(envelope_mismatch()),
        }
    }

    /// One pi passthrough command, pi's response object verbatim.
    pub async fn pi(&self, sid: &str, cmd: Value) -> std::io::Result<Value> {
        let sid = sid.to_string();

        match self.request(|id| Request::Pi { id, sid, cmd }).await? {
            CorrelatedResponse::Pi(response) => Ok(response),
            CorrelatedResponse::Harvester(_) => Err(envelope_mismatch()),
        }
    }

    async fn request(
        &self,
        build: impl FnOnce(Option<Value>) -> Request,
    ) -> std::io::Result<CorrelatedResponse> {
        let id = self.inner.last_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = oneshot::channel();
        let pending = Pending::register(self, id, sender);
        let request = build(Some(json!(id)));

        self.inner.writer.lock().await.send(&request).await?;

        let response = receiver.await.map_err(|_closed| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "connection closed before the response",
            )
        });
        // The waiter is resolved (or gone) either way; releasing it here is
        // only tidiness — the guard's Drop covers every path, including the
        // cancelled-future one that has no code at all.
        drop(pending);

        response
    }

    fn route_line(
        &self,
        line: Outgoing,
        events: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    ) {
        match line {
            Outgoing::Event { sid, event } => {
                let _ = events.send(SessionEvent::Pi { sid, event });
            }
            Outgoing::SessionExit { sid, exit_code } => {
                let _ = events.send(SessionEvent::SessionExit { sid, exit_code });
            }
            Outgoing::EventGap { sid, skipped } => {
                let _ = events.send(SessionEvent::EventGap { sid, skipped });
            }
            Outgoing::HarvesterResponse { id, response } => {
                self.resolve(id.as_ref(), CorrelatedResponse::Harvester(response));
            }
            Outgoing::PiResponse { id, response, .. } => {
                self.resolve(id.as_ref(), CorrelatedResponse::Pi(response));
            }
        }
    }

    fn resolve(&self, id: Option<&Value>, response: CorrelatedResponse) {
        let Some(id) = id.and_then(Value::as_u64) else {
            eprintln!("daemon client: response without a usable id");

            return;
        };

        let Some(waiter) = self.lock_pending().remove(&id) else {
            // The requester gave up (its task died); noteworthy, not fatal.
            eprintln!("daemon client: dropped response for request {id}");

            return;
        };

        let _ = waiter.send(response);
    }

    /// Dropping the senders errors every waiter's receiver — the same
    /// "connection closed" path as a failed send.
    fn fail_all_pending(&self) {
        self.lock_pending().clear();
    }

    fn lock_pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<CorrelatedResponse>>> {
        self.inner.pending.lock().expect("pending mutex")
    }
}

/// Owns one entry in the pending map for as long as the request is in
/// flight. RAII because the ways out are more numerous than the ways
/// through: a failed write, a cancelled future (the caller's task dropped
/// mid-await — the UI does this whenever a flow is abandoned), or the normal
/// resolution. A leaked entry is a waiter that can never be woken and a
/// `HashMap` that only grows, so the removal cannot live at a call site.
struct Pending {
    client: DaemonClient,
    id: u64,
}

impl Pending {
    fn register(
        client: &DaemonClient,
        id: u64,
        sender: oneshot::Sender<CorrelatedResponse>,
    ) -> Pending {
        client.lock_pending().insert(id, sender);

        Pending {
            client: client.clone(),
            id,
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.client.lock_pending().remove(&self.id);
    }
}

fn envelope_mismatch() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "response arrived in the wrong envelope",
    )
}
