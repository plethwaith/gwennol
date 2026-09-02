//! `host_process.run`.

use std::process::Stdio;
use std::time::Duration;

use gwead::kernel::{PluginExecution, StepError};
use gwead::serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};

use super::{StepFuture, cancelled, capped, lossy_capped, resolve, u64_param};
use crate::host::{approval, approve, host, resolve_path};
use crate::operator::Access;

/// Default cap on captured stdout and stderr, each.
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1 << 20;
/// Hard ceiling on `max_output_bytes`: larger requests are clamped, so a
/// plugin cannot ask the host to buffer without bound.
pub const OUTPUT_BYTES_CEILING: u64 = 64 << 20;
/// Default wall-clock limit.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Hard ceiling on `timeout_ms`: larger requests are clamped, so the
/// wall-clock limit cannot be voided by asking for forever.
pub const TIMEOUT_MS_CEILING: u64 = 3_600_000;

/// Read everything `r` produces, keeping at most `cap + 1` bytes (one past
/// the cap marks truncation) and discarding the rest — the pipe is drained
/// to the end so the child never blocks on it, while the host's memory
/// stays bounded by the cap rather than by how fast the child can write.
async fn drain_capped(mut r: impl AsyncRead + Unpin, cap: usize) -> std::io::Result<Vec<u8>> {
    // Saturating so the arithmetic cannot wrap even for an unclamped
    // caller; step bodies clamp the cap before it reaches here.
    let keep = cap.saturating_add(1);
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return Ok(kept);
        }
        let room = keep.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
}

/// How long a timed-out or cancelled step waits for the killed child's
/// pipes to close before abandoning them.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// SIGKILL the whole process group the child leads, so a timed-out
/// `sh -c` leaves no orphans behind. Best-effort: a descendant that
/// detached into its own session (`setsid`) has left the group and is out
/// of reach.
#[cfg(unix)]
fn kill_group(pid: u32) {
    // The child was spawned as its group's leader, so its pid is the pgid.
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
}

/// Non-unix targets have no process-group notion for this to use; the
/// direct child still dies via `kill_on_drop`.
#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

/// `host_process.run`: `{argv, cwd?, stdin?, timeout_ms?, max_output_bytes?}`
/// → `{status, stdout, stderr, stdout_truncated, stderr_truncated}`.
///
/// Exceeding `timeout_ms` fails the step and kills the child's whole
/// process group (the child is spawned as its leader), not just the child —
/// so a timed-out `sh -c` leaves no orphans. Cancellation does the same.
/// Best-effort, not a guarantee: a descendant that detached into its own
/// session is out of the group's reach, and the wait for the killed
/// child's pipes is bounded rather than indefinite.
///
/// No shell is involved: `argv[0]` is the program. A tool that wants a
/// shell asks for one explicitly (`["sh", "-c", …]`) and the operator sees
/// that it did — including `stdin`, which for an interpreter is the real
/// payload.
///
/// stdin is fed and stdout/stderr drained concurrently, each capped at
/// `max_output_bytes` (excess is read and discarded), so a child that
/// floods a pipe or never reads its input cannot deadlock the step or
/// balloon the host.
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
        let timeout = Duration::from_millis(
            u64_param(&p, "timeout_ms", DEFAULT_TIMEOUT_MS)?.min(TIMEOUT_MS_CEILING),
        );
        let max = capped(
            u64_param(&p, "max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES)?,
            OUTPUT_BYTES_CEILING,
        );

        let cancel = ex.cancel_token();
        let ask = approval(
            &*ex,
            Access::Spawn {
                argv: argv.clone(),
                cwd: cwd.clone(),
                stdin: stdin.clone(),
            },
        );
        approve(ask).await?;

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
        #[cfg(unix)]
        cmd.process_group(0);
        if let Some(env) = host().process_env.resolve() {
            cmd.env_clear().envs(env);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| StepError::Failed(format!("spawn {:?}: {e}", argv[0])))?;
        let pid = child.id();
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("stdin piped");
            // Concurrent with the drains below, so a child that fills its
            // stdout before reading its stdin cannot deadlock the step. A
            // child that exits without reading is not an error.
            tokio::spawn(async move {
                let _ = pipe.write_all(input.as_bytes()).await;
                drop(pipe);
            });
        }
        let stdout_pipe = child.stdout.take().expect("stdout piped");
        let stderr_pipe = child.stderr.take().expect("stderr piped");
        let mut work = Box::pin(async move {
            let (out, err) = tokio::join!(
                drain_capped(stdout_pipe, max),
                drain_capped(stderr_pipe, max)
            );
            let status = child.wait().await?;
            std::io::Result::Ok((status, out?, err?))
        });

        // Biased, work first: a child that has already finished has
        // acted, and its result is the truth about that — reporting a
        // deadline or a cancellation that landed in the same instant
        // would tell the caller the work did not happen.
        let (status, stdout_bytes, stderr_bytes) = tokio::select! {
            biased;
            r = &mut work => r.map_err(|e| StepError::Failed(format!("wait {:?}: {e}", argv[0])))?,
            () = tokio::time::sleep(timeout) => {
                if let Some(pid) = pid { kill_group(pid); }
                // Reap, but bounded: a descendant that left the group
                // (setsid) can hold the pipes open — and off unix nothing
                // was group-killed at all. Past the grace, dropping `work`
                // kills the direct child via kill_on_drop and abandons the
                // pipes rather than hang the step.
                let _ = tokio::time::timeout(REAP_GRACE, &mut work).await;
                return Err(StepError::Failed(format!("{:?} exceeded timeout of {timeout:?}", argv[0])));
            }
            () = cancel.cancelled() => {
                if let Some(pid) = pid { kill_group(pid); }
                let _ = tokio::time::timeout(REAP_GRACE, &mut work).await;
                return Err(cancelled());
            }
        };
        let (stdout, stdout_truncated) = lossy_capped(&stdout_bytes, max);
        let (stderr, stderr_truncated) = lossy_capped(&stderr_bytes, max);
        Ok(json!({
            "status": status.code(),
            "stdout": stdout,
            "stderr": stderr,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        })
        .into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drain_capped_bounds_memory_but_reads_to_the_end() {
        let data = vec![7u8; 100_000];
        let kept = drain_capped(data.as_slice(), 10).await.unwrap();
        assert_eq!(kept.len(), 11, "one byte past the cap marks truncation");
        assert!(kept.iter().all(|b| *b == 7));
    }

    #[tokio::test]
    async fn drain_capped_survives_the_maximum_cap() {
        let data = vec![1u8; 10];
        let kept = drain_capped(data.as_slice(), usize::MAX).await.unwrap();
        assert_eq!(kept.len(), 10, "cap+1 must saturate, not wrap to zero");
    }
}
