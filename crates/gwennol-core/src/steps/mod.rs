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
//! They are published to plugins by the `host_fs` manifest in `resources/`
//! as `host_fs.read`, `host_fs.write` and `host_fs.list`. None is
//! `freelyUsable`: a plugin must hold the matching `step_type:host_` grant,
//! which the kernel enforces at dispatch.

pub mod fs;

use gwead::kernel::{PluginExecution, StepError};
use gwead::serde_json::Value;

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
