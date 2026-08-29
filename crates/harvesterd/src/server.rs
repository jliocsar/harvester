use std::future::Future;
use std::sync::Arc;

use harvester_proto::{
    read_frame, HarvesterResponse, Outgoing, Request, FRAME_BUFFER_START_BYTES, LF,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ops::{self, AttachedMap};
use crate::passthrough;
use crate::session::Daemon;

const OUTGOING_QUEUE_DEPTH: usize = 256;

pub enum Outbound {
    Typed(Outgoing),
    Raw(Arc<[u8]>),
}

impl Outbound {
    fn write_into(&self, line: &mut Vec<u8>) {
        line.clear();

        match self {
            Outbound::Typed(outgoing) => {
                serde_json::to_writer(&mut *line, outgoing).expect("serializable")
            }
            Outbound::Raw(bytes) => line.extend_from_slice(bytes),
        }

        line.push(LF);
    }
}

#[derive(Deserialize)]
struct RequestIdProbe {
    #[serde(default)]
    id: Option<Value>,
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
            outbound.write_into(&mut line);

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
                id: request_id_from_raw_line(line),
                response: HarvesterResponse::err(format!("parse error: {error}")),
            };
            let _ = out_tx.send(Outbound::Typed(outgoing)).await;

            None
        }
    }
}

fn request_id_from_raw_line(line: &[u8]) -> Option<Value> {
    serde_json::from_slice::<RequestIdProbe>(line)
        .ok()
        .and_then(|probe| probe.id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn decode_and_take_answer(line: &[u8]) -> (Option<Request>, Option<Outgoing>) {
        let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(OUTGOING_QUEUE_DEPTH);
        let request = decode_line(line, &out_tx).await;

        drop(out_tx);

        let answer = match out_rx.recv().await {
            Some(Outbound::Typed(outgoing)) => Some(outgoing),
            Some(Outbound::Raw(_)) => panic!("decode_line never answers with a raw line"),
            None => None,
        };

        (request, answer)
    }

    fn refusal_id(answer: Option<Outgoing>) -> Option<Value> {
        match answer.expect("an undecodable line must be answered") {
            Outgoing::HarvesterResponse { id, response } => {
                assert!(!response.success, "an undecodable line is never a success");

                id
            }
            other => panic!("expected a harvester_response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_undecodable_request_is_refused_with_the_id_the_client_sent() {
        let (request, answer) =
            decode_and_take_answer(br#"{"id":7,"type":"harvester","cmd":{"type":"telepathy"}}"#)
                .await;

        assert!(request.is_none(), "an unknown command never dispatches");
        assert_eq!(
            refusal_id(answer),
            Some(json!(7)),
            "dropping the id leaves the client's waiter pending forever"
        );
    }

    #[tokio::test]
    async fn an_unknown_envelope_type_keeps_its_id_too() {
        let (_, answer) = decode_and_take_answer(br#"{"id":11,"type":"telepathy"}"#).await;

        assert_eq!(refusal_id(answer), Some(json!(11)));
    }

    #[tokio::test]
    async fn a_non_numeric_id_is_echoed_verbatim() {
        let (_, answer) = decode_and_take_answer(br#"{"id":"abc","type":"telepathy"}"#).await;

        assert_eq!(refusal_id(answer), Some(json!("abc")));
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_is_refused_without_an_id() {
        let (_, answer) = decode_and_take_answer(b"this is not json at all").await;

        assert_eq!(refusal_id(answer), None);
    }

    #[tokio::test]
    async fn a_json_line_carrying_no_id_is_refused_without_one() {
        let (_, answer) = decode_and_take_answer(br#"{"type":"telepathy"}"#).await;

        assert_eq!(refusal_id(answer), None);
    }

    #[tokio::test]
    async fn an_empty_line_is_skipped_without_an_answer() {
        let (request, answer) = decode_and_take_answer(b"").await;

        assert!(request.is_none());
        assert!(answer.is_none(), "a blank line is not an error");
    }

    #[tokio::test]
    async fn a_well_formed_request_decodes_and_is_not_answered_here() {
        let (request, answer) = decode_and_take_answer(
            br#"{"id":1,"type":"harvester","cmd":{"type":"ping","version":1}}"#,
        )
        .await;

        assert!(matches!(request, Some(Request::Harvester { .. })));
        assert!(answer.is_none());
    }

    #[test]
    fn a_json_line_that_is_not_an_object_has_no_id_to_echo() {
        for line in [b"[1,2]".as_slice(), b"42".as_slice(), b"\"id\"".as_slice()] {
            assert_eq!(request_id_from_raw_line(line), None, "line {line:?}");
        }
    }

    #[test]
    fn a_null_id_probes_as_absent() {
        assert_eq!(request_id_from_raw_line(br#"{"id":null}"#), None);
    }
}
