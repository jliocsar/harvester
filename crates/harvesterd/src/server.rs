use std::future::Future;
use std::sync::Arc;

use harvester_proto::{
    read_frame, HarvesterResponse, Outgoing, Request, FRAME_BUFFER_START_BYTES, LF,
};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::daemon::Daemon;
use crate::ops::{self, AttachedMap};
use crate::passthrough;

const OUTGOING_QUEUE_DEPTH: usize = 256;

pub enum Outbound {
    Typed(Outgoing),
    Raw(Arc<[u8]>),
}

pub async fn serve_connection(daemon: Daemon, stream: UnixStream) {
    let (read_half, write_half) = stream.into_split();

    let (out_tx, out_rx) = mpsc::channel::<Outbound>(OUTGOING_QUEUE_DEPTH);
    let writer = spawn_writer(write_half, out_rx);

    let mut attached: AttachedMap = AttachedMap::new();
    let mut reader = BufReader::new(read_half);
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let line = match read_frame(&mut reader, &mut buf).await {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };

        let Some(request) = decode_line(line, &out_tx).await else {
            continue;
        };

        if let Some(outgoing) = handle_request(&daemon, request, &out_tx, &mut attached) {
            let _ = out_tx.send(Outbound::Typed(outgoing)).await;
        }
    }

    attached.clear();
    drop(out_tx);
    let _ = writer.await;
}

fn spawn_writer(
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut out_rx: mpsc::Receiver<Outbound>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut line: Vec<u8> = Vec::with_capacity(FRAME_BUFFER_START_BYTES);

        while let Some(outbound) = out_rx.recv().await {
            line.clear();

            match &outbound {
                Outbound::Typed(outgoing) => {
                    serde_json::to_writer(&mut line, outgoing).expect("serializable");
                }
                Outbound::Raw(bytes) => line.extend_from_slice(bytes),
            }

            line.push(LF);

            if write_half.write_all(&line).await.is_err() {
                break;
            }
        }
    })
}

pub fn answer_later(
    out_tx: &mpsc::Sender<Outbound>,
    answer: impl Future<Output = Outgoing> + Send + 'static,
) {
    let out_tx = out_tx.clone();

    tokio::spawn(async move {
        let _ = out_tx.send(Outbound::Typed(answer.await)).await;
    });
}

async fn decode_line(line: &[u8], out_tx: &mpsc::Sender<Outbound>) -> Option<Request> {
    if line.is_empty() {
        return None;
    }

    match serde_json::from_slice(line) {
        Ok(request) => Some(request),
        Err(error) => {
            let outgoing = Outgoing::HarvesterResponse {
                id: None,
                response: HarvesterResponse::err(format!("parse error: {error}")),
            };
            let _ = out_tx.send(Outbound::Typed(outgoing)).await;

            None
        }
    }
}

fn handle_request(
    daemon: &Daemon,
    request: Request,
    out_tx: &mpsc::Sender<Outbound>,
    attached: &mut AttachedMap,
) -> Option<Outgoing> {
    match request {
        Request::Harvester { id, cmd } => ops::dispatch(daemon, id, cmd, out_tx, attached),
        Request::Pi { id, sid, cmd } => passthrough::pi_passthrough(daemon, id, sid, cmd, out_tx),
    }
}
