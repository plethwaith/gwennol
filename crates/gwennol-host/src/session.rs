//! Driving a session from an [`Operator`]'s input.

use gwead::tokio_util::sync::CancellationToken;
use gwennol_core::{Session, TurnError};

use crate::operator::Operator;

/// Drive turns from [`Operator::input`] until it returns `None`,
/// stopping at the first turn that does not complete. A frontend
/// that wants to carry on past a failed turn drives [`Session::turn`]
/// itself.
pub async fn run(
    session: &mut Session,
    operator: &dyn Operator,
    cancel: &CancellationToken,
) -> Result<(), TurnError> {
    loop {
        let turn = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(TurnError::Cancelled { detail: None }),
            turn = operator.input() => turn,
        };
        let Some(turn) = turn else {
            return Ok(());
        };
        session.turn(&turn.text, cancel).await?;
    }
}
