//! Tightening the permissions of files devbox-mcp already finds on disk, and warning about directories it must not
//! change.
//!
//! What devbox-mcp creates is private from the start (directories 0700, files 0600). But state and configuration from
//! before that, or created by hand, can be readable or writable by other users: session records hold agents' replies,
//! and whoever can rewrite the unit decides what the service runs. So devbox-mcp tightens the paths that are its own
//! whenever it uses them, and only warns about the shared directories above them (`~/.config`, `~/.local/state`), which
//! other programs use and which are the user's to change (SPEC_impl.md, "Storage" and "Deployment").

use std::fs::{self, File, OpenOptions, Permissions};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use anyhow::Context as _;

/// Permission bits that give anyone but the owner access.
const OTHERS: u32 = 0o077;

/// Makes the directory at `path` private (mode 0700) if anyone but its owner has access to it.
///
/// Fails if the mode cannot be changed, for instance because the directory belongs to another user: devbox-mcp then
/// cannot keep what it stores there private, so callers treat that as fatal rather than carrying on.
pub fn tighten_dir(path: &Path) -> anyhow::Result<()> {
    let mode = fs::metadata(path)
        .with_context(|| format!("cannot inspect {}", path.display()))?
        .mode();
    if mode & OTHERS != 0 {
        fs::set_permissions(path, Permissions::from_mode(0o700))
            .with_context(|| format!("cannot make {} private (mode 0700)", path.display()))?;
    }
    Ok(())
}

/// Makes the already open `file` private (mode 0600) if anyone but its owner has access to it. Working on the open
/// file rather than its path means the change cannot be redirected to another file.
pub fn tighten_open_file(file: &File) -> std::io::Result<()> {
    if file.metadata()?.mode() & OTHERS != 0 {
        file.set_permissions(Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Makes the regular file at `path` private (mode 0600) if anyone but its owner has access to it, without following a
/// symbolic link there.
///
/// The path is opened with `O_NOFOLLOW` and changed through that descriptor, so a link someone placed at the path (in a
/// directory that used to be writable by others) cannot make devbox-mcp change the mode of some other file of the user.
/// Anything that is not a regular file is left alone and reported as an error.
pub fn tighten_file_nofollow(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    tighten_open_file(&file)
}

/// Warns on stderr about each directory above `path` that belongs to the current user and that others can write to.
///
/// Whoever can write to such a directory can rename `path` away and put their own in its place, so it undermines
/// whatever privacy `path` has. These directories are shared with other programs, so devbox-mcp does not change them
/// itself. Directories owned by someone else (`/`, `/home`, `/tmp`, owned by root) are not the user's to fix and are
/// skipped; so are directories that cannot be inspected.
pub fn warn_loose_ancestors(path: &Path) {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    for dir in path.ancestors().skip(1) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let Ok(meta) = fs::metadata(dir) else {
            continue;
        };
        if meta.uid() == me && meta.mode() & 0o022 != 0 {
            eprintln!(
                "devbox-mcp: warning: {} is writable by other users (mode {:o}), so they could replace {} inside it; \
                 consider `chmod go-w {}`",
                dir.display(),
                meta.mode() & 0o777,
                path.display(),
                dir.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record path that is a symbolic link must not make devbox-mcp change the mode of the file it points to: the
    /// sessions directory used to be writable by others, so such a link could aim the chmod at any file of the user.
    #[test]
    fn does_not_follow_a_link_when_tightening_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "x").unwrap();
        std::fs::set_permissions(&target, Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("alpha.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(tighten_file_nofollow(&link).is_err());
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o644);

        let file = dir.path().join("beta.json");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, Permissions::from_mode(0o664)).unwrap();
        tighten_file_nofollow(&file).unwrap();
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
    }
}
