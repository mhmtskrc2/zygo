//! `zygo login <registry>` — store a credential for a private registry.
//!
//! Two decisions worth stating, because both are about what this command
//! deliberately does not do.
//!
//! **There is no `--password` flag.** A password in `argv` is visible in `ps`
//! to every process on the machine, and it lands in the shell's history file.
//! Docker deprecated theirs for exactly this and so does everything since.
//! The password comes from the terminal with echo off, or from standard input
//! with `--password-stdin`, which is what a CI job or a secret manager should
//! use.
//!
//! **The credential is checked before it is written.** A `login` that stores
//! what it was told is a setting, not a login: a typo surfaces much later, on
//! a `pull`, looking like a wrong image name. This makes the same request a
//! pull starts with and exchanges the credential for a token, so the answer
//! arrives while the user is still looking at the prompt.
//!
//! It is written to Zygo's own `auth.json`, never to `~/.docker/config.json`.
//! Reading Docker's file is a compatibility promise (principle P4: somebody
//! already logged in does not log in twice); editing it is not, and a
//! credential *helper* entry there is something this must not overwrite.

use std::io::{BufRead, Write};

use anyhow::Context;
use zygo_core::image::auth::{Credential, CredentialStore};
use zygo_core::image::{RegistryClient, Store, Verified};

use crate::cli::Cli;
use crate::output::Style;

pub fn run(
    cli: &Cli,
    registry: &str,
    username: Option<&str>,
    password_stdin: bool,
) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    paths.ensure()?;

    let style = Style::stdout();
    let username = match username {
        Some(u) => u.to_string(),
        None => prompt_line(&format!("Username for {registry}: "))?,
    };
    anyhow::ensure!(!username.is_empty(), "a username is required");

    let password = if password_stdin {
        read_all_stdin()?
    } else {
        prompt_password(
            registry,
            &format!("Password for {username} at {registry}: "),
        )?
    };
    anyhow::ensure!(!password.is_empty(), "a password is required");

    let credential = Credential {
        username: username.clone(),
        password,
    };

    let client = RegistryClient::new(Store::new(paths.clone()))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let verdict = runtime
        .block_on(client.verify(registry, &credential))
        .with_context(|| format!("`{registry}` did not accept that credential"))?;

    if verdict == Verified::NoCredentialsNeeded {
        println!(
            "{} {registry} serves anyone; nothing was stored",
            style.yellow("·")
        );
        return Ok(0);
    }

    let path = CredentialStore::zygo_path(&paths);
    // Read, insert, write: another registry's line in the same file must
    // survive this one.
    let mut store = CredentialStore::from_file(&path);
    store.insert(registry, credential);
    store
        .save(&path)
        .with_context(|| format!("could not write {}", path.display()))?;

    println!("{} signed in to {registry} as {username}", style.green("✓"));
    println!("{}", style.dim(&format!("  stored in {}", path.display())));
    Ok(0)
}

/// One line from the terminal, echoed. For a username, which is not secret.
fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read from the terminal")?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Everything on standard input, for `--password-stdin`.
///
/// Only the trailing newline is stripped: a password may legitimately end in
/// a space, and `echo` adds exactly one newline.
fn read_all_stdin() -> anyhow::Result<String> {
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut buf)
        .context("could not read the password from standard input")?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}

/// One line from the terminal with echo off, restored afterwards whatever
/// happens.
///
/// Refuses when there is no terminal rather than reading a password that
/// would be echoed into a log: a pipeline that meant to supply one has
/// `--password-stdin`, and saying so is better than silently doing the
/// dangerous thing.
#[cfg(unix)]
fn prompt_password(registry: &str, prompt: &str) -> anyhow::Result<String> {
    use std::os::fd::AsRawFd;

    let fd = std::io::stdin().as_raw_fd();
    // SAFETY: `isatty` takes a descriptor and cannot fail destructively.
    if unsafe { libc::isatty(fd) } != 1 {
        anyhow::bail!(
            "standard input is not a terminal, so the password cannot be read without \
             echoing it\n  → pipe it instead: printf %s \"$TOKEN\" | zygo login \
             {registry} --password-stdin"
        );
    }

    // SAFETY: `termios` is written by `tcgetattr` before it is read.
    let mut original: libc::termios = unsafe { core::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return Err(std::io::Error::last_os_error()).context("could not read the terminal mode");
    }
    let mut quiet = original;
    quiet.c_lflag &= !libc::ECHO;
    // SAFETY: `quiet` is a copy of a valid `termios`.
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &quiet) } != 0 {
        return Err(std::io::Error::last_os_error()).context("could not turn the echo off");
    }

    // `ISIG` stays on, so Ctrl-C at the prompt is how you cancel it — and it
    // killed the process with echo still off, leaving the shell typing
    // invisibly (B-08, the code review). This handler puts the echo back and
    // then lets the signal do what it was going to do.
    let restore = EchoRestored { fd };
    install_echo_handler(fd);

    let typed = prompt_line(prompt);

    // Restored before the result is examined, so an error on the way in does
    // not leave the user with a terminal that has stopped echoing. The guard
    // covers the paths this line does not: a panic, and `panic = "abort"`.
    drop(restore);
    println!();
    typed
}

/// Puts the echo back, however this scope is left.
#[cfg(unix)]
struct EchoRestored {
    fd: std::os::fd::RawFd,
}

#[cfg(unix)]
impl Drop for EchoRestored {
    fn drop(&mut self) {
        restore_echo(self.fd);
    }
}

/// Turn echo back on for `fd`.
///
/// Reads the current settings and sets one flag rather than writing back a
/// saved copy, which is what makes it usable from a signal handler:
/// `tcgetattr` and `tcsetattr` are both async-signal-safe, and no state has to
/// be shared with the handler at all.
#[cfg(unix)]
fn restore_echo(fd: std::os::fd::RawFd) {
    // SAFETY: both calls take a descriptor and a `termios` this function owns.
    unsafe {
        let mut current: libc::termios = core::mem::zeroed();
        if libc::tcgetattr(fd, &mut current) == 0 {
            current.c_lflag |= libc::ECHO;
            libc::tcsetattr(fd, libc::TCSAFLUSH, &current);
        }
    }
}

/// The descriptor the signal handler should put back, or `-1`.
#[cfg(unix)]
static PROMPTING: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
fn install_echo_handler(fd: std::os::fd::RawFd) {
    use std::sync::atomic::Ordering;

    PROMPTING.store(fd, Ordering::SeqCst);

    extern "C" fn on_interrupt(signal: libc::c_int) {
        use std::sync::atomic::Ordering;
        let fd = PROMPTING.load(Ordering::SeqCst);
        if fd >= 0 {
            restore_echo(fd);
        }
        // Default disposition, then re-raise: the user asked to cancel, and
        // this handler's only business was the terminal.
        //
        // SAFETY: restoring a default disposition and re-raising is the
        // standard way to let a signal proceed after cleaning up.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    // SAFETY: `on_interrupt` does nothing that is not async-signal-safe.
    unsafe {
        let handler = on_interrupt as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

#[cfg(not(unix))]
fn prompt_password(_registry: &str, _prompt: &str) -> anyhow::Result<String> {
    anyhow::bail!("reading a password without echo needs a Unix terminal; use --password-stdin")
}
