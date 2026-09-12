//! The terminal state a session enters — raw mode, the alternate
//! screen, bracketed paste, the kitty keyboard flag — and undoes on
//! every way out: a guard's `Drop`, and a panic hook for the one way
//! out a guard cannot see. Bits already entered are recorded in a
//! process-wide `AtomicU8`; the panic hook clears them and undoes what
//! they named on stdout directly, since no guard is reachable from a
//! panic. Restoring twice is harmless (every crossterm undo command is
//! idempotent), so the guard and the hook never coordinate beyond the
//! shared bits.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU8, Ordering};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::queue;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

const RAW: u8 = 1;
const SCREEN: u8 = 2;
const KITTY: u8 = 4;

static ENTERED: AtomicU8 = AtomicU8::new(0);

/// Whether to push the kitty keyboard protocol's disambiguation flag.
pub enum Kitty {
    /// Ask the terminal, once raw mode is on.
    Probe,
    /// Always push it (tests).
    On,
    /// Never push it (tests).
    Off,
}

/// A guard over the terminal state a session enters. `Drop` undoes
/// exactly what was entered, in reverse.
pub struct Screen<W: Write> {
    out: W,
    bits: u8,
}

impl<W: Write> Screen<W> {
    /// Enter: raw mode when `raw`, the alternate screen, bracketed
    /// paste, the kitty flag per `kitty`. On any failure after raw
    /// mode is on, raw mode is turned off before the error returns.
    pub fn enter(mut out: W, raw: bool, kitty: Kitty) -> io::Result<Self> {
        let mut bits = 0u8;
        if raw {
            terminal::enable_raw_mode()?;
            bits |= RAW;
        }
        let want_kitty = match kitty {
            Kitty::Probe => terminal::supports_keyboard_enhancement().unwrap_or(false),
            Kitty::On => true,
            Kitty::Off => false,
        };
        let outcome: io::Result<()> = (|| {
            queue!(out, EnterAlternateScreen, EnableBracketedPaste)?;
            bits |= SCREEN;
            if want_kitty {
                queue!(
                    out,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    )
                )?;
                bits |= KITTY;
            }
            out.flush()
        })();
        if let Err(e) = outcome {
            undo(&mut out, bits);
            return Err(e);
        }
        ENTERED.fetch_or(bits, Ordering::SeqCst);
        Ok(Self { out, bits })
    }
}

impl<W: Write> Drop for Screen<W> {
    fn drop(&mut self) {
        undo(&mut self.out, self.bits);
        ENTERED.fetch_and(!self.bits, Ordering::SeqCst);
    }
}

/// Take the previous panic hook and install one that restores whatever
/// bits are still recorded as entered, on stdout, before running it.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bits = ENTERED.swap(0, Ordering::SeqCst);
        undo(&mut io::stdout(), bits);
        prev(info);
    }));
}

/// Undo `bits`, in the reverse of the order [`Screen::enter`] sets
/// them: pop the kitty flag, disable bracketed paste, leave the
/// alternate screen, flush, then disable raw mode last. Every error is
/// ignored: there is nothing better to do with a write failure while
/// leaving.
fn undo(out: &mut impl Write, bits: u8) {
    if bits & KITTY != 0 {
        let _ = queue!(out, PopKeyboardEnhancementFlags);
    }
    if bits & SCREEN != 0 {
        let _ = queue!(out, DisableBracketedPaste, LeaveAlternateScreen);
    }
    let _ = out.flush();
    if bits & RAW != 0 {
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crossterm::Command;

    use super::*;

    /// A `Write` sink two owners can inspect: `Screen` takes one clone,
    /// the test keeps the other.
    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn bytes_of(command: impl Command) -> Vec<u8> {
        let mut s = String::new();
        command.write_ansi(&mut s).unwrap();
        s.into_bytes()
    }

    /// Guards D5: the guard writes the three enter commands on
    /// `enter`, then undoes them in reverse on `Drop`; with kitty off,
    /// neither kitty sequence appears. Mutation: skip
    /// `LeaveAlternateScreen` in `undo`.
    #[test]
    fn the_guard_undoes_what_it_entered_in_reverse() {
        let sink = Sink::default();
        let recorder = sink.0.clone();
        let screen = Screen::enter(sink, false, Kitty::On).unwrap();
        let mut expect = bytes_of(EnterAlternateScreen);
        expect.extend(bytes_of(EnableBracketedPaste));
        expect.extend(bytes_of(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        )));
        assert_eq!(*recorder.borrow(), expect, "enter bytes");

        drop(screen);
        expect.extend(bytes_of(PopKeyboardEnhancementFlags));
        expect.extend(bytes_of(DisableBracketedPaste));
        expect.extend(bytes_of(LeaveAlternateScreen));
        assert_eq!(*recorder.borrow(), expect, "enter then undo bytes");
    }

    #[test]
    fn kitty_off_writes_no_kitty_sequence() {
        let sink = Sink::default();
        let recorder = sink.0.clone();
        let screen = Screen::enter(sink, false, Kitty::Off).unwrap();
        let mut expect = bytes_of(EnterAlternateScreen);
        expect.extend(bytes_of(EnableBracketedPaste));
        assert_eq!(*recorder.borrow(), expect);
        drop(screen);
        expect.extend(bytes_of(DisableBracketedPaste));
        expect.extend(bytes_of(LeaveAlternateScreen));
        assert_eq!(*recorder.borrow(), expect);
    }
}
