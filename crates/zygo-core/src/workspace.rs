// SPDX-License-Identifier: Apache-2.0
//! Files in and out of one request.
//!
//! A script is code and arrives by digest; a **workspace** is data and arrives
//! with the call. An embedder converting a document, resizing an image or
//! running a migration has bytes to hand the handler and bytes to collect
//! afterwards, and neither belongs in a JSON event.
//!
//! ```text
//!   POST /fn/convert            supervisor            sandbox
//!   workspace: {inline: tar} ──► unpack ──────────────► /work/<random>/
//!                                                       handler writes out/
//!   ?out=1                    ◄── pack ◄────────────────
//!                                remove
//! ```
//!
//! ## What isolates one request's files from another's
//!
//! Not a mount namespace. One sandbox serves several requests and, in a pool,
//! several tenants, so the obvious answer — give each request its own view
//! where `/work` means its own directory — needs a mount namespace per
//! request. A forked child cannot create one: measured on 6.12, a child at
//! uid 1000 has no `CAP_SYS_ADMIN` in the sandbox's user namespace and
//! `unshare(CLONE_NEWNS)` returns `EPERM`, with `--seccomp permissive` making
//! no difference. It is the user namespace, not a filter.
//!
//! What is left is three things together, and they are stated here rather than
//! implied because the first two are weaker than a namespace:
//!
//! * **The directory cannot be listed.** `/work` is `0311`, so a child cannot
//!   enumerate its neighbours (the same trick `/run/script` uses).
//! * **Its name cannot be guessed.** 128 bits from the kernel, not the request
//!   id — ids are a counter, and a counter is an invitation.
//! * **It does not outlive the request.** Removed when the request ends,
//!   whatever the request did, so the window is one request long.
//!
//! ## Unpacking somebody else's tar
//!
//! Every entry is checked rather than trusted, because this is a caller's
//! archive being written by a process that can reach the whole sandbox:
//!
//! * regular files and directories only — **no symlinks, no hard links**, no
//!   devices. A symlink is how an archive escapes a directory it was unpacked
//!   into, and there is nothing a workspace needs one for;
//! * relative paths with no `..` and no root prefix;
//! * modes are Zygo's, not the archive's, so nothing arrives setuid;
//! * a cap on entries and on total bytes, checked as they are written rather
//!   than from the header, which an archive is free to lie in.

use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, IoContext, Result};

/// Largest workspace this will unpack, in bytes.
///
/// The sandbox's own `scratch` bounds what the *handler* can then write; this
/// bounds what the supervisor writes on its behalf, which is a different
/// budget and has to be checked here — the supervisor is outside the cgroup
/// that would otherwise stop it.
pub const MAX_WORKSPACE_BYTES: u64 = 256 * 1024 * 1024;

/// Most files one workspace may hold.
///
/// An archive of a million empty files is small and still a denial of service:
/// every one is an inode on a tmpfs and a `openat` this process makes.
pub const MAX_WORKSPACE_ENTRIES: usize = 20_000;

/// Longest path an entry may have, in bytes.
pub const MAX_ENTRY_PATH: usize = 1024;

/// A name for one request's workspace directory.
///
/// 128 bits from the kernel. **Not the request id**, which is a counter: a
/// neighbour in the same sandbox who can guess the name can read the files,
/// and the whole of the defence here is that they cannot.
pub fn new_name() -> Result<String> {
    let path = Path::new("/dev/urandom");
    let mut bytes = [0u8; 16];
    let mut file = std::fs::File::open(path).at(path)?;
    file.read_exact(&mut bytes).at(path)?;
    Ok(hex::encode(bytes))
}

/// Unpack `tar` into `dir`, which must already exist and be empty.
///
/// Returns how many bytes were written. Every refusal names the entry, because
/// an embedder whose upload is rejected needs to know which file did it.
pub fn unpack(tar: &[u8], dir: &Path) -> Result<u64> {
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar));
    let mut written = 0u64;
    let mut entries = 0usize;

    for entry in archive
        .entries()
        .map_err(|e| refused("the body is not a tar archive", e))?
    {
        let mut entry = entry.map_err(|e| refused("the archive ended part way through", e))?;
        entries += 1;
        if entries > MAX_WORKSPACE_ENTRIES {
            return Err(refused(
                format!("the archive has more than {MAX_WORKSPACE_ENTRIES} entries"),
                std::io::Error::other("too many entries"),
            ));
        }

        let raw = entry
            .path()
            .map_err(|e| refused("an entry's path is not valid UTF-8", e))?
            .into_owned();
        let relative = check_path(&raw)?;
        let target = dir.join(&relative);

        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&target).at(&target)?;
                set_mode(&target, 0o700)?;
            }
            tar::EntryType::Regular => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).at(parent)?;
                }
                // Counted from what is actually read, not from the header:
                // an archive is free to claim one size and carry another.
                let mut file = std::fs::File::create(&target).at(&target)?;
                let left = MAX_WORKSPACE_BYTES.saturating_sub(written);
                let copied = std::io::copy(&mut entry.by_ref().take(left + 1), &mut file)
                    .map_err(|e| Error::io(&target, e))?;
                written += copied;
                if written > MAX_WORKSPACE_BYTES {
                    return Err(refused(
                        format!(
                            "the archive unpacks to more than {} MiB",
                            MAX_WORKSPACE_BYTES / (1024 * 1024)
                        ),
                        std::io::Error::other("workspace too large"),
                    ));
                }
                drop(file);
                set_mode(&target, 0o600)?;
            }
            // Deliberately everything else, named rather than ignored. A
            // symlink is how an archive writes outside the directory it was
            // unpacked into, a hard link is the same trick without a target
            // check, and a device node has no business in a workspace.
            other => {
                return Err(refused(
                    format!(
                        "`{}` is a {other:?}; a workspace holds files and directories only",
                        relative.display()
                    ),
                    std::io::Error::other("unsupported entry"),
                ));
            }
        }
    }
    Ok(written)
}

/// Pack `dir` into a tar, for the answer `?out=1` asks for.
///
/// Directories and regular files, like the way in — a handler that left a
/// symlink gets it skipped rather than the request failed, because by then the
/// work is done and losing the answer over a link is the wrong trade.
///
/// **Walked by descriptor, never by path.** `dir` is reached through
/// `/proc/<pid>/root`, and everything under it belongs to the handler — which
/// may have left a process running that is still changing it. A path would be
/// resolved again at every step, and an absolute symlink met on the way is
/// resolved against *this* process's root, not the sandbox's: a folder
/// swapped for `-> /home` between a check and a `read_dir` would pack the
/// supervisor's own files. So each step is `openat` on the folder already
/// open, with `O_NOFOLLOW`, and a name is packed only if what was opened is
/// still the kind of thing it was when the folder was read. The workspace
/// folder itself is opened the same way.
pub fn pack(dir: &Path) -> Result<Vec<u8>> {
    let root = rustix::fs::open(dir, entry_flags(true), rustix::fs::Mode::empty())
        .map_err(|e| Error::io(dir, e.into()))?;
    let mut builder = tar::Builder::new(Vec::new());
    let mut total = 0u64;
    pack_into(&mut builder, root.as_fd(), dir, Path::new(""), &mut total)?;
    builder
        .into_inner()
        .map_err(|e| Error::primitive("pack", "the workspace", e))
}

fn entry_flags(directory: bool) -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    // `NONBLOCK` so that a name swapped for a FIFO after it was looked at
    // opens at once, to be found out by the check below, rather than hang.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY;
    if directory {
        flags | OFlags::DIRECTORY | OFlags::CLOEXEC
    } else {
        flags | OFlags::CLOEXEC
    }
}

/// Open `name` in `parent` as a directory or a regular file, or `None` when it
/// is no longer one: replaced by a symlink, a socket, a FIFO or nothing since
/// the folder was read.
fn open_entry(
    parent: BorrowedFd<'_>,
    name: &std::ffi::CStr,
    directory: bool,
) -> std::io::Result<Option<(std::fs::File, std::fs::Metadata)>> {
    use rustix::io::Errno;
    let fd = match rustix::fs::openat(
        parent,
        name,
        entry_flags(directory),
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e)
            if e == Errno::LOOP
                || e == Errno::NXIO
                || e == Errno::NOTDIR
                || e == Errno::NOENT
                || e == Errno::OPNOTSUPP =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let file = std::fs::File::from(fd);
    let meta = file.metadata()?;
    let still = if directory {
        meta.is_dir()
    } else {
        meta.is_file()
    };
    Ok(still.then_some((file, meta)))
}

fn pack_into(
    builder: &mut tar::Builder<Vec<u8>>,
    dir: BorrowedFd<'_>,
    shown: &Path,
    prefix: &Path,
    total: &mut u64,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(dir).map_err(|e| Error::io(shown, e.into()))? {
        let entry = entry.map_err(|e| Error::io(shown, e.into()))?;
        let name = entry.file_name();
        if name != c"." && name != c".." {
            names.push(name.to_owned());
        }
    }
    // Sorted, so the same workspace packs to the same bytes twice. A caller
    // that hashes what it got back should not see it change because a
    // directory read came out in a different order.
    names.sort();

    for name in names {
        let leaf = std::ffi::OsStr::from_bytes(name.to_bytes());
        let inside = prefix.join(leaf);
        let path = shown.join(leaf);
        let stat = match rustix::fs::statat(dir, &name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            // Gone since the folder was read: nothing to pack.
            Err(rustix::io::Errno::NOENT) => continue,
            Err(e) => return Err(Error::io(&path, e.into())),
        };
        match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::Directory => {
                let Some((sub, meta)) = open_entry(dir, &name, true).at(&path)? else {
                    continue;
                };
                let mut header = tar::Header::new_gnu();
                header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
                builder
                    .append_data(&mut header, &inside, std::io::empty())
                    .map_err(|e| Error::io(&path, e))?;
                pack_into(builder, sub.as_fd(), &path, &inside, total)?;
            }
            rustix::fs::FileType::RegularFile => {
                let Some((file, meta)) = open_entry(dir, &name, false).at(&path)? else {
                    continue;
                };
                *total += meta.len();
                if *total > MAX_WORKSPACE_BYTES {
                    return Err(refused(
                        format!(
                            "the handler left more than {} MiB in its workspace",
                            MAX_WORKSPACE_BYTES / (1024 * 1024)
                        ),
                        std::io::Error::other("workspace too large"),
                    ));
                }
                // No more than the size that was counted: a process still
                // writing to the file cannot push the answer past the cap,
                // and the header always says how many bytes follow it.
                let mut bytes = Vec::new();
                (&file).take(meta.len()).read_to_end(&mut bytes).at(&path)?;
                let mut header = tar::Header::new_gnu();
                header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
                header.set_size(bytes.len() as u64);
                builder
                    .append_data(&mut header, &inside, bytes.as_slice())
                    .map_err(|e| Error::io(&path, e))?;
            }
            // Anything else — a symlink, a socket a handler happened to leave —
            // is skipped. See the note on `pack`.
            _ => {}
        }
    }
    Ok(())
}

/// Check one archive entry's path, or say why it is refused.
///
/// The whole of the traversal defence, and deliberately strict: a workspace is
/// a flat-ish bag of files, so there is nothing legitimate here that needs an
/// absolute path, a `..`, or a Windows prefix.
fn check_path(raw: &Path) -> Result<PathBuf> {
    let shown = raw.display().to_string();
    if shown.len() > MAX_ENTRY_PATH {
        return Err(refused(
            format!("an entry's path is longer than {MAX_ENTRY_PATH} bytes"),
            std::io::Error::other("path too long"),
        ));
    }
    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => out.push(part),
            // `./` is what `tar` itself writes for the archive root.
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(refused(
                    format!("`{shown}` leaves the workspace directory"),
                    std::io::Error::other("path escapes"),
                ));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(refused(
            format!("`{shown}` names nothing"),
            std::io::Error::other("empty path"),
        ));
    }
    Ok(out)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).at(path)
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn refused(what: impl std::fmt::Display, cause: impl Into<std::io::Error>) -> Error {
    Error::primitive("workspace", what.to_string(), cause.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tar built byte by byte, so it can carry a path the `tar` crate's
    /// own builder refuses to write.
    ///
    /// That refusal is a courtesy to whoever is *making* an archive and says
    /// nothing about what arrives over HTTP. A malicious archive is written by
    /// something that does not use this crate, so the test has to be too.
    fn hostile(name: &str, body: &[u8], kind: u8) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"000644 "); // mode
        header[108..115].copy_from_slice(b"000000 "); // uid
        header[116..123].copy_from_slice(b"000000 "); // gid
        let size = format!("{:011o} ", body.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000 "); // mtime
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // The checksum is computed with the field itself read as spaces.
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|b| *b as u32).sum();
        let text = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(text.as_bytes());

        let mut out = header.to_vec();
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out.extend(std::iter::repeat_n(0u8, 1024)); // two empty blocks: the end
        out
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *body)
                .expect("append");
        }
        builder.into_inner().expect("finish")
    }

    #[test]
    fn a_workspace_round_trips_through_a_tar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = unpack(
            &archive(&[("in.txt", b"hello"), ("sub/deep.txt", b"there")]),
            dir.path(),
        )
        .expect("unpack");
        assert_eq!(bytes, 10);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("in.txt")).expect("read"),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/deep.txt")).expect("read"),
            "there"
        );

        let back = tempfile::tempdir().expect("tempdir");
        unpack(&pack(dir.path()).expect("pack"), back.path()).expect("unpack again");
        assert_eq!(
            std::fs::read_to_string(back.path().join("sub/deep.txt")).expect("read"),
            "there"
        );
    }

    /// The answer to `?out=1` is read through `/proc/<pid>/root`, where an
    /// absolute symlink resolves against the supervisor's root. A handler that
    /// swaps its own workspace folder for a link must get nothing, not the
    /// folder the link names.
    #[test]
    fn a_workspace_folder_replaced_by_a_symlink_is_not_packed() {
        let host = tempfile::tempdir().expect("tempdir");
        let secret = host.path().join("operator");
        std::fs::create_dir(&secret).expect("mkdir");
        std::fs::write(secret.join("id_rsa"), b"not for tenants").expect("write");
        let workspace = host.path().join("work");
        std::os::unix::fs::symlink(&secret, &workspace).expect("symlink");

        assert!(pack(&workspace).is_err(), "a linked workspace was packed");
    }

    /// The race itself: a name that was a folder or a file when the folder was
    /// read, and is something else by the time it is opened.
    #[test]
    fn a_name_swapped_after_it_was_looked_at_is_not_followed() {
        let host = tempfile::tempdir().expect("tempdir");
        let outside = host.path().join("outside");
        std::fs::create_dir(&outside).expect("mkdir");
        std::fs::write(outside.join("secret"), b"x").expect("write");
        let work = host.path().join("work");
        std::fs::create_dir(&work).expect("mkdir");
        std::os::unix::fs::symlink(&outside, work.join("was-a-folder")).expect("symlink");
        std::os::unix::fs::symlink(outside.join("secret"), work.join("was-a-file"))
            .expect("symlink");
        let fifo = std::ffi::CString::new(
            work.join("was-a-file-too")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .expect("path");
        // SAFETY: a NUL-terminated path this test owns.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "mkfifo");

        let dir = rustix::fs::open(&work, entry_flags(true), rustix::fs::Mode::empty())
            .expect("open work");
        for (name, directory) in [
            (c"was-a-folder", true),
            (c"was-a-file", false),
            (c"was-a-file-too", false),
        ] {
            let opened = open_entry(dir.as_fd(), name, directory).expect("open_entry");
            assert!(
                opened.is_none(),
                "{name:?} was opened as a {}",
                if directory { "folder" } else { "file" }
            );
        }

        // And the whole walk: the links and the FIFO are skipped, not followed.
        std::fs::write(work.join("kept.txt"), b"mine").expect("write");
        let back = tempfile::tempdir().expect("tempdir");
        unpack(&pack(&work).expect("pack"), back.path()).expect("unpack");
        let mut got: Vec<_> = std::fs::read_dir(back.path())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        got.sort();
        assert_eq!(got, ["kept.txt"]);
    }

    /// The one thing this module exists to get right.
    #[test]
    fn an_archive_cannot_write_outside_the_directory_it_is_unpacked_into() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("work");
        std::fs::create_dir(&dir).expect("mkdir");

        for escape in [
            "../escaped",
            "/etc/passwd",
            "a/../../escaped",
            "../../escaped",
        ] {
            let err = unpack(&hostile(escape, b"owned", b'0'), &dir)
                .expect_err(&format!("`{escape}` was accepted"));
            assert!(
                format!("{err}").contains("leaves the workspace"),
                "`{escape}`: {err}"
            );
        }

        // And nothing was written anywhere, which is the claim that matters.
        assert!(!root.path().join("escaped").exists());
        assert_eq!(
            std::fs::read_dir(&dir).expect("read").count(),
            0,
            "a refused archive left something behind"
        );
    }

    /// A symlink is the other way out, and it is refused rather than followed.
    #[test]
    fn a_symlink_is_refused_because_it_is_how_an_archive_escapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `'2'` is a symlink; `'1'` is a hard link. Both are ways to name a
        // file outside the directory without the path itself saying so.
        for kind in *b"21" {
            let err = unpack(&hostile("link", b"", kind), dir.path()).expect_err("a link entry");
            assert!(
                format!("{err}").contains("files and directories only"),
                "{err}"
            );
        }
    }

    #[test]
    fn an_archive_that_lies_about_its_size_is_still_capped() {
        let dir = tempfile::tempdir().expect("tempdir");
        // One entry whose body is larger than the cap. The header is not
        // consulted: the bytes are counted as they are copied.
        let big = vec![b'x'; (MAX_WORKSPACE_BYTES + 1024) as usize];
        let err = unpack(&archive(&[("big", &big)]), dir.path()).expect_err("over the cap");
        assert!(format!("{err}").contains("unpacks to more than"), "{err}");
    }

    #[test]
    fn nothing_arrives_executable_or_setuid() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        // What an archive would carry to get a setuid binary into place.
        header.set_mode(0o4755);
        header.set_cksum();
        builder
            .append_data(&mut header, "prog", &b"bin"[..])
            .expect("append");
        let tar = builder.into_inner().expect("finish");

        unpack(&tar, dir.path()).expect("unpack");
        let mode = std::fs::metadata(dir.path().join("prog"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o600, "mode {mode:o} came from the archive");
    }

    #[test]
    fn a_name_is_not_a_counter() {
        let a = new_name().expect("a");
        let b = new_name().expect("b");
        assert_eq!(a.len(), 32, "{a}");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
