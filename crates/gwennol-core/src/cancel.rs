//! How a step says it was stopped before it started.
//!
//! The loop reads two values off a step's structured error to tell a call
//! that was cut before it did anything from one that was cut somewhere in
//! its work. They are the loop's contract with any step, whoever supplies it.

/// The error code of a cancelled step, carried by a
/// `KernelError::PluginError`, so the loop recognises the cancellation as
/// data and never by reading error text or guessing from a token's state.
pub const CANCELLED_CODE: &str = "gwennol.cancelled";

/// The `params.phase` value that marks a [`CANCELLED_CODE`] error as
/// reporting that the step was stopped before it began its work. A
/// cancellation without it lands somewhere in the work, where the step
/// cannot say how far it got.
pub const CANCELLED_AT_APPROVAL: &str = "approval";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_what_steps_already_send() {
        assert_eq!(CANCELLED_CODE, "gwennol.cancelled");
        assert_eq!(CANCELLED_AT_APPROVAL, "approval");
    }
}
