// SPDX-License-Identifier: Apache-2.0
//! The connection to one agent, with replies routed back by request id.
//!
//! Requests are not serialised on the wire. The agent can hold several
//! forks at once, so the only exclusive moment is the write of one frame;
//! a thread reads every reply and hands it to the caller whose id it
//! carries. [`Conn`] is the socket and that table; [`Reply`] is one
//! caller's place in it, removed when the caller goes.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::protocol::{Message, ProtocolError};

/// The framed connection, with its reader and writer kept together so a request
/// cannot interleave with another one's reply.
/// The connection to one agent, with a thread demultiplexing its replies.
///
/// Requests are not serialised here. The agent can hold several forks at once,
/// so the only thing that has to be exclusive is the moment a frame is written;
/// replies come back out of order and are routed to their caller by request id.
///
/// The shape this replaced held one lock for the whole `EXEC`→`DONE` round.
/// Measured with the CPU quota lifted, that capped a function at ~600
/// requests/s whether one caller or four were asking, using 1.5 of 4 cores —
/// and because a plain mutex is not fair, one of the four waited 3.7 s.
pub(super) struct Conn {
    pub(super) writer: Mutex<crate::protocol::FrameWriter<std::os::unix::net::UnixStream>>,
    /// Callers waiting for a reply, by request id.
    pub(super) waiting: Mutex<BTreeMap<String, std::sync::mpsc::Sender<Message>>>,
    /// Why the connection stopped working, once it has.
    pub(super) broken: Mutex<Option<String>>,
}

impl Conn {
    /// Route replies to their callers until the agent goes away.
    pub(super) fn read_replies(
        conn: &std::sync::Arc<Conn>,
        mut reader: crate::protocol::FrameReader<std::os::unix::net::UnixStream>,
    ) {
        loop {
            let message = match reader.read() {
                Ok(Some(message)) => message,
                Ok(None) => return conn.fail("the agent closed the connection"),
                Err(e) => return conn.fail(&format!("the agent connection failed: {e}")),
            };
            let Some(id) = message.request_id().map(str::to_string) else {
                // `PONG` and anything else without an id belongs to no caller.
                continue;
            };
            let waiting = conn.waiting.lock().expect("waiting").get(&id).cloned();
            if let Some(caller) = waiting {
                // A caller that has given up and gone is not an error: its
                // deadline expired and it has already said so.
                let _ = caller.send(message);
            }
        }
    }

    /// Record why the connection died and wake everyone waiting on it.
    ///
    /// Dropping the senders is what does the waking: every `recv` returns an
    /// error rather than waiting out a deadline for a reply that is never
    /// coming.
    fn fail(&self, reason: &str) {
        *self.broken.lock().expect("broken") = Some(reason.to_string());
        self.waiting.lock().expect("waiting").clear();
    }

    fn failure(&self) -> Option<String> {
        self.broken.lock().expect("broken").clone()
    }

    /// Register a caller and write its request, as one step.
    ///
    /// Registering first matters: the agent can answer before `write` has even
    /// returned, and a reply that arrives with nobody waiting is dropped.
    pub(super) fn send(&self, id: &str, message: &Message) -> Result<Reply<'_>> {
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut waiting = self.waiting.lock().expect("waiting");
            if let Some(reason) = self.failure() {
                return Err(Error::BackendUnavailable {
                    backend: "pool",
                    reason,
                    remedy: "the function will be rewarmed".into(),
                });
            }
            waiting.insert(id.to_string(), tx);
        }
        let reply = Reply {
            conn: self,
            id: id.to_string(),
            rx,
        };
        self.write(message)?;
        Ok(reply)
    }

    /// Write one frame. Exclusive only for as long as the frame takes.
    pub(super) fn write(&self, message: &Message) -> Result<()> {
        self.writer
            .lock()
            .expect("writer")
            .write(message)
            .map_err(|e| Error::from(ProtocolError::from(e)))
    }
}

/// A caller's place in the reply queue, removed when it goes out of scope.
pub(super) struct Reply<'a> {
    conn: &'a Conn,
    id: String,
    rx: std::sync::mpsc::Receiver<Message>,
}

impl Reply<'_> {
    /// Wait for the next reply to this request.
    pub(super) fn next(
        &self,
        timeout: std::time::Duration,
    ) -> std::result::Result<Message, ReplyError> {
        match self.rx.recv_timeout(timeout) {
            Ok(message) => Ok(message),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(ReplyError::TimedOut),
            // The sender was dropped, which only happens when the reader
            // thread cleared the table because the connection died.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(ReplyError::Gone(self.conn.failure().unwrap_or_else(|| {
                    "the agent connection was closed".into()
                })))
            }
        }
    }
}

impl Drop for Reply<'_> {
    fn drop(&mut self) {
        self.conn.waiting.lock().expect("waiting").remove(&self.id);
    }
}

pub(super) enum ReplyError {
    TimedOut,
    Gone(String),
}
