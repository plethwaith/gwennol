//! `host_fs.read`, `host_fs.write`, `host_fs.list`.

use std::sync::atomic::{AtomicU64, Ordering};

use gwead::kernel::{PluginExecution, StepError};
use gwead::serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{StepFuture, bool_param, lossy_capped, or_cancelled, resolve, str_param, u64_param};
use crate::host::{approval, approve, resolve_path};
use crate::operator::Access;

/// Default cap on bytes returned by `fs_read`.
pub const DEFAULT_READ_MAX_BYTES: u64 = 1 << 20;
/// Default cap on entries returned by `fs_list`.
pub const DEFAULT_LIST_MAX_ENTRIES: u64 = 1000;

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
        let max = u64_param(&p, "max_bytes", DEFAULT_READ_MAX_BYTES)? as usize;
        let cancel = ex.cancel_token();
        // Opened before the approval so the approval can describe the very
        // file this handle holds; opening for read has no side effects.
        let open = async {
            let file = tokio::fs::File::open(&path).await?;
            let meta = file.metadata().await?;
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
            // "truncated" without reading the rest.
            file.take(max as u64 + 1).read_to_end(&mut bytes).await?;
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

/// Distinguishes concurrent temporary files within this process; the
/// process id distinguishes across processes.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `content` to a temporary sibling of `path`, then rename it over
/// `path` — so a crash or a full disk mid-write cannot leave `path`
/// truncated. An existing file's permissions survive the replacement.
async fn write_via_rename(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::other("path has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?;
    let tmp = parent.join(format!(
        ".{}.{}.{}.gwennol-tmp",
        name.to_string_lossy(),
        std::process::id(),
        WRITE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    let write = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(content.as_bytes()).await?;
        // Flushed to disk before the rename makes it the file's content.
        file.sync_all().await?;
        drop(file);
        if let Ok(meta) = tokio::fs::metadata(path).await {
            tokio::fs::set_permissions(&tmp, meta.permissions()).await?;
        }
        tokio::fs::rename(&tmp, path).await
    };
    let result = write.await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
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
        or_cancelled(&cancel, write_via_rename(&canonical, &content))
            .await?
            .map_err(|e| StepError::Failed(format!("write {}: {e}", canonical.display())))?;
        Ok(json!({"bytes_written": content.len()}).into())
    })
}

/// `host_fs.list`: `{path, max_entries?}` →
/// `{entries: [{name, kind, size}], truncated}`.
///
/// The operator is shown, and the step lists, the canonical directory —
/// symlinks resolved. One level only — recursion is a tool's decision to make, one approved
/// listing at a time — and at most `max_entries` entries come back, with
/// `truncated` saying whether the directory held more.
pub fn fs_list<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let path = resolve_path(str_param(&p, "path")?);
        let max = u64_param(&p, "max_entries", DEFAULT_LIST_MAX_ENTRIES)? as usize;
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
