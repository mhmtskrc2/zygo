// SPDX-License-Identifier: Apache-2.0
//! Length-prefixed JSON framing.
//!
//! Wire format: a 4-byte big-endian unsigned length, then exactly that many
//! bytes of UTF-8 JSON. Big-endian because it is what `struct.pack(">I", n)`
//! gives a Python agent and what `Buffer.writeUInt32BE` gives a Node one —
//! the two most likely hand-written implementations.
//!
//! The length cap is a protection boundary, not a tuning knob: the agent runs
//! untrusted code, and a claimed 4 GiB frame must not make the supervisor
//! allocate 4 GiB.

use std::io::{Read, Write};

use super::Message;

/// Largest frame accepted in either direction. Payloads above this should use
/// a scratch file, not the control socket.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Bytes of length prefix.
pub const HEADER_BYTES: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {size} bytes exceeds the {MAX_FRAME_BYTES} byte limit")]
    TooLarge { size: usize },

    #[error("peer closed the connection mid-frame ({got} of {want} bytes)")]
    Truncated { got: usize, want: usize },

    #[error("frame is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Encode one message into a framed byte buffer.
pub fn encode<M: serde::Serialize>(msg: &M) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { size: body.len() });
    }
    let mut out = Vec::with_capacity(HEADER_BYTES + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode one message from a complete frame body (no length prefix).
pub fn decode<M: serde::de::DeserializeOwned>(body: &[u8]) -> Result<M, FrameError> {
    Ok(serde_json::from_slice(body)?)
}

/// Blocking frame reader over any `Read`.
///
/// Generic over the message type so the agent wire ([`Message`]) and the
/// supervisor's control socket ([`crate::supervisor::Request`]) share one
/// framing implementation. They are different protocols on purpose — an agent
/// runs untrusted code and must never be able to speak control messages — but
/// the bytes around the JSON are the same, and one implementation is one place
/// to get the length cap and the truncation rule right.
pub struct FrameReader<R, M = Message> {
    inner: R,
    /// Reused across frames so a busy agent does not allocate per request.
    buf: Vec<u8>,
    _message: std::marker::PhantomData<M>,
}

impl<R: Read, M: serde::de::DeserializeOwned> FrameReader<R, M> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            _message: std::marker::PhantomData,
        }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    /// The stream underneath, for callers that need its descriptor.
    ///
    /// The supervisor waits for readability with a deadline before calling
    /// [`FrameReader::read`], because `read` itself blocks: a handler in an
    /// infinite loop would otherwise hold the connection for ever.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Read the next message, or `None` at a clean end of stream.
    ///
    /// "Clean" means the peer closed before the length prefix. Closing partway
    /// through a frame is [`FrameError::Truncated`] — an agent that dies
    /// mid-write must not look like an orderly shutdown.
    pub fn read(&mut self) -> Result<Option<M>, FrameError> {
        let mut header = [0u8; HEADER_BYTES];
        match read_exact_or_eof(&mut self.inner, &mut header)? {
            0 => return Ok(None),
            n if n < HEADER_BYTES => {
                return Err(FrameError::Truncated {
                    got: n,
                    want: HEADER_BYTES,
                });
            }
            _ => {}
        }

        let size = u32::from_be_bytes(header) as usize;
        if size > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge { size });
        }

        self.buf.clear();
        self.buf.resize(size, 0);
        let got = read_exact_or_eof(&mut self.inner, &mut self.buf)?;
        if got < size {
            return Err(FrameError::Truncated { got, want: size });
        }
        decode(&self.buf).map(Some)
    }
}

/// Blocking frame writer over any `Write`.
pub struct FrameWriter<W, M = Message> {
    inner: W,
    _message: std::marker::PhantomData<M>,
}

impl<W: Write, M: serde::Serialize> FrameWriter<W, M> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            _message: std::marker::PhantomData,
        }
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Write one message and flush it. Flushing every frame is deliberate: the
    /// protocol is request/response, so a buffered `GO` that never reaches the
    /// child is a hang, and the syscall cost is irrelevant next to the `fork()`
    /// it is gating.
    pub fn write(&mut self, msg: &M) -> Result<(), FrameError> {
        let bytes = encode(msg)?;
        self.inner.write_all(&bytes)?;
        self.inner.flush()?;
        Ok(())
    }
}

/// Fill `buf`, returning how many bytes were read. Short only at end of stream.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize, std::io::Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ErrorCode, Metrics, PROTOCOL_VERSION};
    use serde_json::json;

    fn ready() -> Message {
        Message::Ready {
            proto: PROTOCOL_VERSION,
            pid: 7,
            imports_ms: 1.0,
            rss_kb: 2,
            runtime: "test/1".into(),
        }
    }

    #[test]
    fn header_is_four_byte_big_endian_length() {
        let bytes = encode(&Message::Ping { seq: 1, id: None }).unwrap();
        let declared = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(declared, bytes.len() - HEADER_BYTES);
        // Big-endian: a small frame has zeros first, which is what a Python
        // agent's `struct.pack(">I", n)` produces.
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[1], 0);
    }

    #[test]
    fn roundtrip_through_a_pipe() {
        let mut stream = Vec::new();
        let mut w = FrameWriter::new(&mut stream);
        w.write(&ready()).unwrap();
        w.write(&Message::Ping { seq: 9, id: None }).unwrap();

        let mut r: FrameReader<_, Message> = FrameReader::new(std::io::Cursor::new(stream));
        assert_eq!(r.read().unwrap(), Some(ready()));
        assert_eq!(r.read().unwrap(), Some(Message::Ping { seq: 9, id: None }));
        assert_eq!(r.read().unwrap(), None, "clean EOF");
    }

    #[test]
    fn reader_handles_a_frame_split_across_reads() {
        // A reader that hands over one byte at a time — what a socket under
        // load actually looks like.
        struct Trickle(std::io::Cursor<Vec<u8>>);
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if buf.is_empty() {
                    return Ok(0);
                }
                self.0.read(&mut buf[..1])
            }
        }

        let mut stream = Vec::new();
        FrameWriter::new(&mut stream).write(&ready()).unwrap();

        let mut r: FrameReader<_, Message> =
            FrameReader::new(Trickle(std::io::Cursor::new(stream)));
        assert_eq!(r.read().unwrap(), Some(ready()));
    }

    #[test]
    fn a_truncated_body_is_an_error_not_an_eof() {
        let mut stream = Vec::new();
        FrameWriter::new(&mut stream).write(&ready()).unwrap();
        stream.truncate(stream.len() - 3);

        let mut r: FrameReader<_, Message> = FrameReader::new(std::io::Cursor::new(stream));
        assert!(matches!(r.read(), Err(FrameError::Truncated { .. })));
    }

    #[test]
    fn a_truncated_header_is_an_error_too() {
        let mut r: FrameReader<_, Message> =
            FrameReader::new(std::io::Cursor::new(vec![0u8, 0, 1]));
        assert!(matches!(
            r.read(),
            Err(FrameError::Truncated { want: 4, got: 3 })
        ));
    }

    /// The sandbox runs untrusted code; a lying length prefix must not turn
    /// into a 4 GiB allocation in the supervisor.
    #[test]
    fn an_oversized_length_is_rejected_before_allocating() {
        let mut stream = u32::MAX.to_be_bytes().to_vec();
        stream.extend_from_slice(b"{}");

        let mut r: FrameReader<_, Message> = FrameReader::new(std::io::Cursor::new(stream));
        assert!(matches!(r.read(), Err(FrameError::TooLarge { .. })));
    }

    #[test]
    fn malformed_json_is_reported_as_such() {
        let body = b"{not json}";
        let mut stream = (body.len() as u32).to_be_bytes().to_vec();
        stream.extend_from_slice(body);

        let mut r: FrameReader<_, Message> = FrameReader::new(std::io::Cursor::new(stream));
        assert!(matches!(r.read(), Err(FrameError::Json(_))));
    }

    #[test]
    fn large_but_legal_payloads_survive() {
        let big = "x".repeat(1 << 20);
        let msg = Message::Done {
            cancelled: false,
            id: "a".into(),
            exit_code: 0,
            result: json!({ "blob": big }),
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            metrics: Metrics::default(),
        };
        let mut stream = Vec::new();
        FrameWriter::new(&mut stream).write(&msg).unwrap();
        let mut r: FrameReader<_, Message> = FrameReader::new(std::io::Cursor::new(stream));
        assert_eq!(r.read().unwrap(), Some(msg));
    }

    #[test]
    fn error_frames_roundtrip() {
        let msg = Message::Error {
            id: Some("a".into()),
            code: ErrorCode::Timeout,
            message: "deadline exceeded".into(),
        };
        let mut stream = Vec::new();
        FrameWriter::new(&mut stream).write(&msg).unwrap();
        let mut r: FrameReader<_, Message> = FrameReader::new(std::io::Cursor::new(stream));
        assert_eq!(r.read().unwrap(), Some(msg));
    }
}
