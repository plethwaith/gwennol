//! `host_fs.read`, `host_fs.write`, `host_fs.list`.

use std::hash::{BuildHasher as _, Hasher as _, RandomState};

use gwead::kernel::{PluginExecution, StepError};
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

/// `host_fs.read`: `{path, max_bytes?}` → `{content, truncated, size}`.
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
        let (file, meta, canonical) = or_cancelled(&cancel, open)
            .await?
            .map_err(|e| StepError::Failed(format!("read {}: {e}", path.display())))?;
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
        Ok(json!({"content": content, "truncated": truncated, "size": size}).into())
    })
}

/// Resolve `path` for a write approval: the deepest existing ancestor is
/// canonicalised — a symlinked directory resolves to where it really leads
/// — and the not-yet-existing remainder is appended as spelled.
async fn canonicalize_for_write(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
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
    dest: &std::path::Path,
    restrict: bool,
) -> std::io::Result<(tokio::fs::File, std::path::PathBuf)> {
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

/// `host_fs.write`: `{path, content, create_dirs?}` → `{bytes_written}`.
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
        let canonical = or_cancelled(&cancel, canonicalize_for_write(&path))
            .await?
            .map_err(|e| StepError::Failed(format!("write {}: {e}", path.display())))?;
        let ask = approval(&*ex, Access::WriteFile(canonical.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        if create_dirs && let Some(parent) = canonical.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| StepError::Failed(format!("mkdir {}: {e}", parent.display())))?;
        }
        // Whether this replaces an existing file decides both the
        // temporary's birth mode and the permissions it ends with.
        let dest_perms = match tokio::fs::metadata(&canonical).await {
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
            Err(e) => {
                return Err(StepError::Failed(format!(
                    "write {}: {e}",
                    canonical.display()
                )));
            }
        };
        // Creation and rename are quick local operations and are not raced
        // against cancellation: a dropped future cannot clean up after
        // itself, so cancellation is consulted *between* phases instead. A
        // cancelled write either leaves the destination untouched with the
        // temporary removed, or — once the rename begins — completes.
        let (file, tmp) = create_temp_sibling(&canonical, dest_perms.is_some())
            .await
            .map_err(|e| StepError::Failed(format!("write {}: {e}", canonical.display())))?;
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
            return Err(StepError::Failed(format!(
                "write {}: {e}",
                canonical.display()
            )));
        }
        Ok(json!({"bytes_written": content.len()}).into())
    })
}

/// `host_fs.list`: `{path, max_entries?}` →
/// `{entries: [{name, kind, size}], truncated}`.
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
        let canonical = or_cancelled(&cancel, tokio::fs::canonicalize(&path))
            .await?
            .map_err(|e| StepError::Failed(format!("list {}: {e}", path.display())))?;
        let ask = approval(&*ex, Access::ListDir(canonical.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        let mut rd = tokio::fs::read_dir(&canonical)
            .await
            .map_err(|e| StepError::Failed(format!("list {}: {e}", path.display())))?;
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
        Ok(json!({"entries": entries, "truncated": truncated}).into())
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
}
