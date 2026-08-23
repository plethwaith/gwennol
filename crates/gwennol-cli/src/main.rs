//! Gwennol command-line frontend.
//!
//! The first `Operator` implementation: non-interactive by design, so that
//! policy is driven by flags and files rather than prompts. The TUI comes
//! later as a second `Operator`, not a rewrite.

#![forbid(unsafe_code)]

fn main() {
    println!("gwennol {}", env!("CARGO_PKG_VERSION"));
}
