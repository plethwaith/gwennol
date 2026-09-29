//! The record files a run can keep: the transcript (the conversation as
//! the provider saw it) and the trace (the lines a print run writes to
//! stderr, or a session shows in its pane). This module is the one place
//! either is opened, so print mode and a session agree on how a file is
//! created, what a failed write says, and what it does to the exit
//! status: a file this module creates is readable and writable by its
//! owner alone; a write that fails is said once; and a run that would
//! exit 0 exits 2 instead ([`settle`]).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;

use crate::{EXIT_USAGE, Fatal};

/// Open `path` for writing, truncating it. A file this creates is
/// readable and writable by its owner alone (on Unix; elsewhere the
/// platform's default): the transcript holds every tool result the
/// model saw and the trace holds commands and paths, a record of a
/// person's session. An existing file keeps its permissions, as a
/// shell redirection leaves them.
pub fn create(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// The whole chat input, pretty-printed: what the provider was handed
/// on the last round plus its answer, so the file is a request someone
/// can read or replay, not just the messages.
pub fn write_transcript(path: &Path, chat_input: &Value) -> Result<(), Fatal> {
    let text = serde_json::to_string_pretty(chat_input).expect("a Value serialises");
    let fail = |e: std::io::Error| Fatal(format!("transcript {}: {e}", path.display()));
    create(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(fail)
}

/// A trace file: one line per [`TraceFile::line`], each written with a
/// single unbuffered `write_all`, so nothing a crash would lose is held
/// in a buffer. The first failed write closes it.
pub struct TraceFile {
    path: PathBuf,
    out: Option<Box<dyn Write + Send>>,
    failed: bool,
}

impl TraceFile {
    /// Create (or truncate) the file at `path`.
    pub fn create(path: &Path) -> Result<TraceFile, Fatal> {
        let file = create(path).map_err(|e| Fatal(format!("trace {}: {e}", path.display())))?;
        Ok(TraceFile {
            path: path.to_path_buf(),
            out: Some(Box::new(file)),
            failed: false,
        })
    }

    /// A trace that writes to `writer`, for tests.
    #[cfg(test)]
    pub(crate) fn from_writer(path: &Path, writer: Box<dyn Write + Send>) -> TraceFile {
        TraceFile {
            path: path.to_path_buf(),
            out: Some(writer),
            failed: false,
        }
    }

    /// Write `text` and a newline. On the first error the file is
    /// closed and the message `trace <path>: <error>` is returned, once;
    /// after that nothing is written and this returns `None`.
    pub fn line(&mut self, text: &str) -> Option<String> {
        let out = self.out.as_mut()?;
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(b'\n');
        match out.write_all(&bytes) {
            Ok(()) => None,
            Err(e) => {
                self.out = None;
                self.failed = true;
                Some(format!("trace {}: {e}", self.path.display()))
            }
        }
    }
}

impl TraceFile {
    /// Whether a write has failed, which closed the file.
    pub fn failed(&self) -> bool {
        self.failed
    }
}

/// A trace shared by the print run and its operator.
pub type SharedTrace = Arc<Mutex<TraceFile>>;

/// Print `line` on stderr and write it to `trace` when there is one. A
/// write that fails is said on stderr, `gwennol: `-prefixed, and is not
/// itself traced.
pub fn say(trace: Option<&SharedTrace>, line: &str) {
    eprintln!("{line}");
    if let Some(trace) = trace {
        let failure = trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .line(line);
        if let Some(message) = failure {
            eprintln!("gwennol: {message}");
        }
    }
}

/// The exit status once record files are accounted for: a run that
/// would exit 0 exits 2 when a record could not be written; a 1 or a
/// 130 says more about the run than a file does and is kept.
pub fn settle(code: ExitCode, record_failed: bool) -> ExitCode {
    if code == ExitCode::SUCCESS && record_failed {
        ExitCode::from(EXIT_USAGE)
    } else {
        code
    }
}

/// Writers for the tests that need a capturing or a failing trace.
#[cfg(test)]
pub(crate) mod testing {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// A writer that keeps what it is given.
    #[derive(Clone, Default)]
    pub(crate) struct Capture(pub Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose every write fails, counting the attempts.
    #[derive(Clone, Default)]
    pub(crate) struct Failing(pub Arc<Mutex<usize>>);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            *self.0.lock().unwrap() += 1;
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Capture, Failing};
    use super::*;

    /// `create` opens a new file owner-only and truncates an existing
    /// one without touching its mode. Mutations: drop `.mode(0o600)`
    /// (0644 under umask 022); `set_permissions(0o600)` after the open
    /// (the pre-made 0644 file becomes 0600).
    #[cfg(unix)]
    #[test]
    fn a_record_file_is_created_owner_only_and_an_existing_one_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh");
        create(&fresh).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a new file is owner-only");

        let existing = dir.path().join("existing");
        std::fs::write(&existing, "old contents").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
        create(&existing).unwrap();
        let meta = std::fs::metadata(&existing).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o644, "mode kept");
        assert_eq!(meta.len(), 0, "and truncated");
    }

    /// The first failed write is reported once and closes the file.
    /// Mutation: keep `out` after a failure (the second call writes
    /// again and returns `Some` again).
    #[test]
    fn a_trace_reports_its_first_failed_write_once_and_then_writes_nothing() {
        let failing = Failing::default();
        let mut trace = TraceFile::from_writer(Path::new("t.log"), Box::new(failing.clone()));
        let first = trace.line("one").expect("the first failure is reported");
        assert!(first.starts_with("trace t.log: "), "{first}");
        assert_eq!(trace.line("two"), None);
        assert_eq!(*failing.0.lock().unwrap(), 1, "no write after the failure");
    }

    /// Mutation: return 2 whenever `record_failed`.
    #[test]
    fn settle_turns_only_a_success_into_a_usage_error() {
        let code = |n: u8| ExitCode::from(n);
        assert_eq!(settle(ExitCode::SUCCESS, true), code(2));
        assert_eq!(settle(ExitCode::SUCCESS, false), ExitCode::SUCCESS);
        assert_eq!(settle(code(1), true), code(1));
        assert_eq!(settle(code(130), true), code(130));
    }

    /// A line is its text and one newline, whatever the text holds.
    /// Mutation: drop the newline.
    #[test]
    fn a_trace_line_ends_with_one_newline() {
        let capture = Capture::default();
        let mut trace = TraceFile::from_writer(Path::new("t.log"), Box::new(capture.clone()));
        assert_eq!(trace.line("a\n    b"), None);
        assert_eq!(capture.0.lock().unwrap().as_slice(), b"a\n    b\n");
    }
}
