//! Turning layers into a runnable description, and refusing to run the ones
//! that cannot be safe.
//!
//! Two principles drive the rules here:
//!
//! - **P7 — limits are mandatory.** Every limit has a built-in default; there
//!   is no path to an unlimited sandbox without `--allow-unlimited`.
//! - **P6 — secure by default.** Anything that widens the boundary (host
//!   network, private-range egress, writable mounts) has to be spelled out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::types::*;
use super::{Layer, Spec, SpecError};
use crate::sandbox::limits::Limits;

/// Built-in defaults (design doc §3.5). The bottom layer of the merge stack.
pub fn defaults() -> Layer {
    Layer {
        isolation: Some(Isolation::Ns),
        seccomp: Some(SeccompProfile::Default),
        mem: Some(Bytes::from_mib(256)),
        cpu: Some(Cpu(1.0)),
        pids: Some(64),
        timeout: Some(Duration::from_secs(30)),
        // `scratch` is deliberately absent: its default depends on `mem`, so
        // it is decided in `resolve_layer` where `mem` is known. See
        // `default_scratch`.
        nofile: Some(1024),
        connections: Some(256),
        network: Some(Network::None),
        mode: Some(HandlerMode::Function),
        concurrency: Some(4),
        idle_timeout: Some(Duration::from_secs(600)),
        cold_after: Some(Duration::from_secs(3600)),
        user: Some("1000".to_string()),
        workdir: Some(PathBuf::from("/app")),
        ..Default::default()
    }
}

/// Knobs that only a CLI flag can set, because each one removes a guarantee.
#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// `--allow-host-net`: permit `network = "host"`.
    pub allow_host_net: bool,
    /// `--allow-private-net`: permit RFC1918 / link-local egress targets.
    pub allow_private_net: bool,
    /// `--allow-unlimited`: permit limits to be disabled.
    pub allow_unlimited: bool,
    /// Directory relative paths resolve against. Defaults to the spec's
    /// directory, or the process working directory when there is no spec.
    pub base_dir: Option<PathBuf>,
    /// This is a one-shot `zygo run`, not a served function.
    ///
    /// Suppresses warnings that only matter when tenants share a host — a
    /// single sandbox cannot be unfair to neighbours it does not have, and a
    /// warning printed on every invocation of the default configuration is
    /// noise that trains people to ignore the ones that matter.
    pub one_shot: bool,
}

/// A fully resolved function: no optional fields, every limit decided.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedFn {
    pub name: String,
    pub image: String,

    /// Handler file, made absolute. `None` for warm-exec functions.
    pub entry: Option<PathBuf>,
    /// Command for warm-exec. Empty when an agent runtime owns the process.
    pub cmd: Vec<String>,
    pub mode: HandlerMode,
    /// `None` means warm-exec: no agent, spawn `cmd` per request.
    pub runtime: Option<Runtime>,
    pub requirements: Option<PathBuf>,
    pub workdir: PathBuf,
    pub user: String,

    pub system: Vec<String>,
    pub nix: Vec<String>,

    pub isolation: Isolation,
    pub seccomp: SeccompProfile,
    pub limits: Limits,

    pub network: Network,
    pub allow: Vec<AllowRule>,
    /// Whether `--allow-private-net` was given. Part of the resolved function
    /// rather than of the launcher's options because it decides what the
    /// egress ruleset contains, and because a deploy that adds it is a change
    /// the supervisor must notice.
    pub allow_private_net: bool,

    pub mounts: Vec<Mount>,
    pub env: BTreeMap<String, String>,
    pub secrets: Vec<String>,

    pub concurrency: u32,
    pub idle_timeout: Duration,
    pub cold_after: Duration,

    /// Non-fatal problems worth telling the user about. The CLI prints these;
    /// they never block a run.
    pub warnings: Vec<String>,
}

/// Smallest memory limit that can start an interpreter. Below this the sandbox
/// is OOM-killed before it does anything, which reads as a mysterious hang.
const MIN_MEM: Bytes = Bytes(8 * 1024 * 1024);

/// Writable `/tmp` when nothing asked for a particular size.
///
/// 64 MiB is the documented default and what every ordinary sandbox gets, but
/// it is a ceiling rather than a constant: tmpfs pages are billed to the
/// memory cgroup, so on a small `mem` a 64 MiB `/tmp` is most of the budget
/// and, at or below 64 MiB of `mem`, an outright contradiction. Half of `mem`
/// is the cap, which is also the point at which an explicit `scratch` starts
/// warning — so the default never warns about itself.
const DEFAULT_SCRATCH: Bytes = Bytes(64 * 1024 * 1024);

fn default_scratch(mem: Bytes) -> Bytes {
    Bytes(DEFAULT_SCRATCH.get().min(mem.get() / 2))
}

/// Default image per built-in runtime, used when the spec names none.
fn default_image_for(runtime: Option<&Runtime>) -> Option<&'static str> {
    match runtime {
        Some(Runtime::Builtin(BuiltinRuntime::Python)) => Some("python:3.12-slim"),
        Some(Runtime::Builtin(BuiltinRuntime::Node)) => Some("node:22-slim"),
        _ => None,
    }
}

impl Spec {
    /// Resolve a named function (or, with `None`, just `[defaults]` plus
    /// overrides — the `zygo run` path) into something runnable.
    pub fn resolve(
        &self,
        name: Option<&str>,
        overrides: &Layer,
        opts: &ResolveOptions,
    ) -> Result<ResolvedFn, SpecError> {
        let fn_layer = match name {
            Some(n) => self.functions.get(n).cloned().ok_or_else(|| {
                let known: Vec<&str> = self.function_names().collect();
                SpecError::unknown_function(n, self.source.as_deref(), &known)
            })?,
            None => Layer::default(),
        };

        let merged = defaults()
            .merge(&self.defaults)
            .merge(&fn_layer)
            .merge(overrides);

        let base_dir = opts.base_dir.clone().unwrap_or_else(|| self.base_dir());

        resolve_layer(name.unwrap_or("run"), merged, &base_dir, opts)
    }

    /// Resolve for `zygo serve`, where the name is a registration rather than a
    /// lookup.
    ///
    /// [`Spec::resolve`] rejects a name the spec does not declare, which is what
    /// `zygo up` and `zygo spec explain` want: there, an unknown name is a typo
    /// and saying so is the whole value. `zygo serve handler.py --name double`
    /// is the other case — the flags are the definition and the name is chosen
    /// on the spot. A spec entry under the same name is still honoured, so a
    /// declared function can be re-served with one flag changed.
    pub fn resolve_for_serve(
        &self,
        name: &str,
        overrides: &Layer,
        opts: &ResolveOptions,
    ) -> Result<ResolvedFn, SpecError> {
        let fn_layer = self.functions.get(name).cloned().unwrap_or_default();
        let merged = defaults()
            .merge(&self.defaults)
            .merge(&fn_layer)
            .merge(overrides);
        let base_dir = opts.base_dir.clone().unwrap_or_else(|| self.base_dir());
        resolve_layer(name, merged, &base_dir, opts)
    }
}

/// Resolve a merged layer without a spec file behind it.
pub fn resolve_standalone(
    name: &str,
    overrides: &Layer,
    opts: &ResolveOptions,
) -> Result<ResolvedFn, SpecError> {
    let merged = defaults().merge(overrides);
    let base = opts
        .base_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    resolve_layer(name, merged, &base, opts)
}

fn resolve_layer(
    name: &str,
    l: Layer,
    base_dir: &Path,
    opts: &ResolveOptions,
) -> Result<ResolvedFn, SpecError> {
    let field = |suffix: &str| format!("fn.{name}.{suffix}");
    let mut warnings = Vec::new();

    // The name before anything else, because everything after this joins it
    // into a path. `cgroup::sanitise` exists for exactly this reason and only
    // the cgroup hierarchy goes through it: `Paths::tenant_data`,
    // `Paths::agent_sock` and the pasta pid file all join the name raw, and
    // `[fn."../x"]` is legal TOML while `--name` is free text (B-07,
    // the code review). One check here covers every entry point, because
    // every one of them resolves before it builds a path.
    if !is_fn_name(name) {
        return Err(SpecError::invalid_with(
            format!("fn.{name}"),
            format!("`{name}` is not a usable function name"),
            "use letters, digits, `-`, `_` and `.`, starting with a letter or \
             digit; the name becomes a directory, a socket and a cgroup",
        ));
    }

    // --- what to run -------------------------------------------------------
    let entry = l.entry.map(|e| absolutise(base_dir, &e));

    // `runtime` is explicit, else inferred from the entry extension, else
    // absent — which means warm-exec (design doc §3.4 layer 1).
    let runtime = match (&l.runtime, &entry) {
        (Some(r), _) => Some(r.clone()),
        (None, Some(e)) => {
            let inferred = Runtime::infer_from_entry(e);
            if inferred.is_none() {
                return Err(SpecError::invalid_with(
                    field("entry"),
                    format!(
                        "cannot infer a runtime from `{}`",
                        e.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    "set `runtime` explicitly, or use `cmd` for warm-exec",
                ));
            }
            inferred
        }
        (None, None) => None,
    };

    let cmd = l.cmd.unwrap_or_default();

    // A served function that says nothing about what to run is a spec bug, and
    // the earlier it is said the better. A one-shot `zygo run alpine:3` is the
    // opposite: an empty command means "whatever the image runs by default",
    // exactly as `docker run alpine:3` does. The image config is not readable
    // from here — it lives in the store, behind a pull — so the resolver lets
    // the empty command through and `zygo run` fills it in from the image's
    // entrypoint and cmd. An image that declares neither still fails there,
    // with the name of the image that let the user down.
    if entry.is_none() && cmd.is_empty() && !opts.one_shot {
        return Err(SpecError::invalid_with(
            format!("fn.{name}"),
            "nothing to run",
            "set `entry` for an agent runtime, or `cmd` for warm-exec",
        ));
    }
    if entry.is_some() && !cmd.is_empty() {
        return Err(SpecError::invalid_with(
            format!("fn.{name}"),
            "`entry` and `cmd` are mutually exclusive",
            "`entry` runs through a runtime agent; `cmd` is warm-exec. Pick one.",
        ));
    }
    // A built-in runtime needs a handler file to import; a third-party agent
    // supplies its own entry point, so it is allowed to stand alone.
    if entry.is_none() && matches!(runtime, Some(Runtime::Builtin(_))) {
        return Err(SpecError::invalid_with(
            field("runtime"),
            "a built-in runtime needs a handler",
            "set `entry`, or drop `runtime` to use warm-exec with `cmd`",
        ));
    }

    let image = match l.image {
        Some(i) => i,
        None => default_image_for(runtime.as_ref())
            .map(str::to_string)
            .ok_or_else(|| {
                SpecError::invalid_with(
                    field("image"),
                    "no image given and none can be inferred",
                    "set `image`, e.g. `image = \"python:3.12-slim\"`",
                )
            })?,
    };

    // --- limits ------------------------------------------------------------
    // `unwrap`s below are safe: `defaults()` sets each of these fields, and
    // `merge` only ever replaces a `Some` with another `Some`.
    let mem = l.mem.expect("mem has a built-in default");
    if mem < MIN_MEM {
        return Err(SpecError::invalid_with(
            field("mem"),
            format!("{mem} is below the {MIN_MEM} minimum"),
            "anything smaller is OOM-killed before the process starts",
        ));
    }

    // An unset `scratch` follows `mem` down. A flat 64M default meant that
    // `zygo run --mem 64M` — one flag, nothing else said — was refused for a
    // conflict the user had not caused and could not see, and the fix was to
    // pass a second flag they had no reason to know about.
    let scratch = l.scratch.unwrap_or_else(|| default_scratch(mem));
    // tmpfs pages are billed to the memory cgroup (design doc §3.5), so a
    // scratch area at or above the memory limit is a self-inflicted OOM.
    // Reachable only when `scratch` was asked for: the derived default is
    // half of `mem` and cannot collide with it.
    if scratch >= mem {
        return Err(SpecError::invalid_with(
            field("scratch"),
            format!("scratch ({scratch}) must be smaller than mem ({mem})"),
            format!(
                "tmpfs pages count against `mem`; filling /tmp would OOM the sandbox. \
                 Give scratch less than {mem} (unset, it would be {}), or raise mem",
                default_scratch(mem)
            ),
        ));
    }
    if scratch.get() > mem.scaled(0.5).get() {
        warnings.push(format!(
            "{}: scratch ({scratch}) is over half of mem ({mem}); a full /tmp leaves little room to run",
            field("scratch")
        ));
    }

    let pids = l.pids.expect("pids has a built-in default");
    if pids == 0 {
        return Err(SpecError::invalid_with(
            field("pids"),
            "must be at least 1",
            "`pids.max` is the fork-bomb limit; it cannot be zero",
        ));
    }

    let timeout = l.timeout.expect("timeout has a built-in default");
    if timeout.as_millis() == 0 && !opts.allow_unlimited {
        return Err(SpecError::invalid_with(
            field("timeout"),
            "a zero timeout means no wall-clock limit",
            "pass --allow-unlimited to accept a sandbox that can run forever",
        ));
    }

    let concurrency = l.concurrency.expect("concurrency has a built-in default");
    if concurrency == 0 {
        return Err(SpecError::invalid(
            field("concurrency"),
            "must be at least 1",
        ));
    }

    let idle_timeout = l.idle_timeout.expect("idle_timeout has a built-in default");
    let cold_after = l.cold_after.expect("cold_after has a built-in default");
    if cold_after < idle_timeout {
        warnings.push(format!(
            "{}: cold_after ({cold_after}) is shorter than idle_timeout ({idle_timeout}); the sandbox goes cold without ever pausing",
            field("cold_after")
        ));
    }

    if l.io_read.is_none() && l.io_write.is_none() && !opts.one_shot {
        // Design doc §3.5 marks disk I/O as "unlimited (warning)". It is about
        // fairness between tenants, so it is pointless for a one-shot run.
        warnings.push(format!(
            "{}: no disk I/O limit; one tenant can saturate the device for the others",
            field("io_read")
        ));
    }

    let connections = l.connections.expect("connections has a built-in default");
    if connections == 0 {
        return Err(SpecError::invalid_with(
            field("connections"),
            "must be at least 1",
            "a networked sandbox that may open no connections is `network = \"none\"`",
        ));
    }
    if let Some(bandwidth) = l.bandwidth
        && bandwidth.get() == 0
    {
        return Err(SpecError::invalid(
            field("bandwidth"),
            "a zero bandwidth blocks the network; use `network = \"none\"` for that",
        ));
    }

    let limits = Limits {
        mem,
        mem_high: mem.scaled(0.9),
        swap: Bytes(0),
        cpu: l.cpu.expect("cpu has a built-in default"),
        pids,
        timeout,
        scratch,
        scratch_inodes: 10_000,
        io_read: l.io_read,
        io_write: l.io_write,
        nofile: l.nofile.expect("nofile has a built-in default"),
        fsize: scratch,
        oom_group: true,
        connections,
        bandwidth: l.bandwidth,
    };

    // --- network -----------------------------------------------------------
    let network = l.network.expect("network has a built-in default");
    if l.bandwidth.is_none() && matches!(network, Network::Egress | Network::Full) && !opts.one_shot
    {
        // The same standing as the disk I/O warning: fairness between
        // tenants, so a one-shot run has no use for it.
        warnings.push(format!(
            "{}: no bandwidth limit; one tenant can saturate the link for the others",
            field("bandwidth")
        ));
    }
    if network == Network::Host && !opts.allow_host_net {
        return Err(SpecError::invalid_with(
            field("network"),
            "`host` removes the network namespace entirely",
            "pass --allow-host-net if that is really what you want",
        ));
    }

    let allow = l.allow.unwrap_or_default();
    if !allow.is_empty() && network != Network::Egress {
        return Err(SpecError::invalid_with(
            field("allow"),
            format!("an allowlist has no effect with `network = \"{network}\"`"),
            "set `network = \"egress\"`, or remove `allow`",
        ));
    }
    if network == Network::Egress && allow.is_empty() {
        warnings.push(format!(
            "{}: egress with an empty allowlist denies everything",
            field("allow")
        ));
    }
    if !opts.allow_private_net {
        for rule in &allow {
            if let HostPattern::Cidr(c) = &rule.host
                && c.is_private_or_link_local()
            {
                return Err(SpecError::invalid_with(
                    field("allow"),
                    format!("`{rule}` targets a private or link-local range"),
                    "these reach the host and its neighbours; pass --allow-private-net to permit it",
                ));
            }
        }
    }
    // --- mounts ------------------------------------------------------------
    let mut mounts = Vec::new();
    let mut seen_targets: BTreeMap<PathBuf, ()> = BTreeMap::new();
    for m in l.mounts.unwrap_or_default() {
        if is_reserved_mount_target(&m.target) {
            return Err(SpecError::invalid_with(
                field("mounts"),
                format!("`{}` is managed by the sandbox", m.target.display()),
                "/, /proc, /sys and /dev are set up by the launcher and cannot be overridden",
            ));
        }
        if seen_targets.insert(m.target.clone(), ()).is_some() {
            return Err(SpecError::invalid(
                field("mounts"),
                format!("duplicate mount target `{}`", m.target.display()),
            ));
        }
        mounts.push(Mount {
            source: absolutise(base_dir, &m.source),
            target: m.target,
            mode: m.mode,
        });
    }

    // --- env and secrets ---------------------------------------------------
    let env = l.env.unwrap_or_default();
    for key in env.keys() {
        if !is_env_name(key) {
            return Err(SpecError::invalid(
                field("env"),
                format!("`{key}` is not a valid environment variable name"),
            ));
        }
    }
    let secrets = l.secrets.unwrap_or_default();
    for s in &secrets {
        if !is_env_name(s) {
            return Err(SpecError::invalid(
                field("secrets"),
                format!("`{s}` is not a valid environment variable name"),
            ));
        }
        if env.contains_key(s) {
            return Err(SpecError::invalid_with(
                field("secrets"),
                format!("`{s}` is listed in both `env` and `secrets`"),
                "secrets are delivered as files to the child only; `env` is visible to the zygote",
            ));
        }
    }

    // --- derived layers -----------------------------------------------------
    // Validated here, filesystem-free, so `zygo spec explain` rejects a bad
    // package name before any image is copied for the install.
    let system = l.system.unwrap_or_default();
    for p in &system {
        if let Err(message) = crate::derive::validate_package(p) {
            return Err(SpecError::invalid(field("system"), message));
        }
    }

    Ok(ResolvedFn {
        name: name.to_string(),
        image,
        entry,
        cmd,
        mode: l.mode.expect("mode has a built-in default"),
        runtime,
        requirements: l.requirements.map(|r| absolutise(base_dir, &r)),
        workdir: l.workdir.expect("workdir has a built-in default"),
        user: l.user.expect("user has a built-in default"),
        system,
        nix: l.nix.unwrap_or_default(),
        isolation: l.isolation.expect("isolation has a built-in default"),
        seccomp: l.seccomp.expect("seccomp has a built-in default"),
        limits,
        network,
        allow,
        allow_private_net: opts.allow_private_net,
        mounts,
        env,
        secrets,
        concurrency,
        idle_timeout,
        cold_after,
        warnings,
    })
}

fn absolutise(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        normalise(&base.join(p))
    }
}

/// Lexical `.`/`..` removal. Deliberately not `canonicalize`: the path may not
/// exist yet (a scratch mount created on first run), and resolving symlinks on
/// the host would be the wrong semantics for a path that is about to be bound
/// into a different mount namespace.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn is_reserved_mount_target(target: &Path) -> bool {
    matches!(
        target.to_str(),
        Some("/") | Some("/proc") | Some("/sys") | Some("/dev")
    )
}

/// Whether a function name is safe to join into a path.
///
/// Deliberately stricter than "does not contain a slash": the name reaches a
/// directory name, a unix socket path, a pasta pid file and a cgroup
/// directory, and each of those has its own opinion about leading dots,
/// spaces and control characters. An allowlist has one opinion.
///
/// `.` and `..` pass the character filter and are still traversal, so they are
/// excluded by the first-character rule rather than by a special case.
pub fn is_fn_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn is_env_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(text: &str) -> Spec {
        Spec::parse(text, Some(PathBuf::from("/proj/sandbox.toml"))).unwrap()
    }

    fn opts() -> ResolveOptions {
        ResolveOptions::default()
    }

    // --- resolve_for_serve ------------------------------------------------

    #[test]
    fn serving_a_name_the_spec_never_declared_is_not_an_error() {
        // `zygo serve handler.py --name double` on a machine with no spec file
        // at all. Rejecting it would make the documented command impossible.
        let s = Spec::default();
        let overrides = Layer {
            image: Some("python:3.12-slim".into()),
            entry: Some(PathBuf::from("/proj/handler.py")),
            ..Default::default()
        };
        let r = s
            .resolve_for_serve("double", &overrides, &opts())
            .expect("the flags are the whole definition");
        assert_eq!(r.name, "double");
        assert_eq!(r.entry, Some(PathBuf::from("/proj/handler.py")));
    }

    #[test]
    fn serving_a_declared_name_still_layers_the_spec_under_the_flags() {
        let s = spec(
            "[defaults]\nimage = \"python:3.12-slim\"\n\
             \n[fn.resize]\nentry = \"resize.py\"\nmem = \"512M\"\n",
        );
        // No flags: the spec's own definition.
        let r = s
            .resolve_for_serve("resize", &Layer::default(), &opts())
            .expect("declared");
        assert_eq!(r.entry, Some(PathBuf::from("/proj/resize.py")));
        assert_eq!(r.limits.mem, Bytes::from_mib(512));

        // One flag: it wins, the rest of the spec still applies.
        let overrides = Layer {
            mem: Some(Bytes::from_mib(128)),
            ..Default::default()
        };
        let r = s
            .resolve_for_serve("resize", &overrides, &opts())
            .expect("declared");
        assert_eq!(r.limits.mem, Bytes::from_mib(128), "the flag wins");
        assert_eq!(
            r.entry,
            Some(PathBuf::from("/proj/resize.py")),
            "and the spec still supplies everything else"
        );
    }

    #[test]
    fn resolve_still_rejects_an_unknown_name_for_up_and_explain() {
        // The distinction this pair of methods exists for: a typo in `zygo up`
        // must still be caught, because there the name *is* a lookup.
        let s = spec("[fn.resize]\nimage = \"alpine\"\ncmd = [\"/bin/true\"]\n");
        let err = s
            .resolve(Some("resiez"), &Layer::default(), &opts())
            .expect_err("a typo is not a new function");
        assert!(err.to_string().contains("resiez"), "{err}");
    }

    #[test]
    fn a_served_function_resolves_paths_against_the_callers_directory() {
        // The supervisor's working directory is its own, so `base_dir` is what
        // makes `--mount ./data:/data` mean what the caller typed.
        let s = Spec::default();
        let overrides = Layer {
            image: Some("python:3.12-slim".into()),
            entry: Some(PathBuf::from("handler.py")),
            ..Default::default()
        };
        let r = s
            .resolve_for_serve(
                "f",
                &overrides,
                &ResolveOptions {
                    base_dir: Some(PathBuf::from("/home/me/project")),
                    ..opts()
                },
            )
            .expect("resolve");
        assert_eq!(r.entry, Some(PathBuf::from("/home/me/project/handler.py")));
    }

    #[test]
    fn built_in_defaults_apply_to_an_empty_spec() {
        let s = spec("[fn.f]\ncmd = [\"/bin/true\"]\nimage = \"alpine\"\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert_eq!(r.limits.mem, Bytes::from_mib(256));
        assert_eq!(r.limits.pids, 64);
        assert_eq!(r.limits.timeout, Duration::from_secs(30));
        assert_eq!(r.limits.nofile, 1024);
        assert_eq!(r.network, Network::None);
        assert_eq!(r.isolation, Isolation::Ns);
        assert_eq!(r.seccomp, SeccompProfile::Default);
        assert_eq!(r.runtime, None, "no entry ⇒ warm-exec");
    }

    #[test]
    fn precedence_is_flag_over_fn_over_defaults_over_builtin() {
        let s = spec(
            r#"
[defaults]
mem = "512M"
pids = 100
[fn.f]
image = "alpine"
cmd = ["/bin/true"]
mem = "1G"
"#,
        );
        // built-in < defaults
        assert_eq!(
            s.resolve(Some("f"), &Layer::default(), &opts())
                .unwrap()
                .limits
                .pids,
            100
        );
        // defaults < fn
        assert_eq!(
            s.resolve(Some("f"), &Layer::default(), &opts())
                .unwrap()
                .limits
                .mem,
            Bytes::from_mib(1024)
        );
        // fn < flag
        let flags = Layer {
            mem: Some(Bytes::from_mib(128)),
            ..Default::default()
        };
        assert_eq!(
            s.resolve(Some("f"), &flags, &opts()).unwrap().limits.mem,
            Bytes::from_mib(128)
        );
    }

    #[test]
    fn memory_high_is_ninety_percent_of_max() {
        let s = spec("[fn.f]\nimage=\"alpine\"\ncmd=[\"/bin/true\"]\nmem=\"1G\"\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert_eq!(r.limits.mem_high, Bytes::from_mib(1024).scaled(0.9));
        assert_eq!(r.limits.swap, Bytes(0), "swap stays off by default");
    }

    #[test]
    fn runtime_is_inferred_from_the_entry_extension() {
        let s = spec("[fn.f]\nentry = \"./h.py\"\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert_eq!(r.runtime, Some(Runtime::Builtin(BuiltinRuntime::Python)));
        assert_eq!(
            r.image, "python:3.12-slim",
            "default image follows the runtime"
        );
        assert_eq!(r.entry, Some(PathBuf::from("/proj/h.py")), "made absolute");
    }

    #[test]
    fn entry_and_cmd_together_are_rejected() {
        let s = spec("[fn.f]\nentry=\"./h.py\"\ncmd=[\"/bin/true\"]\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn a_function_with_neither_entry_nor_cmd_is_rejected() {
        let s = spec("[fn.f]\nimage=\"alpine\"\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(err.to_string().contains("nothing to run"), "{err}");
    }

    /// `zygo run alpine:3` with no command: the image's own entrypoint and cmd
    /// are the command, and only `zygo run` can read them, so the resolver has
    /// to hand back an empty one rather than refusing.
    #[test]
    fn a_one_shot_run_may_leave_the_command_to_the_image() {
        let one_shot = ResolveOptions {
            one_shot: true,
            ..Default::default()
        };
        let flags = Layer {
            image: Some("alpine:3".into()),
            ..Default::default()
        };
        let r = resolve_standalone("run", &flags, &one_shot).unwrap();
        assert!(r.cmd.is_empty(), "left for the image config to supply");
        assert_eq!(r.entry, None);
        assert_eq!(r.runtime, None, "no agent: this is a plain program");

        // The same layer served rather than run is still a spec bug.
        let err = resolve_standalone("f", &flags, &opts()).unwrap_err();
        assert!(err.to_string().contains("nothing to run"), "{err}");
    }

    #[test]
    fn unknown_extension_needs_an_explicit_runtime() {
        let s = spec("[fn.f]\nentry=\"./h.rb\"\nimage=\"ruby\"\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(err.to_string().contains("cannot infer a runtime"), "{err}");

        let s = spec("[fn.f]\nentry=\"./h.rb\"\nimage=\"ruby\"\nruntime={agent=\"/a\"}\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert_eq!(r.runtime, Some(Runtime::Agent(PathBuf::from("/a"))));
    }

    /// `zygo run --mem 64M`, one flag and nothing else. The old flat 64 MiB
    /// default made that an error about a field the user never mentioned.
    #[test]
    fn an_unset_scratch_follows_mem_down() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmem=\"64M\"\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert_eq!(r.limits.scratch, Bytes::from_mib(32), "half of mem");
        assert!(
            !r.warnings.iter().any(|w| w.contains("scratch")),
            "the default must never warn about itself: {:?}",
            r.warnings
        );

        // An ordinary sandbox is unaffected: 64 MiB is a ceiling, not a ratio.
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert_eq!(r.limits.scratch, Bytes::from_mib(64), "256M default mem");

        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmem=\"1G\"\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert_eq!(r.limits.scratch, Bytes::from_mib(64), "capped, not scaled");
    }

    /// The conflict still exists when someone asks for it, and the error now
    /// says which size would work.
    #[test]
    fn an_explicit_scratch_that_cannot_fit_names_a_size_that_does() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmem=\"64M\"\nscratch=\"64M\"\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be smaller than mem"), "{err}");
        assert!(err.contains("32M"), "names the size that would work: {err}");
    }

    #[test]
    fn scratch_must_fit_inside_mem() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmem=\"64M\"\nscratch=\"64M\"\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(
            err.to_string().contains("must be smaller than mem"),
            "{err}"
        );
    }

    #[test]
    fn oversized_scratch_warns_before_it_errors() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmem=\"100M\"\nscratch=\"80M\"\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("over half of mem")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn tiny_memory_limits_are_rejected() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmem=\"1M\"\nscratch=\"1K\"\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(err.to_string().contains("below the"), "{err}");
    }

    #[test]
    fn host_network_needs_an_explicit_flag() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nnetwork=\"host\"\n");
        assert!(s.resolve(Some("f"), &Layer::default(), &opts()).is_err());

        let o = ResolveOptions {
            allow_host_net: true,
            ..Default::default()
        };
        assert_eq!(
            s.resolve(Some("f"), &Layer::default(), &o).unwrap().network,
            Network::Host
        );
    }

    #[test]
    fn an_allowlist_without_egress_is_an_error_not_a_silent_noop() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nallow=[\"a.com:443\"]\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(err.to_string().contains("no effect"), "{err}");
    }

    #[test]
    fn private_ranges_need_an_explicit_flag() {
        let s = spec(
            "[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nnetwork=\"egress\"\nallow=[\"10.0.0.0/8:5432\"]\n",
        );
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(err.to_string().contains("private or link-local"), "{err}");

        let o = ResolveOptions {
            allow_private_net: true,
            ..Default::default()
        };
        assert!(s.resolve(Some("f"), &Layer::default(), &o).is_ok());
    }

    #[test]
    fn reserved_mount_targets_are_rejected() {
        for target in ["/", "/proc", "/sys", "/dev"] {
            let s = spec(&format!(
                "[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmounts=[\"./x:{target}\"]\n"
            ));
            let err = s
                .resolve(Some("f"), &Layer::default(), &opts())
                .unwrap_err();
            assert!(
                err.to_string().contains("managed by the sandbox"),
                "{target}: {err}"
            );
        }
    }

    #[test]
    fn duplicate_mount_targets_are_rejected() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmounts=[\"./a:/x\",\"./b:/x\"]\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(err.to_string().contains("duplicate mount target"), "{err}");
    }

    #[test]
    fn mount_sources_are_resolved_against_the_spec_directory() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nmounts=[\"./cache:/cache:rw\"]\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert_eq!(r.mounts[0].source, PathBuf::from("/proj/cache"));
        assert_eq!(r.mounts[0].mode, MountMode::Rw);
    }

    #[test]
    fn a_secret_cannot_also_be_an_env_var() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nsecrets=[\"K\"]\n\n[fn.f.env]\nK=\"v\"\n");
        let err = s
            .resolve(Some("f"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(
            err.to_string().contains("both `env` and `secrets`"),
            "{err}"
        );
    }

    #[test]
    fn invalid_env_names_are_rejected() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\n\n[fn.f.env]\n\"1BAD\"=\"v\"\n");
        assert!(s.resolve(Some("f"), &Layer::default(), &opts()).is_err());
    }

    /// A function name becomes a directory, a socket path, a pasta pid file
    /// and a cgroup directory, so it is validated before any of them is built.
    ///
    /// `cgroup::sanitise` existed for this and covered only the cgroup; the
    /// other three joined the raw name (B-07). `[fn."../x"]` is legal TOML and
    /// `--name` is free text, so the check belongs where every path agrees to
    /// start: resolution.
    #[test]
    fn a_function_name_that_would_escape_its_directory_is_refused() {
        for name in ["../x", "a/b", ".", "..", "", " leading", "a b", "a\u{0}b"] {
            assert!(
                !is_fn_name(name),
                "`{name}` would be joined into a path unchanged"
            );
        }
        for name in ["resize", "resize-2", "a_b.c", "x", "9lives"] {
            assert!(is_fn_name(name), "`{name}` is an ordinary name");
        }

        // And the resolver refuses it rather than leaving it to whichever
        // path is built first.
        let s = spec("[fn.\"../escape\"]\nimage=\"a\"\ncmd=[\"x\"]\n");
        let err = s
            .resolve(Some("../escape"), &Layer::default(), &opts())
            .expect_err("a traversing name is refused");
        assert!(
            err.to_string().contains("usable function name"),
            "the refusal should say what is wrong: {err}"
        );

        // A name nobody asked about must not be refused on somebody else's
        // behalf: `resolve` only resolves the one it was given.
        let s = spec("[fn.good]\nimage=\"a\"\ncmd=[\"x\"]\n");
        assert!(s.resolve(Some("good"), &Layer::default(), &opts()).is_ok());
    }

    #[test]
    fn unknown_function_names_report_the_neighbours() {
        let s = spec("[fn.resize]\nentry=\"./r.py\"\n");
        let err = s
            .resolve(Some("resze"), &Layer::default(), &opts())
            .unwrap_err();
        assert!(err.to_string().contains("did you mean `resize`"), "{err}");
    }

    #[test]
    fn run_mode_resolves_defaults_without_a_function() {
        let s = spec("[defaults]\nmem=\"128M\"\n");
        let flags = Layer {
            image: Some("python:3.12".into()),
            cmd: Some(vec!["python".into(), "-c".into(), "pass".into()]),
            ..Default::default()
        };
        let r = s.resolve(None, &flags, &opts()).unwrap();
        assert_eq!(r.name, "run");
        assert_eq!(r.limits.mem, Bytes::from_mib(128));
        assert_eq!(r.cmd, ["python", "-c", "pass"]);
    }

    #[test]
    fn standalone_resolution_needs_no_spec_file() {
        let flags = Layer {
            image: Some("alpine".into()),
            cmd: Some(vec!["/bin/true".into()]),
            ..Default::default()
        };
        let r = resolve_standalone("run", &flags, &opts()).unwrap();
        assert_eq!(r.image, "alpine");
        assert_eq!(r.limits.pids, 64);
    }

    /// The default configuration must not warn on every invocation: a warning
    /// nobody can avoid is a warning everybody learns to ignore.
    #[test]
    fn a_one_shot_run_does_not_warn_about_co_tenancy() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\n");

        let served = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert!(
            served.warnings.iter().any(|w| w.contains("disk I/O")),
            "a served function shares the device with its neighbours"
        );

        let one_shot = ResolveOptions {
            one_shot: true,
            ..Default::default()
        };
        let run = s.resolve(Some("f"), &Layer::default(), &one_shot).unwrap();
        assert!(
            !run.warnings.iter().any(|w| w.contains("disk I/O")),
            "a one-shot run has no neighbours: {:?}",
            run.warnings
        );
    }

    #[test]
    fn cold_after_shorter_than_idle_timeout_warns() {
        let s = spec("[fn.f]\nimage=\"a\"\ncmd=[\"x\"]\nidle_timeout=\"1h\"\ncold_after=\"10m\"\n");
        let r = s.resolve(Some("f"), &Layer::default(), &opts()).unwrap();
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("without ever pausing"))
        );
    }

    #[test]
    fn paths_are_normalised_without_touching_the_filesystem() {
        assert_eq!(normalise(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(
            absolutise(Path::new("/proj"), Path::new("../x")),
            PathBuf::from("/x")
        );
        assert_eq!(
            absolutise(Path::new("/proj"), Path::new("/abs")),
            PathBuf::from("/abs")
        );
    }
}
