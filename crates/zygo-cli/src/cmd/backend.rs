// SPDX-License-Identifier: Apache-2.0
//! `zygo backend list` and `zygo backend install`.

use zygo_core::backend;
use zygo_core::spec::Isolation;

use crate::cli::{BackendCommand, Cli};
use crate::output::{self, Style};

pub fn run(cli: &Cli, command: &BackendCommand) -> anyhow::Result<u8> {
    match command {
        BackendCommand::List => list(cli),
        BackendCommand::Install { name } => install(cli, name),
    }
}

fn list(cli: &Cli) -> anyhow::Result<u8> {
    let style = Style::stdout();
    let paths = super::paths(cli);
    let mut rows = Vec::new();
    let mut json_rows = Vec::new();

    for isolation in Isolation::ALL {
        let (status, detail) = match backend::for_isolation(*isolation, &paths) {
            Ok(_) => ("available".to_string(), String::new()),
            Err(zygo_core::Error::BackendUnavailable { reason, .. }) => {
                ("unavailable".to_string(), reason)
            }
            Err(e) => ("unavailable".to_string(), e.to_string()),
        };

        json_rows.push(serde_json::json!({
            "backend": isolation.as_str(),
            "status": status,
            "detail": detail,
        }));
        rows.push(vec![
            isolation.as_str().to_string(),
            if status == "available" {
                style.green(&status)
            } else {
                style.dim(&status)
            },
            detail,
        ]);
    }

    if cli.json {
        output::json(&json_rows)?;
    } else {
        print!("{}", output::table(&["backend", "status", "detail"], &rows));
    }
    Ok(0)
}

fn install(cli: &Cli, name: &str) -> anyhow::Result<u8> {
    match name {
        "gvisor" => install_gvisor(cli),
        "vm" => install_vm(cli),
        other => anyhow::bail!("unknown backend `{other}`\n  → known backends: gvisor, vm"),
    }
}

/// Where Google publishes gVisor. The release channel, not `nightly`.
const GVISOR_RELEASES: &str = "https://storage.googleapis.com/gvisor/releases/release/latest";

/// The release artefact.
///
/// gVisor's own install instructions still describe a bare `runsc` next to a
/// `runsc.sha512`; the bucket no longer has one. What it has is this archive,
/// holding `runsc`, the containerd shim and a `gvisor-bin/` directory. The
/// `zstd` one rather than the `bz2` one because Zygo already carries a zstd
/// decoder for image layers and carries no bzip2 at all.
const GVISOR_ARCHIVE: &str = "gvisor.tar.zstd";

/// The two URLs for an architecture: the archive and its checksum.
///
/// gVisor names its directories by the kernel's architecture spelling
/// (`x86_64`, `aarch64`), not by Go's or by OCI's, which is why this is not
/// the same mapping the image store uses.
fn gvisor_urls(arch: &str) -> anyhow::Result<(String, String)> {
    let dir = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => anyhow::bail!(
            "gVisor publishes runsc for x86_64 and aarch64; this host is {other}\n  \
             → build runsc from source, or use --isolation ns"
        ),
    };
    Ok((
        format!("{GVISOR_RELEASES}/{dir}/{GVISOR_ARCHIVE}"),
        format!("{GVISOR_RELEASES}/{dir}/{GVISOR_ARCHIVE}.sha512"),
    ))
}

/// Pull the digest out of a `sha512sum` file.
///
/// The format is `<hex>  <filename>`, and the file may name a path rather
/// than a bare name. The digest is matched on the *file* it belongs to: a
/// checksum file listing several artefacts must not have its first line used
/// for whichever one is being downloaded.
fn parse_sha512(text: &str, filename: &str) -> anyhow::Result<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(named)) = (parts.next(), parts.next()) else {
            continue;
        };
        let named = named.trim_start_matches('*');
        let base = named.rsplit('/').next().unwrap_or(named);
        if base == filename {
            anyhow::ensure!(
                digest.len() == 128 && digest.chars().all(|c| c.is_ascii_hexdigit()),
                "`{digest}` is not a 128-character sha512"
            );
            return Ok(digest.to_ascii_lowercase());
        }
    }
    anyhow::bail!("the checksum file does not mention `{filename}`")
}

fn sha512_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha512};
    hex::encode(Sha512::digest(bytes))
}

/// Download `runsc`, verify it, and put it where the backend looks.
///
/// Verification is not optional and there is no flag to skip it: this
/// downloads a binary that will be handed other people's code to run, over a
/// link whose only guarantee is TLS to a bucket.
/// The guest kernel, pinned by digest.
///
/// libkrun itself is linked into this binary, so there is nothing to download
/// for it. The *kernel* is a different matter and deliberately so: it is GPL
/// where this binary is Apache-2.0, and it is twenty-five megabytes against a
/// fifteen megabyte budget. `krun_set_kernel` is what lets it be a file, and
/// this is what puts the file there.
fn install_vm(cli: &Cli) -> anyhow::Result<u8> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "a guest kernel is only useful on Linux; this host runs {}\n  \
         → run Zygo inside a Linux VM or container",
        std::env::consts::OS
    );
    anyhow::ensure!(
        zygo_core::backend::vm::COMPILED_IN,
        "this binary has no virtual machine monitor linked into it, so a guest \
         kernel would have nothing to boot it\n  \
         → build with `--features vm`, or `make vm-build`"
    );

    let paths = super::paths(cli);
    let destination = paths.krun().join(zygo_core::backend::vm::KERNEL_FILE);
    if destination.is_file() {
        eprintln!(
            "{} {}",
            Style::stderr().dim("already installed"),
            destination.display()
        );
        return Ok(0);
    }

    // Deliberately not a download, yet. There is no published artefact to
    // point at: libkrunfw ships source that builds a kernel, and the `Image`
    // this wants is what `make vm-kernel` extracts from that build. Saying so
    // is better than inventing a URL that would rot, and better than a silent
    // "not implemented" — the file is one `make` away and the message says
    // which one.
    anyhow::bail!(
        "there is no published guest kernel to download: libkrunfw ships source, \
         not an image\n  \
         → build one with `make vm-kernel`, then copy it in:\n     \
         install -Dm644 poc/vm-out/Image {}\n  \
         → `zygo doctor` reports it once it is there",
        destination.display()
    )
}

fn install_gvisor(cli: &Cli) -> anyhow::Result<u8> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "runsc only runs on Linux; this host runs {}\n  \
         → run Zygo inside a Linux VM or container",
        std::env::consts::OS
    );

    let (archive_url, checksum_url) = gvisor_urls(std::env::consts::ARCH)?;
    let paths = super::paths(cli);
    let destination = paths
        .backends()
        .join(zygo_core::backend::gvisor::INSTALL_DIR)
        .join(zygo_core::backend::gvisor::RUNSC);
    let style = Style::stderr();
    eprintln!("{} {archive_url}", style.dim("fetching"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (archive, checksum) = runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(900))
            .build()?;
        let fetch = |url: String| {
            let client = client.clone();
            async move {
                let response = client.get(&url).send().await?;
                let status = response.status();
                anyhow::ensure!(status.is_success(), "{url} answered {status}");
                Ok::<Vec<u8>, anyhow::Error>(response.bytes().await?.to_vec())
            }
        };
        let archive = fetch(archive_url.clone()).await?;
        let checksum = fetch(checksum_url.clone()).await?;
        Ok::<(Vec<u8>, Vec<u8>), anyhow::Error>((archive, checksum))
    })?;

    // Verified before a byte of it is decompressed. A decompressor fed
    // unverified input is an attack surface, not a convenience.
    let expected = parse_sha512(&String::from_utf8_lossy(&checksum), GVISOR_ARCHIVE)?;
    let actual = sha512_of(&archive);
    anyhow::ensure!(
        actual == expected,
        "the downloaded {GVISOR_ARCHIVE} does not match its published checksum\n  \
         expected {expected}\n  got      {actual}\n  \
         → nothing was installed; try again, and report it if it persists"
    );

    eprintln!(
        "{} {} MB, sha512 verified — unpacking",
        style.dim("ok"),
        archive.len() / 1_048_576
    );

    // Unpacked beside the destination and renamed into place, so a half
    // written runtime is never at the path the backend runs. A directory
    // rename is one syscall, which is what makes this atomic.
    std::fs::create_dir_all(paths.backends())?;
    let staging = paths.backends().join("gvisor.partial");
    let _ = std::fs::remove_dir_all(&staging);
    let unpacked =
        zygo_core::backend::gvisor::extract_release(std::io::Cursor::new(&archive), &staging)
            .inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&staging);
            })?;
    let size = std::fs::metadata(&unpacked)?.len();

    let install_dir = paths
        .backends()
        .join(zygo_core::backend::gvisor::INSTALL_DIR);
    let _ = std::fs::remove_dir_all(&install_dir);
    std::fs::rename(&staging, &install_dir)?;

    if cli.json {
        output::json(&serde_json::json!({
            "backend": "gvisor",
            "path": destination,
            "archive_sha512": actual,
            "bytes": size,
        }))?;
    } else {
        println!(
            "{} runsc {} — {} MB, with its {} sidecars",
            style.green("✓"),
            destination.display(),
            size / 1_048_576,
            zygo_core::backend::gvisor::SIDECAR_DIR,
        );
        println!(
            "  {}",
            style.dim("zygo run --isolation gvisor <image> <cmd> — one-shot only so far")
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_urls_are_the_release_channel_for_a_known_architecture() {
        let (archive, checksum) = gvisor_urls("aarch64").expect("aarch64 is published");
        assert_eq!(
            archive,
            "https://storage.googleapis.com/gvisor/releases/release/latest/aarch64/gvisor.tar.zstd"
        );
        assert_eq!(checksum, format!("{archive}.sha512"));
        assert!(!archive.contains("nightly"), "nightly is not a release");
        assert!(archive.starts_with("https://"), "the download must be TLS");

        assert!(gvisor_urls("riscv64").is_err(), "unpublished arch");
    }

    /// The exact line gVisor's bucket serves today, so a format change is a
    /// test failure rather than a failed install.
    #[test]
    fn the_published_checksum_line_parses() {
        let digest = "c3f556ea8dd122c99bedffb994801b585d7314ab549a98c295a5f15069d9eacd\
                      44fd241f562fa3b10f6a5d82a71cc676e4aa3df8e7016cb0599471f69e236357";
        assert_eq!(digest.len(), 128, "the fixture itself");
        // Two spaces between the digest and the name, as sha512sum writes it.
        let published = format!("{digest}  {GVISOR_ARCHIVE}\n");
        assert_eq!(
            parse_sha512(&published, GVISOR_ARCHIVE).expect("parses"),
            digest
        );
    }

    #[test]
    fn the_checksum_is_read_for_the_file_it_belongs_to() {
        let text = "\
aaa  gvisor.tar.bz2
bbb  gvisor.tar.zstd
";
        // Both are refused for their length before the name is trusted.
        assert!(
            parse_sha512(text, GVISOR_ARCHIVE).is_err(),
            "a short digest is not a digest"
        );

        let good = "f".repeat(128);
        let other = "a".repeat(128);
        let text = format!("{other}  gvisor.tar.bz2\n{good}  ./gvisor.tar.zstd\n");
        assert_eq!(
            parse_sha512(&text, GVISOR_ARCHIVE).expect("found"),
            good,
            "the digest must be the one on the line naming this file"
        );
        assert!(parse_sha512(&text, "missing").is_err());
    }

    #[test]
    fn a_digest_is_compared_in_lower_case_hex() {
        let upper = "A".repeat(128);
        let line = format!("{upper}  runsc\n");
        assert_eq!(
            parse_sha512(&line, "runsc").expect("found"),
            "a".repeat(128)
        );
        assert_eq!(
            sha512_of(b""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
            "the empty string's sha512, so the hashing itself is pinned"
        );
    }
}
