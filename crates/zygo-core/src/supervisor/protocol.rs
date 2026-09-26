// SPDX-License-Identifier: Apache-2.0
//! The control protocol: CLI ↔ supervisor.
//!
//! Deliberately **not** the agent wire protocol. The agent runs tenant code, so
//! it must never be able to say "stop that other function" or "serve this spec";
//! keeping the two message sets disjoint makes that a type error rather than a
//! review item. Only the framing is shared ([`crate::protocol::frame`]).
//!
//! The CLI sends the *inputs* to resolution — a spec file's contents and the
//! layer its flags built — rather than a resolved function. The supervisor is
//! the authority on what a function is, so it does the resolving, and the
//! warnings come back over the wire. The cost is that the CLI has to make paths
//! absolute first: the supervisor's working directory is its own.

use serde::{Deserialize, Serialize};

use crate::pool::{Outcome, Status};
use crate::spec::{Layer, Spec};

/// Control protocol version, bumped on any incompatible change.
///
/// Separate from [`crate::protocol::PROTOCOL_VERSION`]: a third-party agent and
/// the CLI evolve independently, and tying them together would mean an agent
/// author had to care about `zygo ps`.
///
/// A supervisor that does not know a request drops the connection, which a
/// client sees as a reset with no explanation; the version is what turns that
/// into "stop the running supervisor". It is checked for equality, so a
/// supervisor left running across an upgrade answers every command with the
/// mismatch — except `zygo run`, which only *asks* whether a supervisor will
/// take the sandbox and, told no, builds one itself.
///
/// - v2: `RUN`, `STARTED` and `RAN` — a one-shot sandbox started by the
///   supervisor on the client's streams.
/// - v3: `PUT_SCRIPT`, `GET_SCRIPT`, `DELETE_SCRIPT` and `SCRIPT` — the
///   content-addressed script store behind `PUT /scripts`.
/// - v4: `SERVE_RUNTIME`, `EXEC_SCRIPT`, `RUNTIMES`, `STOP_RUNTIME` and their
///   answers — runtime pools, where the zygote is warm and the script arrives
///   with the request.
/// - v5: `CREATE_TENANT`, `TENANTS`, `DELETE_TENANT`, and a `tenant` on
///   everything that acts for one — the embedder's customers as a first-class
///   object rather than a word for "function".
/// - v6: `MINT_TOKEN`, `TOKENS`, `REVOKE_TOKEN` and their answer — API tokens
///   that say whose request this is, so the tenant comes from something the
///   caller cannot choose rather than from a header.
/// - v7: `CANCEL` and `CANCELLED` — stopping a request that is already
///   running, and an `Outcome` that carries its own id so the caller has
///   something to name.
/// - v8: `CHUNK`, and a `stream` on `EXEC` and `EXEC_SCRIPT`. A streaming
///   request is answered many times — chunks as they are produced, then the
///   `EXECUTED` — which `RUN` already established as a shape this protocol
///   allows.
/// - v9: `PUT_BLOB`, `GET_BLOB`, `DELETE_BLOB` and a `workspace` on `EXEC` and
///   `EXEC_SCRIPT` — files in and out of one request.
/// - v10: `PUT_SECRET`, `SECRETS`, `DELETE_SECRET` and their answer —
///   per-tenant secrets, encrypted at rest and never read back.
/// - v11: `SET_LIMITS` — a tenant's own limits, which can only narrow what
///   the function or pool was declared with.
/// - v12: `DRAIN` and `DRAINED` — stop admitting, finish what is running, and
///   say whether anything was still going when the grace ran out.
/// - v13: `PUT_DEPS`, `DEPS`, `DELETE_DEPS`, their `DEPENDENCIES` answer, and
///   a `deps` on `SERVE_RUNTIME` — a dependency set built from files that
///   arrived over the API rather than from a path on this host.
/// - v14: a `runtimes` on `STOP` — `zygo stop` reaches the pools as well as
///   the functions, so a name that is a pool no longer answers "no function
///   named …". `DELETE /fn/<name>` leaves it off and keeps its meaning.
pub const CONTROL_VERSION: u32 = 14;

/// The files one request brings with it and takes away (v9).
///
/// `inline` and `blob` are two ways to say the same thing — a tar — and
/// exactly one of them may be set. `inline` is the one-off; `blob` is the
/// shape to build on, because an embedder calling a thousand times with the
/// same fixture should send it once.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRequest {
    /// A tar, base64. JSON cannot carry bytes, and a second transport for the
    /// one route that needs one is more moving parts than the encoding costs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
    /// `sha256:…` of a blob this host already holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// Pack the directory up and send it back with the answer.
    ///
    /// Its own flag rather than something implied by having sent files: a
    /// request that only *reads* its input should not pay to have it packed
    /// again, and most do.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collect: bool,
}

/// CLI → supervisor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "UPPERCASE")]
pub enum Request {
    /// First frame on every connection. Checked before anything else runs.
    Hello { control: u32, client: String },

    /// Warm a function up, replacing any function already under that name.
    Serve {
        name: String,
        /// Contents of the spec file, when one was given.
        spec: Option<Box<Spec>>,
        /// What the flags said.
        layer: Box<Layer>,
        /// The directory the client's relative paths are relative to.
        ///
        /// The supervisor has its own working directory and generally is not
        /// even in the same one, so `--mount ./data:/data` would otherwise
        /// resolve somewhere nobody meant. Carrying it explicitly keeps the
        /// client's view of the filesystem the authority on what it asked for.
        base_dir: std::path::PathBuf,
        /// The `--allow-*` flags: each removes a guarantee, so they travel as
        /// data rather than being inferred from the layer.
        allow_host_net: bool,
        allow_private_net: bool,
        allow_unlimited: bool,
        /// Secret values by name, read from the *client's* environment.
        ///
        /// The client's, not the supervisor's: the supervisor was started by
        /// whichever command needed one first and inherited that environment,
        /// which is nobody's idea of where `STRIPE_KEY` lives. The spec names
        /// the secrets; the shell that runs `zygo serve` supplies them.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        secrets: std::collections::BTreeMap<String, String>,
        /// Leave the function alone if what is registered under this name is
        /// already exactly this: the same resolved spec, the same secret
        /// values, the same handler and requirements bytes on disk.
        ///
        /// `zygo up` sets this, so running it twice is not two deploys. A bare
        /// `zygo serve` leaves it off: the user just said what they want, and
        /// "already running" is not an answer to that.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        if_changed: bool,
        /// Whose function this is. `None` is the operator's own.
        ///
        /// What it decides is the cgroup the sandbox lives in, who may call
        /// it (see `Exec::tenant`), and who `DELETE /tenants/<id>` stops.
        ///
        /// It does **not** namespace the *name*: the registry is keyed by
        /// name alone. That is sound because only an operator serves — a
        /// tenant token registers scripts and calls, and `may_deploy` in the
        /// API refuses it everything that names an image, a mount or a
        /// command. So every name on this host was chosen by one person, and
        /// two customers cannot collide on `resize` because neither of them
        /// picked it. The day a tenant token can serve, the key has to become
        /// the pair; nothing else here changes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },

    /// Call a warm function.
    ///
    /// `tenant` is who is asking. `None` is the operator, who may call
    /// anything; a tenant may only call its own, and a name belonging to
    /// somebody else answers `not_found` — the same answer a name that was
    /// never served gets, because "it exists but is not yours" is a fact about
    /// another customer.
    Exec {
        name: String,
        event: serde_json::Value,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
        /// A name the caller chose for this request, so they can cancel it
        /// before it answers (v7).
        ///
        /// The id is assigned by the supervisor and only reaches the caller
        /// *with the answer*, which is too late to stop the call it belongs
        /// to. Naming the request on the way in is the only way a caller can
        /// cancel its own. See `crate::pool::InFlight`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// Answer with `CHUNK` frames as output is produced, then `EXECUTED`
        /// (v8).
        ///
        /// The one place this protocol's "exactly one response per request"
        /// rule bends, and `RUN` bent it first: a caller that asked to watch
        /// a request gets what it asked for, and one that did not is
        /// byte-for-byte unaffected — the `EXEC` does not even carry the flag.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stream: bool,
        /// Files in and out of this request (v9).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceRequest>,
    },

    /// Run a one-shot sandbox here, on the client's behalf.
    ///
    /// Why a one-shot goes through a supervisor at all: on an ordinary systemd
    /// session `zygo run` cannot build a cgroup where it starts, so it
    /// re-executes itself in a transient scope — about 34 ms of a 45 ms run,
    /// and unavoidable from that process, because cgroup delegation
    /// containment forbids it moving anywhere better (`zygo-cli/src/scope.rs`
    /// has the measurements and the proof). The supervisor is the one process
    /// on the machine already sitting in a delegated, built `zygo.slice`. It
    /// forks the sandbox there instead, with the client's own standard
    /// streams: the client sends its three descriptors over the connection
    /// with `SCM_RIGHTS` immediately after this frame, in order — stdin,
    /// stdout, stderr.
    ///
    /// The image must already be in the store, like `SERVE`; pulling is the
    /// client's job and its output.
    ///
    /// Answered twice: `STARTED` with the sandbox's pid as soon as it exists,
    /// so the client can forward the terminal's signals to it, and `RAN`
    /// when it has exited.
    Run {
        spec: Option<Box<Spec>>,
        layer: Box<Layer>,
        base_dir: std::path::PathBuf,
        allow_host_net: bool,
        allow_private_net: bool,
        allow_unlimited: bool,
        /// The three descriptors that follow are one terminal, not three
        /// streams: the child adopts it as its controlling terminal.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        tty: bool,
        /// The signals the client ignores, bit `n - 1` for signal `n`: the
        /// program gets these as `SIG_IGN` and every other signal as its
        /// default, whatever the supervisor's own dispositions are. What
        /// `nohup zygo run …` means, kept; what a supervisor started with
        /// `&` from a script would otherwise pass on — `SIGINT` ignored, so
        /// that Ctrl-C did nothing in any run it started — dropped.
        #[serde(default)]
        ignored_signals: u64,
    },

    /// Everything `zygo ps` shows.
    List,

    /// Shut a function down, or all of them.
    ///
    /// With `runtimes`, a pool under that name goes too — and with no name,
    /// every pool. It is a flag rather than the default because the two
    /// registries are separate and `DELETE /fn/<name>` must not reach into
    /// the other one: a pool is shared by every tenant on the host, and a
    /// route about one function is not the place to take it down (v14).
    Stop {
        name: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        runtimes: bool,
    },

    /// Ask the supervisor itself to exit once in-flight requests finish.
    Shutdown,

    /// Stop admitting, let what is running finish, then exit.
    ///
    /// Answered when everything has finished or `grace_ms` has passed,
    /// whichever comes first, with a count of what was still running. A deploy
    /// script needs that number: it is the difference between "drained" and
    /// "gave up", and a grace that silently became "for ever" is how a rolling
    /// restart hangs.
    Drain {
        #[serde(default = "default_grace")]
        grace_ms: u64,
    },

    /// Build a dependency set from files that came over the API (v13).
    ///
    /// Answered as soon as the files are on disk, with `building`: a `pip
    /// install` is minutes and a control connection is not the place to spend
    /// them. The same files against the same image are the same id, so a
    /// second caller joins the first one's build rather than starting another.
    PutDeps {
        /// The image the dependencies are built *inside*. Part of the key: a
        /// wheel built for one interpreter fails at import in another.
        image: String,
        /// File name to base64 of its bytes. JSON cannot carry bytes, and a
        /// lockfile is not always UTF-8.
        files: std::collections::BTreeMap<String, String>,
        /// Whose dependency set this is. `None` is the operator's own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },

    /// One dependency set, with its build log (v13).
    ///
    /// `id` absent lists them, which is the same shape `Tenants` and `Tokens`
    /// use: one request type, one response type, and a caller that reads a
    /// list either way.
    Deps {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },

    /// Forget a dependency set (v13). Refused while a pool names it.
    DeleteDeps { id: String },

    /// Liveness probe, used by the client to decide whether an existing socket
    /// belongs to a supervisor that is actually running.
    Ping,

    /// Where to enter a function's sandbox, for `zygo shell`.
    ///
    /// The supervisor answers with a pid and stops there. It does not run the
    /// shell: a terminal would then have to be proxied over this socket, and
    /// the client can do the whole thing itself — it runs as the same user, so
    /// entering the user namespace Zygo created grants it the same capability
    /// set inside that namespace as the supervisor would have had.
    Shell { name: String },

    /// A function's recent log: the zygote's own output and one entry per
    /// request.
    ///
    /// `after` is the sequence number to start from — zero for "the last
    /// `limit`", anything else for "everything since", which is how
    /// `zygo logs -f` follows without a stream.
    Logs {
        name: String,
        #[serde(default)]
        after: u64,
        #[serde(default = "default_log_limit")]
        limit: u32,
        #[serde(default)]
        failed: bool,
        /// Who is asking. See `Exec::tenant`; a log is a function's output,
        /// which is the most directly readable thing a customer owns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },

    /// Make a registered function warm now, without calling it.
    ///
    /// For the moment after a deploy: a function that went cold, or one that
    /// is paused, is brought back so the first real request does not pay for
    /// it. A function that was never served is `not_found` — this warms, it
    /// does not register.
    Warm {
        name: String,
        /// Who is asking. See `Exec::tenant`; warming costs memory, so it is
        /// not something to let one customer spend on another's behalf.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },

    /// Register a script, and get back the name the store gave it.
    ///
    /// `tenant` is whose script it is. The bytes are shared — two tenants
    /// with identical scripts have one file — but the *reference* is not, and
    /// it is what lets deleting a tenant take its code with it.
    ///
    /// Content-addressed, so this is idempotent in the strongest sense: the
    /// same bytes are the same name and the same file, however many tenants
    /// send them and however many times. The answer says whether this call
    /// was the one that wrote it.
    ///
    /// The script is not run, and naming it does not make it runnable: a
    /// request has to name a function or a runtime as well.
    PutScript {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },

    /// Store bytes a caller will name by digest later.
    ///
    /// The same bargain `PutScript` makes, for data rather than code: sent
    /// once, named on every call after. `tar` is base64 on the wire for the
    /// reason `WorkspaceRequest::inline` gives.
    PutBlob { tar: String },

    /// Whether the store holds this blob, and how big it is.
    GetBlob { digest: String },

    /// Forget a blob. `not_found` if it was never stored.
    DeleteBlob { digest: String },

    /// Whether the store holds this digest, and how big it is.
    GetScript { digest: String },

    /// Forget a script. `not_found` if it was never registered.
    DeleteScript { digest: String },

    /// Register a runtime pool and warm `min_warm` zygotes.
    ///
    /// The same inputs as `SERVE` minus the secret *values*: a pool's zygotes
    /// are shared, so no value belongs to the pool. The layer's `secrets`
    /// lists the *names* a request may receive; the values are read from the
    /// calling tenant's store when the request arrives, and the request has a
    /// zygote to itself while they are in place. What it may not carry is
    /// `entry` — the resolver refuses that for a pool, and the refusal is the
    /// isolation claim.
    ServeRuntime {
        name: String,
        spec: Option<Box<Spec>>,
        layer: Box<Layer>,
        base_dir: std::path::PathBuf,
        allow_host_net: bool,
        allow_private_net: bool,
        allow_unlimited: bool,
        /// Whose pool this is. See `Serve::tenant`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
        /// A dependency set this pool's zygotes are built with (v13).
        ///
        /// Named rather than sent: the files were uploaded once and built
        /// once, and a pool that carried them would rebuild on every deploy.
        /// One that is still building is **refused**, not queued — a zygote
        /// warmed without the dependencies it was promised serves requests
        /// that fail at `import`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deps: Option<String>,
    },

    /// Run one script in a pool.
    ///
    /// `script` is the request's own code, as protocol 1.1 carries it: either
    /// `source` or a `digest` the store already holds. The supervisor decides
    /// how it reaches the child.
    ExecScript {
        runtime: String,
        script: crate::protocol::Script,
        event: serde_json::Value,
        timeout_ms: u64,
        /// Whose request this is. `None` is the operator's own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
        /// See `Exec::key`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// See `Exec::stream`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stream: bool,
        /// See `Exec::workspace`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceRequest>,
    },

    /// Everything `zygo top` shows about the pools.
    Runtimes,

    /// Stop a pool and drop its zygotes.
    StopRuntime { name: String },

    /// Register a tenant, or find the one already registered.
    ///
    /// Idempotent: an embedder creating a customer they already have is not
    /// an error worth failing a deploy over.
    CreateTenant { id: String },

    /// Every tenant this host holds, or one of them.
    Tenants { id: Option<String> },

    /// Forget a tenant: its functions and pools stop, and the scripts nothing
    /// else refers to are removed with it.
    DeleteTenant { id: String },

    /// Mint an API token. `tenant: None` mints an operator token.
    ///
    /// The answer carries the secret, and this is the only frame that ever
    /// does: the store keeps a hash, so a second `TOKENS` could not produce
    /// it again even if something asked.
    ///
    /// Minting for a tenant creates that tenant if it does not exist, for the
    /// same reason `PUT_SCRIPT` does — an embedder onboarding a customer
    /// should not have to get two calls in the right order.
    MintToken {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },

    /// Every token this host holds, hashes and all — never a secret.
    ///
    /// Revoked ones included, because "was this token revoked, and when?" is
    /// the question somebody reading a log line has.
    Tokens,

    /// Revoke one by its public id. `not_found` if no token has that id.
    RevokeToken { id: String },

    /// Replace a tenant's limits.
    ///
    /// They can only **narrow**: applied as the minimum of themselves and the
    /// function's or pool's own, on the request's cgroup. A value that could
    /// never take effect — above every ceiling this tenant currently has — is
    /// refused rather than stored, so an operator is told instead of left
    /// believing it did something.
    SetLimits {
        tenant: String,
        limits: Box<crate::tenants::TenantLimits>,
    },

    /// Store one of a tenant's secrets, encrypted at rest.
    ///
    /// The value crosses this socket in the clear, which is what a `0600`
    /// unix socket between two processes of the same user is for; it is
    /// encrypted the moment it lands. There is no `GET`: the store can list
    /// names and cannot produce values, which is the whole point of sealing
    /// them.
    PutSecret {
        tenant: String,
        name: String,
        value: String,
    },

    /// The names a tenant has. Never the values.
    Secrets { tenant: String },

    /// Forget one. `not_found` if the tenant has no secret by that name.
    DeleteSecret { tenant: String, name: String },

    /// Stop a request that is running.
    ///
    /// `id` is what an `Outcome` carries and what `POST /fn/<name>` returns in
    /// `X-Zygo-Request-Id`. Ids are unique across the host, so this names one
    /// request and does not need to say which function it is on. A caller's
    /// own `key` is accepted here too, and is the only name a caller has for a
    /// request that has not answered yet.
    ///
    /// Answered as soon as the kill has been *sent*. The request's own
    /// connection is what reports the outcome, and it is still waiting for the
    /// agent — so a caller that wants to know what the cancelled request
    /// produced reads the answer to its own call, not this one.
    ///
    /// `tenant` is who is asking, and a request belonging to somebody else is
    /// `not_found` — the same answer an id that finished a second ago gets.
    Cancel {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant: Option<String>,
    },
}

fn default_log_limit() -> u32 {
    50
}

/// How long a drain waits for in-flight requests by default.
///
/// Thirty seconds: longer than an ordinary request and shorter than a
/// deployment tool's own patience. A caller with slow requests passes its own.
fn default_grace() -> u64 {
    30_000
}

/// What a `SERVE` did to the name it served.
///
/// A deploy tool reads this to say "3 replaced, 7 unchanged" rather than
/// listing ten green ticks that hide which functions actually restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// Nothing held the name; a sandbox was started.
    #[default]
    Started,
    /// Something held the name and this is its replacement: the new sandbox
    /// was warm before the old one stopped taking requests, and requests the
    /// old one had accepted finish on it.
    Replaced,
    /// The function already registered was identical, so it was kept —
    /// counters, resident pages and all. Only with `if_changed`.
    Unchanged,
}

/// Supervisor → CLI. Exactly one per request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "UPPERCASE")]
pub enum Response {
    /// Answer to `Hello`.
    Welcome {
        control: u32,
        version: String,
        pid: u32,
    },

    /// A function is warm and ready.
    Served {
        name: String,
        runtime: String,
        rss_kb: u64,
        imports_ms: f64,
        /// How long the warm-up took, which is the cold start this saves later.
        warm_ms: f64,
        /// Non-fatal resolution notes. The CLI prints them.
        warnings: Vec<String>,
        /// What serving did to whatever held the name before.
        #[serde(default)]
        change: Change,
    },

    /// A request ran. `outcome.succeeded()` says whether the handler liked it.
    Executed {
        outcome: Box<Outcome>,
    },

    /// A `RUN` sandbox exists and is about to `execve`.
    ///
    /// Sent before the sandbox runs so the client can forward its terminal's
    /// signals to the right pid; a Ctrl-C at the client would otherwise reach
    /// nothing, because the sandbox is the supervisor's child, not the
    /// client's. The pid is init's, as the client sees it; init leads its own
    /// process group, so `kill(-pid)` reaches everything it forked.
    Started {
        pid: u32,
    },

    /// A `RUN` sandbox has exited. The same fields `zygo run --outcome`
    /// writes, because they are the same facts.
    Ran {
        exit_code: i32,
        /// The supervisor's deadline killed it.
        timed_out: bool,
        /// The kernel killed something in it for running out of memory.
        oom_killed: bool,
        peak_rss_kb: u64,
        wall_ms: f64,
    },

    Functions {
        functions: Vec<Status>,
    },

    Stopped {
        names: Vec<String>,
    },

    /// Answer to `Shell`: the sandbox's first process, on the host.
    ///
    /// `/proc/<pid>/ns/*` is every namespace the sandbox is made of. The pid is
    /// only useful to a client on the same host and as the same user, which is
    /// the only kind this socket has.
    Sandbox {
        name: String,
        pid: u32,
        /// The function's `workdir`, so the shell starts where a request does.
        workdir: std::path::PathBuf,
    },

    /// Answer to `Logs`. `next` is what to pass as `after` to continue.
    Logs {
        name: String,
        entries: Vec<crate::pool::LogEntry>,
        next: u64,
    },

    /// Answer to `Warm`: the function's state afterwards.
    Warmed {
        name: String,
        state: crate::sandbox::SandboxState,
    },

    /// A runtime pool is registered and its floor is warm.
    RuntimeServed {
        name: String,
        /// What the agent announced, e.g. `python/3.12.4`.
        runtime: String,
        /// Zygotes warmed, which is the pool's `min_warm`.
        warm: u32,
        rss_kb: u64,
        imports_ms: f64,
        warm_ms: f64,
        warnings: Vec<String>,
        #[serde(default)]
        change: Change,
    },

    /// Answer to `Runtimes`.
    Runtimes {
        runtimes: Vec<super::runtime::RuntimeStatus>,
    },

    /// Answer to `CreateTenant`, `Tenants` and `DeleteTenant`.
    ///
    /// One shape for all three, because the interesting fields are the same
    /// and a caller that has to branch on the response type to read an id is
    /// a caller writing three code paths for one idea.
    Tenants {
        tenants: Vec<crate::tenants::Tenant>,
        /// For a create: the tenant was already registered.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        existed: bool,
        /// For a delete: the scripts removed because nothing else referred to
        /// them, and the functions and pools stopped with the tenant.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed_scripts: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        stopped: Vec<String>,
    },

    /// One piece of a streaming request's output (v8).
    ///
    /// Sent before the `EXECUTED` it belongs to, and only when the request
    /// asked. The connection carries nothing else in the meantime — a control
    /// connection serves one request at a time — so there is no id to
    /// correlate on.
    Chunk {
        stream: crate::protocol::Stream,
        data: String,
    },

    /// Answer to `PutSecret`, `Secrets` and `DeleteSecret`.
    ///
    /// Names only, and there is no shape here that could carry a value. That
    /// is deliberate: a response type with an optional value field is one
    /// somebody eventually fills in.
    Secrets {
        names: Vec<String>,
    },

    /// Answer to `Drain`: admitting has stopped, and this is what was left.
    ///
    /// `in_flight` is zero when everything finished inside the grace, and the
    /// number still running when it did not — which is the caller's cue that
    /// it timed out rather than drained.
    Drained {
        in_flight: u32,
        grace_ms: u64,
    },

    /// Answer to `Cancel`: the kill was sent.
    ///
    /// `started` is whether the request had reached the handler. `false` means
    /// the child was still parked waiting for `GO`, so it is cancelled without
    /// having run one instruction of it — the better outcome, and worth
    /// telling a caller apart from the other one.
    Cancelled {
        id: String,
        started: bool,
    },

    /// Answer to `MintToken`, `Tokens` and `RevokeToken`.
    ///
    /// One shape for all three, like `Tenants`. A mint answers with the one
    /// token it made and the secret beside it; a list answers with all of them
    /// and no secret.
    Tokens {
        tokens: Vec<crate::tokens::Token>,
        /// The secret, in the clear, on the one frame that carries it.
        ///
        /// Nothing stores this. A client that does not keep it has lost the
        /// token, and the only remedy is to mint another and revoke this one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    },

    /// Answer to `PutDeps`, `Deps` and `DeleteDeps` (v13).
    ///
    /// One shape for all three, like `Tenants` and `Tokens`. A `PutDeps`
    /// answers with the one it made or found; a list answers with all of them
    /// and no log, because a list of build logs is a response nobody wanted.
    Dependencies {
        deps: Vec<crate::deps::Status>,
        /// The build's output, on the one frame that carries it: the answer to
        /// a request that named a single id. Empty for a list.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        log: String,
        /// This host already had this dependency set.
        ///
        /// The observable half of the content-addressed key, as `Script` has:
        /// two embedders who send identical files against identical images
        /// share one build, and the second is told rather than left to assume.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        existed: bool,
    },

    /// Answer to `PutScript` and `GetScript`.
    Script {
        /// `sha256:…`, which is the script's name everywhere else.
        digest: String,
        size: u64,
        /// The store already held these bytes.
        ///
        /// The observable half of deduplication: two tenants that register
        /// byte-identical scripts get one file, and the second is told so
        /// rather than being left to assume it. Always true for a `GetScript`,
        /// which answers `not_found` otherwise.
        existed: bool,
    },

    Pong,

    Ok,

    /// The tenant is at its concurrency limit and its queue is full.
    ///
    /// The API's `429`: a distinct response rather than an error,
    /// because the caller's correct reaction is to retry rather than to give up,
    /// and a platform routing to a second machine needs to tell the two apart.
    Busy {
        name: String,
        in_flight: u32,
        queued: u32,
        limit: u32,
    },

    Error {
        code: ControlError,
        message: String,
    },
}

/// Stable machine-readable failure kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlError {
    /// The client speaks a control version this supervisor does not.
    VersionMismatch,
    /// No function registered under that name.
    NotFound,
    /// The spec did not resolve.
    BadSpec,
    /// Warming the sandbox failed.
    WarmFailed,
    /// The request reached the function but the call failed.
    CallFailed,
    /// A frame arrived out of order — anything before `Hello`, for instance.
    BadMessage,
    /// The connection is not from the user who owns the supervisor.
    Unauthorised,
    /// A value was asked for that is above a ceiling somebody else set.
    ///
    /// Its own code rather than `BadSpec`, because the request was *well
    /// formed* and the answer is "not that much", which an API maps to `422`
    /// rather than `400`.
    AboveCeiling,
    /// A pool named a dependency set that is still being built (v13).
    ///
    /// Its own code because the caller's correct reaction is unlike every
    /// other failure here: wait and send the same request again. An API maps
    /// it to `503` with a `Retry-After`, and nothing about the request needs
    /// changing.
    DepsBuilding,
}

impl ControlError {
    pub const fn as_str(self) -> &'static str {
        match self {
            ControlError::VersionMismatch => "version_mismatch",
            ControlError::NotFound => "not_found",
            ControlError::BadSpec => "bad_spec",
            ControlError::WarmFailed => "warm_failed",
            ControlError::CallFailed => "call_failed",
            ControlError::BadMessage => "bad_message",
            ControlError::Unauthorised => "unauthorised",
            ControlError::AboveCeiling => "above_ceiling",
            ControlError::DepsBuilding => "deps_building",
        }
    }
}

impl Response {
    /// Build an error response from anything printable.
    pub fn error(code: ControlError, message: impl std::fmt::Display) -> Response {
        Response::Error {
            code,
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_request(r: &Request) -> Request {
        let bytes = crate::protocol::frame::encode(r).expect("encode");
        // Skip the 4-byte length prefix the framing added.
        crate::protocol::frame::decode(&bytes[4..]).expect("decode")
    }

    fn roundtrip_response(r: &Response) -> Response {
        let bytes = crate::protocol::frame::encode(r).expect("encode");
        crate::protocol::frame::decode(&bytes[4..]).expect("decode")
    }

    #[test]
    fn every_request_survives_the_wire() {
        let requests = [
            Request::Hello {
                control: CONTROL_VERSION,
                client: "zygo 0.1.0".into(),
            },
            Request::Serve {
                name: "resize".into(),
                spec: None,
                layer: Box::new(Layer {
                    image: Some("python:3.12-slim".into()),
                    ..Default::default()
                }),
                base_dir: "/home/me/project".into(),
                allow_host_net: false,
                allow_private_net: true,
                allow_unlimited: false,
                secrets: std::collections::BTreeMap::from([(
                    "STRIPE_KEY".to_string(),
                    "sk_test_123".to_string(),
                )]),
                if_changed: true,
                tenant: Some("acme".into()),
            },
            Request::Exec {
                name: "resize".into(),
                event: serde_json::json!({ "url": "https://example.com" }),
                timeout_ms: 30_000,
                tenant: Some("acme".into()),
                key: Some("job-4711".into()),
                stream: true,
                workspace: None,
            },
            Request::List,
            Request::Run {
                spec: None,
                layer: Box::default(),
                base_dir: "/work".into(),
                allow_host_net: false,
                allow_private_net: true,
                allow_unlimited: false,
                tty: false,
                ignored_signals: 1 << (libc::SIGHUP - 1),
            },
            Request::Stop {
                name: Some("resize".into()),
                runtimes: false,
            },
            Request::Stop {
                name: None,
                runtimes: true,
            },
            Request::Shutdown,
            Request::Drain { grace_ms: 5_000 },
            Request::Ping,
            Request::Warm {
                name: "resize".into(),
                tenant: None,
            },
            Request::Shell {
                name: "resize".into(),
            },
            Request::Logs {
                name: "resize".into(),
                after: 17,
                limit: 20,
                failed: true,
                tenant: Some("acme".into()),
            },
            Request::PutScript {
                source: "def handler(event):\n    return event\n".into(),
                tenant: Some("acme".into()),
            },
            Request::GetScript {
                digest: "sha256:abc".into(),
            },
            Request::DeleteScript {
                digest: "sha256:abc".into(),
            },
            Request::ServeRuntime {
                name: "py312".into(),
                spec: None,
                layer: Box::new(Layer {
                    image: Some("python:3.12-slim".into()),
                    min_warm: Some(2),
                    ..Default::default()
                }),
                base_dir: "/srv".into(),
                allow_host_net: false,
                allow_private_net: false,
                allow_unlimited: false,
                tenant: Some("acme".into()),
                deps: Some(format!("deps_{}", "a".repeat(32))),
            },
            Request::ExecScript {
                runtime: "py312".into(),
                script: crate::protocol::Script::inline("def handler(e):\n    return e\n"),
                event: serde_json::json!({ "n": 1 }),
                timeout_ms: 30_000,
                tenant: Some("acme".into()),
                key: None,
                stream: false,
                workspace: None,
            },
            Request::CreateTenant { id: "acme".into() },
            Request::Tenants { id: None },
            Request::DeleteTenant { id: "acme".into() },
            Request::MintToken {
                tenant: Some("acme".into()),
            },
            Request::MintToken { tenant: None },
            Request::Tokens,
            Request::RevokeToken {
                id: "tok_1a2b3c4d5e6f".into(),
            },
            Request::Cancel {
                id: "r-0001".into(),
                tenant: Some("acme".into()),
            },
            Request::PutSecret {
                tenant: "acme".into(),
                name: "STRIPE_KEY".into(),
                value: "sk_live_abc".into(),
            },
            Request::SetLimits {
                tenant: "acme".into(),
                limits: Box::new(crate::tenants::TenantLimits {
                    mem: Some(crate::spec::Bytes::from_mib(64)),
                    ..Default::default()
                }),
            },
            Request::Secrets {
                tenant: "acme".into(),
            },
            Request::DeleteSecret {
                tenant: "acme".into(),
                name: "STRIPE_KEY".into(),
            },
            Request::Runtimes,
            Request::StopRuntime {
                name: "py312".into(),
            },
        ];
        for r in &requests {
            assert_eq!(&roundtrip_request(r), r, "{r:?}");
        }
    }

    #[test]
    fn every_response_survives_the_wire() {
        let responses = [
            Response::Welcome {
                control: CONTROL_VERSION,
                version: "0.1.0".into(),
                pid: 42,
            },
            Response::Served {
                name: "resize".into(),
                runtime: "python3.12".into(),
                rss_kb: 15_000,
                imports_ms: 120.5,
                warm_ms: 310.0,
                warnings: vec!["scratch is larger than memory".into()],
                change: Change::Replaced,
            },
            Response::Functions {
                functions: vec![Status {
                    name: "resize".into(),
                    tenant: "acme".into(),
                    image: String::new(),
                    state: crate::sandbox::SandboxState::Warm,
                    runtime: "python3.12".into(),
                    rss_kb: 15_000,
                    imports_ms: 120.5,
                    requests: 9,
                    failures: 1,
                }],
            },
            Response::Stopped {
                names: vec!["resize".into()],
            },
            Response::Warmed {
                name: "resize".into(),
                state: crate::sandbox::SandboxState::Warm,
            },
            Response::Script {
                digest: "sha256:abc".into(),
                size: 41,
                existed: true,
            },
            Response::Tenants {
                tenants: vec![crate::tenants::Tenant {
                    id: "acme".into(),
                    created_ms: 1_700_000_000_000,
                    scripts: ["sha256:abc".to_string()].into_iter().collect(),
                    limits: crate::tenants::TenantLimits {
                        pids: Some(64),
                        ..Default::default()
                    },
                }],
                existed: true,
                removed_scripts: vec!["sha256:abc".into()],
                stopped: vec!["resize".into()],
            },
            Response::Cancelled {
                id: "r-0001".into(),
                started: true,
            },
            Response::Drained {
                in_flight: 0,
                grace_ms: 30_000,
            },
            Response::Secrets {
                names: vec!["STRIPE_KEY".into()],
            },
            Response::Chunk {
                stream: crate::protocol::Stream::Stdout,
                data: "halfway\n".into(),
            },
            Response::Tokens {
                tokens: vec![crate::tokens::Token {
                    id: "tok_1a2b3c4d5e6f".into(),
                    kind: crate::tokens::TokenKind::Tenant {
                        tenant: "acme".into(),
                    },
                    created_ms: 1_700_000_000_000,
                    revoked_ms: None,
                    hash: "sha256:abc".into(),
                }],
                secret: Some("zygo_deadbeef".into()),
            },
            Response::RuntimeServed {
                name: "py312".into(),
                runtime: "python/3.12.4".into(),
                warm: 2,
                rss_kb: 15_000,
                imports_ms: 120.5,
                warm_ms: 410.0,
                warnings: vec![],
                change: Change::Started,
            },
            Response::Runtimes {
                runtimes: vec![crate::supervisor::runtime::RuntimeStatus {
                    name: "py312".into(),
                    tenant: "acme".into(),
                    image: "python:3.12-slim".into(),
                    runtime: "python/3.12.4".into(),
                    warm: 2,
                    paused: 1,
                    cold: 1,
                    min_warm: 2,
                    max_warm: 4,
                    in_flight: 3,
                    queued: 0,
                    requests: 91,
                    failures: 2,
                    rss_kb: 30_000,
                    uptime_s: 42,
                }],
            },
            Response::Sandbox {
                name: "resize".into(),
                pid: 4242,
                workdir: "/zygo".into(),
            },
            Response::Logs {
                name: "resize".into(),
                entries: vec![
                    crate::pool::LogEntry {
                        seq: 3,
                        at_ms: 1_700_000_000_000,
                        kind: crate::pool::LogKind::Zygote,
                        text: "warming up".into(),
                    },
                    crate::pool::LogEntry {
                        seq: 4,
                        at_ms: 1_700_000_000_500,
                        kind: crate::pool::LogKind::Request {
                            id: "01f3".into(),
                            exit_code: 1,
                            timed_out: false,
                            wall_ms: 12.5,
                            error: Some("ZeroDivisionError".into()),
                            stderr: "Traceback".into(),
                        },
                        text: String::new(),
                    },
                ],
                next: 5,
            },
            Response::Busy {
                name: "resize".into(),
                in_flight: 4,
                queued: 16,
                limit: 4,
            },
            Response::Pong,
            Response::Ok,
            Response::error(ControlError::NotFound, "no function named `resize`"),
        ];
        for r in &responses {
            assert_eq!(&roundtrip_response(r), r, "{r:?}");
        }
    }

    #[test]
    fn an_outcome_crosses_the_control_socket_intact() {
        // `zygo exec` prints this, so a field lost in transit is a wrong answer
        // rather than an error.
        let outcome = Outcome {
            tenant: "default".into(),
            function: "resize".into(),
            script: None,
            id: "00000001".into(),
            cancelled: false,
            stuck: false,
            workspace: None,
            exit_code: 0,
            result: serde_json::json!({ "ok": true, "n": 3 }),
            stdout: "hello\n".into(),
            stderr: String::new(),
            error: None,
            metrics: crate::protocol::Metrics {
                wall_ms: 1.25,
                ..Default::default()
            },
            timed_out: false,
        };
        let sent = Response::Executed {
            outcome: Box::new(outcome.clone()),
        };
        match roundtrip_response(&sent) {
            Response::Executed { outcome: got } => {
                assert_eq!(*got, outcome);
                assert!(got.succeeded());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failed_outcome_keeps_its_error_across_the_wire() {
        let outcome = Outcome {
            tenant: "acme".into(),
            function: "py312".into(),
            script: Some("sha256:abc".into()),
            id: "00000002".into(),
            cancelled: true,
            stuck: false,
            workspace: None,
            exit_code: 1,
            result: serde_json::Value::Null,
            stdout: String::new(),
            stderr: "Traceback...\n".into(),
            error: Some("ZeroDivisionError: division by zero".into()),
            metrics: crate::protocol::Metrics::default(),
            timed_out: false,
        };
        let sent = Response::Executed {
            outcome: Box::new(outcome),
        };
        match roundtrip_response(&sent) {
            Response::Executed { outcome } => {
                assert!(!outcome.succeeded());
                assert_eq!(
                    outcome.error.as_deref(),
                    Some("ZeroDivisionError: division by zero")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_tag_names_are_the_stable_part_of_the_protocol() {
        // A third-party client reads these strings. Renaming a variant must not
        // silently rename the wire.
        let json = serde_json::to_value(Request::Ping).expect("json");
        assert_eq!(json["type"], "PING");

        let json = serde_json::to_value(Response::Busy {
            name: "x".into(),
            in_flight: 1,
            queued: 0,
            limit: 1,
        })
        .expect("json");
        assert_eq!(json["type"], "BUSY");

        let json =
            serde_json::to_value(Response::error(ControlError::NotFound, "x")).expect("json");
        assert_eq!(json["type"], "ERROR");
        assert_eq!(json["code"], "not_found");
    }

    #[test]
    fn error_codes_have_stable_names() {
        for (code, want) in [
            (ControlError::VersionMismatch, "version_mismatch"),
            (ControlError::NotFound, "not_found"),
            (ControlError::BadSpec, "bad_spec"),
            (ControlError::WarmFailed, "warm_failed"),
            (ControlError::CallFailed, "call_failed"),
            (ControlError::BadMessage, "bad_message"),
            (ControlError::Unauthorised, "unauthorised"),
        ] {
            assert_eq!(code.as_str(), want);
            assert_eq!(
                serde_json::to_value(code).expect("json"),
                serde_json::Value::String(want.into()),
                "`as_str` and serde must not drift apart"
            );
        }
    }

    #[test]
    fn a_serve_with_no_secrets_does_not_put_an_empty_map_on_the_wire() {
        // Keeps the common frame small, and keeps a client that predates the
        // field able to read the JSON a newer one produces.
        let request = Request::Serve {
            name: "f".into(),
            spec: None,
            layer: Box::default(),
            base_dir: "/p".into(),
            allow_host_net: false,
            allow_private_net: false,
            allow_unlimited: false,
            secrets: Default::default(),
            if_changed: false,
            tenant: None,
        };
        // The *top-level* key, not the word: the layer inside has a `secrets`
        // field of its own (the names), and that one is allowed to be there.
        let json = serde_json::to_value(&request).expect("json");
        assert!(json.get("secrets").is_none(), "{json}");
        assert!(json.get("if_changed").is_none(), "{json}");
        assert_eq!(roundtrip_request(&request), request);
    }

    #[test]
    fn a_stop_from_before_v14_still_parses_and_means_functions_only() {
        // `DELETE /fn/<name>` and a v13 client both send a `STOP` with no
        // `runtimes`, and both mean the function. The flag is off the wire
        // when it is off, so the frame they send is the frame they sent.
        let json = serde_json::json!({ "type": "STOP", "name": "resize" });
        let parsed: Request = serde_json::from_value(json).expect("a v13 STOP");
        assert_eq!(
            parsed,
            Request::Stop {
                name: Some("resize".into()),
                runtimes: false,
            }
        );
        let request = Request::Stop {
            name: Some("resize".into()),
            runtimes: false,
        };
        let json = serde_json::to_value(&request).expect("json");
        assert!(json.get("runtimes").is_none(), "{json}");

        let json = serde_json::to_value(Request::Stop {
            name: None,
            runtimes: true,
        })
        .expect("json");
        assert_eq!(json["runtimes"], true);
    }

    #[test]
    fn a_frame_from_before_blue_green_still_parses() {
        // Both sides default the fields blue/green added, so a client and a
        // supervisor from either side of that change keep understanding each
        // other without a control-version bump.
        let serve = br#"{"type":"SERVE","name":"f","spec":null,"layer":{},"base_dir":"/p",
            "allow_host_net":false,"allow_private_net":false,"allow_unlimited":false}"#;
        match crate::protocol::frame::decode::<Request>(serve).expect("decode") {
            Request::Serve { if_changed, .. } => assert!(!if_changed),
            other => panic!("{other:?}"),
        }

        let served = br#"{"type":"SERVED","name":"f","runtime":"exec","rss_kb":1,
            "imports_ms":0.0,"warm_ms":2.0,"warnings":[]}"#;
        match crate::protocol::frame::decode::<Response>(served).expect("decode") {
            Response::Served { change, .. } => assert_eq!(change, Change::Started),
            other => panic!("{other:?}"),
        }

        // And the names a deploy script will grep for.
        for (change, want) in [
            (Change::Started, "started"),
            (Change::Replaced, "replaced"),
            (Change::Unchanged, "unchanged"),
        ] {
            assert_eq!(
                serde_json::to_value(change).expect("json"),
                serde_json::Value::String(want.into())
            );
        }
    }

    #[test]
    fn an_unknown_message_type_is_rejected_rather_than_guessed() {
        let body = br#"{"type":"DESTROY_EVERYTHING"}"#;
        assert!(crate::protocol::frame::decode::<Request>(body).is_err());
    }

    #[test]
    fn an_agent_wire_message_is_not_a_control_message() {
        // The whole point of two protocols: tenant code that gets hold of the
        // agent socket still cannot say anything the supervisor will act on.
        let exec = crate::protocol::Message::Exec {
            script: None,
            id: "1".into(),
            event: serde_json::Value::Null,
            timeout_ms: 1000,
            env_overrides: Default::default(),
            stream: false,
            workspace: None,
        };
        let bytes = crate::protocol::frame::encode(&exec).expect("encode");
        assert!(
            crate::protocol::frame::decode::<Request>(&bytes[4..]).is_err(),
            "an agent `EXEC` must not deserialise as a control request"
        );
    }
}
