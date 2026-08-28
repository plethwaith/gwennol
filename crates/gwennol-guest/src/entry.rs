//! The `alloc`/`execute` exports and the entry-point table behind them.
//!
//! Gwead's `script` step instantiates the registered wasm module fresh
//! per step, calls `alloc` twice (once for the step's `source` string,
//! once for the resolution-context JSON), writes both into linear
//! memory, and calls `execute`. This module supplies those exports via
//! [`entrypoints!`](crate::entrypoints): the `source` string selects a
//! Rust function from the table the macro froze, which is the whole
//! sense in which this "interpreter" interprets.

use serde_json::Value;

use crate::args::Args;
use crate::sys;

/// The shape of a guest entry point: resolution context in, JSON result
/// (or failure message) out.
pub type EntryFn = fn(Args) -> Result<Value, String>;

/// Resolve `source` against the entry table and run the match.
///
/// Pure with respect to the ABI — no host calls — so it is unit-tested
/// on the host target. `source` is trimmed first: manifests written by
/// hand pick up stray whitespace, and an entry name is an identifier,
/// never whitespace-significant.
///
/// On success the returned bytes are the serialized JSON result the
/// caller reports via `host_set_result`.
pub fn dispatch(
    source: &str,
    args_json: &[u8],
    table: &[(&str, EntryFn)],
) -> Result<Vec<u8>, String> {
    let name = source.trim();
    let args: Value = serde_json::from_slice(args_json)
        .map_err(|e| format!("guest args are not valid JSON: {e}"))?;
    let Some((_, entry)) = table.iter().find(|(n, _)| *n == name) else {
        let known: Vec<&str> = table.iter().map(|(n, _)| *n).collect();
        return Err(format!(
            "no entry point named '{name}' in this guest module; \
             known entry points: {}",
            known.join(", ")
        ));
    };
    let value = entry(Args::new(args))?;
    serde_json::to_vec(&value).map_err(|e| format!("guest result failed to serialize: {e}"))
}

/// Implementation behind the `alloc` export `entrypoints!` generates.
#[doc(hidden)]
pub fn __alloc_impl(len: i32) -> i32 {
    let Ok(size) = usize::try_from(len) else {
        // The host never asks for a negative allocation; a caller that
        // does has broken the ABI, and trapping is the honest answer —
        // returning 0 would invite a write over offset 0.
        panic!("alloc called with a negative length ({len})");
    };
    if size == 0 {
        // The host writes zero bytes through a zero-length allocation;
        // any offset satisfies that, and 0 avoids allocator UB on a
        // zero-size layout.
        return 0;
    }
    // Layout::array::<u8> only fails past isize::MAX, unreachable from
    // an i32 length.
    let layout = std::alloc::Layout::array::<u8>(size).expect("i32 length fits a layout");
    // SAFETY: `layout` has non-zero size (checked above).
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        // Deterministic trap, which the kernel classifies as a failed
        // step, rather than handing the host offset 0 to overwrite.
        std::alloc::handle_alloc_error(layout);
    }
    // The allocation is deliberately leaked: the instance lives for one
    // `execute` call and the host needs the bytes valid throughout it.
    ptr as usize as i32
}

/// Validate one host-supplied `(ptr, len)` pair down to a raw range.
///
/// Pure, so the guards are unit-tested on the host target. Returns the
/// byte count (possibly 0 — the pointer is unused then) or an error the
/// caller reports; a valid non-empty range has a positive pointer.
fn guest_range(ptr: i32, len: i32) -> Result<usize, String> {
    let Ok(size) = usize::try_from(len) else {
        return Err(format!("host passed a negative buffer length ({len})"));
    };
    if size > 0 && ptr <= 0 {
        return Err(format!("host passed a non-positive buffer pointer ({ptr})"));
    }
    Ok(size)
}

/// Implementation behind the `execute` export `entrypoints!` generates:
/// read the two host-written buffers, dispatch, report through
/// `host_set_result`/`host_set_error`.
///
/// # Safety
///
/// Each `(ptr, len)` pair must denote `len` readable bytes in this
/// module's linear memory — concretely, a pointer this module's own
/// `alloc` returned for at least `len` bytes, into which the host wrote
/// exactly `len` bytes, still live for the whole call. The kernel's
/// `script` step upholds this; nothing else should call this function.
#[doc(hidden)]
pub unsafe fn __execute_impl(
    source_ptr: i32,
    source_len: i32,
    args_ptr: i32,
    args_len: i32,
    table: &[(&str, EntryFn)],
) -> i32 {
    let outcome = (|| {
        let source_size = guest_range(source_ptr, source_len)?;
        let args_size = guest_range(args_ptr, args_len)?;
        // SAFETY: ranges validated above; the pointed-to bytes are this
        // function's documented precondition, and the slices do not
        // outlive the call.
        let source_bytes: &[u8] = if source_size == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(source_ptr as usize as *const u8, source_size) }
        };
        let args_bytes: &[u8] = if args_size == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(args_ptr as usize as *const u8, args_size) }
        };
        let source = std::str::from_utf8(source_bytes)
            .map_err(|e| format!("script source is not UTF-8: {e}"))?;
        dispatch(source, args_bytes, table)
    })();

    match outcome {
        Ok(result_json) => {
            sys::host_set_result(&result_json);
            1
        }
        Err(message) => {
            sys::host_set_error(message.as_bytes());
            0
        }
    }
}

/// Declare the guest module's entry points and generate the wasm
/// exports Gwead's `script` step calls.
///
/// ```ignore
/// entrypoints! {
///     "chat" => chat,
///     "relay_sse" => relay_sse,
/// }
/// ```
///
/// Each name is what a manifest step's `source` field selects; each
/// value is a `fn(Args) -> Result<serde_json::Value, String>`. Use the
/// macro exactly once per guest crate — it defines the module-level
/// `alloc` and `execute` exports.
#[macro_export]
macro_rules! entrypoints {
    ( $( $name:literal => $func:expr ),+ $(,)? ) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn alloc(len: i32) -> i32 {
            $crate::__alloc_impl(len)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn execute(
            source_ptr: i32,
            source_len: i32,
            args_ptr: i32,
            args_len: i32,
        ) -> i32 {
            const TABLE: &[(&str, $crate::EntryFn)] = &[$(($name, $func)),+];
            // SAFETY: the caller is the kernel's `script` step, which
            // wrote both buffers through this module's `alloc` — the
            // exact precondition `__execute_impl` documents.
            unsafe { $crate::__execute_impl(source_ptr, source_len, args_ptr, args_len, TABLE) }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn greet(args: Args) -> Result<Value, String> {
        let who = args
            .field("who")
            .and_then(Value::as_str)
            .ok_or("missing 'who'")?;
        Ok(json!({ "greeting": format!("hello, {who}") }))
    }

    fn fails(_args: Args) -> Result<Value, String> {
        Err("deliberate".into())
    }

    const TABLE: &[(&str, EntryFn)] = &[("greet", greet), ("fails", fails)];

    #[test]
    fn dispatch_selects_by_trimmed_source() {
        let out = dispatch("  greet\n", br#"{"who": "gwennol"}"#, TABLE).expect("dispatches");
        let v: Value = serde_json::from_slice(&out).expect("result is JSON");
        assert_eq!(v, json!({"greeting": "hello, gwennol"}));
    }

    #[test]
    fn dispatch_names_the_known_entries_on_a_miss() {
        let err = dispatch("nope", b"{}", TABLE).expect_err("unknown entry fails");
        assert!(err.contains("'nope'"), "names the miss: {err}");
        assert!(
            err.contains("greet") && err.contains("fails"),
            "lists what would have worked: {err}"
        );
    }

    #[test]
    fn dispatch_propagates_entry_failure() {
        let err = dispatch("fails", b"{}", TABLE).expect_err("entry error propagates");
        assert_eq!(err, "deliberate");
    }

    #[test]
    fn dispatch_rejects_malformed_args() {
        let err = dispatch("greet", b"not json", TABLE).expect_err("bad args fail");
        assert!(err.contains("not valid JSON"), "{err}");
    }

    #[test]
    fn guest_range_accepts_what_the_host_protocol_produces() {
        assert_eq!(guest_range(32, 20), Ok(20));
        assert_eq!(guest_range(0, 0), Ok(0), "empty buffer, pointer unused");
        assert_eq!(guest_range(-1, 0), Ok(0), "empty buffer, pointer ignored");
    }

    #[test]
    fn guest_range_refuses_protocol_violations() {
        let err = guest_range(32, -1).expect_err("negative length");
        assert!(err.contains("negative buffer length"), "{err}");
        let err = guest_range(0, 4).expect_err("null pointer with bytes");
        assert!(err.contains("non-positive buffer pointer"), "{err}");
        let err = guest_range(-8, 4).expect_err("negative pointer with bytes");
        assert!(err.contains("non-positive buffer pointer"), "{err}");
    }
}
