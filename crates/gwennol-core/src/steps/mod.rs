//! Native host step types.
//!
//! These are the only code in Gwennol that touches the filesystem, spawns
//! processes, or opens sockets. Every one of them:
//!
//! 1. resolves its params against the action's template context;
//! 2. validates them;
//! 3. asks the [`Operator`](crate::Operator) through [`crate::host::approve`]
//!    — after any kernel-side capability check, so a plugin that lacks the
//!    grant is refused before the operator is bothered;
//! 4. does the work, racing it against the invocation's cancel token.
//!
//! One deliberate exception to that ordering: `host_fs.read` opens the
//! file — read-only, side-effect free, non-blocking — *before* asking, so
//! the approval can be verified to name the very file the handle holds.
//!
//! They are published to plugins by the `host_fs`, `host_process` and
//! `host_http` manifests in `resources/` as `host_fs.read`, `host_fs.write`,
//! `host_fs.list`, `host_process.run`, `host_http.get` and `host_http.post`.
//! None is `freelyUsable`: a plugin must hold the matching `step_type:host_`
//! grant, which the kernel enforces at dispatch.

pub mod fs;
pub mod http;
pub mod process;

use std::future::Future;
use std::pin::Pin;

use gwead::kernel::host_api::PluginErrorPayload;
use gwead::kernel::{PluginExecution, StepError, StepOutput};
use gwead::serde_json::Value;
use gwead::tokio_util::sync::CancellationToken;

/// The boxed future a native step implementation returns.
pub(crate) type StepFuture<'a> =
    Pin<Box<dyn Future<Output = Result<StepOutput, StepError>> + Send + 'a>>;

/// A plugin-requested cap, clamped to the host's ceiling. Every byte- and
/// entry-cap site goes through here so the arithmetic is pinned once —
/// a raw `as usize` on an unclamped u64 is how a cap silently becomes
/// unbounded. Ceilings must fit in `usize` on every supported target: the
/// cast truncates a >usize ceiling on a 32-bit platform, which today's
/// 64 MiB values cannot reach.
pub(crate) fn capped(requested: u64, ceiling: u64) -> usize {
    requested.min(ceiling) as usize
}

/// The error code a host step fails with when its invocation was
/// cancelled: a `KernelError::PluginError` carrying it, so a consumer —
/// the agent loop — recognises cancellation as data and never by
/// reading error text or guessing from a token's state.
pub const CANCELLED_CODE: &str = "gwennol.cancelled";

/// The `params.phase` value on a cancellation that withdrew the step's
/// approval: the operator never answered, so nothing was done. Every
/// other cancellation lands somewhere in the work, where the step
/// cannot say how far it got.
pub const CANCELLED_AT_APPROVAL: &str = "approval";

/// The structured error a cancelled host step returns from inside its
/// work.
pub(crate) fn cancelled() -> StepError {
    cancelled_with(Value::Null)
}

/// The structured error a host step returns when its approval was
/// withdrawn: [`CANCELLED_CODE`] with `params.phase` set to
/// [`CANCELLED_AT_APPROVAL`].
pub(crate) fn withdrawn() -> StepError {
    cancelled_with(gwead::serde_json::json!({"phase": CANCELLED_AT_APPROVAL}))
}

fn cancelled_with(params: Value) -> StepError {
    StepError::Thrown(PluginErrorPayload {
        code: CANCELLED_CODE.to_string(),
        message: "cancelled".to_string(),
        params,
    })
}

/// Run `work` to completion, unless the invocation is cancelled first.
///
/// Biased toward cancellation: a checkpoint reached with the token
/// already cancelled — the operator cancelled while an earlier phase
/// ran — stops here by construction rather than by winning a coin toss
/// against work that happens to be ready on its first poll.
pub(crate) async fn or_cancelled<T>(
    cancel: &CancellationToken,
    work: impl Future<Output = T>,
) -> Result<T, StepError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(cancelled()),
        r = work => Ok(r),
    }
}

/// Resolve `{{templates}}` in the raw step params.
pub(crate) fn resolve(ex: &dyn PluginExecution, params: &Value) -> Value {
    let ctx = ex.resolution_context();
    ex.resolve_value_with(params, &ctx)
}

/// A required string param.
pub(crate) fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, StepError> {
    match params.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s),
        Some(Value::String(_)) => Err(StepError::Failed(format!(
            "param '{key}' must not be empty"
        ))),
        Some(other) => Err(StepError::Failed(format!(
            "param '{key}' must be a string, got {other}"
        ))),
        None => Err(StepError::Failed(format!("missing required param '{key}'"))),
    }
}

/// An optional non-negative integer param with a default.
pub(crate) fn u64_param(params: &Value, key: &str, default: u64) -> Result<u64, StepError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().ok_or_else(|| {
            StepError::Failed(format!("param '{key}' must be a non-negative integer"))
        }),
    }
}

/// An optional boolean param with a default.
pub(crate) fn bool_param(params: &Value, key: &str, default: bool) -> Result<bool, StepError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(StepError::Failed(format!(
            "param '{key}' must be a boolean"
        ))),
    }
}

/// Truncate a byte buffer to `max` bytes, on a UTF-8 boundary, reporting
/// whether anything was dropped.
pub(crate) fn lossy_capped(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let mut cut = max;
    while cut > 0 && !bytes.is_char_boundary_lossy(cut) {
        cut -= 1;
    }
    (String::from_utf8_lossy(&bytes[..cut]).into_owned(), true)
}

trait CharBoundary {
    fn is_char_boundary_lossy(&self, i: usize) -> bool;
}

impl CharBoundary for [u8] {
    fn is_char_boundary_lossy(&self, i: usize) -> bool {
        // Same rule as `str::is_char_boundary`: a continuation byte is
        // 0b10xxxxxx. Valid on arbitrary bytes; lossy decoding handles the
        // rest.
        i == self.len() || (self[i] as i8) >= -0x40
    }
}

#[cfg(test)]
mod tests {
    use super::capped;

    #[test]
    fn a_cap_beyond_the_ceiling_clamps_instead_of_wrapping_or_ballooning() {
        assert_eq!(capped(u64::MAX, 64 << 20), 64 << 20);
        assert_eq!(capped(0, 64 << 20), 0);
        assert_eq!(capped(5, 64 << 20), 5);
    }
}
