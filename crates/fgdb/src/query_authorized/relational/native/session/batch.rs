//! Atomic read-batch delivery over the existing pinned session and live permit.
//! This is sequential governed execution, not another parser or transaction.

use super::*;

const MAX_STATEMENTS: usize = 32;

fn admit(count: usize, execution: &Live<'_, '_, '_>) -> Result<(), QueryError> {
    execution.borrow_mut().checkpoint()?;
    if count > MAX_STATEMENTS {
        return Err(QueryError::Unsupported {
            diagnostics: vec!["authorized read batch exceeds 32 statements".to_owned()],
        });
    }
    Ok(())
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Execute up to 32 native reads as one atomically delivered batch.
    ///
    /// All statements use this session's immutable generation and fixed host
    /// inputs. Results preserve statement order and their independent schemas,
    /// rows, duplicates and exact numeric types. Temporal selectors may choose
    /// different retained cuts within the SAME pinned generation; no statement
    /// reacquires the writer. Branch selectors can only confirm the fixed branch.
    ///
    /// ONE signed node/work/row allowance covers preparation, every statement
    /// and final delivery. Rows are reserved on that permit after each result,
    /// before retaining it for delivery. Group queries charge final group rows,
    /// not private input occurrences. The final session check does not recharge
    /// reserved rows. A late error or credential invalidation drops ALL staged
    /// results; there is no successful prefix or partial per-statement response.
    /// Execution stops at the first error and leaves later statements unrun.
    ///
    /// The stored native policy still bounds EACH statement independently.
    /// This does not claim a batch-wide native scratch/source-record meter or
    /// allocator-byte bound. Signed work/nodes/rows are cumulative across the
    /// entire batch, and the statement count bounds retained schema overhead.
    /// An empty or oversized batch still authenticates before shape admission.
    /// This is read-only and does not stage writes or acquire a transaction.
    pub fn query_batch(
        &mut self,
        cx: &QueryCx,
        statements: &[(&str, &GqlParameters)],
    ) -> Result<Vec<QueryResult>, QueryError> {
        self.run(cx, |view, branch, scope, resolver, policy, execution| {
            admit(statements.len(), execution)?;
            let mut results = Vec::new();
            for &(text, params) in statements {
                execution.borrow_mut().checkpoint()?;
                let (result, rows) = text_at(
                    view, branch, scope, resolver, policy, execution, text, params,
                )?;
                // Reservation and fresh validity checks precede retention.
                // The same permit survives every statement in this loop.
                execution.borrow_mut().deliver(rows)?;
                results.push(result);
            }
            Ok((results, 0))
        })
    }

    /// Execute a batch of this exact session's frozen templates with new values.
    /// No catalog callback is needed. All owner identities and branch selectors
    /// are admitted before executing the first source, so a foreign plan cannot
    /// be smuggled behind an empty or zero-page earlier query. Native argument
    /// validation then happens in statement order through the original engines.
    ///
    /// Results and shared signed allowances follow query_batch. The same handle
    /// may occur repeatedly with different arguments; each occurrence executes
    /// and is charged independently. No result cache or second permit exists.
    pub fn execute_batch(
        &mut self,
        cx: &QueryCx,
        statements: &[(&AuthorizedPreparedRead, &GqlParameters)],
    ) -> Result<Vec<QueryResult>, QueryError> {
        let owner = Arc::clone(&self.owner);
        self.run(cx, |view, branch, scope, _, policy, execution| {
            admit(statements.len(), execution)?;
            for &(prepared, params) in statements {
                execution.borrow_mut().checkpoint()?;
                if !Arc::ptr_eq(&owner, &prepared.owner) {
                    return Err(QueryError::Authorization(AuthorizationError::WrongAuthority));
                }
                let selected = prepared.selector.bind_parameters(params).map_err(selector_error)?;
                check_branch(&selected, branch)?;
            }
            let mut results = Vec::new();
            for &(prepared, params) in statements {
                execution.borrow_mut().checkpoint()?;
                let (result, rows) = prepared_at(
                    view, branch, scope, policy, execution, &owner, prepared, params,
                )?;
                execution.borrow_mut().deliver(rows)?;
                results.push(result);
            }
            Ok((results, 0))
        })
    }
}

#[cfg(test)]
mod tests;
