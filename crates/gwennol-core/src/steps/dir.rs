//! A directory held open: the anchor every operation of a write is
//! relative to, once the operator has approved the path it stands for.
//!
//! `host_fs.write` approves a path and then makes directories, a
//! temporary, and finally the destination — each a separate syscall,
//! and each, done by path, resolving that path from the root again.
//! Between the approval and the last of them a parent directory can be
//! swapped for a symlink, or renamed away and something else put in its
//! place, and the same spelled path then names somewhere the operator
//! never saw. Holding the deepest existing ancestor open and doing
//! everything after the approval *relative to that handle* — `openat`,
//! `mkdirat`, `fstatat`, `renameat`, `unlinkat` — closes that: the
//! handle *is* the directory, wherever its name goes, and a name
//! resolved from it resolves inside it. Nothing below the handle is
//! followed through a symlink (`O_NOFOLLOW` on every descent), so a link
//! planted at a name the operator approved as "to be created" is
//! refused, not followed.
//!
//! On a target without the `openat` family the same interface is
//! provided over paths — the milestone-1 behaviour the handle-based one
//! replaces, with the race it does not close.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::fd::OwnedFd;

/// What a name inside a directory is, without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory, which a descent may hold.
    Directory,
    /// A symlink, which nothing here follows.
    Symlink,
    /// A regular file, or anything else that is neither of the above.
    Other,
}

/// A name's metadata inside a directory, as `lstat` reports it: what it
/// is, and its permissions as the filesystem holds them (every bit, on
/// unix — the caller decides which survive a replacement).
#[derive(Debug, Clone)]
pub struct Stat {
    /// What the name is.
    pub kind: Kind,
    /// Its permissions, every bit.
    pub permissions: std::fs::Permissions,
}

/// An open directory.
#[derive(Debug)]
pub struct Dir {
    #[cfg(unix)]
    fd: OwnedFd,
    #[cfg(not(unix))]
    path: PathBuf,
}

/// The flags a directory is held with: a handle to resolve names
/// against, which needs search permission on the directory and not
/// read — a `0o311` drop box takes a write through the handle as it
/// does by path. `O_PATH` and `O_SEARCH` say exactly that where they
/// exist; elsewhere a read-only open is the closest thing.
#[cfg(unix)]
fn search_flags() -> nix::fcntl::OFlag {
    use nix::fcntl::OFlag;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let search = OFlag::O_PATH;
    #[cfg(target_vendor = "apple")]
    let search = OFlag::O_SEARCH;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    let search = OFlag::O_RDONLY;
    search | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC
}

impl Dir {
    /// Hold the directory at `path` open, following symlinks on the way
    /// — the caller canonicalises and verifies afterwards, through
    /// [`Dir::open_canonical`]. A path that is not a directory is
    /// `NotADirectory`; one the agent's user may not search is
    /// `PermissionDenied`.
    pub fn open(path: &Path) -> io::Result<Dir> {
        #[cfg(unix)]
        {
            let fd = nix::fcntl::open(path, search_flags(), nix::sys::stat::Mode::empty())?;
            Ok(Dir { fd })
        }
        #[cfg(not(unix))]
        {
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::Error::from(io::ErrorKind::NotADirectory));
            }
            Ok(Dir {
                path: path.to_path_buf(),
            })
        }
    }

    /// Hold `path` open and return it canonical, verified — by device
    /// and inode, as `host_fs.read` verifies its file — to name the very
    /// directory the handle holds. An approval of the returned path is
    /// then an approval of the returned handle; a path swapped between
    /// the open and the check is refused rather than have one directory
    /// approved and another written.
    pub fn open_canonical(path: &Path) -> io::Result<(Dir, PathBuf)> {
        let dir = Dir::open(path)?;
        let canonical = std::fs::canonicalize(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let named = std::fs::metadata(&canonical)?;
            let held = nix::sys::stat::fstat(&dir.fd)?;
            #[allow(clippy::unnecessary_cast)] // dev_t and ino_t vary by platform
            if (named.dev(), named.ino()) != (held.st_dev as u64, held.st_ino as u64) {
                return Err(io::Error::other(format!(
                    "{} changed while being opened",
                    path.display()
                )));
            }
        }
        Ok((dir, canonical))
    }

    /// Hold the directory `name` directly inside this one, following no
    /// symlink: a link at that name — whatever it points at — is
    /// `NotADirectory`, as a file is.
    pub fn open_child(&self, name: &OsStr) -> io::Result<Dir> {
        #[cfg(unix)]
        {
            let fd = nix::fcntl::openat(
                &self.fd,
                name,
                search_flags() | nix::fcntl::OFlag::O_NOFOLLOW,
                nix::sys::stat::Mode::empty(),
            )
            .map_err(|e| {
                // `O_NOFOLLOW` reports a link as ELOOP, except where
                // `O_PATH` lets the open reach the link itself and
                // `O_DIRECTORY` then refuses it — one answer either way.
                if e == nix::errno::Errno::ELOOP {
                    io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "is a symlink, which is not followed",
                    )
                } else {
                    io::Error::from(e)
                }
            })?;
            Ok(Dir { fd })
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "is a symlink, which is not followed",
                ));
            }
            if !meta.is_dir() {
                return Err(io::Error::from(io::ErrorKind::NotADirectory));
            }
            Ok(Dir { path })
        }
    }

    /// Make the directory `name` inside this one (mode `0o777`, less
    /// the umask). `AlreadyExists` if anything is at that name.
    pub fn mkdir(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            nix::sys::stat::mkdirat(
                &self.fd,
                name,
                nix::sys::stat::Mode::from_bits_truncate(0o777),
            )?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir(self.path.join(name))
        }
    }

    /// What `name` is inside this directory, without following it.
    pub fn lstat(&self, name: &OsStr) -> io::Result<Stat> {
        #[cfg(unix)]
        {
            use nix::sys::stat::SFlag;
            use std::os::unix::fs::PermissionsExt as _;
            let st =
                nix::sys::stat::fstatat(&self.fd, name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)?;
            let kind = match SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT {
                SFlag::S_IFDIR => Kind::Directory,
                SFlag::S_IFLNK => Kind::Symlink,
                _ => Kind::Other,
            };
            #[allow(clippy::unnecessary_cast)] // mode_t is u16 on some targets
            let permissions = std::fs::Permissions::from_mode((st.st_mode as u32) & 0o7777);
            Ok(Stat { kind, permissions })
        }
        #[cfg(not(unix))]
        {
            let meta = std::fs::symlink_metadata(self.path.join(name))?;
            let t = meta.file_type();
            let kind = if t.is_symlink() {
                Kind::Symlink
            } else if t.is_dir() {
                Kind::Directory
            } else {
                Kind::Other
            };
            Ok(Stat {
                kind,
                permissions: meta.permissions(),
            })
        }
    }

    /// Create the file `name` inside this directory, exclusively — it
    /// must not exist, and a symlink at that name makes creation fail
    /// rather than redirect it — born with `mode` (less the umask), open
    /// for writing.
    pub fn create_new(&self, name: &OsStr, mode: u32) -> io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use nix::fcntl::OFlag;
            let fd = nix::fcntl::openat(
                &self.fd,
                name,
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
                #[allow(clippy::unnecessary_cast)] // mode_t is u16 on some targets
                nix::sys::stat::Mode::from_bits_truncate(mode as _),
            )?;
            Ok(std::fs::File::from(fd))
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.path.join(name))
        }
    }

    /// Rename `from` to `to`, both inside this directory: atomic, and
    /// replacing whatever entry is at `to`.
    pub fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            nix::fcntl::renameat(&self.fd, from, &self.fd, to)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            std::fs::rename(self.path.join(from), self.path.join(to))
        }
    }

    /// Remove the file `name` from this directory.
    pub fn unlink(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            nix::unistd::unlinkat(&self.fd, name, nix::unistd::UnlinkatFlags::NoRemoveDir)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            std::fs::remove_file(self.path.join(name))
        }
    }
}

/// A name for a temporary, unique to one attempt.
pub fn temp_name(beside: &OsStr, nonce: u64) -> OsString {
    let mut name = OsString::from(".");
    name.push(beside);
    name.push(format!(".{nonce:016x}.gwennol-tmp"));
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_descent_follows_no_symlink_whatever_it_points_at() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Dir::open(tmp.path()).unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("to-dir")).unwrap();
        std::os::unix::fs::symlink("nowhere", tmp.path().join("dangling")).unwrap();
        std::fs::write(tmp.path().join("plain"), "x").unwrap();
        assert!(root.open_child(OsStr::new("real")).is_ok());
        for name in ["to-dir", "dangling", "plain"] {
            let e = root.open_child(OsStr::new(name)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::NotADirectory, "{name}: {e}");
        }
        assert_eq!(
            root.lstat(OsStr::new("to-dir")).unwrap().kind,
            Kind::Symlink
        );
        assert_eq!(
            root.lstat(OsStr::new("real")).unwrap().kind,
            Kind::Directory
        );
        assert_eq!(root.lstat(OsStr::new("plain")).unwrap().kind, Kind::Other);
    }

    #[cfg(unix)]
    #[test]
    fn the_handle_outlives_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("here")).unwrap();
        let (dir, canonical) = Dir::open_canonical(&tmp.path().join("here")).unwrap();
        assert_eq!(canonical, tmp.path().canonicalize().unwrap().join("here"));
        // The directory moves; the handle does not.
        std::fs::rename(tmp.path().join("here"), tmp.path().join("there")).unwrap();
        let file = dir.create_new(OsStr::new("made"), 0o644).unwrap();
        drop(file);
        assert!(tmp.path().join("there/made").exists());
        assert!(!tmp.path().join("here").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_created_file_is_born_with_its_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = Dir::open(tmp.path()).unwrap();
        let file = dir.create_new(OsStr::new("private"), 0o600).unwrap();
        assert_eq!(
            file.metadata().unwrap().permissions().mode() & 0o7777,
            0o600
        );
        // Exclusive: the same name again is refused, and so is a symlink
        // planted at a name.
        assert_eq!(
            dir.create_new(OsStr::new("private"), 0o600)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        std::os::unix::fs::symlink("elsewhere", tmp.path().join("link")).unwrap();
        assert_eq!(
            dir.create_new(OsStr::new("link"), 0o600)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }
}
