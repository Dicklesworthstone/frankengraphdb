//! Finite mixed HTTP scripts use the same native owner as explicit FGP.
//! Input adaptation finishes before BEGIN; every result is retained privately
//! and admitted before COMMIT. There is no HTTP-specific transaction lifecycle.

use super::*;
use crate::Served;
use crate::execute::transaction::NativeWriteTransaction;
use fgdb_protocol::client::MAX_TRANSACTION_BATCH_STATEMENTS;
use fgdb_protocol::json::{JsonParseError, parse_json_controlled};
use fgdb_warden::{ExecutionPermit, WriteAccess};

fn statements(
    request: &Request,
    mut checkpoint: impl FnMut() -> Result<(), Refusal>,
) -> Result<Vec<Execute>, Response> {
    let malformed = |detail: &str| refusal_response(ErrorCode::Protocol, detail);
    let refused = |Refusal { code, message }| refusal_response(code, &message);
    let text = core::str::from_utf8(&request.body).map_err(|_| malformed("body is not UTF-8"))?;
    let json = parse_json_controlled(text, MAX_JSON_VALUES, MAX_JSON_TOKEN_BYTES, |_| {
        checkpoint()
    })
    .map_err(|error| match error {
        JsonParseError::Invalid(message) => malformed(&format!("invalid JSON body: {message}")),
        JsonParseError::Interrupted(error) => refused(error),
    })?;
    let Json::Object(mut fields) = json else {
        return Err(malformed("body must be a JSON object"));
    };
    let Some(Json::Array(statements)) = fields.remove("statements") else {
        return Err(malformed(
            "\"statements\" must be a nonempty array of objects",
        ));
    };
    if !(1..=MAX_TRANSACTION_BATCH_STATEMENTS).contains(&statements.len()) {
        return Err(malformed(
            "a transaction requires between 1 and 32 statements",
        ));
    }
    if let Some(unknown) = fields.keys().next() {
        return Err(malformed(&format!("unknown body field {unknown:?}")));
    }
    statements
        .into_iter()
        .map(|statement| {
            checkpoint().map_err(refused)?;
            let Json::Object(mut fields) = statement else {
                return Err(malformed("each statement must be an object"));
            };
            let mode = match fields.remove("mode") {
                Some(Json::String(mode)) if mode == "read" => ExecuteMode::Read,
                Some(Json::String(mode)) if mode == "write" => ExecuteMode::Write,
                _ => return Err(malformed("each statement needs mode \"read\" or \"write\"")),
            };
            let statement = statement_fields(fields, mode)?;
            if statement.statement.trim().is_empty() {
                return Err(malformed("transaction statements must not be empty"));
            }
            // Apply the native name, value, depth and parameter-map bounds to
            // EVERY entry before the first owner can reserve an identity.
            statement
                .encode()
                .map_err(|error| malformed(&format!("invalid transaction statement: {error}")))?;
            checkpoint().map_err(refused)?;
            Ok(statement)
        })
        .collect()
}

fn checkpoint(
    cx: &Cx,
    generation: &Generation,
    admission: &mut ExecutionPermit<'_, WriteAccess>,
) -> Result<(), Refusal> {
    cx.checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    admission
        .checkpoint_at(crate::unix_millis())
        .map_err(|error| crate::execute::write_refusal(&error))
}

fn response_budget() -> Refusal {
    Refusal::new(
        ErrorCode::Budget,
        "the complete transaction response exceeds the configured frame limit; no write was committed",
    )
}

/// Reserve the largest terminal before the final sequence number is known.
/// After publication, appending a completion cannot become a new budget error.
const COMPLETION_RESERVE: usize =
    r#"],"seq":18446744073709551615,"statements":18446744073709551615,"committed":false}"#.len();

struct RetainedResponse {
    body: String,
    maximum: usize,
}

impl RetainedResponse {
    fn new(basis: u64, maximum: usize) -> Result<Self, Refusal> {
        let maximum = maximum
            .checked_sub(COMPLETION_RESERVE)
            .ok_or_else(response_budget)?;
        let mut response = Self {
            body: String::new(),
            maximum,
        };
        response.push(&format!(r#"{{"v":1,"basis":{basis},"results":["#))?;
        Ok(response)
    }

    fn push(&mut self, fragment: &str) -> Result<(), Refusal> {
        if fragment.len() > self.maximum.saturating_sub(self.body.len()) {
            return Err(response_budget());
        }
        self.body.push_str(fragment);
        Ok(())
    }

    fn result(
        &mut self,
        answer: &Answer,
        mut checkpoint: impl FnMut() -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        checkpoint()?;
        self.push(r#"{"columns":["#)?;
        for (index, column) in answer.columns.iter().enumerate() {
            checkpoint()?;
            if index != 0 {
                self.push(",")?;
            }
            self.push(&quote(column))?;
        }
        self.push(r#"],"rows":["#)?;
        for (index, row) in answer.rows.iter().enumerate() {
            checkpoint()?;
            if index != 0 {
                self.push(",")?;
            }
            self.push("[")?;
            for (column, value) in row.iter().enumerate() {
                checkpoint()?;
                if column != 0 {
                    self.push(",")?;
                }
                // Each source cell already passed the native frame and value
                // limits. Check the actual shared JSON encoding too: escaped
                // text and typed envelopes can exceed their binary wire size.
                self.push(&cell(value))?;
            }
            self.push("]")?;
        }
        self.push("]}")
    }

    fn complete(mut self, seq: u64, statements: u64, committed: bool) -> Response {
        let terminal =
            format!(r#"],"seq":{seq},"statements":{statements},"committed":{committed}}}"#);
        debug_assert!(terminal.len() <= COMPLETION_RESERVE);
        self.body.push_str(&terminal);
        json_response(200, self.body)
    }
}

async fn execute(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statements: &[Execute],
    generation: &Generation,
    admission: &mut ExecutionPermit<'_, WriteAccess>,
    maximum: usize,
) -> Result<Response, Refusal> {
    let mut owner = NativeWriteTransaction::begin(cx, db, token).await?;
    // A recovery during body adaptation or BEGIN cannot silently select a new
    // generation under the response's original authority binding.
    generation.check().map_err(Refusal::from)?;
    let mut response = RetainedResponse::new(owner.basis().0, maximum)?;
    for (index, statement) in statements.iter().enumerate() {
        let answer = owner.execute_framed(cx, statement, maximum).await?;
        if index != 0 {
            response.push(",")?;
        }
        response.result(&answer, || checkpoint(cx, generation, admission))?;
        // Drop this statement's wire values after encoding. Retention is one
        // bounded JSON response, never 32 independent result bags.
    }
    // Every fallible input/result operation precedes this sole publication.
    // Dropping the owner on any earlier return abandons the entire workspace.
    // An unknown native completion is returned unchanged, without a retry or
    // a fabricated rollback acknowledgement.
    let answer = owner.commit(cx).await?;
    let (seq, statements, committed) = match answer.outcome {
        Outcome::WriteCommitted { seq, statements } => (seq, statements, true),
        Outcome::ReadClosed { seq, statements } => (seq, statements, false),
        Outcome::Rows { .. } => unreachable!("native COMMIT returns a terminal completion"),
    };
    // The already-pinned OutputAuthority fences every physical response byte.
    // There is no post-commit retryable check that can imply the write aborted.
    Ok(response.complete(seq, statements, committed))
}

pub(super) async fn respond<'s>(
    cx: &Cx,
    server: &'s Server,
    db: &'s Served,
    token: &CapabilityToken,
    request: &Request,
    output: &OutputAuthority<'s>,
) -> Response {
    // This opens a write-capable owner, including for a read-only script.
    // Read-only credentials use /query-batch. Rights precede body diagnostics.
    // This unspent guard checks live validity without re-verifying the token's
    // MAC for every copied cell. It never supplies a second execution budget;
    // all semantic work and rows are charged by the retained native owner.
    let now = crate::unix_millis();
    let mut admission = match db
        .authority
        .verify_at(token, crate::TRUNK, now)
        .and_then(|capability| capability.into_write_at(crate::TRUNK, now))
    {
        Ok(admission) => admission,
        Err(error) => {
            let Refusal { code, message } = crate::execute::write_refusal(&error);
            return refusal_response(code, &message);
        }
    };
    let generation = match db.db.generation() {
        Ok(generation) => generation,
        Err(error) => return refusal_response(error.code(), error.message()),
    };
    // Data-dependent refusal text and successful output have the same fence,
    // even when recovery begins while the HTTP response waits for its socket.
    if !output.pin_generation(generation.clone()) {
        return refusal_response(ErrorCode::Execution, "connection unavailable");
    }
    let statements = match statements(request, || checkpoint(cx, &generation, &mut admission)) {
        Ok(statements) => statements,
        Err(response) => return response,
    };
    match execute(
        cx,
        db,
        token,
        &statements,
        &generation,
        &mut admission,
        server.limits.max_frame_len as usize,
    )
    .await
    {
        Ok(response) => response,
        Err(Refusal { code, message }) => refusal_response(code, &message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::http::h1::types::RequestBuilder;

    #[test]
    fn input_cancellation_preserves_its_class_before_later_invalid_fields() {
        let mut request =
            RequestBuilder::new(Method::Post, "/v1/databases/test/transaction").build();
        request.body = format!(
            r#"{{"statements":[{{"mode":"read","statement":"RETURN $text","parameters":{{"text":{}}}}},{{"mode":"invalid","statement":"RETURN 1"}}]}}"#,
            quote(&"x".repeat(fgdb_protocol::json::JSON_PARSE_RUN_BYTES + 10)),
        )
        .into_bytes();
        let mut checks = 0;
        let response = statements(&request, || {
            checks += 1;
            if checks > 1 {
                Err(Refusal::new(ErrorCode::Cancelled, "admission cancelled"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(checks, 2);
        assert_eq!(response.status, 500);
        assert_eq!(
            std::str::from_utf8(&response.body).unwrap(),
            r#"{"v":1,"error":{"code":"cancelled","message":"admission cancelled"}}"#,
        );
    }

    #[test]
    fn terminal_reservation_covers_maximum_counters_without_postcommit_admission() {
        for committed in [false, true] {
            let prefix = format!(r#"{{"v":1,"basis":{},"results":["#, u64::MAX);
            let maximum = prefix.len() + COMPLETION_RESERVE;
            let response = RetainedResponse::new(u64::MAX, maximum)
                .unwrap_or_else(|error| panic!("{}", error.message));
            let response = response.complete(u64::MAX, u64::MAX, committed);
            assert!(response.body.len() <= maximum);
            assert!(parse_json(std::str::from_utf8(&response.body).unwrap(), 32, 64).is_ok());
            assert!(RetainedResponse::new(u64::MAX, maximum - 1).is_err());
        }
    }
}
