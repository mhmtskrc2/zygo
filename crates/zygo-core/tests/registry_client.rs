// SPDX-License-Identifier: Apache-2.0
//! The registry client, against a registry that really answers.
//!
//! Everything below the network was already covered by unit tests; what was
//! not covered at all was the client's behaviour when a registry answers
//! *differently from what was asked* — and that is where the bug was: a
//! pinned digest was never compared with what arrived, so
//! `zygo pull python@sha256:<X>` accepted any self-consistent manifest a
//! registry chose to serve.
//!
//! The server here is about a hundred lines of `std::net`, written out rather
//! than taken from a crate, because a test fixture that pulls in an HTTP stack
//! is a dependency the shipped code does not have. It is enough to be a
//! registry: `/v2/`, a manifest, blobs, a `401` with a `WWW-Authenticate`
//! challenge and a token endpoint — which also makes the token exchange and the
//! retry-after-401 path testable for the first time.

#![cfg(feature = "registry")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zygo_core::image::{Reference, RegistryClient, Store, digest_of};

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// What the registry should answer with.
#[derive(Default)]
struct Plan {
    /// Path → (status, content type, body).
    routes: HashMap<String, (u16, String, Vec<u8>)>,
    /// Manifest paths that answer with this `Docker-Content-Digest` instead of
    /// the body's own.
    lying_header: Option<String>,
    /// Demand a bearer token before serving anything but `/v2/`.
    require_token: bool,
}

struct FakeRegistry {
    address: String,
    plan: Arc<Mutex<Plan>>,
    requests: Arc<Mutex<Vec<String>>>,
    tokens_issued: Arc<AtomicUsize>,
}

impl FakeRegistry {
    fn start(plan: Plan) -> FakeRegistry {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = format!("127.0.0.1:{}", listener.local_addr().expect("addr").port());
        let plan = Arc::new(Mutex::new(plan));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let tokens_issued = Arc::new(AtomicUsize::new(0));

        let for_thread = (
            Arc::clone(&plan),
            Arc::clone(&requests),
            Arc::clone(&tokens_issued),
        );
        std::thread::spawn(move || {
            let (plan, requests, tokens) = for_thread;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let _ = serve_one(stream, &plan, &requests, &tokens);
            }
        });

        FakeRegistry {
            address,
            plan,
            requests,
            tokens_issued,
        }
    }

    fn reference(&self, repository: &str, digest: Option<&str>, tag: Option<&str>) -> Reference {
        Reference {
            registry: self.address.clone(),
            repository: repository.to_string(),
            tag: tag.map(str::to_string),
            digest: digest.map(str::to_string),
        }
    }

    fn serve(&self, path: &str, status: u16, content_type: &str, body: Vec<u8>) {
        self.plan
            .lock()
            .expect("plan")
            .routes
            .insert(path.to_string(), (status, content_type.to_string(), body));
    }

    fn paths(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

fn serve_one(
    mut stream: TcpStream,
    plan: &Arc<Mutex<Plan>>,
    requests: &Arc<Mutex<Vec<String>>>,
    tokens: &Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();

    let mut authorised = false;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if header
            .to_ascii_lowercase()
            .starts_with("authorization: bearer ")
        {
            authorised = true;
        }
    }
    requests.lock().expect("requests").push(path.clone());

    let plan = plan.lock().expect("plan");

    // The token endpoint, which is what a client reaches after a 401.
    if path.starts_with("/token") {
        tokens.fetch_add(1, Ordering::SeqCst);
        let body = br#"{"token":"a-real-token"}"#;
        return respond(&mut stream, 200, "application/json", body, None);
    }

    if path == "/v2/" {
        return respond(&mut stream, 200, "application/json", b"{}", None);
    }

    if plan.require_token && !authorised {
        let challenge = format!(
            "Bearer realm=\"http://{}/token\",service=\"fake\"",
            stream.local_addr()?
        );
        return respond_with_challenge(&mut stream, &challenge);
    }

    match plan.routes.get(&path) {
        Some((status, content_type, body)) => {
            let digest = if path.contains("/manifests/") {
                plan.lying_header.clone().or_else(|| Some(digest_of(body)))
            } else {
                None
            };
            respond(&mut stream, *status, content_type, body, digest.as_deref())
        }
        None => respond(&mut stream, 404, "application/json", b"{}", None),
    }
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    digest: Option<&str>,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n",
        body.len()
    );
    if let Some(digest) = digest {
        head.push_str(&format!("docker-content-digest: {digest}\r\n"));
    }
    head.push_str("connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn respond_with_challenge(stream: &mut TcpStream, challenge: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: {challenge}\r\n\
         content-length: 0\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Helpers for building an image the fixture can serve
// ---------------------------------------------------------------------------

/// A one-layer image: the layer tar, the config, and the manifest naming both.
struct Image {
    layer: Vec<u8>,
    config: Vec<u8>,
    manifest: Vec<u8>,
}

fn build_image() -> Image {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "hello", &b"world"[..])
        .unwrap();
    let layer = builder.into_inner().unwrap();

    let config = br#"{"architecture":"arm64","os":"linux","config":{"Cmd":["/bin/sh"]},"rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec();

    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
            "config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},
            "layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
        digest_of(&config),
        config.len(),
        digest_of(&layer),
        layer.len()
    )
    .into_bytes();

    Image {
        layer,
        config,
        manifest,
    }
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = zygo_core::Paths::rooted(dir.path());
    paths.ensure().expect("ensure");
    (dir, Store::new(paths))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

/// Register an image with the fixture and return its manifest digest.
fn publish(registry: &FakeRegistry, repository: &str, tag: &str, image: &Image) -> String {
    let manifest_digest = digest_of(&image.manifest);
    for version in [tag.to_string(), manifest_digest.clone()] {
        registry.serve(
            &format!("/v2/{repository}/manifests/{version}"),
            200,
            "application/vnd.oci.image.manifest.v1+json",
            image.manifest.clone(),
        );
    }
    registry.serve(
        &format!("/v2/{repository}/blobs/{}", digest_of(&image.config)),
        200,
        "application/octet-stream",
        image.config.clone(),
    );
    registry.serve(
        &format!("/v2/{repository}/blobs/{}", digest_of(&image.layer)),
        200,
        "application/octet-stream",
        image.layer.clone(),
    );
    manifest_digest
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// The positive case, first: an honest registry is pulled from successfully.
///
/// Without this, every refusal below is also satisfied by a client that cannot
/// talk to this fixture at all — which is the failure mode that makes a
/// security test worthless.
#[test]
fn an_honest_registry_is_pulled_from() {
    let registry = FakeRegistry::start(Plan::default());
    let image = build_image();
    let digest = publish(&registry, "test/app", "v1", &image);

    let (_dir, store) = store();
    let client = RegistryClient::new(store.clone()).expect("client");
    let reference = registry.reference("test/app", None, Some("v1"));

    let entry = runtime()
        .block_on(client.pull(&reference, |_| {}))
        .expect("the pull should succeed");

    assert_eq!(entry.manifest, digest);
    assert!(store.has_blob(&digest_of(&image.config)));
    assert!(
        registry.paths().iter().any(|p| p.contains("/manifests/v1")),
        "the tag was never requested: {:?}",
        registry.paths()
    );
}

/// A registry that serves a manifest other than the one asked for is refused.
///
/// The client never compared `reference.digest` with what
/// arrived, so a pinned pull — the whole mechanism `zygo.lock` rests on —
/// accepted a substitution silently.
#[test]
fn a_manifest_that_is_not_the_one_asked_for_is_refused() {
    let registry = FakeRegistry::start(Plan::default());
    let image = build_image();
    publish(&registry, "test/app", "v1", &image);

    // Ask for a digest the registry does not have, and let it answer with the
    // manifest it does have — exactly what a compromised or confused registry
    // would do.
    let wanted = digest_of(b"a manifest that was never served");
    registry.serve(
        &format!("/v2/test/app/manifests/{wanted}"),
        200,
        "application/vnd.oci.image.manifest.v1+json",
        image.manifest.clone(),
    );

    let (_dir, store) = store();
    let client = RegistryClient::new(store.clone()).expect("client");
    let reference = registry.reference("test/app", Some(&wanted), None);

    let err = runtime()
        .block_on(client.pull(&reference, |_| {}))
        .expect_err("a substituted manifest must be refused");
    let text = err.to_string();
    assert!(
        text.contains(&wanted[..20]) && text.contains("served"),
        "the refusal should name both digests: {text}"
    );
    assert!(
        !store.has_blob(&digest_of(&image.manifest)),
        "the substituted manifest was written to the store anyway"
    );
}

/// A registry whose `Docker-Content-Digest` header disagrees with its own body
/// is refused.
///
/// The header used to be taken as the answer whenever it was present, and the
/// body was hashed only when it was absent — so this disagreement was
/// invisible, and the *header's* value went into `zygo.lock`.
#[test]
fn a_header_that_disagrees_with_the_body_is_refused() {
    let image = build_image();
    let registry = FakeRegistry::start(Plan {
        lying_header: Some(digest_of(b"not this manifest")),
        ..Default::default()
    });
    publish(&registry, "test/app", "v1", &image);

    let (_dir, store) = store();
    let client = RegistryClient::new(store).expect("client");
    let reference = registry.reference("test/app", None, Some("v1"));

    let err = runtime()
        .block_on(client.pull(&reference, |_| {}))
        .expect_err("a lying header must be refused");
    assert!(err.to_string().contains("Docker-Content-Digest"), "{err}");
}

/// The 401 → token → retry path, which had no test at all.
///
/// A registry that demands a bearer token answers the first request with a
/// challenge; the client is expected to fetch a token from the realm and try
/// again. That it works is asserted by the pull succeeding *and* by the token
/// endpoint having been reached.
#[test]
fn a_registry_that_demands_a_token_is_satisfied_and_then_pulled_from() {
    let image = build_image();
    let registry = FakeRegistry::start(Plan {
        require_token: true,
        ..Default::default()
    });
    let digest = publish(&registry, "test/app", "v1", &image);

    let (_dir, store) = store();
    let client = RegistryClient::new(store.clone()).expect("client");
    let reference = registry.reference("test/app", None, Some("v1"));

    let entry = runtime()
        .block_on(client.pull(&reference, |_| {}))
        .expect("the token exchange should let the pull through");

    assert_eq!(entry.manifest, digest);
    assert!(
        registry.tokens_issued.load(Ordering::SeqCst) >= 1,
        "no token was ever requested, so the 401 path was not exercised"
    );
    // And the token is reused rather than fetched per blob: a pull makes
    // several requests and each round trip costs a connection.
    assert!(
        registry.tokens_issued.load(Ordering::SeqCst) <= 2,
        "a token was fetched for every request: {} times",
        registry.tokens_issued.load(Ordering::SeqCst)
    );
}
