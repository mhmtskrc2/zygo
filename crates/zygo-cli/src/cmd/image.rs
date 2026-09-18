//! `zygo pull`, `zygo images`, `zygo image prune`.

use anyhow::Context;
use zygo_core::image::{Platform, PullProgress, Reference, RegistryClient, Store};

use crate::cli::{Cli, ImageCommand};
use crate::output::{self, Style};

pub fn pull(cli: &Cli, image: &str, platform: Option<&str>) -> anyhow::Result<u8> {
    let reference: Reference = image
        .parse()
        .with_context(|| format!("cannot pull `{image}`"))?;

    let paths = super::paths(cli);
    paths.ensure()?;
    let store = Store::new(paths);

    let mut client = RegistryClient::new(store)?;
    if let Some(p) = platform {
        client = client.with_platform(parse_platform(p)?);
    }

    let style = Style::stdout();
    let quiet = cli.json;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let entry = runtime.block_on(client.pull(&reference, |event| {
        if quiet {
            return;
        }
        match event {
            PullProgress::Resolving { reference } => {
                println!("resolving {reference}");
            }
            PullProgress::LayerStart { digest, size } => {
                println!(
                    "  {} {}  {}",
                    style.dim("pull"),
                    short(&digest),
                    output::human_bytes(size)
                );
            }
            PullProgress::LayerCached { digest } => {
                println!("  {} {}", style.dim("cached"), short(&digest));
            }
            PullProgress::Unpacking { digest } => {
                println!("  {} {}", style.dim("unpack"), short(&digest));
            }
            PullProgress::LayerDone { .. } => {}
            PullProgress::Done { layers, bytes } => {
                println!(
                    "{} {layers} layers, {}",
                    style.green("done"),
                    output::human_bytes(bytes)
                );
            }
        }
    }))?;

    if cli.json {
        output::json(&entry)?;
    }
    Ok(0)
}

pub fn list(cli: &Cli) -> anyhow::Result<u8> {
    let store = Store::new(super::paths(cli));
    let images = store.list();

    if cli.json {
        output::json(&images)?;
        return Ok(0);
    }

    if images.is_empty() {
        println!("no images pulled yet — try `zygo pull python:3.12-slim`");
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = images
        .iter()
        .map(|i| {
            vec![
                i.reference.clone(),
                short(&i.manifest),
                i.layers.len().to_string(),
                output::human_bytes(i.size),
                output::human_age(i.pulled_at),
            ]
        })
        .collect();

    print!(
        "{}",
        output::table(&["reference", "digest", "layers", "size", "pulled"], &rows)
    );
    Ok(0)
}

pub fn maintain(cli: &Cli, command: &ImageCommand) -> anyhow::Result<u8> {
    match command {
        ImageCommand::Prune { dry_run } => prune(cli, *dry_run),
    }
}

fn prune(cli: &Cli, dry_run: bool) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let store = Store::new(paths.clone());
    let orphans = store.unreferenced_layers()?;

    if orphans.is_empty() {
        if !cli.json {
            println!("nothing to prune");
        }
        return Ok(0);
    }

    let style = Style::stdout();
    let mut freed = 0u64;
    for digest in &orphans {
        let dir = paths.layers().join(digest.trim_start_matches("sha256:"));
        freed += dir_size(&dir);
        if dry_run {
            println!("{} {}", style.dim("would remove"), short(digest));
        } else {
            std::fs::remove_dir_all(&dir).ok();
            // The compressed blob is only needed to unpack the layer again.
            if let Ok(blob) = store.blob_path(digest) {
                freed += blob.metadata().map(|m| m.len()).unwrap_or(0);
                std::fs::remove_file(&blob).ok();
            }
            println!("{} {}", style.dim("removed"), short(digest));
        }
    }

    println!(
        "{} {} across {} layers",
        if dry_run { "would free" } else { "freed" },
        output::human_bytes(freed),
        orphans.len()
    );
    Ok(0)
}

fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// `sha256:abcdef…` → `abcdef123456`, the form registries print.
fn short(digest: &str) -> String {
    digest
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect()
}

fn parse_platform(s: &str) -> anyhow::Result<Platform> {
    let mut parts = s.split('/');
    let os = parts
        .next()
        .filter(|p| !p.is_empty())
        .context("--platform expects os/arch, e.g. linux/amd64")?;
    let architecture = parts
        .next()
        .filter(|p| !p.is_empty())
        .context("--platform expects os/arch, e.g. linux/amd64")?;
    Ok(Platform {
        os: os.to_string(),
        architecture: architecture.to_string(),
        variant: parts.next().map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_shortened_the_way_registries_print_them() {
        assert_eq!(short(&format!("sha256:{}", "a".repeat(64))), "aaaaaaaaaaaa");
        assert_eq!(short("abc"), "abc");
    }

    #[test]
    fn platform_strings_parse() {
        let p = parse_platform("linux/amd64").unwrap();
        assert_eq!(p.os, "linux");
        assert_eq!(p.architecture, "amd64");
        assert_eq!(p.variant, None);

        let p = parse_platform("linux/arm/v7").unwrap();
        assert_eq!(p.variant.as_deref(), Some("v7"));

        assert!(parse_platform("linux").is_err());
        assert!(parse_platform("linux/").is_err());
    }

    #[test]
    fn directory_sizes_are_summed_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_size(tmp.path()), 150);
        assert_eq!(dir_size(&tmp.path().join("missing")), 0);
    }
}
