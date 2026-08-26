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
        let ask = approval(&*ex, Access::ReadFile(path.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        let cancel = ex.cancel_token();
        let read = async {
            let file = tokio::fs::File::open(&path).await?;
            let size = file.metadata().await?.len();
            let mut bytes = Vec::new();
            // One byte past the cap distinguishes "exactly the cap" from
            // "truncated" without reading the rest.
            file.take(max as u64 + 1).read_to_end(&mut bytes).await?;
            std::io::Result::Ok((size, bytes))
        };
        let (size, bytes) = or_cancelled(&cancel, read)
            .await?
            .map_err(|e| StepError::Failed(format!("read {}: {e}", path.display())))?;
        let (content, truncated) = lossy_capped(&bytes, max);
        Ok(json!({"content": content, "truncated": truncated, "size": size}).into())
    })
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
        let ask = approval(&*ex, Access::WriteFile(path.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        let cancel = ex.cancel_token();
        if create_dirs && let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| StepError::Failed(format!("mkdir {}: {e}", parent.display())))?;
        }
        or_cancelled(&cancel, write_via_rename(&path, &content))
            .await?
            .map_err(|e| StepError::Failed(format!("write {}: {e}", path.display())))?;
        Ok(json!({"bytes_written": content.len()}).into())
    })
}

/// `host_fs.list`: `{path, max_entries?}` →
/// `{entries: [{name, kind, size}], truncated}`.
///
/// One level only — recursion is a tool's decision to make, one approved
/// listing at a time — and at most `max_entries` entries come back, with
/// `truncated` saying whether the directory held more.
pub fn fs_list<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let path = resolve_path(str_param(&p, "path")?);
        let max = u64_param(&p, "max_entries", DEFAULT_LIST_MAX_ENTRIES)? as usize;
        let ask = approval(&*ex, Access::ListDir(path.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        let cancel = ex.cancel_token();
        let mut rd = tokio::fs::read_dir(&path)
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
