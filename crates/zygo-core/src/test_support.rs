// SPDX-License-Identifier: Apache-2.0
//! What the unit tests share.

use std::path::Path;

/// Writes `script` to `path` as an executable, without this process ever
/// holding `path` open for writing.
///
/// The tests run on parallel threads, and a thread that forks while another
/// has a freshly written file open hands the child that descriptor: running
/// the file then fails with `ETXTBSY`, "Text file busy", until the child
/// execs. A CI run lost the fake `pasta` of a `net` test to exactly this.
/// Copying the file in with `cp` keeps the only writable descriptor in a
/// process of its own, which nothing here forks.
pub(crate) fn write_executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;

    let source = path.with_extension("source");
    std::fs::write(&source, script).expect("write the script");
    let status = std::process::Command::new("cp")
        .arg(&source)
        .arg(path)
        .status()
        .expect("run cp");
    assert!(status.success(), "cp to {} failed", path.display());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make it executable");
}
