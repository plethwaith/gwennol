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
//! plugin probed — canonical up to its deepest existing ancestor, since a
//! missing file has no canonical path of its own — before the plugin
//! learns that nothing is there.

use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::path::{Path, PathBuf};

use gwead::kernel::{PluginExecution, StepError, StepOutput};
use gwead::serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{
    StepFuture, bool_param, capped, lossy_capped, or_cancelled, resolve, str_param, u64_param,
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
    /// is not a directory.
    NotADirectory,
    /// The agent's user may not do this.
    PermissionDenied,
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
        };
        json!({"outcome": self.name(), "message": message}).into()
    }
}

/// Turn an I/O error at `path` into either a data outcome or a step
/// error. `what` names the operation for the error message.
fn outcome_or_error(what: &str, path: &Path, e: std::io::Error) -> Result<StepOutput, StepError> {
    match Outcome::classify(&e) {
        Some(outcome) => Ok(outcome.result(path)),
        None => Err(StepError::Failed(format!("{what} {}: {e}", path.display()))),
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
            // path is approved canonical up to its deepest existing
            // ancestor, the closest thing a missing file has to a
            // canonical path.
            Err(e) => {
                let Some(outcome) = Outcome::classify(&e) else {
                    return Err(StepError::Failed(format!("read {}: {e}", path.display())));
                };
                let probed = canonicalize_missing(&path)
                    .await
                    .map_err(|e| StepError::Failed(format!("read {}: {e}", path.display())))?;
                let ask = approval(&*ex, Access::ReadFile(probed));
                approve(ask).await.map_err(StepError::Failed)?;
                return Ok(outcome.result(&path));
            }
        };
        // The operator judges the canonical path, so it must name the file
        // the handle holds — otherwise the path was swapped mid-open and
        // the step refuses rather than approve one file and read another.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let named = tokio::fs::metadata(&canonical)
                .await
                .map_err(|e| StepError::Failed(format!("read {}: {e}", canonical.display())))?;
            if (named.dev(), named.ino()) != (meta.dev(), meta.ino()) {
                return Err(StepError::Failed(format!(
                    "{} changed while being opened",
                    path.display()
                )));
            }
        }
        let ask = approval(&*ex, Access::ReadFile(canonical));
        approve(ask).await.map_err(StepError::Failed)?;
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

/// Resolve a path that need not exist: the deepest existing ancestor is
/// canonicalised — a symlinked directory resolves to where it really
/// leads — and the not-yet-existing remainder is appended as spelled.
/// This is the path a write approval names, and the path a miss is
/// approved under.
async fn canonicalize_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path;
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match tokio::fs::canonicalize(existing).await {
            Ok(mut canonical) => {
                for component in rest.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            // A component that exists but is not a directory ends the
            // walk the same way a missing one does: what lies below it
            // cannot be canonicalised, only spelled.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                rest.push(
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

/// Create an exclusive (`create_new`) temporary sibling of `dest`, named
/// with a random nonce — so a file or symlink planted at a guessed name
/// makes creation *fail* rather than redirect the write. With `restrict`
/// (used when replacing an existing file) the temporary is born 0600, so
/// its content is never readable beyond what the destination allowed —
/// there is no create-then-chmod gap in which another opener can grab a
/// readable fd.
async fn create_temp_sibling(
    dest: &Path,
    restrict: bool,
) -> std::io::Result<(tokio::fs::File, PathBuf)> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::other("path has no parent directory"))?;
    let name = dest
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if restrict {
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = restrict;
    for _ in 0..16 {
        let nonce = RandomState::new().build_hasher().finish();
        let tmp = parent.join(format!(
            ".{}.{nonce:016x}.gwennol-tmp",
            name.to_string_lossy()
        ));
        match opts.open(&tmp).await {
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
/// take the file (a missing parent without `create_dirs`, a directory in
/// the way, permission denied).
///
/// A symlink destination is refused before the operator is asked, and the
/// approved path is canonical up to its deepest existing ancestor — so the
/// operator judges where the bytes will actually land, not an alias.
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
        // A symlink destination is refused outright: writing through it
        // would land the bytes somewhere the approved path does not name,
        // and the rename would otherwise silently destroy the link.
        if let Ok(m) = tokio::fs::symlink_metadata(&path).await
            && m.file_type().is_symlink()
        {
            return Err(StepError::Failed(format!(
                "{} is a symlink; refusing to write through or replace it",
                path.display()
            )));
        }
        let canonical = or_cancelled(&cancel, canonicalize_missing(&path))
            .await?
            .map_err(|e| StepError::Failed(format!("write {}: {e}", path.display())))?;
        let ask = approval(&*ex, Access::WriteFile(canonical.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        if create_dirs
            && let Some(parent) = canonical.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return outcome_or_error("mkdir", parent, e);
        }
        // Whether this replaces an existing file decides both the
        // temporary's birth mode and the permissions it ends with.
        let dest_perms = match tokio::fs::metadata(&canonical).await {
            Ok(meta) if meta.is_dir() => return Ok(Outcome::IsDirectory.result(&path)),
            Ok(meta) => {
                #[cfg(unix)]
                let perms = {
                    use std::os::unix::fs::PermissionsExt as _;
                    // Permission bits only: fchmod after the write means
                    // the kernel will not strip setuid/setgid for us, and
                    // replacing an 04755 file must not mint a setuid
                    // binary owned by the agent's user.
                    std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777)
                };
                #[cfg(not(unix))]
                let perms = meta.permissions();
                Some(perms)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return outcome_or_error("write", &canonical, e),
        };
        // Creation and rename are quick local operations and are not raced
        // against cancellation: a dropped future cannot clean up after
        // itself, so cancellation is consulted *between* phases instead. A
        // cancelled write either leaves the destination untouched with the
        // temporary removed, or — once the rename begins — completes.
        let (file, tmp) = match create_temp_sibling(&canonical, dest_perms.is_some()).await {
            Ok(created) => created,
            // The parent is missing (and `create_dirs` was not asked
            // for), is not a directory, or is not writable: the
            // destination's answer, reported as such.
            Err(e) => return outcome_or_error("write", &path, e),
        };
        let filled = or_cancelled(&cancel, fill_temp(file, &content, dest_perms)).await;
        let cancelled_after_fill = cancel.is_cancelled();
        if !matches!(filled, Ok(Ok(()))) || cancelled_after_fill {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        filled?.map_err(|e| StepError::Failed(format!("write {}: {e}", canonical.display())))?;
        if cancelled_after_fill {
            return Err(StepError::Failed("cancelled".into()));
        }
        if let Err(e) = tokio::fs::rename(&tmp, &canonical).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return outcome_or_error("write", &canonical, e);
        }
        Ok(json!({"outcome": OUTCOME_OK, "bytes_written": content.len()}).into())
    })
}

/// `host_fs.list`: `{path, max_entries?}` → `{outcome: "ok", entries:
/// [{name, kind, size}], truncated}`, or `{outcome, message}` when there
/// is no directory to list.
///
/// The operator is shown, and the step lists, the canonical directory —
/// symlinks resolved. One level only — recursion is a tool's decision to
/// make, one approved listing at a time — and at most `max_entries`
/// entries come back, with `truncated` saying whether the directory held
/// more. A truncated listing is whatever the directory yielded first,
/// sorted for presentation — not the lexicographically first entries,
/// which would require reading the whole directory the cap exists to
/// avoid.
pub fn fs_list<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let path = resolve_path(str_param(&p, "path")?);
        let max = capped(
            u64_param(&p, "max_entries", DEFAULT_LIST_MAX_ENTRIES)?,
            LIST_ENTRIES_CEILING,
        );
        let cancel = ex.cancel_token();
        let canonical = match or_cancelled(&cancel, tokio::fs::canonicalize(&path)).await? {
            Ok(canonical) => canonical,
            // Nothing to list — approved as a probe of the deepest
            // existing ancestor plus the spelled remainder, then
            // answered as data.
            Err(e) => {
                let Some(outcome) = Outcome::classify(&e) else {
                    return Err(StepError::Failed(format!("list {}: {e}", path.display())));
                };
                let probed = canonicalize_missing(&path)
                    .await
                    .map_err(|e| StepError::Failed(format!("list {}: {e}", path.display())))?;
                let ask = approval(&*ex, Access::ListDir(probed));
                approve(ask).await.map_err(StepError::Failed)?;
                return Ok(outcome.result(&path));
            }
        };
        let ask = approval(&*ex, Access::ListDir(canonical.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        let mut rd = match tokio::fs::read_dir(&canonical).await {
            Ok(rd) => rd,
            Err(e) => return outcome_or_error("list", &path, e),
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        loop {
            let next = or_cancelled(&cancel, rd.next_entry())
                .await?
                .map_err(|e| StepError::Failed(format!("list {}: {e}", path.display())))?;
            let Some(entry) = next else { break };
            if entries.len() >= max {
                truncated = true;
                break;
            }
            let meta = entry.metadata().await.ok();
            let kind = match meta.as_ref().map(|m| m.file_type()) {
                Some(t) if t.is_dir() => "dir",
                Some(t) if t.is_symlink() => "symlink",
                Some(_) => "file",
                None => "unknown",
            };
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": kind,
                "size": meta.as_ref().map(|m| m.len()),
            }));
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"outcome": OUTCOME_OK, "entries": entries, "truncated": truncated}).into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn a_restricted_temporary_is_born_0600_not_chmodded_down_later() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("private.txt");
        let (file, tmp) = create_temp_sibling(&dest, true).await.unwrap();
        // The mode must hold from the instant of creation — before any
        // write, sync, or chmod — or an opener in the gap keeps an fd no
        // later chmod can revoke.
        let mode = file.metadata().await.unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "temporary was created readable");
        drop(file);
        let _ = std::fs::remove_file(tmp);
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
