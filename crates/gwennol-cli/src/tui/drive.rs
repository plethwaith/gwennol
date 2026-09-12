//! The loop: idle until a turn is submitted, then drive
//! [`Session::turn`] itself, one turn at a time (D7), keys always
//! polled before the turn future or a redraw so a queued `/exit`
//! is never left behind. `Session::run` is never called: it stops at
//! the first turn that does not complete, and a session must carry on
//! past a failed or cancelled one.

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
use crate::{EXIT_CANCELLED, EXIT_TURN_FAILED, Fatal};

/// What handling one key means for the loop driving it.
#[derive(Debug, PartialEq, Eq)]
enum Action {
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
/// or a `/exit` reaches [`CancellationToken::cancel`].
fn handle_key(
    shared: &Shared,
    cancel: &CancellationToken,
    running: bool,
    input: Input,
) -> Option<Action> {
    let mut action = None;
    shared.update(|ui| {
        ui.notice = None;
        let key = match input {
            Input::Resize => return,
            Input::Paste(text) => {
                ui.editor.paste(&text);
                return;
            }
            Input::Key(key) => key,
        };
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
            }
            Submission::Command(Command::Unknown(word)) => {
                ui.notice = Some(format!("unknown command: {word}; /help lists them"));
            }
        }
    });
    action
}

fn draw<B: Backend>(shared: &Shared, terminal: &mut Terminal<B>) -> Result<(), Fatal> {
    terminal.draw(|f| render(&shared.lock(), f))?;
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
        _ = changes.changed() => None,
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
                _ = changes.changed() => { draw(shared, terminal)?; }
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
    /// and leaves the editor and notice alone either way; `Alt+b`
    /// reaches the editor; `Ctrl-C` sets the notice. Mutation: drop
    /// the `Esc` arm.
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
    }

    /// Guards D7's redraw arm: an update from another task, with no
    /// key ever arriving, still causes a redraw. Mutation: drop the
    /// `changed()` arm — the step waits on keys forever and the
    /// timeout fails the test (named in the PR body as the one unit
    /// pin bound by time).
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
}
