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
    /// retry, a startup warning, an input-read error, or an event this
    /// frontend cannot show, each `gwennol: `-prefixed; `/help`'s lines
    /// ([`HELP`]) are pushed as written.
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
        /// When the turn started; carried unchanged across every
        /// state change within one turn, so the status line's seconds
        /// are the turn's, not the round's.
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
    /// Every line the pane holds, oldest first. Callers outside `Ui`
    /// must only read this, never mutate it: `revision` below is kept
    /// in step with it by `Ui::push` and `Ui::apply` alone, and the
    /// type cannot enforce that — a direct `ui.entries.push(...)` would
    /// compile and leave `render_pane`'s cache silently stale.
    pub entries: Vec<Entry>,
    /// The index of the open `Assistant` entry, if any: where the next
    /// `Text` event appends.
    open: Option<usize>,
    /// The running turn's state, for the status line.
    pub turn: TurnState,
    /// The line editor.
    pub editor: Editor,
    /// A message that replaces the status text until the next key (a
    /// Ctrl-C hint, an unknown command): `drive` clears it only on
    /// `Input::Key`, not on a resize, a paste, or an input-read error.
    pub notice: Option<String>,
    /// The session ends once the running turn finishes: set by
    /// a `/exit` submitted while a turn is running, or by the key
    /// source closing then. `drive` takes it after the outcome.
    pub exiting: bool,
    /// Bumped on every redraw tick, for the spinner frame.
    pub tick: usize,
    /// Bumped whenever `entries` or `open` changes in a way that could
    /// change what `render_pane` draws (every [`Ui::push`] and every
    /// call to [`Ui::apply`]). `render_pane` rewraps the whole
    /// transcript only when this, or the pane's width, differs from
    /// the cached frame: the 100ms tick and a redraw it triggers
    /// otherwise rewrap on every frame for a spinner character alone.
    revision: u64,
    /// `render_pane`'s last computed rows, and the `(revision, width)`
    /// they were computed at.
    pane_cache: std::cell::RefCell<Option<PaneCache>>,
}

/// `render_pane`'s wrapped, styled rows and the `(revision, width)`
/// they were computed at.
type PaneCache = (u64, u16, Vec<(String, Style)>);

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
            revision: 0,
            pane_cache: std::cell::RefCell::new(None),
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
        self.revision = self.revision.wrapping_add(1);
    }

    /// Map one loop event onto the pane and the turn state (D6).
    pub fn apply(&mut self, event: Event, verbosity: u8) {
        self.revision = self.revision.wrapping_add(1);
        match event {
            Event::Text(text) => {
                match self.open {
                    Some(idx) if matches!(self.entries[idx], Entry::Assistant(_)) => {
                        if let Entry::Assistant(existing) = &mut self.entries[idx] {
                            existing.push_str(&text);
                        }
                    }
                    // Normally `open` is `None` and this opens the turn's next
                    // `Assistant` entry. It cannot name a non-`Assistant` one:
                    // `open` is only ever set by `push` on an `Assistant`
                    // entry, so the `debug_assert` below is a tripwire for a
                    // future edit, not a live case; either way the text is
                    // kept rather than dropped silently.
                    _ => {
                        debug_assert!(self.open.is_none(), "open pointed at a non-Assistant entry");
                        self.push(Entry::Assistant(text));
                    }
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
            // Bounded and one-lined through `show::preview`, the
            // treatment a tool result's own text gets without `-v` (unlike
            // a tool result, this stays previewed at `-v` and above);
            // the `{other:?}` form would otherwise go into the pane
            // whole.
            other => self.push(Entry::Trace(format!(
                "gwennol: event this frontend cannot show: {}",
                show::preview(&format!("{other:?}"))
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

/// A shared handle to the `Ui` a loop drives and other tasks
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

/// The tail of `text` (as `char`s) from the column a `width`-column
/// row ending at `cursor` would start at, and the cursor's column
/// offset within it: no scrolling while the cursor and the text
/// before it fit, else a start that keeps the cursor in view at the
/// row's last column. Not clipped on the right to `width`: the
/// caller's `buf.set_stringn` truncates.
fn editor_window(text: &[char], cursor: usize, width: usize) -> (String, usize) {
    let start = if cursor + 2 >= width {
        (cursor + 3).saturating_sub(width)
    } else {
        0
    };
    // `width` under 3 can put `start` past `cursor` (`cursor + 3 -
    // width` grows faster than `cursor` as `width` shrinks below 3);
    // clamped so the offset below never underflows.
    let start = start.min(cursor).min(text.len());
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
    // `regions` can hand back a rect [`Layout`] could not actually fit
    // inside `buf`'s area when the frame is shorter than the three
    // fixed rows it asks for (`Length(1)` twice plus `Min(1)`); clipped
    // to what the buffer really has before any `set_stringn` below
    // indexes it, rather than trusting the sub-rect's own bounds.
    let area = area.intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let height = area.height as usize;
    // Rewrapping the whole, unbounded transcript is only worth
    // redoing when `entries`/`open` (`revision`) or the pane's width
    // changed since the last frame: the 100ms tick redraws for the
    // spinner alone, and the `changed()` arm it also triggers would
    // otherwise redo this same work a second time right after.
    let mut cache = ui.pane_cache.borrow_mut();
    let fresh = matches!(&*cache, Some((rev, w, _)) if *rev == ui.revision && *w == area.width);
    if !fresh {
        let mut rows: Vec<(String, Style)> = Vec::new();
        for entry in &ui.entries {
            let style = style_of(entry);
            for row in wrap(text_of(entry), width) {
                rows.push((row, style));
            }
        }
        *cache = Some((ui.revision, area.width, rows));
    }
    let rows = &cache.as_ref().expect("just populated above").2;
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
    // See `render_pane`: `area` can lie outside `buf` on a too-short
    // frame.
    let area = area.intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
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
    // See `render_pane`: `area` can lie outside `buf` on a too-short
    // frame.
    let area = area.intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
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

    /// Guards two panics on a terminal under three rows: `regions`
    /// splits `Min(1)/Length(1)/Length(1)`, so below three rows the
    /// status or editor area's `y` falls outside the buffer and
    /// `buf.set_stringn` indexed it unconditionally; `editor_window`'s
    /// `cursor - start` underflowed once `width < 3`. Mutation: drop
    /// either guard — `TestBackend::new(80, 1)` or `(80, 2)` then
    /// panics with "index outside of buffer"; a 5-char buffer at
    /// width 1 or 2 panics with "attempt to subtract with overflow".
    #[test]
    fn a_too_small_terminal_neither_panics_nor_runs_the_cursor_past_the_window() {
        let mut ui = Ui::default();
        ui.push(Entry::Assistant("hi".to_string()));
        for (w, h) in [(80u16, 1u16), (80, 2), (80, 3), (1, 6)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| render(&ui, f))
                .unwrap_or_else(|e| panic!("{w}x{h} failed to draw: {e}"));
        }

        let text: Vec<char> = "abcde".chars().collect();
        for width in [1usize, 2, 3, 80] {
            let (window, offset) = editor_window(&text, 4, width);
            assert!(
                offset <= window.chars().count(),
                "width {width}: offset {offset} exceeds window {window:?}"
            );
        }
    }

    /// Guards D8's status line, previously pinned only by the idle
    /// string: a notice overrides the state text; `Working` and
    /// `Streaming` render distinct prefixes; two different `tick`
    /// values give two different spinner frames. Mutation: replace
    /// `"working {frame} …"`/`"streaming {frame} …"` with fixed
    /// strings, or `SPINNER[ui.tick % SPINNER.len()]` with
    /// `SPINNER[0]` — either leaves this red.
    #[test]
    fn the_status_line_shows_the_notice_the_state_and_the_spinner() {
        let mut ui = Ui::default();
        assert_eq!(status_text(&ui), "/help for commands");

        ui.notice = Some("a notice".to_string());
        assert_eq!(status_text(&ui), "a notice");
        ui.notice = None;

        ui.turn = TurnState::Working {
            since: Instant::now(),
        };
        assert!(
            status_text(&ui).starts_with("working "),
            "{}",
            status_text(&ui)
        );
        ui.turn = TurnState::Streaming {
            since: Instant::now(),
        };
        assert!(
            status_text(&ui).starts_with("streaming "),
            "{}",
            status_text(&ui)
        );

        ui.tick = 0;
        let frame0 = status_text(&ui);
        ui.tick = 1;
        let frame1 = status_text(&ui);
        assert_ne!(frame0, frame1, "the spinner did not change with tick");
    }

    /// Guards the cursor's screen column, previously unpinned: the
    /// terminal's reported cursor position tracks the editor's own
    /// cursor as it moves. Mutation: replace the computed `x` with
    /// `editor_area.x` — both assertions below fail.
    #[test]
    fn the_terminal_cursor_tracks_the_editor_cursor() {
        let mut ui = Ui::default();
        for c in "hello".chars() {
            ui.editor.key(&crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&ui, f)).unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            ratatui::layout::Position::new(2 + 5, 5)
        );

        ui.editor.key(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::NONE,
        ));
        terminal.draw(|f| render(&ui, f)).unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            ratatui::layout::Position::new(2 + 4, 5)
        );
    }

    /// Guards `style_of`, previously unpinned: each entry kind renders
    /// with its own style. Mutation: collapse every arm to
    /// `Style::new()` — every assertion below fails.
    #[test]
    fn entries_render_with_their_kind_style() {
        let mut ui = Ui::default();
        ui.push(Entry::User("u".to_string()));
        ui.push(Entry::Assistant("a".to_string()));
        ui.push(Entry::Trace("t".to_string()));
        ui.push(Entry::Outcome("o".to_string()));
        let backend = TestBackend::new(20, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&ui, f)).unwrap();
        let buf = terminal.backend().buffer();
        assert!(
            buf[(0, 0)].modifier.contains(Modifier::BOLD),
            "User is not bold"
        );
        assert!(
            !buf[(0, 1)].modifier.contains(Modifier::BOLD),
            "Assistant is bold"
        );
        assert!(
            !buf[(0, 1)].modifier.contains(Modifier::DIM),
            "Assistant is dim"
        );
        assert!(
            buf[(0, 2)].modifier.contains(Modifier::DIM),
            "Trace is not dim"
        );
        assert!(
            buf[(0, 3)].modifier.contains(Modifier::DIM),
            "Outcome is not dim"
        );
    }

    /// Guards the pane's render cache: a render after `entries` changed
    /// shows the new content, not a stale cached frame; the cache is
    /// keyed on width too, so a resize is not served the old width's
    /// rows. Mutation: drop `revision` (or `width`) from the cache key
    /// comparison in `render_pane` — the second draw below still
    /// shows only "first" (or the 20-column wrapping).
    #[test]
    fn the_pane_cache_invalidates_on_new_entries_and_on_resize() {
        let mut ui = Ui::default();
        ui.push(Entry::Trace("gwennol: first".to_string()));
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&ui, f)).unwrap();
        assert!(row(terminal.backend().buffer(), 0, 20).contains("first"));

        ui.push(Entry::Trace("gwennol: second".to_string()));
        terminal.draw(|f| render(&ui, f)).unwrap();
        let buf = terminal.backend().buffer();
        assert!(row(buf, 0, 20).contains("first"));
        assert!(row(buf, 1, 20).contains("second"));

        // A resize (a fresh, wider backend) rewraps rather than
        // reusing rows cached at the old width: this entry is long
        // enough to wrap across two rows at 20 columns but fits one
        // at 40, so a stale, narrower cache would visibly split it.
        let mut wrapping = Ui::default();
        wrapping.push(Entry::Trace("gwennol: twenty-six characters".to_string()));
        let narrow_backend = TestBackend::new(20, 6);
        let mut narrow_terminal = Terminal::new(narrow_backend).unwrap();
        narrow_terminal.draw(|f| render(&wrapping, f)).unwrap();
        let narrow_buf = narrow_terminal.backend().buffer();
        assert!(
            row(narrow_buf, 1, 20).trim_end() != "",
            "fixture does not wrap at 20 columns: {:?}",
            row(narrow_buf, 0, 20)
        );

        let wide_backend = TestBackend::new(40, 6);
        let mut wide_terminal = Terminal::new(wide_backend).unwrap();
        wide_terminal.draw(|f| render(&wrapping, f)).unwrap();
        let wide_buf = wide_terminal.backend().buffer();
        assert_eq!(
            row(wide_buf, 0, 40).trim_end(),
            "gwennol: twenty-six characters",
            "a resize reused rows wrapped at the old, narrower width"
        );
        assert_eq!(row(wide_buf, 1, 40).trim_end(), "");
    }

    /// Guards the `revision` bump at the top of `apply` itself, load-
    /// bearing for the two arms that mutate `entries` in place rather
    /// than through `push`: a streamed `Text` chunk appended to an
    /// already-open `Assistant` entry must invalidate the pane cache
    /// the same as a `push` does.
    /// `the_pane_cache_invalidates_on_new_entries_and_on_resize` does
    /// not cover this: it only drives `Ui::push` and a width change.
    /// Mutation: delete `self.revision = self.revision.wrapping_add(1);`
    /// from `apply` — the second draw below still shows only "aaa".
    #[test]
    fn a_streamed_append_invalidates_the_pane_cache() {
        let mut ui = Ui::default();
        ui.apply(Event::Text("aaa".to_string()), 0);
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&ui, f)).unwrap();
        assert!(row(terminal.backend().buffer(), 0, 20).contains("aaa"));

        ui.apply(Event::Text("bbb".to_string()), 0);
        terminal.draw(|f| render(&ui, f)).unwrap();
        assert!(
            row(terminal.backend().buffer(), 0, 20).contains("aaabbb"),
            "the pane showed a stale frame after a streamed append: {:?}",
            row(terminal.backend().buffer(), 0, 20)
        );
    }

    /// Same bump, the retraction side: `apply`'s `Event::Retry` arm
    /// over an already-open entry also mutates `entries` in place.
    /// Mutation: delete the same `revision` bump — the second draw
    /// below still shows "partial" after the retraction.
    #[test]
    fn a_retry_over_an_open_entry_invalidates_the_pane_cache() {
        let mut ui = Ui::default();
        ui.apply(Event::Text("partial".to_string()), 0);
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&ui, f)).unwrap();
        assert!(row(terminal.backend().buffer(), 0, 20).contains("partial"));

        ui.apply(
            Event::Retry {
                attempt: 2,
                max_attempts: 3,
                failure: failure("overloaded"),
            },
            0,
        );
        terminal.draw(|f| render(&ui, f)).unwrap();
        assert!(
            !row(terminal.backend().buffer(), 0, 20).contains("partial"),
            "the retracted partial text is still on screen: {:?}",
            row(terminal.backend().buffer(), 0, 20)
        );
    }
}
