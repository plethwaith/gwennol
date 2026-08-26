//! `host_process.run`.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use gwead::kernel::{PluginExecution, StepError, StepOutput};
use gwead::serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::{lossy_capped, resolve, u64_param};
use crate::host::{approval, approve, host, resolve_path};
use crate::operator::Access;

/// Default cap on captured stdout and stderr, each.
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1 << 20;
/// Default wall-clock limit.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<StepOutput, StepError>> + Send + 'a>>;

/// `host_process.run`: `{argv, cwd?, stdin?, timeout_ms?, max_output_bytes?}`
/// → `{status, stdout, stderr, stdout_truncated, stderr_truncated}`.
///
/// Exceeding `timeout_ms` fails the step; the child is killed on drop.
///
/// No shell is involved: `argv[0]` is the program. A tool that wants a
/// shell asks for one explicitly (`["sh", "-c", …]`) and the operator sees
/// that it did.
///
/// The child's environment is whatever [`ProcessEnv`](crate::ProcessEnv)
/// the frontend installed — by default an allow-list, so credentials the
/// agent was merely *launched with* are not authority every approved
/// command inherits. The plugin does not choose it: an environment the
/// operator was never shown is not something a manifest can declare.
pub fn process_run<'a>(
    ex: &'a mut (dyn PluginExecution + Send),
    params: &'a Value,
) -> StepFuture<'a> {
    Box::pin(async move {
        let p = resolve(ex, params);
        let argv: Vec<String> = match p.get("argv") {
            Some(Value::Array(items)) if !items.is_empty() => items
                .iter()
                .map(|v| {
                    v.as_str().map(str::to_owned).ok_or_else(|| {
                        StepError::Failed("param 'argv' must be an array of strings".into())
                    })
                })
                .collect::<Result<_, _>>()?,
            _ => {
                return Err(StepError::Failed(
                    "param 'argv' must be a non-empty array of strings".into(),
                ));
            }
        };
        let cwd = match p.get("cwd") {
            None | Some(Value::Null) => host().workspace_root.clone(),
            Some(Value::String(s)) => resolve_path(s),
            Some(_) => return Err(StepError::Failed("param 'cwd' must be a string".into())),
        };
        let stdin = match p.get("stdin") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(StepError::Failed("param 'stdin' must be a string".into())),
        };
        let timeout = Duration::from_millis(u64_param(&p, "timeout_ms", DEFAULT_TIMEOUT_MS)?);
        let max = u64_param(&p, "max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES)? as usize;

        let ask = approval(
            &*ex,
            Access::Spawn {
                argv: argv.clone(),
                cwd: cwd.clone(),
            },
        );
        approve(ask).await.map_err(StepError::Failed)?;

        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(&cwd)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = host().process_env.resolve() {
            cmd.env_clear().envs(env);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| StepError::Failed(format!("spawn {:?}: {e}", argv[0])))?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("stdin piped");
            // A child that exits without reading is not an error.
            let _ = pipe.write_all(input.as_bytes()).await;
            drop(pipe);
        }

        let cancel = ex.cancel_token();
        let output = tokio::select! {
            r = child.wait_with_output() => r.map_err(|e| StepError::Failed(format!("wait {:?}: {e}", argv[0])))?,
            () = tokio::time::sleep(timeout) => return Err(StepError::Failed(format!("{:?} exceeded timeout of {timeout:?}", argv[0]))),
            () = cancel.cancelled() => return Err(StepError::Failed("cancelled".into())),
        };
        let (stdout, stdout_truncated) = lossy_capped(&output.stdout, max);
        let (stderr, stderr_truncated) = lossy_capped(&output.stderr, max);
        Ok(json!({
            "status": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        })
        .into())
    })
}
