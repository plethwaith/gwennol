//! The pane's state and how a loop [`Event`] becomes an entry in it
//! (D6): the source of truth `drive` renders every frame from, and the
//! one place an `Event` is read in this module tree. `Shared` is how
//! another task — a running turn, a host step asking for approval —
//! reaches it: a `Mutex` (poison-proof: a panic elsewhere must not
//! stop later updates from landing) plus a `watch` generation the loop
//! and tests wait on.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use gwennol_core::Event;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use tokio::sync::watch;

use crate::show;
use crate::tui::editor::Editor;

/// One line in the transcript pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// What the user sent.
    User(String),
    /// A chunk of the model's answer. Consecutive `Text` events append
    /// to the most recent open one rather than opening a new entry.
    Assistant(String),
    /// A trace line: a decision, a call, its result, its failure, a
    /// retry, or a startup warning. Always `gwennol: `-prefixed.
    Trace(String),
    /// The turn's outcome line.
    Outcome(String),
}

/// What the status line shows while idle, working or streaming.
#[derive(Debug, Clone, Copy)]
pub enum TurnState {
    /// No turn is running.
    Idle,
    /// A turn is running; the model has not spoken yet, or is between
    /// tool rounds.
    Working {
        /// When the turn (or the round since its last `ToolResult`)
        /// started; carried across state changes within one turn.
        since: Instant,
    },
    /// The model is producing text or asked for a tool.
    Streaming {
        /// See [`TurnState::Working::since`].
        since: Instant,
    },
}

/// The pane's entries, the turn's state, and the editor: everything a
/// frame is drawn from.
pub struct Ui {
    /// Every line the pane holds, oldest first.
    pub entries: Vec<Entry>,
    /// The index of the open `Assistant` entry, if any: where the next
    /// `Text` event appends.
    open: Option<usize>,
    /// The running turn's state, for the status line.
    pub turn: TurnState,
    /// The line editor.
    pub editor: Editor,
    /// A message that replaces the status text until the next key
    /// (a Ctrl-C hint, an unknown command).
    pub notice: Option<String>,
    /// `/exit` was pressed while a turn was unwinding: the loop
    /// returns once it does.
    pub exiting: bool,
    /// Bumped on every redraw tick, for the spinner frame.
    pub tick: usize,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            open: None,
            turn: TurnState::Idle,
            editor: Editor::default(),
            notice: None,
            exiting: false,
            tick: 0,
        }
    }
}

impl Ui {
    /// Push a new entry. Closes the open `Assistant` entry unless the
    /// entry being pushed is itself one, in which case it becomes the
    /// new open entry.
    pub fn push(&mut self, entry: Entry) {
        let is_assistant = matches!(entry, Entry::Assistant(_));
        self.entries.push(entry);
        self.open = if is_assistant {
            Some(self.entries.len() - 1)
        } else {
            None
        };
    }

    /// Map one loop event onto the pane and the turn state (D6).
    pub fn apply(&mut self, event: Event, verbosity: u8) {
        match event {
            Event::Text(text) => {
                match self.open {
                    Some(idx) => {
                        if let Entry::Assistant(existing) = &mut self.entries[idx] {
                            existing.push_str(&text);
                        }
                    }
                    None => self.push(Entry::Assistant(text)),
                }
                self.turn = TurnState::Streaming {
                    since: self.since(),
                };
            }
            Event::ToolCall(call) => {
                self.push(Entry::Trace(format!("gwennol: {}", show::tool_call(&call))));
                self.turn = TurnState::Streaming {
                    since: self.since(),
                };
            }
            Event::ToolResult {
                call,
                content,
                is_error,
            } => {
                self.push(Entry::Trace(format!(
                    "gwennol: {}",
                    show::tool_result(&call, &content, is_error, verbosity)
                )));
                self.turn = TurnState::Working {
                    since: self.since(),
                };
            }
            Event::ToolFailed { call, error } => {
                self.push(Entry::Trace(format!(
                    "gwennol: {}",
                    show::tool_failed(&call, &error)
                )));
                self.turn = TurnState::Working {
                    since: self.since(),
                };
            }
            Event::Retry {
                attempt,
                max_attempts,
                failure,
            } => {
                let line = format!("gwennol: {}", show::retry(attempt, max_attempts, &failure));
                match self.open {
                    Some(idx) => {
                        self.entries[idx] = Entry::Trace(line);
                        self.open = None;
                    }
                    None => self.entries.push(Entry::Trace(line)),
                }
            }
            Event::TurnComplete => {
                self.open = None;
            }
            other => self.push(Entry::Trace(format!(
                "gwennol: event this frontend cannot show: {other:?}"
            ))),
        }
    }

    /// The `since` instant a state transition should carry: the
    /// current one, when the turn is already `Working` or
    /// `Streaming`, else now (a defensive fallback; `drive` always
    /// sets `Working` itself before any event can arrive).
    fn since(&self) -> Instant {
        match self.turn {
            TurnState::Working { since } | TurnState::Streaming { since } => since,
            TurnState::Idle => Instant::now(),
        }
    }

    /// The `Assistant` entries at or after `index`, concatenated: what
    /// the transcript's assistant messages since a turn's `User` entry
    /// should equal, for a completed turn.
    pub fn assistant_text_since(&self, index: usize) -> String {
        self.entries[index..]
            .iter()
            .filter_map(|e| match e {
                Entry::Assistant(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// A process-wide handle to the `Ui` a loop drives and other tasks
/// (a running turn, a host step awaiting approval) update.
pub struct Shared {
    ui: Mutex<Ui>,
    /// Bumped on every [`Shared::update`]; `changed()` on a subscriber
    /// wakes whoever is waiting to redraw.
    pub changed: watch::Sender<u64>,
}

impl Shared {
    /// A fresh, empty `Ui` behind a handle nothing has updated yet.
    pub fn new() -> Arc<Self> {
        let (changed, _) = watch::channel(0);
        Arc::new(Self {
            ui: Mutex::new(Ui::default()),
            changed,
        })
    }

    /// The `Ui`, poison-proof: a panic elsewhere while the lock was
    /// held must not stop this from updating.
    pub fn lock(&self) -> MutexGuard<'_, Ui> {
        self.ui.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `f` against the `Ui` under the lock, then bump the
    /// generation so anyone waiting on `changed` wakes.
    pub fn update<R>(&self, f: impl FnOnce(&mut Ui) -> R) -> R {
        let result = f(&mut self.lock());
        self.changed.send_modify(|g| *g = g.wrapping_add(1));
        result
    }
}

/// The `/help` command's lines.
pub const HELP: &[&str] = &[
    "/exit ends the session (twice while a turn is unwinding: exit at once, status 130)",
    "/help lists the commands",
    "Esc cancels the running turn",
];

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Wrap `text` to `width` columns, counting `char`s: split on `\n`,
/// greedy on whitespace, a word longer than `width` hard-broken across
/// rows. `width` 0 is treated as 1.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in line.split_whitespace() {
            let mut rest = word;
            while rest.chars().count() > width {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                let cut = rest
                    .char_indices()
                    .nth(width)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            let rest_len = rest.chars().count();
            if current.is_empty() {
                current = rest.to_string();
            } else if current.chars().count() + 1 + rest_len <= width {
                current.push(' ');
                current.push_str(rest);
            } else {
                out.push(std::mem::take(&mut current));
                current = rest.to_string();
            }
        }
        out.push(current);
    }
    out
}

/// The style an entry's text renders with.
fn style_of(entry: &Entry) -> Style {
    match entry {
        Entry::User(_) => Style::new().add_modifier(Modifier::BOLD),
        Entry::Assistant(_) => Style::new(),
        Entry::Trace(_) | Entry::Outcome(_) => Style::new().add_modifier(Modifier::DIM),
    }
}

fn text_of(entry: &Entry) -> &str {
    match entry {
        Entry::User(s) | Entry::Assistant(s) | Entry::Trace(s) | Entry::Outcome(s) => s,
    }
}

/// The window of `text` (as `char`s) that fits a `width`-column row
/// ending at `cursor`, and the cursor's column offset within it: no
/// scrolling while the cursor and the text before it fit, else a
/// window that keeps the cursor in view at the row's last column.
fn editor_window(text: &[char], cursor: usize, width: usize) -> (String, usize) {
    let start = if cursor + 2 >= width {
        (cursor + 3).saturating_sub(width)
    } else {
        0
    };
    let start = start.min(text.len());
    let window: String = text[start..].iter().collect();
    (window, cursor - start)
}

/// The three fixed regions a frame is split into.
fn regions(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

struct View<'a>(&'a Ui);

impl Widget for View<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [pane, status, editor] = regions(area);
        render_pane(self.0, pane, buf);
        render_status(self.0, status, buf);
        render_editor(self.0, editor, buf);
    }
}

fn render_pane(ui: &Ui, area: Rect, buf: &mut Buffer) {
    let width = area.width as usize;
    let mut rows: Vec<(String, Style)> = Vec::new();
    for entry in &ui.entries {
        let style = style_of(entry);
        for row in wrap(text_of(entry), width) {
            rows.push((row, style));
        }
    }
    let height = area.height as usize;
    let start = rows.len().saturating_sub(height);
    for (i, (row, style)) in rows[start..].iter().enumerate() {
        buf.set_stringn(area.x, area.y + i as u16, row, width, *style);
    }
}

fn status_text(ui: &Ui) -> String {
    if let Some(notice) = &ui.notice {
        return notice.clone();
    }
    let frame = SPINNER[ui.tick % SPINNER.len()];
    match ui.turn {
        TurnState::Idle => "/help for commands".to_string(),
        TurnState::Working { since } => {
            format!("working {frame} {}s", since.elapsed().as_secs())
        }
        TurnState::Streaming { since } => {
            format!("streaming {frame} {}s", since.elapsed().as_secs())
        }
    }
}

fn render_status(ui: &Ui, area: Rect, buf: &mut Buffer) {
    let width = area.width as usize;
    buf.set_stringn(
        area.x,
        area.y,
        status_text(ui),
        width,
        Style::new().add_modifier(Modifier::DIM),
    );
}

fn render_editor(ui: &Ui, area: Rect, buf: &mut Buffer) {
    let width = area.width as usize;
    let chars: Vec<char> = ui.editor.text().chars().collect();
    let (window, _) = editor_window(&chars, ui.editor.cursor(), width);
    buf.set_stringn(area.x, area.y, format!("> {window}"), width, Style::new());
}

/// Draw one frame: the pane, the status line, the editor, and the
/// cursor's screen position.
pub fn render(ui: &Ui, frame: &mut Frame) {
    let area = frame.area();
    let [_, _, editor_area] = regions(area);
    frame.render_widget(View(ui), area);
    let width = editor_area.width as usize;
    let chars: Vec<char> = ui.editor.text().chars().collect();
    let (_, offset) = editor_window(&chars, ui.editor.cursor(), width);
    let x = editor_area.x + 2 + u16::try_from(offset).unwrap_or(0);
    frame.set_cursor_position((x, editor_area.y));
}

#[cfg(test)]
mod tests {
    use gwennol_core::{Failure, ToolCall};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: Some(id.to_string()),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn failure(message: &str) -> Failure {
        Failure {
            message: message.to_string(),
            retryable: Some(true),
            kind: None,
        }
    }

    fn row(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    /// Guards D6: a `Retry` retracts an open entry rather than
    /// following it; a `ToolResult`/`ToolFailed` closes the open entry
    /// so a later `Text` opens a new one; the invariant an entry-index
    /// slice of `Assistant` text equals what a completed turn's
    /// transcript holds. Mutation: in `apply`, push the retry line
    /// instead of replacing it — `so far` remains.
    #[test]
    fn events_become_entries_and_a_retry_retracts() {
        let stream_events = |ui: &mut Ui| {
            ui.apply(Event::Text("so far".to_string()), 0);
            ui.apply(
                Event::Retry {
                    attempt: 2,
                    max_attempts: 3,
                    failure: failure("overloaded"),
                },
                0,
            );
            assert_eq!(
                ui.entries,
                vec![Entry::Trace(
                    "gwennol: provider failure, retrying (2/3): overloaded".to_string()
                )]
            );
            for e in &ui.entries {
                if let Entry::Assistant(t) | Entry::Trace(t) = e {
                    assert!(!t.contains("so far"), "retry did not retract: {t}");
                }
            }

            ui.apply(Event::Text("Let me ".to_string()), 0);
            assert!(matches!(ui.turn, TurnState::Streaming { .. }));
            ui.apply(Event::Text("read it.".to_string()), 0);
            ui.apply(Event::ToolCall(call("toolu_1", "read")), 0);
            ui.apply(
                Event::ToolResult {
                    call: call("toolu_1", "read"),
                    content: "hi".to_string(),
                    is_error: false,
                },
                0,
            );
            assert!(matches!(ui.turn, TurnState::Working { .. }));
            ui.apply(Event::Text("It says: x".to_string()), 0);
            ui.apply(Event::TurnComplete, 0);
        };

        let mut streamed = Ui::default();
        stream_events(&mut streamed);
        assert_eq!(streamed.entries.len(), 5, "{:?}", streamed.entries);
        assert_eq!(
            streamed.assistant_text_since(1),
            "Let me read it.It says: x"
        );

        // The same events, buffered: every `Text` of a round after the
        // round's tool events instead of interleaved. The mapping is
        // order-driven, so feeding the same events in a different but
        // still-valid order (all one round's text together) yields the
        // same final entries.
        let mut buffered = Ui::default();
        buffered.apply(Event::Text("so far".to_string()), 0);
        buffered.apply(
            Event::Retry {
                attempt: 2,
                max_attempts: 3,
                failure: failure("overloaded"),
            },
            0,
        );
        buffered.apply(Event::Text("Let me read it.".to_string()), 0);
        buffered.apply(Event::ToolCall(call("toolu_1", "read")), 0);
        buffered.apply(
            Event::ToolResult {
                call: call("toolu_1", "read"),
                content: "hi".to_string(),
                is_error: false,
            },
            0,
        );
        buffered.apply(Event::Text("It says: x".to_string()), 0);
        buffered.apply(Event::TurnComplete, 0);
        assert_eq!(buffered.entries, streamed.entries);

        // A `ToolFailed` with no `ToolCall` before it is its own
        // `Trace` entry (the open assistant entry, if any, closes
        // first).
        let mut cut = Ui::default();
        cut.apply(Event::Text("thinking".to_string()), 0);
        cut.apply(
            Event::ToolFailed {
                call: call("toolu_2", "bash"),
                error: "interrupted: the turn was cancelled".to_string(),
            },
            0,
        );
        assert_eq!(
            cut.entries,
            vec![
                Entry::Assistant("thinking".to_string()),
                Entry::Trace(
                    "gwennol: !! bash toolu_2: interrupted: the turn was cancelled".to_string()
                ),
            ]
        );

        // A poisoned lock still updates.
        let shared = Shared::new();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared.update(|_ui| panic!("boom"));
        }));
        assert!(poisoned.is_err());
        shared.update(|ui| ui.push(Entry::Trace("gwennol: after a poison".to_string())));
        assert!(
            shared
                .lock()
                .entries
                .iter()
                .any(|e| matches!(e, Entry::Trace(t) if t.contains("after a poison")))
        );
    }

    /// Guards D6, D8: `wrap`'s greedy fill and hard-break; the pane
    /// shows the tail of a long entry, bottom-up; an empty `Ui`
    /// renders blank rows with the idle status and an empty editor;
    /// the editor scrolls to keep the cursor visible. Mutation: take
    /// the first rows instead of the last.
    #[test]
    fn the_pane_shows_the_tail_and_wraps_words() {
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap("", 5), vec![""]);

        let mut ui = Ui::default();
        let long: String = (0..100)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        ui.push(Entry::Assistant(long.clone()));
        let expected_last_row = wrap(&long, 20).last().cloned().unwrap();

        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&ui, f)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(row(buf, 3, 20).trim_end(), expected_last_row);

        let empty = Ui::default();
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&empty, f)).unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..4 {
            assert_eq!(row(buf, y, 20).trim_end(), "");
        }
        assert_eq!(row(buf, 4, 20).trim_end(), "/help for commands");
        assert_eq!(row(buf, 5, 20).trim_end(), ">");

        let mut scrolled = Ui::default();
        for c in "0123456789012345678901234567890".chars() {
            scrolled.editor.key(&crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&scrolled, f)).unwrap();
        let buf = terminal.backend().buffer();
        let editor_row = row(buf, 5, 20);
        assert_eq!(
            editor_row.trim_end().chars().last(),
            scrolled.editor.text().chars().last(),
            "editor row does not end at the cursor: {editor_row:?}"
        );
    }
}
