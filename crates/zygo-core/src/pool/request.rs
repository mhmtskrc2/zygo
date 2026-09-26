// SPDX-License-Identifier: Apache-2.0
//! One request, from the moment it is sent to the moment it is answered.
//!
//! What both warm shapes share about a request in flight: its id, the
//! registry a cancel finds it in ([`InFlight`], `Requests`), the cgroup it
//! is admitted into and killed through, the files it brings
//! ([`Workspace`]), and [`Call`] — everything a caller can ask for at once.
//! The agent path adds a protocol on top (`warm_fn`); the warm-exec path
//! adds a fresh process (`warm_exec`); neither owns anything here.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One request that has been sent and not yet answered.
///
/// A cancel arrives on a different connection, on a different thread, while
/// the request's own thread is blocked waiting for `DONE`. This is what the
/// two share: a flag the waiting thread reads once it wakes, and where to kill
/// the work.
///
/// **The kill is the cancel.** Writing `cgroup.kill` from outside the sandbox
/// is what stops the request; the `CANCEL` frame only tells the agent why, and
/// an agent that ignores it changes nothing. Trusting tenant code to stop
/// itself on request is trusting the blast radius to contain itself — the same
/// argument that makes `timeout_ms` a courtesy rather than a control.
#[derive(Debug, Default)]
pub struct InFlight {
    /// A name the *caller* chose for this request, if they chose one.
    ///
    /// The id is assigned here and only reaches the caller with the answer,
    /// which is too late to cancel the call it belongs to. A caller that wants
    /// to be able to stop its own request before it finishes has to name it on
    /// the way in, and this is that name.
    ///
    /// It is not an id and does not have to be unique: two requests sharing a
    /// key are two requests one cancel stops, which is what a caller who
    /// reused a key meant. Ownership is what keeps it safe — a key only ever
    /// matches the requests of whoever sent it.
    key: Option<String>,
    /// Whose request this is, when it came from a tenant.
    ///
    /// Here because a request id is a **counter**, not a secret: anybody who
    /// has seen one can count. That is fine for an id, which is a name rather
    /// than a capability — the same argument a script digest gets — but it
    /// means the owner has to be recorded, or a tenant sharing a pool could
    /// cancel another tenant's request by counting up from their own.
    owner: Option<String>,
    /// Somebody asked for this request to stop.
    cancelled: AtomicBool,
    /// Where the work is, once there is any.
    ///
    /// `None` between the `EXEC` and the `GO`: the child exists but has run
    /// nothing, and a cancel that lands in that window is answered by never
    /// sending `GO` rather than by killing something that has not started.
    target: Mutex<Option<Target>>,
}

#[derive(Debug)]
struct Target {
    cgroup: Option<PathBuf>,
    host_pid: u32,
}

impl InFlight {
    /// Whether `caller` may cancel this. The operator may cancel anything on
    /// their own host; a tenant may cancel only their own.
    pub(super) fn is_for(&self, caller: Option<&str>) -> bool {
        match (caller, self.owner.as_deref()) {
            (None, _) => true,
            (Some(caller), Some(owner)) => caller == owner,
            (Some(_), None) => false,
        }
    }

    /// Whether `name` is this request's key.
    fn keyed(&self, name: &str) -> bool {
        self.key.as_deref() == Some(name)
    }

    pub(super) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Mark it cancelled and kill it if there is anything to kill.
    ///
    /// Returns whether the work had started. `false` means the child is still
    /// parked waiting for `GO`, and the request's own thread will deal with it
    /// — which is the better outcome, because then no tenant code ran at all.
    pub(super) fn cancel(&self) -> bool {
        self.cancelled.store(true, Ordering::SeqCst);
        match self.target.lock().expect("target").as_ref() {
            Some(target) => {
                kill_request(target.cgroup.as_deref(), target.host_pid);
                true
            }
            None => false,
        }
    }

    pub(super) fn running_at(&self, cgroup: Option<&std::path::Path>, host_pid: u32) {
        *self.target.lock().expect("target") = Some(Target {
            cgroup: cgroup.map(PathBuf::from),
            host_pid,
        });
    }
}

/// Every request this sandbox has sent and not yet answered.
///
/// Keyed by the request id, which is unique across the host, so the supervisor
/// can ask each function and pool in turn whether an id is theirs rather than
/// keeping a second index that could disagree with this one.
#[derive(Debug, Default)]
pub(super) struct Requests(Mutex<BTreeMap<String, Arc<InFlight>>>);

impl Requests {
    pub(super) fn start(&self, id: &str, owner: Option<&str>, key: Option<&str>) -> Arc<InFlight> {
        let entry = Arc::new(InFlight {
            owner: owner.map(str::to_string),
            key: key.map(str::to_string),
            ..InFlight::default()
        });
        self.0
            .lock()
            .expect("in flight")
            .insert(id.to_string(), Arc::clone(&entry));
        entry
    }

    pub(super) fn finish(&self, id: &str) {
        self.0.lock().expect("in flight").remove(id);
    }

    /// Find a request by its id, or by the key its caller gave it.
    ///
    /// The id first, because it is the unambiguous name. A key is searched for
    /// only when no id matches, so a caller cannot shadow somebody's id with a
    /// key — and could not reach it anyway, since a key only matches a request
    /// they are allowed to cancel.
    pub(super) fn get(&self, name: &str) -> Option<Arc<InFlight>> {
        let requests = self.0.lock().expect("in flight");
        if let Some(entry) = requests.get(name) {
            return Some(Arc::clone(entry));
        }
        requests.values().find(|e| e.keyed(name)).map(Arc::clone)
    }

    pub(super) fn ids(&self) -> Vec<String> {
        self.0.lock().expect("in flight").keys().cloned().collect()
    }
}

/// Where a streaming request's output goes as it is produced.
///
/// Called on the thread serving the request, between the `EXEC` and the
/// `DONE`, so it must not block for long: whatever is on the other end is
/// holding up the request that is writing to it. The supervisor's own
/// implementation writes one control frame and returns.
pub type ChunkSink<'a> = &'a (dyn Fn(crate::protocol::Stream, &str) + Send + Sync);

/// What a request brings with it and takes away.
///
/// `inbox` is a tar the caller sent, unpacked into the request's own directory
/// before the handler runs. `collect` asks for the directory back as a tar
/// when the handler is done — which is a separate question, because a request
/// that only *reads* its input should not pay to have it packed again.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    pub inbox: Option<Vec<u8>>,
    pub collect: bool,
}

impl Workspace {
    /// Whether this request needs a directory at all.
    pub(super) fn wanted(&self) -> bool {
        self.inbox.is_some() || self.collect
    }
}

/// One request's directory inside the sandbox, removed when this is dropped.
///
/// Held for the whole request by the thread serving it, so the directory goes
/// on every path out — including the ones where the request failed or was
/// killed. A workspace that outlived its request would be a neighbour's to
/// find, and the window is meant to be one request long.
pub(super) struct WorkspaceLease {
    /// The directory as *this* process can reach it, through the sandbox's
    /// `/proc/<pid>/root`.
    pub(super) host: PathBuf,
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.host);
    }
}

/// Takes a request off the in-flight list however its thread leaves.
///
/// A `Drop` rather than a call at the end, because `call_script_timed` has
/// eight early returns and the one that forgot would leave an id that a later
/// cancel could find and kill somebody else's child with — the ids are not
/// reused, but the entry would name a cgroup that is.
pub(super) struct RequestLease<'a> {
    pub(super) requests: &'a Requests,
    pub(super) id: &'a str,
}

impl Drop for RequestLease<'_> {
    fn drop(&mut self) {
        self.requests.finish(self.id);
    }
}

/// Everything one request can carry.
///
/// The whole of what [`Function::call_full`](super::Function::call_full) and
/// [`WarmFn::call_streaming`](super::WarmFn::call_streaming) take; every
/// narrower `call_*` fills in the fields it does not name. One struct rather
/// than nine arguments, so a call site says which of them it is setting.
pub struct Call<'a> {
    /// The event the handler receives.
    pub event: serde_json::Value,
    /// A script that did not come with the zygote — the runtime-pool shape
    /// (protocol 1.1). `None` runs the handler the function was served with.
    pub script: Option<crate::protocol::Script>,
    /// How long the caller waits. The function's own budget is the ceiling.
    pub timeout: std::time::Duration,
    /// Whose request this is, which decides who may cancel it. See
    /// [`WarmFn::call_script_for`](super::WarmFn::call_script_for).
    pub caller: Option<&'a str>,
    /// A name the caller chose, so it can cancel the request before the id
    /// reaches it. See [`InFlight`].
    pub key: Option<&'a str>,
    /// Where output goes as it is produced (protocol 1.3). `None` is the
    /// ordinary path: the child captures its output as it always has, and
    /// not one extra frame crosses the socket.
    pub sink: Option<ChunkSink<'a>>,
    /// Files the request brings, and whether it wants the directory back.
    pub workspace: Option<Workspace>,
    /// A tenant's own limits, when they narrow the function's.
    pub tenant_limits: Option<crate::tenants::TenantLimits>,
    /// A runtime pool request's own secret values — the calling tenant's,
    /// read from the store for this one request — or `None` for a
    /// function's request, which uses the values it was served with.
    pub secrets: Option<BTreeMap<String, String>>,
}

impl<'a> Call<'a> {
    /// A plain request: the event and how long to wait, nothing else.
    pub fn new(event: serde_json::Value, timeout: std::time::Duration) -> Call<'a> {
        Call {
            event,
            timeout,
            script: None,
            caller: None,
            key: None,
            sink: None,
            workspace: None,
            tenant_limits: None,
            secrets: None,
        }
    }
}

/// The pid of `host_pid` as seen in its own innermost pid namespace.
///
/// `NSpid` lists a process's pid in each namespace it belongs to, outermost
/// first, so the last entry is the number the process sees for itself. A kernel
/// without `NSpid` (below 4.1) reports nothing, which is treated as "cannot
/// translate" rather than guessed at.
pub(super) fn innermost_ns_pid(host_pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{host_pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("NSpid:"))?
        .split_whitespace()
        .next_back()?
        .parse()
        .ok()
}

/// Monotonic request ids. Short, because they become cgroup directory names.
pub(super) fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{:08x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// `VmRSS` of a process, for `zygo ps`.
///
/// `None` where there is no `/proc` to read it from, which leaves the caller
/// with the last figure it knew rather than a zero that would read as "this
/// function is using no memory".
#[cfg(not(target_os = "linux"))]
pub(super) fn resident_kb(_pid: u32) -> Option<u64> {
    None
}

/// `VmRSS` of a process, for `zygo ps`.
#[cfg(target_os = "linux")]
pub(super) fn resident_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Put a request's process in its own cgroup. Returns the directory to remove
/// afterwards, if one was created.
///
/// `host_pid` must be a pid in *this* process's namespace — translated first on
/// the agent path (`FORKED`'s pid is in the sandbox's pid namespace, see
/// `spec/protocol.md`), native on the warm-exec path. Failing to move it is
/// not worth failing the request over: the process is still inside the
/// sandbox's generation under the *tenant* cgroup, so the tenant's limits
/// still apply; what is lost is only the per-request accounting and
/// `cgroup.kill`.
pub(super) fn admit(
    generation: Option<&std::path::Path>,
    per_request: bool,
    id: &str,
    host_pid: u32,
    narrower: Option<&crate::sandbox::limits::Limits>,
) -> Option<PathBuf> {
    let dir = request_cgroup(generation, per_request, id, narrower)?;
    let _ = crate::cgroup::attach(&dir, host_pid);
    Some(dir)
}

/// Make a request's cgroup, with its limits written, before anything is in
/// it. [`admit`] then moves the request in; the warm-exec path instead
/// creates the request inside it (`CLONE_INTO_CGROUP`), which never takes the
/// kernel's thread-group lock for writing and so never waits on it.
pub(super) fn request_cgroup(
    generation: Option<&std::path::Path>,
    per_request: bool,
    id: &str,
    narrower: Option<&crate::sandbox::limits::Limits>,
) -> Option<PathBuf> {
    if !per_request {
        return None;
    }
    let dir = crate::cgroup::Hierarchy::request(generation?, id);
    if std::fs::create_dir(&dir).is_err() {
        return None;
    }
    // A tenant's own limits, written on *this request's* cgroup before the
    // child is let go. Cgroups nest, so a narrower number here binds whatever
    // the function above was declared with — and a wider one would not,
    // which is why `TenantLimits::narrow` can only produce a smaller value.
    //
    // Best effort, like the attach below: a limit that could not be written
    // leaves the request under the function's own, which is the promise the
    // operator already made. Failing the request instead would turn a tenant's
    // *tightening* into an outage.
    if let Some(limits) = narrower {
        let _ = crate::cgroup::apply(&dir, &limits.cgroup_writes());
    }
    Some(dir)
}

/// Kill a request that overran its deadline, given its host pid.
///
/// The request cgroup is the good path: `cgroup.kill` (or its freeze-and-signal
/// fallback) takes the process and everything it spawned. Without one all we
/// have is the pid, which misses grandchildren — a reason to keep per-request
/// cgroups on, not a reason to skip the kill.
pub(super) fn kill_request(request_cgroup: Option<&std::path::Path>, host_pid: u32) {
    if let Some(dir) = request_cgroup
        && let Ok(true) = crate::cgroup::kill(dir)
    {
        return;
    }
    // SAFETY: `kill` touches no memory, so any pid is sound to pass. Which
    // process it reaches is the real question, and the answer is not "one
    // this supervisor holds unreaped": a request is the child of the
    // warm-exec helper or of the agent, and *they* reap it. Between its exit
    // and this call the pid can in principle be reused by an unrelated
    // process. The cgroup path above has no such window; this one is taken
    // only when there is no request cgroup or killing through it failed.
    unsafe { libc::kill(host_pid as libc::pid_t, libc::SIGKILL) };
}

/// How long the agent gets to report a killed request before it is itself
/// declared broken.
///
/// The agent's own work here is one `waitpid` on a process the kernel has
/// already killed and one small frame, so this is generous. What it is really
/// bounding is the case where the agent is wedged too — a lock some
/// import-time thread was holding, say — and the honest answer is to replace
/// the function rather than wait on it.
pub const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_unique_and_safe_as_cgroup_names() {
        let ids: Vec<String> = (0..1000).map(|_| next_request_id()).collect();
        let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "request ids collided");

        for id in &ids {
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit()),
                "`{id}` is not safe as a directory name"
            );
        }
    }

    /// A request can be found by its id or by the name its caller gave it.
    ///
    /// The key exists because the id only reaches the caller *with the
    /// answer*, which is too late to stop the call it belongs to.
    #[test]
    fn a_request_is_reachable_by_its_id_and_by_its_callers_own_key() {
        let requests = Requests::default();
        requests.start("00000001", None, Some("job-4711"));

        assert!(requests.get("00000001").is_some(), "by id");
        assert!(requests.get("job-4711").is_some(), "by key");
        assert!(requests.get("job-0000").is_none(), "a key nobody used");

        requests.finish("00000001");
        assert!(requests.get("00000001").is_none());
        assert!(
            requests.get("job-4711").is_none(),
            "the key outlived the request"
        );
    }

    /// One cancel stops every request sharing a key, which is what a caller
    /// who reused one meant.
    #[test]
    fn two_requests_under_one_key_are_two_requests_one_cancel_finds() {
        let requests = Requests::default();
        let a = requests.start("00000001", None, Some("batch"));
        let b = requests.start("00000002", None, Some("batch"));

        // One lookup finds one of them; the caller repeats until there is
        // nothing left, which is the same shape as cancelling by id twice.
        for _ in 0..2 {
            let Some(found) = requests.get("batch") else {
                panic!("a request under this key");
            };
            found.cancel();
            requests.finish(if a.cancelled() && !b.cancelled() {
                "00000001"
            } else {
                "00000002"
            });
        }
        assert!(a.cancelled() && b.cancelled());
    }

    /// A request id is a counter, so ownership is what keeps a cancel honest.
    #[test]
    fn a_tenant_can_only_cancel_its_own_requests() {
        let requests = Requests::default();
        let theirs = requests.start("00000001", Some("acme"), None);
        let operators = requests.start("00000002", None, None);

        assert!(theirs.is_for(None), "the operator may cancel anything");
        assert!(theirs.is_for(Some("acme")));
        assert!(
            !theirs.is_for(Some("globex")),
            "another tenant reached it by counting"
        );
        assert!(
            !operators.is_for(Some("acme")),
            "a tenant reached the operator's own request"
        );
    }

    /// Cancelling before the work starts is the better outcome, and is
    /// reported as a different one.
    #[test]
    fn a_cancel_before_the_child_is_admitted_says_the_work_had_not_started() {
        let requests = Requests::default();
        let request = requests.start("00000001", None, None);

        assert!(
            !request.cancel(),
            "nothing had started, so there was nothing to kill"
        );
        assert!(
            request.cancelled(),
            "and it is marked, which is what stops it"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_in_the_host_namespace_translates_to_itself() {
        // The identity case, which is also the one that made this bug invisible
        // for so long: in `crates/zygo-core/examples/poc3_warm_path.rs` the
        // agent was a plain host subprocess, so the
        // pid it reported happened to be correct.
        let me = std::process::id();
        assert_eq!(innermost_ns_pid(me), Some(me));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pid_that_does_not_exist_translates_to_nothing() {
        // Better to decline than to guess: a wrong answer here is a SIGKILL
        // sent to an unrelated process.
        assert_eq!(innermost_ns_pid(u32::MAX), None);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn translation_declines_where_there_is_no_procfs() {
        assert_eq!(innermost_ns_pid(std::process::id()), None);
    }

    // --- CpuAccounting ---------------------------------------------------
    //
    // Parsed from files rather than mocked, because the format is the thing
    // being got right: `cpu.stat` gained fields between kernel releases and
    // `cpu.max` has two shapes.
}
