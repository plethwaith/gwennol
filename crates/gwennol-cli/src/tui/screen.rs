//! The terminal state a session enters — raw mode, the alternate
//! screen, bracketed paste, the kitty keyboard flag — and undoes on
//! every way out: a guard's `Drop`, and a panic hook for the one way
//! out a guard cannot see. Bits already entered are recorded in a
//! process-wide `AtomicU8`; the panic hook clears them and undoes what
//! they named on stdout directly, since no guard is reachable from a
//! panic. Restoring is not harmless to repeat for every command
//! (`PopKeyboardEnhancementFlags` pops one real entry off the
//! terminal's keyboard-enhancement stack, not a no-op the second
//! time), so the guard's `Drop` only undoes what the shared bits still
//! say is entered: if the panic hook already cleared and undid a bit
//! during unwind, `Drop` running afterward on the same guard leaves it
//! alone.

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

// A test in this module that reads or writes this (directly, or
// through `Screen` or the panic hook) must hold the `tests` module's
// `TEST_LOCK` first: nothing about a process-wide static enforces
// that on its own.
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

/// A guard over the terminal state a session enters. `Drop` undoes, in
/// reverse, whatever it entered that the shared bits still record as
/// entered: a bit a panic hook already restored is not restored twice
/// (see the module doc).
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
            // Recorded as soon as it is true, not batched until the
            // whole function returns: a panic between here and the end
            // (D5's case) must still see raw mode as entered.
            ENTERED.fetch_or(RAW, Ordering::SeqCst);
        }
        let want_kitty = match kitty {
            Kitty::Probe => terminal::supports_keyboard_enhancement().unwrap_or(false),
            Kitty::On => true,
            Kitty::Off => false,
        };
        let outcome: io::Result<()> = (|| {
            // Marked before the write it covers is attempted, not
            // after: `queue!`'s commands can fail between them, and
            // recording optimistically means `undo`'s matching pair is
            // asked to clean up a byte sequence that might have been
            // partly written, rather than one the accounting says
            // never was. `SCREEN`'s undo (`DisableBracketedPaste`,
            // `LeaveAlternateScreen`) is safe to send even over a
            // partial write; `KITTY`'s (`PopKeyboardEnhancementFlags`,
            // below) is not, but it is recorded exactly the same
            // optimistic way — a repeat is prevented by the masked
            // `Drop`/panic-hook accounting (see the module doc), not by
            // this bit's undo being repeat-safe.
            bits |= SCREEN;
            ENTERED.fetch_or(SCREEN, Ordering::SeqCst);
            queue!(out, EnterAlternateScreen, EnableBracketedPaste)?;
            if want_kitty {
                bits |= KITTY;
                ENTERED.fetch_or(KITTY, Ordering::SeqCst);
                queue!(
                    out,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    )
                )?;
            }
            out.flush()
        })();
        if let Err(e) = outcome {
            // Undoes raw `bits` directly, unmasked: no `Screen` guard
            // exists yet for a panic hook to have raced with, so there
            // is nothing to mask against here (unlike `Drop`, below).
            undo(&mut out, bits);
            ENTERED.fetch_and(!bits, Ordering::SeqCst);
            return Err(e);
        }
        Ok(Self { out, bits })
    }
}

impl<W: Write> Drop for Screen<W> {
    fn drop(&mut self) {
        // A panic hook may have already run and undone some or all of
        // `self.bits` (clearing them from `ENTERED` as it does) before
        // this `drop` runs during the same unwind: only undo what
        // `ENTERED` still records as entered, so a bit the hook already
        // restored is not restored a second time (`PopKeyboardEnhancementFlags`
        // is not safe to repeat: see the module doc).
        let still_entered = ENTERED.load(Ordering::SeqCst) & self.bits;
        undo(&mut self.out, still_entered);
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
    use std::sync::Mutex;

    use crossterm::Command;

    use super::*;

    /// The tests below share the process-wide `ENTERED`; serialized so
    /// one test's `Screen` cannot see or clear another's concurrently
    /// running bits.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

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
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ENTERED.store(0, Ordering::SeqCst);
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
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ENTERED.store(0, Ordering::SeqCst);
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

    /// Guards the `Drop`/panic-hook coordination: once a panic hook has
    /// undone a guard's bits (clearing them from `ENTERED`, as
    /// [`install_panic_hook`] does, simulated here by doing the same
    /// directly), the same guard's `Drop` running afterward during
    /// unwind must not repeat them — the terminal's
    /// keyboard-enhancement stack has only one real entry to pop.
    /// Mutation: undo `self.bits` unconditionally in `Drop` instead of
    /// masking against `ENTERED` — the pop-disable-leave sequence
    /// reappears in `recorder` a second time.
    #[test]
    fn drop_after_a_panic_hook_already_undid_the_bits_does_not_repeat_them() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ENTERED.store(0, Ordering::SeqCst);
        let sink = Sink::default();
        let recorder = sink.0.clone();
        let screen = Screen::enter(sink, false, Kitty::On).unwrap();
        recorder.borrow_mut().clear();

        // What `install_panic_hook`'s closure does: swap `ENTERED` to
        // 0 and undo those bits itself, on its own writer (the same
        // sink here, so the bytes it writes are visible below).
        let bits = ENTERED.swap(0, Ordering::SeqCst);
        undo(&mut Sink(recorder.clone()), bits);
        let after_hook = recorder.borrow().clone();
        assert!(
            !after_hook.is_empty(),
            "the simulated panic hook wrote nothing to undo"
        );

        // `Screen`'s own `Drop`, running afterward as the panic
        // unwinds: must not write the same undo bytes again.
        drop(screen);
        assert_eq!(
            *recorder.borrow(),
            after_hook,
            "Drop repeated the panic hook's own undo"
        );

        ENTERED.store(0, Ordering::SeqCst);
    }
}
