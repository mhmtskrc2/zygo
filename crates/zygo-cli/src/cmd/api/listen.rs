// SPDX-License-Identifier: Apache-2.0
//! Where the API listens, and the connections it accepts.
//!
//! `HOST:PORT` or `unix://PATH`, and the one rule about them: serving
//! without a token is allowed only where refusing would protect nobody —
//! a `0600` socket, or loopback. Past the bind, a connection is one hyper
//! task with a header timeout, so a client that opens a socket and stops
//! is not held for ever.

use std::sync::Arc;

use anyhow::Context;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

use super::{Api, handle};

/// How long a connection has to send its request headers.
///
/// hyper only honours this when the builder has a timer, and it had none —
/// so a connection that opened and sent one byte was held for ever.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Where to listen, parsed from `HOST:PORT` or `unix://PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Listen {
    Tcp(std::net::SocketAddr),
    Unix(std::path::PathBuf),
}

impl Listen {
    pub(super) fn parse(text: &str) -> anyhow::Result<Listen> {
        if let Some(path) = text.strip_prefix("unix://") {
            anyhow::ensure!(!path.is_empty(), "unix:// needs a path");
            return Ok(Listen::Unix(path.into()));
        }
        let addr = text
            .parse()
            .with_context(|| format!("`{text}` is not HOST:PORT or unix://PATH"))?;
        Ok(Listen::Tcp(addr))
    }

    /// Whether serving unauthenticated here exposes anything.
    ///
    /// A unix socket is `0600` in a `0700` directory — the same protection the
    /// control socket has — and loopback is reachable only from this host.
    /// Anything else is a network interface, and "no auth" there means anyone
    /// who can route to it can run code as this user.
    pub(super) fn allows_no_auth(&self) -> bool {
        match self {
            Listen::Unix(_) => true,
            Listen::Tcp(addr) => addr.ip().is_loopback(),
        }
    }
}

impl std::fmt::Display for Listen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Listen::Tcp(addr) => write!(f, "http://{addr}"),
            Listen::Unix(path) => write!(f, "unix://{}", path.display()),
        }
    }
}

pub(super) async fn serve(listen: Listen, api: Arc<Api>) -> anyhow::Result<()> {
    match listen {
        Listen::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("cannot listen on {addr}"))?;
            loop {
                let (stream, _) = listener.accept().await?;
                spawn_connection(stream, Arc::clone(&api));
            }
        }
        Listen::Unix(path) => {
            // A path left by a previous run is a file nobody is listening on;
            // the socket that matters is the one about to be bound.
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)
                .with_context(|| format!("cannot listen on {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            loop {
                let (stream, _) = listener.accept().await?;
                spawn_connection(stream, Arc::clone(&api));
            }
        }
    }
}

fn spawn_connection<S>(stream: S, api: Arc<Api>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let io = TokioIo::new(stream);
        let service = service_fn(move |req| handle(req, Arc::clone(&api)));
        let result = http1::Builder::new()
            // Without a timer hyper accepts `header_read_timeout` and then
            // silently does nothing with it, so a connection that sends one
            // byte and stops is held for ever.
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .serve_connection(io, service)
            .await;
        if let Err(e) = result {
            tracing::debug!("connection ended: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addresses_parse_both_forms() {
        assert_eq!(
            Listen::parse("127.0.0.1:7700").unwrap(),
            Listen::Tcp("127.0.0.1:7700".parse().unwrap())
        );
        assert_eq!(
            Listen::parse("unix:///tmp/api.sock").unwrap(),
            Listen::Unix("/tmp/api.sock".into())
        );
        assert!(Listen::parse("unix://").is_err());
        assert!(Listen::parse("not an address").is_err());
    }

    #[test]
    fn no_auth_is_only_allowed_where_it_exposes_nothing() {
        // P6: an unauthenticated API on a reachable address is code execution
        // for anyone on the network. Loopback and a 0600 unix socket are the
        // only places refusing would protect nobody.
        assert!(Listen::parse("127.0.0.1:7700").unwrap().allows_no_auth());
        assert!(Listen::parse("[::1]:7700").unwrap().allows_no_auth());
        assert!(
            Listen::parse("unix:///tmp/api.sock")
                .unwrap()
                .allows_no_auth()
        );
        assert!(!Listen::parse("0.0.0.0:7700").unwrap().allows_no_auth());
        assert!(!Listen::parse("10.0.0.5:7700").unwrap().allows_no_auth());
        assert!(!Listen::parse("[::]:7700").unwrap().allows_no_auth());
    }
}
