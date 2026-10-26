//! Exact-frontier conditional text writes use the ordinary result protocol.

use super::*;
use crate::body::ExecuteConditionalWrite;

impl Client {
    /// Execute one native text write only if the selected database still has
    /// the expected committed sequence. Both the condition and the write run
    /// under the server's exclusive writer ownership and fresh live authority.
    /// A stale condition returns Conflict without effects, even for a write
    /// that would otherwise change no rows. Native RETURN is supported.
    ///
    /// The sequence belongs to this selected database incarnation; it is not
    /// a historical write basis or an idempotency key. This method never
    /// retries, reconnects or substitutes an unconditional write. An uncertain
    /// completion or disconnected delivery does not establish rollback.
    pub async fn execute_write_if_current(
        &mut self,
        cx: &Cx,
        expected_seq: u64,
        statement: &str,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<Answer, ClientError> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let outcome = self
            .execute_write_if_current_streaming(
                cx,
                expected_seq,
                statement,
                parameters,
                |names| {
                    columns = names.to_vec();
                    Ok(())
                },
                |row| {
                    rows.push(row);
                    Ok(())
                },
            )
            .await?;
        Ok(Answer {
            columns,
            rows,
            outcome,
        })
    }

    /// Deliver a conditional write's RETURN rows with ordinary flow credit.
    /// The server admits the complete bounded answer before native completion;
    /// callbacks receive it after that boundary. Callback refusal cancels and
    /// drains delivery, not committed effects. Conflict and OutcomeUnknown
    /// retain their distinct server classes and are never automatically retried.
    pub async fn execute_write_if_current_streaming(
        &mut self,
        cx: &Cx,
        expected_seq: u64,
        statement: &str,
        mut parameters: Vec<(String, WireValue)>,
        on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        on_row: impl FnMut(Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<Outcome, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let request = self
            .send(
                cx,
                FrameKind::ExecuteConditionalWrite,
                StreamId::CONTROL,
                &ExecuteConditionalWrite {
                    expected_seq,
                    statement: statement.to_owned(),
                    parameters,
                },
            )
            .await?;
        let outcome = self.receive_result(cx, request, on_columns, on_row).await?;
        if matches!(outcome, Outcome::Rows { .. }) {
            return Err(ClientError::Protocol(
                "conditional write returned a read outcome",
            ));
        }
        Ok(outcome)
    }
}
