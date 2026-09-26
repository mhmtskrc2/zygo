// SPDX-License-Identifier: Apache-2.0
//! Pseudo-terminal allocation for `zygo run --tty`.
//!
//! Zygo is daemonless, so a sandbox inherits the caller's terminal by default —
//! which is what makes `zygo run alpine sh` feel like running a local command,
//! with colours, prompts and job control all working for free.
//!
//! The cost is that tenant code then holds a *writable* descriptor to the
//! user's terminal. `--tty` gives the sandbox a terminal of its own instead:
//! the caller keeps the master side and relays bytes, and the sandbox never
//! sees the real one.
//!
//! (The `TIOCSTI` injection that made this worth building is blocked in the
//! seccomp profile regardless, so the default is safe; this removes the
//! descriptor entirely rather than filtering what can be done with it.)

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A freshly allocated pty pair.
pub struct Pty {
    /// The caller's end: reads what the sandbox writes, writes what it reads.
    pub master: OwnedFd,
    /// The sandbox's end, adopted as its stdin, stdout and stderr.
    pub slave: OwnedFd,
}

/// Allocate a pty.
pub fn open() -> std::io::Result<Pty> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: both out-parameters are live locals for the duration of the call,
    // and the remaining three are the documented "use the defaults" nulls.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            // `null_mut` for all three: glibc declares the last two as
            // `*const` and Apple's libc as `*mut`, and a null `*mut` coerces
            // to either.
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(Pty {
        // SAFETY: `openpty` succeeded, so this is a fresh descriptor nobody
        // else holds.
        master: unsafe { OwnedFd::from_raw_fd(master) },
        // SAFETY: as above.
        slave: unsafe { OwnedFd::from_raw_fd(slave) },
    })
}

/// Copy the caller's terminal size onto the pty, so the sandbox sees the same
/// geometry. Without this, full-screen programs draw at 80×24 regardless.
pub fn copy_window_size(from: RawFd, to: RawFd) {
    // SAFETY: all-zero bytes are a valid `winsize`: plain data the ioctl fills
    // in.
    let mut size: libc::winsize = unsafe { core::mem::zeroed() };
    // SAFETY: `winsize` is the struct both ioctls expect.
    if unsafe { libc::ioctl(from, libc::TIOCGWINSZ, &mut size) } == 0 {
        // SAFETY: `size` is a live local of the struct `TIOCSWINSZ` reads.
        unsafe { libc::ioctl(to, libc::TIOCSWINSZ, &size) };
    }
}

/// The caller's terminal settings, restored when this is dropped.
///
/// Raw mode is what lets keystrokes reach the sandbox unprocessed — no line
/// buffering, no local echo, no `^C` being turned into a signal on this side.
/// Restoring on drop matters more than usual: leaving a terminal in raw mode
/// makes the user's shell appear broken afterwards, and that must survive a
/// panic or an error path as well as a clean exit.
pub struct RawMode {
    fd: RawFd,
    original: libc::termios,
}

impl RawMode {
    pub fn enable(fd: RawFd) -> std::io::Result<Option<RawMode>> {
        // SAFETY: `isatty` takes a descriptor and touches no memory.
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(None); // not a terminal: nothing to put into raw mode
        }

        // SAFETY: all-zero bytes are a valid `termios`: plain data `tcgetattr`
        // fills in.
        let mut original: libc::termios = unsafe { core::mem::zeroed() };
        // SAFETY: `original` is a live local for the call.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut raw = original;
        // SAFETY: `raw` is a live local; `cfmakeraw` only rewrites its fields.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: `raw` is a live local `tcsetattr` only reads.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Registered before the guard exists, so a panic between here and the
        // caller taking it still restores.
        remember(fd, original);
        Ok(Some(RawMode { fd, original }))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: `original` was filled in by `tcgetattr` on this descriptor
        // and is a live field for the call.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
        forget(self.fd);
    }
}

/// Terminals to put back, and the panic hook that does it.
///
/// `Drop` alone is not enough. The release profile is `panic = "abort"`
/// (`Cargo.toml`), so a panic runs no destructors at all — the guard's `Drop`
/// and the `catch_unwind` test both worked only because the *test* profile
/// unwinds (B-08, the code review). A panic hook runs before the abort, in
/// ordinary Rust context, which is where this can safely take a lock.
static SAVED: std::sync::Mutex<Vec<(RawFd, libc::termios)>> = std::sync::Mutex::new(Vec::new());
static HOOK: std::sync::Once = std::sync::Once::new();

fn remember(fd: RawFd, original: libc::termios) {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_all();
            // The default hook, or whatever was installed before this: a
            // terminal that has been put back is no reason to swallow the
            // message explaining why.
            previous(info);
        }));
    });
    let mut saved = SAVED.lock().expect("saved terminals");
    saved.retain(|(seen, _)| *seen != fd);
    saved.push((fd, original));
}

fn forget(fd: RawFd) {
    if let Ok(mut saved) = SAVED.lock() {
        saved.retain(|(seen, _)| *seen != fd);
    }
}

/// Put every terminal this process changed back the way it found it, and
/// forget them.
///
/// Public because a panic is not the only way out that skips destructors: the
/// binary aborts on one, and a future signal path would want the same thing.
///
/// Forgetting matters as much as restoring. An entry is a descriptor
/// *number*: once its terminal is put back and closed, the number goes to the
/// next file opened, and a later `restore_all` would write old settings onto
/// that. A pty's master and slave share one set of settings, so a stale entry
/// that lands on a new pty's master rewrites its slave — which is how the
/// `mem::forget` test below broke a neighbouring test, 2 runs in 40.
pub fn restore_all() {
    // A poisoned lock still holds the settings, and a terminal left raw is
    // worse than reading data a panicking thread was halfway through writing.
    let mut saved = match SAVED.lock() {
        Ok(saved) => saved,
        Err(poisoned) => poisoned.into_inner(),
    };
    for (fd, original) in saved.drain(..) {
        // SAFETY: `original` was read from this descriptor by `tcgetattr`.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
    }
}

/// Shuttle bytes between the caller's terminal and the sandbox's pty until the
/// sandbox closes it.
///
/// Runs on its own thread so the caller can go on waiting for the sandbox: the
/// relay ends when the pty reports end of file, which happens when the last
/// process inside the sandbox exits.
pub fn relay(master: OwnedFd) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let master_fd = master.as_raw_fd();

        // Caller's input → sandbox. A separate thread because a blocking read
        // on stdin must not hold up output.
        // SAFETY: `dup` returns a fresh descriptor nobody else holds, or -1,
        // which `OwnedFd` treats as an error on use rather than as memory.
        let writer = unsafe { OwnedFd::from_raw_fd(libc::dup(master_fd)) };
        std::thread::spawn(move || {
            let mut input = std::io::stdin();
            let mut out = std::fs::File::from(writer);
            let mut buf = [0u8; 4096];
            while let Ok(n) = input.read(&mut buf) {
                if n == 0 || out.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        });

        // Sandbox output → caller.
        let mut input = std::fs::File::from(master);
        let mut out = std::io::stdout();
        let mut buf = [0u8; 4096];
        loop {
            match input.read(&mut buf) {
                // A pty master reports EIO, not EOF, once the last slave is
                // gone; treating that as an error would print a spurious
                // message on every normal exit.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pty_pair_is_two_distinct_terminals() {
        let pty = open().expect("openpty");
        let master = pty.master.as_raw_fd();
        let slave = pty.slave.as_raw_fd();

        assert_ne!(master, slave);
        // SAFETY: `isatty` takes a descriptor and touches no memory.
        assert_eq!(unsafe { libc::isatty(slave) }, 1, "the slave must be a tty");
    }

    #[test]
    fn bytes_written_to_the_master_arrive_at_the_slave() {
        let pty = open().expect("openpty");
        let mut master = std::fs::File::from(pty.master);
        let mut slave = std::fs::File::from(pty.slave);

        master.write_all(b"hello\n").unwrap();
        let mut buf = [0u8; 64];
        let n = slave.read(&mut buf).unwrap();
        // A pty in its default line discipline echoes and translates, so the
        // exact bytes vary; what matters is that the two ends are connected.
        assert!(n > 0);
        assert!(buf[..n].starts_with(b"hello"), "{:?}", &buf[..n]);
    }

    #[test]
    fn window_size_is_copied_between_terminals() {
        let a = open().expect("openpty");
        let b = open().expect("openpty");

        let want = libc::winsize {
            ws_row: 40,
            ws_col: 132,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `want` is a live local of the struct the ioctl reads.
        unsafe { libc::ioctl(a.slave.as_raw_fd(), libc::TIOCSWINSZ, &want) };

        copy_window_size(a.slave.as_raw_fd(), b.slave.as_raw_fd());

        // SAFETY: all-zero bytes are a valid `winsize`: plain data the ioctl
        // fills in.
        let mut got: libc::winsize = unsafe { core::mem::zeroed() };
        // SAFETY: `got` is a live local for the call.
        unsafe { libc::ioctl(b.slave.as_raw_fd(), libc::TIOCGWINSZ, &mut got) };
        assert_eq!((got.ws_row, got.ws_col), (40, 132));
    }

    /// The settings that decide whether a shell looks usable afterwards.
    ///
    /// The whole `c_lflag` word cannot be compared: the kernel owns status bits
    /// in it — macOS reports `PENDIN` set after any `tcsetattr` — and those are
    /// not settings anyone restored or failed to restore.
    const MEANINGFUL: libc::tcflag_t = libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN;

    /// `restore_all` and the panic hook put back *every* terminal the process
    /// holds raw, so a test running beside another undoes that one's raw mode
    /// halfway through. The tests that touch raw mode take turns.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lflags(fd: RawFd) -> libc::tcflag_t {
        // SAFETY: all-zero bytes are a valid `termios`: plain data `tcgetattr`
        // fills in.
        let mut t: libc::termios = unsafe { core::mem::zeroed() };
        // SAFETY: `t` is a live local for the call.
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
        t.c_lflag & MEANINGFUL
    }

    #[test]
    fn raw_mode_restores_the_original_settings_on_drop() {
        let _serial = serial();
        let pty = open().expect("openpty");
        let fd = pty.slave.as_raw_fd();

        let before = lflags(fd);
        assert_ne!(
            before & libc::ICANON,
            0,
            "a fresh pty starts in canonical mode"
        );

        {
            let guard = RawMode::enable(fd).unwrap();
            assert!(guard.is_some(), "a pty slave is a terminal");

            let during = lflags(fd);
            assert_eq!(
                during & libc::ICANON,
                0,
                "raw mode leaves canonical input on"
            );
            assert_eq!(during & libc::ECHO, 0, "raw mode leaves local echo on");
        }

        assert_eq!(lflags(fd), before, "the terminal was not restored");
    }

    /// Raw mode is restored even when no destructor runs.
    ///
    /// This is the case the release binary is in: `panic = "abort"` runs no
    /// destructors, so `Drop` never fires and the terminal stayed raw (B-08).
    /// `mem::forget` is exactly that situation, reached without aborting the
    /// test process — the panic hook and this test take the same path through
    /// `restore_all`.
    #[test]
    fn raw_mode_is_restored_even_when_no_destructor_runs() {
        let _serial = serial();
        let pty = open().expect("openpty");
        let fd = pty.slave.as_raw_fd();
        let before = lflags(fd);

        let guard = RawMode::enable(fd).expect("enable").expect("a terminal");
        assert_ne!(lflags(fd), before, "raw mode did not take effect");

        // The destructor that `panic = "abort"` skips.
        std::mem::forget(guard);
        assert_ne!(lflags(fd), before, "something restored it early");

        restore_all();
        assert_eq!(lflags(fd), before, "the hook did not put the terminal back");
    }

    /// A terminal that has been restored is no longer registered, so a later
    /// `restore_all` cannot write settings back onto a reused descriptor.
    #[test]
    fn a_restored_terminal_is_forgotten() {
        let _serial = serial();
        let pty = open().expect("openpty");
        let fd = pty.slave.as_raw_fd();
        let before = lflags(fd);
        drop(RawMode::enable(fd).expect("enable"));
        assert_eq!(lflags(fd), before);

        // Change it by hand; `restore_all` must now leave it alone, because
        // nothing in this process is holding it raw.
        // SAFETY: all-zero bytes are a valid `termios`: plain data `tcgetattr`
        // fills in.
        let mut raw: libc::termios = unsafe { core::mem::zeroed() };
        // SAFETY: `raw` is a live local for the call.
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut raw) }, 0);
        // SAFETY: `raw` is a live local; `cfmakeraw` only rewrites its fields.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: `raw` is a live local `tcsetattr` only reads.
        assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) }, 0);
        let deliberate = lflags(fd);

        restore_all();
        assert_eq!(
            lflags(fd),
            deliberate,
            "a descriptor nobody is holding was overwritten"
        );
    }

    /// A terminal `restore_all` has put back is forgotten too, so its number,
    /// once it belongs to another file, is not written to again.
    ///
    /// The stale number is made a copy of a new pty's *master*: master and
    /// slave share their settings, so writing through it rewrites the slave.
    /// `dup2` onto a number this test still owns, rather than waiting for the
    /// kernel to hand it out again, so no other test's file is involved.
    /// Before `restore_all` forgot what it restored, this failed every time.
    #[test]
    fn a_terminal_restored_by_restore_all_is_forgotten_too() {
        let _serial = serial();
        let first = open().expect("openpty");
        let stale = first.slave.as_raw_fd();
        std::mem::forget(RawMode::enable(stale).expect("enable").expect("a terminal"));
        restore_all();

        let second = open().expect("openpty");
        assert_eq!(
            // SAFETY: both are descriptors this test owns; `stale`'s previous
            // file is closed by the `dup2`, and `first` is not used again.
            unsafe { libc::dup2(second.master.as_raw_fd(), stale) },
            stale
        );
        let fd = second.slave.as_raw_fd();

        // SAFETY: all-zero bytes are a valid `termios`: plain data `tcgetattr`
        // fills in.
        let mut raw: libc::termios = unsafe { core::mem::zeroed() };
        // SAFETY: `raw` is a live local for the call.
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut raw) }, 0);
        // SAFETY: `raw` is a live local; `cfmakeraw` only rewrites its fields.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: `raw` is a live local `tcsetattr` only reads.
        assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) }, 0);
        let deliberate = lflags(fd);

        restore_all();
        assert_eq!(
            lflags(fd),
            deliberate,
            "settings saved for descriptor {stale} were written to the file that now has it"
        );
    }

    /// Leaving a terminal in raw mode makes the user's shell look broken, so
    /// the restore has to survive an unwind as well.
    ///
    /// Only the unwinding profile reaches this path; the abort profile is
    /// covered by `raw_mode_is_restored_even_when_no_destructor_runs` above.
    #[test]
    fn raw_mode_restores_even_when_the_scope_panics() {
        let _serial = serial();
        let pty = open().expect("openpty");
        let fd = pty.slave.as_raw_fd();

        let before = lflags(fd);

        let result = std::panic::catch_unwind(|| {
            let _guard = RawMode::enable(fd).unwrap();
            panic!("something went wrong inside the sandbox run");
        });
        assert!(result.is_err());

        assert_eq!(lflags(fd), before, "raw mode outlived the panic");
    }

    #[test]
    fn a_pipe_is_not_put_into_raw_mode() {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `pipe` writes two descriptors into a live two-element array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let guard = RawMode::enable(fds[0]).unwrap();
        assert!(guard.is_none(), "a pipe has no terminal settings to change");
        // SAFETY: both descriptors were made by the `pipe` above and are not
        // used again.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
