//! The pane's own keys — paging it off the tail and back, and
//! focusing a tool call or result to expand it in place — computed
//! against what the last frame drew ([`Ui::pane_view`]) and clamped
//! again at the next render, the shape `prompt::key` and
//! `render_prompt` share.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::ui::{self, Entry, PaneView, Ui};

/// Route one key to the pane. `true` when the pane took it: `PageUp`
/// and `PageDown` always; `Home` and `End` while the editor holds no
/// text; `Tab` and `BackTab` always; `Enter` while the editor holds
/// no text and an entry is focused. Anything else is the editor's.
pub fn key(ui: &mut Ui, event: &KeyEvent) -> bool {
    match event.code {
        KeyCode::PageUp | KeyCode::PageDown => {
            let PaneView {
                rows, height, top, ..
            } = ui.pane_view.get();
            let page = (height as usize).max(1);
            let max_top = rows.saturating_sub(height as usize);
            let new_top = if event.code == KeyCode::PageUp {
                top.saturating_sub(page)
            } else {
                top.saturating_add(page).min(max_top)
            };
            ui.scroll = normalise(new_top, max_top);
            true
        }
        KeyCode::Home if event.modifiers == KeyModifiers::NONE => {
            if !ui.editor.text().is_empty() {
                return false;
            }
            let PaneView { rows, height, .. } = ui.pane_view.get();
            let max_top = rows.saturating_sub(height as usize);
            ui.scroll = normalise(0, max_top);
            true
        }
        KeyCode::End if event.modifiers == KeyModifiers::NONE => {
            if !ui.editor.text().is_empty() {
                return false;
            }
            ui.scroll = None;
            true
        }
        KeyCode::Tab => {
            ui.focus = older(&ui.entries, ui.focus);
            if let Some(i) = ui.focus {
                reveal(ui, i);
            }
            true
        }
        KeyCode::BackTab => {
            ui.focus = newer(&ui.entries, ui.focus);
            if let Some(i) = ui.focus {
                reveal(ui, i);
            }
            true
        }
        KeyCode::Enter => {
            if !ui.editor.text().is_empty() {
                return false;
            }
            match ui.focus {
                Some(i) => {
                    if ui.toggle(i) {
                        reveal(ui, i);
                    }
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

/// Whether an entry can expand: a tool call or a tool result.
pub fn expandable(entry: &Entry) -> bool {
    matches!(entry, Entry::ToolCall { .. } | Entry::ToolResult { .. })
}

/// `Tab`: the newest expandable entry older than `from` (`None`: the
/// newest of all); `from` itself when there is none older.
fn older(entries: &[Entry], from: Option<usize>) -> Option<usize> {
    match from {
        None => (0..entries.len()).rev().find(|&i| expandable(&entries[i])),
        Some(from) => (0..from)
            .rev()
            .find(|&i| expandable(&entries[i]))
            .or(Some(from)),
    }
}

/// `BackTab`: the next expandable entry newer than `from`; `None`
/// past the newest, or from `None`.
fn newer(entries: &[Entry], from: Option<usize>) -> Option<usize> {
    let from = from?;
    (from + 1..entries.len()).find(|&i| expandable(&entries[i]))
}

/// D6. The least scroll that puts entry `index`'s head row on screen.
fn reveal(ui: &mut Ui, index: usize) {
    let PaneView { width, height, .. } = ui.pane_view.get();
    let width = (width as usize).max(1);
    let height = height as usize;
    let mut head = 0usize;
    let mut rows = 0usize;
    for (i, entry) in ui.entries.iter().enumerate() {
        let wrapped = ui::wrap(&entry.text(), width).len();
        if i < index {
            head += wrapped;
        }
        rows += wrapped;
    }
    let max_top = rows.saturating_sub(height);
    let top = ui.scroll.map_or(max_top, |t| t.min(max_top));
    if !(top..top + height).contains(&head) {
        ui.scroll = normalise(head, max_top);
    }
}

/// D5's normalisation: `top` at or past `max_top` is the tail.
fn normalise(top: usize, max_top: usize) -> Option<usize> {
    if top >= max_top { None } else { Some(top) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gwennol_core::ToolCall;
    use gwennol_core::gwead::tokio_util::sync::CancellationToken;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::tui::drive::{Action, handle_key};
    use crate::tui::keys::Input;
    use crate::tui::ui::{Shared, render};

    /// A fresh `Shared` whose `Ui` holds exactly `entries`.
    fn shared_with(entries: Vec<Entry>) -> Arc<Shared> {
        let shared = Shared::new();
        shared.update(|ui| ui.entries = entries);
        shared
    }

    /// One key, through the same entry point `drive` uses, `running:
    /// false` throughout — nothing here needs a turn in flight.
    fn press(shared: &Arc<Shared>, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        handle_key(
            shared,
            &CancellationToken::new(),
            false,
            Input::Key(KeyEvent::new(code, modifiers)),
        )
    }

    /// Render `shared`'s current `Ui` at `w`x`h` and return the raw
    /// buffer.
    fn draw(shared: &Arc<Shared>, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&shared.lock(), f)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Row `y` of `buf`, its full width, trailing padding included.
    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    /// A `ToolResult` named `name` whose content is `lines` lines,
    /// `l1` through `l{lines}`, collapsed.
    fn result(name: &str, lines: usize) -> Entry {
        let content = (1..=lines)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        Entry::ToolResult {
            call: ToolCall {
                id: None,
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
            content,
            is_error: false,
            expanded: false,
        }
    }

    /// A `ToolCall` named `name` with `arguments`, collapsed.
    fn call(name: &str, arguments: &str) -> Entry {
        Entry::ToolCall {
            call: ToolCall {
                id: None,
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
            expanded: false,
        }
    }

    /// Guards D5: `PageUp`/`PageDown` move the pane off the tail and
    /// back; the status line's marker tracks `following`; a key before
    /// the first render (`pane_view` zeroed) does not panic; a
    /// transcript that fits the pane at all shows no marker. Mutations:
    /// ignore `ui.scroll` in `render_pane`; skip `normalise` on
    /// `PageDown`.
    #[test]
    fn page_keys_scroll_the_pane_and_end_follows_the_tail_again() {
        let long: String = (0..100)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let rows = ui::wrap(&long, 40);
        let max_top = rows.len() - 4;
        let shared = shared_with(vec![Entry::Assistant(long)]);

        // Before any render, `pane_view` is zeroed: no panic.
        press(&shared, KeyCode::PageUp, KeyModifiers::NONE);

        draw(&shared, 40, 6);
        press(&shared, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, Some(max_top - 4));
        let buf = draw(&shared, 40, 6);
        assert_eq!(row(&buf, 0).trim_end(), rows[max_top - 4]);
        assert!(row(&buf, 4).contains(ui::SCROLLED));

        for _ in 0..50 {
            press(&shared, KeyCode::PageUp, KeyModifiers::NONE);
            draw(&shared, 40, 6);
        }
        assert_eq!(shared.lock().scroll, Some(0));
        let buf = draw(&shared, 40, 6);
        assert_eq!(row(&buf, 0).trim_end(), rows[0]);

        let mut presses = 0;
        loop {
            press(&shared, KeyCode::PageDown, KeyModifiers::NONE);
            draw(&shared, 40, 6);
            presses += 1;
            if shared.lock().scroll.is_none() {
                break;
            }
            assert!(presses <= rows.len(), "PageDown never reached the tail");
        }
        let buf = draw(&shared, 40, 6);
        assert_eq!(row(&buf, 3).trim_end(), rows.last().unwrap().as_str());
        assert!(!row(&buf, 4).contains(ui::SCROLLED));

        press(&shared, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, Some(0));

        shared.update(|ui| ui.push(Entry::Trace("gwennol: another".to_string())));
        let buf = draw(&shared, 40, 6);
        assert_eq!(row(&buf, 0).trim_end(), rows[0]);
        assert!(row(&buf, 4).contains(ui::SCROLLED));

        press(&shared, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, None);
        let buf = draw(&shared, 40, 6);
        assert!(row(&buf, 3).contains("another"));

        // `Home` first (any key input, `handle_key`'s own notice would
        // otherwise clear a notice set beforehand), the notice set
        // directly afterward so it survives to the render; at width
        // 80 there is room for both the notice and the marker without
        // one overwriting the other, unlike the 40-column frame above.
        press(&shared, KeyCode::Home, KeyModifiers::NONE);
        shared.update(|ui| ui.notice = Some("a notice".to_string()));
        let buf = draw(&shared, 80, 6);
        let status = row(&buf, 4);
        assert!(status.starts_with("a notice"), "{status:?}");
        assert!(status.trim_end().ends_with(ui::SCROLLED), "{status:?}");

        // A transcript short enough to fit the pane whole: no marker,
        // `Home`/`PageUp` are already at the tail.
        let shared2 = shared_with(vec![Entry::Trace("gwennol: hi".to_string())]);
        draw(&shared2, 40, 6);
        press(&shared2, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(shared2.lock().scroll, None);
        press(&shared2, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(shared2.lock().scroll, None);
        let buf = draw(&shared2, 40, 6);
        assert!(!row(&buf, 4).contains(ui::SCROLLED));
    }

    /// Guards D5: `Home`/`End` reach the pane only while the editor
    /// holds no text, else the editor's own. Mutation: drop the
    /// `is_empty` check.
    #[test]
    fn home_and_end_reach_the_pane_only_while_the_editor_is_empty() {
        let long: String = (0..100)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let shared = shared_with(vec![Entry::Assistant(long)]);
        draw(&shared, 40, 6);

        press(&shared, KeyCode::Char('h'), KeyModifiers::NONE);
        press(&shared, KeyCode::Char('i'), KeyModifiers::NONE);
        press(&shared, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(shared.lock().editor.cursor(), 0);
        assert_eq!(shared.lock().scroll, None, "Home reached the pane too");

        press(&shared, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(shared.lock().editor.cursor(), 2);

        shared.update(|ui| {
            ui.editor = crate::tui::editor::Editor::default();
        });
        press(&shared, KeyCode::Char(' '), KeyModifiers::NONE);
        press(&shared, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(shared.lock().editor.cursor(), 0);
        assert_eq!(
            shared.lock().scroll,
            None,
            "Home reached the pane while the editor held whitespace"
        );

        shared.update(|ui| {
            ui.editor = crate::tui::editor::Editor::default();
        });
        press(&shared, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, Some(0), "Home did not reach the pane");

        press(&shared, KeyCode::Char('h'), KeyModifiers::NONE);
        press(&shared, KeyCode::Char('i'), KeyModifiers::NONE);
        draw(&shared, 40, 6);
        press(&shared, KeyCode::PageUp, KeyModifiers::NONE);
        assert!(shared.lock().scroll.is_some(), "PageUp reached the editor");
    }

    /// Guards D4: `Tab` focuses the newest expandable entry and walks
    /// older; `BackTab` walks newer; a `Ui` with nothing expandable
    /// stays `None`. Mutations: `older` walks every entry; drop the
    /// `REVERSED` in `render_pane`.
    #[test]
    fn tab_focuses_the_newest_tool_entry_and_walks_older() {
        let shared = shared_with(vec![
            Entry::Trace("gwennol: trace".to_string()),
            call("a", "{}"),
            result("a", 1),
            Entry::Assistant("thinking".to_string()),
            call("b", "{}"),
            result("b", 1),
            Entry::Outcome("gwennol: done".to_string()),
        ]);
        draw(&shared, 40, 12);

        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, Some(5));
        let buf = draw(&shared, 40, 12);
        let reversed: Vec<u16> = (0..buf.area.height)
            .filter(|&y| {
                buf[(0, y)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
            .collect();
        assert_eq!(reversed.len(), 1, "{reversed:?}");
        assert!(row(&buf, reversed[0]).starts_with("gwennol: <- b"));

        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, Some(4));
        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, Some(2));
        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, Some(1));
        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, Some(1), "Tab at the oldest moved");

        press(&shared, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, Some(2));
        press(&shared, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, Some(4));
        press(&shared, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, Some(5));
        press(&shared, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, None, "BackTab past the newest moved");
        let buf = draw(&shared, 40, 12);
        assert!(
            (0..buf.area.height).all(|y| !buf[(0, y)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)),
            "a row is still reversed with no focus"
        );
        press(&shared, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(shared.lock().focus, None);

        let empty = shared_with(vec![]);
        press(&empty, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(empty.lock().focus, None);

        let none_expandable = shared_with(vec![
            Entry::Trace("gwennol: t".to_string()),
            Entry::Outcome("gwennol: o".to_string()),
        ]);
        press(&none_expandable, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(none_expandable.lock().focus, None);
    }

    /// Guards D4 and the cache row: `Enter` on an empty editor toggles
    /// the focused entry's expansion, which changes the row count the
    /// pane draws; a focused, unexpanded entry with no focus toggles
    /// nothing. Mutation: drop the `revision` bump in `toggle`.
    #[test]
    fn enter_on_an_empty_editor_toggles_the_focused_entry() {
        let shared = shared_with(vec![result("r", 10)]);
        draw(&shared, 40, 12);

        // `ui::wrap` rebuilds each row from `split_whitespace`, so the
        // 4-space indent `show::tool_result` writes for the preview
        // and each expanded line does not survive onto the pane's own
        // rows (the same is true of the approval box's pretty-printed
        // JSON, which wraps through the same function).
        let full = "l1 l2 l3 l4 l5 l6 l7 l8 l9 l10";
        {
            let buf = draw(&shared, 40, 12);
            assert!((0..buf.area.height).any(|y| row(&buf, y).trim_end() == full));
            assert!(!(0..buf.area.height).any(|y| row(&buf, y).trim_end() == "l2"));
        }

        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            shared.lock().entries[0],
            Entry::ToolResult { expanded: true, .. }
        ));
        {
            let buf = draw(&shared, 40, 12);
            assert!((0..buf.area.height).any(|y| row(&buf, y).trim_end() == "l1"));
            assert!((0..buf.area.height).any(|y| row(&buf, y).trim_end() == "l2"));
            assert!(!(0..buf.area.height).any(|y| row(&buf, y).trim_end() == full));
        }

        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            shared.lock().entries[0],
            Entry::ToolResult {
                expanded: false,
                ..
            }
        ));
        {
            let buf = draw(&shared, 40, 12);
            assert!((0..buf.area.height).any(|y| row(&buf, y).trim_end() == full));
        }

        press(&shared, KeyCode::Char(' '), KeyModifiers::NONE);
        let action = press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(action, None);
        assert!(matches!(
            shared.lock().entries[0],
            Entry::ToolResult {
                expanded: false,
                ..
            }
        ));

        let fresh = shared_with(vec![result("r", 1)]);
        draw(&fresh, 40, 12);
        let action = press(&fresh, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(action, None);
    }

    /// Guards D6: focusing an entry, or toggling one, keeps its head
    /// row on screen — scrolling the least amount necessary. Mutations:
    /// drop `reveal` in the toggle path; drop it in the `Tab` path.
    #[test]
    fn a_toggle_or_a_focus_move_keeps_the_head_row_on_screen() {
        let shared = shared_with(vec![
            Entry::Assistant("aaa bbb ccc ddd eee fff ggg hhh".to_string()),
            result("r", 30),
        ]);
        draw(&shared, 40, 8);
        let assistant_rows = ui::wrap("aaa bbb ccc ddd eee fff ggg hhh", 40).len();

        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, None, "the head was already on screen");

        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, Some(assistant_rows));
        let buf = draw(&shared, 40, 8);
        assert!(row(&buf, 0).starts_with("gwennol: <- r"));
        assert!(row(&buf, 6).contains(ui::SCROLLED));

        press(&shared, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, None);
        let buf = draw(&shared, 40, 8);
        assert!(!row(&buf, 0).starts_with("gwennol: <-"));

        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, None);

        // A second `Ui`, scrolled to the top with `Home`, then `Tab`
        // reveals the result far below (entries after it too, so the
        // tail's own window does not already happen to include it).
        let mut entries: Vec<Entry> = (0..30)
            .map(|n| Entry::Trace(format!("gwennol: t{n}")))
            .collect();
        entries.push(result("s", 1));
        entries.extend((0..5).map(|n| Entry::Trace(format!("gwennol: u{n}"))));
        let second = shared_with(entries);
        draw(&second, 40, 8);
        press(&second, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(second.lock().scroll, Some(0));
        press(&second, KeyCode::Tab, KeyModifiers::NONE);
        let head_of_result: usize = second
            .lock()
            .entries
            .iter()
            .take(30)
            .map(|e| ui::wrap(&e.text(), 40).len())
            .sum();
        assert_eq!(second.lock().scroll, Some(head_of_result));
    }

    /// Guards D5: a submitted turn, or `/help`, returns the pane to
    /// the tail and drops any focus. Mutation: drop `follow_tail` in
    /// the `Turn` arm (the `/help` case is the second mutation).
    #[test]
    fn a_submit_returns_the_pane_to_the_tail_and_drops_the_focus() {
        // A small pane (height 2: 4 rows less the status and editor
        // lines) so the 3-row transcript below does not fit it whole,
        // and `Home` has somewhere to scroll to.
        let shared = shared_with(vec![
            Entry::Assistant("aaa bbb ccc ddd eee fff ggg hhh".to_string()),
            result("r", 30),
        ]);
        draw(&shared, 40, 4);
        press(&shared, KeyCode::Home, KeyModifiers::NONE);
        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, Some(0));
        assert_eq!(shared.lock().focus, Some(1));

        for c in "go".chars() {
            press(&shared, KeyCode::Char(c), KeyModifiers::NONE);
        }
        let action = press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(action, Some(Action::Submit("go".to_string())));
        assert_eq!(shared.lock().scroll, None);
        assert_eq!(shared.lock().focus, None);

        press(&shared, KeyCode::Home, KeyModifiers::NONE);
        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(shared.lock().scroll, Some(0));
        assert_eq!(shared.lock().focus, Some(1));
        for c in "/help".chars() {
            press(&shared, KeyCode::Char(c), KeyModifiers::NONE);
        }
        let action = press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(action, None);
        assert_eq!(shared.lock().scroll, None);
        assert_eq!(shared.lock().focus, None);
        assert!(
            shared
                .lock()
                .entries
                .iter()
                .any(|e| e.text() == crate::tui::ui::HELP[0])
        );
    }
}
