//! `host_fs.read`, `host_fs.write`, `host_fs.list`.
//!
//! # Outcomes are data
//!
//! A filesystem answer the *model* should react to — no such file, that
//! path is a directory, permission denied — comes back as the step's
//! result with an `outcome` field naming it, never as a step error. The
//! rule is `docs/SPI.md`'s: a declarative tool's only failure primitive
//! is `try`, whose `catch` sees an error as a string, so a tool that
//! wrapped these steps in `try` could tell "file not found" from "the
//! operator said no" only by matching English text. Handing the model's
//! outcomes over as data leaves the string path to the failures it is
//! for: the operator's denial, the kernel's refusal, cancellation, and
//! I/O errors nobody can act on.
//!
//! Every miss is still approved first. The operator sees the path the
//! plugin probed — canonical up to its deepest canonicalisable ancestor, since a
//! missing file has no canonical path of its own — before the plugin
//! learns that nothing is there.

use std::ffi::{OsStr, OsString};
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::path::{Path, PathBuf};

use gwead::kernel::{PluginExecution, StepError, StepOutput};
use gwead::serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::dir::{Dir, Hold, Kind};
use super::{
    StepFuture, bool_param, cancelled, capped, lossy_capped, or_cancelled, resolve, str_param,
    u64_param,
};
use crate::host::{approval, approve, resolve_path};
use crate::operator::Access;

/// Default cap on bytes returned by `fs_read`.
pub const DEFAULT_READ_MAX_BYTES: u64 = 1 << 20;
/// Hard ceiling on `max_bytes`: larger requests are clamped, so a plugin
/// cannot ask the host to buffer without bound.
pub const READ_BYTES_CEILING: u64 = 64 << 20;
/// Default cap on entries returned by `fs_list`.
pub const DEFAULT_LIST_MAX_ENTRIES: u64 = 1000;
/// Hard ceiling on `max_entries`: larger requests are clamped.
pub const LIST_ENTRIES_CEILING: u64 = 100_000;

/// The `outcome` value of a step that did its work.
pub const OUTCOME_OK: &str = "ok";

/// A filesystem answer the model can act on, as the `outcome` a step
/// reports instead of failing. Each variant is one value of the
/// manifests' `outcome` enumeration; the mapping from OS errors is
/// [`Outcome::classify`], and everything it does not classify stays a
/// step error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing exists at the path (or a component of it is missing).
    NotFound,
    /// The path names a directory where a file was expected.
    IsDirectory,
    /// A component of the path — or the path itself, for a listing —
    /// is not a directory. For a write, also a symlink at an
    /// intermediate component below the deepest canonicalisable
    /// ancestor, which the descent does not follow whatever it points
    /// at (a symlink at the destination itself is [`Outcome::IsSymlink`]).
    NotADirectory,
    /// The agent's user may not do this.
    PermissionDenied,
    /// A write destination is a symlink, which the host refuses to
    /// write through or replace (the approved path would not name where
    /// the bytes land, and the rename would destroy the link).
    IsSymlink,
}

impl Outcome {
    /// The outcome an I/O error maps to, if the model can act on it.
    pub fn classify(e: &std::io::Error) -> Option<Outcome> {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => Some(Outcome::NotFound),
            ErrorKind::IsADirectory => Some(Outcome::IsDirectory),
            ErrorKind::NotADirectory => Some(Outcome::NotADirectory),
            ErrorKind::PermissionDenied => Some(Outcome::PermissionDenied),
            _ => None,
        }
    }

    /// The manifests' enumeration value.
    pub fn name(self) -> &'static str {
        match self {
            Outcome::NotFound => "not_found",
            Outcome::IsDirectory => "is_directory",
            Outcome::NotADirectory => "not_a_directory",
            Outcome::PermissionDenied => "permission_denied",
            Outcome::IsSymlink => "is_symlink",
        }
    }

    /// The step result for this outcome at `path`: the enumeration value
    /// plus a one-line `message` a tool can hand the model verbatim.
    fn result(self, path: &Path) -> StepOutput {
        let path = path.display();
        let message = match self {
            Outcome::NotFound => format!("no such file or directory: {path}"),
            Outcome::IsDirectory => format!("is a directory: {path}"),
            Outcome::NotADirectory => format!("not a directory: {path}"),
            Outcome::PermissionDenied => format!("permission denied: {path}"),
            Outcome::IsSymlink => {
                format!("is a symlink, which the host will not write through or replace: {path}")
            }
        };
        json!({"outcome": self.name(), "message": message}).into()
    }
}

/// `host_fs.read`: `{path, max_bytes?}` → `{outcome: "ok", content,
/// truncated, size}`, or `{outcome, message}` for a miss.
///
/// The operator is shown the *canonical* path — symlinks resolved — and
/// the step verifies, by device and inode, that it names the very file
/// whose already-open handle is then read: the returned bytes provably
/// come from the file that was approved, however the paths move meanwhile.
///
/// The cap bounds what is *read*, not just what is returned: at most one
/// byte past `max_bytes` leaves the file, so a huge file cannot balloon the
/// host and a special file that never ends still terminates. `size` is the
/// size the filesystem reports, which for such a file may not be the number
/// of bytes a full read would produce.
pub fn fs_read<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let path = resolve_path(str_param(&p, "path")?);
        let max = capped(
            u64_param(&p, "max_bytes", DEFAULT_READ_MAX_BYTES)?,
            READ_BYTES_CEILING,
        );
        let cancel = ex.cancel_token();
        // Opened before the approval so the approval can describe the very
        // file this handle holds; opening for read has no side effects.
        // This is the one documented exception to the module's
        // validate → approve → work ordering. O_NONBLOCK so that a FIFO at
        // an unapproved path cannot park the open waiting for a writer;
        // fifos and sockets are then refused outright — they are conduits,
        // not files, and "read this file" cannot honestly describe them.
        let open = async {
            let mut opts = tokio::fs::OpenOptions::new();
            opts.read(true);
            #[cfg(unix)]
            opts.custom_flags(nix::libc::O_NONBLOCK);
            let file = opts.open(&path).await?;
            let meta = file.metadata().await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt as _;
                let t = meta.file_type();
                if t.is_fifo() || t.is_socket() {
                    return Err(std::io::Error::other(
                        "not a readable file (fifo or socket)",
                    ));
                }
            }
            // O_NONBLOCK deliberately stays set on the handle. A regular
            // file ignores it; a character device that has no data ready
            // then fails the read fast (EAGAIN) instead of parking a
            // blocking worker forever — a tty could otherwise wedge one
            // per read, and cancellation cannot reclaim a parked worker.
            // Failing fast over waiting is the host's side of the
            // bounded-work bargain. The cost, accepted: a slow device's
            // partially-read bytes are discarded with a generic I/O error
            // rather than returned.
            let canonical = tokio::fs::canonicalize(&path).await?;
            std::io::Result::Ok((file, meta, canonical))
        };
        let (file, meta, canonical) = match or_cancelled(&cancel, open).await? {
            Ok(opened) => opened,
            // No handle to bind an approval to — but the operator still
            // sees the probe before the plugin learns its answer. The
            // path is approved canonical up to its deepest canonicalisable
            // ancestor, the closest thing a missing file has to a
            // canonical path.
            Err(e) => {
                let Some(outcome) = Outcome::classify(&e) else {
                    return Err(StepError::Failed(format!("read {}: {e}", path.display())));
                };
                let probed = or_cancelled(&cancel, canonicalize_missing(&path))
                    .await?
                    .map_err(|e| StepError::Failed(format!("read {}: {e}", path.display())))?;
                let ask = approval(&*ex, Access::ReadFile(probed));
                approve(ask).await?;
                return Ok(outcome.result(&path));
            }
        };
        // The operator judges the canonical path, so it must name the file
        // the handle holds — otherwise the path was swapped mid-open and
        // the step refuses rather than approve one file and read another.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let (named, held) = (canonical.clone(), (meta.dev(), meta.ino()));
            blocking(move || super::dir::verify_named(&named, held))
                .await
                .and_then(|verified| verified)
                .map_err(|e| StepError::Failed(format!("read {}: {e}", path.display())))?;
        }
        let ask = approval(&*ex, Access::ReadFile(canonical));
        approve(ask).await?;
        // Opening a directory read-only succeeds on unix; it is the read
        // that would fail, and "is a directory" is the model's to act on.
        if meta.is_dir() {
            return Ok(Outcome::IsDirectory.result(&path));
        }
        let size = meta.len();
        let read = async {
            let mut bytes = Vec::new();
            // One byte past the cap distinguishes "exactly the cap" from
            // "truncated" without reading the rest. The cap is already
            // clamped; saturating so the arithmetic cannot wrap even for
            // an unclamped caller.
            file.take((max as u64).saturating_add(1))
                .read_to_end(&mut bytes)
                .await?;
            std::io::Result::Ok(bytes)
        };
        let bytes = or_cancelled(&cancel, read)
            .await?
            .map_err(|e| StepError::Failed(format!("read {}: {e}", path.display())))?;
        let (content, truncated) = lossy_capped(&bytes, max);
        Ok(json!({
            "outcome": OUTCOME_OK,
            "content": content,
            "truncated": truncated,
            "size": size,
        })
        .into())
    })
}

/// Resolve a path that need not exist: the deepest ancestor that
/// canonicalises — a symlinked directory resolves to where it really
/// leads; a component that is a file, or that may not be searched, ends
/// the walk like a missing one — and the remainder is appended as
/// spelled.
/// This is the path a write approval names — except a symlink
/// destination, approved under its own name with its parent canonical
/// — and the path a miss is approved under; a frontend that judges
/// those paths by pattern must
/// spell its patterns the same way, which is why this is public and
/// synchronous — the one walk, for the host and for whoever needs to
/// agree with it by construction.
pub fn deepest_canonical(path: &Path) -> std::io::Result<PathBuf> {
    let (_, mut canonical, below) = split_at_existing(path)?;
    canonical.extend(below);
    Ok(canonical)
}

/// The walk behind [`deepest_canonical`]: the deepest ancestor of `path`
/// that canonicalises — as spelled, and canonical — and the components
/// below it, in order.
fn split_at_existing(path: &Path) -> std::io::Result<(&Path, PathBuf, Vec<OsString>)> {
    let mut existing = path;
    let mut below: Vec<OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(canonical) => {
                below.reverse();
                return Ok((existing, canonical, below));
            }
            // A component that exists but is not a directory, or one
            // the agent's user may not search, ends the walk the same
            // way a missing one does: what lies below it cannot be
            // canonicalised, only spelled — and the step is about to
            // report the outcome as data, so the approval must not be
            // the thing that turns it back into an error.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                below.push(
                    existing
                        .file_name()
                        .ok_or_else(|| std::io::Error::other("path has no file name"))?
                        .to_os_string(),
                );
                existing = existing
                    .parent()
                    .ok_or_else(|| std::io::Error::other("path has no existing ancestor"))?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// [`deepest_canonical`] off the runtime's blocking pool — the same
/// place `tokio::fs::canonicalize` would have done each probe, in one
/// hop instead of one per component.
async fn canonicalize_missing(path: &Path) -> std::io::Result<PathBuf> {
    let path = path.to_path_buf();
    blocking(move || deepest_canonical(&path)).await?
}

/// Run `work` on the blocking pool: a few syscalls that must not park
/// a runtime worker, in one hop.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(std::io::Error::other)
}

/// A destination's answer to an error met at `component` on the way to
/// it. `not_a_directory` names the component that is not one; every
/// other outcome names the destination that was asked for — the same
/// answer whichever ancestor a platform's handle happened to catch it
/// at (`O_PATH` holds an unsearchable directory that `O_SEARCH` refuses,
/// so one state is met a level apart on Linux and macOS). What is not
/// an outcome is an error, naming the component.
fn destination_answer(
    what: &str,
    component: &Path,
    path: &Path,
    e: std::io::Error,
) -> Result<StepOutput, StepError> {
    match Outcome::classify(&e) {
        Some(Outcome::NotADirectory) => Ok(Outcome::NotADirectory.result(component)),
        Some(outcome) => Ok(outcome.result(path)),
        None => Err(StepError::Failed(format!(
            "{what} {}: {e}",
            component.display()
        ))),
    }
}

/// Where a write is anchored: the path the operator judges, the handle
/// on its deepest canonicalisable ancestor, and the names below.
struct Anchored {
    /// The approved path: what [`deepest_canonical`] spells for the
    /// destination — except a symlink destination, approved under its
    /// own name with its parent canonical.
    approved: PathBuf,
    /// The anchor as canonicalised — a prefix of `approved`, and the
    /// path a message about the ancestor itself names.
    anchor: PathBuf,
    /// The handle on the anchor, or why it could not be held: it is a
    /// file (`NotADirectory`), the agent's user may not search it
    /// (`PermissionDenied`), or it went away since the walk found it
    /// (`NotFound`) — the destination's answer, given after the
    /// approval as data like any other.
    dir: std::io::Result<Dir>,
    /// The names below the anchor in order, the last the destination's
    /// own. Every one before it failed to canonicalise when the anchor
    /// was found: missing, or a file, or a directory the agent's user
    /// may not search — which the descent finds out and reports.
    below: Vec<OsString>,
    /// The destination is a symlink: refused after the approval, as
    /// data.
    dest_is_symlink: bool,
}

/// Find and hold a write's anchor. Opening a directory for search has
/// no side effects, so this precedes the approval — the read path's
/// exception, shared — and the approval can then name the very
/// directory the handle holds.
fn anchor(path: &Path) -> std::io::Result<Anchored> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::other("path has no parent directory"))?;
    let mut name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?
        .to_os_string();
    let (existing, walked, mut below) = split_at_existing(parent)?;
    let (dir, anchor) = match Dir::open_canonical(existing, Hold::Search) {
        Ok((dir, canonical)) => (Ok(dir), canonical),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotADirectory
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::NotFound
            ) =>
        {
            (Err(e), walked)
        }
        Err(e) => return Err(e),
    };
    // Only a destination whose parent is the anchor can exist at all.
    // One that does is approved under the name the path canonicalises
    // to — on a case-insensitive filesystem the name on disk, not the
    // one spelled — because that is what `deepest_canonical` spells for
    // it and so what a frontend's patterns match; and it is written
    // under that name too. The canonicalisation is by path — the one
    // path-based step after the handle is held that nothing has yet
    // vouched for — so what it names is checked through the handle: the
    // name on disk must lie in the anchor *and* be the very file the
    // spelled name was (by device and inode). A link that turned up
    // meanwhile — to a sibling, say — resolves to another file and is
    // refused rather than have its target approved and replaced; and a
    // file that is simply not the same one any more (an editor's save
    // landing in the window) is the race it is, not a symlink.
    let mut dest_is_symlink = false;
    if below.is_empty()
        && let Ok(dir) = &dir
    {
        match dir.lstat(&name) {
            Ok(stat) if stat.kind == Kind::Symlink => dest_is_symlink = true,
            Ok(spelled) => {
                if let Some(on_disk) =
                    vouched_name(dir, &anchor, &spelled, || std::fs::canonicalize(path))?
                {
                    name = on_disk;
                }
            }
            // Missing — or unanswerable through the handle (unsearchable,
            // say), which is answered as data once the approval is given.
            Err(_) => {}
        }
    }
    below.push(name);
    let mut approved = anchor.clone();
    approved.extend(&below);
    Ok(Anchored {
        approved,
        anchor,
        dir,
        below,
        dest_is_symlink,
    })
}

/// The on-disk name of an existing destination, vouched for by the
/// handle. `resolve` canonicalises the destination by path — the one
/// step after the handle is held that nothing has yet vouched for — and
/// the name it yields must lie in `anchor` and be the very file
/// `spelled` was, by device and inode. `None`: the destination is gone
/// since `spelled` looked, and the spelled name stands, as for a
/// missing file. A name that resolves elsewhere, or to another file, is
/// the race it is — "changed while being opened" — and not a symlink: a
/// link at the destination's name was answered before this ran, and a
/// fresh regular file (an editor's save landing in the window) is no
/// link either.
fn vouched_name(
    dir: &Dir,
    anchor: &Path,
    spelled: &super::dir::Stat,
    resolve: impl FnOnce() -> std::io::Result<PathBuf>,
) -> std::io::Result<Option<OsString>> {
    let changed = || std::io::Error::other("changed while being opened");
    let canonical = match resolve() {
        Ok(canonical) => canonical,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if canonical.parent() != Some(anchor) {
        return Err(changed());
    }
    let on_disk = canonical
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?;
    match dir.lstat(on_disk) {
        Ok(named) => match (spelled.identity, named.identity) {
            (Some(a), Some(b)) if a != b => Err(changed()),
            // Equal — or the path fallback, which closes no race.
            _ => Ok(Some(on_disk.to_os_string())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// What a write holds once its approval is given and its ground is
/// prepared: the destination's directory, open; the temporary made in
/// it, open for writing; and what the destination's permissions are to
/// be, if it is being replaced.
struct Prepared {
    dir: Dir,
    name: OsString,
    tmp: OsString,
    file: std::fs::File,
    dest_perms: Option<std::fs::Permissions>,
}

/// Between the approval and the bytes, everything relative to the
/// anchor: descend through the missing directories — making them with
/// `create_dirs`, following no symlink — classify the destination, and
/// create the temporary beside it. What is not a [`Prepared`] is the
/// step's answer: an outcome as data, or an error.
fn prepare(
    mut dir: Dir,
    mut at: PathBuf,
    mut below: Vec<OsString>,
    create_dirs: bool,
    path: &Path,
) -> Result<Prepared, Result<StepOutput, StepError>> {
    let Some(name) = below.pop() else {
        return Err(Err(StepError::Failed("path has no file name".into())));
    };
    for component in below {
        at.push(&component);
        if create_dirs {
            match dir.mkdir(&component) {
                Ok(()) => {}
                // Made meanwhile by someone else — or something else is
                // in the way, which the descent reports.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(destination_answer("mkdir", &at, path, e)),
            }
        }
        dir = match dir.open_child(&component) {
            Ok(dir) => dir,
            // A file, or a symlink — however long it has been there,
            // whatever it points at — which the descent does not
            // follow: `not_a_directory`, naming it. Still missing, with
            // `create_dirs` not asked for: `not_found`. Not searchable
            // through the handle: `permission_denied`.
            Err(e) => return Err(destination_answer("write", &at, path, e)),
        };
    }
    // Whether this replaces an existing file decides both the
    // temporary's birth mode and the permissions it ends with.
    let dest_perms = match dir.lstat(&name) {
        Ok(stat) if stat.kind == Kind::Directory => {
            return Err(Ok(Outcome::IsDirectory.result(path)));
        }
        // Planted since the anchor looked: the same refusal, as data.
        Ok(stat) if stat.kind == Kind::Symlink => {
            return Err(Ok(Outcome::IsSymlink.result(path)));
        }
        Ok(stat) => Some(replacement_permissions(stat.permissions)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(destination_answer("write", &at, path, e)),
    };
    let (file, tmp) = match create_temp_in(&dir, &name, dest_perms.is_some()) {
        Ok(created) => created,
        // The directory is not writable, or something the classification
        // above could not see: the destination's answer, reported as
        // such.
        Err(e) => return Err(destination_answer("write", &at, path, e)),
    };
    Ok(Prepared {
        dir,
        name,
        tmp,
        file,
        dest_perms,
    })
}

/// The permissions a replacement keeps: the permission bits only.
/// `fchmod` after the write means the kernel will not strip
/// setuid/setgid for us, and replacing an `04755` file must not mint a
/// setuid binary owned by the agent's user.
fn replacement_permissions(perms: std::fs::Permissions) -> std::fs::Permissions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::Permissions::from_mode(perms.mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        perms
    }
}

/// Create an exclusive temporary beside `dest` in `dir`, named with a
/// random nonce — so a file or symlink planted at a guessed name makes
/// creation *fail* rather than redirect the write. With `restrict`
/// (used when replacing an existing file) the temporary is born 0600,
/// so its content is never readable beyond what the destination
/// allowed — there is no create-then-chmod gap in which another opener
/// can grab a readable fd. (A birth mode is a unix notion; on another
/// target the fallback creates with the platform's default.)
fn create_temp_in(
    dir: &Dir,
    dest: &OsStr,
    restrict: bool,
) -> std::io::Result<(std::fs::File, OsString)> {
    let mode = if restrict { 0o600 } else { 0o666 };
    for _ in 0..16 {
        let nonce = RandomState::new().build_hasher().finish();
        let tmp = super::dir::temp_name(dest, nonce);
        match dir.create_new(&tmp, mode) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(
        "could not create a unique temporary file",
    ))
}

/// Fill the temporary: write, flush to disk, and — when replacing an
/// existing file — set its permissions on the *open handle* (fchmod, so a
/// swapped path cannot redirect the chmod), widening the 0600 it was born
/// with only after the content is fully written.
async fn fill_temp(
    mut file: tokio::fs::File,
    content: &str,
    dest_perms: Option<std::fs::Permissions>,
) -> std::io::Result<()> {
    file.write_all(content.as_bytes()).await?;
    // Flushed to disk before the rename makes it the file's content.
    file.sync_all().await?;
    if let Some(perms) = dest_perms {
        file.set_permissions(perms).await?;
    }
    Ok(())
}

/// `host_fs.write`: `{path, content, create_dirs?}` → `{outcome: "ok",
/// bytes_written}`, or `{outcome, message}` when the destination cannot
/// take the file (a missing parent without `create_dirs`, a directory or
/// a non-directory in the way, permission denied, a symlink).
///
/// The approved path is what [`deepest_canonical`] spells for the
/// destination — canonical up to its deepest canonicalisable ancestor, so the
/// operator judges where the bytes will actually land, not an alias —
/// and that ancestor is *held open* before the approval, verified to be
/// what the canonical path names, and is what every operation after the
/// approval is relative to: the directories `create_dirs` makes, the
/// temporary, the rename that makes it the destination. A parent
/// swapped after the approval cannot redirect the write, because nothing
/// after the approval resolves the parent by name again; the bytes land
/// in the directory the operator approved, wherever its name has gone
/// meanwhile. See [`super::dir`].
///
/// Below the anchor no symlink is followed, however long it has been
/// there: a link at a component the operator was shown as "to be
/// created" — a dangling one that was always there, or one planted
/// after the approval — is `not_a_directory`, naming it. A symlink
/// destination is never written through or replaced — the outcome is
/// `is_symlink`, approved as a probe under the link's own name with its
/// parent canonical: resolving the link would show the operator a write
/// to the target that is precisely what will not happen. A link planted
/// at the destination's name after the approval gets the same answer;
/// one planted between that check and the rename is replaced by the
/// rename, in the approved directory under the approved name — the
/// bytes still land where the operator was told, and only the link is
/// lost.
///
/// The write goes through a temporary file in the same directory and a
/// rename, so the destination is never observable half-written.
pub fn fs_write<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let path = resolve_path(str_param(&p, "path")?);
        let content = match p.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => {
                return Err(StepError::Failed(format!(
                    "param 'content' must be a string, got {other}"
                )));
            }
            None => return Err(StepError::Failed("missing required param 'content'".into())),
        };
        let create_dirs = bool_param(&p, "create_dirs", false)?;
        let cancel = ex.cancel_token();
        let failed =
            |e: std::io::Error| StepError::Failed(format!("write {}: {e}", path.display()));
        // The anchor is found and held before the approval, so the
        // approval can name the very directory the handle holds — the
        // module's documented exception to validate → approve → work,
        // shared with `host_fs.read`. A few syscalls, none with a side
        // effect: raced against cancellation, and abandoned if it wins.
        let anchored = {
            let path = path.clone();
            or_cancelled(&cancel, blocking(move || anchor(&path))).await?
        }
        .and_then(|found| found)
        .map_err(failed)?;
        let Anchored {
            approved,
            anchor,
            dir,
            below,
            dest_is_symlink,
        } = anchored;
        let ask = approval(&*ex, Access::WriteFile(approved.clone()));
        approve(ask).await?;
        let dir = match dir {
            Ok(dir) => dir,
            // The ancestor is a file, may not be searched, or is gone:
            // the destination's answer, reported as such.
            Err(e) => return destination_answer("write", &anchor, &path, e),
        };
        if dest_is_symlink {
            return Ok(Outcome::IsSymlink.result(&path));
        }
        // Creation and rename are quick local operations and are not raced
        // against cancellation: a dropped future cannot clean up after
        // itself, so cancellation is consulted *between* phases instead. A
        // cancelled write either leaves the destination untouched with the
        // temporary removed, or — once the rename begins — completes.
        let prepared = {
            let path = path.clone();
            blocking(move || prepare(dir, anchor, below, create_dirs, &path))
                .await
                .map_err(failed)?
        };
        let Prepared {
            dir,
            name,
            tmp,
            file,
            dest_perms,
        } = match prepared {
            Ok(prepared) => prepared,
            Err(answer) => return answer,
        };
        let file = tokio::fs::File::from_std(file);
        let filled = or_cancelled(&cancel, fill_temp(file, &content, dest_perms)).await;
        let cancelled_after_fill = cancel.is_cancelled();
        if !matches!(filled, Ok(Ok(()))) || cancelled_after_fill {
            let _ = blocking(move || dir.unlink(&tmp)).await;
            filled?.map_err(failed)?;
            return Err(cancelled());
        }
        let renamed = blocking(move || {
            dir.rename(&tmp, &name).inspect_err(|_| {
                let _ = dir.unlink(&tmp);
            })
        })
        .await
        .map_err(failed)?;
        if let Err(e) = renamed {
            return destination_answer("write", &approved, &path, e);
        }
        Ok(json!({"outcome": OUTCOME_OK, "bytes_written": content.len()}).into())
    })
}

/// `host_fs.list`: `{path, max_entries?}` → `{outcome: "ok", entries:
/// [{name, kind, size}], truncated}`, or `{outcome, message}` when there
/// is no directory to list.
///
/// The operator is shown the canonical directory — symlinks resolved —
/// and the step lists the very directory that was shown: it is held
/// open before the approval, verified to be what the canonical path
/// names, and read through that handle, so a directory swapped after
/// the approval is not what gets listed. See [`super::dir`]. One level
/// only — recursion is a tool's decision to make, one approved listing
/// at a time — and at most `max_entries` entries come back, with
/// `truncated` saying whether the directory held more. A truncated
/// listing is whatever the directory yielded first, sorted for
/// presentation — not the lexicographically first entries, which would
/// require reading the whole directory the cap exists to avoid.
pub fn fs_list<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let path = resolve_path(str_param(&p, "path")?);
        let max = capped(
            u64_param(&p, "max_entries", DEFAULT_LIST_MAX_ENTRIES)?,
            LIST_ENTRIES_CEILING,
        );
        let cancel = ex.cancel_token();
        let failed = |e: std::io::Error| StepError::Failed(format!("list {}: {e}", path.display()));
        // Held — for reading, which is what a listing needs — before the
        // approval, so the approval names the very directory the handle
        // holds: the module's exception, shared by all three steps.
        let opened = {
            let path = path.clone();
            or_cancelled(
                &cancel,
                blocking(move || Dir::open_canonical(&path, Hold::Read)),
            )
            .await?
        }
        .and_then(|opened| opened);
        let (dir, canonical) = match opened {
            Ok(opened) => opened,
            // Nothing to list, or nothing this user may — approved as a
            // probe of the deepest canonicalisable ancestor plus the spelled
            // remainder, then answered as data.
            Err(e) => {
                let Some(outcome) = Outcome::classify(&e) else {
                    return Err(failed(e));
                };
                let probed = or_cancelled(&cancel, canonicalize_missing(&path))
                    .await?
                    .map_err(failed)?;
                let ask = approval(&*ex, Access::ListDir(probed));
                approve(ask).await?;
                return Ok(outcome.result(&path));
            }
        };
        let ask = approval(&*ex, Access::ListDir(canonical));
        approve(ask).await?;
        // Bounded by the cap, so one blocking hop; cancellation is
        // consulted after it rather than between entries.
        let (entries, truncated) = blocking(move || dir.list(max))
            .await
            .and_then(|listed| listed)
            .map_err(failed)?;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let mut entries: Vec<Value> = entries
            .into_iter()
            .map(|entry| {
                let kind = match entry.stat.as_ref().map(|s| s.kind) {
                    Some(Kind::Directory) => "dir",
                    Some(Kind::Symlink) => "symlink",
                    Some(Kind::Other) => "file",
                    None => "unknown",
                };
                json!({
                    "name": entry.name.to_string_lossy(),
                    "kind": kind,
                    "size": entry.stat.as_ref().map(|s| s.size),
                })
            })
            .collect();
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"outcome": OUTCOME_OK, "entries": entries, "truncated": truncated}).into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_restricted_temporary_is_born_0600_not_chmodded_down_later() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = Dir::open(tmp.path(), Hold::Search).unwrap();
        let (file, name) = create_temp_in(&dir, OsStr::new("private.txt"), true).unwrap();
        // The mode must hold from the instant of creation — before any
        // write, sync, or chmod — or an opener in the gap keeps an fd no
        // later chmod can revoke.
        let mode = file.metadata().unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "temporary was created readable");
        assert!(tmp.path().join(&name).exists());
        // A replacement keeps permission bits and nothing else.
        let kept = replacement_permissions(std::fs::Permissions::from_mode(0o4755));
        assert_eq!(kept.mode() & 0o7777, 0o755);
    }

    /// The case-fold lookup's decisions, with the by-path resolution
    /// under the test's control so each race is deterministic. Only the
    /// replaced-file pin needs a real identity; the rest is path logic
    /// the fallback shares.
    mod vouched {
        use super::super::*;
        use std::ffi::OsStr;

        fn setup() -> (
            tempfile::TempDir,
            PathBuf,
            Dir,
            super::super::super::dir::Stat,
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let anchor = tmp.path().canonicalize().unwrap();
            std::fs::write(anchor.join("Cased"), "x").unwrap();
            let dir = Dir::open(&anchor, Hold::Search).unwrap();
            let spelled = dir.lstat(OsStr::new("Cased")).unwrap();
            (tmp, anchor, dir, spelled)
        }

        #[test]
        fn the_name_on_disk_is_vouched_for_when_it_is_the_same_file() {
            let (_tmp, anchor, dir, spelled) = setup();
            let named = vouched_name(&dir, &anchor, &spelled, || Ok(anchor.join("Cased"))).unwrap();
            assert_eq!(named, Some(OsString::from("Cased")));
        }

        #[cfg(dir_handles)]
        #[test]
        fn a_file_replaced_in_the_window_is_the_race_not_a_symlink() {
            let (_tmp, anchor, dir, spelled) = setup();
            let e = vouched_name(&dir, &anchor, &spelled, || {
                // An editor's atomic save: the replacement is a new
                // file while the old one still exists, then renamed over
                // it — same name, fresh inode. (Unlinking first would let
                // the filesystem hand the new file the old inode.)
                std::fs::write(anchor.join(".Cased.saving"), "y").unwrap();
                std::fs::rename(anchor.join(".Cased.saving"), anchor.join("Cased")).unwrap();
                Ok(anchor.join("Cased"))
            })
            .unwrap_err();
            assert!(e.to_string().contains("changed while being opened"), "{e}");
        }

        #[test]
        fn a_name_that_resolves_outside_the_anchor_is_the_race_too() {
            let (_tmp, anchor, dir, spelled) = setup();
            let elsewhere = tempfile::tempdir().unwrap();
            let e = vouched_name(&dir, &anchor, &spelled, || {
                Ok(elsewhere.path().canonicalize().unwrap().join("Cased"))
            })
            .unwrap_err();
            assert!(e.to_string().contains("changed while being opened"), "{e}");
        }

        #[test]
        fn a_destination_gone_by_now_leaves_the_spelled_name() {
            let (_tmp, anchor, dir, spelled) = setup();
            let gone = vouched_name(&dir, &anchor, &spelled, || {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            })
            .unwrap();
            assert_eq!(gone, None);
            // Resolved, then gone before the handle could look.
            let gone = vouched_name(&dir, &anchor, &spelled, || {
                std::fs::remove_file(anchor.join("Cased")).unwrap();
                Ok(anchor.join("Cased"))
            })
            .unwrap();
            assert_eq!(gone, None);
            // Any other failure to resolve is nobody's to swallow.
            let e = vouched_name(&dir, &anchor, &spelled, || {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            })
            .unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn only_the_answers_a_model_can_act_on_become_outcomes() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            Outcome::classify(&Error::from(ErrorKind::NotFound)),
            Some(Outcome::NotFound)
        );
        assert_eq!(
            Outcome::classify(&Error::from(ErrorKind::PermissionDenied)),
            Some(Outcome::PermissionDenied)
        );
        assert_eq!(
            Outcome::classify(&Error::from(ErrorKind::IsADirectory)),
            Some(Outcome::IsDirectory)
        );
        assert_eq!(
            Outcome::classify(&Error::from(ErrorKind::NotADirectory)),
            Some(Outcome::NotADirectory)
        );
        // A disk error is nobody's to react to: it stays a step error.
        assert_eq!(Outcome::classify(&Error::other("I/O error")), None);
        assert_eq!(
            Outcome::classify(&Error::from(ErrorKind::Interrupted)),
            None
        );
    }

    #[tokio::test]
    async fn a_missing_path_canonicalises_to_its_deepest_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let probed = canonicalize_missing(&root.join("no/such/file.txt"))
            .await
            .unwrap();
        assert_eq!(probed, root.join("no/such/file.txt"));
        // A file in the middle of the path ends the walk the same way.
        std::fs::write(root.join("plain"), "x").unwrap();
        let probed = canonicalize_missing(&root.join("plain/below.txt"))
            .await
            .unwrap();
        assert_eq!(probed, root.join("plain/below.txt"));
    }
}
