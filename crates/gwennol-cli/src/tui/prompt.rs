//! The approval prompt: what `Interactive::approve`
//! installs when no rule (compiled or session) decides a request, how a
//! key answers it, how it comes down when the future behind it is
//! dropped, and what the box shows. Cancel-safety in one sentence: a
//! [`PromptGuard`] removes the prompt by id when dropped — which is what
//! happens when the turn is cancelled while a prompt is open — and an
//! answer whose receiver is already gone is simply ignored.
//!
//! A prompt takes a letter only after [`ARM_DELAY`] with no key pressed
//! since it became the first, so a person typing when it opens does
//! not answer it.

use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gwennol_core::{Access, ApprovalRequest, Decision};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Widget};
use tokio::sync::oneshot;

use crate::policy::{Kind, SessionRule, Unjudgeable};
use crate::show;
use crate::tui::ui::{self, Entry, Shared, Ui};

/// The key legend when the request can be remembered for the session.
pub const KEYS: &str =
    "y allow once · n deny once · a allow for the session · d deny for the session · Esc deny once";
/// The key legend when it cannot: `a`/`d` would have nothing to hold.
pub const KEYS_ONCE: &str = "y allow once · n deny once · Esc deny once · a and d act once here: this request cannot be remembered";
/// The box's title.
pub const TITLE: &str = "approval";

/// How long the keyboard must be quiet, with the same prompt first,
/// before `y`, `n`, `a` or `d` answers it: a person typing when a
/// prompt opens has not read it.
pub const ARM_DELAY: Duration = Duration::from_secs(1);
/// The notice when a letter arrived before [`ARM_DELAY`] had passed.
pub const EARLY: &str =
    "not answered: y, n, a and d count once no key has been pressed for a second; Esc denies now";

/// One open approval, shown until answered or withdrawn.
#[derive(Debug)]
pub struct Prompt {
    /// Assigned by [`PromptGuard::open`]; `0` until then.
    pub id: u64,
    /// What is being asked.
    pub request: ApprovalRequest,
    /// Why no rule short of `any` could judge this. `None` for the
    /// ordinary case, no rule matched; `Some` names the reason the
    /// prompt exists at all when it is something else.
    pub unjudgeable: Option<Unjudgeable>,
    /// `ShowAccess` of the request, as `approve` rendered it.
    pub access_line: String,
    /// [`show::subject`] of the request: `None` when no session rule
    /// can hold it.
    pub subject: Option<String>,
    /// Rows scrolled off the top of the box; clamped at render.
    pub scroll: usize,
    /// `(rows, visible)` the last render measured, for the handler's
    /// clamp: set by [`render_prompt`], read by [`key`].
    pub view: Cell<(usize, usize)>,
    /// [`lines`]'s content rows, computed once: `request`,
    /// `unjudgeable` and `access_line` never change after
    /// construction, so the rows never do either. Populated lazily by
    /// [`lines`]'s first call rather than eagerly here, since a
    /// prompt's rows are otherwise never needed before the first
    /// render. If any of the three ever becomes mutable, clear this
    /// cache (`.take()` it) wherever it changes: nothing enforces the
    /// invariant today, since `Prompt` is `pub` and reachable through
    /// `pub prompts: Vec<Prompt>`.
    lines_cache: std::cell::OnceCell<Vec<String>>,
    answer: Option<oneshot::Sender<Decision>>,
}

impl Prompt {
    /// A prompt for `request`, carrying `answer` to send the decision
    /// back through. `id` is `0` until [`PromptGuard::open`] assigns
    /// one.
    pub fn new(
        request: ApprovalRequest,
        unjudgeable: Option<Unjudgeable>,
        access_line: String,
        subject: Option<String>,
        answer: oneshot::Sender<Decision>,
    ) -> Self {
        Self {
            id: 0,
            request,
            unjudgeable,
            access_line,
            subject,
            scroll: 0,
            view: Cell::new((0, 0)),
            lines_cache: std::cell::OnceCell::new(),
            answer: Some(answer),
        }
    }
}

/// Removes prompt `id` when dropped: the shape
/// [`gwennol_core::Operator::approve`]'s drop obligation asks for. Holds
/// no lock across an `await`, so it cannot deadlock the state it
/// updates.
pub struct PromptGuard {
    shared: Arc<Shared>,
    id: u64,
}

impl PromptGuard {
    /// Installs `prompt` in `shared`'s `Ui`, assigning it the next
    /// id, and returns a guard that removes it again when dropped.
    pub fn open(shared: &Arc<Shared>, mut prompt: Prompt) -> Self {
        let id = shared.update(|ui| {
            if ui.prompts.is_empty() {
                ui.prompt_armed_at = Instant::now() + ARM_DELAY;
            }
            ui.prompt_seq += 1;
            let id = ui.prompt_seq;
            prompt.id = id;
            ui.prompts.push(prompt);
            id
        });
        Self {
            shared: shared.clone(),
            id,
        }
    }
}

impl Drop for PromptGuard {
    fn drop(&mut self) {
        self.shared.update(|ui| {
            let was_first = ui.prompts.first().is_some_and(|p| p.id == self.id);
            ui.prompts.retain(|p| p.id != self.id);
            if was_first && !ui.prompts.is_empty() {
                ui.prompt_armed_at = Instant::now() + ARM_DELAY;
            }
        });
    }
}

/// What a key means to an open prompt.
enum Answer {
    /// `y`/`n`/`Esc`: decides this request alone.
    Once(Decision),
    /// `a`/`d`: decides this request and, when it can be remembered,
    /// every identical one for the rest of the session.
    Session(Decision),
}

/// Route one key to the first open prompt. `true` when the key was
/// the prompt's — every key is, while a prompt is open (D4). `Esc`
/// denies once at any time. `y`/`n`/`a`/`d` (unmodified only) answer
/// once `now` has reached `ui.prompt_armed_at`; before then each is
/// swallowed and sets [`EARLY`]. The scroll keys move `scroll`
/// regardless of modifiers. Every key that does not answer pushes
/// `prompt_armed_at` to `now + ARM_DELAY`.
pub fn key(ui: &mut Ui, event: &KeyEvent, workspace: &Path, now: Instant) -> bool {
    if ui.prompts.is_empty() {
        return false;
    }
    if event.modifiers == KeyModifiers::NONE {
        if event.code == KeyCode::Esc {
            answer_prompt(ui, Answer::Once(Decision::Deny), workspace, now);
            return true;
        }
        let answer = match event.code {
            KeyCode::Char('y') => Some(Answer::Once(Decision::Allow)),
            KeyCode::Char('n') => Some(Answer::Once(Decision::Deny)),
            KeyCode::Char('a') => Some(Answer::Session(Decision::Allow)),
            KeyCode::Char('d') => Some(Answer::Session(Decision::Deny)),
            _ => None,
        };
        if let Some(answer) = answer {
            if now >= ui.prompt_armed_at {
                answer_prompt(ui, answer, workspace, now);
                return true;
            }
            ui.notice = Some(EARLY.to_string());
        }
    }
    ui.prompt_armed_at = now + ARM_DELAY;
    let (rows, visible) = ui.prompts[0].view.get();
    let max_scroll = rows.saturating_sub(visible);
    let page = visible.max(1);
    match event.code {
        KeyCode::Up => {
            ui.prompts[0].scroll = ui.prompts[0].scroll.saturating_sub(1).min(max_scroll);
        }
        KeyCode::Down => {
            ui.prompts[0].scroll = (ui.prompts[0].scroll + 1).min(max_scroll);
        }
        KeyCode::PageUp => {
            ui.prompts[0].scroll = ui.prompts[0].scroll.saturating_sub(page).min(max_scroll);
        }
        KeyCode::PageDown => {
            ui.prompts[0].scroll = (ui.prompts[0].scroll + page).min(max_scroll);
        }
        _ => {}
    }
    true
}

/// Remove the first prompt and send the decision; only when the send
/// lands do the rule the answer asked for (if the subject allows one)
/// and the trace get recorded — so a failed send leaves neither behind.
fn answer_prompt(ui: &mut Ui, answer: Answer, workspace: &Path, now: Instant) {
    let mut prompt = ui.prompts.remove(0);
    if !ui.prompts.is_empty() {
        ui.prompt_armed_at = now + ARM_DELAY;
    }
    let (decision, suffix, rule) = match answer {
        Answer::Once(d) => (d, "", None),
        Answer::Session(d) => match (prompt.subject.clone(), Kind::of(&prompt.request.access)) {
            (Some(subject), Some(kind)) => (
                d,
                " for the rest of the session",
                Some(SessionRule {
                    decision: d,
                    plugin: prompt.request.plugin.clone(),
                    kind,
                    subject,
                }),
            ),
            _ => (d, "", None),
        },
    };
    let verb = match decision {
        Decision::Allow => "allowed",
        Decision::Deny => "denied",
    };
    let sent = prompt
        .answer
        .take()
        .is_some_and(|tx| tx.send(decision).is_ok());
    if sent {
        if let Some(rule) = rule {
            crate::policy::remember(&mut ui.session_rules, rule);
        }
        let line = format!(
            "gwennol: {}",
            show::decided(
                &prompt.request,
                format!("{verb} at the prompt{suffix}"),
                workspace
            )
        );
        ui.push(Entry::Trace(line));
    }
}

/// The box's content rows before wrapping (D5 order), the key line
/// excluded. Computed once per prompt and cloned out of a cache on
/// every later call — `render` and `View::render` each call
/// `regions`, which calls [`height`], and then `render_prompt` calls
/// it again, so this would otherwise re-parse and re-pretty-print the
/// whole tool-call `arguments` three times a frame at the 100 ms tick.
pub fn lines(prompt: &Prompt) -> Vec<String> {
    prompt
        .lines_cache
        .get_or_init(|| compute_lines(prompt))
        .clone()
}

fn compute_lines(prompt: &Prompt) -> Vec<String> {
    let mut out = vec![prompt.access_line.clone()];
    out.push(format!("asked by {}", prompt.request.plugin));
    match &prompt.request.cause {
        Some(call) => {
            out.push(format!(
                "the model asked {} ({}) with these arguments:",
                call.name,
                call.id.as_deref().unwrap_or("")
            ));
            out.extend(show::arguments_lines(&call.arguments));
        }
        None => out.push("started by the frontend, not by a tool call".to_string()),
    }
    if let Access::Spawn {
        stdin: Some(stdin), ..
    } = &prompt.request.access
    {
        out.push(format!(
            "the child will read this on stdin ({} bytes):",
            stdin.len()
        ));
        out.extend(stdin.lines().map(str::to_string));
    }
    if let Some(u) = &prompt.unjudgeable {
        out.push(format!(
            "only a rule of kind any could have matched this: {u}"
        ));
        if matches!(u, Unjudgeable::UnknownKind) {
            out.push(format!("the request: {:?}", prompt.request.access));
        }
    }
    out
}

/// The key legend for `prompt`: the full [`KEYS`] when a session rule
/// can hold this request, else [`KEYS_ONCE`]. Neither constant is a
/// prefix of the other, so a check over every wrapped row of one
/// cannot pass on the other. [`height`] and [`render_prompt`] wrap the
/// legend in full, but a box too short for it draws only the first of
/// its wrapped rows.
fn key_line(prompt: &Prompt) -> &'static str {
    if prompt.subject.is_some() {
        KEYS
    } else {
        KEYS_ONCE
    }
}

/// The box height for `lines` at `width` inside `area_height` (D5):
/// `min(rows + 2 + legend_rows, max(4, area_height * 2 / 3))`, where
/// `rows` is the wrapped content row count and `legend_rows` is
/// `key_line` wrapped at the same width (the two borders account
/// for the other two rows; the legend is wrapped, not truncated, so
/// it is counted like any other content instead of assumed to fit in
/// one row).
pub fn height(prompt: &Prompt, width: u16, area_height: u16) -> u16 {
    let inner_width = width.saturating_sub(2).max(1) as usize;
    let rows: usize = lines(prompt)
        .iter()
        .map(|line| ui::wrap(line, inner_width).len())
        .sum();
    let legend_rows = ui::wrap(key_line(prompt), inner_width).len();
    let max_box = ((area_height as usize) * 2 / 3).max(4);
    (rows + 2 + legend_rows).min(max_box) as u16
}

/// Draw the box: a bordered block titled [`TITLE`]; the content rows
/// wrapped at the inner width, showing the `scroll`-clamped window;
/// the key legend (`key_line`), wrapped at the same width, as the
/// last inner rows, never scrolled. When the box is shorter than
/// [`height`] asked for (D5's "terminal shorter than the box"), one
/// content row is reserved before the legend — the legend is what gets
/// clipped, losing its last wrapped rows first, down to none at all
/// when the box has room for only that one row. A box with no inner
/// rows at all (`inner.height == 0`, which the layout produces at
/// terminal heights 3-5) shows neither: the borders are drawn and
/// nothing inside them.
pub fn render_prompt(prompt: &Prompt, area: Rect, buf: &mut Buffer) {
    let area = area.intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::bordered().title(TITLE);
    let inner = block.inner(area);
    (&block).render(area, buf);
    let width = inner.width as usize;
    let rows: Vec<String> = lines(prompt)
        .iter()
        .flat_map(|line| ui::wrap(line, width))
        .collect();
    let legend = ui::wrap(key_line(prompt), width);
    let visible = (inner.height as usize)
        .saturating_sub(legend.len())
        .max(1)
        .min(inner.height as usize);
    prompt.view.set((rows.len(), visible));
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let start = prompt.scroll.min(rows.len().saturating_sub(visible));
    for (i, row) in rows[start..].iter().take(visible).enumerate() {
        buf.set_stringn(inner.x, inner.y + i as u16, row, width, Style::new());
    }
    let legend_budget = (inner.height as usize).saturating_sub(visible);
    for (i, row) in legend.iter().take(legend_budget).enumerate() {
        buf.set_stringn(
            inner.x,
            inner.y + visible as u16 + i as u16,
            row,
            width,
            Style::new().add_modifier(Modifier::DIM),
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::task::{Context, Poll, Waker};

    use gwennol_core::gwead::tokio_util::sync::CancellationToken;
    use gwennol_core::{Operator, ToolCall};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::policy::Policy;
    use crate::secrets::Secrets;
    use crate::tui::keys::Input;
    use crate::tui::operator::Interactive;

    /// The `TestBackend` width every test below renders at, and the
    /// inner width (`FRAME_W` minus the two border columns) every
    /// `ui::wrap(_, ..)` legend check wraps at: one constant so the
    /// two can never drift apart, as they did when the legend checks
    /// hardcoded `78` (this file's inner width) independently of the
    /// `80` `render_frame` hardcoded as the outer one.
    const FRAME_W: u16 = 80;
    const INNER_W: usize = (FRAME_W - 2) as usize;

    /// An `Interactive` judging by `policy` for workspace `/ws`, and
    /// the `Shared` it renders through (with `workspace` already set,
    /// as `tui::start` sets it before any turn runs).
    pub(crate) fn op(policy: Policy) -> (Arc<Interactive>, Arc<Shared>) {
        let shared = Shared::new();
        let workspace = std::path::PathBuf::from("/ws");
        shared.update(|ui| ui.workspace = workspace.clone());
        let interactive = Arc::new(Interactive::new(
            policy,
            Secrets::new(Vec::new()),
            workspace,
            0,
            shared.clone(),
        ));
        (interactive, shared)
    }

    /// A `write` request from `tool-write`, call `write t1`, arguments
    /// `{"content":"hello","path":"out.txt"}`, access
    /// `WriteFile("/ws/out.txt")`.
    pub(crate) fn write_req() -> ApprovalRequest {
        ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: Some(ToolCall {
                id: Some("t1".to_string()),
                name: "write".to_string(),
                arguments: r#"{"content":"hello","path":"out.txt"}"#.to_string(),
            }),
            access: Access::WriteFile(std::path::PathBuf::from("/ws/out.txt")),
        }
    }

    fn empty_policy() -> Policy {
        Policy::compile(Vec::new(), Path::new("/ws")).unwrap()
    }

    /// The instant `shared`'s first prompt is armed at: what a test
    /// passes as `now` to answer it, instead of sleeping on the wall
    /// clock.
    fn armed(shared: &Arc<Shared>) -> Instant {
        shared.lock().prompt_armed_at
    }

    /// `Context::from_waker(Waker::noop())`, one line at every call
    /// site below instead of three.
    fn noop_context() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    fn render_frame(shared: &Arc<Shared>) -> Vec<String> {
        render_frame_sized(shared, FRAME_W, 24)
    }

    /// Like [`render_frame`], at a caller-chosen size — for the cases
    /// where the fixed 80x24 frame never exercises a short terminal's
    /// box height.
    fn render_frame_sized(shared: &Arc<Shared>, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| crate::tui::ui::render(&shared.lock(), f))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    /// Guards D3: the first poll installs the prompt (id 1) and it
    /// shows in the frame; dropping the future — what happens when
    /// the turn is cancelled under an open prompt — removes it and
    /// the box comes down, with no trace line written (no answer
    /// ever came). Mutation: empty `PromptGuard::drop`.
    #[test]
    fn a_prompt_opens_on_the_first_poll_and_comes_down_when_the_future_is_dropped() {
        let (interactive, shared) = op(empty_policy());
        let mut cx = noop_context();
        let mut fut = interactive.approve(write_req());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        {
            let ui = shared.lock();
            assert_eq!(ui.prompts.len(), 1);
            assert_eq!(ui.prompts[0].id, 1);
        }
        let frame = render_frame(&shared);
        assert!(frame.iter().any(|r| r.contains(TITLE)), "{frame:?}");
        assert!(
            frame.iter().any(|r| r.contains("write /ws/out.txt")),
            "{frame:?}"
        );
        drop(fut);
        assert!(shared.lock().prompts.is_empty());
        let frame = render_frame(&shared);
        assert!(!frame.iter().any(|r| r.contains(TITLE)), "{frame:?}");
        assert!(shared.lock().entries.is_empty(), "no answer ever came");
    }

    /// Guards D2: `compute_lines` renders a call's arguments through
    /// [`show::arguments_lines`], not an inline pretty-print of its
    /// own — the box and an expanded pane entry cannot drift only if
    /// both call the one function. Non-JSON arguments with a trailing
    /// newline distinguish the two: `arguments_lines`'s `.lines()`
    /// drops it (two rows, `"a"` and `"b"`), while pushing the raw
    /// text as one `Vec` entry (the pre-refactor shape) would keep it
    /// embedded in a single row. The JSON case does not distinguish
    /// them (both paths pretty-print identically), so this must be
    /// non-JSON. Mutation: revert `compute_lines`'s `Some(call)` arm
    /// to matching on `serde_json::from_str` directly and pushing
    /// `call.arguments.clone()` in the `Err` case.
    #[test]
    fn compute_lines_renders_arguments_through_show_arguments_lines() {
        let request = ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: Some(ToolCall {
                id: Some("t9".to_string()),
                name: "write".to_string(),
                arguments: "a\nb\n".to_string(),
            }),
            access: Access::WriteFile(std::path::PathBuf::from("/ws/out.txt")),
        };
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let prompt = Prompt::new(request, None, "write /ws/out.txt".to_string(), None, tx);
        let rows = lines(&prompt);
        let idx = rows
            .iter()
            .position(|l| l.ends_with("with these arguments:"))
            .expect("no arguments header");
        assert_eq!(
            &rows[idx + 1..],
            &["a".to_string(), "b".to_string()],
            "{rows:?}"
        );
    }

    /// Guards D4: a prompt whose receiver is already gone (the guard
    /// stays alive; only the receiver is dropped) is answered without
    /// panicking, and the answer is simply discarded: no rule, no
    /// trace. Mutation: `expect` the send in `answer_prompt`.
    #[test]
    fn an_answer_after_the_future_was_dropped_is_ignored() {
        let shared = Shared::new();
        let workspace = std::path::PathBuf::from("/ws");
        shared.update(|ui| ui.workspace = workspace.clone());
        let (tx, rx) = oneshot::channel();
        drop(rx);
        let prompt = Prompt::new(
            write_req(),
            None,
            "write /ws/out.txt".to_string(),
            Some("/ws/out.txt".to_string()),
            tx,
        );
        let _guard = PromptGuard::open(&shared, prompt);
        let armed = shared.lock().prompt_armed_at;
        let handled = shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                &workspace,
                armed,
            )
        });
        assert!(handled);
        let ui = shared.lock();
        assert!(ui.prompts.is_empty());
        assert!(ui.entries.is_empty());
        assert!(ui.session_rules.is_empty());
    }

    /// Guards D3, D4: each key answers exactly as the legend says,
    /// driven through `drive::handle_key` (so `Esc` there must deny
    /// rather than reach the token); an `a`/`d` answer both records a
    /// session rule and survives the future being dropped unpolled
    /// afterward; a prompt removed from under the future by something
    /// other than an answer resolves it to `Deny` with no new trace.
    /// Mutations: swap the `y`/`n` arms; `unwrap` the receive.
    #[test]
    fn each_key_answers_as_the_legend_says() {
        for (code, decision, is_session) in [
            (KeyCode::Char('y'), Decision::Allow, false),
            (KeyCode::Char('n'), Decision::Deny, false),
            (KeyCode::Char('a'), Decision::Allow, true),
            (KeyCode::Char('d'), Decision::Deny, true),
            (KeyCode::Esc, Decision::Deny, false),
        ] {
            let (interactive, shared) = op(empty_policy());
            let mut cx = noop_context();
            let mut fut = interactive.approve(write_req());
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));

            let cancel = CancellationToken::new();
            let armed = shared.lock().prompt_armed_at;
            let _ = crate::tui::drive::handle_key(
                &shared,
                &cancel,
                true,
                Input::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                armed,
            );
            assert!(!cancel.is_cancelled(), "{code:?} reached the token");

            let polled = fut.as_mut().poll(&mut cx);
            assert!(
                matches!(polled, Poll::Ready(d) if d == decision),
                "{code:?}: {polled:?}"
            );

            let suffix = if is_session {
                " for the rest of the session"
            } else {
                ""
            };
            let verb = if decision == Decision::Allow {
                "allowed"
            } else {
                "denied"
            };
            let expected = format!(
                "gwennol: write /ws/out.txt from tool-write (call write t1): {verb} at the prompt{suffix}"
            );
            {
                let ui = shared.lock();
                match ui.entries.last() {
                    Some(Entry::Trace(t)) => assert_eq!(t, &expected),
                    other => panic!("expected a Trace entry, got {other:?}"),
                }
                if is_session {
                    assert_eq!(
                        ui.session_rules,
                        vec![SessionRule {
                            decision,
                            plugin: "tool-write".to_string(),
                            kind: Kind::Write,
                            subject: "/ws/out.txt".to_string(),
                        }]
                    );
                } else {
                    assert!(ui.session_rules.is_empty());
                }
            }
        }

        // The state case: `a`, then drop the future without polling
        // again — the rule and the trace remain regardless.
        {
            let (interactive, shared) = op(empty_policy());
            let mut cx = noop_context();
            let mut fut = interactive.approve(write_req());
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
            let cancel = CancellationToken::new();
            let armed = shared.lock().prompt_armed_at;
            let _ = crate::tui::drive::handle_key(
                &shared,
                &cancel,
                true,
                Input::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
                armed,
            );
            drop(fut);
            let ui = shared.lock();
            assert_eq!(ui.session_rules.len(), 1);
            assert!(matches!(ui.entries.last(), Some(Entry::Trace(_))));
        }

        // Last: poll → Pending, remove the prompt from `prompts` by
        // hand (something other than an answer), poll → Ready(Deny),
        // no new entry.
        {
            let (interactive, shared) = op(empty_policy());
            let mut cx = noop_context();
            let mut fut = interactive.approve(write_req());
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
            let before = shared.lock().entries.len();
            shared.update(|ui| ui.prompts.clear());
            let polled = fut.as_mut().poll(&mut cx);
            assert!(matches!(polled, Poll::Ready(Decision::Deny)), "{polled:?}");
            assert_eq!(shared.lock().entries.len(), before);
        }
    }

    /// Guards D2: a request no session rule can hold (a spawn with
    /// stdin) is decided once and remembers nothing, its key line is
    /// [`KEYS_ONCE`], and the same request prompts again next time.
    /// Mutation: push the rule anyway.
    #[test]
    fn a_request_no_session_rule_can_hold_is_decided_once() {
        let (interactive, shared) = op(empty_policy());
        let request = || ApprovalRequest {
            plugin: "tool-sh".to_string(),
            cause: Some(ToolCall {
                id: Some("t2".to_string()),
                name: "sh".to_string(),
                arguments: r#"{"script":"echo hi\n"}"#.to_string(),
            }),
            access: Access::Spawn {
                argv: vec!["sh".to_string()],
                cwd: std::path::PathBuf::from("/ws"),
                stdin: Some("echo hi\n".to_string()),
            },
        };
        let mut cx = noop_context();
        let mut fut = interactive.approve(request());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));

        let frame = render_frame(&shared);
        for needle in [
            "spawn [\"sh\"] with 8 bytes on stdin",
            "asked by tool-sh",
            "the child will read this on stdin (8 bytes):",
            "echo hi",
            "only a rule of kind any could have matched this: a spawn with stdin",
        ] {
            assert!(
                frame.iter().any(|r| r.contains(needle)),
                "missing {needle:?} in {frame:?}"
            );
        }
        // The full legend, not a prefix `KEYS` also shares: every
        // wrapped row of `KEYS_ONCE` must reach the screen.
        for row in ui::wrap(KEYS_ONCE, INNER_W) {
            assert!(
                frame.iter().any(|r| r.contains(&row)),
                "missing legend row {row:?} in {frame:?}"
            );
        }
        // 10 content rows (access line, "asked by", the model-asked
        // line, the 3-line pretty-printed arguments, the stdin header,
        // the stdin itself, and the unjudgeable note) plus the 2-row
        // `KEYS_ONCE` legend, under the cap: unlike `KEYS`, `KEYS_ONCE`
        // had no height assertion pinning it, so it could be emptied
        // to `""` and the suite would stay green — `ui::wrap` returns
        // `[""]` for an empty string, so the loop above still passes.
        {
            let ui = shared.lock();
            let prompt = &ui.prompts[0];
            assert_eq!(height(prompt, FRAME_W, 24), 14, "10 + 2 + 2, under the cap");
        }
        // The wording itself, pinned by something other than the
        // constant it quotes.
        assert!(
            frame.iter().any(|r| r.contains("cannot be remembered")),
            "{frame:?}"
        );

        let cancel = CancellationToken::new();
        let armed = shared.lock().prompt_armed_at;
        let _ = crate::tui::drive::handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            armed,
        );
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Allow)
        ));
        assert!(shared.lock().session_rules.is_empty());
        match shared.lock().entries.last() {
            Some(Entry::Trace(t)) => assert!(
                t.ends_with("allowed at the prompt"),
                "session suffix leaked for an unremembered request: {t}"
            ),
            other => panic!("expected a Trace entry, got {other:?}"),
        }

        // The same request again: it prompts once more.
        let mut fut2 = interactive.approve(request());
        assert!(matches!(fut2.as_mut().poll(&mut cx), Poll::Pending));
    }

    /// Guards D6: a path that is not valid UTF-8 has no subject
    /// ([`crate::show::subject`]), so a seeded rule for the lossy
    /// rendering of another path cannot decide it — it prompts,
    /// `KEYS_ONCE` legend and all — and answering it records nothing.
    /// Mutation: revert `show::subject`'s path arm (the request would
    /// be decided by the seeded rule, `Ready` at the first poll, no
    /// prompt at all).
    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_decided_once() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let (interactive, shared) = op(empty_policy());
        let seeded = SessionRule {
            decision: Decision::Allow,
            plugin: "tool-write".to_string(),
            kind: Kind::Write,
            subject: "/ws/a\u{FFFD}.txt".to_string(),
        };
        shared.update(|ui| ui.session_rules.push(seeded.clone()));

        let lossy_path = std::path::PathBuf::from(OsString::from_vec(b"/ws/a\xff.txt".to_vec()));
        let request = ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: None,
            access: Access::WriteFile(lossy_path),
        };
        let mut cx = noop_context();
        let mut fut = interactive.approve(request);
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(shared.lock().session_rules, vec![seeded]);

        let frame = render_frame(&shared);
        for row in ui::wrap(KEYS_ONCE, INNER_W) {
            assert!(
                frame.iter().any(|r| r.contains(&row)),
                "missing KEYS_ONCE row {row:?} in {frame:?}"
            );
        }

        let workspace = std::path::PathBuf::from("/ws");
        let armed_at = armed(&shared);
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                &workspace,
                armed_at,
            )
        });
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Allow)
        ));
        assert_eq!(
            shared.lock().session_rules.len(),
            1,
            "nothing new was remembered"
        );
    }

    /// A request whose box has enough rows to scroll: 40 named fields.
    fn long_request() -> ApprovalRequest {
        let mut fields = serde_json::Map::new();
        for i in 0..40 {
            fields.insert(format!("k{i:02}"), serde_json::json!(i));
        }
        ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: Some(ToolCall {
                id: Some("t1".to_string()),
                name: "write".to_string(),
                arguments: serde_json::Value::Object(fields).to_string(),
            }),
            access: Access::WriteFile(std::path::PathBuf::from("/ws/out.txt")),
        }
    }

    /// Guards D1: an unmodified `y`/`n`/`a`/`d` before [`ARM_DELAY`] has
    /// passed since the prompt became the first is swallowed — no
    /// answer, no rule — and sets [`EARLY`], pushing the gate to
    /// `now + ARM_DELAY`; the same letter once `now` has reached the
    /// (new) armed instant answers. Mutation: drop the `now <
    /// ui.prompt_armed_at` check in `key` — the first `a` answers and
    /// records a rule.
    #[test]
    fn an_answer_letter_waits_for_the_keyboard_to_go_quiet() {
        let (interactive, shared) = op(empty_policy());
        let mut cx = noop_context();
        let mut fut = interactive.approve(write_req());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));

        let workspace = std::path::PathBuf::from("/ws");
        let t = armed(&shared);
        let early = t - Duration::from_millis(1);
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                &workspace,
                early,
            )
        });
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Pending),
            "an early letter answered the prompt"
        );
        assert!(shared.lock().session_rules.is_empty());
        assert_eq!(shared.lock().notice.as_deref(), Some(EARLY));
        let pushed = shared.lock().prompt_armed_at;
        assert_eq!(pushed, early + ARM_DELAY);

        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                &workspace,
                pushed,
            )
        });
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Allow)
        ));
        assert_eq!(shared.lock().session_rules.len(), 1);
    }

    /// Guards D2: every key that reaches an open prompt without
    /// answering it — a swallowed early letter, a scroll key — pushes
    /// the gate back, so continuous typing never answers even once the
    /// original delay has passed; a scroll key still scrolls while
    /// doing so. Mutations, named in the PR body: push the gate back
    /// only on letters (the later `a` would answer); return before the
    /// scroll match when early (`scroll` stays 0).
    #[test]
    fn typing_keeps_a_prompt_from_answering() {
        let (interactive, shared) = op(empty_policy());
        let mut cx = noop_context();
        let mut fut = interactive.approve(long_request());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        // `view` (rows, visible) is measured at render; the box's
        // scroll match reads it to bound the key.
        render_frame(&shared);

        let workspace = std::path::PathBuf::from("/ws");
        let t = armed(&shared);
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
                &workspace,
                t - Duration::from_millis(1),
            )
        });
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                &workspace,
                t + Duration::from_millis(300),
            )
        });
        assert_eq!(shared.lock().prompts[0].scroll, 1, "Down did not scroll");
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                &workspace,
                t + Duration::from_millis(600),
            )
        });
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Pending),
            "an `a` after the original delay, but before the pushed-back gate, answered"
        );
        assert!(shared.lock().session_rules.is_empty());

        let pushed = shared.lock().prompt_armed_at;
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &workspace,
                pushed,
            )
        });
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Allow)
        ));
        assert!(shared.lock().session_rules.is_empty());
    }

    /// Guards D2, D3: `Esc` denies at once, never gated, even before
    /// the delay has passed; a second prompt that becomes first this
    /// way still gets its own second before a letter can answer it.
    /// Mutations, named in the PR body: gate `Esc` too (the first
    /// prompt would stay open at `armed - 1ms`); drop the reset in
    /// `answer_prompt` (the second prompt's `y` at the same instant
    /// would answer it).
    #[test]
    fn esc_denies_at_once_and_a_new_first_prompt_waits_its_own_second() {
        let (interactive, shared) = op(empty_policy());
        let mut cx = noop_context();
        let req = |name: &str| ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: None,
            access: Access::WriteFile(std::path::PathBuf::from(format!("/ws/{name}"))),
        };
        let mut first = interactive.approve(req("a.txt"));
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Pending));
        let mut second = interactive.approve(req("b.txt"));
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Pending));

        let workspace = std::path::PathBuf::from("/ws");
        let early = armed(&shared) - Duration::from_millis(1);
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &workspace,
                early,
            )
        });
        assert!(matches!(
            first.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Deny)
        ));
        assert_eq!(
            shared.lock().prompt_armed_at,
            early + ARM_DELAY,
            "the new first prompt did not get its own second"
        );

        // The second prompt is now first, and gets its own second: the
        // same early instant does not answer it.
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &workspace,
                early,
            )
        });
        assert!(
            matches!(second.as_mut().poll(&mut cx), Poll::Pending),
            "the second prompt answered at the first prompt's armed instant"
        );

        let pushed = shared.lock().prompt_armed_at;
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &workspace,
                pushed,
            )
        });
        assert!(matches!(
            second.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Allow)
        ));
    }

    /// Guards D2: the gate moves when the first prompt does (a new one
    /// opening onto an empty queue, or the first being withdrawn with
    /// another behind it) and only then — a second prompt opening
    /// behind the first does not move it. Mutations, named in the PR
    /// body: reset `prompt_armed_at` on every `open` (the seeded
    /// instant would change when the second opens); drop the reset in
    /// `Drop` (the third prompt would not get its own second).
    #[test]
    fn the_gate_moves_when_the_first_prompt_does_and_only_then() {
        let (interactive, shared) = op(empty_policy());
        let mut cx = noop_context();
        let req = |name: &str| ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: None,
            access: Access::WriteFile(std::path::PathBuf::from(format!("/ws/{name}"))),
        };
        let mut first = interactive.approve(req("a.txt"));
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Pending));

        let seeded = Instant::now() - Duration::from_secs(60);
        shared.update(|ui| ui.prompt_armed_at = seeded);

        // A second prompt opening behind the first does not move it.
        let mut second = interactive.approve(req("b.txt"));
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(shared.lock().prompt_armed_at, seeded);

        // Withdrawing the first (something other than an answer) with
        // the second behind it hands the second its own second.
        let before = Instant::now();
        drop(first);
        let after = shared.lock().prompt_armed_at;
        assert!(
            after >= before + ARM_DELAY,
            "the gate did not move when the first prompt was withdrawn: {after:?} vs {before:?}"
        );
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Pending));
    }

    /// Guards D5: the box shows the whole arguments (not a preview)
    /// and scrolls to reach a row past the visible window; the key
    /// line stays put; a request with no cause shows the frontend
    /// sentence instead of a call. Mutation: ignore `scroll` in
    /// `render_prompt`.
    #[test]
    fn the_box_shows_the_whole_arguments_and_scrolls() {
        let mut fields = serde_json::Map::new();
        for i in 0..40 {
            fields.insert(format!("k{i:02}"), serde_json::json!(i));
        }
        let request = ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: Some(ToolCall {
                id: Some("t1".to_string()),
                name: "write".to_string(),
                arguments: serde_json::Value::Object(fields).to_string(),
            }),
            access: Access::WriteFile(std::path::PathBuf::from("/ws/out.txt")),
        };
        let (interactive, shared) = op(empty_policy());
        let mut cx = noop_context();
        let mut fut = interactive.approve(request);
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));

        let workspace = std::path::PathBuf::from("/ws");
        let frame = render_frame(&shared);
        assert!(frame.iter().any(|r| r.contains("\"k00\"")), "{frame:?}");
        assert!(!frame.iter().any(|r| r.contains("\"k39\"")), "{frame:?}");
        // The full legend, not a prefix `KEYS_ONCE` also shares.
        for row in ui::wrap(KEYS, INNER_W) {
            assert!(
                frame.iter().any(|r| r.contains(&row)),
                "missing legend row {row:?} in {frame:?}"
            );
        }
        // D5: "the key line is the block's last inner row, never
        // scrolled" — position, not just membership. The row directly
        // above the box's bottom border is the legend's last wrapped
        // row.
        {
            let border = frame
                .iter()
                .position(|r| r.trim_end().ends_with('┘'))
                .expect("no bottom border in the frame");
            let last_legend_row = ui::wrap(KEYS, INNER_W)
                .last()
                .expect("KEYS wraps to at least one row")
                .clone();
            assert!(
                frame[border - 1].contains(&last_legend_row),
                "last inner row is not the legend's last row: {:?} in {frame:?}",
                frame[border - 1]
            );
        }

        // D5's box-height rule: `min(rows + 2 + legend_rows, max(4,
        // area_height * 2 / 3))`. This prompt's 45 content rows plus
        // its 2-row legend (93 chars at inner width 78) hit the `* 2
        // / 3` cap at a tall terminal and the `max(4)` floor at a
        // short one; neither is reachable through the pane, whose
        // `Min(1)` constraint hands it whatever `height` returns.
        {
            let ui = shared.lock();
            let prompt = &ui.prompts[0];
            assert_eq!(height(prompt, FRAME_W, 24), 16, "the `* 2 / 3` cap, not 49");
            assert_eq!(height(prompt, FRAME_W, 3), 4, "the `max(4)` floor");
        }

        let mut presses = 0;
        loop {
            shared.update(|ui| {
                key(
                    ui,
                    &KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                    &workspace,
                    Instant::now(),
                )
            });
            presses += 1;
            let frame = render_frame(&shared);
            if frame.iter().any(|r| r.contains("\"k39\"")) {
                break;
            }
            assert!(presses <= 10, "k39 never scrolled into view");
        }
        let frame = render_frame(&shared);
        for row in ui::wrap(KEYS, INNER_W) {
            assert!(
                frame.iter().any(|r| r.contains(&row)),
                "missing legend row {row:?} in {frame:?}"
            );
        }

        for _ in 0..100 {
            shared.update(|ui| {
                key(
                    ui,
                    &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    &workspace,
                    Instant::now(),
                )
            });
        }
        let frame = render_frame(&shared);
        assert!(frame.iter().any(|r| r.contains("\"k39\"")), "{frame:?}");

        for _ in 0..100 {
            shared.update(|ui| {
                key(
                    ui,
                    &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                    &workspace,
                    Instant::now(),
                )
            });
        }
        let frame = render_frame(&shared);
        assert!(frame.iter().any(|r| r.contains("\"k00\"")), "{frame:?}");

        // A request with no cause: the frontend-started sentence,
        // not a call.
        let no_cause = ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: None,
            access: Access::WriteFile(std::path::PathBuf::from("/ws/other.txt")),
        };
        let (interactive2, shared2) = op(empty_policy());
        let mut fut3 = interactive2.approve(no_cause);
        assert!(matches!(fut3.as_mut().poll(&mut cx), Poll::Pending));
        let frame = render_frame(&shared2);
        assert!(
            frame.iter().any(|r| r.contains("started by the frontend")),
            "{frame:?}"
        );
        // This prompt's 3 content rows (access line, "asked by", the
        // no-cause sentence) plus its 2-row legend sit under both
        // caps: `rows + 2 + legend_rows`, not `max(4, area_height * 2
        // / 3) = 16`.
        {
            let ui = shared2.lock();
            let prompt = &ui.prompts[0];
            assert_eq!(height(prompt, FRAME_W, 24), 7, "3 + 2 + 2, under the cap");
        }
    }

    /// Guards D5: the box reserves at least one content row before
    /// the legend, so a short terminal never shows the key legend
    /// alone with nothing to approve, and it never drops the legend
    /// entirely once there's room for it. Two mutations, one per
    /// assertion: drop the `.max(1)` in `render_prompt`'s `visible` —
    /// the access line disappears at 80x7 (`inner.height == 2`,
    /// `legend.len() == 2`, `visible == 0`); or force `legend_budget`
    /// to 0 once the legend cannot fit — the legend disappears instead.
    #[test]
    fn a_short_terminal_still_shows_the_access_line_behind_the_legend() {
        let (interactive, shared) = op(empty_policy());
        let mut cx = noop_context();
        let mut fut = interactive.approve(write_req());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));

        let frame = render_frame_sized(&shared, FRAME_W, 7);
        assert!(
            frame.iter().any(|r| r.contains("write /ws/out.txt")),
            "no access line on screen at 80x7: {frame:?}"
        );
        let first_legend_row = ui::wrap(KEYS, INNER_W)[0].clone();
        assert!(
            frame.iter().any(|r| r.contains(&first_legend_row)),
            "no legend at all on screen at 80x7: {frame:?}"
        );
    }

    /// Guards D3: the first prompt is shown and answered first; the
    /// second waits behind it, unaffected, until the first is
    /// answered. Mutation: `PromptGuard::open` replaces the vec's
    /// contents instead of pushing.
    #[test]
    fn a_second_prompt_waits_behind_the_first() {
        let (interactive, shared) = op(empty_policy());
        let req = |name: &str| ApprovalRequest {
            plugin: "tool-write".to_string(),
            cause: None,
            access: Access::WriteFile(std::path::PathBuf::from(format!("/ws/{name}"))),
        };
        let mut cx = noop_context();
        let mut first = interactive.approve(req("out.txt"));
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Pending));
        let mut second = interactive.approve(req("two.txt"));
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Pending));

        {
            let ui = shared.lock();
            assert_eq!(ui.prompts.len(), 2);
            assert_eq!(ui.prompts[0].id, 1);
            assert_eq!(ui.prompts[1].id, 2);
        }
        let frame = render_frame(&shared);
        assert!(frame.iter().any(|r| r.contains("out.txt")), "{frame:?}");
        assert!(!frame.iter().any(|r| r.contains("two.txt")), "{frame:?}");

        let workspace = std::path::PathBuf::from("/ws");
        let armed = shared.lock().prompt_armed_at;
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &workspace,
                armed,
            )
        });
        assert!(matches!(
            first.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Allow)
        ));
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Pending));
        let frame = render_frame(&shared);
        assert!(frame.iter().any(|r| r.contains("two.txt")), "{frame:?}");

        let armed = shared.lock().prompt_armed_at;
        shared.update(|ui| {
            key(
                ui,
                &KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                &workspace,
                armed,
            )
        });
        assert!(matches!(
            second.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Deny)
        ));
    }
}
