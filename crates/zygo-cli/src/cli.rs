//! Command-line surface (design doc §4.3).
//!
//! The CLI is a thin client over `zygo-core` (ADR-008), so this module is
//! almost entirely declaration: parse, build a [`Layer`] of overrides, hand it
//! to the library.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use zygo_core::spec::{
    AllowRule, Bytes, Cpu, Duration, HandlerMode, Isolation, Layer, Mount, Network, SeccompProfile,
};

#[derive(Debug, Parser)]
#[command(
    name = "zygo",
    version,
    about = "Run functions in warm, isolated sandboxes — no daemon, no root",
    long_about = "Zygo runs function-shaped code with Docker's ergonomics but without \
                  the container create/destroy cycle: the sandbox waits warm, and a \
                  request costs a fork.",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Emit machine-readable JSON instead of tables.
    #[arg(long, global = true)]
    pub json: bool,

    /// Increase log verbosity. Repeat for more.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Override the data directory (default `$XDG_DATA_HOME/zygo`).
    #[arg(long, global = true, value_name = "DIR", env = "ZYGO_DATA_HOME")]
    pub data_root: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a one-shot sandbox; stdin, stdout and the exit code pass through.
    Run(RunArgs),

    /// Start a warm zygote for a handler.
    Serve(ServeArgs),

    /// Call a warm function: the result on stdout, the request's own exit
    /// status (137 when its deadline killed it).
    Exec(ExecArgs),

    /// List warm sandboxes.
    Ps,

    /// Show a function's recent log: the zygote's own output and one line
    /// per request, with its stdout and stderr.
    ///
    /// The supervisor keeps the last 500 entries per function, across
    /// replacements and cold spells. `--json` prints one entry per line.
    Logs {
        name: String,
        /// Keep printing as new entries arrive.
        #[arg(short, long)]
        follow: bool,
        /// How many of the most recent entries to start with.
        #[arg(short = 'n', long, default_value_t = 50)]
        tail: u32,
        /// Only requests that failed: a non-zero exit, or an error.
        #[arg(long)]
        failed: bool,
    },

    /// Stop a sandbox.
    Stop {
        name: Option<String>,
        /// Stop every sandbox.
        #[arg(long)]
        all: bool,
    },

    /// The warm pool's own process.
    ///
    /// Hidden because `zygo serve` starts one when it needs one; it is here for
    /// running the supervisor in the foreground to see why it will not start,
    /// and it is what `serve` re-execs.
    #[command(subcommand, hide = true)]
    Supervisor(SupervisorCommand),

    /// Live resource table.
    Top,

    /// Metric summary.
    Stats { name: Option<String> },

    /// Download an image into the local store.
    Pull {
        /// Image reference, e.g. `python:3.12-slim`.
        image: String,
        /// Pull for a platform other than this host's, e.g. `linux/amd64`.
        #[arg(long)]
        platform: Option<String>,
    },

    /// List images in the local store.
    Images,

    /// Image store maintenance.
    #[command(subcommand)]
    Image(ImageCommand),

    /// Store registry credentials.
    Login { registry: String },

    /// Start every function in the spec file.
    ///
    /// Writes `zygo.lock` beside the spec: the image digest each function
    /// resolved to, and the versions `apt` chose for its `system` packages.
    /// A later `up` refuses a function whose image has moved under an
    /// unedited spec, until `--relock` says that is wanted.
    Up {
        #[command(flatten)]
        file: SpecFileArgs,

        /// Accept an image that has moved and rewrite `zygo.lock`.
        #[arg(long)]
        relock: bool,
    },

    /// Stop everything `up` started.
    Down {
        #[command(flatten)]
        file: SpecFileArgs,
    },

    /// Inspect the spec file.
    Spec {
        #[command(flatten)]
        file: SpecFileArgs,
        #[command(subcommand)]
        command: SpecCommand,
    },

    /// Manage optional isolation backends.
    #[command(subcommand)]
    Backend(BackendCommand),

    /// Runtime agent tooling.
    #[command(subcommand)]
    Agent(AgentCommand),

    /// Check whether this host can run sandboxes, and how to fix it if not.
    Doctor,

    /// Interactive shell inside a sandbox, for debugging.
    ///
    /// A fresh process entered into the function's namespaces; the warm agent
    /// is untouched, keeps its memory and keeps serving. It sees the sandbox's
    /// filesystem, pids, network and hostname, holds no capabilities, and is
    /// deliberately **not** under the seccomp filter, the Landlock ruleset or
    /// the tenant's cgroup — a debug shell that the memory limit kills is not
    /// one.
    Shell {
        name: String,
        /// Command to run instead of an interactive shell, after `--`.
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// Run the HTTP API in the foreground.
    Api(ApiArgs),

    /// Measure the warm path.
    #[command(subcommand)]
    Bench(BenchCommand),

    /// Print a shell completion script.
    ///
    /// Generated from the parser itself, so it can never drift from the flags
    /// the binary actually accepts — which is what a hand-written one does the
    /// first time a command is added.
    ///
    /// ```text
    /// zygo completion bash > /etc/bash_completion.d/zygo
    /// zygo completion zsh  > "${fpath[1]}/_zygo"
    /// zygo completion fish > ~/.config/fish/completions/zygo.fish
    /// ```
    Completion {
        /// bash, zsh, fish, elvish or powershell.
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    /// Delete unreferenced layers and stale caches.
    Prune {
        /// Report what would be deleted without deleting it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SpecCommand {
    /// Parse and validate the spec file.
    Validate,
    /// Print the fully resolved configuration for a function.
    Explain {
        /// Function name. Omit to show the `run` defaults.
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum BackendCommand {
    /// Show which backends this host can use.
    List,
    /// Download an optional backend.
    Install { name: String },
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Run the protocol conformance suite against a third-party agent.
    ///
    /// The agent is started with the control socket at descriptor 3 — where a
    /// sandboxed agent finds it too — and everything after the binary is passed
    /// through as its arguments. The handler it loads has to echo the event and
    /// honour `stdout`/`stderr`; see `examples/agents/`.
    Test {
        binary: PathBuf,
        /// Arguments for the agent, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum BenchCommand {
    /// Warm request overhead.
    Warm {
        #[arg(long, default_value_t = 10_000)]
        n: u32,
        /// Serve requests without a per-request cgroup, to see what it costs.
        #[arg(long)]
        no_cgroup: bool,
        /// Offered load in requests/s. Unset means as fast as possible, which
        /// drives the tenant into its own CPU quota and measures the quota
        /// rather than the runtime — see the saturation note in the output.
        #[arg(long)]
        rate: Option<f64>,
        /// CPU quota for the tenant, in cores. Defaults to the spec's `1.0`.
        #[arg(long)]
        cpu: Option<f64>,
        /// Measure warm-exec instead of the agent: the sandbox is held and this
        /// command runs per request, reading the event on stdin. Given after
        /// `--`, so its own flags are not mistaken for ours:
        /// `zygo bench warm --rate 250 -- sh -c cat` is the smallest program
        /// that completes the contract.
        #[arg(last = true, value_name = "CMD")]
        cmd: Vec<String>,
    },
    /// Cold `run` latency: build a sandbox, run a trivial program, tear it down.
    Cold {
        #[arg(long, default_value_t = 50)]
        n: u32,
        /// Image to start. The default is what requirement N2 is written about.
        #[arg(long, default_value = "python:3.12-slim")]
        image: String,
        /// Command to run. Defaults to starting the interpreter and exiting,
        /// because N2's budget includes the interpreter.
        #[arg(long, num_args = 1.., value_delimiter = ' ')]
        command: Option<Vec<String>>,
    },
    /// Sustained throughput through one warm function.
    Load {
        #[arg(long, default_value_t = 10)]
        seconds: u32,
        /// Clients calling at once. The design document's acceptance criterion
        /// is >= 600 requests/s at 4.
        #[arg(long, default_value_t = 4)]
        concurrency: u32,
        /// CPU quota for the tenant, in cores.
        #[arg(long)]
        cpu: Option<f64>,
    },
}

/// `-f/--file`, on the commands that read a spec.
///
/// Not a global flag: `zygo logs -f` means *follow*, and both spellings are in
/// the design doc's CLI reference. Scoping the flag keeps both.
#[derive(Debug, Args, Default, Clone)]
pub struct SpecFileArgs {
    /// Spec file to read. Defaults to `sandbox.toml`, searched upwards.
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    pub file: Option<PathBuf>,
}

impl SpecFileArgs {
    pub fn path(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Image reference.
    pub image: String,

    #[command(flatten)]
    pub spec_file: SpecFileArgs,

    /// Command to run. Defaults to the image's entrypoint and cmd.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,

    #[command(flatten)]
    pub limits: LimitArgs,

    #[command(flatten)]
    pub sandbox: SandboxArgs,

    /// Give the sandbox its own terminal instead of the caller's.
    ///
    /// Without it the sandbox inherits the caller's terminal, which is what
    /// makes colours and prompts work by default — but also hands tenant code a
    /// writable descriptor to it.
    #[arg(short = 't', long)]
    pub tty: bool,

    /// Print the resolved plan instead of running anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ApiArgs {
    #[command(flatten)]
    pub spec_file: SpecFileArgs,

    /// Where to listen: `HOST:PORT`, or `unix://PATH`. Overrides `[api] listen`.
    #[arg(long)]
    pub listen: Option<String>,

    /// Serve without authentication. Overrides `[api] auth`.
    ///
    /// Only accepted on a unix socket or a loopback address: an unauthenticated
    /// API on a reachable interface lets anyone on the network run code as you.
    #[arg(long)]
    pub no_auth: bool,

    /// Push metrics to an OTLP/HTTP collector at this base URL, e.g.
    /// `http://localhost:4318` (`/v1/metrics` is appended). The same numbers
    /// as `/metrics`, JSON-encoded; `OTEL_EXPORTER_OTLP_HEADERS` adds request
    /// headers (`key=value,…`).
    #[arg(long, value_name = "URL", env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,

    /// How often to push to the OTLP collector.
    #[arg(long, value_name = "DURATION", default_value = "60s", value_parser = parse_duration)]
    pub otlp_interval: Duration,
}

#[derive(Debug, Subcommand)]
pub enum SupervisorCommand {
    /// Run the supervisor in this process until it is told to stop.
    Run,
    /// Print what the supervisor is and where its socket lives.
    Status,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Handler file.
    pub handler: PathBuf,

    /// Name to register the function under.
    #[arg(long)]
    pub name: String,

    #[command(flatten)]
    pub spec_file: SpecFileArgs,

    /// Dependency file installed into a shared, cached venv.
    #[arg(long)]
    pub requirements: Option<PathBuf>,

    /// Concurrent forks allowed per zygote.
    #[arg(long)]
    pub concurrency: Option<u32>,

    /// Pause the zygote after this long idle.
    #[arg(long, value_parser = parse_duration)]
    pub idle_timeout: Option<Duration>,

    /// Handler calling convention.
    #[arg(long, value_parser = parse_mode)]
    pub mode: Option<HandlerMode>,

    /// A secret to deliver to the handler as `/run/secrets/<NAME>`, taken from
    /// this shell's environment. Repeatable.
    #[arg(long = "secret", value_name = "NAME")]
    pub secrets: Vec<String>,

    #[command(flatten)]
    pub limits: LimitArgs,

    #[command(flatten)]
    pub sandbox: SandboxArgs,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Function name.
    pub name: String,

    /// JSON event. Read from stdin when omitted.
    pub event: Option<String>,

    /// Read newline-delimited JSON events and run them in parallel.
    #[arg(long)]
    pub batch: bool,

    /// Give up on the handler after this long. Defaults to the function's own
    /// `timeout`, which is what `serve` resolved.
    #[arg(long, value_parser = parse_duration)]
    pub timeout: Option<Duration>,
}

/// Resource limits, shared by `run` and `serve`.
#[derive(Debug, Args, Default)]
pub struct LimitArgs {
    /// Memory limit, e.g. `512M`.
    #[arg(long, value_parser = parse_bytes)]
    pub mem: Option<Bytes>,

    /// CPU quota in cores, e.g. `0.5`.
    #[arg(long, value_parser = parse_cpu)]
    pub cpu: Option<Cpu>,

    /// Maximum process count. The fork-bomb limit.
    #[arg(long)]
    pub pids: Option<u32>,

    /// Wall-clock limit, e.g. `30s`.
    #[arg(long, value_parser = parse_duration)]
    pub timeout: Option<Duration>,

    /// Size of the writable `/tmp`, e.g. `64M`.
    #[arg(long, value_parser = parse_bytes)]
    pub scratch: Option<Bytes>,

    /// Open file limit.
    #[arg(long)]
    pub nofile: Option<u64>,
}

/// Isolation and environment, shared by `run` and `serve`.
#[derive(Debug, Args, Default)]
pub struct SandboxArgs {
    /// Isolation backend.
    #[arg(long, value_parser = parse_isolation)]
    pub isolation: Option<Isolation>,

    /// seccomp profile.
    #[arg(long, value_parser = parse_seccomp)]
    pub seccomp: Option<SeccompProfile>,

    /// Network policy.
    #[arg(long = "net", value_parser = parse_network)]
    pub network: Option<Network>,

    /// Egress allowlist entry, e.g. `api.stripe.com:443`. Repeatable.
    #[arg(long = "allow", value_parser = parse_allow)]
    pub allow: Vec<AllowRule>,

    /// Bind mount `host:guest[:ro|rw]`. Read-only unless `rw` is given.
    /// Repeatable.
    #[arg(long = "mount", value_parser = parse_mount)]
    pub mounts: Vec<Mount>,

    /// Environment variable `KEY=VALUE`. Repeatable.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// uid inside the sandbox.
    #[arg(long)]
    pub user: Option<String>,

    /// Working directory inside the sandbox.
    #[arg(long)]
    pub workdir: Option<PathBuf>,

    /// Permit `--net host`, which removes the network boundary.
    #[arg(long)]
    pub allow_host_net: bool,

    /// Permit egress rules targeting private and link-local ranges.
    #[arg(long)]
    pub allow_private_net: bool,

    /// Permit limits to be disabled. There is no unlimited sandbox without it.
    #[arg(long)]
    pub allow_unlimited: bool,
}

impl LimitArgs {
    fn apply(&self, layer: &mut Layer) {
        layer.mem = self.mem.or(layer.mem);
        layer.cpu = self.cpu.or(layer.cpu);
        layer.pids = self.pids.or(layer.pids);
        layer.timeout = self.timeout.or(layer.timeout);
        layer.scratch = self.scratch.or(layer.scratch);
        layer.nofile = self.nofile.or(layer.nofile);
    }
}

impl SandboxArgs {
    fn apply(&self, layer: &mut Layer) -> anyhow::Result<()> {
        layer.isolation = self.isolation.or(layer.isolation);
        layer.seccomp = self.seccomp.or(layer.seccomp);
        layer.network = self.network.or(layer.network);
        layer.user = self.user.clone().or_else(|| layer.user.clone());
        layer.workdir = self.workdir.clone().or_else(|| layer.workdir.clone());

        if !self.allow.is_empty() {
            layer.allow = Some(self.allow.clone());
        }
        if !self.mounts.is_empty() {
            layer.mounts = Some(self.mounts.clone());
        }
        if !self.env.is_empty() {
            let mut env = layer.env.clone().unwrap_or_default();
            for pair in &self.env {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--env expects KEY=VALUE, got `{pair}`"))?;
                env.insert(k.to_string(), v.to_string());
            }
            layer.env = Some(env);
        }
        Ok(())
    }

    pub fn resolve_options(&self) -> zygo_core::spec::ResolveOptions {
        zygo_core::spec::ResolveOptions {
            allow_host_net: self.allow_host_net,
            allow_private_net: self.allow_private_net,
            allow_unlimited: self.allow_unlimited,
            base_dir: None,
            one_shot: false,
        }
    }
}

impl RunArgs {
    /// The override layer these flags describe.
    pub fn to_layer(&self) -> anyhow::Result<Layer> {
        let mut layer = Layer {
            image: Some(self.image.clone()),
            ..Default::default()
        };
        if !self.command.is_empty() {
            layer.cmd = Some(self.command.clone());
        }
        self.limits.apply(&mut layer);
        self.sandbox.apply(&mut layer)?;
        Ok(layer)
    }
}

impl ServeArgs {
    /// The override layer these flags describe.
    pub fn to_layer(&self) -> anyhow::Result<Layer> {
        let mut layer = Layer {
            entry: Some(self.handler.clone()),
            requirements: self.requirements.clone(),
            concurrency: self.concurrency,
            idle_timeout: self.idle_timeout,
            mode: self.mode,
            secrets: (!self.secrets.is_empty()).then(|| self.secrets.clone()),
            ..Default::default()
        };
        self.limits.apply(&mut layer);
        self.sandbox.apply(&mut layer)?;
        Ok(layer)
    }
}

// Value parsers. Each defers to the same `FromStr` the spec file uses, so a
// flag and a spec field can never disagree about syntax.
macro_rules! parser {
    ($name:ident, $ty:ty) => {
        fn $name(s: &str) -> Result<$ty, String> {
            s.parse::<$ty>().map_err(|e| e.to_string())
        }
    };
}

parser!(parse_bytes, Bytes);
parser!(parse_cpu, Cpu);
parser!(parse_duration, Duration);
parser!(parse_isolation, Isolation);
parser!(parse_network, Network);
parser!(parse_seccomp, SeccompProfile);
parser!(parse_mount, Mount);
parser!(parse_allow, AllowRule);
parser!(parse_mode, HandlerMode);

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn completion_is_generated_for_every_shell_and_follows_the_parser() {
        // Generated rather than written by hand, so it cannot drift from the
        // commands the binary accepts. The assertion is that a command added
        // to the enum appears without anyone remembering to add it here.
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::Elvish,
            clap_complete::Shell::PowerShell,
        ] {
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "zygo", &mut out);
            let script = String::from_utf8(out).expect("completions are text");
            assert!(!script.is_empty(), "{shell} produced nothing");
            for command in ["serve", "exec", "up", "down", "agent", "completion"] {
                assert!(script.contains(command), "{shell} is missing `{command}`");
            }
        }
    }

    #[test]
    fn every_subcommand_is_reachable_from_the_parser() {
        // A command that is declared but never dispatched is worse than one
        // that is missing: `--help` promises it.
        let command = Cli::command();
        let names: Vec<String> = command
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for expected in [
            "run",
            "serve",
            "exec",
            "ps",
            "stop",
            "up",
            "down",
            "api",
            "agent",
            "doctor",
            "spec",
            "image",
            "images",
            "pull",
            "backend",
            "bench",
            "completion",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "`{expected}` is not a subcommand; have {names:?}"
            );
        }
    }

    #[test]
    fn run_parses_the_documented_invocation() {
        let cli = Cli::try_parse_from([
            "zygo",
            "run",
            "--mount",
            "./repo:/src:ro",
            "--net",
            "egress",
            "--allow",
            "pypi.org:443",
            "python:3.12",
            "pytest",
            "/src",
        ])
        .unwrap();

        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.image, "python:3.12");
        assert_eq!(args.command, ["pytest", "/src"]);

        let layer = args.to_layer().unwrap();
        assert_eq!(layer.network, Some(Network::Egress));
        assert_eq!(
            layer.mounts.as_ref().unwrap()[0].to_string(),
            "./repo:/src:ro"
        );
        assert_eq!(layer.allow.as_ref().unwrap()[0].to_string(), "pypi.org:443");
    }

    #[test]
    fn trailing_command_arguments_are_not_eaten_as_flags() {
        let cli =
            Cli::try_parse_from(["zygo", "run", "alpine", "sh", "-c", "echo --mem hi"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.command, ["sh", "-c", "echo --mem hi"]);
        assert_eq!(
            args.limits.mem, None,
            "--mem inside the command is not ours"
        );
    }

    #[test]
    fn limit_flags_use_the_spec_syntax() {
        let cli = Cli::try_parse_from([
            "zygo",
            "run",
            "--mem",
            "512M",
            "--cpu",
            "0.5",
            "--timeout",
            "10s",
            "--pids",
            "32",
            "alpine",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.limits.mem, Some(Bytes::from_mib(512)));
        assert_eq!(args.limits.cpu, Some(Cpu(0.5)));
        assert_eq!(args.limits.timeout, Some(Duration::from_secs(10)));
        assert_eq!(args.limits.pids, Some(32));
    }

    #[test]
    fn a_bad_limit_value_is_rejected_at_parse_time() {
        assert!(Cli::try_parse_from(["zygo", "run", "--mem", "512X", "alpine"]).is_err());
        assert!(Cli::try_parse_from(["zygo", "run", "--net", "sideways", "alpine"]).is_err());
        assert!(Cli::try_parse_from(["zygo", "run", "--mount", "no-colon", "alpine"]).is_err());
    }

    #[test]
    fn serve_matches_the_sixty_second_example() {
        let cli = Cli::try_parse_from([
            "zygo",
            "serve",
            "./handler.py",
            "--name",
            "fetch",
            "--net",
            "egress",
            "--allow",
            "example.com:443",
        ])
        .unwrap();
        let Command::Serve(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.name, "fetch");
        let layer = args.to_layer().unwrap();
        assert_eq!(layer.entry, Some(PathBuf::from("./handler.py")));
        assert_eq!(layer.network, Some(Network::Egress));
    }

    #[test]
    fn env_flags_become_a_map_and_reject_bad_syntax() {
        let cli =
            Cli::try_parse_from(["zygo", "run", "--env", "A=1", "--env", "B=2", "alpine"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        let env = args.to_layer().unwrap().env.unwrap();
        assert_eq!(env.get("A").unwrap(), "1");
        assert_eq!(env.get("B").unwrap(), "2");

        let cli = Cli::try_parse_from(["zygo", "run", "--env", "NOEQUALS", "alpine"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        assert!(args.to_layer().is_err());
    }

    #[test]
    fn escape_hatches_are_off_unless_asked_for() {
        let cli = Cli::try_parse_from(["zygo", "run", "alpine"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        let o = args.sandbox.resolve_options();
        assert!(!o.allow_host_net);
        assert!(!o.allow_private_net);
        assert!(!o.allow_unlimited);

        let cli =
            Cli::try_parse_from(["zygo", "run", "--allow-host-net", "--net", "host", "alpine"])
                .unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        assert!(args.sandbox.resolve_options().allow_host_net);
    }

    #[test]
    fn tty_can_be_requested_long_or_short() {
        for argv in [
            vec!["zygo", "run", "--tty", "alpine", "sh"],
            vec!["zygo", "run", "-t", "alpine", "sh"],
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap();
            let Command::Run(args) = cli.command else {
                panic!()
            };
            assert!(args.tty, "{argv:?}");
        }
        let cli = Cli::try_parse_from(["zygo", "run", "alpine", "sh"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        assert!(!args.tty, "a terminal of its own is opt-in");
    }

    #[test]
    fn every_documented_command_is_reachable() {
        for argv in [
            vec!["zygo", "ps"],
            vec!["zygo", "logs", "x", "-f"],
            vec!["zygo", "stop", "--all"],
            vec!["zygo", "top"],
            vec!["zygo", "stats"],
            vec!["zygo", "pull", "python:3.12"],
            vec!["zygo", "images"],
            vec!["zygo", "image", "prune"],
            vec!["zygo", "login", "ghcr.io"],
            vec!["zygo", "up", "-f", "sandbox.toml"],
            vec!["zygo", "down"],
            vec!["zygo", "backend", "install", "gvisor"],
            vec!["zygo", "agent", "test", "./my-agent"],
            vec!["zygo", "doctor"],
            vec!["zygo", "shell", "resize"],
            vec!["zygo", "api"],
            vec!["zygo", "bench", "warm"],
            vec!["zygo", "spec", "validate"],
            vec!["zygo", "exec", "resize", "{}"],
        ] {
            assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?} should parse");
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from(["zygo", "ps", "--json"]).unwrap();
        assert!(cli.json);
    }

    /// `-f` means *file* on spec-reading commands and *follow* on `logs`; both
    /// are in the design doc's CLI reference, so both must work.
    #[test]
    fn dash_f_means_file_or_follow_depending_on_the_command() {
        let cli = Cli::try_parse_from(["zygo", "spec", "-f", "other.toml", "validate"]).unwrap();
        let Command::Spec { file, .. } = cli.command else {
            panic!()
        };
        assert_eq!(file.path(), Some(std::path::Path::new("other.toml")));

        let cli = Cli::try_parse_from(["zygo", "logs", "resize", "-f"]).unwrap();
        let Command::Logs { follow, .. } = cli.command else {
            panic!()
        };
        assert!(follow);

        let cli = Cli::try_parse_from(["zygo", "run", "-f", "s.toml", "alpine"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.spec_file.path(), Some(std::path::Path::new("s.toml")));
    }
}
