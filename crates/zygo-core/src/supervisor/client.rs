// SPDX-License-Identifier: Apache-2.0
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

/// How long to wait for an answer to a request that should be instant.
///
/// `ps`, `stop`, `logs`, `status`: all of them read data the supervisor
/// already has. Thirty seconds is far longer than any of them can honestly
/// need, and finite, which is the point — see [`Client::budget`].
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a `serve`.
///
/// Warming can legitimately take minutes: a venv build runs `pip` and a
/// derived layer runs `apt`, both inside a sandbox, both over the network.
/// The budget is for a supervisor that has *stopped*, not for a slow one.
pub const SERVE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Added to a request's own deadline before the client gives up on it.
///
/// The supervisor enforces the deadline and then still has to reply; a client
/// that gave up at exactly the same moment would race it and report a
/// timeout for a request that was answered.
pub const REPLY_GRACE: Duration = Duration::from_secs(10);

/// A connection to a supervisor, greeted and ready.
pub struct Client {
    reader: FrameReader<UnixStream, Response>,
    writer: FrameWriter<UnixStream, Request>,
    /// A third handle on the same socket, kept only to set the read timeout
    /// before each request. The reader owns its own clone and offers no way
    /// to reach the socket underneath.
    deadline: UnixStream,
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
        let dup = |s: &UnixStream| {
            s.try_clone()
                .map_err(|e| Error::primitive("dup", "control socket", e))
        };
        let reader = FrameReader::new(dup(&stream)?);
        let deadline = dup(&stream)?;
        let writer = FrameWriter::new(stream);
        let mut client = Client {
            reader,
            writer,
            deadline,
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
                remedy: "`zygo supervisor stop` restarts only the supervisor — it \
                         drains, exits, and the next `serve` or `up` starts one of \
                         this release; `zygo stop --all` is the heavier fallback, and \
                         on a Mac it stops the whole Linux VM"
                    .into(),
            }),
            other => Err(unexpected(&other)),
        }
    }

    /// Send one request and read its answer, within that request's budget.
    pub fn send(&mut self, request: &Request) -> Result<Response> {
        self.send_within(request, Client::budget(request))
    }

    /// How long this request may take before the client stops believing in it.
    ///
    /// Per request rather than one number, because the honest budgets differ
    /// by three orders of magnitude: `ps` reads a map, `serve` may run `apt`.
    /// A single short timeout would break warming; a single long one would be
    /// the same as none, which is what this used to have.
    pub fn budget(request: &Request) -> Duration {
        match request {
            Request::Serve { .. } => SERVE_TIMEOUT,
            // The supervisor owns the deadline and then has to answer, so the
            // client waits for the deadline *and* the reply.
            Request::Exec { timeout_ms, .. } => {
                Duration::from_millis(*timeout_ms).saturating_add(REPLY_GRACE)
            }
            // `RUN` is never sent through `send`: it is answered twice and
            // carries descriptors, so it has methods of its own below. The
            // arm exists so the match stays exhaustive.
            Request::Run { .. } => CONTROL_TIMEOUT,
            // A drain waits for in-flight requests, so the client has to wait
            // for the grace it asked for and then for the answer.
            Request::Drain { grace_ms } => {
                Duration::from_millis(*grace_ms).saturating_add(REPLY_GRACE)
            }
            // A follow is the caller's own loop of short requests; each one is
            // ordinary. Everything else reads state the supervisor has.
            _ => CONTROL_TIMEOUT,
        }
    }

    /// Send one request with an explicit budget.
    ///
    /// The budget is a **liveness** check, not a deadline for the work: it is
    /// there so that a supervisor which has stopped answering — a deadlocked
    /// thread, a wedged launcher — produces an error rather than a client that
    /// waits for ever. Before this, every hang in this project looked like the
    /// host had locked up, and three separate investigations began by ruling
    /// that out.
    pub fn send_within(&mut self, request: &Request, budget: Duration) -> Result<Response> {
        self.writer
            .write(request)
            .map_err(|e| Error::primitive("write", "control socket", std::io::Error::other(e)))?;

        // Set for this request and cleared after it, so a long `serve` cannot
        // leave its budget behind for the `ps` that follows on the same
        // connection.
        let _ = self.deadline.set_read_timeout(Some(budget));
        let answer = self.reader.read();
        let _ = self.deadline.set_read_timeout(None);

        match answer {
            Ok(Some(response)) => Ok(response),
            Ok(None) => Err(Error::BackendUnavailable {
                backend: "supervisor",
                reason: "the supervisor closed the connection without answering".into(),
                remedy: "check `zygo logs` for why it exited".into(),
            }),
            Err(e) if is_timeout(&e) => Err(Error::BackendUnavailable {
                backend: "supervisor",
                reason: format!(
                    "the supervisor did not answer {} within {}s",
                    name_of(request),
                    budget.as_secs()
                ),
                remedy: "it is running but not replying — check `zygo logs`, and \
                         `zygo supervisor stop` to restart it; report it if it repeats"
                    .into(),
            }),
            Err(e) => Err(Error::primitive(
                "read",
                "control socket",
                std::io::Error::other(e),
            )),
        }
    }
}

impl Client {
    /// Send a streaming request, handing each `CHUNK` to `on_chunk` as it
    /// arrives, and return the answer that ends it.
    ///
    /// The one call that reads more than one frame for one request, which is
    /// what a streaming `EXEC` answers with. `RUN` established the shape; this
    /// differs only in that the number of frames before the last is unknown,
    /// so the loop ends on the first frame that is not a `CHUNK`.
    ///
    /// The budget is per *frame*, not for the request: a stream is alive while
    /// it is producing, and a handler that prints every second for an hour is
    /// working rather than wedged. What bounds the request is the supervisor's
    /// own deadline, which it enforces through the cgroup.
    pub fn send_streaming(
        &mut self,
        request: &Request,
        mut on_chunk: impl FnMut(crate::protocol::Stream, &str),
    ) -> Result<Response> {
        self.writer
            .write(request)
            .map_err(|e| Error::primitive("write", "control socket", std::io::Error::other(e)))?;

        let budget = Client::budget(request);
        loop {
            let _ = self.deadline.set_read_timeout(Some(budget));
            let answer = self.reader.read();
            let _ = self.deadline.set_read_timeout(None);
            match answer {
                Ok(Some(Response::Chunk { stream, data })) => on_chunk(stream, &data),
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {
                    return Err(Error::BackendUnavailable {
                        backend: "supervisor",
                        reason: "the supervisor closed the connection mid-stream".into(),
                        remedy: "check `zygo logs` for why it exited".into(),
                    });
                }
                Err(e) if is_timeout(&e) => {
                    return Err(Error::BackendUnavailable {
                        backend: "supervisor",
                        reason: format!(
                            "the supervisor stopped sending for {}s during {}",
                            budget.as_secs(),
                            name_of(request)
                        ),
                        remedy: "it is running but not replying — check `zygo logs`".into(),
                    });
                }
                Err(e) => {
                    return Err(Error::primitive(
                        "read",
                        "control socket",
                        std::io::Error::other(e),
                    ));
                }
            }
        }
    }

    /// Ask the supervisor to run a one-shot sandbox with *this* process's
    /// standard streams, and return its pid once it exists.
    ///
    /// The three descriptors follow the frame on the same connection, sent
    /// with `SCM_RIGHTS` in the order stdin, stdout, stderr. They go
    /// **after** the frame and **before** the reply is read, and the framing
    /// is what makes that safe: `FrameReader` reads exactly a header and
    /// exactly a body, never ahead, so the supervisor's reader is positioned
    /// on the first descriptor message when it turns to receive them.
    ///
    /// Why the supervisor and not this process: on an ordinary systemd
    /// session this process cannot build a cgroup where it is, and cgroup
    /// delegation containment forbids it moving to one that would do — see
    /// `zygo-cli/src/scope.rs`. The supervisor already lives in one.
    ///
    /// The pid comes back first so the caller can forward its terminal's
    /// signals; [`Client::run_wait`] then waits for the exit.
    #[cfg(target_os = "linux")]
    pub fn run_start(&mut self, request: &Request, stdio: [std::os::fd::RawFd; 3]) -> Result<u32> {
        use std::os::fd::AsRawFd;

        debug_assert!(matches!(request, Request::Run { .. }));
        self.writer
            .write(request)
            .map_err(|e| Error::primitive("write", "control socket", std::io::Error::other(e)))?;
        for fd in stdio {
            // SAFETY: a plain `sendmsg` on this process's own socket with a
            // descriptor it owns; the helper touches nothing but its stack.
            unsafe { crate::net::linux::send_fd(self.deadline.as_raw_fd(), fd) }
                .map_err(|e| Error::primitive("sendmsg", "control socket", e))?;
        }

        let _ = self.deadline.set_read_timeout(Some(CONTROL_TIMEOUT));
        let answer = self.reader.read();
        let _ = self.deadline.set_read_timeout(None);
        match answer {
            Ok(Some(Response::Started { pid })) => Ok(pid),
            Ok(Some(Response::Error { code, message })) => Err(control_error(code, message)),
            Ok(Some(other)) => Err(unexpected(&other)),
            Ok(None) => Err(Error::BackendUnavailable {
                backend: "supervisor",
                reason: "the supervisor closed the connection before the sandbox started".into(),
                remedy: "check `zygo logs` for why it exited".into(),
            }),
            Err(e) => Err(Error::primitive(
                "read",
                "control socket",
                std::io::Error::other(e),
            )),
        }
    }

    /// Wait for the sandbox [`Client::run_start`] started to exit.
    ///
    /// `budget` is the sandbox's own deadline plus grace, or `None` for a run
    /// with no deadline at all — a program that is meant to run for hours
    /// must not be reported dead by a liveness timer. The supervisor owns the
    /// real deadline and enforces it through the cgroup either way.
    #[cfg(target_os = "linux")]
    pub fn run_wait(&mut self, budget: Option<Duration>) -> Result<Response> {
        let _ = self.deadline.set_read_timeout(budget);
        let answer = self.reader.read();
        let _ = self.deadline.set_read_timeout(None);
        match answer {
            Ok(Some(response @ Response::Ran { .. })) => Ok(response),
            Ok(Some(Response::Error { code, message })) => Err(control_error(code, message)),
            Ok(Some(other)) => Err(unexpected(&other)),
            Ok(None) => Err(Error::BackendUnavailable {
                backend: "supervisor",
                reason: "the supervisor closed the connection while the sandbox was running".into(),
                remedy: "the sandbox was killed with it; check `zygo logs`".into(),
            }),
            Err(e) if is_timeout(&e) => Err(Error::BackendUnavailable {
                backend: "supervisor",
                reason: "the supervisor did not report the sandbox's exit within its \
                         deadline plus grace"
                    .into(),
                remedy: "it is running but not replying — check `zygo logs`, and \
                         `zygo supervisor stop` to restart it; report it if it repeats"
                    .into(),
            }),
            Err(e) => Err(Error::primitive(
                "read",
                "control socket",
                std::io::Error::other(e),
            )),
        }
    }
}

/// A supervisor's `ERROR` frame, as the error the caller should see.
#[cfg(target_os = "linux")]
fn control_error(code: ControlError, message: String) -> Error {
    match code {
        ControlError::BadSpec => Error::Spec(crate::spec::SpecError::invalid("run", message)),
        _ => Error::BackendUnavailable {
            backend: "supervisor",
            reason: message,
            remedy: "run `zygo logs` on the supervisor, or run without one: `zygo stop --all`"
                .into(),
        },
    }
}

/// Whether a framing error is the read timeout expiring.
///
/// A socket read timeout surfaces as `WouldBlock` on Linux and `TimedOut` on
/// some other platforms; both mean the same thing here.
fn is_timeout(e: &crate::protocol::frame::FrameError) -> bool {
    match e {
        crate::protocol::frame::FrameError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        // A read that timed out part way through a frame surfaces as a short
        // read rather than as the error, because the reader had already
        // consumed the length prefix.
        crate::protocol::frame::FrameError::Truncated { .. } => false,
        _ => false,
    }
}

/// The request's name, for an error a user reads.
fn name_of(request: &Request) -> &'static str {
    match request {
        Request::Hello { .. } => "the greeting",
        Request::Serve { .. } => "serve",
        Request::Exec { .. } => "exec",
        Request::List => "ps",
        Request::Stop { .. } => "stop",
        Request::Shutdown => "shutdown",
        Request::Drain { .. } => "drain",
        Request::Ping => "ping",
        Request::Shell { .. } => "shell",
        Request::Logs { .. } => "logs",
        Request::Warm { .. } => "warm",
        Request::Run { .. } => "run",
        Request::ServeRuntime { .. } => "serving a runtime",
        Request::ExecScript { .. } => "running a script",
        Request::Runtimes => "listing the runtimes",
        Request::StopRuntime { .. } => "stopping a runtime",
        Request::PutScript { .. } => "registering a script",
        Request::PutDeps { .. } => "building a dependency set",
        Request::Deps { .. } => "looking a dependency set up",
        Request::DeleteDeps { .. } => "deleting a dependency set",
        Request::CreateTenant { .. } => "creating a tenant",
        Request::Tenants { .. } => "listing the tenants",
        Request::DeleteTenant { .. } => "deleting a tenant",
        Request::GetScript { .. } => "looking a script up",
        Request::SetLimits { .. } => "setting a tenant's limits",
        Request::PutSecret { .. } => "storing a secret",
        Request::Secrets { .. } => "listing the secrets",
        Request::DeleteSecret { .. } => "removing a secret",
        Request::PutBlob { .. } => "storing a blob",
        Request::GetBlob { .. } => "looking a blob up",
        Request::DeleteBlob { .. } => "removing a blob",
        Request::DeleteScript { .. } => "removing a script",
        Request::MintToken { .. } => "minting a token",
        Request::Tokens => "listing the tokens",
        Request::RevokeToken { .. } => "revoking a token",
        Request::Cancel { .. } => "cancelling a request",
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
///
/// The child is given the layout as **both** halves, through the environment
/// `Paths::from_env` already reads. It used to be handed `--data-root`, which
/// selects the rooted layout — `<root>/run` for the socket — while the parent
/// had worked out its own runtime directory from `XDG_RUNTIME_DIR`. So on any
/// ordinary login session the two disagreed: the supervisor came up and
/// listened on `~/.local/share/zygo/run/supervisor.sock` while the client
/// waited ten seconds on `/run/user/1000/zygo/supervisor.sock` and reported
/// that it "did not answer".
///
/// Neither development environment could show it. Both set `ZYGO_DATA_HOME`
/// *and* have no `XDG_RUNTIME_DIR`, so both halves came from the same place
/// and the two layouts happened to coincide.
fn start_supervisor(paths: &Paths, exe: &Path) -> Result<()> {
    let mut command = Command::new(exe);
    for (key, value) in paths.as_vars() {
        command.env(key, value);
    }
    let mut child = command
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
        // Naming the socket is the difference between a mystery and a
        // one-line diagnosis: when the supervisor is listening somewhere
        // else, this sentence is the only place the two paths could ever
        // have been compared.
        reason: format!(
            "the supervisor did not answer on {} within {} s",
            socket.display(),
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

    /// A supervisor that greets and then stops answering, which is what a
    /// deadlocked thread looks like from the outside.
    ///
    /// The real one did exactly this on a Raspberry Pi — a launcher thread
    /// parked on a futex — and every client that spoke to it waited for ever,
    /// because the control socket had no timeout at all. The symptom was
    /// indistinguishable from the machine having locked up.
    #[test]
    fn a_supervisor_that_stops_answering_is_an_error_not_a_hang() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(paths.supervisor_sock().parent().expect("parent"))
            .expect("run dir");
        let listener =
            std::os::unix::net::UnixListener::bind(paths.supervisor_sock()).expect("bind");

        let wedged = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Read the Hello frame: four bytes of length, then the body.
            let mut header = [0u8; 4];
            stream.read_exact(&mut header).expect("header");
            let mut body = vec![0u8; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut body).expect("body");
            let welcome = crate::protocol::frame::encode(&Response::Welcome {
                control: CONTROL_VERSION,
                version: "test".into(),
                pid: 1,
            })
            .expect("encode");
            stream.write_all(&welcome).expect("welcome");
            // And now nothing, ever. Held so the socket stays open: a closed
            // one would be an end of file, which the client already handled.
            std::thread::sleep(Duration::from_secs(30));
        });

        let mut client = Client::connect(&paths).expect("the greeting is answered");
        let started = Instant::now();
        let err = client
            .send_within(&Request::List, Duration::from_millis(300))
            .expect_err("a supervisor that never answers must be an error");
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_secs(5),
            "waited {waited:?} for a 300ms budget"
        );
        let message = err.to_string();
        assert!(message.contains("did not answer"), "{message}");
        assert!(message.contains("ps"), "the request is named: {message}");
        drop(client);
        drop(wedged);
    }

    /// The budgets differ by three orders of magnitude on purpose: one number
    /// would either break a warm-up or be the same as no timeout at all.
    #[test]
    fn each_request_gets_a_budget_that_fits_what_it_does() {
        let serve = Client::budget(&Request::Serve {
            tenant: None,
            name: "x".into(),
            spec: None,
            layer: Box::default(),
            base_dir: "/tmp".into(),
            allow_host_net: false,
            allow_private_net: false,
            allow_unlimited: false,
            secrets: Default::default(),
            if_changed: false,
        });
        assert_eq!(serve, SERVE_TIMEOUT);
        assert!(
            serve > Duration::from_secs(60),
            "a venv build or an apt layer takes minutes"
        );

        // An exec waits for the request's own deadline and then for the reply.
        let exec = Client::budget(&Request::Exec {
            name: "x".into(),
            event: serde_json::Value::Null,
            timeout_ms: 5_000,
            tenant: None,
            key: None,
            stream: false,
            workspace: None,
        });
        assert_eq!(exec, Duration::from_secs(5) + REPLY_GRACE);

        // Everything else reads state the supervisor already has.
        assert_eq!(Client::budget(&Request::List), CONTROL_TIMEOUT);
        assert_eq!(
            Client::budget(&Request::Stop { name: None }),
            CONTROL_TIMEOUT
        );
        assert!(CONTROL_TIMEOUT < Duration::from_secs(60));
    }

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
                tenant: None,
                key: None,
                stream: false,
                workspace: None,
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
        std::fs::write(&path, format!("#!/bin/sh\necho '{message}' >&2\nexit 3\n")).expect("write");
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
