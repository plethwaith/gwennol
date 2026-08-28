//! Typed byte-stream I/O over Gwead stream handles.

use crate::sys;

/// A stream-handle failure, decoded from the ABI's negative return
/// codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    /// The handle is not in this invocation's stream registry.
    InvalidHandle,
    /// Read on a writable handle, or write on a readable one.
    DirectionMismatch,
    /// The handle was closed, or (on write) the paired consumer is
    /// gone. For a producer this is the normal way to learn the reader
    /// stopped listening — wind down, don't retry.
    Closed,
    /// The readable's underlying source reported an I/O error.
    Io,
    /// A code this crate does not know; carries the raw value. Seeing
    /// one means the kernel speaks a newer ABI revision than this
    /// crate was built for.
    Other(i32),
}

impl StreamError {
    fn from_code(code: i32) -> StreamError {
        match code {
            sys::STREAM_INVALID_HANDLE => StreamError::InvalidHandle,
            sys::STREAM_DIRECTION_MISMATCH => StreamError::DirectionMismatch,
            sys::STREAM_CLOSED => StreamError::Closed,
            sys::STREAM_IO_ERROR => StreamError::Io,
            // STREAM_OOB can't arise from these wrappers — the buffer
            // is a real slice in our own linear memory — so it lands in
            // Other alongside genuinely unknown codes.
            other => StreamError::Other(other),
        }
    }
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::InvalidHandle => write!(f, "invalid stream handle"),
            StreamError::DirectionMismatch => write!(f, "stream direction mismatch"),
            StreamError::Closed => write!(f, "stream closed"),
            StreamError::Io => write!(f, "stream source I/O error"),
            StreamError::Other(code) => write!(f, "unknown stream error code {code}"),
        }
    }
}

/// One Gwead stream handle: an index into the current invocation's
/// stream registry, readable or writable (the registry knows which;
/// calling the wrong direction returns
/// [`StreamError::DirectionMismatch`]).
///
/// Dropping a `Stream` does **not** close the handle. That is
/// deliberate: handles regularly outlive the guest's interest in them —
/// an entry point that obtains a handle from
/// [`invoke_streaming`](crate::invoke_streaming) and returns it as part
/// of its result must leave it open for whoever reads the result. Call
/// [`Stream::close`] when the guest itself is the endpoint and is done;
/// otherwise the kernel's post-invocation drain cleans up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream {
    handle: i32,
}

impl Stream {
    /// Wrap a handle received as data — a prior step's result, say.
    /// Returns `None` for values that cannot be a handle (handles are
    /// positive by the ABI's construction).
    pub fn from_handle(handle: i32) -> Option<Stream> {
        (handle > 0).then_some(Stream { handle })
    }

    /// The pre-provisioned writable output of this step, when the step
    /// is `long_running` in a `dataflow: true` action; `None` for every
    /// other kind of step.
    pub fn output() -> Option<Stream> {
        Stream::from_handle(sys::stream_output())
    }

    /// The raw handle, for carrying in a result.
    pub fn handle(&self) -> i32 {
        self.handle
    }

    /// Read into `buf`, blocking (via the host) until bytes arrive.
    /// Returns the number of bytes read; `Ok(0)` means end-of-stream
    /// when `buf` is non-empty. Pass a non-empty buffer — a zero-length
    /// read also returns 0 and is indistinguishable from EOF.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, StreamError> {
        debug_assert!(!buf.is_empty(), "zero-length reads are ambiguous with EOF");
        match sys::stream_read(self.handle, buf) {
            n if n >= 0 => Ok(n as usize),
            sys::STREAM_EOF => Ok(0),
            code => Err(StreamError::from_code(code)),
        }
    }

    /// Write all of `buf`, blocking (via the host) while the consumer
    /// applies backpressure.
    pub fn write_all(&self, buf: &[u8]) -> Result<(), StreamError> {
        // The ABI commits the whole buffer per successful call; the
        // loop is defensive against a future partial-commit revision.
        let mut rest = buf;
        while !rest.is_empty() {
            match sys::stream_write(self.handle, rest) {
                n if n > 0 => rest = &rest[(n as usize).min(rest.len())..],
                0 => return Err(StreamError::Other(0)),
                code => return Err(StreamError::from_code(code)),
            }
        }
        Ok(())
    }

    /// Close the handle. Idempotent; closing early is how a consumer
    /// tells a producer to stop, and how a producer signals EOF before
    /// its step returns.
    pub fn close(&self) {
        let _ = sys::stream_close(self.handle);
    }
}
