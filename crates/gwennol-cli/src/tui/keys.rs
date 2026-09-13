//! Keys come through here so the loop never touches the terminal: a
//! real terminal's [`TerminalKeys`], or, in tests, a channel that
//! implements the same trait.

use crossterm::event::{Event, EventStream, KeyEvent, KeyEventKind};
use futures_util::StreamExt;

/// One thing the terminal told the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A key was pressed or is repeating.
    Key(KeyEvent),
    /// A bracketed paste landed whole.
    Paste(String),
    /// The terminal was resized.
    Resize,
    /// Reading the terminal's events failed. Not the source closing
    /// (`next` returns `None` for that, also once errors repeat —
    /// see `TerminalKeys`'s `MAX_CONSECUTIVE_READ_ERRORS`): the loop
    /// is told about a lone error, rather than treating it as a
    /// silent, unremarked exit.
    Errored(String),
}

/// Where the loop gets its keys. `None` once the source is closed.
#[async_trait::async_trait]
pub trait KeySource {
    /// The next input, or `None` once the source is closed.
    async fn next(&mut self) -> Option<Input>;
}

/// How many consecutive read errors `TerminalKeys::next` traces before
/// treating the source as closed. crossterm's `EventStream::poll_next`
/// returns `Poll::Ready(Some(Ok(..)))`, `Poll::Ready(Some(Err(..)))` or
/// `Poll::Pending`, and **never** `Poll::Ready(None)`
/// (`crossterm-0.29.0/src/event/stream.rs:101-135`): a broken terminal
/// fd surfaces only as a run of `Err`s, so this is the one place
/// `TerminalKeys::next`'s `None => return None` arm can still be
/// reached in production. A single error is traced and kept open (a
/// transient one should not end the session); a second consecutive one
/// is treated as the source closing, the same as `None`, rather than
/// tracing forever at whatever rate the fd re-errors.
const MAX_CONSECUTIVE_READ_ERRORS: u32 = 2;

/// Keys from a real terminal, over crossterm's `EventStream`. Release
/// events are dropped (most terminals never send them; some do, and a
/// release is not a new input), as are focus and mouse events.
pub struct TerminalKeys {
    stream: EventStream,
    /// Consecutive read errors since the last successful event; reset
    /// on any `Ok`. See [`MAX_CONSECUTIVE_READ_ERRORS`].
    errors: u32,
}

/// What one poll of the underlying stream means for `next`'s loop:
/// keep polling, or return this to `next`'s own caller.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// An ignored event (a key release, a focus or mouse event): poll
    /// again without returning.
    Continue,
    /// Return this from `next`.
    Return(Option<Input>),
}

impl TerminalKeys {
    /// A source reading the process's own terminal.
    pub fn new() -> Self {
        Self {
            stream: EventStream::new(),
            errors: 0,
        }
    }

    /// The mapping for one poll of the stream, pinned on its own (with
    /// a plain `Option<Result<..>>` standing in for the poll) so a test
    /// does not need a real, breakable `EventStream` to exercise it,
    /// including the error counter's reset on success.
    fn step(errors: &mut u32, event: Option<Result<Event, std::io::Error>>) -> Step {
        match event {
            Some(Ok(Event::Key(key))) => {
                *errors = 0;
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    Step::Return(Some(Input::Key(key)))
                } else {
                    Step::Continue
                }
            }
            Some(Ok(Event::Paste(text))) => {
                *errors = 0;
                Step::Return(Some(Input::Paste(text)))
            }
            Some(Ok(Event::Resize(_, _))) => {
                *errors = 0;
                Step::Return(Some(Input::Resize))
            }
            Some(Ok(Event::FocusGained | Event::FocusLost | Event::Mouse(_))) => {
                *errors = 0;
                Step::Continue
            }
            Some(Err(e)) => {
                *errors += 1;
                if *errors >= MAX_CONSECUTIVE_READ_ERRORS {
                    Step::Return(None)
                } else {
                    Step::Return(Some(Input::Errored(e.to_string())))
                }
            }
            None => Step::Return(None),
        }
    }
}

impl Default for TerminalKeys {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl KeySource for TerminalKeys {
    async fn next(&mut self) -> Option<Input> {
        loop {
            match Self::step(&mut self.errors, self.stream.next().await) {
                Step::Continue => {}
                Step::Return(input) => return input,
            }
        }
    }
}

/// A channel of keys, for tests: `recv` closes when every sender drops.
#[async_trait::async_trait]
impl KeySource for tokio::sync::mpsc::UnboundedReceiver<Input> {
    async fn next(&mut self) -> Option<Input> {
        self.recv().await
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    fn err() -> Option<Result<Event, std::io::Error>> {
        Some(Err(std::io::Error::other("boom")))
    }

    fn key_press() -> Option<Result<Event, std::io::Error>> {
        Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        ))))
    }

    /// Guards the mapping `TerminalKeys::next` cannot be unit-tested
    /// through directly (it reads a real `EventStream`): a first read
    /// error is traced and the source stays open; a second consecutive
    /// one closes it, the same as the source itself closing. Mutation:
    /// raise `MAX_CONSECUTIVE_READ_ERRORS` to 3 — the second call below
    /// returns `Step::Return(Some(Errored(..)))` instead of
    /// `Step::Return(None)`.
    #[test]
    fn a_second_consecutive_read_error_closes_the_source() {
        let mut errors = 0;
        assert_eq!(
            TerminalKeys::step(&mut errors, err()),
            Step::Return(Some(Input::Errored("boom".to_string())))
        );
        assert_eq!(TerminalKeys::step(&mut errors, err()), Step::Return(None));
    }

    /// A successful read in between resets the count: guards
    /// `*errors = 0` on `step`'s `Ok` arms. Mutation: drop the reset on
    /// the `Event::Key` arm — the third call below returns
    /// `Step::Return(None)` instead of tracing again.
    #[test]
    fn a_success_between_errors_resets_the_count() {
        let mut errors = 0;
        assert_eq!(
            TerminalKeys::step(&mut errors, err()),
            Step::Return(Some(Input::Errored("boom".to_string())))
        );
        assert_eq!(
            TerminalKeys::step(&mut errors, key_press()),
            Step::Return(Some(Input::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE
            ))))
        );
        assert_eq!(
            TerminalKeys::step(&mut errors, err()),
            Step::Return(Some(Input::Errored("boom".to_string()))),
            "a reset counter should trace again rather than closing"
        );
    }
}
