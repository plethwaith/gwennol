//! The raw script-runtime ABI, wrapped just enough to be callable from
//! safe Rust.
//!
//! Every function here corresponds one-to-one to a host import from the
//! wasm module `"gwead1"` — Gwead's script-runtime ABI, version 1, as
//! pinned by its `STREAMS_ABI.md`. The wrappers translate between Rust
//! slices and the ABI's `(ptr, len)` pairs and nothing more: return
//! codes come back verbatim, and the typed layers above
//! ([`Stream`](crate::Stream), [`invoke`](crate::invoke)) give them
//! meaning.
//!
//! On non-wasm targets the same signatures exist but panic when called:
//! the crate has to *compile* host-side so lints, docs, and pure-logic
//! tests cover it, but there is no host to import from, and a call
//! reaching one of these outside a wasm guest is a bug in the caller.

/// Stream return code: successful end-of-stream on a readable.
pub const STREAM_EOF: i32 = -1;
/// Stream return code: handle not present in the registry.
pub const STREAM_INVALID_HANDLE: i32 = -2;
/// Stream return code: read on a writable, or write on a readable.
pub const STREAM_DIRECTION_MISMATCH: i32 = -3;
/// Stream return code: handle closed, or the paired consumer is gone.
pub const STREAM_CLOSED: i32 = -4;
/// Stream return code: the readable's source reported an I/O error.
pub const STREAM_IO_ERROR: i32 = -5;
/// Stream return code: the guest passed an out-of-bounds buffer.
pub const STREAM_OOB: i32 = -6;

#[cfg(target_arch = "wasm32")]
mod imp {
    // The module name is the ABI version handshake: `"gwead"` with the
    // ABI version appended. A kernel speaking a different ABI registers
    // a different module name and this module fails to instantiate,
    // which is the intended diagnostic. Bumping this constant IS the
    // ABI migration for every guest built on this crate.
    #[link(wasm_import_module = "gwead1")]
    unsafe extern "C" {
        fn host_set_result(ptr: i32, len: i32);
        fn host_set_error(ptr: i32, len: i32);
        fn host_log(level: i32, ptr: i32, len: i32);
        fn stream_read(handle: i32, buf_ptr: i32, buf_len: i32) -> i32;
        fn stream_write(handle: i32, buf_ptr: i32, buf_len: i32) -> i32;
        fn stream_close(handle: i32) -> i32;
        fn stream_output() -> i32;
        fn is_cancelled() -> i32;
        fn host_invoke(
            target_ptr: i32,
            target_len: i32,
            action_ptr: i32,
            action_len: i32,
            input_ptr: i32,
            input_len: i32,
        ) -> i32;
        fn host_invoke_streaming(
            target_ptr: i32,
            target_len: i32,
            action_ptr: i32,
            action_len: i32,
            input_ptr: i32,
            input_len: i32,
        ) -> i32;
        fn host_call_result_size() -> i32;
        fn host_call_result_read(buf_ptr: i32, max_len: i32) -> i32;
    }

    fn ptr_len(buf: &[u8]) -> (i32, i32) {
        // The ABI carries lengths as i32. A buffer past i32::MAX cannot
        // be described to the host at all — `as` would wrap it negative
        // and the host would answer with a misleading STREAM_OOB — so
        // trap with the real reason instead. Unreachable below a 2 GiB
        // wasm memory, which no default configuration allows.
        let len = i32::try_from(buf.len())
            .expect("buffer length exceeds i32::MAX — not expressible in the gwead1 ABI");
        (buf.as_ptr() as usize as i32, len)
    }

    pub fn set_result(bytes: &[u8]) {
        let (p, l) = ptr_len(bytes);
        unsafe { host_set_result(p, l) }
    }

    pub fn set_error(bytes: &[u8]) {
        let (p, l) = ptr_len(bytes);
        unsafe { host_set_error(p, l) }
    }

    pub fn log(level: i32, message: &[u8]) {
        let (p, l) = ptr_len(message);
        unsafe { host_log(level, p, l) }
    }

    pub fn read(handle: i32, buf: &mut [u8]) -> i32 {
        let ptr = buf.as_mut_ptr() as usize as i32;
        let len = buf.len() as i32;
        unsafe { stream_read(handle, ptr, len) }
    }

    pub fn write(handle: i32, buf: &[u8]) -> i32 {
        let (p, l) = ptr_len(buf);
        unsafe { stream_write(handle, p, l) }
    }

    pub fn close(handle: i32) -> i32 {
        unsafe { stream_close(handle) }
    }

    pub fn output() -> i32 {
        unsafe { stream_output() }
    }

    pub fn cancelled_flag() -> i32 {
        unsafe { is_cancelled() }
    }

    pub fn invoke(target: &[u8], action: &[u8], input: &[u8]) -> i32 {
        let (tp, tl) = ptr_len(target);
        let (ap, al) = ptr_len(action);
        let (ip, il) = ptr_len(input);
        unsafe { host_invoke(tp, tl, ap, al, ip, il) }
    }

    pub fn invoke_streaming(target: &[u8], action: &[u8], input: &[u8]) -> i32 {
        let (tp, tl) = ptr_len(target);
        let (ap, al) = ptr_len(action);
        let (ip, il) = ptr_len(input);
        unsafe { host_invoke_streaming(tp, tl, ap, al, ip, il) }
    }

    pub fn call_result_size() -> i32 {
        unsafe { host_call_result_size() }
    }

    pub fn call_result_read(buf: &mut [u8]) -> i32 {
        let ptr = buf.as_mut_ptr() as usize as i32;
        let len = buf.len() as i32;
        unsafe { host_call_result_read(ptr, len) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    //! Host-target stand-ins. Compiling is the point; calling is a bug.

    fn absent(name: &str) -> ! {
        panic!(
            "gwennol-guest::sys::{name} called outside a Gwead script-runtime \
             wasm module — this crate's runtime surface only works compiled \
             to wasm32 and executed by the kernel"
        )
    }

    pub fn set_result(_bytes: &[u8]) {
        absent("set_result")
    }
    pub fn set_error(_bytes: &[u8]) {
        absent("set_error")
    }
    pub fn log(_level: i32, _message: &[u8]) {
        absent("log")
    }
    pub fn read(_handle: i32, _buf: &mut [u8]) -> i32 {
        absent("read")
    }
    pub fn write(_handle: i32, _buf: &[u8]) -> i32 {
        absent("write")
    }
    pub fn close(_handle: i32) -> i32 {
        absent("close")
    }
    pub fn output() -> i32 {
        absent("output")
    }
    pub fn cancelled_flag() -> i32 {
        absent("cancelled_flag")
    }
    pub fn invoke(_target: &[u8], _action: &[u8], _input: &[u8]) -> i32 {
        absent("invoke")
    }
    pub fn invoke_streaming(_target: &[u8], _action: &[u8], _input: &[u8]) -> i32 {
        absent("invoke_streaming")
    }
    pub fn call_result_size() -> i32 {
        absent("call_result_size")
    }
    pub fn call_result_read(_buf: &mut [u8]) -> i32 {
        absent("call_result_read")
    }
}

/// Report the step's result: a UTF-8 JSON document the kernel parses
/// after `execute` returns 1. Later calls overwrite earlier ones.
pub fn host_set_result(bytes: &[u8]) {
    imp::set_result(bytes)
}

/// Report the step's error: a UTF-8 message the kernel reads after
/// `execute` returns 0. Later calls overwrite earlier ones.
pub fn host_set_error(bytes: &[u8]) {
    imp::set_error(bytes)
}

/// Emit a log line at the ABI's numeric level (debug 0, info 1, warn 2,
/// error 3; the host treats unknown values as info).
pub fn host_log(level: i32, message: &[u8]) {
    imp::log(level, message)
}

/// Read up to `buf.len()` bytes from a readable stream. Returns bytes
/// copied, [`STREAM_EOF`], or another negative `STREAM_*` code.
pub fn stream_read(handle: i32, buf: &mut [u8]) -> i32 {
    imp::read(handle, buf)
}

/// Write `buf` to a writable stream. Returns bytes committed (the whole
/// buffer on success) or a negative `STREAM_*` code. Blocks the guest
/// (via the host's async import) while the consumer applies
/// backpressure.
pub fn stream_write(handle: i32, buf: &[u8]) -> i32 {
    imp::write(handle, buf)
}

/// Close a handle. Idempotent; returns 0 on success or
/// [`STREAM_INVALID_HANDLE`].
pub fn stream_close(handle: i32) -> i32 {
    imp::close(handle)
}

/// The pre-provisioned writable output of a `long_running` step in a
/// `dataflow: true` action, or [`STREAM_INVALID_HANDLE`] when this step
/// is not one.
pub fn stream_output() -> i32 {
    imp::output()
}

/// 1 if the parent step's cancellation token has fired, else 0.
pub fn is_cancelled() -> i32 {
    imp::cancelled_flag()
}

/// Dispatch a synchronous action invocation. `target` is the UTF-8 JSON
/// target spec, `action` the UTF-8 action name, `input` the UTF-8 JSON
/// input. Returns 1 (result stashed) or another value (error stashed);
/// drain the stash with [`host_call_result_size`] /
/// [`host_call_result_read`] either way.
pub fn host_invoke(target: &[u8], action: &[u8], input: &[u8]) -> i32 {
    imp::invoke(target, action, input)
}

/// Dispatch a streaming action invocation (the callee must be a
/// `dataflow: true` action with exactly one `long_running` step whose
/// output flows to the caller). Returns a positive stream handle, or a
/// non-positive value with the error stashed for
/// [`host_call_result_read`].
pub fn host_invoke_streaming(target: &[u8], action: &[u8], input: &[u8]) -> i32 {
    imp::invoke_streaming(target, action, input)
}

/// Byte length of the pending invoke result-or-error stash (0 when
/// empty).
pub fn host_call_result_size() -> i32 {
    imp::call_result_size()
}

/// Copy the pending stash into `buf` and clear it. Returns bytes
/// copied, or -1 on a host-side failure.
pub fn host_call_result_read(buf: &mut [u8]) -> i32 {
    imp::call_result_read(buf)
}
