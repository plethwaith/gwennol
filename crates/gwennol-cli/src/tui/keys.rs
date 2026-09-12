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
}

/// Where the loop gets its keys. `None` once the source is closed.
#[async_trait::async_trait]
pub trait KeySource {
    /// The next input, or `None` once the source is closed.
    async fn next(&mut self) -> Option<Input>;
}

/// Keys from a real terminal, over crossterm's `EventStream`. Release
/// events are dropped (most terminals never send them; some do, and a
/// release is not a new input), as are focus and mouse events.
pub struct TerminalKeys(EventStream);

impl TerminalKeys {
    /// A source reading the process's own terminal.
    pub fn new() -> Self {
        Self(EventStream::new())
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
            match self.0.next().await {
                Some(Ok(Event::Key(key))) => {
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        return Some(Input::Key(key));
                    }
                }
                Some(Ok(Event::Paste(text))) => return Some(Input::Paste(text)),
                Some(Ok(Event::Resize(_, _))) => return Some(Input::Resize),
                Some(Ok(Event::FocusGained | Event::FocusLost | Event::Mouse(_))) => {}
                Some(Err(_)) | None => return None,
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
