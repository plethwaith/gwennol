//! Dispatching back into the kernel from guest code.

use serde_json::Value;

use crate::stream::Stream;
use crate::sys;

/// What to dispatch into: a plugin by name (resolved along the calling
/// plugin's namespace ancestor chain) or an SPI role.
///
/// A plugin may always invoke its own actions; anything else requires a
/// matching `invoke:plugin:<name>` / `invoke:role:<name>` grant in the
/// calling plugin's manifest — the kernel enforces this, and a missing
/// grant surfaces as an `Err` here.
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    /// Another plugin, by manifest name.
    Plugin(&'a str),
    /// Whatever plugin fulfils an SPI role.
    Role(&'a str),
}

impl Target<'_> {
    fn spec(&self) -> Vec<u8> {
        let spec = match self {
            Target::Plugin(name) => serde_json::json!({ "plugin": name }),
            Target::Role(name) => serde_json::json!({ "role": name }),
        };
        serde_json::to_vec(&spec).expect("target spec serializes")
    }
}

/// Drain the single-slot result-or-error stash the invoke imports fill.
fn drain_call_stash() -> Option<Vec<u8>> {
    let size = sys::host_call_result_size();
    if size <= 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let copied = sys::host_call_result_read(&mut buf);
    if copied < 0 {
        return None;
    }
    buf.truncate(copied as usize);
    Some(buf)
}

fn stashed_error(what: &str, status: i32) -> String {
    match drain_call_stash() {
        Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        None => format!("{what} failed with status {status} and no error message"),
    }
}

/// Invoke an action and wait for its result.
///
/// The error string is the kernel's message: a permission denial, a
/// dispatch failure, or the callee's own error, already prefixed with
/// which plugin and action failed.
pub fn invoke(target: Target<'_>, action: &str, input: &Value) -> Result<Value, String> {
    let input_json =
        serde_json::to_vec(input).map_err(|e| format!("invoke input failed to serialize: {e}"))?;
    let status = sys::host_invoke(&target.spec(), action.as_bytes(), &input_json);
    if status == 1 {
        let bytes = drain_call_stash().unwrap_or_else(|| b"null".to_vec());
        serde_json::from_slice(&bytes).map_err(|e| format!("invoke result is not valid JSON: {e}"))
    } else {
        Err(stashed_error("io.invoke", status))
    }
}

/// Invoke a `dataflow: true` action and get the readable end of its
/// single `long_running` step's output.
///
/// The callee starts on a background task and keeps producing after
/// this call returns; the handle lands in the *calling invocation's*
/// stream table, so an entry point may either drain it or hand it back
/// as part of its own result for the embedder to drain — the handle
/// stays valid after the action returns when the embedder supplied the
/// stream table. A callee failure mid-stream surfaces as early
/// end-of-stream on the handle, not as an `Err` here.
pub fn invoke_streaming(target: Target<'_>, action: &str, input: &Value) -> Result<Stream, String> {
    let input_json = serde_json::to_vec(input)
        .map_err(|e| format!("invoke_streaming input failed to serialize: {e}"))?;
    let status = sys::host_invoke_streaming(&target.spec(), action.as_bytes(), &input_json);
    match Stream::from_handle(status) {
        Some(stream) => Ok(stream),
        None => Err(stashed_error("io.invoke_streaming", status)),
    }
}
