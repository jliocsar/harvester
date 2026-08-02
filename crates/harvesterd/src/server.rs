//! The controller layer: JSONL framing, decode, route, encode. Command logic
//! lives in `ops.rs` (harvester) and `passthrough.rs` (pi).

use harvester_proto::{HarvesterResponse, Outgoing, Request, CR, LF};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::daemon::Daemon;
use crate::ops::{self, AttachedMap};
use crate::passthrough;

/// Outgoing lines queued per connection before backpressure kicks in.
const OUTGOING_QUEUE_DEPTH: usize = 256;

pub async fn serve_connection(daemon: Daemon, stream: UnixStream) {
    let (read_half, write_half) = stream.into_split();

    // Single writer task: everything outgoing is serialized through one channel
    // so response lines and forwarded events never interleave mid-line.
    let (out_tx, out_rx) = mpsc::channel::<Outgoing>(OUTGOING_QUEUE_DEPTH);
    let writer = spawn_writer(write_half, out_rx);

    let mut attached: AttachedMap = AttachedMap::new();
    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();

    loop {
        buf.clear();

        match reader.read_line(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let Some(request) = decode_line(&buf, &out_tx).await else {
            continue;
        };

        let outgoing = handle_request(&daemon, request, &out_tx, &mut attached).await;
        let _ = out_tx.send(outgoing).await;
    }

    cleanup_connection(&mut attached);
    writer.abort();
}

fn spawn_writer(
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut out_rx: mpsc::Receiver<Outgoing>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(outgoing) = out_rx.recv().await {
            let mut line = serde_json::to_vec(&outgoing).expect("serializable");
            line.push(LF);

            if write_half.write_all(&line).await.is_err() {
                break;
            }
        }
    })
}

/// Strip framing, parse one request. `None` means "nothing to dispatch"
/// (blank line, or a parse error already reported to the client).
async fn decode_line(buf: &str, out_tx: &mpsc::Sender<Outgoing>) -> Option<Request> {
    let line = buf
        .trim_end_matches(LF as char)
        .trim_end_matches(CR as char);

    if line.is_empty() {
        return None;
    }

    match serde_json::from_str(line) {
        Ok(request) => Some(request),
        Err(error) => {
            let outgoing = Outgoing::HarvesterResponse {
                id: None,
                response: HarvesterResponse::err(format!("parse error: {error}")),
            };
            let _ = out_tx.send(outgoing).await;

            None
        }
    }
}

async fn handle_request(
    daemon: &Daemon,
    request: Request,
    out_tx: &mpsc::Sender<Outgoing>,
    attached: &mut AttachedMap,
) -> Outgoing {
    match request {
        Request::Harvester { id, cmd } => {
            let response = ops::dispatch(daemon, cmd, out_tx, attached).await;

            Outgoing::HarvesterResponse { id, response }
        }
        Request::Pi { id, sid, cmd } => passthrough::pi_passthrough(daemon, id, sid, cmd).await,
    }
}

/// Connection gone: detach everywhere so the empty-room dialog policy kicks in.
fn cleanup_connection(attached: &mut AttachedMap) {
    for (_sid, attachment) in attached.drain() {
        ops::release_attachment(attachment);
    }
}
