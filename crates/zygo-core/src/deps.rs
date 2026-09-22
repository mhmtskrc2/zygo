//! Dependency sets built from files that arrived over the API (todo 3.3).
//!
//! [`crate::venv`] builds a venv from a `requirements.txt` **on this host**,
//! named by a spec file the operator wrote. That is the right shape for
//! someone with a shell on the machine and the wrong one for an embedder: an
//! embedder's customers have a `package.json` in a database row, and no way to
//! put a file anywhere. So this is the same idea addressed the other way
//! round — the bytes arrive in a request, the build happens here, and what
//! comes back is an id a runtime pool can name.
//!
//! Two languages, one mechanism. What differs is three lines: which files are
//! accepted, what command builds them, and what environment the result needs.
//!
//! | | Python | Node |
//! |---|---|---|
//! | files | `requirements.txt` | `package.json` + `package-lock.json` |
//! | built by | `pip install` into a venv | `npm ci` |
//! | mounted at | `/venv` | `/venv` |
//! | needs | `PATH=/venv/bin:…` | `NODE_PATH=/venv/node_modules` |
//!
//! # The network, which is the whole security difference
//!
//! `venv.rs` gives its build sandbox **host networking**, and justifies it by
//! saying the requirements file is the operator's own, read from their disk,
//! before any tenant code exists. None of that is true here. These bytes came
//! over HTTP from whoever holds a token, and installing a package *runs* code
//! — a `setup.py`, a build backend, an npm `install` script. A build with host
//! networking would put that code on the Zygo host's own network, where
//! `localhost` is the supervisor and the private ranges are the embedder's
//! infrastructure.
//!
//! So a build here runs with `network = "egress"` and an allowlist of the
//! package registries, and **is refused on a host that cannot do that** rather
//! than falling back. A refusal is a host that needs `passt` and `nftables`
//! installed; a fallback would be an SSRF nobody asked for.
//!
//! # Never on the request path
//!
//! `POST /deps` answers `building` and returns. The build runs in a thread,
//! one at a time, and a pool that names a dependency set which is still
//! building is refused with `503` and a `Retry-After` — not queued, and never
//! started without it. A zygote warmed without the dependencies it was
//! promised would serve requests that fail at `import`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, Result};
use crate::image::{ImageEntry, Store};
use crate::sandbox::oneshot::{run_captured, tail};
use crate::sandbox::{DEFAULT_PATH, SandboxConfig};
use crate::spec::{AllowRule, Bytes, Cpu, Duration, Mount, MountMode, Network, ResolvedFn};

/// Where a built dependency set is mounted inside a sandbox.
///
/// The same path a venv uses, because it is the same kind of thing and one
/// path is one thing to reason about. A function may not have both: `deps` and
/// `requirements` are refused together, rather than silently shadowing.
pub const DEPS_IN_SANDBOX: &str = "/venv";

/// Where the uploaded files are mounted for the build.
const INPUT_IN_SANDBOX: &str = "/deps-input";

/// The most one dependency set's input may be, in bytes.
///
/// A `requirements.txt` is a few kilobytes and a `package-lock.json` for a
/// large project is a few megabytes. This is a cap on the *request*, not on
/// what it builds.
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;

/// The most of a build log that is kept, in bytes.
///
/// From the *end*: a build that failed says why in its last lines, and a
/// resolver that printed ten thousand candidate versions before it gave up
/// should not be able to push that off a caller's screen — or out of this
/// host's disk.
pub const MAX_LOG_BYTES: usize = 256 * 1024;

/// How long a build may take before it is killed.
///
/// Ten minutes: the same budget `venv.rs` gives a `pip install`, which was
/// chosen against a cold wheel cache on a slow link.
const BUILD_TIMEOUT_SECS: u64 = 600;

/// Which language's dependencies these are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Python,
    Node,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Python => "python",
            Kind::Node => "node",
        }
    }

    /// The registries a build of this kind may reach, and nothing else.
    ///
    /// Names rather than addresses, resolved by Zygo's own resolver as the
    /// build asks for them: a CDN's addresses rotate, and a list of addresses
    /// written down today is a build that fails next month.
    fn registries(&self) -> &'static [&'static str] {
        match self {
            // `pypi.org` answers the queries; the files come from the CDN.
            Kind::Python => &["pypi.org:443", "files.pythonhosted.org:443"],
            Kind::Node => &["registry.npmjs.org:443"],
        }
    }

    /// The environment a sandbox needs to find these dependencies.
    pub fn env(&self) -> Vec<(String, String)> {
        match self {
            Kind::Python => crate::venv::Venv::env(),
            // `NODE_PATH` rather than a `node_modules` beside the script: a
            // pool's scripts are loaded from `/run/script/<digest>`, and
            // Node's directory walk from there finds nothing. `NODE_PATH`
            // is consulted after the walk fails, which is exactly this case.
            Kind::Node => vec![(
                "NODE_PATH".to_string(),
                format!("{DEPS_IN_SANDBOX}/node_modules"),
            )],
        }
    }

    /// The same, over a `PATH` that came from the image's own config.
    pub fn env_over(&self, base_path: &str) -> Vec<(String, String)> {
        match self {
            Kind::Python => crate::venv::Venv::env_over(base_path),
            Kind::Node => self.env(),
        }
    }

    /// What builds it, inside the image.
    fn argv(&self) -> Vec<String> {
        let script = match self {
            Kind::Python => format!(
                "python3 -m venv --without-pip {DEPS_IN_SANDBOX} && \
                 {DEPS_IN_SANDBOX}/bin/python3 -m ensurepip --upgrade --default-pip && \
                 {DEPS_IN_SANDBOX}/bin/pip install --no-cache-dir --disable-pip-version-check \
                 --timeout 60 --retries 5 -r {INPUT_IN_SANDBOX}/requirements.txt"
            ),
            // `npm ci` and not `npm install`: it installs the lockfile and
            // refuses when the lockfile and the manifest disagree, which is
            // the difference between a dependency set that is reproducible
            // and one that is whatever npm resolved this morning.
            //
            // `--omit=dev`, because this is a runtime, and `--ignore-scripts`
            // because a package's `postinstall` is arbitrary code that the
            // person who *uploaded* the lockfile chose, not the person
            // running this host. A package that needs its scripts fails here
            // rather than running them.
            Kind::Node => format!(
                "cd {DEPS_IN_SANDBOX} && \
                 cp {INPUT_IN_SANDBOX}/package.json {INPUT_IN_SANDBOX}/package-lock.json . && \
                 npm ci --omit=dev --ignore-scripts --no-audit --no-fund"
            ),
        };
        vec!["sh".to_string(), "-c".to_string(), script]
    }

    fn build_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            // Both package managers want somewhere writable for their own
            // state, and `/tmp` is the sandbox's scratch.
            ("HOME".to_string(), "/tmp".to_string()),
            ("PATH".to_string(), DEFAULT_PATH.to_string()),
        ];
        if *self == Kind::Node {
            env.push(("npm_config_cache".to_string(), "/tmp/npm".to_string()));
            env.push((
                "npm_config_update_notifier".to_string(),
                "false".to_string(),
            ));
        }
        env
    }
}

/// Where a dependency set is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Building,
    Ready,
    Failed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Building => "building",
            State::Ready => "ready",
            State::Failed => "failed",
        }
    }
}

/// What a dependency set is, and how its build went.
///
/// Written to `deps/<id>/status.json` at every transition, and read from there
/// rather than kept only in memory: a supervisor restart must not turn a
/// dependency set that took four minutes to build into one nobody can find.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub kind: Kind,
    /// The image reference this was built against, as asked for.
    pub image: String,
    /// The image's manifest digest, which is what the id is keyed on.
    pub manifest: String,
    pub state: State,
    /// Why it failed, in one line. `None` unless `state` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Which files were uploaded, and how big each was.
    pub files: BTreeMap<String, usize>,
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    /// Which tenants have uploaded these files.
    ///
    /// A **list**, because a dependency set is shared by content like a
    /// script is: two customers who send the same lockfile against the same
    /// image get the same id and one build, and both are entitled to see it
    /// in their own listing. Recording only the first would mean the second
    /// uploaded something they cannot find afterwards.
    ///
    /// Empty is the operator's own. It is not a secret — the id is a hash of
    /// a lockfile — but a listing that showed every customer's would tell each
    /// of them what the others run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tenants: Vec<String>,
}

impl Status {
    /// Whether a pool may be warmed against this.
    pub fn is_ready(&self) -> bool {
        self.state == State::Ready
    }
}

/// The files one `POST /deps` carried, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub kind: Kind,
    pub files: BTreeMap<String, Vec<u8>>,
}

impl Input {
    /// Read a request's files, deciding what they are and refusing what they
    /// are not.
    ///
    /// The language is decided by the file names, which is how every tool that
    /// reads these files decides. A set that is neither — or that is both — is
    /// a refusal naming what was expected, because guessing at it would build
    /// the wrong thing and report success.
    pub fn read(files: BTreeMap<String, Vec<u8>>) -> Result<Input> {
        let total: usize = files.values().map(|f| f.len()).sum();
        if total > MAX_INPUT_BYTES {
            return Err(invalid(format!(
                "these files are {total} bytes and the limit is {MAX_INPUT_BYTES}"
            )));
        }
        for name in files.keys() {
            if name.contains('/') || name.contains("..") || name.trim().is_empty() {
                return Err(invalid(format!(
                    "`{name}` is not a file name; send the files themselves, not paths"
                )));
            }
        }
        let names: Vec<&str> = files.keys().map(|k| k.as_str()).collect();
        let python = names.contains(&"requirements.txt");
        let node = names.contains(&"package.json");

        let kind = match (python, node) {
            (true, false) => Kind::Python,
            (false, true) => Kind::Node,
            (true, true) => {
                return Err(invalid(
                    "both a requirements.txt and a package.json: send one dependency set per \
                     request, so there is one thing for a pool to name"
                        .to_string(),
                ));
            }
            (false, false) => {
                return Err(invalid(format!(
                    "no requirements.txt and no package.json; got {}",
                    if names.is_empty() {
                        "nothing".to_string()
                    } else {
                        names.join(", ")
                    }
                )));
            }
        };
        if kind == Kind::Node && !names.contains(&"package-lock.json") {
            // `npm ci` needs one, and a build that fell back to `npm install`
            // would resolve whatever is newest today — which is not what the
            // caller uploading a manifest asked for.
            return Err(invalid(
                "a package.json with no package-lock.json: send the lockfile too, so that the \
                 dependency set is the one you tested rather than the one npm resolves today"
                    .to_string(),
            ));
        }
        if files.values().all(|f| f.is_empty()) {
            return Err(invalid("every file is empty".to_string()));
        }
        Ok(Input { kind, files })
    }

    /// The id these files have against this image.
    ///
    /// Keyed on the image's *manifest digest* and on every uploaded byte: the
    /// same lockfile against a different image is a different dependency set,
    /// because a wheel built for one interpreter fails at import in another.
    /// Two embedders who send identical files against identical images share
    /// one build, which is the point of a content-addressed key.
    pub fn id(&self, manifest: &str) -> String {
        let mut key = Sha256::new();
        key.update(manifest.as_bytes());
        key.update(b"\n");
        key.update(self.kind.as_str().as_bytes());
        for (name, bytes) in &self.files {
            key.update(b"\n");
            key.update(name.as_bytes());
            key.update(b"\n");
            key.update(bytes);
        }
        format!("deps_{}", &hex::encode(key.finalize())[..32])
    }
}

fn invalid(message: String) -> Error {
    Error::Spec(crate::spec::SpecError::Invalid {
        field: "deps".into(),
        message,
        remedy: String::new(),
    })
}

/// Whether an id could name a dependency set. Checked before it reaches a path.
pub fn valid_id(id: &str) -> bool {
    id.len() == 37
        && id.starts_with("deps_")
        && id[5..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
}

/// Where this dependency set lives on the host.
fn dir(paths: &crate::Paths, id: &str) -> PathBuf {
    paths.deps().join(id)
}

/// The built tree, which is what gets mounted.
pub fn tree(paths: &crate::Paths, id: &str) -> PathBuf {
    dir(paths, id).join("tree")
}

/// The mount that puts a built dependency set into a sandbox.
pub fn mount(paths: &crate::Paths, id: &str) -> Mount {
    Mount {
        source: tree(paths, id),
        target: PathBuf::from(DEPS_IN_SANDBOX),
        // Read-only, for the reason a venv is: one build is shared by every
        // sandbox that names it, including sandboxes belonging to other
        // tenants.
        mode: MountMode::Ro,
    }
}

/// Every dependency set this host holds.
pub fn list(paths: &crate::Paths) -> Vec<Status> {
    let Ok(entries) = std::fs::read_dir(paths.deps()) else {
        return Vec::new();
    };
    let mut out: Vec<Status> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter_map(|id| status(paths, &id))
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// What this host knows about one dependency set.
pub fn status(paths: &crate::Paths, id: &str) -> Option<Status> {
    if !valid_id(id) {
        return None;
    }
    let raw = std::fs::read(dir(paths, id).join("status.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// The build's output, from the end.
pub fn log(paths: &crate::Paths, id: &str) -> String {
    if !valid_id(id) {
        return String::new();
    }
    let Ok(raw) = std::fs::read(dir(paths, id).join("log")) else {
        return String::new();
    };
    let from = raw.len().saturating_sub(MAX_LOG_BYTES);
    String::from_utf8_lossy(&raw[from..]).into_owned()
}

/// Forget a dependency set entirely.
///
/// The caller decides whether anything references it; this only removes.
pub fn remove(paths: &crate::Paths, id: &str) -> Result<()> {
    if !valid_id(id) {
        return Err(invalid(format!("`{id}` is not a dependency set id")));
    }
    let dir = dir(paths, id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).at(&dir)?;
    }
    Ok(())
}

/// Record a dependency set as `building`, with its input on disk.
///
/// Returns `None` when this host already has it — the same files against the
/// same image are the same build, whether it is finished, running or failed.
/// A *failed* one is returned as it is rather than rebuilt: a build that
/// failed on a typo will fail again, and the caller has the log to read.
pub fn begin(
    paths: &crate::Paths,
    input: &Input,
    image: &ImageEntry,
    tenant: Option<&str>,
) -> Result<(String, Option<Status>)> {
    let id = input.id(&image.manifest);
    if let Some(mut existing) = status(paths, &id) {
        // Shared by content, so this caller joins it rather than building a
        // second copy — and is added to its owners, or they would have
        // uploaded something they cannot see afterwards.
        if let Some(tenant) = tenant
            && !existing.tenants.iter().any(|t| t == tenant)
        {
            existing.tenants.push(tenant.to_string());
            existing.tenants.sort();
            write_status(paths, &existing)?;
        }
        return Ok((id, Some(existing)));
    }

    let dir = dir(paths, &id);
    // A directory with no status is a build whose supervisor died between
    // `create_dir_all` and the first write. Start over rather than trust it.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).at(&dir)?;
    }
    let input_dir = dir.join("input");
    std::fs::create_dir_all(&input_dir).at(&input_dir)?;
    std::fs::create_dir_all(dir.join("tree")).at(&dir)?;
    for (name, bytes) in &input.files {
        let path = input_dir.join(name);
        std::fs::write(&path, bytes).at(&path)?;
    }

    let status = Status {
        id: id.clone(),
        kind: input.kind,
        image: image.reference.clone(),
        manifest: image.manifest.clone(),
        state: State::Building,
        error: None,
        files: input
            .files
            .iter()
            .map(|(name, bytes)| (name.clone(), bytes.len()))
            .collect(),
        started_ms: now_ms(),
        finished_ms: None,
        tenants: tenant.map(str::to_string).into_iter().collect(),
    };
    write_status(paths, &status)?;
    Ok((id, None))
}

/// Build what `begin` recorded. Blocking; run it off the request path.
///
/// Always leaves a terminal state behind: a build that fails, or a host that
/// cannot start a sandbox at all, both end as `failed` with the reason in the
/// status and the output in the log. Nothing is left saying `building` except
/// a build that is running.
pub fn build(paths: &crate::Paths, id: &str) -> Result<Status> {
    let Some(mut status) = status(paths, id) else {
        return Err(invalid(format!(
            "`{id}` is not a dependency set on this host"
        )));
    };
    let store = Store::new(paths.clone());
    let outcome = run_build(&store, &status);

    status.finished_ms = Some(now_ms());
    match outcome {
        Ok((0, output)) => {
            write_log(paths, id, &output)?;
            status.state = State::Ready;
            tracing::info!(
                deps = id,
                kind = status.kind.as_str(),
                seconds = (status.finished_ms.unwrap_or_default() - status.started_ms) / 1000,
                "dependency set built"
            );
        }
        Ok((code, output)) => {
            write_log(paths, id, &output)?;
            status.state = State::Failed;
            status.error = Some(format!(
                "the build exited {code}; the last of its output:\n{}",
                tail(&output, 20)
            ));
        }
        Err(e) => {
            write_log(paths, id, e.to_string().as_bytes())?;
            status.state = State::Failed;
            status.error = Some(e.to_string());
        }
    }
    write_status(paths, &status)?;
    Ok(status)
}

/// One build, in a sandbox, with the registries and nothing else.
fn run_build(store: &Store, status: &Status) -> Result<(i32, Vec<u8>)> {
    let paths = store.paths();
    let reference: crate::image::Reference = status
        .image
        .parse()
        .map_err(|e| invalid(format!("`{}` is not an image reference: {e}", status.image)))?;
    let Some(image) = store.get(&reference) else {
        return Err(Error::Spec(crate::spec::SpecError::Invalid {
            field: "deps.image".into(),
            message: format!("`{}` is not in this host's image store", status.image),
            remedy: "\n  → pull it first; the API does not fetch images on a caller's behalf"
                .into(),
        }));
    };
    if image.manifest != status.manifest {
        // The reference moved under us between `begin` and here. The id names
        // the manifest, so building against the new one would put something
        // else behind an id somebody already holds.
        return Err(invalid(format!(
            "`{}` now resolves to {} and this dependency set is keyed on {}; \
             upload it again to build against the new image",
            status.image, image.manifest, status.manifest
        )));
    }

    let mut spec = build_spec(&image.reference, status, paths)?;
    let net = crate::net::setup(paths, "deps-build", &spec)?;
    if let Some(mount) = net.mount.clone() {
        spec.mounts.push(mount);
    }
    for w in &net.warnings {
        tracing::warn!("{w}");
    }

    let mount_points = crate::sandbox::mount::required_mount_points(&spec.mounts);
    let overlay = crate::doctor::cached(paths)
        .checks
        .iter()
        .any(|c| c.name == "overlayfs (userns)" && c.status == crate::doctor::Status::Ok);
    let view = store.rootfs_view(&image.layers, overlay, &mount_points)?;

    let newroot = paths
        .tmp()
        .join(format!("deps-build-{}-{}", std::process::id(), status.id));
    std::fs::create_dir_all(&newroot).at(&newroot)?;

    let mut config = SandboxConfig::from_resolved(
        &spec,
        &view,
        &newroot,
        status.kind.argv(),
        &status.kind.build_env(),
    );
    config.allow_resolved = net.allowed;
    config.pasta_pid_file = net.pid_file;

    tracing::info!(
        deps = status.id,
        kind = status.kind.as_str(),
        image = %image.reference,
        "building a dependency set inside the image (egress to the registries only)"
    );
    let outcome = run_captured(&mut config, paths);
    let _ = std::fs::remove_dir_all(&newroot);
    outcome
}

/// The one-shot function that performs the build.
fn build_spec(image: &str, status: &Status, paths: &crate::Paths) -> Result<ResolvedFn> {
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    let allow: Vec<AllowRule> = status
        .kind
        .registries()
        .iter()
        .map(|r| r.parse())
        .collect::<std::result::Result<_, _>>()
        .expect("the registry list is fixed and valid");

    let mut spec = resolve_standalone(
        "deps-build",
        &Layer {
            image: Some(image.to_string()),
            cmd: Some(status.kind.argv()),
            // Resolving and compiling is not a 256 MB job.
            mem: Some(Bytes::from_mib(1024)),
            cpu: Some(Cpu(2.0)),
            pids: Some(256),
            timeout: Some(Duration::from_secs(BUILD_TIMEOUT_SECS)),
            scratch: Some(Bytes::from_mib(512)),
            network: Some(Network::Egress),
            allow: Some(allow),
            ..Default::default()
        },
        &ResolveOptions {
            one_shot: true,
            ..Default::default()
        },
    )
    .map_err(Error::Spec)?;

    spec.mounts = vec![
        Mount {
            source: tree(paths, &status.id),
            target: PathBuf::from(DEPS_IN_SANDBOX),
            mode: MountMode::Rw,
        },
        Mount {
            source: dir(paths, &status.id).join("input"),
            target: PathBuf::from(INPUT_IN_SANDBOX),
            mode: MountMode::Ro,
        },
    ];
    Ok(spec)
}

fn write_status(paths: &crate::Paths, status: &Status) -> Result<()> {
    let dir = dir(paths, &status.id);
    std::fs::create_dir_all(&dir).at(&dir)?;
    let path = dir.join("status.json");
    let tmp = dir.join("status.json.tmp");
    let body = serde_json::to_vec_pretty(status).expect("a status serialises");
    std::fs::write(&tmp, body).at(&tmp)?;
    // Renamed rather than written in place: a reader that arrives mid-write
    // would otherwise see half a JSON object and decide this dependency set
    // does not exist.
    std::fs::rename(&tmp, &path).at(&path)
}

fn write_log(paths: &crate::Paths, id: &str, output: &[u8]) -> Result<()> {
    let from = output.len().saturating_sub(MAX_LOG_BYTES);
    let path = dir(paths, id).join("log");
    std::fs::write(&path, &output[from..]).at(&path)
}

/// Turn every `building` on disk into a `failed`.
///
/// Called once at start-up. A build belongs to the supervisor that started it:
/// if that process is gone, nothing is going to finish this one, and leaving
/// it `building` would mean a pool that waits for ever on a `503` telling it
/// to try again. The files are still there, so uploading them again starts a
/// real build.
pub fn fail_interrupted(paths: &crate::Paths) -> usize {
    let mut failed = 0;
    for mut status in list(paths) {
        if status.state != State::Building {
            continue;
        }
        status.state = State::Failed;
        status.finished_ms = Some(now_ms());
        status.error = Some(
            "the supervisor that was building this went away; upload the files again to \
             build it"
                .to_string(),
        );
        if write_status(paths, &status).is_ok() {
            failed += 1;
        }
    }
    failed
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Bytes on the wire, as JSON carries them: base64 per file.
pub fn decode_files(files: &BTreeMap<String, String>) -> Result<BTreeMap<String, Vec<u8>>> {
    use base64::Engine as _;
    let mut out = BTreeMap::new();
    for (name, encoded) in files {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| invalid(format!("`{name}` is not base64: {e}")))?;
        out.insert(name.clone(), bytes);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn the_language_comes_from_the_file_names() {
        let python = Input::read(files(&[("requirements.txt", "requests==2.32\n")])).unwrap();
        assert_eq!(python.kind, Kind::Python);

        let node = Input::read(files(&[
            ("package.json", "{\"dependencies\":{}}"),
            ("package-lock.json", "{\"lockfileVersion\":3}"),
        ]))
        .unwrap();
        assert_eq!(node.kind, Kind::Node);
    }

    #[test]
    fn a_package_json_without_its_lockfile_is_refused() {
        // `npm ci` needs one, and falling back to `npm install` would install
        // whatever is newest rather than what the caller tested.
        let e = Input::read(files(&[("package.json", "{}")])).unwrap_err();
        assert!(e.to_string().contains("lockfile"), "{e}");
    }

    #[test]
    fn files_that_are_neither_or_both_are_refused_by_name() {
        let neither = Input::read(files(&[("Gemfile", "source 'https://rubygems.org'\n")]))
            .unwrap_err()
            .to_string();
        assert!(neither.contains("Gemfile"), "{neither}");

        let both = Input::read(files(&[
            ("requirements.txt", "requests\n"),
            ("package.json", "{}"),
            ("package-lock.json", "{}"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(both.contains("one dependency set per request"), "{both}");
    }

    #[test]
    fn a_name_that_is_a_path_is_refused() {
        // These names become file names under `deps/<id>/input`, so a `..` in
        // one is a write wherever the caller likes.
        for name in ["../requirements.txt", "a/requirements.txt", " "] {
            assert!(
                Input::read(files(&[(name, "x")])).is_err(),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn the_id_keys_on_the_image_and_on_every_byte() {
        let input = Input::read(files(&[("requirements.txt", "requests==2.32\n")])).unwrap();
        let other = Input::read(files(&[("requirements.txt", "requests==2.31\n")])).unwrap();

        let a = input.id("sha256:aaa");
        assert_eq!(a, input.id("sha256:aaa"), "identical inputs share a build");
        assert_ne!(
            a,
            input.id("sha256:bbb"),
            "the same requirements against another image are another dependency set"
        );
        assert_ne!(a, other.id("sha256:aaa"), "an edit invalidates");
        assert!(valid_id(&a), "{a}");
    }

    #[test]
    fn an_id_that_could_reach_a_path_is_not_a_valid_id() {
        assert!(!valid_id("deps_../../etc"));
        assert!(!valid_id("deps_"));
        assert!(!valid_id(&format!("deps_{}", "A".repeat(32))));
        assert!(valid_id(&format!("deps_{}", "a1".repeat(16))));
    }

    #[test]
    fn a_build_reaches_the_registries_and_nothing_else() {
        // The whole security difference between this and `venv.rs`: these
        // files came from whoever holds a token, and installing a package runs
        // that package's code.
        for kind in [Kind::Python, Kind::Node] {
            let status = Status {
                id: format!("deps_{}", "a".repeat(32)),
                kind,
                image: "python:3.12-slim".into(),
                manifest: "sha256:aaa".into(),
                state: State::Building,
                error: None,
                files: BTreeMap::new(),
                started_ms: 0,
                finished_ms: None,
                tenants: Vec::new(),
            };
            let paths = crate::Paths::rooted("/tmp/zygo-test");
            let spec = build_spec("python:3.12-slim", &status, &paths).unwrap();
            assert_eq!(spec.network, Network::Egress, "{kind:?}");
            let allow: Vec<String> = spec.allow.iter().map(|a| a.to_string()).collect();
            assert_eq!(allow.len(), kind.registries().len(), "{kind:?}");
            assert!(
                allow.iter().all(|a| a.ends_with(":443")),
                "{allow:?} should be https only"
            );
        }
    }

    #[test]
    fn the_built_tree_is_mounted_read_only() {
        // One build is shared by every sandbox that names it, including other
        // tenants'.
        let paths = crate::Paths::rooted("/tmp/zygo-test");
        let m = mount(&paths, "deps_abc");
        assert_eq!(m.mode, MountMode::Ro);
        assert_eq!(m.target, PathBuf::from(DEPS_IN_SANDBOX));
    }

    #[test]
    fn node_gets_a_node_path_and_python_gets_the_venv() {
        let node = Kind::Node.env();
        assert!(
            node.iter()
                .any(|(k, v)| k == "NODE_PATH" && v == "/venv/node_modules"),
            "{node:?}"
        );
        let python = Kind::Python.env();
        assert!(python.iter().any(|(k, _)| k == "VIRTUAL_ENV"), "{python:?}");
        assert!(
            python
                .iter()
                .any(|(k, v)| k == "PATH" && v.starts_with("/venv/bin:")),
            "{python:?}"
        );
    }

    #[test]
    fn a_log_is_kept_from_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::Paths::rooted(dir.path());
        let id = format!("deps_{}", "b".repeat(32));
        std::fs::create_dir_all(super::dir(&paths, &id)).unwrap();
        let long = vec![b'x'; MAX_LOG_BYTES + 1000];
        write_log(&paths, &id, &long).unwrap();
        assert_eq!(log(&paths, &id).len(), MAX_LOG_BYTES);
    }

    #[test]
    fn an_interrupted_build_is_failed_rather_than_left_building() {
        // A `building` that nothing is building is a pool waiting for ever on
        // a `503` telling it to try again.
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::Paths::rooted(dir.path());
        let id = format!("deps_{}", "c".repeat(32));
        let status = Status {
            id: id.clone(),
            kind: Kind::Python,
            image: "python:3.12-slim".into(),
            manifest: "sha256:aaa".into(),
            state: State::Building,
            error: None,
            files: BTreeMap::new(),
            started_ms: 1,
            finished_ms: None,
            tenants: Vec::new(),
        };
        write_status(&paths, &status).unwrap();

        assert_eq!(fail_interrupted(&paths), 1);
        let after = super::status(&paths, &id).unwrap();
        assert_eq!(after.state, State::Failed);
        assert!(after.error.unwrap().contains("went away"));

        // And a second pass finds nothing left to fail.
        assert_eq!(fail_interrupted(&paths), 0);
    }

    #[test]
    fn a_status_is_read_back_as_it_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::Paths::rooted(dir.path());
        let id = format!("deps_{}", "d".repeat(32));
        let status = Status {
            id: id.clone(),
            kind: Kind::Node,
            image: "node:22-slim".into(),
            manifest: "sha256:bbb".into(),
            state: State::Ready,
            error: None,
            files: [("package.json".to_string(), 42)].into_iter().collect(),
            started_ms: 7,
            finished_ms: Some(9),
            tenants: vec!["acme".into()],
        };
        write_status(&paths, &status).unwrap();
        assert_eq!(super::status(&paths, &id).unwrap(), status);
        assert_eq!(list(&paths).len(), 1);

        remove(&paths, &id).unwrap();
        assert!(super::status(&paths, &id).is_none());
        assert!(list(&paths).is_empty());
    }
}
