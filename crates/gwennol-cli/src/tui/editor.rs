//! The line editor: a `char` buffer and cursor (never bytes, so
//! Unicode is never split mid-character), whitespace-delimited words,
//! history on Up/Down with the in-progress draft kept, and the
//! bindings table the issue specifies, verbatim — the spellings
//! terminals disagree on (`Alt+b`, `Ctrl+Left`, …) are all bound to the
//! same operation. Enter submits; the submitted text is classified
//! before the loop ever sees it: empty is nothing, a `/word` is a
//! command, anything else is a turn, so nothing starting with `/`
//! reaches the model.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The editor's buffer, cursor and history.
#[derive(Debug, Clone, Default)]
pub struct Editor {
    text: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// `Some(i)` while Up/Down is walking `history[i]`; `None` while
    /// editing a fresh line.
    browsing: Option<usize>,
    /// The line being composed when Up first walks into history, so
    /// Down can restore it.
    draft: Vec<char>,
}

/// What Enter on the current text means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// Empty or whitespace-only: nothing happens.
    Nothing,
    /// A `/`-prefixed line.
    Command(Command),
    /// Anything else: the next turn.
    Turn(String),
}

/// A slash command, classified by its first word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/exit`.
    Exit,
    /// `/help`.
    Help,
    /// Any other first word, carried as typed (with its leading `/`).
    Unknown(String),
}

impl Editor {
    /// Apply one key. Returns `true` on a bare Enter; every other
    /// binding edits the buffer (or does nothing) and returns `false`.
    pub fn key(&mut self, key: &KeyEvent) -> bool {
        let none = key.modifiers.is_empty();
        let alt = key.modifiers == KeyModifiers::ALT;
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        // Shift alone does not change what a character key does: the
        // char itself already reflects the case.
        let plain_char = key.modifiers - KeyModifiers::SHIFT == KeyModifiers::NONE;

        match key.code {
            KeyCode::Enter if none => return true,
            KeyCode::Char(c) if plain_char => self.insert(c),
            KeyCode::Char('b') if alt => self.word_left(),
            KeyCode::Char('f') if alt => self.word_right(),
            KeyCode::Char('w') if ctrl => self.delete_word_left(),
            KeyCode::Char('d') if alt => self.delete_word_right(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Backspace if none => self.delete_left(),
            KeyCode::Backspace if alt || ctrl => self.delete_word_left(),
            KeyCode::Delete if none => self.delete_right(),
            KeyCode::Delete if alt => self.delete_word_right(),
            KeyCode::Left if none => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Left if alt || ctrl => self.word_left(),
            KeyCode::Right if none => self.cursor = (self.cursor + 1).min(self.text.len()),
            KeyCode::Right if alt || ctrl => self.word_right(),
            KeyCode::Home if none => self.cursor = 0,
            KeyCode::End if none => self.cursor = self.text.len(),
            KeyCode::Up if none => self.history_up(),
            KeyCode::Down if none => self.history_down(),
            _ => {}
        }
        false
    }

    /// A bracketed paste: newlines become spaces, and it never submits.
    pub fn paste(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' || c == '\r' {
                self.insert(' ');
            } else {
                self.insert(c);
            }
        }
    }

    /// The trimmed text, classified.
    pub fn submission(&self) -> Submission {
        let text: String = self.text.iter().collect();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Submission::Nothing;
        }
        match trimmed.strip_prefix('/') {
            Some(rest) => {
                let first = rest.split_whitespace().next().unwrap_or("");
                let word = format!("/{first}");
                match word.as_str() {
                    "/exit" => Submission::Command(Command::Exit),
                    "/help" => Submission::Command(Command::Help),
                    _ => Submission::Command(Command::Unknown(word)),
                }
            }
            None => Submission::Turn(trimmed.to_string()),
        }
    }

    /// Push the current text to history (unless it equals the last
    /// entry) and clear the buffer. Called when a turn is sent or
    /// `/help` runs.
    pub fn commit(&mut self) {
        let text: String = self.text.iter().collect();
        if self.history.last() != Some(&text) {
            self.history.push(text);
        }
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
        self.draft.clear();
    }

    /// The buffer's text.
    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    /// The cursor, in `char`s from the start.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += 1;
        self.browsing = None;
    }

    fn delete_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    fn delete_right(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    /// The index a leftward word-skip lands on: past whitespace, then
    /// past the non-whitespace run before it.
    fn word_left_index(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.text[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.text[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// The index a rightward word-skip lands on: past the
    /// non-whitespace run at the cursor, then past the whitespace
    /// after it.
    fn word_right_index(&self) -> usize {
        let mut i = self.cursor;
        let len = self.text.len();
        while i < len && !self.text[i].is_whitespace() {
            i += 1;
        }
        while i < len && self.text[i].is_whitespace() {
            i += 1;
        }
        i
    }

    fn word_left(&mut self) {
        self.cursor = self.word_left_index();
    }

    fn word_right(&mut self) {
        self.cursor = self.word_right_index();
    }

    fn delete_word_left(&mut self) {
        let start = self.word_left_index();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    fn delete_word_right(&mut self) {
        let end = self.word_right_index();
        self.text.drain(self.cursor..end);
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.browsing {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browsing = Some(next);
        self.text = self.history[next].chars().collect();
        self.cursor = self.text.len();
    }

    fn history_down(&mut self) {
        match self.browsing {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.browsing = Some(i + 1);
                self.text = self.history[i + 1].chars().collect();
                self.cursor = self.text.len();
            }
            Some(_) => {
                self.browsing = None;
                self.text = std::mem::take(&mut self.draft);
                self.cursor = self.text.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn type_text(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            editor.key(&key(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// Guards D9: every spelling of a word-navigation operation lands
    /// the same as its siblings, from `héllo wörld  end` with the
    /// cursor at the end. History and paste are covered alongside since
    /// they share the fixture. Mutation: drop one spelling
    /// (`Char('b')+ALT`) — its row fails.
    #[test]
    fn word_navigation_is_bound_under_every_spelling() {
        let base = "héllo wörld  end";
        let groups: &[&[(KeyCode, KeyModifiers)]] = &[
            &[
                (KeyCode::Left, KeyModifiers::ALT),
                (KeyCode::Left, KeyModifiers::CONTROL),
                (KeyCode::Char('b'), KeyModifiers::ALT),
            ],
            &[
                (KeyCode::Right, KeyModifiers::ALT),
                (KeyCode::Right, KeyModifiers::CONTROL),
                (KeyCode::Char('f'), KeyModifiers::ALT),
            ],
            &[
                (KeyCode::Backspace, KeyModifiers::ALT),
                (KeyCode::Backspace, KeyModifiers::CONTROL),
                (KeyCode::Char('w'), KeyModifiers::CONTROL),
            ],
            &[
                (KeyCode::Char('d'), KeyModifiers::ALT),
                (KeyCode::Delete, KeyModifiers::ALT),
            ],
        ];
        for group in groups {
            let mut results = Vec::new();
            for (code, modifiers) in *group {
                let mut editor = Editor::default();
                type_text(&mut editor, base);
                editor.key(&key(*code, *modifiers));
                results.push((editor.text(), editor.cursor()));
            }
            let first = &results[0];
            for (i, r) in results.iter().enumerate() {
                assert_eq!(r, first, "row {i} of group {group:?} diverged");
            }
        }

        // Line start/end.
        let mut editor = Editor::default();
        type_text(&mut editor, base);
        editor.key(&key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(editor.cursor(), 0);
        editor.key(&key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(editor.cursor(), base.chars().count());

        // History: Up shows the last entry and saves the draft; Down
        // past the newest restores it.
        let mut editor = Editor::default();
        type_text(&mut editor, "first");
        editor.commit();
        type_text(&mut editor, "second");
        editor.commit();
        type_text(&mut editor, "draft");
        editor.key(&key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(editor.text(), "second");
        editor.key(&key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(editor.text(), "first");
        editor.key(&key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(editor.text(), "second");
        editor.key(&key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(editor.text(), "draft");

        // Paste: '\n' becomes a space and nothing submits.
        let mut editor = Editor::default();
        editor.paste("a\nb");
        assert_eq!(editor.text(), "a b");
    }

    /// Guards D9's classification: an empty or blank line is nothing; a
    /// `/`-prefixed line is a command, its first word taken as
    /// written; anything else is a turn. Mutation: classify `/nope` as
    /// a turn.
    #[test]
    fn a_slash_line_is_a_command_never_a_turn() {
        let submission_of = |text: &str| {
            let mut editor = Editor::default();
            type_text(&mut editor, text);
            editor.submission()
        };
        assert_eq!(submission_of(""), Submission::Nothing);
        assert_eq!(submission_of("  "), Submission::Nothing);
        assert_eq!(submission_of("/exit"), Submission::Command(Command::Exit));
        assert_eq!(
            submission_of(" /exit now"),
            Submission::Command(Command::Exit)
        );
        assert_eq!(submission_of("/help"), Submission::Command(Command::Help));
        assert_eq!(
            submission_of("/"),
            Submission::Command(Command::Unknown("/".to_string()))
        );
        assert_eq!(
            submission_of("/EXIT"),
            Submission::Command(Command::Unknown("/EXIT".to_string()))
        );
        assert_eq!(
            submission_of("/nope x"),
            Submission::Command(Command::Unknown("/nope".to_string()))
        );
        assert_eq!(submission_of("hi"), Submission::Turn("hi".to_string()));
    }
}
