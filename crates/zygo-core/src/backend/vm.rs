//! The `vm` backend: a sandbox whose boundary is hardware.
//!
//! libkrun is linked *into* this binary rather than loaded as a shared object.
//! It exports a C ABI from a Rust crate, so a cargo dependency and an rlib are
//! the whole mechanism: no `.so`, no `dlopen`, no C static library. That is
//! what keeps `dist-linux`'s one static musl binary intact, and it is the
//! route measured in `make vm-build`.
//!
//! # The shape
//!
//! A sandbox is a **forked child that becomes the virtual machine monitor**.
//! `krun_start_enter` never returns, so the fork before it is to this backend
//! what `clone3` is to `ns`: the moment the sandbox stops being this process.
//! `VmSandbox` is then the same three things `NsSandbox` is — a pid, a state
//! and a cgroup — where the pid is the VMM's (D1).
//!
//! The VMM lives in the `ns` backend's own cgroup tree, so `memory.max`,
//! `pids.max`, `cpu.max`, `freeze` and `cgroup.kill` all apply to it
//! unchanged, and freezing a tenant freezes the whole guest (D2). What they
//! bound is the VMM's own footprint, which is the guest's RAM plus its
//! overhead; the guest's own limits are a second fence inside it, and that is
//! M2's work.
//!
//! # What it refuses, by name
//!
//! Every refusal here is a test in this module rather than a comment. A
//! backend that silently does less than it was asked for is the failure mode
//! the whole project is arranged against: `gvisor.rs` refuses the same way and
//! for the same reason.

use std::path::{Path, PathBuf};

use crate::backend::{Availability, Backend, Sandbox, SandboxOutcome};
#[cfg(target_os = "linux")]
use crate::cgroup;
use crate::error::{Error, Result};
use crate::sandbox::{SandboxConfig, SandboxState};
use crate::spec::Network;

/// Where `zygo backend install vm` puts the guest kernel, under `krun/`.
///
/// A downloaded artefact and not a linked one, for two reasons that happen to
/// have the same answer: the kernel is GPL and this binary is Apache-2.0, and
/// it is ten to twenty megabytes against a fifteen megabyte budget. Requirement
/// D8; `krun_set_kernel` is what makes it possible, and it is present in the
/// pinned libkrun.
pub const KERNEL_FILE: &str = "Image";

/// Which of libkrun's kernel formats this file is, read from the file.
///
/// This was `KRUN_KERNEL_FORMAT_RAW`, unconditionally, and the guest kernel
/// that `make vm-kernel` produces is an **ELF**. libkrun then copied the ELF
/// headers to the guest's load address and jumped into them, so the vCPU
/// executed whatever those bytes decoded to: it spun at 100%, printed nothing,
/// and was killed by its deadline.
///
/// That failure was read as an interrupt-controller problem for a long time,
/// because the host is GICv2 and libkrun logs a GICv3 fallback on the way past.
/// It was not: with `earlycon=pl011` on the command line — which prints from
/// the first lines of `start_kernel`, long before any interrupt is wanted —
/// the guest still produced nothing, which places the failure *before* the
/// kernel began, not in what it did afterwards.
///
/// So the format is read from the file rather than assumed. The magic numbers
/// are the ones the loader itself keys on: `\x7fELF`, gzip's `\x1f\x8b`, and
/// the arm64 Image header's `ARM\x64` at offset 56 (`arch/arm64/kernel/image.h`).
/// A file that is none of them is a refusal that names what was found, because
/// the alternative is this bug again with different bytes.
#[cfg(all(feature = "vm", target_os = "linux"))]
fn kernel_format(kernel: &Path) -> std::result::Result<u32, String> {
    use std::io::Read as _;

    let mut head = [0u8; 64];
    let mut file = std::fs::File::open(kernel).map_err(|e| format!("{}: {e}", kernel.display()))?;
    let read = file
        .read(&mut head)
        .map_err(|e| format!("{}: {e}", kernel.display()))?;
    if read < 64 {
        return Err(format!(
            "{} is {read} bytes, too small to be a kernel",
            kernel.display()
        ));
    }

    if &head[..4] == b"\x7fELF" {
        return Ok(ffi::KRUN_KERNEL_FORMAT_ELF);
    }
    if head[..2] == [0x1f, 0x8b] {
        return Ok(ffi::KRUN_KERNEL_FORMAT_IMAGE_GZ);
    }
    // The arm64 Image header carries `ARM\x64` at offset 56; x86's bzImage
    // carries `HdrS` at 514, and either way an uncompressed image is what
    // libkrun calls "raw".
    if &head[56..60] == b"ARM\x64" {
        return Ok(ffi::KRUN_KERNEL_FORMAT_RAW);
    }

    Err(format!(
        "{} is not a kernel libkrun can load: it is neither an ELF, nor gzip, \
         nor an arm64 Image (first bytes {:02x?})",
        kernel.display(),
        &head[..8]
    ))
}

/// How much RAM the guest gets beyond what the spec asked for.
///
/// A kernel, its page tables and the virtio queues are not the tenant's
/// memory, and charging them to `mem` would mean a function given 128 MB had
/// rather less than that. Provisional until M2 measures it on the Pi; the
/// number is here rather than scattered so that the measurement has one place
/// to land.
pub const GUEST_KERNEL_ALLOWANCE_MIB: u32 = 128;

/// What the host cgroup allows the VMM beyond the guest's RAM.
///
/// The VMM maps the guest's memory and keeps its own; a `memory.max` set to
/// exactly the guest's RAM kills the VMM as the guest fills up, which reads
/// as a crash rather than as the guest's own out-of-memory. Provisional, M2.
pub const VMM_OVERHEAD_MIB: u32 = 64;

/// Whether a virtual machine monitor is linked into this binary.
///
/// Exposed because the *CLI* needs the answer and cannot ask its own
/// `cfg!(feature = "vm")`: the feature belongs to `zygo-core`, and asking for
/// it in a crate that does not declare one is a `cfg` that is always false and
/// a warning that says so.
pub const COMPILED_IN: bool = cfg!(feature = "vm");

pub struct VmBackend {
    kernel: Option<PathBuf>,
}

impl VmBackend {
    pub fn new() -> Self {
        Self::with_paths(&crate::Paths::from_env())
    }

    pub fn with_paths(paths: &crate::Paths) -> Self {
        let kernel = paths.krun().join(KERNEL_FILE);
        Self {
            kernel: kernel.is_file().then_some(kernel),
        }
    }
}

impl Default for VmBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether this host can hand out a virtual machine, attempted rather than
/// inspected.
///
/// Opening `/dev/kvm` is not the question: a nested or restricted hypervisor
/// hands out the device and then refuses `KVM_CREATE_VM`. `doctor::kvm` makes
/// the same attempt for the same reason (V10).
#[cfg(target_os = "linux")]
fn kvm_usable() -> std::result::Result<(), String> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let kvm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map_err(|e| format!("/dev/kvm: {e}"))?;

    // `_IO(KVMIO, 0x01)` with `KVMIO` = 0xAE, on every architecture. Machine
    // type zero is this host's default, which is the only thing a probe should
    // ask for.
    const KVM_CREATE_VM: libc::Ioctl = 0xAE01;
    // SAFETY: `kvm` is an open `/dev/kvm`; the ioctl takes an integer by value
    // and returns a descriptor or -1.
    let vm = unsafe { libc::ioctl(kvm.as_raw_fd(), KVM_CREATE_VM, 0) };
    if vm < 0 {
        return Err(format!(
            "/dev/kvm opens but KVM_CREATE_VM failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: a descriptor the kernel just created and nobody else holds.
    drop(unsafe { OwnedFd::from_raw_fd(vm) });
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn kvm_usable() -> std::result::Result<(), String> {
    Err(format!(
        "KVM is a Linux feature; this host runs {}",
        std::env::consts::OS
    ))
}

impl Backend for VmBackend {
    fn name(&self) -> &'static str {
        "vm"
    }

    fn availability(&self) -> Availability {
        if !COMPILED_IN {
            return Availability::unavailable(
                "this binary was built without the `vm` feature, so no virtual machine \
                 monitor is linked into it",
                "build with `--features vm`, or `make vm-build`; use --isolation ns here",
            );
        }
        if let Err(reason) = kvm_usable() {
            return Availability::unavailable(
                reason,
                "the `vm` backend needs a working KVM; use --isolation ns here",
            );
        }
        match &self.kernel {
            Some(_) => Availability::Available,
            None => Availability::unavailable(
                "the guest kernel is not installed",
                "zygo backend install vm",
            ),
        }
    }

    fn start(&self, config: &SandboxConfig) -> Result<Box<dyn Sandbox>> {
        if let Some(e) = self.availability().into_error("vm") {
            return Err(e);
        }
        let kernel = self.kernel.clone().expect("availability checked it");
        refuse_what_v1_cannot_do(config)?;
        start_vm(config, &kernel)
    }
}

/// Everything the first cut does not do, refused with the reason and the
/// milestone that will do it.
///
/// Separated from `start` so it can be tested on any host: what a backend
/// refuses is a decision, and a decision should not need a hypervisor to
/// check.
pub fn refuse_what_v1_cannot_do(config: &SandboxConfig) -> Result<()> {
    let unsupported = |reason: String, remedy: &str| Error::BackendUnavailable {
        backend: "vm",
        reason,
        remedy: remedy.to_string(),
    };

    if config.hold {
        return Err(unsupported(
            "the vm backend cannot hold a warm sandbox yet: a request enters a guest \
             over vsock, not through `setns`"
                .into(),
            "use --isolation ns for warm functions",
        ));
    }
    if config.agent_fd.is_some() {
        return Err(unsupported(
            "the runtime agent is handed its control socket as an inherited descriptor, \
             and a guest inherits nothing from the host"
                .into(),
            "use --isolation ns for agent runtimes",
        ));
    }
    if config.network != Network::None {
        return Err(unsupported(
            format!(
                "the vm backend has only `network = \"none\"`; this sandbox asks for \
                 `{}`, which needs the VMM inside Zygo's own network namespace",
                config.network
            ),
            "use --isolation ns for a networked sandbox",
        ));
    }
    if config.stdio.is_some() {
        return Err(unsupported(
            "a terminal inside a guest is virtio-console, which is not wired up".into(),
            "drop --tty, or use --isolation ns",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The FFI
// ---------------------------------------------------------------------------
//
// Declared here rather than taken from a `-sys` crate, because libkrun exposes
// a C ABI from a Rust crate: the symbols are in the rlib cargo already linked,
// and what is missing is only their signatures. Every one of these is from
// `include/libkrun.h` at the pinned tag.

#[cfg(all(feature = "vm", target_os = "linux"))]
mod ffi {
    use std::os::raw::{c_char, c_int};

    /// The tag libkrun gives the root filesystem. `krun_set_root` uses it
    /// internally; naming it is what lets the root be configured instead.
    pub const KRUN_FS_ROOT_TAG: &str = "/dev/root";

    /// The DAX window `krun_set_root` picks for the root, matched here so that
    /// asking for a read-only root changes the permission and nothing else.
    pub const ROOT_SHM_SIZE: u64 = 1 << 29;

    pub const KRUN_KERNEL_FORMAT_RAW: u32 = 0;
    pub const KRUN_KERNEL_FORMAT_ELF: u32 = 1;
    pub const KRUN_KERNEL_FORMAT_IMAGE_GZ: u32 = 4;

    // Declared ahead of the milestone that uses each, because the signatures
    // are the thing that has to be right and reading them all from one header
    // at one version is how they stay that way. The milestone is named so an
    // unused one is a plan, not a leftover.
    #[allow(dead_code)]
    unsafe extern "C" {
        pub fn krun_set_log_level(level: u32) -> i32;
        pub fn krun_create_ctx() -> i32;
        pub fn krun_set_vm_config(ctx: u32, vcpus: u8, ram_mib: u32) -> i32;
        pub fn krun_set_root(ctx: u32, root_path: *const c_char) -> i32;
        /// The root filesystem with its own parameters, the one that matters
        /// being `read_only`. `krun_set_root` is this call with the flag off.
        pub fn krun_add_virtiofs3(
            ctx: u32,
            tag: *const c_char,
            path: *const c_char,
            shm_size: u64,
            read_only: bool,
        ) -> i32;
        pub fn krun_set_kernel(
            ctx: u32,
            kernel_path: *const c_char,
            kernel_format: u32,
            initramfs: *const c_char,
            cmdline: *const c_char,
        ) -> i32;
        /// M3: `/run/secrets` as a per-sandbox tag (D6).
        pub fn krun_add_virtiofs(ctx: u32, tag: *const c_char, path: *const c_char) -> i32;
        pub fn krun_set_workdir(ctx: u32, workdir_path: *const c_char) -> i32;
        pub fn krun_set_exec(
            ctx: u32,
            exec_path: *const c_char,
            argv: *const *const c_char,
            envp: *const *const c_char,
        ) -> i32;
        pub fn krun_set_env(ctx: u32, envp: *const *const c_char) -> i32;
        /// M2: the rlimits the `ns` child applies, applied in the guest.
        pub fn krun_set_rlimits(ctx: u32, rlimits: *const *const c_char) -> i32;
        /// M3: the agent's socket on port 3, and `RequestControl` on port 4
        /// (D5) — the two that let a warm function work at all.
        pub fn krun_add_vsock_port(ctx: u32, port: u32, filepath: *const c_char) -> i32;
        /// M5: `--tty` and the guest's own console.
        pub fn krun_set_console_output(ctx: u32, filepath: *const c_char) -> i32;
        /// M2: the uid inside the guest, which is not the host's.
        pub fn krun_setuid(ctx: u32, uid: c_int) -> i32;
        pub fn krun_start_enter(ctx: u32) -> i32;
    }
}

// ---------------------------------------------------------------------------
// Starting one
// ---------------------------------------------------------------------------

#[cfg(all(feature = "vm", target_os = "linux"))]
fn start_vm(config: &SandboxConfig, kernel: &Path) -> Result<Box<dyn Sandbox>> {
    use std::os::fd::AsRawFd;

    // `krun_set_root` takes one directory, so the rootfs has to be flat. The
    // store already keys a flattened rootfs on its layers, and `gvisor` needs
    // the same thing for the same reason (D3). If PoC 8 says virtiofs makes
    // imports slow, M3 replaces this with a tag per layer and a guest overlay.
    let rootfs =
        crate::backend::gvisor::flat_rootfs(config).ok_or_else(|| Error::BackendUnavailable {
            backend: "vm",
            reason: "a guest's root is one directory over virtiofs, and this sandbox's \
                     rootfs is an overlay of image layers"
                .into(),
            remedy: "the store can flatten instead; this is the backend's own bug if you \
                     see it"
                .into(),
        })?;

    // The same two-level hierarchy every other backend builds: the VMM goes in
    // it, and everything the guest costs the host is charged there (D2).
    let hierarchy = cgroup::Hierarchy::discover().ok();
    let generation = match hierarchy.as_ref() {
        Some(h) => {
            h.ensure(cgroup::Hierarchy::host_ram())?;
            // The VMM's own ceiling is the guest's RAM plus what the monitor
            // needs to run it; a limit set to exactly the guest's RAM kills the
            // monitor as the guest fills up, which reads as a crash rather than
            // as the guest running out of memory.
            let mut limits = config.limits.clone();
            limits.mem = crate::spec::Bytes(
                u64::from(guest_ram_mib(&config.limits) + VMM_OVERHEAD_MIB) * 1024 * 1024,
            );
            h.create_tenant(&config.id.tenant, &limits)?;
            Some(h.create_generation(&config.id.tenant)?)
        }
        None => None,
    };

    // A pipe the child can report a pre-`krun_start_enter` failure on. Without
    // it, a child that dies while setting the VMM up is a bare exit code: the
    // library's own errors are negative return values, and nobody is reading
    // them (V7).
    let (err_read, err_write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|e| Error::primitive("pipe", "internal vm launcher error", e.into()))?;

    // SAFETY: the child does only what is listed below and then either execs
    // into the VMM or `_exit`s; it runs no Rust destructor belonging to this
    // process.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(Error::primitive(
            "fork",
            "the vm backend could not start a monitor process",
            std::io::Error::last_os_error(),
        ));
    }
    if pid == 0 {
        drop(err_read);
        // From here on nothing returns: either the VMM runs, or the child
        // reports and exits.
        child_becomes_the_vmm(config, kernel, &rootfs, generation.as_deref(), err_write);
    }

    drop(err_write);
    let mut sandbox = VmSandbox {
        pid: pid as u32,
        state: SandboxState::Starting,
        cgroup_dir: generation,
        timeout: config.limits.timeout.get(),
        exit_code: None,
        outcome: SandboxOutcome::default(),
    };

    // Anything the child wrote before the VMM took over. End of file means it
    // got that far, which is the same signal the `ns` launcher uses.
    let mut reported = String::new();
    {
        use std::io::Read as _;
        let mut file = std::fs::File::from(err_read);
        let _ = file.read_to_string(&mut reported);
    }
    if !reported.trim().is_empty() {
        let _ = sandbox.kill();
        return Err(Error::BackendUnavailable {
            backend: "vm",
            reason: format!("the monitor could not start the guest: {}", reported.trim()),
            remedy: "run `zygo doctor` and check `make vm-kernel` produced a usable Image".into(),
        });
    }

    // End of file on that pipe is ambiguous. The child closes `err_write`
    // deliberately, just before `krun_start_enter` — and the kernel closes it
    // for the child too, when the child dies. Both arrive here as EOF with
    // nothing read, so EOF alone cannot mean "the monitor is up".
    //
    // `wait` catches a monitor that dies later, because it reaps in its poll
    // loop; nothing caught one that died here, and `serve` never calls `wait`,
    // so a warm function could be declared warm against a process that was
    // already gone. This closes that: it cannot catch a death that happens
    // after the close and before this line, but the answer it does give is
    // never wrong.
    #[cfg(target_os = "linux")]
    if let Ok(Some(code)) = sandbox.reap(false) {
        return Err(Error::BackendUnavailable {
            backend: "vm",
            reason: format!(
                "the monitor exited with status {code} before the guest was up, and said nothing"
            ),
            remedy: "run with ZYGO_KRUN_CONSOLE=/path to capture the guest's console, \
                     which is the only channel a kernel that has not reached its init has"
                .into(),
        });
    }

    sandbox.state = SandboxState::Warm;
    let _ = std::io::stderr().as_raw_fd();
    Ok(Box::new(sandbox))
}

/// How much RAM the guest is given: what the spec asked for, plus a kernel.
pub fn guest_ram_mib(limits: &crate::sandbox::Limits) -> u32 {
    let asked_mib = (limits.mem.get() / (1024 * 1024)) as u32;
    asked_mib.saturating_add(GUEST_KERNEL_ALLOWANCE_MIB)
}

#[cfg(all(feature = "vm", target_os = "linux"))]
fn child_becomes_the_vmm(
    config: &SandboxConfig,
    kernel: &Path,
    rootfs: &Path,
    cgroup_dir: Option<&Path>,
    err_write: std::os::fd::OwnedFd,
) -> ! {
    // Moved into the block below before the monitor starts; `fail` writes to
    // the raw descriptor, which stays valid until that point.
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    let err_fd = err_write.as_raw_fd();
    let fail = |what: &str| -> ! {
        let message = format!("{what}\n");
        // SAFETY: a write to a descriptor this process owns; the buffer is
        // live for the call.
        unsafe {
            libc::write(
                err_fd,
                message.as_ptr() as *const libc::c_void,
                message.len(),
            );
            libc::_exit(1)
        }
    };
    let check = |rc: i32, what: &str| {
        if rc < 0 {
            fail(&format!("{what} failed: libkrun returned {rc}"));
        }
    };
    let cstr = |s: &str| CString::new(s).unwrap_or_else(|_| fail("a path contains a NUL byte"));

    // Into the cgroup before anything is allocated, so every page the monitor
    // maps is charged where the spec said it should be.
    if let Some(dir) = cgroup_dir {
        // The `zygote` sub-cgroup, not the generation itself. A cgroup with
        // `cgroup.subtree_control` set cannot hold processes — the kernel's
        // "no internal processes" rule — so writing the monitor's pid into the
        // generation fails with `EBUSY`, which is the same reason the `ns`
        // backend puts its child one level down.
        let leaf = cgroup::Hierarchy::zygote(dir);
        // SAFETY: `getpid` takes no arguments and cannot fail.
        let me = unsafe { libc::getpid() } as u32;
        if let Err(e) = cgroup::attach(&leaf, me) {
            fail(&format!("could not enter the tenant cgroup: {e}"));
        }
    }

    // SAFETY: each of these takes plain values and pointers to strings that
    // outlive the call.
    unsafe {
        // `krun_set_log_level` is deliberately **not** called.
        //
        // It ends in `env_logger::Builder::init`, which panics when a global
        // logger is already installed — and Zygo installs one in `main` before
        // any of this runs. The panic is
        // "Builder::init should not be called after logger initialized", from
        // inside a forked child that is about to become a virtual machine
        // monitor, which is the least debuggable place in the program. The
        // monitor logs through the `log` facade either way, so Zygo's own
        // subscriber already receives whatever it says.
        let ctx = ffi::krun_create_ctx();
        if ctx < 0 {
            fail(&format!("krun_create_ctx failed: {ctx}"));
        }
        let ctx = ctx as u32;

        let vcpus = config.limits.cpu.0.ceil().max(1.0).min(255.0) as u8;
        check(
            ffi::krun_set_vm_config(ctx, vcpus, guest_ram_mib(&config.limits)),
            "krun_set_vm_config",
        );

        let kernel_c = cstr(&kernel.to_string_lossy());
        let format = match kernel_format(kernel) {
            Ok(f) => f,
            Err(why) => fail(&why),
        };
        check(
            ffi::krun_set_kernel(
                ctx,
                kernel_c.as_ptr(),
                format,
                std::ptr::null(),
                std::ptr::null(),
            ),
            "krun_set_kernel",
        );

        // The root goes in **read-only**, and that is a correctness fix rather
        // than a hardening nicety.
        //
        // `krun_set_root` is `krun_add_virtiofs3` with `read_only: false`, and
        // the directory it shares is the store's flattened rootfs for an image
        // digest — one directory, shared by every sandbox that ever runs that
        // image. With the share writable, `echo x > /pwned` inside a guest
        // created `cache/flat/<digest>/pwned` **on the host**, where the next
        // tenant on the same image would find it. The `ns` backend has always
        // mounted this read-only; the `vm` backend, whose whole claim is a
        // stronger boundary, had the weaker one.
        //
        // Read-only on the *host* side, at the device, not a `ro` mount option
        // in the guest: a guest kernel is the tenant's to subvert, and a
        // remount is one syscall. The VMM refusing the write is not.
        let root_c = cstr(&rootfs.to_string_lossy());
        let root_tag_c = cstr(ffi::KRUN_FS_ROOT_TAG);
        check(
            ffi::krun_add_virtiofs3(
                ctx,
                root_tag_c.as_ptr(),
                root_c.as_ptr(),
                ffi::ROOT_SHM_SIZE,
                true,
            ),
            "krun_add_virtiofs3(root, read-only)",
        );

        let workdir_c = cstr(&config.workdir.to_string_lossy());
        check(
            ffi::krun_set_workdir(ctx, workdir_c.as_ptr()),
            "krun_set_workdir",
        );

        // The environment, as a NUL-terminated array of `KEY=VALUE`.
        let env: Vec<CString> = config
            .env
            .iter()
            .map(|(k, v)| cstr(&format!("{k}={v}")))
            .collect();
        let mut env_ptrs: Vec<*const libc::c_char> = env.iter().map(|s| s.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());
        check(ffi::krun_set_env(ctx, env_ptrs.as_ptr()), "krun_set_env");

        let (program, arguments) = config
            .argv
            .split_first()
            .unwrap_or_else(|| fail("the sandbox has no command to run"));
        let program_c = cstr(program);
        let argv: Vec<CString> = arguments.iter().map(|a| cstr(a)).collect();
        let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        check(
            ffi::krun_set_exec(
                ctx,
                program_c.as_ptr(),
                argv_ptrs.as_ptr(),
                std::ptr::null(),
            ),
            "krun_set_exec",
        );

        // The guest's console, when somebody is debugging. A guest that will
        // not boot says why here and nowhere else: there is no other channel
        // between a kernel that has not reached its init and the host.
        if let Some(path) = std::env::var_os("ZYGO_KRUN_CONSOLE") {
            let console_c = cstr(&path.to_string_lossy());
            check(
                ffi::krun_set_console_output(ctx, console_c.as_ptr()),
                "krun_set_console_output",
            );
        }

        // The error pipe closes *here*, and this is the signal the parent is
        // waiting for.
        //
        // `krun_start_enter` does not `execve`; it turns this process into the
        // monitor and never returns. So close-on-exec never fires and the
        // write end stays open for the life of the guest — which left the
        // parent blocked in `read_to_string` until the guest exited, rather
        // than until the guest had *started*. Closing it deliberately is what
        // makes "no bytes, then end of file" mean "the monitor got this far".
        drop(err_write);

        let rc = ffi::krun_start_enter(ctx);
        // Unreachable unless the monitor refused to run. Nothing can be
        // reported now — the pipe is closed — so the exit code carries it, and
        // the console file above is where the reason is.
        let _ = rc;
        libc::_exit(126)
    }
}

#[cfg(not(all(feature = "vm", target_os = "linux")))]
fn start_vm(_config: &SandboxConfig, _kernel: &Path) -> Result<Box<dyn Sandbox>> {
    Err(Error::BackendUnavailable {
        backend: "vm",
        reason: "this binary was built without the `vm` feature".into(),
        remedy: "build with `--features vm`, or `make vm-build`".into(),
    })
}

// ---------------------------------------------------------------------------
// The sandbox
// ---------------------------------------------------------------------------

/// A running guest, from the host's side: the monitor's pid and its cgroup.
pub struct VmSandbox {
    pid: u32,
    state: SandboxState,
    cgroup_dir: Option<PathBuf>,
    /// Wall-clock budget, enforced by [`VmSandbox::wait`] on the monitor.
    ///
    /// The guest gets its own, tighter one in M2; this is the backstop a
    /// wedged guest cannot defeat, because it is the host killing the process
    /// that *is* the virtual machine.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    timeout: std::time::Duration,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    exit_code: Option<i32>,
    outcome: SandboxOutcome,
}

impl Sandbox for VmSandbox {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn outcome(&self) -> SandboxOutcome {
        self.outcome
    }

    fn cgroup(&self) -> Option<&Path> {
        self.cgroup_dir.as_deref()
    }

    fn state(&self) -> SandboxState {
        self.state
    }

    fn wait(&mut self) -> Result<i32> {
        #[cfg(target_os = "linux")]
        {
            use std::time::Instant;

            if self.timeout.is_zero() {
                return self.reap(true).map(|c| c.unwrap_or(0));
            }
            let deadline = Instant::now() + self.timeout;
            let mut backoff = std::time::Duration::from_micros(500);
            loop {
                if let Some(code) = self.reap(false)? {
                    return Ok(code);
                }
                if Instant::now() >= deadline {
                    self.kill()?;
                    // `ETIMEDOUT` is read by `Error::exit_code` and by
                    // `Error::timed_out`, which is what lets a caller tell a
                    // deadline kill from an out-of-memory one — both are exit
                    // 137 and the wait status says no more.
                    return Err(Error::Primitive {
                        operation: "the sandbox",
                        remedy: format!(
                            "raise `timeout` if {:?} is not long enough for this work",
                            self.timeout
                        ),
                        source: std::io::Error::from_raw_os_error(libc::ETIMEDOUT),
                    });
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_millis(20));
            }
        }
        #[cfg(not(target_os = "linux"))]
        Err(Error::BackendUnavailable {
            backend: "vm",
            reason: "there is no guest to wait for on this host".into(),
            remedy: "use --isolation ns".into(),
        })
    }

    fn kill(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            // `cgroup.kill` first, so anything the monitor started goes with
            // it; the monitor itself is in that cgroup too.
            if let Some(dir) = &self.cgroup_dir {
                let _ = cgroup::kill(dir);
            }
            // SAFETY: a pid this process forked and has not reaped.
            unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
            let _ = self.reap(true);
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl VmSandbox {
    fn reap(&mut self, block: bool) -> Result<Option<i32>> {
        if let Some(code) = self.exit_code {
            return Ok(Some(code));
        }
        let mut status: libc::c_int = 0;
        let flags = if block { 0 } else { libc::WNOHANG };
        // SAFETY: `status` is a live local; the pid is our own child.
        let rc = unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, flags) };
        if rc == 0 {
            return Ok(None);
        }
        if rc < 0 {
            return Err(Error::primitive(
                "waitpid",
                "the monitor process could not be reaped",
                std::io::Error::last_os_error(),
            ));
        }

        let code = if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            libc::WEXITSTATUS(status)
        };
        self.exit_code = Some(code);
        self.state = SandboxState::Cold;

        if let Some(dir) = &self.cgroup_dir {
            // Read before the removal: this is the last moment the kernel's
            // account of why the guest ended exists.
            self.outcome = SandboxOutcome {
                oom_kills: cgroup::oom_kills(dir).unwrap_or(0),
                peak_rss_kb: cgroup::peak_memory(dir)
                    .map(|b| b.get() / 1024)
                    .unwrap_or(0),
            };
            let _ = cgroup::Hierarchy::remove(dir);
            if let Some(tenant) = dir.parent() {
                let _ = std::fs::remove_dir(tenant);
            }
        }
        Ok(Some(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Limits;

    fn config() -> SandboxConfig {
        let mut config = SandboxConfig::for_tests();
        config.argv = vec!["/bin/true".into()];
        config
    }

    /// The guest kernel is looked for under the paths the caller handed in,
    /// and nowhere else.
    ///
    /// `VmBackend::new` derives them from the environment, which is right for
    /// a caller that has none. The bug this pins is the *other* caller: with
    /// `--data-root /tmp/x`, `for_isolation` used to build a `VmBackend` that
    /// had gone back to the environment for its own answer, so the backend
    /// found a kernel in the default data directory while everything else in
    /// that command — the image store, the run directory — was under
    /// `/tmp/x`. `doctor` reported the same phantom.
    ///
    /// Both directions are checked, because a test that only proves "absent
    /// when absent" also passes for a backend that always says absent.
    #[test]
    fn the_guest_kernel_is_found_under_the_paths_the_caller_gave() {
        let root = tempfile::tempdir().expect("a temp dir");
        let paths = crate::Paths::rooted(root.path());

        let empty = VmBackend::with_paths(&paths);
        assert!(
            empty.kernel.is_none(),
            "a data root with no kernel must not produce one"
        );

        std::fs::create_dir_all(paths.krun()).expect("the krun directory");
        let installed = paths.krun().join(KERNEL_FILE);
        std::fs::write(&installed, b"not a kernel, but a file").expect("the kernel file");

        let found = VmBackend::with_paths(&paths);
        assert_eq!(
            found.kernel.as_deref(),
            Some(installed.as_path()),
            "the kernel under the given data root is the one the backend uses"
        );
    }

    /// Every refusal names what it will not do and where that is tracked.
    ///
    /// A backend that silently does less than it was asked for is the failure
    /// this project is arranged against, and a refusal without a remedy is the
    /// same thing with better manners. `gvisor` has the same test.
    #[test]
    fn what_the_backend_cannot_do_is_refused_by_name() {
        let ordinary = config();
        assert!(
            refuse_what_v1_cannot_do(&ordinary).is_ok(),
            "an ordinary one-shot sandbox must not be refused"
        );

        /// One refusal: what is asked for, how to ask for it, and a word the
        /// message has to carry so that it is about *this* request.
        ///
        /// What is asserted is the remedy, not a roadmap. An earlier version
        /// required each message to name the milestone that would implement
        /// the feature — "M3", "M4" — which is a fact about this project's
        /// plan and no use at all to the person who just typed the command.
        /// A refusal owes them two things: which of their choices was the
        /// problem, and what to do instead.
        struct Case {
            what: &'static str,
            ask: fn(&mut SandboxConfig),
            /// A word from the request itself, so the message cannot be a
            /// generic "unsupported" that fits every case equally.
            names: &'static str,
        }

        let cases = [
            Case {
                what: "a held sandbox",
                ask: |c| c.hold = true,
                names: "warm",
            },
            Case {
                what: "an agent runtime",
                ask: |c| c.agent_fd = Some(3),
                names: "agent",
            },
            Case {
                what: "a networked sandbox",
                ask: |c| c.network = Network::Egress,
                names: "network",
            },
            Case {
                what: "a terminal",
                ask: |c| c.stdio = Some(0),
                names: "terminal",
            },
        ];

        for case in cases {
            let mut c = config();
            (case.ask)(&mut c);
            let err = refuse_what_v1_cannot_do(&c)
                .expect_err(&format!("{} should be refused", case.what));
            let text = format!("{err}");
            assert!(
                text.contains(case.names),
                "the refusal for {} should say which choice it is about: {text}",
                case.what
            );
            assert!(
                text.contains("--isolation ns") || text.contains("--tty"),
                "the refusal for {} should say what to do instead: {text}",
                case.what
            );
        }
    }

    /// The guest gets what the spec asked for *plus* a kernel, and the host
    /// cgroup gets that plus the monitor's own overhead.
    ///
    /// Charging the kernel to `mem` would mean a function given 128 MB had
    /// rather less than that, and a host limit set to exactly the guest's RAM
    /// kills the monitor as the guest fills up — which reads as a crash, not
    /// as the guest running out of memory.
    #[test]
    fn the_guest_is_given_more_than_the_tenant_asked_for() {
        let mut limits = Limits::for_tests();
        limits.mem = crate::spec::Bytes(128 * 1024 * 1024);
        let guest = guest_ram_mib(&limits);
        assert_eq!(guest, 128 + GUEST_KERNEL_ALLOWANCE_MIB);
        assert!(
            guest + VMM_OVERHEAD_MIB > guest,
            "the host ceiling has to leave room for the monitor itself"
        );
    }

    /// Without the feature there is no monitor, and the backend says so rather
    /// than failing somewhere deeper.
    #[cfg(not(feature = "vm"))]
    #[test]
    fn a_binary_without_the_feature_refuses_with_the_build_flag() {
        let backend = VmBackend { kernel: None };
        match backend.availability() {
            Availability::Unavailable { reason, remedy } => {
                assert!(reason.contains("without the `vm` feature"), "{reason}");
                assert!(remedy.contains("--features vm"), "{remedy}");
            }
            Availability::Available => panic!("a binary with no monitor called itself available"),
        }
    }
}
