//! The CLI's side of the control socket, including starting a supervisor.
//!
//! `zygo serve` is expected to work on a machine where nothing is running yet
//! (design doc §4.2), so the client starts a supervisor when it cannot find
//! one. That is the only place in Zygo that spawns a background process, and it
//! is written to fail loudly: a `serve` that silently did nothing because the
//! supervisor died at startup is the worst possible outcome, so the client
//! waits for the socket to answer and reports the child's exit otherwise.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::protocol::frame::{FrameReader, FrameWriter};

use super::protocol::{CONTROL_VERSION, ControlError, Request, Response};

/// How long to wait for a freshly started supervisor to answer.
///
/// Generous: it has to create its directories and write the agent out, and on a
/// cold page cache that is disk-bound. A client that gave up early would start
/// a second supervisor, which is the one outcome worth ruling out.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection to a supervisor, greeted and ready.
pub struct Client {
    reader: FrameReader<UnixStream, Response>,
    writer: FrameWriter<UnixStream, Request>,
    /// What the supervisor said about itself, for `zygo ps` headers.
    pub supervisor_pid: u32,
    pub supervisor_version: String,
}

impl std::fmt::Debug for Client {
    /// Hand-written because the framed reader and writer own sockets and have
    /// nothing worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("supervisor_pid", &self.supervisor_pid)
            .field("supervisor_version", &self.supervisor_version)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connect to a running supervisor, or fail if there is not one.
    pub fn connect(paths: &Paths) -> Result<Client> {
        let socket = paths.supervisor_sock();
        let stream = UnixStream::connect(&socket).map_err(|e| Error::BackendUnavailable {
            backend: "supervisor",
            reason: format!("no supervisor at {}: {e}", socket.display()),
            remedy: "start one by serving a function: `zygo serve <handler> --name <name>`".into(),
        })?;
        Client::greet(stream)
    }

    /// Connect, starting a supervisor first if nothing answers.
    ///
    /// `exe` is the binary to re-exec — normally `std::env::current_exe()`, so
    /// the supervisor is always the same build as the client that started it.
    pub fn connect_or_start(paths: &Paths, exe: &Path) -> Result<Client> {
        if let Ok(client) = Client::connect(paths) {
            return Ok(client);
        }
        start_supervisor(paths, exe)?;
        Client::connect(paths)
    }

    fn greet(stream: UnixStream) -> Result<Client> {
        let reader = FrameReader::new(
            stream
                .try_clone()
                .map_err(|e| Error::primitive("dup", "control socket", e))?,
        );
        let writer = FrameWriter::new(stream);
        let mut client = Client {
            reader,
            writer,
            supervisor_pid: 0,
            supervisor_version: String::new(),
        };

        match client.send(&Request::Hello {
            control: CONTROL_VERSION,
            client: format!("zygo {}", crate::VERSION),
        })? {
            Response::Welcome { version, pid, .. } => {
                client.supervisor_pid = pid;
                client.supervisor_version = version;
                Ok(client)
            }
            Response::Error {
                code: ControlError::VersionMismatch,
                message,
            } => Err(Error::BackendUnavailable {
                backend: "supervisor",
                reason: message,
                remedy: "stop the running supervisor with `zygo stop --all`, \
                         then run the command again"
                    .into(),
            }),
            other => Err(unexpected(&other)),
        }
    }

    /// Send one request and read its answer.
    pub fn send(&mut self, request: &Request) -> Result<Response> {
        self.writer
            .write(request)
            .map_err(|e| Error::primitive("write", "control socket", std::io::Error::other(e)))?;
        match self
            .reader
            .read()
            .map_err(|e| Error::primitive("read", "control socket", std::io::Error::other(e)))?
        {
            Some(response) => Ok(response),
            None => Err(Error::BackendUnavailable {
                backend: "supervisor",
                reason: "the supervisor closed the connection without answering".into(),
                remedy: "check `zygo logs` for why it exited".into(),
            }),
        }
    }
}

/// Anything the client did not ask for is a protocol bug, not a user error.
fn unexpected(response: &Response) -> Error {
    Error::BackendUnavailable {
        backend: "supervisor",
        reason: format!("unexpected control response: {response:?}"),
        remedy: "this is a bug; please report it with the command you ran".into(),
    }
}

/// Start a supervisor in the background and wait for its socket to answer.
fn start_supervisor(paths: &Paths, exe: &Path) -> Result<()> {
    let mut child = Command::new(exe)
        .arg("--data-root")
        .arg(paths.data())
        .arg("supervisor")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Kept, so a supervisor that dies on startup can say why instead of
        // leaving the client with a bare timeout.
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::primitive("spawn", "supervisor", e))?;

    let deadline = Instant::now() + START_TIMEOUT;
    let socket = paths.supervisor_sock();
    while Instant::now() < deadline {
        if UnixStream::connect(&socket).is_ok() {
            return Ok(());
        }
        // Check the child before sleeping: a supervisor that failed to bind
        // exits in milliseconds, and waiting the full timeout to say so would
        // look like a hang.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Error::BackendUnavailable {
                backend: "supervisor",
                reason: format!(
                    "the supervisor exited immediately ({status}): {}",
                    first_line_of_stderr(&mut child)
                ),
                remedy: "run `zygo doctor`, or start it in the foreground with \
                         `zygo supervisor run` to see the whole error"
                    .into(),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let _ = child.kill();
    Err(Error::BackendUnavailable {
        backend: "supervisor",
        reason: format!(
            "the supervisor did not answer within {} s",
            START_TIMEOUT.as_secs()
        ),
        remedy: "start it in the foreground with `zygo supervisor run` to see why".into(),
    })
}

/// The child's first line of stderr, which is where its error message is.
fn first_line_of_stderr(child: &mut std::process::Child) -> String {
    let Some(stderr) = child.stderr.take() else {
        return "no output".into();
    };
    BufReader::new(stderr)
        .lines()
        .map_while(std::result::Result::ok)
        .find(|l| !l.trim().is_empty())
        .unwrap_or_else(|| "no output".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::{Listener, Supervisor};
    use std::sync::Arc;

    /// A supervisor on a scratch socket, torn down when the guard drops.
    struct Running {
        _dir: tempfile::TempDir,
        paths: Paths,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Running {
        fn start() -> Running {
            let dir = tempfile::tempdir().expect("tempdir");
            let paths = Paths::rooted(dir.path());
            let listener = Listener::bind(&paths).expect("bind");
            let supervisor = Arc::new(Supervisor::new(paths.clone()).expect("supervisor"));
            let thread = std::thread::spawn(move || {
                let _ = listener.serve(supervisor);
            });
            Running {
                _dir: dir,
                paths,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Ok(mut client) = Client::connect(&self.paths) {
                let _ = client.send(&Request::Shutdown);
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    #[test]
    fn a_client_greets_and_learns_who_it_is_talking_to() {
        let running = Running::start();
        let client = Client::connect(&running.paths).expect("connect");
        assert_eq!(client.supervisor_pid, std::process::id());
        assert_eq!(client.supervisor_version, crate::VERSION);
    }

    #[test]
    fn requests_and_answers_survive_a_real_socket() {
        let running = Running::start();
        let mut client = Client::connect(&running.paths).expect("connect");

        assert_eq!(client.send(&Request::Ping).expect("ping"), Response::Pong);
        assert_eq!(
            client.send(&Request::List).expect("list"),
            Response::Functions { functions: vec![] }
        );
        assert_eq!(
            client.send(&Request::Stop { name: None }).expect("stop"),
            Response::Stopped { names: vec![] }
        );
    }

    #[test]
    fn several_clients_can_be_connected_at_once() {
        // `zygo ps` in one terminal while `zygo exec` runs in another.
        let running = Running::start();
        let mut a = Client::connect(&running.paths).expect("first");
        let mut b = Client::connect(&running.paths).expect("second");
        assert_eq!(a.send(&Request::Ping).expect("a"), Response::Pong);
        assert_eq!(b.send(&Request::Ping).expect("b"), Response::Pong);
        assert_eq!(a.send(&Request::Ping).expect("a again"), Response::Pong);
    }

    #[test]
    fn a_missing_function_comes_back_as_an_error_not_a_hang() {
        let running = Running::start();
        let mut client = Client::connect(&running.paths).expect("connect");
        let response = client
            .send(&Request::Exec {
                name: "nope".into(),
                event: serde_json::Value::Null,
                timeout_ms: 1_000,
            })
            .expect("a response, whatever it says");
        assert!(matches!(
            response,
            Response::Error {
                code: ControlError::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn connecting_with_no_supervisor_explains_how_to_get_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::rooted(dir.path());
        let err = Client::connect(&paths).expect_err("nothing is listening");
        let text = err.to_string();
        assert!(text.contains("zygo serve"), "no next step: {text}");
    }

    /// An executable that fails the way a supervisor with a taken socket does:
    /// one line on stderr, then a non-zero exit.
    fn failing_executable(dir: &Path, message: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-supervisor");
        std::fs::write(&path, format!("#!/bin/sh\necho '{message}' >&2\nexit 3\n"))
            .expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[test]
    fn starting_a_supervisor_that_cannot_run_reports_the_child_not_a_timeout() {
        // The failure mode worth guarding: `connect_or_start` waiting the full
        // ten seconds and then blaming the socket, when the child exited at
        // once and said exactly what was wrong.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::rooted(dir.path());
        let exe = failing_executable(dir.path(), "the socket is already taken");

        let started = Instant::now();
        let err = Client::connect_or_start(&paths, &exe).expect_err("it serves nothing");
        assert!(
            started.elapsed() < START_TIMEOUT,
            "waited for the timeout instead of noticing the child had exited"
        );
        let text = err.to_string();
        assert!(text.contains("exited immediately"), "unhelpful: {text}");
        assert!(
            text.contains("the socket is already taken"),
            "the child's own error is the useful part, and it was dropped: {text}"
        );
    }

    #[test]
    fn connect_or_start_uses_a_supervisor_that_is_already_there() {
        // It must not spawn a second one. A path that cannot be executed proves
        // nothing was spawned: a spawn would have failed the call.
        let running = Running::start();
        let client = Client::connect_or_start(&running.paths, Path::new("/nonexistent/zygo"))
            .expect("the running supervisor was used");
        assert_eq!(client.supervisor_pid, std::process::id());
    }
}
