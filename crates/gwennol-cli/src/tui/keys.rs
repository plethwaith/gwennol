//! Keys come through here so the loop never touches the terminal: a
//! real terminal's [`TerminalKeys`], `TerminalKeys` driven by a
//! scripted stream in unit tests, or a channel that implements the
//! same trait.

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
    /// (`next` returns `None` for that, also once errors repeat — see
    /// `MAX_CONSECUTIVE_READ_ERRORS`): the loop is told about a lone
    /// error, rather than treating it as a silent, unremarked exit.
    Errored(String),
}

/// Where the loop gets its keys. `None` once the source is closed.
#[async_trait::async_trait]
pub trait KeySource {
    /// The next input, or `None` once the source is closed.
    async fn next(&mut self) -> Option<Input>;
}

/// The number of consecutive read errors at which
/// `TerminalKeys::next` reports the source closed: the first is traced
/// as `Input::Errored` and the source stays open (a transient error
/// should not end the session), the second returns `None` instead of
/// being traced, rather than tracing forever at whatever rate the fd
/// re-errors. Bounding the run is the only way a dead terminal ends a
/// session at all: crossterm's `EventStream::poll_next` returns
/// `Poll::Ready(Some(Ok(..)))`, `Poll::Ready(Some(Err(..)))` or
/// `Poll::Pending` and **never** `Poll::Ready(None)`
/// (`crossterm-0.29.0/src/event/stream.rs:104-137`), so `step`'s own
/// `None` arm below is unreachable behind a real `EventStream`.
///
/// This is not a latch: `step` resets `errors` to 0 on the three arms
/// that hand a successfully read event back (a key press or repeat, a
/// paste, a resize), so a caller that kept polling past a `None` and
/// saw one of those again would trace once more rather than staying
/// closed.
/// Unreachable today — both `drive`'s idle and running loops stop
/// polling a `KeySource` the moment `next` returns `None` — but worth
/// saying so a future caller does not assume otherwise.
const MAX_CONSECUTIVE_READ_ERRORS: u32 = 2;

/// Keys from a real terminal, over crossterm's `EventStream`. Release
/// events are dropped (most terminals never send them; some do, and a
/// release is not a new input), as are focus and mouse events.
/// Generic over the stream so a test can drive `next` more than once
/// against a scripted sequence of polls: production code only ever
/// names `TerminalKeys` (the default `S = EventStream`); `new()`
/// still returns exactly that.
pub struct TerminalKeys<S = EventStream> {
    stream: S,
    /// Consecutive read errors since the last successfully read event
    /// that reached `next`'s caller; reset on `step`'s key
    /// press-or-repeat, paste and resize arms only. See
    /// `MAX_CONSECUTIVE_READ_ERRORS`.
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
}

/// The mapping for one poll of the stream, pinned on its own (with a
/// plain `Option<Result<..>>` standing in for the poll) so a test does
/// not need a real, breakable `EventStream` to exercise it in
/// isolation. `errors` is reset on three arms only — a key press or
/// repeat, a paste, a resize — the successful reads that reach
/// `next`'s caller. The two arms that return `Step::Continue` (a key
/// release; a focus or mouse event) leave it alone, so an `Err` run
/// alternating with either still reaches `MAX_CONSECUTIVE_READ_ERRORS`
/// instead of being masked by an event `next`'s caller never sees; the
/// `Err` arm itself increments rather than resets, which is what lets
/// a run of errors reach the threshold at all.
fn step(errors: &mut u32, event: Option<Result<Event, std::io::Error>>) -> Step {
    match event {
        Some(Ok(Event::Key(key))) => {
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                *errors = 0;
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
        Some(Ok(Event::FocusGained | Event::FocusLost | Event::Mouse(_))) => Step::Continue,
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

impl Default for TerminalKeys {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl<S> KeySource for TerminalKeys<S>
where
    S: futures_util::Stream<Item = Result<Event, std::io::Error>> + Unpin + Send,
{
    async fn next(&mut self) -> Option<Input> {
        loop {
            match step(&mut self.errors, self.stream.next().await) {
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
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

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

    fn key_release() -> Option<Result<Event, std::io::Error>> {
        Some(Ok(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))))
    }

    fn paste() -> Option<Result<Event, std::io::Error>> {
        Some(Ok(Event::Paste("hi".to_string())))
    }

    fn resize() -> Option<Result<Event, std::io::Error>> {
        Some(Ok(Event::Resize(80, 24)))
    }

    fn focus() -> Option<Result<Event, std::io::Error>> {
        Some(Ok(Event::FocusGained))
    }

    /// Guards the per-poll mapping in isolation, with a bare `&mut u32`
    /// and no stream at all: a first read error is traced and the source
    /// stays open; a second consecutive one closes it, the same as the
    /// source itself closing. The counter's own accumulation across
    /// polls is pinned separately, through `next`, by
    /// `a_second_consecutive_read_error_closes_the_source_across_polls`.
    /// Mutation:
    /// raise `MAX_CONSECUTIVE_READ_ERRORS` to 3 — the second call below
    /// returns `Step::Return(Some(Errored(..)))` instead of
    /// `Step::Return(None)`.
    #[test]
    fn a_second_consecutive_read_error_closes_the_source() {
        let mut errors = 0;
        assert_eq!(
            step(&mut errors, err()),
            Step::Return(Some(Input::Errored("boom".to_string())))
        );
        assert_eq!(step(&mut errors, err()), Step::Return(None));
    }

    /// A successful read that reaches `next`'s caller (a key press, a
    /// paste, a resize) resets the count in between two errors: guards
    /// `step`'s `*errors = 0` on each of its three success arms.
    /// Mutation: drop the reset on the arm named in the failure — the
    /// third call below returns `Step::Return(None)` instead of tracing
    /// again.
    #[test]
    fn a_success_between_errors_resets_the_count() {
        for (label, reset) in [
            ("key press", key_press()),
            ("paste", paste()),
            ("resize", resize()),
        ] {
            let mut errors = 0;
            assert_eq!(
                step(&mut errors, err()),
                Step::Return(Some(Input::Errored("boom".to_string())))
            );
            step(&mut errors, reset);
            assert_eq!(
                step(&mut errors, err()),
                Step::Return(Some(Input::Errored("boom".to_string()))),
                "a reset counter should trace again rather than closing ({label})"
            );
        }
    }

    /// A key release or a focus/mouse event is dropped (`Step::Continue`)
    /// and must not reset the count: guards the arms that ignored this
    /// on purpose, so an fd alternating an error with one of these
    /// still reaches the threshold. Mutation: put `*errors = 0;` back
    /// on the arm named in the failure — the third call below returns
    /// `Step::Return(Some(Errored(..)))` instead of `Step::Return(None)`.
    #[test]
    fn an_ignored_event_between_errors_does_not_reset_the_count() {
        for (label, ignored) in [("key release", key_release()), ("focus", focus())] {
            let mut errors = 0;
            assert_eq!(
                step(&mut errors, err()),
                Step::Return(Some(Input::Errored("boom".to_string())))
            );
            assert_eq!(
                step(&mut errors, ignored),
                Step::Continue,
                "{label} should be dropped, not returned"
            );
            assert_eq!(
                step(&mut errors, err()),
                Step::Return(None),
                "an ignored event ({label}) reset the error count"
            );
        }
    }

    /// Guards the counter itself, not just the pure mapping above: two
    /// consecutive errors polled off a real (if scripted) stream, via
    /// `TerminalKeys::next`, close the source — the `errors: u32` field
    /// and the `&mut self.errors` that threads it between polls. The
    /// tests above call `step` directly with a local counter, which
    /// cannot exercise this: it is the `errors` field on `TerminalKeys`,
    /// not `step`, that carries the count across `.await` points — each
    /// `next()` call below returns on its first poll, so the loop body
    /// runs once per call and never comes round again. Mutation: reset
    /// `self.errors` to 0 at the top of `next`'s loop body — the second
    /// `next().await` below then returns `Some(Errored(..))` again
    /// instead of `None`.
    #[tokio::test]
    async fn a_second_consecutive_read_error_closes_the_source_across_polls() {
        let mut keys = TerminalKeys {
            stream: futures_util::stream::iter([
                Err(std::io::Error::other("boom")),
                Err(std::io::Error::other("boom")),
            ]),
            errors: 0,
        };
        assert_eq!(keys.next().await, Some(Input::Errored("boom".to_string())));
        assert_eq!(keys.next().await, None);
    }

    /// Guards `next`'s own loop, not just `step`'s mapping: a
    /// `Step::Continue` must make the loop poll again rather than
    /// return. Mutation: replace `Step::Continue => {}` with
    /// `Step::Continue => return None,` — `next` then returns `None`
    /// on the release below instead of polling again for the press.
    #[tokio::test]
    async fn an_ignored_event_does_not_end_the_source_through_next() {
        let press = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let mut keys = TerminalKeys {
            stream: futures_util::stream::iter([
                Ok(Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ))),
                Ok(Event::Key(press)),
            ]),
            errors: 0,
        };
        assert_eq!(keys.next().await, Some(Input::Key(press)));
    }
}
