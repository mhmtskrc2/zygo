// SPDX-License-Identifier: Apache-2.0
//! One connection: `HELLO`, then a request at a time, each answered once.
//!
//! [`handle`] reads frames and writes answers. Two requests are answered
//! here rather than in the table below it, because they need the socket
//! itself: `RUN` is followed by three descriptors, and a streaming `EXEC`
//! is answered many times. [`dispatch`] is the table — every other request
//! is one method on [`Supervisor`] and one `Response`.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::listener::reject_foreign_peer;
use super::oneshot::{ClientWatch, receive_stdio};
use super::{CONTROL_VERSION, ControlError, Request, Response, ScriptRequest, Supervisor};
use crate::error::{Error, Result};
use crate::spec::ResolveOptions;

/// Serve one connection: `HELLO`, then requests until the peer goes away.
pub(super) fn handle(supervisor: &Supervisor, stream: UnixStream) -> Result<()> {
    if let Some(response) = reject_foreign_peer(&stream) {
        let mut writer: crate::protocol::frame::FrameWriter<_, Response> =
            crate::protocol::frame::FrameWriter::new(&stream);
        let _ = writer.write(&response);
        return Ok(());
    }

    let dup = |s: &UnixStream| {
        s.try_clone()
            .map_err(|e| Error::primitive("dup", "control socket", e))
    };
    // The socket itself, for the one request whose payload is not all in the
    // frame: `RUN` is followed by three descriptors over `SCM_RIGHTS`, and a
    // `FrameReader` — which reads exactly a header and exactly a body, never
    // ahead — is positioned on the first of them when the frame is done.
    let raw = dup(&stream)?;
    let mut reader: crate::protocol::frame::FrameReader<_, Request> =
        crate::protocol::frame::FrameReader::new(dup(&stream)?);
    let mut writer: crate::protocol::frame::FrameWriter<_, Response> =
        crate::protocol::frame::FrameWriter::new(stream);

    let mut greeted = false;
    while let Some(request) = reader
        .read()
        .map_err(|e| Error::primitive("read", "control socket", std::io::Error::other(e)))?
    {
        let shutting_down = matches!(request, Request::Shutdown);
        let response = match request {
            Request::Run {
                spec,
                layer,
                base_dir,
                allow_host_net,
                allow_private_net,
                allow_unlimited,
                tty,
                ignored_signals,
            } if greeted => {
                let options = ResolveOptions {
                    allow_host_net,
                    allow_private_net,
                    allow_unlimited,
                    base_dir: Some(base_dir),
                    one_shot: true,
                    pool: false,
                    tenant: None,
                };
                // Received before anything else, and before deciding
                // anything: the client has already sent them, and leaving
                // them on the socket would put the next frame out of
                // alignment.
                let stdio = receive_stdio(&raw);
                match stdio {
                    Ok(stdio) => {
                        let watch = ClientWatch::start(&raw);
                        let client = crate::pool::ClientStreams {
                            stdio,
                            tty,
                            ignored_signals,
                        };
                        let ran = supervisor.run(
                            spec.as_deref(),
                            &layer,
                            &options,
                            client,
                            |pid, cgroup| {
                                watch.started(pid, cgroup);
                                writer.write(&Response::Started { pid }).map_err(|e| {
                                    Error::primitive(
                                        "write",
                                        "control socket",
                                        std::io::Error::other(e),
                                    )
                                })
                            },
                        );
                        watch.finished();
                        merge(ran)
                    }
                    Err(e) => Response::error(
                        ControlError::BadMessage,
                        format!("RUN's descriptors did not arrive: {e}"),
                    ),
                }
            }
            // A streaming request is answered many times: a `CHUNK` per piece
            // of output, then the `EXECUTED`. Intercepted here rather than in
            // `dispatch` for the same reason `RUN` is — this is the scope
            // that has the socket, and `dispatch` returns one response by
            // construction.
            Request::Exec {
                name,
                event,
                timeout_ms,
                tenant,
                key,
                stream: true,
                workspace,
            } if greeted => {
                // Borrowed for the length of the call and released before the
                // final answer is written. One thread, so uncontended: the
                // lock is what lets a `Fn` closure reach a `&mut` writer, not
                // a synchronisation point.
                let out = std::sync::Mutex::new(&mut writer);
                let sink = chunk_sink(&out);
                let answered = supervisor
                    .owned_by(&name, tenant.as_deref())
                    .and_then(|()| {
                        supervisor.exec_full(
                            &name,
                            event,
                            Duration::from_millis(timeout_ms),
                            key.as_deref(),
                            Some(&sink),
                            workspace,
                        )
                    });
                merge(answered)
            }
            Request::ExecScript {
                runtime,
                script,
                event,
                timeout_ms,
                tenant,
                key,
                stream: true,
                workspace,
            } if greeted => {
                let out = std::sync::Mutex::new(&mut writer);
                let sink = chunk_sink(&out);
                merge(supervisor.exec_script_full(ScriptRequest {
                    tenant: tenant.as_deref(),
                    key: key.as_deref(),
                    sink: Some(&sink),
                    workspace,
                    ..ScriptRequest::new(&runtime, script, event, Duration::from_millis(timeout_ms))
                }))
            }
            other => dispatch(supervisor, other, &mut greeted),
        };
        writer
            .write(&response)
            .map_err(|e| Error::primitive("write", "control socket", std::io::Error::other(e)))?;
        if shutting_down {
            supervisor.shutdown();
            // Nudge the accept loop out of `accept` so it sees the flag.
            let _ = UnixStream::connect(supervisor.paths().supervisor_sock());
            break;
        }
    }
    Ok(())
}

/// A sink that writes each chunk straight out as a `CHUNK` frame.
///
/// Write errors are dropped on purpose. The client going away mid-stream is
/// ordinary — somebody closed a terminal — and it is not a reason to fail the
/// request, which is still running and whose `EXECUTED` will fail to write for
/// the same reason a moment later. That is where the connection ends.
fn chunk_sink<'a, W: std::io::Write + Send>(
    out: &'a std::sync::Mutex<&'a mut crate::protocol::frame::FrameWriter<W, Response>>,
) -> impl Fn(crate::protocol::Stream, &str) + Send + Sync + use<'a, W> {
    move |stream, data| {
        let _ = out.lock().expect("writer").write(&Response::Chunk {
            stream,
            data: data.to_string(),
        });
    }
}

/// Answer one request.
pub(super) fn dispatch(supervisor: &Supervisor, request: Request, greeted: &mut bool) -> Response {
    // `HELLO` first, always: a client that has not agreed a control version
    // must not be able to reach anything that changes state.
    if let Request::Hello { control, .. } = &request {
        if *control != CONTROL_VERSION {
            return Response::error(
                ControlError::VersionMismatch,
                format!(
                    "client speaks control v{control}, this supervisor speaks v{CONTROL_VERSION}"
                ),
            );
        }
        *greeted = true;
        return Response::Welcome {
            control: CONTROL_VERSION,
            version: crate::VERSION.to_string(),
            pid: std::process::id(),
        };
    }
    if !*greeted {
        return Response::error(ControlError::BadMessage, "expected HELLO first");
    }

    match request {
        Request::Hello { .. } => unreachable!("handled above"),
        Request::Ping => Response::Pong,
        Request::List => Response::Functions {
            functions: supervisor.list(),
        },
        Request::Serve {
            name,
            spec,
            layer,
            base_dir,
            allow_host_net,
            allow_private_net,
            allow_unlimited,
            secrets,
            if_changed,
            tenant,
        } => {
            let options = ResolveOptions {
                allow_host_net,
                allow_private_net,
                allow_unlimited,
                base_dir: Some(base_dir),
                one_shot: false,
                pool: false,
                tenant,
            };
            merge(supervisor.serve(
                &name,
                spec.as_deref(),
                &layer,
                &options,
                secrets,
                if_changed,
            ))
        }
        // A streaming `EXEC` is intercepted in `handle`, which has the socket
        // the chunks go out on. One that reaches here asked for none.
        Request::Exec {
            name,
            event,
            timeout_ms,
            tenant,
            key,
            stream: _,
            workspace,
        } => merge(
            supervisor
                .owned_by(&name, tenant.as_deref())
                .and_then(|()| {
                    supervisor.exec_full(
                        &name,
                        event,
                        Duration::from_millis(timeout_ms),
                        key.as_deref(),
                        None,
                        workspace,
                    )
                }),
        ),
        Request::Stop { name, runtimes } => merge(supervisor.stop(name.as_deref(), runtimes)),
        Request::Warm { name, tenant } => merge(
            supervisor
                .owned_by(&name, tenant.as_deref())
                .and_then(|()| supervisor.warm(&name)),
        ),
        Request::Shell { name } => merge(supervisor.shell(&name)),
        Request::Logs {
            name,
            after,
            limit,
            failed,
            tenant,
        } => merge(
            supervisor
                .owned_by(&name, tenant.as_deref())
                .and_then(|()| supervisor.logs(&name, after, limit, failed)),
        ),
        Request::ServeRuntime {
            name,
            spec,
            layer,
            base_dir,
            allow_host_net,
            allow_private_net,
            allow_unlimited,
            tenant,
            deps,
        } => {
            let options = ResolveOptions {
                allow_host_net,
                allow_private_net,
                allow_unlimited,
                base_dir: Some(base_dir),
                one_shot: false,
                pool: true,
                tenant,
            };
            merge(supervisor.serve_runtime(
                &name,
                spec.as_deref(),
                &layer,
                &options,
                deps.as_deref(),
            ))
        }
        Request::PutDeps {
            image,
            files,
            tenant,
        } => merge(supervisor.put_deps(&image, &files, tenant.as_deref())),
        Request::Deps { id, tenant } => merge(supervisor.deps(id.as_deref(), tenant.as_deref())),
        Request::DeleteDeps { id } => merge(supervisor.delete_deps(&id)),
        Request::ExecScript {
            runtime,
            script,
            event,
            timeout_ms,
            tenant,
            key,
            stream: _,
            workspace,
        } => merge(supervisor.exec_script_full(ScriptRequest {
            tenant: tenant.as_deref(),
            key: key.as_deref(),
            workspace,
            ..ScriptRequest::new(&runtime, script, event, Duration::from_millis(timeout_ms))
        })),
        Request::Runtimes => Response::Runtimes {
            runtimes: supervisor.runtimes(),
        },
        Request::StopRuntime { name } => merge(supervisor.stop_runtime(&name)),
        Request::PutScript { source, tenant } => {
            merge(supervisor.put_script(&source, tenant.as_deref()))
        }
        Request::CreateTenant { id } => merge(supervisor.create_tenant(&id)),
        Request::Tenants { id } => merge(supervisor.tenants(id.as_deref())),
        Request::DeleteTenant { id } => merge(supervisor.delete_tenant(&id)),
        Request::MintToken { tenant } => merge(supervisor.mint_token(tenant.as_deref())),
        Request::Tokens => merge(supervisor.list_tokens()),
        Request::RevokeToken { id } => merge(supervisor.revoke_token(&id)),
        Request::Cancel { id, tenant } => merge(supervisor.cancel(&id, tenant.as_deref())),
        Request::SetLimits { tenant, limits } => merge(supervisor.set_limits(&tenant, *limits)),
        Request::PutSecret {
            tenant,
            name,
            value,
        } => merge(supervisor.put_secret(&tenant, &name, &value)),
        Request::Secrets { tenant } => merge(supervisor.secret_names(&tenant)),
        Request::DeleteSecret { tenant, name } => merge(supervisor.delete_secret(&tenant, &name)),
        Request::PutBlob { tar } => merge(supervisor.put_blob(&tar)),
        Request::GetBlob { digest } => merge(supervisor.get_blob(&digest)),
        Request::DeleteBlob { digest } => merge(supervisor.delete_blob(&digest)),
        Request::GetScript { digest } => merge(supervisor.get_script(&digest)),
        Request::DeleteScript { digest } => merge(supervisor.delete_script(&digest)),
        Request::Shutdown => Response::Ok,
        Request::Drain { grace_ms } => supervisor.drain(Duration::from_millis(grace_ms)),
        // Intercepted in `handle`, which has the socket the descriptors
        // arrive on; a `RUN` that reaches this table was sent to a code path
        // that cannot receive them.
        Request::Run { .. } => Response::error(
            ControlError::BadMessage,
            "RUN carries descriptors and is answered before dispatch",
        ),
    }
}

/// Both arms of these results are already responses; the split only exists so
/// the happy path can use `?`.
fn merge(result: std::result::Result<Response, Response>) -> Response {
    match result {
        Ok(r) | Err(r) => r,
    }
}
