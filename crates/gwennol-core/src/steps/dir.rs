//! A directory held open: the anchor every operation of a write is
//! relative to, and the directory a listing reads, once the operator
//! has approved the path it stands for.
//!
//! `host_fs.write` approves a path and then makes directories, a
//! temporary, and finally the destination — each a separate syscall,
//! and each, done by path, resolving that path from the root again.
//! Between the approval and the last of them a parent directory can be
//! swapped for a symlink, or renamed away and something else put in its
//! place, and the same spelled path then names somewhere the operator
//! never saw. `host_fs.list` has the same shape in one step: approve a
//! directory, then read whatever the path names by then. Holding the
//! directory open *before* the approval, verifying that the approved
//! path names the very directory the handle holds, and doing everything
//! after the approval relative to that handle — `openat`, `mkdirat`,
//! `fstatat`, `renameat`, `unlinkat`, `fdopendir` — closes both: the
//! handle *is* the directory, wherever its name goes, and a name
//! resolved from it resolves inside it. Nothing below the handle is
//! followed through a symlink (`O_NOFOLLOW` on every descent), so a link
//! at a name the operator approved as "to be created" is refused, not
//! followed.
//!
//! On a target without the `openat` family and `fdopendir` — non-unix,
//! and Redox, where nix has no `dir` module — the same interface is
//! provided over paths (`cfg(dir_handles)`, from the build script,
//! draws the line): the milestone-1 behaviour the handle-based one
//! replaces, with the race it does not close.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(dir_handles)]
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
/// is, its size, and its permissions as the filesystem holds them (every
/// bit, on unix — the caller decides which survive a replacement).
#[derive(Debug, Clone)]
pub struct Stat {
    /// What the name is.
    pub kind: Kind,
    /// Its size in bytes, as the filesystem reports it.
    pub size: u64,
    /// Its permissions, every bit.
    pub permissions: std::fs::Permissions,
    /// Device and inode: what makes two names the same file. `None`
    /// where the path-based fallback cannot say.
    pub identity: Option<(u64, u64)>,
}

/// One entry of a listing: the name and what [`Dir::lstat`] said of it,
/// if it could be asked.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The entry's name, as the directory spells it.
    pub name: OsString,
    /// Its metadata, or `None` where the entry could not be asked about:
    /// it vanished between being listed and being asked, or the
    /// directory is readable but not searchable, so every entry is a
    /// name and nothing more.
    pub stat: Option<Stat>,
}

/// How a directory is held: to resolve names against, or to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// A handle to resolve names against, which needs no read permission
    /// on the directory — a `0o311` drop box takes a write through the
    /// handle as it does by path. `O_SEARCH` gives such a handle and
    /// checks search permission; `O_PATH` gives one and checks nothing
    /// on the directory itself, leaving the descent to meet a refusal.
    /// One or the other is used on every target where nix exposes it;
    /// elsewhere a read-only open is the closest thing, and a drop box
    /// is refused there.
    Search,
    /// A handle whose entries will be read, which needs read permission
    /// as `opendir` does.
    Read,
}

/// An open directory.
#[derive(Debug)]
pub struct Dir {
    #[cfg(dir_handles)]
    fd: OwnedFd,
    #[cfg(not(dir_handles))]
    path: PathBuf,
}

#[cfg(dir_handles)]
fn flags(hold: Hold) -> nix::fcntl::OFlag {
    use nix::fcntl::OFlag;
    let access = match hold {
        Hold::Search => {
            #[cfg(any(
                target_os = "linux",
                target_os = "android",
                target_os = "redox",
                target_os = "fuchsia"
            ))]
            let search = OFlag::O_PATH;
            #[cfg(any(
                target_vendor = "apple",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "solaris",
                target_os = "illumos",
                target_os = "aix"
            ))]
            let search = OFlag::O_SEARCH;
            #[cfg(not(any(
                target_os = "linux",
                target_os = "android",
                target_os = "redox",
                target_os = "fuchsia",
                target_vendor = "apple",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "solaris",
                target_os = "illumos",
                target_os = "aix"
            )))]
            let search = OFlag::O_RDONLY;
            search
        }
        Hold::Read => OFlag::O_RDONLY,
    };
    access | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC
}

/// The check an approval of a canonical path rests on: that the path
/// names the very object whose handle is held — by device and inode —
/// or else the path was swapped mid-open and the step refuses rather
/// than approve one thing and touch another. `host_fs.read` makes it on
/// its file; [`Dir::open_canonical`] on its directory.
#[cfg(unix)]
pub fn verify_named(canonical: &Path, held: (u64, u64)) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let named = std::fs::metadata(canonical)?;
    if (named.dev(), named.ino()) != held {
        return Err(io::Error::other("changed while being opened"));
    }
    Ok(())
}

/// A symlink reached with `O_NOFOLLOW`, as each platform reports it.
#[cfg(dir_handles)]
fn is_nofollow_refusal(e: nix::errno::Errno) -> bool {
    use nix::errno::Errno;
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "hurd",
        target_os = "cygwin"
    ))]
    if e == Errno::EFTYPE {
        return true;
    }
    matches!(e, Errno::ELOOP | Errno::EMLINK)
}

impl Dir {
    /// Hold the directory at `path` open, following symlinks on the way
    /// — the caller canonicalises and verifies afterwards, through
    /// [`Dir::open_canonical`]. A path that is not a directory is
    /// `NotADirectory`; one the agent's user lacks the hold's permission
    /// on is `PermissionDenied` — read, for [`Hold::Read`]; search, for
    /// [`Hold::Search`] where the open checks it (`O_SEARCH` does;
    /// `O_PATH` checks nothing on the directory itself, and the descent
    /// meets the refusal instead).
    pub fn open(path: &Path, hold: Hold) -> io::Result<Dir> {
        #[cfg(dir_handles)]
        {
            let fd = nix::fcntl::open(path, flags(hold), nix::sys::stat::Mode::empty())?;
            Ok(Dir { fd })
        }
        #[cfg(not(dir_handles))]
        {
            let _ = hold;
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::Error::from(io::ErrorKind::NotADirectory));
            }
            Ok(Dir {
                path: path.to_path_buf(),
            })
        }
    }

    /// Hold `path` open and return it canonical, verified by
    /// [`verify_named`] to name the very directory the handle holds. An
    /// approval of the returned path is then an approval of the returned
    /// handle.
    pub fn open_canonical(path: &Path, hold: Hold) -> io::Result<(Dir, PathBuf)> {
        let dir = Dir::open(path, hold)?;
        let canonical = std::fs::canonicalize(path)?;
        #[cfg(dir_handles)]
        {
            let held = nix::sys::stat::fstat(&dir.fd)?;
            #[allow(clippy::unnecessary_cast)] // dev_t and ino_t vary by platform
            verify_named(&canonical, (held.st_dev as u64, held.st_ino as u64))?;
        }
        Ok((dir, canonical))
    }

    /// Hold the directory `name` directly inside this one, following no
    /// symlink: a link at that name — whatever it points at — is
    /// `NotADirectory`, as a file is.
    pub fn open_child(&self, name: &OsStr) -> io::Result<Dir> {
        #[cfg(dir_handles)]
        {
            let fd = nix::fcntl::openat(
                &self.fd,
                name,
                flags(Hold::Search) | nix::fcntl::OFlag::O_NOFOLLOW,
                nix::sys::stat::Mode::empty(),
            )
            .map_err(|e| {
                // `O_NOFOLLOW` reports a link as ELOOP, EMLINK (FreeBSD,
                // DragonFly) or EFTYPE (NetBSD; accepted wherever nix
                // defines it), except where `O_PATH` lets the open reach
                // the link itself and `O_DIRECTORY` then refuses it — one
                // answer either way.
                if is_nofollow_refusal(e) {
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
        #[cfg(not(dir_handles))]
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
        #[cfg(dir_handles)]
        {
            nix::sys::stat::mkdirat(
                &self.fd,
                name,
                nix::sys::stat::Mode::from_bits_truncate(0o777),
            )?;
            Ok(())
        }
        #[cfg(not(dir_handles))]
        {
            std::fs::create_dir(self.path.join(name))
        }
    }

    /// What `name` is inside this directory, without following it.
    pub fn lstat(&self, name: &OsStr) -> io::Result<Stat> {
        #[cfg(dir_handles)]
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
            #[allow(clippy::unnecessary_cast)] // mode_t, off_t, dev_t, ino_t vary by platform
            Ok(Stat {
                kind,
                size: st.st_size as u64,
                permissions: std::fs::Permissions::from_mode((st.st_mode as u32) & 0o7777),
                identity: Some((st.st_dev as u64, st.st_ino as u64)),
            })
        }
        #[cfg(not(dir_handles))]
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
                size: meta.len(),
                permissions: meta.permissions(),
                identity: None,
            })
        }
    }

    /// Create the file `name` inside this directory, exclusively — it
    /// must not exist, and a symlink at that name makes creation fail
    /// rather than redirect it — born with `mode` (less the umask), open
    /// for writing.
    pub fn create_new(&self, name: &OsStr, mode: u32) -> io::Result<std::fs::File> {
        #[cfg(dir_handles)]
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
        #[cfg(not(dir_handles))]
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            // The birth mode is the point: a temporary born wider and
            // narrowed later has a window in which an opener keeps a
            // readable descriptor no chmod revokes.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                opts.mode(mode);
            }
            #[cfg(not(unix))]
            let _ = mode;
            opts.open(self.path.join(name))
        }
    }

    /// Rename `from` to `to`, both inside this directory: atomic, and
    /// replacing a file or a link at `to` — a directory there fails.
    pub fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        #[cfg(dir_handles)]
        {
            nix::fcntl::renameat(&self.fd, from, &self.fd, to)?;
            Ok(())
        }
        #[cfg(not(dir_handles))]
        {
            std::fs::rename(self.path.join(from), self.path.join(to))
        }
    }

    /// Remove the file `name` from this directory.
    pub fn unlink(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(dir_handles)]
        {
            nix::unistd::unlinkat(&self.fd, name, nix::unistd::UnlinkatFlags::NoRemoveDir)?;
            Ok(())
        }
        #[cfg(not(dir_handles))]
        {
            std::fs::remove_file(self.path.join(name))
        }
    }

    /// Read this directory's entries — at most `max`, in whatever order
    /// the directory yields them, with `.` and `..` left out — and
    /// whether it held more. Needs a [`Hold::Read`] handle; the entries
    /// come from the handle, whatever the directory's name is by now.
    pub fn list(&self, max: usize) -> io::Result<(Vec<Entry>, bool)> {
        let mut entries = Vec::new();
        let mut truncated = false;
        #[cfg(dir_handles)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            // `fdopendir` takes the descriptor over, and a `Dir` may
            // outlive one listing: it reads through a duplicate. The
            // duplicate shares the handle's offset, so the stream is put
            // at the start first rather than left where the last listing
            // stopped (nix rewinds when its iterator drops; this does
            // not rely on it).
            let dup = self.fd.try_clone()?;
            nix::unistd::lseek(&dup, 0, nix::unistd::Whence::SeekSet)?;
            let mut dir = nix::dir::Dir::from_fd(dup)?;
            for entry in dir.iter() {
                let entry = entry?;
                let name = OsStr::from_bytes(entry.file_name().to_bytes());
                if name == "." || name == ".." {
                    continue;
                }
                if entries.len() >= max {
                    truncated = true;
                    break;
                }
                entries.push(Entry {
                    name: name.to_os_string(),
                    stat: self.lstat(name).ok(),
                });
            }
        }
        #[cfg(not(dir_handles))]
        {
            for entry in std::fs::read_dir(&self.path)? {
                let entry = entry?;
                if entries.len() >= max {
                    truncated = true;
                    break;
                }
                let name = entry.file_name();
                let stat = self.lstat(&name).ok();
                entries.push(Entry { name, stat });
            }
        }
        Ok((entries, truncated))
    }
}

/// A name for a temporary beside `beside`, unique to one attempt and
/// bounded in length: the destination's name is quoted only in part, so
/// a destination near `NAME_MAX` still has room for a temporary.
pub fn temp_name(beside: &OsStr, nonce: u64) -> OsString {
    const QUOTED: usize = 24;
    let lossy = beside.to_string_lossy();
    let mut cut = lossy.len().min(QUOTED);
    while !lossy.is_char_boundary(cut) {
        cut -= 1;
    }
    OsString::from(format!(".{}.{nonce:016x}.gwennol-tmp", &lossy[..cut]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_descent_follows_no_symlink_whatever_it_points_at() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Dir::open(tmp.path(), Hold::Search).unwrap();
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
        let plain = root.lstat(OsStr::new("plain")).unwrap();
        assert_eq!(plain.kind, Kind::Other);
        assert_eq!(plain.size, 1);
    }

    #[cfg(dir_handles)]
    #[test]
    fn the_handle_outlives_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("here")).unwrap();
        let (dir, canonical) = Dir::open_canonical(&tmp.path().join("here"), Hold::Search).unwrap();
        assert_eq!(canonical, tmp.path().canonicalize().unwrap().join("here"));
        // The directory moves; the handle does not.
        std::fs::rename(tmp.path().join("here"), tmp.path().join("there")).unwrap();
        let file = dir.create_new(OsStr::new("made"), 0o644).unwrap();
        drop(file);
        assert!(tmp.path().join("there/made").exists());
        assert!(!tmp.path().join("here").exists());
    }

    #[cfg(dir_handles)]
    #[test]
    fn a_listing_comes_from_the_handle_and_leaves_out_the_dots() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("here")).unwrap();
        std::fs::write(tmp.path().join("here/a"), "aa").unwrap();
        std::fs::create_dir(tmp.path().join("here/b")).unwrap();
        let dir = Dir::open(&tmp.path().join("here"), Hold::Read).unwrap();
        std::fs::rename(tmp.path().join("here"), tmp.path().join("there")).unwrap();
        let (mut entries, truncated) = dir.list(10).unwrap();
        entries.sort_by(|x, y| x.name.cmp(&y.name));
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec![OsString::from("a"), OsString::from("b")]);
        assert!(!truncated);
        assert_eq!(entries[0].stat.as_ref().unwrap().size, 2);
        assert_eq!(entries[1].stat.as_ref().unwrap().kind, Kind::Directory);
        // Two listings from one handle; a cap that cuts says so.
        let (cut, truncated) = dir.list(1).unwrap();
        assert_eq!(cut.len(), 1);
        assert!(truncated);
    }

    #[cfg(unix)]
    #[test]
    fn a_created_file_is_born_with_its_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = Dir::open(tmp.path(), Hold::Search).unwrap();
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

    /// The identity the case-fold check compares: two names of one file
    /// share it, two files do not.
    #[cfg(dir_handles)]
    #[test]
    fn identity_is_the_file_not_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Dir::open(tmp.path(), Hold::Search).unwrap();
        std::fs::write(tmp.path().join("one"), "x").unwrap();
        std::fs::write(tmp.path().join("two"), "x").unwrap();
        std::fs::hard_link(tmp.path().join("one"), tmp.path().join("also-one")).unwrap();
        let one = dir.lstat(OsStr::new("one")).unwrap().identity;
        assert!(one.is_some());
        assert_eq!(one, dir.lstat(OsStr::new("also-one")).unwrap().identity);
        assert_ne!(one, dir.lstat(OsStr::new("two")).unwrap().identity);
    }

    #[test]
    fn a_temporary_name_is_bounded_however_long_the_destination() {
        let long = "x".repeat(255);
        let name = temp_name(OsStr::new(&long), 7);
        assert!(name.len() < 64, "{}", name.len());
        assert!(name.to_string_lossy().ends_with(".gwennol-tmp"));
        // Cut on a character boundary, not inside one: byte 24 falls
        // inside the eighth euro sign, so the cut backs up to 22.
        let multibyte = format!("a{}", "€".repeat(10));
        let name = temp_name(OsStr::new(&multibyte), 7).into_string().unwrap();
        assert!(name.starts_with(".a€€€€€€€."), "{name}");
        assert_ne!(temp_name(OsStr::new("a"), 1), temp_name(OsStr::new("a"), 2));
    }
}
