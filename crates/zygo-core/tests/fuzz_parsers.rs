// SPDX-License-Identifier: Apache-2.0
//! Property and fuzz-style tests for everything that parses untrusted input.
//!
//! The spec file comes from a user and the wire protocol comes from inside a
//! sandbox, so neither may panic, hang or allocate unboundedly on malformed
//! input. These are deterministic pseudo-random sweeps rather than a
//! `cargo-fuzz` target: they run in CI on every change without a nightly
//! toolchain, and a failure is reproducible from the seed it prints.
//!
//! A coverage-guided `cargo-fuzz` target still belongs in the plan; this closes
//! the "must never panic" hole today.

// Gated because `image::auth` is: a build without the registry client has no
// credential store to sweep, and `cargo test --no-default-features` used to
// fail to compile here — so the feature-off build was never tested at all.
#[cfg(feature = "registry")]
use zygo_core::image::auth::CredentialStore;
use zygo_core::image::media::{Index, Manifest};
use zygo_core::lock::LockFile;
use zygo_core::protocol::{Message, decode, encode};
use zygo_core::spec::Spec;

/// xorshift64*, so a failure is reproducible from its seed without pulling in
/// a dependency just to generate noise.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Fragments drawn from the real grammar, so the sweep spends its time on
/// nearly-valid input rather than on bytes the lexer rejects immediately.
const SPEC_FRAGMENTS: &[&str] = &[
    "[defaults]",
    "[fn.a]",
    "[fn.a.env]",
    "[api]",
    "mem",
    "cpu",
    "pids",
    "timeout",
    "scratch",
    "network",
    "allow",
    "mounts",
    "entry",
    "cmd",
    "image",
    "runtime",
    "isolation",
    "seccomp",
    "secrets",
    "concurrency",
    "idle_timeout",
    "=",
    "\"",
    "'",
    "[",
    "]",
    "{",
    "}",
    ",",
    ".",
    "\n",
    " ",
    "\t",
    "-",
    "_",
    "256M",
    "0.5",
    "30s",
    "none",
    "egress",
    "host",
    "ns",
    "vm",
    "strict",
    "true",
    "false",
    "0",
    "-1",
    "99999999999999999999",
    "1e400",
    "nan",
    "python",
    "./h.py",
    "a:b:rw",
    "*.example.com:443",
    "10.0.0.0/8",
    "\u{feff}",
    "🙂",
    "\r\n",
    "#comment",
];

fn generated_spec(seed: u64) -> String {
    let mut rng = Rng(seed);
    let mut text = String::new();
    for _ in 0..(1 + rng.below(24)) {
        text.push_str(SPEC_FRAGMENTS[rng.below(SPEC_FRAGMENTS.len())]);
    }
    text
}

#[test]
fn the_spec_parser_never_panics_on_generated_input() {
    for seed in 1..=4000u64 {
        let text = generated_spec(seed);
        // Any outcome is acceptable except a panic.
        let result = std::panic::catch_unwind(|| Spec::parse(&text, None));
        assert!(result.is_ok(), "seed {seed} panicked on:\n{text}");
    }
}

#[test]
fn the_spec_parser_never_panics_on_arbitrary_bytes() {
    for seed in 1..=2000u64 {
        let mut rng = Rng(seed);
        let len = rng.below(256);
        let bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        let text = String::from_utf8_lossy(&bytes).into_owned();

        let result = std::panic::catch_unwind(|| Spec::parse(&text, None));
        assert!(result.is_ok(), "seed {seed} panicked on {bytes:?}");
    }
}

/// Resolution is where the limits and network rules are checked, so it sees
/// values the parser happily accepted. It must report them, not panic on them.
#[test]
fn resolution_never_panics_on_anything_that_parsed() {
    use zygo_core::spec::{Layer, ResolveOptions};

    for seed in 1..=4000u64 {
        let text = generated_spec(seed);
        let Ok(spec) = Spec::parse(&text, None) else {
            continue;
        };
        let names: Vec<String> = spec.function_names().map(str::to_string).collect();

        let result = std::panic::catch_unwind(|| {
            for name in &names {
                let _ = spec.resolve(Some(name), &Layer::default(), &ResolveOptions::default());
            }
            let _ = spec.resolve(None, &Layer::default(), &ResolveOptions::default());
        });
        assert!(result.is_ok(), "seed {seed} panicked resolving:\n{text}");
    }
}

#[test]
fn protocol_decoding_never_panics_on_arbitrary_bytes() {
    for seed in 1..=4000u64 {
        let mut rng = Rng(seed);
        let len = rng.below(512);
        let bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();

        let result = std::panic::catch_unwind(|| decode::<Message>(&bytes));
        assert!(result.is_ok(), "seed {seed} panicked on {len} bytes");
    }
}

/// JSON shaped like a protocol message, with fields missing, negative, or of
/// the wrong type — what a buggy third-party agent actually sends.
#[test]
fn protocol_decoding_never_panics_on_malformed_messages() {
    const SHAPES: &[&str] = &[
        r#"{"type":"READY"}"#,
        r#"{"type":"READY","proto":"one"}"#,
        r#"{"type":"READY","proto":-1,"pid":-1}"#,
        r#"{"type":"EXEC"}"#,
        r#"{"type":"EXEC","id":null,"event":{},"timeout_ms":-5}"#,
        r#"{"type":"RESULT","id":"a","exit_code":99999999999999999999}"#,
        r#"{"type":"DONE","id":"a","exit_code":0,"wall_ms":"fast"}"#,
        r#"{"type":"UNKNOWN"}"#,
        r#"{"type":42}"#,
        r#"{}"#,
        r#"[]"#,
        r#"null"#,
        r#"{"type":"FORKED","id":"a","pid":18446744073709551615}"#,
        r#"{"type":"ERROR","code":"not_a_code","message":"x"}"#,
    ];

    for shape in SHAPES {
        let result = std::panic::catch_unwind(|| decode::<Message>(shape.as_bytes()));
        assert!(result.is_ok(), "panicked on {shape}");
    }
}

/// A message that changed as it passed through would let an agent and a
/// supervisor disagree about what was said.
#[test]
fn decoded_messages_round_trip() {
    let messages = [
        r#"{"type":"READY","proto":1,"pid":7,"imports_ms":1.5,"rss_kb":2,"runtime":"x/1"}"#,
        r#"{"type":"EXEC","id":"a","event":{"n":[1,2,{"deep":true}]},"timeout_ms":30000}"#,
        r#"{"type":"FORKED","id":"a","pid":1234}"#,
        r#"{"type":"GO","id":"a"}"#,
        r#"{"type":"RESULT","id":"a","exit_code":0,"stdout":"ü\n","wall_ms":1.25}"#,
        r#"{"type":"DONE","id":"a","exit_code":1,"error":"boom","peak_rss_kb":9}"#,
        r#"{"type":"PING","seq":18446744073709551615}"#,
        r#"{"type":"SHUTDOWN","grace_ms":0}"#,
    ];

    for text in messages {
        let first: Message = decode(text.as_bytes()).unwrap_or_else(|e| panic!("{text}: {e}"));
        let bytes = encode(&first).expect("encode");
        // `encode` prepends a four-byte length header; skip it to decode the
        // body again.
        let second = decode(&bytes[4..]).expect("re-decode");
        assert_eq!(first, second, "{text} did not survive a round trip");
    }
}

/// The framing layer is fed by a sandbox running untrusted code, so a hostile
/// length prefix must not become a hostile allocation.
#[test]
fn framing_never_allocates_what_a_header_claims() {
    use std::io::Cursor;
    use zygo_core::protocol::FrameReader;

    for seed in 1..=500u64 {
        let mut rng = Rng(seed);
        let claimed = rng.next() as u32;
        let body_len = rng.below(32);

        let mut stream = claimed.to_be_bytes().to_vec();
        stream.extend((0..body_len).map(|_| rng.byte()));

        let result = std::panic::catch_unwind(|| {
            let mut reader: FrameReader<_, Message> = FrameReader::new(Cursor::new(stream));
            let _ = reader.read();
        });
        assert!(
            result.is_ok(),
            "seed {seed} panicked on a header claiming {claimed} bytes"
        );
    }
}

/// Image references come from the command line and from spec files.
#[test]
fn image_reference_parsing_never_panics() {
    use zygo_core::image::Reference;

    let long = "x".repeat(200);
    let pieces: Vec<&str> = vec![
        "a",
        "A",
        ".",
        "-",
        "_",
        "/",
        ":",
        "@",
        "sha256:",
        "localhost",
        "5000",
        "docker.io",
        "library",
        "latest",
        "ghcr.io",
        "\u{feff}",
        "🙂",
        " ",
        &long,
    ];

    for seed in 1..=4000u64 {
        let mut rng = Rng(seed);
        let mut text = String::new();
        for _ in 0..(1 + rng.below(12)) {
            text.push_str(pieces[rng.below(pieces.len())]);
        }
        let result = std::panic::catch_unwind(|| text.parse::<Reference>());
        assert!(result.is_ok(), "seed {seed} panicked on `{text}`");
    }
}

/// The scalar types back both CLI flags and spec fields, so they see whatever a
/// user types.
#[test]
fn scalar_parsing_never_panics() {
    use zygo_core::spec::{AllowRule, Bytes, Cpu, Duration, Mount};

    let long_digits = "9".repeat(40);
    let pieces: Vec<&str> = vec![
        "0",
        "-",
        "+",
        ".",
        "e",
        "9",
        "M",
        "G",
        "s",
        "ms",
        "h",
        ":",
        "/",
        "*",
        "1e400",
        "nan",
        "inf",
        "18446744073709551616",
        "-0.0",
        " ",
        "\t",
        &long_digits,
    ];

    for seed in 1..=4000u64 {
        let mut rng = Rng(seed);
        let mut text = String::new();
        for _ in 0..(1 + rng.below(10)) {
            text.push_str(pieces[rng.below(pieces.len())]);
        }
        let result = std::panic::catch_unwind(|| {
            let _ = text.parse::<Bytes>();
            let _ = text.parse::<Cpu>();
            let _ = text.parse::<Duration>();
            let _ = text.parse::<Mount>();
            let _ = text.parse::<AllowRule>();
        });
        assert!(result.is_ok(), "seed {seed} panicked on `{text}`");
    }
}

/// An OCI index or manifest comes from a **registry**, which is the most
/// remote input this program has: a hostile or merely broken one can answer
/// anything at all to a pull. `serde` refusing it is the expected outcome; a
/// panic on the way to refusing it would be a denial of service reachable by
/// anyone who can make Zygo pull from a host they control.
#[test]
fn registry_documents_never_panic_on_arbitrary_bytes() {
    let mut rng = Rng(0x0C13_D1A5_EED0);
    for _ in 0..8_000 {
        let len = rng.below(256);
        let bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        let text = String::from_utf8_lossy(&bytes);
        let _ = serde_json::from_str::<Index>(&text);
        let _ = serde_json::from_str::<Manifest>(&text);
    }
}

/// The same documents, but shaped like the real thing and then damaged: pure
/// noise is refused by the first byte and never reaches the fields, so it
/// exercises the outside of the parser and none of the inside.
#[test]
fn registry_documents_never_panic_when_a_plausible_one_is_damaged() {
    const SEED: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":7143,"platform":{"architecture":"arm64","os":"linux"}}]}"#;
    let mut rng = Rng(0xD0C7_0FED);
    for _ in 0..8_000 {
        let mut bytes = SEED.as_bytes().to_vec();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(bytes.len());
            bytes[at] = rng.byte();
        }
        let text = String::from_utf8_lossy(&bytes);
        let _ = serde_json::from_str::<Index>(&text);
        let _ = serde_json::from_str::<Manifest>(&text);
    }
}

/// `~/.docker/config.json` is a file a person edits, and Zygo reads it
/// whether or not it makes sense. `parse` is written to return an empty store
/// rather than fail, so the property is that it always returns *something* —
/// including for input that is not JSON at all.
#[cfg(feature = "registry")]
#[test]
fn docker_credentials_never_panic_and_always_return_a_store() {
    let mut rng = Rng(0x00C0_FFEE_A417);
    for _ in 0..8_000 {
        let len = rng.below(200);
        let bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        let store = CredentialStore::parse(&String::from_utf8_lossy(&bytes));
        // Looking one up must work on whatever came back.
        let _ = store.get("ghcr.io");
    }

    // And a plausible file with a damaged `auth` value, which is the field
    // that gets base64-decoded and split on a colon.
    const SEED: &str = r#"{"auths":{"ghcr.io":{"auth":"dXNlcjpwYXNz"},"docker.io":{"username":"u","password":"p"}}}"#;
    for _ in 0..8_000 {
        let mut bytes = SEED.as_bytes().to_vec();
        for _ in 0..1 + rng.below(3) {
            let at = rng.below(bytes.len());
            bytes[at] = rng.byte();
        }
        let store = CredentialStore::parse(&String::from_utf8_lossy(&bytes));
        let _ = store.get("ghcr.io");
    }
}

/// `zygo.lock` sits in a repository, which means it is edited by hand, merged
/// badly, and committed half-resolved. It is refused when it does not parse —
/// the point here is only that refusing it is a `Result` and never a panic.
#[test]
fn the_lock_file_parser_never_panics() {
    let mut rng = Rng(0x10CF_1125_EED0);
    for _ in 0..8_000 {
        let len = rng.below(200);
        let bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        let _ = toml::from_str::<LockFile>(&String::from_utf8_lossy(&bytes));
    }

    // And a plausible file with a few bytes changed, which is what a bad
    // merge produces.
    const SEED: &str = r#"
version = 1

[fn.resize]
image = "python:3.12-slim"
digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
"#;
    for _ in 0..8_000 {
        let mut bytes = SEED.as_bytes().to_vec();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(bytes.len());
            bytes[at] = rng.byte();
        }
        let _ = toml::from_str::<LockFile>(&String::from_utf8_lossy(&bytes));
    }
}

/// The DNS wire parser is the only one in Zygo that reads raw bytes a *tenant*
/// sent.
///
/// Every other parser here reads a file a person wrote or a frame an agent
/// Zygo started produced. `parse_query` reads whatever a program inside the
/// sandbox puts on a UDP socket, and the resolver runs in the supervisor's
/// process — so a panic there is a denial of service against every other
/// function on the host, reached from inside one sandbox.
///
/// The properties: parsing never panics, and every answer the resolver would
/// send is a well-formed packet whose header describes what is actually in it.
#[test]
fn dns_queries_from_a_sandbox_never_panic_and_always_answer_with_a_valid_packet() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use zygo_core::net::dns::{self, Rcode};

    let mut rng = Rng(0x0D_1157_4E17);

    // Arbitrary bytes, including packets that claim to be well-formed.
    for _ in 0..20_000 {
        let len = rng.below(600);
        let bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        if let Some(query) = dns::parse_query(&bytes) {
            check_answer(&query, &[]);
        }
    }

    // A real query with a few bytes changed: the shape that gets past the
    // header checks and then fails in the name.
    let seed = query_for("registry-1.docker.io", 1);
    for _ in 0..20_000 {
        let mut bytes = seed.clone();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(bytes.len());
            bytes[at] = rng.byte();
        }
        if let Some(query) = dns::parse_query(&bytes) {
            check_answer(&query, &[]);
        }
    }

    // A name with more addresses than one packet holds. The header used to
    // promise every one of them while the body stopped at the limit, which is
    // a malformed answer and reads to a resolver as a failed lookup.
    let many_v4: Vec<IpAddr> = (0..80)
        .map(|i| IpAddr::V4(Ipv4Addr::new(203, 0, 113, i as u8)))
        .collect();
    let many_v6: Vec<IpAddr> = (0..80)
        .map(|i| IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, i as u16)))
        .collect();
    for (qtype, addrs) in [(1u16, &many_v4), (28u16, &many_v6)] {
        // A long name leaves less room for records, so both ends are covered.
        for name in [
            "a.example.com",
            &"x".repeat(60),
            &format!("{}.example.com", "y".repeat(180)),
        ] {
            let Some(query) = dns::parse_query(&query_for(name, qtype)) else {
                continue;
            };
            let written = check_answer(&query, addrs);
            assert!(
                written > 0,
                "no records fitted for a {qtype} query on a {}-byte name",
                name.len()
            );
        }
    }

    /// Build the answer and check it describes itself honestly.
    ///
    /// Returns how many records it carries, so the caller can assert that the
    /// packet is not merely well-formed but also useful — a response with
    /// ANCOUNT 0 satisfies "the header matches the body" and answers nobody.
    fn check_answer(query: &dns::Query, addrs: &[IpAddr]) -> usize {
        let packet = dns::response(query, Rcode::NoError, addrs);
        assert!(
            packet.len() >= 12,
            "an answer shorter than a DNS header: {} bytes",
            packet.len()
        );
        assert!(
            packet.len() <= 512,
            "an answer over the 512-byte limit: {} bytes",
            packet.len()
        );

        let ancount = u16::from_be_bytes([packet[6], packet[7]]) as usize;
        let each = if query.qtype == 28 { 28 } else { 16 };
        let body = packet.len() - 12 - query_section_len(&packet);
        assert_eq!(
            ancount * each,
            body,
            "the header promises {ancount} records and the body holds {} bytes",
            body
        );
        ancount
    }

    /// The question section's length, read back out of the packet.
    fn query_section_len(packet: &[u8]) -> usize {
        let mut at = 12;
        while at < packet.len() {
            let label = packet[at] as usize;
            at += 1;
            if label == 0 {
                break;
            }
            at += label;
        }
        (at + 4).min(packet.len()) - 12
    }

    /// One well-formed question, as a resolver in a sandbox would send it.
    fn query_for(name: &str, qtype: u16) -> Vec<u8> {
        let mut packet = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            packet.push(label.len().min(63) as u8);
            packet.extend_from_slice(&label.as_bytes()[..label.len().min(63)]);
        }
        packet.push(0);
        packet.extend_from_slice(&qtype.to_be_bytes());
        packet.extend_from_slice(&1u16.to_be_bytes());
        packet
    }
}
