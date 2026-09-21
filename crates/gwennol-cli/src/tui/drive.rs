//! The loop: idle until a turn is submitted, then drive
//! [`Session::turn`] itself, one turn at a time (D7), keys always
//! polled before the turn future or a redraw so a queued `/exit`
//! is never left behind. `Session::run` is never called: it stops at
//! the first turn that does not complete, and a session must carry on
//! past a failed or cancelled one. Keys go to an open approval prompt
//! before anything else typed reaches the editor or the token (a
//! Ctrl-C notice is still retired first; a resize or a read error
//! never reaches the prompt at all, and a paste is dropped rather
//! than routed to it), so `Esc` there denies rather than cancels; the
//! pane's own keys (paging, focus, expand: [`pane`]) come next, and
//! the editor last.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use gwennol_core::gwead::tokio_util::sync::CancellationToken;
use gwennol_core::{Session, TurnError};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::watch;

use crate::show::outcome_line;
use crate::tui::editor::{Command, Submission};
use crate::tui::keys::{Input, KeySource};
use crate::tui::ui::{Entry, Shared, TurnState, render};
use crate::tui::{pane, prompt};
use crate::{EXIT_CANCELLED, EXIT_TURN_FAILED, Fatal};

/// What handling one key means for the loop driving it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Idle only: `Enter` on a turn's text.
    Submit(String),
    /// Idle `/exit`, or the key source closing while idle: `drive`
    /// returns.
    ExitIdle,
    /// A second `/exit` while a cancel from the first is still
    /// pending: leave at once, status 130, dropping the turn future.
    ForceExit,
}

/// Apply one key (or paste, or resize) to the editor and the pane,
/// `running` saying whether a turn is in flight. The only place `Esc`
/// or a `/exit` reaches [`CancellationToken::cancel`] — and it does
/// not while a prompt is open: keys go to the prompt first, so `Esc`
/// there denies rather than cancels, and nothing typed during a
/// prompt reaches the editor; the pane's keys are taken before the
/// editor's, so `Home` on an empty editor scrolls rather than moving a
/// cursor that has nowhere to go. `pub(crate)` so `tui::prompt`'s own
/// tests can drive a key through the same entry point `drive` uses,
/// rather than a copy of its prompt-routing branch.
pub(crate) fn handle_key(
    shared: &Shared,
    cancel: &CancellationToken,
    running: bool,
    input: Input,
) -> Option<Action> {
    let mut action = None;
    shared.update(|ui| {
        let key = match input {
            // Touches neither `scroll` nor `focus`: a resize that
            // shrinks `max_top` past a `Some(top)` a previous `reveal`
            // left in place never reaches `pane::reveal` to fix it up;
            // `render_pane`'s own clamp (ui.rs) is what keeps the
            // pane in range until something rewrites `ui.scroll`: a
            // pane key, or the `follow_tail` a submitted turn or
            // `/help` does.
            Input::Resize => return,
            Input::Paste(text) => {
                // A prompt swallows a paste too, dropped rather than
                // routed to it (never reaching `prompt::key`, unlike
                // a key, so it can never answer or scroll a prompt):
                // pasted text must not accumulate behind an open
                // prompt, silently, for the editor to submit later.
                if ui.prompts.is_empty() {
                    ui.editor.paste(&text);
                }
                return;
            }
            Input::Errored(message) => {
                ui.push(Entry::Trace(format!(
                    "gwennol: reading input failed: {message}"
                )));
                return;
            }
            Input::Key(key) => key,
        };
        // Only an actual key input retires a standing notice: a
        // resize or a paste must not clear a Ctrl-C hint the user has
        // not yet read.
        ui.notice = None;
        if !ui.prompts.is_empty() {
            let workspace = ui.workspace.clone();
            prompt::key(ui, &key, &workspace);
            return;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Esc {
            if running {
                cancel.cancel();
            }
            return;
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            ui.notice = Some("Esc cancels the turn, /exit leaves".to_string());
            return;
        }
        if pane::key(ui, &key) {
            return;
        }
        if !ui.editor.key(&key) {
            return;
        }
        match ui.editor.submission() {
            Submission::Nothing => {}
            Submission::Turn(text) => {
                if running {
                    ui.notice = Some("a turn is running; Esc cancels it".to_string());
                } else {
                    ui.editor.commit();
                    ui.follow_tail();
                    action = Some(Action::Submit(text));
                }
            }
            Submission::Command(Command::Exit) => {
                // Committed like any recognized command (`Help` does
                // the same): a pending exit still reads more keys
                // while the turn unwinds, and a second `/exit` typed
                // fresh must parse as `/exit`, not as `/exit` with the
                // first one's text still ahead of it.
                ui.editor.commit();
                if !running {
                    action = Some(Action::ExitIdle);
                } else if ui.exiting {
                    action = Some(Action::ForceExit);
                } else {
                    ui.exiting = true;
                    cancel.cancel();
                }
            }
            Submission::Command(Command::Help) => {
                for line in super::ui::HELP {
                    ui.push(Entry::Trace((*line).to_string()));
                }
                ui.editor.commit();
                ui.follow_tail();
            }
            Submission::Command(Command::Unknown(word)) => {
                ui.notice = Some(format!("unknown command: {word}; /help lists them"));
            }
        }
    });
    action
}

fn draw<B: Backend>(shared: &Shared, terminal: &mut Terminal<B>) -> Result<(), Fatal> {
    terminal
        .draw(|f| render(&shared.lock(), f))
        .map_err(|e| Fatal(format!("terminal: {e}")))?;
    Ok(())
}

/// One idle iteration: keys first, else a redraw on `changed`, then
/// draw. No turn is running, so there is nothing else to poll.
async fn idle_step<B: Backend, K: KeySource>(
    shared: &Arc<Shared>,
    terminal: &mut Terminal<B>,
    keys: &mut K,
    changes: &mut watch::Receiver<u64>,
) -> Result<Option<Action>, Fatal> {
    // No turn is running, so nothing this call does can reach a
    // `cancel()`; the token exists only so `handle_key`'s signature is
    // the same in both loops.
    let scratch = CancellationToken::new();
    let action = tokio::select! {
        biased;
        key = keys.next() => match key {
            Some(input) => handle_key(shared, &scratch, false, input),
            None => Some(Action::ExitIdle),
        },
        // `Err` only once every sender has dropped, permanently; `Ok`
        // here (rather than matching either) means that happening
        // disables this arm instead of winning every later poll.
        Ok(_) = changes.changed() => None,
    };
    draw(shared, terminal)?;
    Ok(action)
}

fn exit_code(last_failed: bool) -> ExitCode {
    if last_failed {
        ExitCode::from(EXIT_TURN_FAILED)
    } else {
        ExitCode::SUCCESS
    }
}

/// Drive `session` from `keys` and `first` until the session ends.
/// Exit status: 130 forced; else 1 when the last turn of this call
/// failed (an `Err` that is not `Cancelled`); else 0.
pub async fn drive<B: Backend, K: KeySource>(
    session: &mut Session,
    shared: &Arc<Shared>,
    terminal: &mut Terminal<B>,
    keys: &mut K,
    first: Option<String>,
) -> Result<ExitCode, Fatal> {
    let mut changes = shared.changed.subscribe();
    let mut pending = first;
    let mut last_failed = false;
    draw(shared, terminal)?;

    loop {
        // ---- idle: await a submission.
        let text = loop {
            if let Some(text) = pending.take() {
                break text;
            }
            match idle_step(shared, terminal, keys, &mut changes).await? {
                Some(Action::Submit(text)) => break text,
                Some(Action::ExitIdle) => return Ok(exit_code(last_failed)),
                Some(Action::ForceExit) | None => {}
            }
        };

        // ---- running: the turn beside keys, redraws and a tick.
        shared.update(|ui| {
            ui.push(Entry::User(text.clone()));
            ui.turn = TurnState::Working {
                since: Instant::now(),
            };
        });
        let cancel = CancellationToken::new();
        let fut = session.turn(&text, &cancel);
        tokio::pin!(fut);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.tick().await; // the first tick fires immediately; consumed so later ones are spaced.

        let outcome = loop {
            tokio::select! {
                biased;
                key = keys.next() => {
                    match key {
                        Some(input) => {
                            if handle_key(shared, &cancel, true, input) == Some(Action::ForceExit) {
                                return Ok(ExitCode::from(EXIT_CANCELLED));
                            }
                            draw(shared, terminal)?;
                        }
                        None => {
                            // The source closed: as a running `/exit`,
                            // but there is nothing left to poll for, so
                            // the turn is awaited directly rather than
                            // through a select a closed source would
                            // otherwise always win.
                            shared.update(|ui| ui.exiting = true);
                            cancel.cancel();
                            break fut.await;
                        }
                    }
                }
                // See `idle_step`: `Ok` only, so a permanently
                // dropped sender disables this arm instead of
                // spinning it at 100% CPU.
                Ok(_) = changes.changed() => { draw(shared, terminal)?; }
                _ = tick.tick() => {
                    shared.update(|ui| ui.tick += 1);
                    draw(shared, terminal)?;
                }
                r = &mut fut => break r,
            }
        };

        let (line, _) = outcome_line(&outcome);
        last_failed = matches!(&outcome, Err(e) if !matches!(e, TurnError::Cancelled { .. }));
        let exiting = shared.update(|ui| {
            ui.push(Entry::Outcome(line));
            ui.turn = TurnState::Idle;
            std::mem::take(&mut ui.exiting)
        });
        draw(shared, terminal)?;
        if exiting {
            return Ok(exit_code(last_failed));
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEvent;
    use ratatui::backend::TestBackend;

    use super::*;

    /// Guards D7: `Esc` fires the token only while a turn is running,
    /// and leaves the editor alone either way (every key input clears
    /// the notice, Esc included, but a resize or a paste must not);
    /// `Alt+b` reaches the editor; `Ctrl-C` sets the notice. Mutation:
    /// drop the `Esc` arm.
    #[test]
    fn esc_cancels_a_running_turn_and_nothing_else() {
        let shared = Shared::new();
        shared.update(|ui| {
            ui.editor.paste("hello");
        });
        let cancel = CancellationToken::new();

        // Idle: Esc does nothing to the token or the editor.
        handle_key(
            &shared,
            &cancel,
            false,
            Input::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(!cancel.is_cancelled());
        assert_eq!(shared.lock().editor.text(), "hello");

        // Running: Esc fires the token.
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(cancel.is_cancelled());
        assert_eq!(shared.lock().editor.text(), "hello");

        // Alt+b reaches the editor: the cursor moves back a word.
        let before = shared.lock().editor.cursor();
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
        );
        assert!(shared.lock().editor.cursor() < before);

        // Ctrl-C sets the notice.
        assert!(shared.lock().notice.is_none());
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert_eq!(
            shared.lock().notice.as_deref(),
            Some("Esc cancels the turn, /exit leaves")
        );

        // A resize or a paste must not clear a standing notice; only
        // a key input does. Mutation: clear `ui.notice` before
        // discriminating `input` — both assertions below fail.
        handle_key(&shared, &cancel, true, Input::Resize);
        assert!(
            shared.lock().notice.is_some(),
            "a resize cleared the notice"
        );
        handle_key(&shared, &cancel, true, Input::Paste("x".to_string()));
        assert!(shared.lock().notice.is_some(), "a paste cleared the notice");
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        );
        assert!(
            shared.lock().notice.is_none(),
            "a key input did not clear the notice"
        );
    }

    /// Guards D4: while a prompt is open, every key is the prompt's —
    /// `Esc` denies rather than reaching the token, typing `/exi` never
    /// reaches the editor, a recognized `/exit` + Enter still never sets
    /// `exiting`, a bracketed paste never reaches the editor either, and
    /// Ctrl-C sets no notice — until the prompt itself is answered.
    /// Neither the `/exi` block nor the `/exit` block discriminates the
    /// prompt-routing-arm mutation on its own; the `Esc` assertion above
    /// them does, and each block's own comment says what it is for.
    /// Mutations: drop the prompt-routing arm in `handle_key` (the key
    /// case); drop the `ui.prompts.is_empty()` guard around
    /// `ui.editor.paste` (the paste case).
    #[test]
    fn keys_go_to_an_open_prompt_never_to_the_editor_or_the_token() {
        use std::path::Path;
        use std::task::{Context, Poll, Waker};

        use gwennol_core::{Decision, Operator};

        use crate::tui::prompt::tests::{op, write_req};

        let (interactive, shared) =
            op(crate::policy::Policy::compile(Vec::new(), Path::new("/ws")).unwrap());
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        let mut fut = interactive.approve(write_req());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        let cancel = CancellationToken::new();
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(
            !cancel.is_cancelled(),
            "Esc reached the token while a prompt was open"
        );
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Deny)
        ));

        // A second prompt: typing past it reaches neither the editor
        // nor `exiting`, and it is still open afterward. "/exi", not
        // "/exit": a fix that intercepted only recognized commands
        // would still leave `editor.text()` empty for "/exit", since
        // `Command::Exit`'s own handling always calls `commit()`; "/exi"
        // is `Command::Unknown`, whose arm never commits, so it is the
        // one that would catch a fix narrowed to recognized commands
        // instead of every key. It is not independently
        // mutation-sensitive to the prompt-routing arm itself, though:
        // dropping that arm fails earlier, at this test's `Esc`
        // assertion above, before this block or the "/exit" block below
        // ever runs.
        let mut fut2 = interactive.approve(write_req());
        assert!(matches!(fut2.as_mut().poll(&mut cx), Poll::Pending));

        // A bracketed paste is swallowed the same as a key: it must
        // not reach the editor while a prompt is open, and it must
        // not answer or scroll the prompt either (the `Paste` arm
        // returns before the key-routing branch is ever reached).
        handle_key(&shared, &cancel, true, Input::Paste("rm -rf /".to_string()));
        assert_eq!(
            shared.lock().editor.text(),
            "",
            "a paste reached the editor while a prompt was open"
        );
        assert_eq!(
            shared.lock().prompts.len(),
            1,
            "a paste answered or closed the prompt"
        );

        for c in "/exi".chars() {
            handle_key(
                &shared,
                &cancel,
                true,
                Input::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        let action = handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(
            action, None,
            "typed text reached the editor's own submission while a prompt was open"
        );
        assert_eq!(
            shared.lock().editor.text(),
            "",
            "typed text reached the editor while a prompt was open"
        );
        assert!(!shared.lock().exiting);
        assert_eq!(
            shared.lock().prompts.len(),
            1,
            "the prompt closed on its own"
        );

        // A whole recognized command, typed the same way: `/exit` +
        // Enter still never sets `exiting` while the prompt is open.
        // "/exi" above cannot check this — it parses to
        // `Command::Unknown`, which has no path to `exiting` at all —
        // so this is the only assertion that a *recognized* command
        // cannot take effect behind a prompt, though it is not
        // independently mutation-sensitive: the sole production guard
        // is the prompt-routing arm above, and dropping it fails at
        // this test's earlier `Esc` assertion, before this block ever
        // runs. The buffer is cleared first: "/exi" left uncommitted
        // (`Command::Unknown` never commits) would otherwise glue onto
        // "/exit" as "/exi/exit", which also parses to `Unknown` and
        // never reaches `exiting` regardless of whether the
        // prompt-routing arm in `handle_key` is reverted.
        shared.update(|ui| ui.editor.commit());
        for c in "/exit".chars() {
            handle_key(
                &shared,
                &cancel,
                true,
                Input::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(
            !shared.lock().exiting,
            "a recognized /exit reached exiting while a prompt was open"
        );
        assert_eq!(
            shared.lock().prompts.len(),
            1,
            "the prompt closed on its own"
        );

        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(
            shared.lock().notice.is_none(),
            "Ctrl-C set a notice while a prompt was open"
        );

        // The pane's own keys are swallowed the same way: with a
        // prompt open, `PageUp` and `Tab` change neither `scroll` nor
        // `focus`. A long entry and a render first, so `pane_view` is
        // not still zeroed: `PageUp` reaching the pane from this state
        // would otherwise compute `None` regardless of the gate. The
        // `ToolResult` pushed just below gives `Tab` an expandable
        // entry to find instead of the `None` `older` returns from a
        // transcript with nothing expandable in it, so it too would
        // move `focus` if the gate did not hold, and the mutation
        // below would go uncaught.
        shared.update(|ui| {
            let long: String = (0..100)
                .map(|n| format!("line{n}"))
                .collect::<Vec<_>>()
                .join(" ");
            ui.push(Entry::Assistant(long));
            ui.push(Entry::ToolResult {
                call: gwennol_core::ToolCall {
                    id: None,
                    name: "r".to_string(),
                    arguments: "{}".to_string(),
                },
                content: "c".to_string(),
                is_error: false,
                expanded: false,
            });
        });
        {
            let backend = TestBackend::new(20, 6);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| render(&shared.lock(), f)).unwrap();
        }
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        );
        assert_eq!(
            shared.lock().scroll,
            None,
            "PageUp reached the pane while a prompt was open"
        );
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        assert_eq!(
            shared.lock().focus,
            None,
            "Tab reached the pane while a prompt was open"
        );

        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        );
        assert!(matches!(
            fut2.as_mut().poll(&mut cx),
            Poll::Ready(Decision::Deny)
        ));
    }

    /// Guards the forced-exit path directly and deterministically. The
    /// end-to-end double-`/exit` scenario in `tests/interactive.rs`
    /// does not pin `biased` in the running loop's `select!`: with it
    /// removed, the key and the cancelled turn's own completion race,
    /// and the mutant survives most runs (see run H's own comment).
    /// With a cancel already pending from a first `/exit`, a second one
    /// returns `ForceExit` here regardless of any scheduling.
    /// Mutation: swap the branches of the `ui.exiting` check
    /// (`ForceExit` when *not* already exiting) — the *first* assertion
    /// below fails: with no cancel pending yet, the first `/exit`
    /// returns `Some(ForceExit)` instead of `None`.
    #[test]
    fn a_second_exit_forces_the_session_out_while_the_first_is_pending() {
        let shared = Shared::new();
        let cancel = CancellationToken::new();
        let submit_exit = |shared: &Shared, cancel: &CancellationToken| {
            for c in "/exit".chars() {
                handle_key(
                    shared,
                    cancel,
                    true,
                    Input::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
                );
            }
            handle_key(
                shared,
                cancel,
                true,
                Input::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            )
        };

        let first = submit_exit(&shared, &cancel);
        assert_eq!(
            first, None,
            "the first /exit while running returns no Action"
        );
        assert!(shared.lock().exiting, "the first /exit did not set exiting");
        assert!(cancel.is_cancelled(), "the first /exit did not cancel");

        let second = submit_exit(&shared, &cancel);
        assert_eq!(second, Some(Action::ForceExit));
    }

    /// Guards `Input::Errored`: a read error is traced (rather than
    /// folded into `None`, the same as the source closing, and said
    /// nowhere) and `handle_key` returns no `Action`, so the loop keeps
    /// running rather than treating the trace as a hidden exit.
    /// Mutation: fold `Input::Errored` into the `Input::Resize` arm
    /// (silently discarded) — the first assertion below fails.
    #[test]
    fn a_read_error_is_traced_not_silently_treated_as_closed() {
        let shared = Shared::new();
        let cancel = CancellationToken::new();
        let action = handle_key(
            &shared,
            &cancel,
            false,
            Input::Errored("the terminal went away".to_string()),
        );
        assert!(
            shared
                .lock()
                .entries
                .iter()
                .any(|e| matches!(e, Entry::Trace(t) if t.contains("the terminal went away"))),
            "the read error was not traced: {:?}",
            shared.lock().entries
        );
        assert_eq!(
            action, None,
            "a read error was treated as an action instead of just a trace"
        );
    }

    /// Guards the "a turn is running; Esc cancels it" branch:
    /// submitting a turn while one is already running sets the notice,
    /// returns no `Action`, and leaves the editor's text in place
    /// rather than discarding it. Mutation: replace the whole
    /// `Submission::Turn` arm with an unconditional commit-and-submit
    /// — the three assertions below fail.
    #[test]
    fn a_turn_typed_while_one_is_running_is_not_swallowed() {
        let shared = Shared::new();
        let cancel = CancellationToken::new();
        for c in "another turn".chars() {
            handle_key(
                &shared,
                &cancel,
                true,
                Input::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        let action = handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(action, None, "a turn was submitted while one was running");
        assert_eq!(
            shared.lock().notice.as_deref(),
            Some("a turn is running; Esc cancels it")
        );
        assert_eq!(
            shared.lock().editor.text(),
            "another turn",
            "the editor was cleared instead of keeping the swallowed text"
        );
    }

    /// Guards D7's redraw arm: an update from another task, with no
    /// key ever arriving, still causes a redraw. Mutation: drop the
    /// `changed()` arm — the step waits on keys forever and the
    /// 10s timeout below fails the test (the one unit pin bound by
    /// time).
    #[tokio::test]
    async fn an_update_from_another_task_redraws() {
        let shared = Shared::new();
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
        let mut changes = shared.changed.subscribe();
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        let updater = {
            let shared = shared.clone();
            tokio::spawn(async move {
                shared.update(|ui| ui.push(Entry::Trace("gwennol: from another task".to_string())));
            })
        };

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            idle_step(&shared, &mut terminal, &mut rx, &mut changes),
        )
        .await
        .expect("idle_step timed out waiting for a key that never came")
        .unwrap();
        assert_eq!(result, None);
        updater.await.unwrap();

        let buf = terminal.backend().buffer();
        let mut found = false;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            // The pushed entry's text word-wraps across rows at this
            // width, so look for a word from it rather than the whole
            // phrase.
            if row.contains("another") {
                found = true;
            }
        }
        assert!(found, "the redraw did not show the pushed entry");
    }

    /// Guards `idle_step`'s own `biased;` (distinct from the running
    /// loop's, whose mutant no test kills reliably — see run H's comment
    /// in `tests/interactive.rs`): with a key and a pending redraw both
    /// ready, the key arm runs first, reaching the editor. Racy on its
    /// own, as run H is: without `biased;`, `tokio::select!`'s own per-call
    /// rotation still sometimes starts at the key arm anyway (about
    /// half of local runs of the mutation below still green), so the
    /// check runs inside a 20-iteration loop with fresh state each
    /// time, which takes the mutant's survival from roughly 0.5 per
    /// run to about 1e-6. Mutation: remove `biased;` from `idle_step`'s
    /// `select!`.
    #[tokio::test]
    async fn idle_step_handles_a_ready_key_before_a_ready_change() {
        for _ in 0..20 {
            let shared = Shared::new();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
            let mut changes = shared.changed.subscribe();
            tx.send(Input::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            )))
            .unwrap();
            // A pending redraw, ready alongside the key above.
            shared.update(|ui| ui.push(Entry::Trace("gwennol: bump".to_string())));
            let backend = TestBackend::new(20, 6);
            let mut terminal = Terminal::new(backend).unwrap();
            idle_step(&shared, &mut terminal, &mut rx, &mut changes)
                .await
                .unwrap();
            assert_eq!(
                shared.lock().editor.text(),
                "a",
                "the ready redraw was handled before the ready key"
            );
        }
    }
}
