//! The pane's state and how a loop [`Event`] becomes an entry in it
//! (D6): the source of truth `drive` renders every frame from, and the
//! one place an `Event` is read in this module tree. `Shared` is how
//! another task — a running turn, a host step asking for approval —
//! reaches it: a `Mutex` (poison-proof: a panic elsewhere must not
//! stop later updates from landing) plus a `watch` generation the loop
//! and tests wait on. `Ui` also holds the open approval prompts, the
//! session rules an `a`/`d` answer at one has made, and the pane's
//! scroll and focus, all read and written under the same lock a
//! judgement runs under.

use std::borrow::Cow;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use gwennol_core::{Event, ToolCall};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use tokio::sync::watch;

use crate::policy::SessionRule;
use crate::show;
use crate::tui::editor::Editor;
use crate::tui::prompt::{self, Prompt};

/// One line in the transcript pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// What the user sent.
    User(String),
    /// A chunk of the model's answer. Consecutive `Text` events append
    /// to the most recent open one rather than opening a new entry.
    Assistant(String),
    /// A tool call: print mode's one-line form, or the arguments
    /// whole.
    ToolCall {
        /// The call itself.
        call: ToolCall,
        /// Drawn whole rather than as print mode's one-line form;
        /// flipped by `Ui::toggle`.
        expanded: bool,
    },
    /// A tool result: print mode's one-line preview, or the content
    /// whole.
    ToolResult {
        /// The call this is the result of.
        call: ToolCall,
        /// The tool's own output.
        content: String,
        /// Whether the tool reported an error.
        is_error: bool,
        /// Drawn whole rather than as print mode's one-line form;
        /// flipped by `Ui::toggle`.
        expanded: bool,
    },
    /// A trace line: a decision, a tool call's failure, a retry, a
    /// startup warning, an input-read error, or an event this
    /// frontend cannot show, each `gwennol: `-prefixed; `/help`'s
    /// lines ([`HELP`]) are pushed as written.
    Trace(String),
    /// The turn's outcome line.
    Outcome(String),
}

impl Entry {
    /// The text the pane draws for this entry at its current
    /// expansion; for a tool call or result, print mode's line
    /// collapsed and the whole form expanded.
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Entry::User(s) | Entry::Assistant(s) | Entry::Trace(s) | Entry::Outcome(s) => {
                Cow::Borrowed(s.as_str())
            }
            Entry::ToolCall { call, expanded } => Cow::Owned(format!(
                "gwennol: {}",
                if *expanded {
                    show::tool_call_whole(call)
                } else {
                    show::tool_call(call)
                }
            )),
            Entry::ToolResult {
                call,
                content,
                is_error,
                expanded,
            } => Cow::Owned(format!(
                "gwennol: {}",
                show::tool_result(call, content, *is_error, u8::from(*expanded))
            )),
        }
    }
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
    /// change what `render_pane` draws (every [`Ui::push`], every call
    /// to [`Ui::apply`], and every [`Ui::toggle`]). `render_pane`
    /// rewraps the whole transcript only when this, or the pane's
    /// width, differs from the cached frame: the 100ms tick and a
    /// redraw it triggers otherwise rewrap on every frame for a
    /// spinner character alone.
    revision: u64,
    /// `render_pane`'s last computed rows, and the `(revision, width)`
    /// they were computed at.
    pane_cache: std::cell::RefCell<Option<PaneCache>>,
    /// Open approval prompts: the first is shown, and answered first;
    /// the rest wait behind it. [`crate::tui::prompt::PromptGuard`]
    /// and [`crate::tui::prompt::key`] are the only writers — the
    /// `entries` doc's "only read" rule applies here too.
    pub prompts: Vec<Prompt>,
    /// The last id assigned to a prompt; [`crate::tui::prompt::PromptGuard::open`]
    /// increments and assigns it.
    pub prompt_seq: u64,
    /// Rules made at prompts, in the order made: tried after every
    /// compiled rule by `Interactive::approve` under this same lock.
    pub session_rules: Vec<SessionRule>,
    /// The canonical workspace, set once by `tui::start` (`tui/mod.rs`)
    /// before the frontend runs a turn: what a prompt's key handler
    /// renders the answered request's trace line against (what
    /// `answer_prompt` passes to `show::decided`). A session rule's
    /// own subject is not taken from here — `Interactive::approve`
    /// computes it from its own workspace via `show::subject` and
    /// the prompt carries it.
    pub workspace: PathBuf,
    /// `None` while the pane follows the tail; `Some(top)` names the
    /// first row shown, clamped again at every render. Written by
    /// [`crate::tui::pane::key`] and reset by [`Ui::follow_tail`]; the
    /// `entries` doc's "only read" rule does not apply here — this is
    /// `pane`'s own state, not derived from `entries`.
    pub scroll: Option<usize>,
    /// The index of the entry a `Tab`/`Shift+Tab`/`Enter` acts on, if
    /// any. Written by [`crate::tui::pane::key`] and reset by
    /// [`Ui::follow_tail`]; the `entries` doc's "only read" rule
    /// applies to this field's target, same as `open`'s.
    pub focus: Option<usize>,
    /// What the last `render_pane` drew. [`crate::tui::pane::key`]'s
    /// paging and `Home` arms compute against it; `reveal` takes only
    /// its width and height; `render_status` reads `following`.
    pub pane_view: Cell<PaneView>,
}

/// What the last `render_pane` drew: its area's width and height, the
/// wrapped row count, and the first row shown. `pane::key`'s paging and
/// `Home` arms compute against it; `reveal` takes only its width and
/// height; `render_status` reads `following`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneView {
    /// The area's width, in columns.
    pub width: u16,
    /// The area's height, in rows.
    pub height: u16,
    /// The whole wrapped row count.
    pub rows: usize,
    /// The first row shown.
    pub top: usize,
}

impl PaneView {
    /// Whether the last row drawn was the last row there is.
    pub fn following(&self) -> bool {
        self.top + (self.height as usize) >= self.rows
    }
}

/// One of `render_pane`'s wrapped, styled rows: which entry it belongs
/// to, and whether it is that entry's first (head) row — the one a
/// focus highlight draws on.
struct PaneRow {
    text: String,
    style: Style,
    entry: usize,
    head: bool,
}

/// `render_pane`'s last computed rows, and the `(revision, width)`
/// they were computed at.
type PaneCache = (u64, u16, Vec<PaneRow>);

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
            prompts: Vec::new(),
            prompt_seq: 0,
            session_rules: Vec::new(),
            workspace: PathBuf::new(),
            scroll: None,
            focus: None,
            pane_view: Cell::new(PaneView::default()),
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

    /// Map one loop event onto the pane and the turn state (D6): a
    /// tool result starts expanded when `verbosity` is 1 or more —
    /// print mode's `-v` — and a tool call never starts expanded,
    /// since print mode never writes a call's arguments as rows either.
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
                self.push(Entry::ToolCall {
                    call,
                    expanded: false,
                });
                self.turn = TurnState::Streaming {
                    since: self.since(),
                };
            }
            Event::ToolResult {
                call,
                content,
                is_error,
            } => {
                self.push(Entry::ToolResult {
                    call,
                    content,
                    is_error,
                    expanded: verbosity >= 1,
                });
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

    /// Flip `expanded` on the entry at `index`, if it can expand
    /// (D4), bumping `revision` so the next frame rewraps at the new
    /// size. `false`, with nothing changed, when the index is out of
    /// range or the entry cannot expand.
    pub fn toggle(&mut self, index: usize) -> bool {
        match self.entries.get_mut(index) {
            Some(Entry::ToolCall { expanded, .. } | Entry::ToolResult { expanded, .. }) => {
                *expanded = !*expanded;
                self.revision = self.revision.wrapping_add(1);
                true
            }
            _ => false,
        }
    }

    /// Return the pane to the tail and drop any focus: what a
    /// submitted turn or `/help` does, since the answer they are
    /// about to produce belongs at the tail.
    pub fn follow_tail(&mut self) {
        self.scroll = None;
        self.focus = None;
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
    /// held must not stop this from updating. Plain and non-reentrant:
    /// two `lock()` calls live at once in the same expression or
    /// scope deadlock, with no timeout to surface it.
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
    "Esc cancels the running turn (denies once instead, at an open approval prompt)",
    "y n a d answer an open approval prompt: once, or for the rest of the session; Esc denies once",
    "PageUp PageDown scroll the pane; Home End too while the editor is empty; End follows the tail again",
    "Tab Shift+Tab focus a tool call or result, newest first; Enter on an empty line expands or collapses it",
];

/// The status line's marker for a pane not following the tail.
pub const SCROLLED: &str = "scrolled up · End follows the tail";

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
        Entry::Trace(_) | Entry::Outcome(_) | Entry::ToolCall { .. } | Entry::ToolResult { .. } => {
            Style::new().add_modifier(Modifier::DIM)
        }
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

/// The four regions a frame is split into: the pane, the approval box
/// (zero height with no prompt open), the status line, and the
/// editor.
fn regions(ui: &Ui, area: Rect) -> [Rect; 4] {
    let h = ui
        .prompts
        .first()
        .map_or(0, |p| prompt::height(p, area.width, area.height));
    Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

struct View<'a>(&'a Ui);

impl Widget for View<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [pane, prompt_area, status, editor] = regions(self.0, area);
        render_pane(self.0, pane, buf);
        if let Some(prompt) = self.0.prompts.first()
            && prompt_area.height > 0
        {
            prompt::render_prompt(prompt, prompt_area, buf);
        }
        render_status(self.0, status, buf);
        render_editor(self.0, editor, buf);
    }
}

fn render_pane(ui: &Ui, area: Rect, buf: &mut Buffer) {
    // `regions` can hand back a rect [`Layout`] could not actually fit
    // inside `buf`'s area when the frame is shorter than the fixed
    // rows it asks for (`Length(1)` twice, `Length(h)` for an open
    // prompt, plus `Min(1)`); clipped to what the buffer really has
    // before any `set_stringn` below indexes it, rather than trusting
    // the sub-rect's own bounds.
    let area = area.intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        // A stale `pane_view` from a previous, taller frame must not
        // go on claiming the pane follows (or does not follow) the
        // tail once there is no pane to draw at all.
        ui.pane_view.set(PaneView::default());
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
        let mut rows: Vec<PaneRow> = Vec::new();
        for (entry_idx, entry) in ui.entries.iter().enumerate() {
            let style = style_of(entry);
            for (i, row) in wrap(&entry.text(), width).into_iter().enumerate() {
                rows.push(PaneRow {
                    text: row,
                    style,
                    entry: entry_idx,
                    head: i == 0,
                });
            }
        }
        *cache = Some((ui.revision, area.width, rows));
    }
    let rows = &cache.as_ref().expect("just populated above").2;
    let max_top = rows.len().saturating_sub(height);
    let top = ui.scroll.map_or(max_top, |t| t.min(max_top));
    ui.pane_view.set(PaneView {
        width: area.width,
        height: area.height,
        rows: rows.len(),
        top,
    });
    for (i, row) in rows[top..].iter().take(height).enumerate() {
        let style = if row.head && ui.focus == Some(row.entry) {
            row.style.add_modifier(Modifier::REVERSED)
        } else {
            row.style
        };
        buf.set_stringn(area.x, area.y + i as u16, &row.text, width, style);
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
    if !ui.pane_view.get().following() {
        let x = area.x + (area.width).saturating_sub(SCROLLED.chars().count() as u16);
        let remaining = (area.width as usize).saturating_sub((x - area.x) as usize);
        buf.set_stringn(
            x,
            area.y,
            SCROLLED,
            remaining,
            Style::new().add_modifier(Modifier::BOLD),
        );
    }
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

/// Draw one frame: the pane, the approval box, the status line, the
/// editor, and the cursor's screen position.
pub fn render(ui: &Ui, frame: &mut Frame) {
    let area = frame.area();
    let [_, _, _, editor_area] = regions(ui, area);
    frame.render_widget(View(ui), area);
    let width = editor_area.width as usize;
    let chars: Vec<char> = ui.editor.text().chars().collect();
    let (_, offset) = editor_window(&chars, ui.editor.cursor(), width);
    let x = editor_area.x + 2 + u16::try_from(offset).unwrap_or(0);
    frame.set_cursor_position((x, editor_area.y));
}

#[cfg(test)]
mod tests {
    use gwennol_core::{Access, ApprovalRequest, Failure, ToolCall};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::prompt::{self, Prompt};

    /// A `Prompt` for a plain write with no cause, for the render
    /// tests below: never answered, and never needs to be — these
    /// tests only draw it.
    fn open_write_prompt() -> Prompt {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        Prompt::new(
            ApprovalRequest {
                plugin: "tool-write".to_string(),
                cause: None,
                access: Access::WriteFile(std::path::PathBuf::from("/ws/out.txt")),
            },
            None,
            "write /ws/out.txt".to_string(),
            Some("/ws/out.txt".to_string()),
            tx,
        )
    }

    /// Guards the two `/help` lines this change adds. `interactive.rs`'s
    /// run F pushes `HELP` into the pane and compares the result back
    /// against `HELP` itself, which stays true regardless of `HELP`'s
    /// length or content — deleting either line here leaves that
    /// comparison green, since it slices exactly `HELP.len()` entries
    /// either way. A literal count and literal text is the only pin.
    /// Mutations: delete either of the two lines this change adds.
    #[test]
    fn help_gained_the_pane_and_focus_lines() {
        assert_eq!(HELP.len(), 6, "{HELP:?}");
        assert_eq!(
            HELP[4],
            "PageUp PageDown scroll the pane; Home End too while the editor is empty; End follows the tail again"
        );
        assert_eq!(
            HELP[5],
            "Tab Shift+Tab focus a tool call or result, newest first; Enter on an empty line expands or collapses it"
        );
    }

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
        assert!(
            matches!(
                streamed.entries[2],
                Entry::ToolCall {
                    expanded: false,
                    ..
                }
            ),
            "{:?}",
            streamed.entries[2]
        );
        assert!(
            matches!(
                streamed.entries[3],
                Entry::ToolResult {
                    expanded: false,
                    ..
                }
            ),
            "{:?}",
            streamed.entries[3]
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
    /// splits `Min(1)/Length(h)/Length(1)/Length(1)` (`regions` in this
    /// file; `h` is zero with no prompt open), so below three rows the
    /// status or editor area's `y` falls outside the buffer and
    /// `buf.set_stringn` indexed it unconditionally; `editor_window`'s
    /// `cursor - start` underflowed once `width < 3`; and, with a
    /// prompt open, `render_prompt`'s own block and rows the same way
    /// at heights 1–4 and 20. Mutation: drop either of the first two guards —
    /// `TestBackend::new(80, 1)` or `(80, 2)` then panics with "index
    /// outside of buffer"; a 5-char buffer at width 1 or 2 panics with
    /// "attempt to subtract with overflow". Also: `render_prompt`
    /// given an area larger than the buffer it draws into — which
    /// `regions`' own `Layout` never hands it in practice (every
    /// region it returns already fits `area`), but the direct call
    /// below bypasses that and drives the guard itself — must not
    /// panic either, at heights 1–4 and 20 inside a 5x2 buffer. Mutation for
    /// that case: drop the `intersection` in `render_prompt`. A scrolled,
    /// focused `ToolResult` at width 5 (narrower than [`SCROLLED`]
    /// itself) must not panic either: mutation, drop the
    /// `saturating_sub` in `render_status`'s marker `x` — overflow at
    /// width 5.
    #[test]
    fn a_too_small_terminal_neither_panics_nor_runs_the_cursor_past_the_window() {
        let mut ui = Ui::default();
        ui.push(Entry::Assistant("hi".to_string()));
        ui.push(Entry::ToolResult {
            call: call("toolu_1", "read"),
            content: "l1\nl2\nl3".to_string(),
            is_error: false,
            expanded: false,
        });
        ui.scroll = Some(3);
        ui.focus = Some(1);
        for (w, h) in [(80u16, 1u16), (80, 2), (80, 3), (1, 6), (5, 3)] {
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

        let prompt = open_write_prompt();
        for area_height in [1u16, 2, 3, 4, 20] {
            let mut buf = Buffer::empty(Rect::new(0, 0, 5, 2));
            prompt::render_prompt(&prompt, Rect::new(0, 0, 20, area_height), &mut buf);
        }
    }

    /// Guards the pane's shrink beside an open prompt: `render_pane`
    /// takes the tail of a long entry from its own, now-smaller
    /// region, so the row immediately above the box's top border
    /// (the row carrying the title) is the entry's true last wrapped
    /// row. Mutation: size the pane's `start` from the frame's full
    /// height instead of its own area's.
    #[test]
    fn the_pane_shows_the_tail_beside_an_open_prompt() {
        let mut ui = Ui::default();
        let long: String = (0..100)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        ui.push(Entry::Assistant(long.clone()));
        let expected_last_row = wrap(&long, 20).last().cloned().unwrap();
        ui.prompts.push(open_write_prompt());

        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&ui, f)).unwrap();
        let buf = terminal.backend().buffer();

        let title_row = (0..12u16)
            .find(|&y| row(buf, y, 20).contains(prompt::TITLE))
            .expect("no row carries the prompt's title");
        assert!(title_row > 0, "the box left no room for the pane above it");
        assert_eq!(
            row(buf, title_row - 1, 20).trim_end(),
            expected_last_row,
            "the pane's tail was not shown right above the box"
        );
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
    /// with its own style, the two D1 variants included. Mutation:
    /// collapse every arm to `Style::new()` — every assertion below
    /// fails.
    #[test]
    fn entries_render_with_their_kind_style() {
        let mut ui = Ui::default();
        ui.push(Entry::User("u".to_string()));
        ui.push(Entry::Assistant("a".to_string()));
        ui.push(Entry::Trace("t".to_string()));
        ui.push(Entry::Outcome("o".to_string()));
        ui.push(Entry::ToolCall {
            call: call("c", "x"),
            expanded: false,
        });
        ui.push(Entry::ToolResult {
            call: call("c", "x"),
            content: "r".to_string(),
            is_error: false,
            expanded: false,
        });
        // Wide enough that none of the six one- or two-row entries
        // below wraps unexpectedly and pushes the `User` row (row 0,
        // following the tail) off the top of a pane sized to hold
        // them exactly.
        let backend = TestBackend::new(40, 9);
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
        assert!(
            buf[(0, 4)].modifier.contains(Modifier::DIM)
                && !buf[(0, 4)].modifier.contains(Modifier::BOLD),
            "ToolCall is not dim"
        );
        assert!(
            buf[(0, 5)].modifier.contains(Modifier::DIM)
                && !buf[(0, 5)].modifier.contains(Modifier::BOLD),
            "ToolResult is not dim"
        );
    }

    /// Guards D1, D2, D3: `Entry::text` collapsed is byte-identical to
    /// print mode's line, and expanded to the whole form; `-v`'s
    /// default (results start expanded, calls never); a `Retry` or a
    /// `Text` event leaves a focused expandable entry, and its focus,
    /// untouched. Mutations: `text` uses the whole form for both;
    /// `expanded: false` unconditionally; `tool_call_whole` built from
    /// `preview`.
    #[test]
    fn a_tool_entry_renders_collapsed_or_whole() {
        let mut fields = serde_json::Map::new();
        for i in 0..40 {
            fields.insert(format!("k{i:02}"), serde_json::json!(i));
        }
        let arguments = serde_json::Value::Object(fields).to_string();
        let c = call("toolu_1", "write");
        let c = ToolCall {
            arguments: arguments.clone(),
            ..c
        };

        let collapsed = Entry::ToolCall {
            call: c.clone(),
            expanded: false,
        };
        assert_eq!(
            collapsed.text(),
            format!("gwennol: {}", show::tool_call(&c))
        );
        assert!(collapsed.text().ends_with('…'), "{}", collapsed.text());

        let expanded = Entry::ToolCall {
            call: c.clone(),
            expanded: true,
        };
        assert_eq!(
            expanded.text(),
            format!("gwennol: {}", show::tool_call_whole(&c))
        );
        assert!(expanded.text().contains("\"k39\""), "{}", expanded.text());
        assert!(!expanded.text().contains('…'), "{}", expanded.text());
        // Computed by hand from `serde_json`'s own pretty-print
        // (2-space indent, keys sorted), not by calling
        // `tool_call_whole` again: a mutation that leaves both sides
        // of the `assert_eq!` above equally wrong (`arguments_lines`
        // returning `vec![pretty]` as one un-indented blob, so only
        // the JSON's very first line gains the extra four-space
        // indent) still fails here, since `"k00"`'s own line and the
        // closing brace would not.
        assert!(
            expanded.text().contains("\n    {\n      \"k00\": 0,"),
            "{}",
            expanded.text()
        );
        assert!(
            expanded.text().contains("\"k39\": 39\n    }"),
            "{}",
            expanded.text()
        );

        // Non-JSON arguments expand verbatim, one line per row.
        let verbatim = Entry::ToolCall {
            call: ToolCall {
                arguments: "a\nb".to_string(),
                ..call("toolu_2", "write")
            },
            expanded: true,
        };
        assert!(
            verbatim.text().contains("\n    a\n    b"),
            "{}",
            verbatim.text()
        );

        // A three-line result.
        let content = "line1\nline2\nline3".to_string();
        let result_collapsed = Entry::ToolResult {
            call: c.clone(),
            content: content.clone(),
            is_error: false,
            expanded: false,
        };
        assert_eq!(
            result_collapsed.text(),
            format!("gwennol: {}", show::tool_result(&c, &content, false, 0))
        );
        // Hand-computed, not through `show::tool_result` again: the
        // preview is the content's lines space-joined, on its own
        // four-space-indented row below the byte-count line; the pair
        // above has no assertion of its own otherwise, so a mutation
        // that breaks both sides of that `assert_eq!` identically
        // (e.g. `tool_result` returning the head line alone at every
        // verbosity) would go uncaught.
        assert!(
            result_collapsed.text().ends_with("line1 line2 line3"),
            "{}",
            result_collapsed.text()
        );
        let result_expanded = Entry::ToolResult {
            call: c.clone(),
            content: content.clone(),
            is_error: false,
            expanded: true,
        };
        assert_eq!(
            result_expanded.text(),
            format!("gwennol: {}", show::tool_result(&c, &content, false, 1))
        );
        assert!(
            result_expanded
                .text()
                .ends_with("\n    line1\n    line2\n    line3"),
            "{}",
            result_expanded.text()
        );

        // Empty content: collapsed and expanded agree (nothing to show
        // either way) — checked against a literal, not against each
        // other, so a bug that made both sides equally empty (rather
        // than genuinely equal to the one correct line) would not
        // pass silently.
        let empty_collapsed = Entry::ToolResult {
            call: c.clone(),
            content: String::new(),
            is_error: false,
            expanded: false,
        };
        let empty_expanded = Entry::ToolResult {
            call: c.clone(),
            content: String::new(),
            is_error: false,
            expanded: true,
        };
        assert_eq!(
            empty_collapsed.text(),
            "gwennol: <- write toolu_1: ok, 0 bytes"
        );
        assert_eq!(
            empty_expanded.text(),
            "gwennol: <- write toolu_1: ok, 0 bytes"
        );

        // `-v`'s default: a result starts expanded at verbosity 1+; a
        // call never does.
        let mut ui = Ui::default();
        ui.apply(
            Event::ToolResult {
                call: c.clone(),
                content: content.clone(),
                is_error: false,
            },
            1,
        );
        assert!(matches!(
            ui.entries[0],
            Entry::ToolResult { expanded: true, .. }
        ));
        let mut ui0 = Ui::default();
        ui0.apply(
            Event::ToolResult {
                call: c.clone(),
                content: content.clone(),
                is_error: false,
            },
            0,
        );
        assert!(matches!(
            ui0.entries[0],
            Entry::ToolResult {
                expanded: false,
                ..
            }
        ));
        let mut ui_call = Ui::default();
        ui_call.apply(Event::ToolCall(c.clone()), 1);
        assert!(matches!(
            ui_call.entries[0],
            Entry::ToolCall {
                expanded: false,
                ..
            }
        ));

        // A focused expandable entry survives an unrelated event: a
        // `Retry` retracts only the open `Assistant` entry, never an
        // expandable one, and `Text`/`Retry` never touch `focus`.
        let mut ui = Ui::default();
        ui.apply(Event::ToolCall(c.clone()), 0);
        ui.apply(
            Event::ToolResult {
                call: c.clone(),
                content: content.clone(),
                is_error: false,
            },
            0,
        );
        ui.focus = Some(1);
        let before = ui.entries[1].clone();
        ui.apply(Event::Text("thinking".to_string()), 0);
        ui.apply(
            Event::Retry {
                attempt: 1,
                max_attempts: 2,
                failure: failure("overloaded"),
            },
            0,
        );
        assert_eq!(ui.focus, Some(1));
        assert_eq!(ui.entries[1], before);
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
