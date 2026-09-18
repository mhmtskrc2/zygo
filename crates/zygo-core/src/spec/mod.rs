//! The `sandbox.toml` surface (design doc §4.4) and the layering rules that
//! turn it into a runnable description.
//!
//! Precedence, highest first: **CLI flags → `[fn.<name>]` → `[defaults]` →
//! built-in defaults**. All four are the same [`Layer`] type with optional
//! fields, so a field can never be settable in one place and not another.

pub mod resolve;
pub mod types;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use resolve::{ResolveOptions, ResolvedFn, defaults, resolve_standalone};
pub use types::{
    AllowRule, BuiltinRuntime, Bytes, Cidr, Cpu, Duration, HandlerMode, HostPattern, Isolation,
    Mount, MountMode, Network, ParseError, Runtime, SeccompProfile,
};

/// A parsed spec file.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Project-level defaults applied to every function.
    #[serde(default)]
    pub defaults: Layer,

    /// Named functions. `BTreeMap` so `zygo up` and `zygo ps` list them in a
    /// stable order regardless of file order.
    #[serde(default, rename = "fn")]
    pub functions: BTreeMap<String, Layer>,

    #[serde(default)]
    pub api: Option<ApiSpec>,

    /// Path the spec was read from; used for resolving relative `entry`,
    /// `mounts` and `requirements` paths, and for error messages.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

/// One layer of configuration. Every field is optional: absence means "inherit
/// from the layer below", which is what makes merging associative.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    // --- what to run -------------------------------------------------------
    pub image: Option<String>,
    pub entry: Option<PathBuf>,
    pub cmd: Option<Vec<String>>,
    pub mode: Option<HandlerMode>,
    pub runtime: Option<Runtime>,
    pub requirements: Option<PathBuf>,
    pub workdir: Option<PathBuf>,
    pub user: Option<String>,

    // --- derived system layer (design doc §3.7) ---------------------------
    pub system: Option<Vec<String>>,
    pub nix: Option<Vec<String>>,

    // --- isolation ---------------------------------------------------------
    pub isolation: Option<Isolation>,
    pub seccomp: Option<SeccompProfile>,

    // --- limits (design doc §3.5) -----------------------------------------
    pub mem: Option<Bytes>,
    pub cpu: Option<Cpu>,
    pub pids: Option<u32>,
    pub timeout: Option<Duration>,
    pub scratch: Option<Bytes>,
    pub io_read: Option<Bytes>,
    pub io_write: Option<Bytes>,
    pub nofile: Option<u64>,

    // --- network (design doc §3.8) ----------------------------------------
    pub network: Option<Network>,
    pub allow: Option<Vec<AllowRule>>,

    // --- environment -------------------------------------------------------
    pub mounts: Option<Vec<Mount>>,
    pub env: Option<BTreeMap<String, String>>,
    pub secrets: Option<Vec<String>>,

    // --- warm pool ---------------------------------------------------------
    pub concurrency: Option<u32>,
    pub idle_timeout: Option<Duration>,
    pub cold_after: Option<Duration>,
}

/// `[api]` block: the local HTTP/Unix endpoint (design doc §4.6).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApiSpec {
    /// `127.0.0.1:7700` or `unix:///run/user/1000/zygo/api.sock`.
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub auth: ApiAuth,
}

fn default_listen() -> String {
    "127.0.0.1:7700".to_string()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiAuth {
    /// Token from `ZYGO_API_TOKEN`.
    #[default]
    Bearer,
    /// No authentication. Only sensible on a unix socket with 0600 permissions.
    None,
}

impl Default for ApiSpec {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            auth: ApiAuth::default(),
        }
    }
}

impl Layer {
    /// Merge `self` under `over`: every field set in `over` wins.
    ///
    /// Collection fields replace rather than concatenate. Appending would make
    /// it impossible to *remove* an inherited mount or allow-rule from a
    /// function, and silently widening a security boundary by inheritance is
    /// exactly the failure mode principle P6 exists to prevent.
    pub fn merge(&self, over: &Layer) -> Layer {
        macro_rules! pick {
            ($($field:ident),+ $(,)?) => {
                Layer { $($field: over.$field.clone().or_else(|| self.$field.clone())),+ }
            };
        }
        pick!(
            image,
            entry,
            cmd,
            mode,
            runtime,
            requirements,
            workdir,
            user,
            system,
            nix,
            isolation,
            seccomp,
            mem,
            cpu,
            pids,
            timeout,
            scratch,
            io_read,
            io_write,
            nofile,
            network,
            allow,
            mounts,
            env,
            secrets,
            concurrency,
            idle_timeout,
            cold_after,
        )
    }
}

impl Spec {
    /// Parse a spec from TOML text. `source` is used for relative path
    /// resolution and in error messages.
    pub fn parse(text: &str, source: Option<PathBuf>) -> Result<Spec, SpecError> {
        let mut spec: Spec = toml::from_str(text).map_err(|e| SpecError::Parse {
            file: display_path(source.as_deref()),
            message: e.to_string(),
        })?;
        spec.source = source;
        Ok(spec)
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Spec, SpecError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| SpecError::Read {
            file: path.display().to_string(),
            message: e.to_string(),
        })?;
        Spec::parse(&text, Some(path.to_path_buf()))
    }

    /// Directory that relative paths in the spec are resolved against.
    pub fn base_dir(&self) -> PathBuf {
        self.source
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Search for a spec file the way `zygo up` does: the given path, else
    /// `sandbox.toml` in the current directory and its ancestors.
    pub fn discover(explicit: Option<&Path>) -> Result<Option<Spec>, SpecError> {
        if let Some(p) = explicit {
            return Spec::from_file(p).map(Some);
        }
        let mut dir = std::env::current_dir().ok();
        while let Some(d) = dir {
            let candidate = d.join("sandbox.toml");
            if candidate.is_file() {
                return Spec::from_file(&candidate).map(Some);
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        Ok(None)
    }

    pub fn function(&self, name: &str) -> Option<&Layer> {
        self.functions.get(name)
    }

    /// Names of every declared function, in stable order.
    pub fn function_names(&self) -> impl Iterator<Item = &str> {
        self.functions.keys().map(String::as_str)
    }
}

/// Errors that come out of reading, parsing, resolving or validating a spec.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SpecError {
    #[error("cannot read spec file {file}: {message}")]
    Read { file: String, message: String },

    #[error("{file}: {message}")]
    Parse { file: String, message: String },

    #[error("function `{name}` is not defined in {file}{suggestion}")]
    UnknownFunction {
        name: String,
        file: String,
        /// Pre-rendered "\n  → did you mean `x`?" hint.
        suggestion: String,
    },

    /// A semantic problem: the file parses, but the configuration it describes
    /// cannot be run. `field` is the dotted key path so the user can find it.
    #[error("{field}: {message}{remedy}")]
    Invalid {
        field: String,
        message: String,
        /// Pre-rendered "\n  → ..." hint, empty when there is nothing to add.
        remedy: String,
    },
}

impl SpecError {
    pub fn invalid(field: impl Into<String>, message: impl Into<String>) -> Self {
        SpecError::Invalid {
            field: field.into(),
            message: message.into(),
            remedy: String::new(),
        }
    }

    pub fn invalid_with(
        field: impl Into<String>,
        message: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        SpecError::Invalid {
            field: field.into(),
            message: message.into(),
            remedy: format!("\n  → {}", remedy.into()),
        }
    }

    pub fn unknown_function(name: &str, file: Option<&Path>, known: &[&str]) -> Self {
        SpecError::UnknownFunction {
            name: name.to_string(),
            file: display_path(file),
            suggestion: match closest(name, known) {
                Some(c) => format!("\n  → did you mean `{c}`?"),
                None if known.is_empty() => "\n  → the spec declares no functions".to_string(),
                None => format!("\n  → known functions: {}", known.join(", ")),
            },
        }
    }
}

fn display_path(p: Option<&Path>) -> String {
    p.map(|p| p.display().to_string())
        .unwrap_or_else(|| "<spec>".to_string())
}

/// Locate a dotted key path in the spec source, for `file:line:col` reporting.
///
/// Best-effort and intentionally simple: `toml` gives spans for parse errors
/// but not for semantic ones, and reproducing its span tracking to point at
/// `fn.resize.mem` is not worth the machinery. If the key cannot be found the
/// caller just omits the location.
pub fn locate_field(source: &str, field: &str) -> Option<(usize, usize)> {
    let (table, key) = match field.rsplit_once('.') {
        Some((t, k)) => (Some(t), k),
        None => (None, field),
    };

    let mut current_table: Option<String> = None;
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('[') {
            current_table = rest.split(']').next().map(|s| s.trim().to_string());
            // A `[fn.resize]` header is itself the location for `fn.resize`.
            if current_table.as_deref() == Some(field) {
                let col = line.len() - trimmed.len() + 1;
                return Some((idx + 1, col));
            }
            continue;
        }
        let Some((lhs, _)) = trimmed.split_once('=') else {
            continue;
        };
        if lhs.trim() != key {
            continue;
        }
        let in_right_table = match table {
            None => current_table.is_none(),
            Some(t) => current_table.as_deref() == Some(t),
        };
        if in_right_table {
            let col = line.len() - trimmed.len() + 1;
            return Some((idx + 1, col));
        }
    }
    None
}

/// Nearest known name within a small edit distance, for "did you mean".
fn closest<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let max = (input.len() / 3).max(1);
    candidates
        .iter()
        .map(|c| (*c, edit_distance(input, c)))
        .filter(|(_, d)| *d <= max)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC_EXAMPLE: &str = r#"
[defaults]
image        = "python:3.12-slim"
isolation    = "ns"
mem          = "256M"
cpu          = 0.5
pids         = 64
timeout      = "30s"
network      = "none"
scratch      = "64M"
concurrency  = 4
idle_timeout = "10m"

[fn.resize]
runtime      = "python"
entry        = "./resize.py"
requirements = "./requirements.txt"
mem          = "512M"
mounts       = ["./cache:/cache:rw"]

[fn.parse]
image  = "golang:1.23"
cmd    = ["/app/parser"]
mounts = ["./bin:/app:ro"]

[fn.legacy_ssl]
entry  = "./legacy.py"
system = ["libssl3=3.5.*", "libpq5"]

[fn.custom_rt]
image   = "my/elixir-app"
runtime = { agent = "/app/zygo-agent" }

[fn.fetch]
entry   = "./fetch.py"
network = "egress"
allow   = ["api.stripe.com:443", "*.example.com:443"]
env     = { STRIPE_MODE = "test" }
secrets = ["STRIPE_KEY"]

[fn.untrusted]
entry     = "./user_code.py"
isolation = "vm"
mem       = "128M"
timeout   = "10s"

[fn.legacy]
entry = "./old_script.py"
mode  = "stdin"

[api]
listen = "127.0.0.1:7700"
auth   = "bearer"
"#;

    /// The design doc's example must parse verbatim — it is the contract.
    #[test]
    fn design_doc_example_parses() {
        let spec = Spec::parse(DOC_EXAMPLE, None).expect("doc example should parse");

        assert_eq!(spec.defaults.mem, Some(Bytes::from_mib(256)));
        assert_eq!(spec.defaults.cpu, Some(Cpu(0.5)));
        assert_eq!(spec.defaults.pids, Some(64));
        assert_eq!(spec.defaults.isolation, Some(Isolation::Ns));

        let names: Vec<&str> = spec.function_names().collect();
        assert_eq!(
            names,
            [
                "custom_rt",
                "fetch",
                "legacy",
                "legacy_ssl",
                "parse",
                "resize",
                "untrusted"
            ]
        );

        let resize = spec.function("resize").unwrap();
        assert_eq!(resize.mem, Some(Bytes::from_mib(512)));
        assert_eq!(
            resize.runtime,
            Some(Runtime::Builtin(BuiltinRuntime::Python))
        );
        assert_eq!(resize.mounts.as_ref().unwrap()[0].mode, MountMode::Rw);

        let custom = spec.function("custom_rt").unwrap();
        assert_eq!(
            custom.runtime,
            Some(Runtime::Agent(PathBuf::from("/app/zygo-agent")))
        );

        let fetch = spec.function("fetch").unwrap();
        assert_eq!(fetch.network, Some(Network::Egress));
        assert_eq!(fetch.allow.as_ref().unwrap().len(), 2);
        assert_eq!(
            fetch.secrets.as_deref(),
            Some(&["STRIPE_KEY".to_string()][..])
        );

        assert_eq!(
            spec.function("legacy").unwrap().mode,
            Some(HandlerMode::Stdin)
        );
        assert_eq!(spec.api.unwrap().listen, "127.0.0.1:7700");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = Spec::parse("[defaults]\nmemroy = \"256M\"\n", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("memroy"), "{msg}");
    }

    #[test]
    fn merge_prefers_the_upper_layer() {
        let base = Layer {
            mem: Some(Bytes::from_mib(256)),
            pids: Some(64),
            image: Some("python:3.12".into()),
            ..Default::default()
        };
        let over = Layer {
            mem: Some(Bytes::from_mib(512)),
            ..Default::default()
        };
        let merged = base.merge(&over);
        assert_eq!(merged.mem, Some(Bytes::from_mib(512)));
        assert_eq!(merged.pids, Some(64), "unset fields fall through");
        assert_eq!(merged.image.as_deref(), Some("python:3.12"));
    }

    #[test]
    fn merge_replaces_collections_instead_of_appending() {
        let base = Layer {
            allow: Some(vec!["a.com:443".parse().unwrap()]),
            ..Default::default()
        };
        let over = Layer {
            allow: Some(vec!["b.com:443".parse().unwrap()]),
            ..Default::default()
        };
        let merged = base.merge(&over);
        let allow = merged.allow.unwrap();
        assert_eq!(allow.len(), 1);
        assert_eq!(allow[0].to_string(), "b.com:443");
    }

    #[test]
    fn merge_is_associative() {
        let a = Layer {
            mem: Some(Bytes::from_mib(64)),
            pids: Some(8),
            ..Default::default()
        };
        let b = Layer {
            mem: Some(Bytes::from_mib(128)),
            ..Default::default()
        };
        let c = Layer {
            pids: Some(32),
            ..Default::default()
        };
        assert_eq!(a.merge(&b).merge(&c), a.merge(&b.merge(&c)));
    }

    #[test]
    fn locate_field_finds_nested_keys() {
        // Assert on the *content* of the located line rather than on a line
        // number, so editing the fixture cannot silently invalidate the test.
        let line_at = |field: &str| -> String {
            let (line, col) =
                locate_field(DOC_EXAMPLE, field).unwrap_or_else(|| panic!("`{field}` not located"));
            assert_eq!(col, 1, "`{field}` should be at the start of the line");
            DOC_EXAMPLE.lines().nth(line - 1).unwrap().to_string()
        };

        assert!(line_at("defaults.mem").contains("256M"));
        assert!(
            line_at("fn.resize.mem").contains("512M"),
            "the *function's* mem"
        );
        assert!(line_at("fn.untrusted.mem").contains("128M"));
        // A table header locates itself.
        assert_eq!(line_at("fn.untrusted").trim(), "[fn.untrusted]");

        assert_eq!(locate_field(DOC_EXAMPLE, "fn.nope.mem"), None);
        assert_eq!(locate_field(DOC_EXAMPLE, "defaults.nosuchfield"), None);
    }

    #[test]
    fn unknown_function_suggests_a_neighbour() {
        let err = SpecError::unknown_function("resze", None, &["resize", "parse"]);
        assert!(err.to_string().contains("did you mean `resize`"), "{err}");

        let err = SpecError::unknown_function("zzz", None, &["resize"]);
        assert!(err.to_string().contains("known functions: resize"), "{err}");
    }
}
