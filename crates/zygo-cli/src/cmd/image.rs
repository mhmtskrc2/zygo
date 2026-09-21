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
        ImageCommand::Prune {
            dry_run,
            unused_for,
            blobs,
        } => prune(
            cli,
            &PruneOptions {
                dry_run: *dry_run,
                unused_for: unused_for.map(|d| d.get()),
                blobs: *blobs,
            },
        ),
        ImageCommand::Rm { references } => rm(cli, references),
    }
}

/// `zygo image rm`.
///
/// The verb a Docker user reaches for as `rmi`, and until it existed the only
/// thing `prune` could ever collect was what a crash had orphaned: nothing a
/// user decided they were finished with could become unreferenced. Three
/// steps, in this order. Refuse if a warm function is on the image, because
/// its rootfs is these layers mounted and the supervisor has no way to be
/// told they went. Drop the index entries — the base, and every `+system.`
/// image derived from it. Then run the collection `prune` runs, so the disk
/// comes back now rather than after a second command nobody knows to type.
fn rm(cli: &Cli, references: &[String]) -> anyhow::Result<u8> {
    use zygo_core::supervisor::client::Client;
    use zygo_core::supervisor::protocol::{Request, Response};

    let paths = super::paths(cli);
    let store = Store::new(paths.clone());
    let style = Style::stdout();

    let wanted: Vec<Reference> = references
        .iter()
        .map(|r| {
            r.parse::<Reference>()
                .with_context(|| format!("cannot remove `{r}`"))
        })
        .collect::<anyhow::Result<_>>()?;

    // Asked, never started: a supervisor that is not running holds nothing.
    let warm = match Client::connect(&paths) {
        Ok(mut client) => match client.send(&Request::List)? {
            Response::Functions { functions } => functions,
            _ => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    for reference in &wanted {
        let key = reference.to_string();
        let on_it: Vec<&str> = warm
            .iter()
            .filter(|f| {
                f.image
                    .parse::<Reference>()
                    .is_ok_and(|r| r.to_string() == key)
            })
            .map(|f| f.name.as_str())
            .collect();
        anyhow::ensure!(
            on_it.is_empty(),
            "`{key}` is in use: {} running on it\n  → stop it first: zygo stop {}",
            on_it.join(", "),
            on_it.join(" ")
        );
    }

    let mut removed = Vec::new();
    for reference in &wanted {
        let gone = store.remove(reference)?;
        anyhow::ensure!(
            !gone.is_empty(),
            "no image `{reference}` in the store\n  → `zygo images` lists what is here"
        );
        removed.extend(gone);
    }

    if cli.json {
        output::json(&serde_json::json!({
            "removed": removed.iter().map(|e| e.reference.clone()).collect::<Vec<_>>(),
        }))?;
    } else {
        for entry in &removed {
            println!("{} {}", style.dim("removed"), entry.reference);
        }
    }

    // What only those images kept alive is unreferenced now.
    prune(
        cli,
        &PruneOptions {
            dry_run: false,
            unused_for: None,
            blobs: false,
        },
    )
}

/// What one `prune` was asked to consider.
struct PruneOptions {
    dry_run: bool,
    /// Also collect live venvs and rootfs caches unused for this long.
    unused_for: Option<std::time::Duration>,
    /// Also drop the compressed blob of every unpacked layer.
    blobs: bool,
}

/// One kind of reclaimable thing, and what it costs to lose it.
struct Category {
    /// Plural noun for the summary line.
    name: &'static str,
    /// What is freed, already measured.
    items: Vec<Reclaimable>,
}

struct Reclaimable {
    /// How the item is named on screen: a short digest, or a cache key.
    label: String,
    /// Directory to remove.
    dir: std::path::PathBuf,
    /// A second file to remove with it — a layer's compressed blob.
    blob: Option<std::path::PathBuf>,
    bytes: u64,
}

/// `zygo image prune`.
///
/// Unreferenced layers used to be the whole of it, and they are the smallest
/// part: three images totalling 47 MB of layers left a 230 MB data directory,
/// because a flattened rootfs, a venv and a derived system layer are each
/// larger than the layers they come from and none of them was ever collected.
/// Every cache Zygo writes is reachable from here, and each is reported on its
/// own line so the size that matters is the one the user can see.
fn prune(cli: &Cli, opts: &PruneOptions) -> anyhow::Result<u8> {
    let dry_run = opts.dry_run;
    let paths = super::paths(cli);
    let store = Store::new(paths.clone());

    let layers = Category {
        name: "layers",
        items: store
            .unreferenced_layers()?
            .into_iter()
            .map(|digest| {
                let dir = paths.layers().join(digest.trim_start_matches("sha256:"));
                // The compressed blob is only needed to unpack the layer again.
                let blob = store.blob_path(&digest).ok();
                let bytes = dir_size(&dir)
                    + blob
                        .as_ref()
                        .and_then(|b| b.metadata().ok())
                        .map_or(0, |m| m.len());
                Reclaimable {
                    label: short(&digest),
                    dir,
                    blob,
                    bytes,
                }
            })
            .collect(),
    };

    let cache = |name, dirs: Vec<std::path::PathBuf>| Category {
        name,
        items: dirs
            .into_iter()
            .map(|dir| Reclaimable {
                label: key_of(&dir),
                bytes: dir_size(&dir),
                blob: None,
                dir,
            })
            .collect(),
    };

    // Sandbox roots whose process is gone. `zygo run` removes its own on the
    // way out, and did not always: a run that failed after the directory was
    // made used to leave it behind, so a host that has seen failures has a
    // pile of empty `root-<pid>` directories nothing else will ever collect.
    let abandoned = abandoned_roots(&paths.tmp());

    let mut categories = vec![
        layers,
        cache("flattened rootfs caches", store.unreferenced_flat()?),
        cache("venvs", zygo_core::venv::unreferenced(&store)?),
        cache(
            "system layer records",
            zygo_core::derive::unreferenced(&store)?,
        ),
        cache("abandoned sandbox roots", abandoned),
    ];
    if let Some(window) = opts.unused_for {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(window)
            .unwrap_or(std::time::UNIX_EPOCH);
        categories.push(cache(
            "unused venvs",
            zygo_core::venv::unused_since(&store, cutoff)?,
        ));
        categories.push(cache(
            "unused flattened rootfs caches",
            store.flat_unused_since(cutoff)?,
        ));
    }
    if opts.blobs {
        categories.push(Category {
            name: "compressed layer blobs",
            items: store
                .droppable_blobs()?
                .into_iter()
                .map(|(digest, blob)| Reclaimable {
                    label: short(&digest),
                    bytes: blob.metadata().map_or(0, |m| m.len()),
                    // The blob is the item; there is no directory to remove.
                    dir: blob,
                    blob: None,
                })
                .collect(),
        });
    }

    let total: u64 = categories
        .iter()
        .flat_map(|c| &c.items)
        .map(|i| i.bytes)
        .sum();
    let count: usize = categories.iter().map(|c| c.items.len()).sum();

    if cli.json {
        output::json(&serde_json::json!({
            "dry_run": dry_run,
            "bytes": total,
            "items": count,
            "categories": categories.iter().map(|c| serde_json::json!({
                "name": c.name,
                "count": c.items.len(),
                "bytes": c.items.iter().map(|i| i.bytes).sum::<u64>(),
                "items": c.items.iter().map(|i| i.label.clone()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }))?;
        if !dry_run {
            remove(&categories);
        }
        return Ok(0);
    }

    let style = Style::stdout();
    if count == 0 {
        println!("nothing to prune");
        print_kept(&store, opts, &style);
        return Ok(0);
    }

    for category in &categories {
        for item in &category.items {
            println!(
                "{} {} {}",
                style.dim(if dry_run { "would remove" } else { "removing" }),
                item.label,
                style.dim(&output::human_bytes(item.bytes)),
            );
        }
    }
    if !dry_run {
        remove(&categories);
    }

    for category in &categories {
        if category.items.is_empty() {
            continue;
        }
        let bytes: u64 = category.items.iter().map(|i| i.bytes).sum();
        println!(
            "  {} {} across {} {}",
            if dry_run { "would free" } else { "freed" },
            output::human_bytes(bytes),
            category.items.len(),
            category.name,
        );
    }
    println!(
        "{} {} in total",
        if dry_run { "would free" } else { "freed" },
        output::human_bytes(total)
    );
    print_kept(&store, opts, &style);
    Ok(0)
}

/// What prune left alone and a flag would have taken.
///
/// The size has to be visible before the flag is chosen: "34 MB of venvs
/// kept, the oldest unused for 12 days" makes `--unused-for 7d` a decision
/// rather than a guess, and it is the line that would have answered the
/// second report's "the data directory only grows" without reaching for
/// `du`. Silent when there is nothing to say, and about any category whose
/// flag was given.
fn print_kept(store: &Store, opts: &PruneOptions, style: &Style) {
    let now = std::time::SystemTime::now();

    if opts.unused_for.is_none() {
        // A cutoff of *now* selects every live entry, whatever its age —
        // which is the set any `--unused-for` could ever reach.
        let venvs = zygo_core::venv::unused_since(store, now).unwrap_or_default();
        let flat = store.flat_unused_since(now).unwrap_or_default();
        for (name, dirs) in [("venvs", venvs), ("flattened rootfs caches", flat)] {
            if dirs.is_empty() {
                continue;
            }
            let bytes: u64 = dirs.iter().map(|d| dir_size(d)).sum();
            let age = dirs
                .iter()
                .filter_map(|d| marker_age(d, now))
                .max()
                .map_or_else(|| "an unknown time".to_string(), human_duration);
            println!(
                "{}",
                style.dim(&format!(
                    "  kept {} across {} {name}, the oldest unused for {age} \
                     (`--unused-for <duration>` collects those)",
                    output::human_bytes(bytes),
                    dirs.len(),
                ))
            );
        }
    }

    if !opts.blobs {
        let blobs = store.droppable_blobs().unwrap_or_default();
        if !blobs.is_empty() {
            let bytes: u64 = blobs
                .iter()
                .map(|(_, b)| b.metadata().map_or(0, |m| m.len()))
                .sum();
            println!(
                "{}",
                style.dim(&format!(
                    "  kept {} of compressed blobs beside {} unpacked layers \
                     (`--blobs` drops them)",
                    output::human_bytes(bytes),
                    blobs.len(),
                ))
            );
        }
    }
}

/// `root-<pid>` directories in `tmp/` whose process is no longer running.
///
/// The pid is in the name, so "is this run still going" is answerable
/// without any bookkeeping. A directory whose pid *is* alive is left alone
/// even though that pid may have been recycled onto an unrelated process:
/// skipping one costs a wait until the next prune, and being wrong the other
/// way pulls the ground out from under a running sandbox.
fn abandoned_roots(tmp: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let pid: i32 = name.to_str()?.strip_prefix("root-")?.parse().ok()?;
            let path = entry.path();
            (path.is_dir() && !is_running(pid)).then_some(path)
        })
        .collect();
    out.sort();
    out
}

/// Whether a process with this id exists. Signal 0 asks without sending.
#[cfg(unix)]
fn is_running(pid: i32) -> bool {
    // A pid that is not ours answers `EPERM`, which still means it is there.
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
fn is_running(_pid: i32) -> bool {
    true
}

/// How long ago a cache directory was last used, from its `.zygo-` marker.
fn marker_age(dir: &std::path::Path, now: std::time::SystemTime) -> Option<std::time::Duration> {
    let used = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".zygo-"))
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()?;
    now.duration_since(used).ok()
}

/// `3 days`, `5 hours`, `12 minutes` — the resolution a cache age needs.
fn human_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let (n, unit) = match secs {
        0..=119 => return "under 2 minutes".to_string(),
        120..=7199 => (secs / 60, "minutes"),
        7200..=172_799 => (secs / 3600, "hours"),
        _ => (secs / 86_400, "days"),
    };
    format!("{n} {unit}")
}

/// Best effort, deliberately: a cache entry that will not delete is a cache
/// entry, and failing the command over one would leave the rest behind.
fn remove(categories: &[Category]) {
    for item in categories.iter().flat_map(|c| &c.items) {
        std::fs::remove_dir_all(&item.dir).ok();
        if let Some(blob) = &item.blob {
            std::fs::remove_file(blob).ok();
        }
    }
}

/// A cache directory's own name, shortened the way a digest is.
fn key_of(dir: &std::path::Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().chars().take(12).collect())
        .unwrap_or_else(|| dir.display().to_string())
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
