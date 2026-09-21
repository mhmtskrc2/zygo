//! Running a sandbox to completion with its output captured.
//!
//! The build steps — installing a requirements file, installing system
//! packages — are sandboxes like any other, except that nobody is watching
//! their terminal. Everything they print goes to one pipe, so a failure can
//! show the last lines of `pip` or `apt` rather than "exit 1".

use std::os::fd::AsRawFd;

use crate::error::{Error, Result};
use crate::sandbox::SandboxConfig;

/// Start `config`, wait for it, and return its exit code with everything it
/// wrote to stdout and stderr.
///
/// `config.stdio` is replaced by the pipe; whatever it held is ignored.
pub fn run_captured(config: &mut SandboxConfig, paths: &crate::Paths) -> Result<(i32, Vec<u8>)> {
    let (read_end, write_end) = rustix::pipe::pipe()
        .map_err(|e| Error::primitive("pipe", "internal build error", e.into()))?;
    let drain = {
        let mut read_end = std::fs::File::from(read_end);
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = std::io::Read::read_to_end(&mut read_end, &mut out);
            out
        })
    };
    config.stdio = Some(write_end.as_raw_fd());

    let backend = crate::backend::for_isolation(config.isolation, paths)?;
    let outcome = backend.start(config).and_then(|mut sandbox| sandbox.wait());
    // Ours is the last write end once the sandbox has exited; closing it is
    // what lets the drain thread see end of file.
    drop(write_end);
    config.stdio = None;
    let output = drain.join().unwrap_or_default();

    outcome.map(|code| (code, output))
}

/// The last `n` lines of captured output, as text.
pub fn tail(output: &[u8], n: usize) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines_only() {
        let out = b"a\nb\nc\nd\n";
        assert_eq!(tail(out, 2), "c\nd");
        assert_eq!(tail(out, 10), "a\nb\nc\nd");
        assert_eq!(tail(b"", 3), "");
    }
}
