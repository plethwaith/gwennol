//! `host_fs.read`, `host_fs.write`, `host_fs.list`.

use std::future::Future;
use std::pin::Pin;

use gwead::kernel::{PluginExecution, StepError, StepOutput};
use gwead::serde_json::{Value, json};

use super::{bool_param, lossy_capped, resolve, str_param, u64_param};
use crate::host::{approval, approve, resolve_path};
use crate::operator::Access;

/// Default cap on bytes returned by `fs_read`.
pub const DEFAULT_READ_MAX_BYTES: u64 = 1 << 20;

type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<StepOutput, StepError>> + Send + 'a>>;

/// `host_fs.read`: `{path, max_bytes?}` → `{content, truncated, size}`.
pub fn fs_read<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let path = resolve_path(str_param(&p, "path")?);
        let max = u64_param(&p, "max_bytes", DEFAULT_READ_MAX_BYTES)? as usize;
        let ask = approval(&*ex, Access::ReadFile(path.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        let cancel = ex.cancel_token();
        let bytes = tokio::select! {
            r = tokio::fs::read(&path) => r.map_err(|e| StepError::Failed(format!("read {}: {e}", path.display())))?,
            () = cancel.cancelled() => return Err(StepError::Failed("cancelled".into())),
        };
        let size = bytes.len();
        let (content, truncated) = lossy_capped(&bytes, max);
        Ok(json!({"content": content, "truncated": truncated, "size": size}).into())
    })
}

/// `host_fs.write`: `{path, content, create_dirs?}` → `{bytes_written}`.
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
        tokio::select! {
            r = tokio::fs::write(&path, content.as_bytes()) => r.map_err(|e| StepError::Failed(format!("write {}: {e}", path.display())))?,
            () = cancel.cancelled() => return Err(StepError::Failed("cancelled".into())),
        }
        Ok(json!({"bytes_written": content.len()}).into())
    })
}

/// `host_fs.list`: `{path}` → `{entries: [{name, kind, size}]}`.
///
/// One level only — recursion is a tool's decision to make, one approved
/// listing at a time.
pub fn fs_list<'a>(ex: &'a mut (dyn PluginExecution + Send), params: &'a Value) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let path = resolve_path(str_param(&p, "path")?);
        let ask = approval(&*ex, Access::ListDir(path.clone()));
        approve(ask).await.map_err(StepError::Failed)?;
        let cancel = ex.cancel_token();
        let mut rd = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| StepError::Failed(format!("list {}: {e}", path.display())))?;
        let mut entries = Vec::new();
        loop {
            let next = tokio::select! {
                r = rd.next_entry() => r.map_err(|e| StepError::Failed(format!("list {}: {e}", path.display())))?,
                () = cancel.cancelled() => return Err(StepError::Failed("cancelled".into())),
            };
            let Some(entry) = next else { break };
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
        Ok(json!({"entries": entries}).into())
    })
}
