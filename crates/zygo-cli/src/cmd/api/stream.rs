// SPDX-License-Identifier: Apache-2.0
//! `?stream=1`: the answer as newline-delimited JSON, while it runs.
//!
//! One object per line — the chunks as the handler prints them, then one
//! last line carrying exactly what the non-streaming answer would have
//! been. The control call blocks on its own thread for the whole request;
//! the body is whatever it has sent so far.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{Response, StatusCode};
use zygo_core::supervisor::Request as Control;
use zygo_core::supervisor::client::Client;

use super::reply::{ApiBody, HttpError, reply_to_json};
use super::usage::count_usage;
use super::{Api, MAX_IDLE_CLIENTS};

/// A body whose bytes arrive from somewhere else, while the response is open.
///
/// Twenty lines rather than a `tokio-stream` dependency for its `StreamBody`
/// adapter: this is the whole of what that would do, and a crate added for a
/// wrapper is a crate to keep up with.
struct Streamed(tokio::sync::mpsc::Receiver<Bytes>);

impl hyper::body::Body for Streamed {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        // The sender going away ends the body, which is how the request's own
        // thread says it has finished: it drops its half.
        self.0
            .poll_recv(cx)
            .map(|frame| frame.map(|bytes| Ok(hyper::body::Frame::data(bytes))))
    }
}

/// `?stream=1`: newline-delimited JSON, a line per event, as they happen.
///
/// One object per line — `{"stream":"stdout","data":"…"}` while the request
/// runs, then one final `{"result":…}` or `{"error":…}` carrying exactly what
/// the non-streaming answer would have been, plus its status. The status *code*
/// is 200 as soon as the first byte goes out, because it has to be: a response
/// cannot be given a status after its body has started. The last line is where
/// the outcome is.
///
/// NDJSON rather than server-sent events: every client in every language can
/// read a line and parse JSON, and SSE's framing buys nothing here — there is
/// one stream, no reconnection, and no last-event id to resume from.
const STREAM_CONTENT_TYPE: &str = "application/x-ndjson";

/// How many lines may be waiting to be written before the request's own thread
/// is held up.
///
/// Small on purpose. The point of a stream is that the reader sees output
/// early, and a deep buffer between the handler and the socket is a way to
/// arrive at the same time as `RESULT` would have. When it fills, the
/// supervisor's sink blocks — which is backpressure reaching the handler,
/// which is the correct place for it to reach.
const STREAM_BACKLOG: usize = 64;

/// Run a request and answer with its output as it is produced.
pub(super) async fn exec_streaming(
    api: &Arc<Api>,
    request: Control,
) -> Result<Response<ApiBody>, HttpError> {
    let (lines, rx) = tokio::sync::mpsc::channel::<Bytes>(STREAM_BACKLOG);
    let api = Arc::clone(api);

    // The control call is blocking and lives on its own thread for the whole
    // request; the body is whatever it has sent so far. Nothing here awaits
    // the end, which is the point.
    tokio::task::spawn_blocking(move || {
        let mut client = match api.clients.lock().expect("clients").pop() {
            Some(client) => client,
            None => match Client::connect_or_start(&api.paths, &api.exe) {
                Ok(client) => client,
                Err(e) => {
                    let _ = lines.blocking_send(line(&serde_json::json!({
                        "status": 500,
                        "error": format!("{e:#}"),
                    })));
                    return;
                }
            },
        };

        let answer = client.send_streaming(&request, |stream, data| {
            // A full channel blocks this thread, which is the supervisor
            // connection, which is the request. That is backpressure arriving
            // where it can do something: the handler waits for its reader.
            let _ = lines.blocking_send(line(&serde_json::json!({
                "stream": stream.as_str(),
                "data": data,
            })));
        });

        let last = match answer {
            Ok(reply) => {
                count_usage(&api, &reply);
                let (status, mut body) = reply_to_json(reply);
                body["status"] = status.as_u16().into();
                body
            }
            Err(e) => serde_json::json!({ "status": 500, "error": format!("{e:#}") }),
        };
        let _ = lines.blocking_send(line(&last));

        let mut idle = api.clients.lock().expect("clients");
        if idle.len() < MAX_IDLE_CLIENTS {
            idle.push(client);
        }
    });

    let body = Streamed(rx).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", STREAM_CONTENT_TYPE)
        // Nothing between here and the client may hold a line back waiting for
        // more: a proxy that buffers turns a stream into a slow whole answer.
        .header("cache-control", "no-store")
        .header("x-accel-buffering", "no")
        .body(body)
        .map_err(|e| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, e))
}

/// One NDJSON line: the object, then a newline.
fn line(value: &serde_json::Value) -> Bytes {
    let mut out = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    out.push(b'\n');
    Bytes::from(out)
}
