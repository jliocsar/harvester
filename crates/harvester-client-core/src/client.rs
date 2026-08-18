use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use harvester_proto::{HarvesterCmd, HarvesterResponse, Outgoing, Request};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::connection::{DaemonConnection, DaemonWriter, HANDSHAKE_PING_ID};

#[derive(Debug)]
pub enum SessionEvent {
    Pi { sid: String, event: Value },
    SessionExit { sid: String, exit_code: Option<i32> },
    EventGap { sid: String, skipped: u64 },
}

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    writer: AsyncMutex<DaemonWriter>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Outgoing>>>,
    last_request_id: AtomicU64,
}

impl DaemonClient {
    pub fn start(connection: DaemonConnection) -> (DaemonClient, UnboundedReceiver<SessionEvent>) {
        let DaemonConnection { mut reader, writer } = connection;
        let (event_sender, event_receiver) = unbounded_channel();
        let client = DaemonClient {
            inner: Arc::new(ClientInner {
                writer: AsyncMutex::new(writer),
                pending: Mutex::new(HashMap::new()),
                last_request_id: AtomicU64::new(HANDSHAKE_PING_ID),
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
        });

        (client, event_receiver)
    }

    pub async fn harvester(&self, cmd: HarvesterCmd) -> std::io::Result<HarvesterResponse> {
        match self.request(|id| Request::Harvester { id, cmd }).await? {
            Outgoing::HarvesterResponse { response, .. } => Ok(response),
            mismatched => Err(envelope_mismatch(&mismatched)),
        }
    }

    pub async fn pi(&self, sid: &str, cmd: Value) -> std::io::Result<Value> {
        let sid = sid.to_string();

        match self.request(|id| Request::Pi { id, sid, cmd }).await? {
            Outgoing::PiResponse { response, .. } => Ok(response),
            mismatched => Err(envelope_mismatch(&mismatched)),
        }
    }

    async fn request(
        &self,
        build: impl FnOnce(Option<Value>) -> Request,
    ) -> std::io::Result<Outgoing> {
        let id = self.inner.last_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = oneshot::channel();
        let _pending = Pending::register(self, id, sender);
        let request = build(Some(json!(id)));

        self.inner.writer.lock().await.send(&request).await?;

        receiver.await.map_err(|_closed| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "connection closed before the response",
            )
        })
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
            response @ (Outgoing::HarvesterResponse { .. } | Outgoing::PiResponse { .. }) => {
                self.resolve(response);
            }
        }
    }

    fn resolve(&self, response: Outgoing) {
        let id = match &response {
            Outgoing::HarvesterResponse { id, .. } | Outgoing::PiResponse { id, .. } => id.as_ref(),
            _ => None,
        };
        let Some(id) = id.and_then(Value::as_u64) else {
            eprintln!("daemon client: response without a usable id");

            return;
        };

        let Some(waiter) = self.lock_pending().remove(&id) else {
            eprintln!("daemon client: dropped response for request {id}");

            return;
        };

        let _ = waiter.send(response);
    }

    fn fail_all_pending(&self) {
        self.lock_pending().clear();
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Outgoing>>> {
        self.inner.pending.lock().expect("pending mutex")
    }
}

struct Pending {
    client: DaemonClient,
    id: u64,
}

impl Pending {
    fn register(client: &DaemonClient, id: u64, sender: oneshot::Sender<Outgoing>) -> Pending {
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

fn envelope_mismatch(response: &Outgoing) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("response arrived in the wrong envelope: {response:?}"),
    )
}
