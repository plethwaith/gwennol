//! The pane's own keys — paging it off the tail and back, and
//! focusing a tool call or result to expand it in place. Paging and
//! `Home` compute against what the last frame drew
//! ([`Ui::pane_view`]); focusing and toggling recount fresh from the
//! entries at their current expansion (`reveal`), taking only the
//! last frame's width and height. Every write clamps again at the
//! next render, the shape `prompt::key` and `render_prompt` share.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::ui::{self, Entry, PaneView, Ui};

/// Route one key to the pane. `true` when the pane took it: `PageUp`
/// and `PageDown`, any modifiers; `Home` and `End`, unmodified, while
/// the editor holds no text; `Tab` and `BackTab`, any modifiers;
/// `Enter`, any modifiers, while the editor holds no text and an
/// entry is focused. Anything else is the editor's.
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

/// D6. Compares entry `index`'s head row against the window the
/// next frame would draw: the rows recounted through `Entry::text`
/// and `ui::wrap` at the entries' current expansion, at the last
/// frame's width and height. Outside that window, `scroll` becomes
/// the head row normalised as D5, so a head row at or past
/// `max_top` follows the tail instead of sitting on the first row.
/// Inside it, `scroll` is kept but re-normalised against this
/// call's row count, so a collapse cannot strand it at or past the
/// new `max_top`.
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
    if (top..top + height).contains(&head) {
        // The head row is already on screen: keep the scroll, but
        // normalise it against this call's own row count. Without
        // this, a `Some(top)` `reveal` leaves alone here can still be
        // at or past a `max_top` a row-count change (a collapse, most
        // often) just shrank past it, leaving a `Some(top)` that D5's
        // rule for every move ("a `top` at or past `max_top` becomes
        // `None`") would have cleared had the collapse been a move,
        // until the transcript grows again and pins the pane away
        // from the tail with no scroll having happened. `.and(` states
        // that intent (kept only when it was `Some`) rather than
        // changing the result: `top` already carries `.min(max_top)`,
        // so `ui.scroll = normalise(top, max_top)` gives the same value.
        ui.scroll = ui.scroll.and(normalise(top, max_top));
    } else {
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

    /// Guards D5/D6: collapsing an expanded entry must not strand
    /// `ui.scroll` at a row `reveal` kept because it was already on
    /// screen, once the collapse's own smaller row count already put
    /// that row at or past `max_top` — the pane silently stops
    /// following the tail the moment a later push grows the
    /// transcript back past the stale `top`, with no scroll having
    /// happened in between. Mutation: drop the
    /// `ui.scroll.and(normalise(top, max_top))` on `reveal`'s
    /// kept-scroll branch, leaving it empty.
    #[test]
    fn collapsing_an_expanded_entry_normalises_the_kept_scroll() {
        let shared = shared_with(vec![Entry::Assistant("aaa".to_string()), result("r", 30)]);
        draw(&shared, 40, 8);
        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        press(&shared, KeyCode::Enter, KeyModifiers::NONE); // expand
        press(&shared, KeyCode::Enter, KeyModifiers::NONE); // collapse, no `End` in between
        let buf = draw(&shared, 40, 8);
        assert!(
            shared.lock().pane_view.get().following(),
            "the collapsed transcript no longer fits the pane: {:?}",
            shared.lock().pane_view.get()
        );
        assert!(
            !row(&buf, 6).contains(ui::SCROLLED),
            "marker shown with nothing having scrolled"
        );

        for n in 0..10 {
            shared.update(|ui| ui.push(Entry::Trace(format!("gwennol: later{n}"))));
        }
        let buf = draw(&shared, 40, 8);
        assert!(
            shared.lock().pane_view.get().following(),
            "the pane stopped following the tail once later pushes grew the \
             transcript past a scroll the collapse left stale: {:?}",
            shared.lock().pane_view.get()
        );
        assert!(!row(&buf, 6).contains(ui::SCROLLED));
        assert!(
            row(&buf, 5).contains("later9"),
            "the tail is not drawn: {:?}",
            row(&buf, 5)
        );
    }

    /// Guards `render_pane`'s own clamp (`ui.rs`): a `scroll` past
    /// `max_top` — the state left behind by a resize, which touches
    /// neither `scroll` nor `focus` and never reaches `reveal` (see
    /// `drive.rs`'s `Input::Resize` arm) — is clamped to `max_top`,
    /// not merely to the row count, so the pane still draws a full
    /// `height` rows rather than trailing blank space below a
    /// too-large `top`. Mutation:
    /// `ui.scroll.unwrap_or(max_top).min(rows.len())` in place of
    /// `ui.scroll.map_or(max_top, |t| t.min(max_top))`.
    #[test]
    fn render_pane_clamps_a_stale_scroll_to_max_top_not_to_the_row_count() {
        let shared = shared_with(vec![result("r", 100)]);
        draw(&shared, 40, 8);
        let (total_rows, height) = {
            let view = shared.lock().pane_view.get();
            (view.rows, view.height as usize)
        };
        let max_top = total_rows.saturating_sub(height);
        assert!(max_top > 0, "fixture too small to exercise the clamp");
        // Set directly: `pane::key` never leaves `scroll` here (every
        // write there normalises against a `max_top`: the last
        // frame's `pane_view` in the paging and `Home` arms, a
        // freshly counted one in `reveal`), but a resize can, since
        // it touches nothing.
        shared.update(|ui| ui.scroll = Some(total_rows - 1));
        draw(&shared, 40, 8);
        assert_eq!(
            shared.lock().pane_view.get().top,
            max_top,
            "render_pane clamped to something other than max_top"
        );
    }

    /// Guards D7: the pane's keys are not gated on whether a turn is
    /// running — reading a session back while it is producing output
    /// is the change's stated purpose (plan section 1) — so `PageUp`
    /// still moves `scroll` with `running: true`. Mutation: `if
    /// !running && pane::key(ui, &key)` in `handle_key`.
    #[test]
    fn pane_keys_reach_the_pane_while_a_turn_is_running() {
        let long: String = (0..100)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let shared = shared_with(vec![Entry::Assistant(long)]);
        draw(&shared, 40, 6);
        let cancel = CancellationToken::new();
        handle_key(
            &shared,
            &cancel,
            true,
            Input::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        );
        assert!(
            shared.lock().scroll.is_some(),
            "PageUp did not reach the pane while a turn was running"
        );
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

        // `Home`/`End` reach the pane only when unmodified, unlike
        // `PageUp`/`PageDown`/`Tab`/`BackTab`/`Enter`, which take any
        // modifiers (the editor binds `Home`/`End` the same way, so a
        // modified one is nobody's). With the editor empty and a
        // scroll standing away from both 0 and the tail, `Shift+Home`
        // and `Shift+End` must move it not at all. Mutations: drop
        // the `KeyModifiers::NONE` guard from `key`'s `Home` arm;
        // drop it from the `End` arm.
        shared.update(|ui| {
            ui.editor = crate::tui::editor::Editor::default();
            ui.scroll = Some(5);
        });
        press(&shared, KeyCode::Home, KeyModifiers::SHIFT);
        assert_eq!(shared.lock().scroll, Some(5), "Shift+Home reached the pane");
        press(&shared, KeyCode::End, KeyModifiers::SHIFT);
        assert_eq!(shared.lock().scroll, Some(5), "Shift+End reached the pane");
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
    /// pane draws; `Enter` with text in the editor, or with no entry
    /// focused, toggles nothing. Mutation: drop the `revision` bump in
    /// `toggle`.
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
        // `press`'s `Action` is `None` whether `pane::key` swallowed
        // this `Enter` (`true`, no focus) or let it fall to the
        // editor's own (`false`, an empty submission is also
        // `Action`-less): calling `key` directly is the only way to
        // pin the `None` arm itself. Mutation: `None => true` in
        // `key`'s `Enter` match.
        {
            let mut ui = fresh.lock();
            assert!(
                !key(&mut ui, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                "Enter with no focus returned true from pane::key"
            );
        }
    }

    /// Guards D6: focusing an entry, or toggling one, brings its head
    /// row into the window the next frame would draw, moving the scroll
    /// only when it is not there already — the first fixture below
    /// toggles an entry whose head row the last frame did show, and the
    /// scroll moves all the same (`reveal` still re-normalises a kept
    /// `scroll`; `collapsing_an_expanded_entry_normalises_the_kept_scroll`
    /// pins that). Mutations: drop `reveal` in the
    /// toggle path; drop it in the `Tab` path; drop it in the `BackTab`
    /// arm.
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
        entries[5] = call("older", "{}");
        entries.push(result("s", 1));
        entries.extend((0..5).map(|n| Entry::Trace(format!("gwennol: u{n}"))));
        let second = shared_with(entries);
        draw(&second, 40, 8);
        press(&second, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(second.lock().scroll, Some(0));
        press(&second, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(second.lock().focus, Some(30));
        let head_of_result: usize = second
            .lock()
            .entries
            .iter()
            .take(30)
            .map(|e| ui::wrap(&e.text(), 40).len())
            .sum();
        assert_eq!(second.lock().scroll, Some(head_of_result));

        // `Tab` again walks to the older expandable entry at index 5
        // (near the top, scrolling back up); `BackTab` returns to the
        // newer one just focused above, and D6 says its own site
        // (`reveal` in the `BackTab` arm) must scroll the head row
        // back into view exactly as the `Tab` path did — nothing pins
        // this site otherwise, since dropping its `reveal` call left
        // every other test green. Mutation: drop `reveal` from the
        // `BackTab` arm.
        press(&second, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(second.lock().focus, Some(5));
        assert_ne!(
            second.lock().scroll,
            Some(head_of_result),
            "focusing the older entry did not move the scroll"
        );
        press(&second, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(second.lock().focus, Some(30));
        assert_eq!(
            second.lock().scroll,
            Some(head_of_result),
            "BackTab's reveal did not bring the newer entry's head back on screen"
        );
        let buf = draw(&second, 40, 8);
        let reversed: Vec<u16> = (0..buf.area.height)
            .filter(|&y| {
                buf[(0, y)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
            .collect();
        assert_eq!(reversed.len(), 1, "{reversed:?}");
        assert!(row(&buf, reversed[0]).starts_with("gwennol: <- s"));
    }

    /// Guards D5: a submitted turn, or `/help`, returns the pane to
    /// the tail and drops any focus. Mutation: drop `follow_tail` in
    /// the `Turn` arm (the `/help` case is the second mutation).
    #[test]
    fn a_submit_returns_the_pane_to_the_tail_and_drops_the_focus() {
        // A small pane (height 2: 4 rows less the status and editor
        // lines) so the 5-row transcript below does not fit it whole,
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
