//! Ephemeral, transaction-scoped Bolt result ownership.
//!
//! Query IDs name the last executed statement, not the last *remaining*
//! result. Consuming a newer cursor must never make `qid = -1` select an
//! older one. IDs are not reused within a transaction, including empty
//! results and schema procedures. No durable cursor or write owner lives here.
//!
//! The cursor-count and retained-capacity limits bound multiplexing on top of
//! the existing per-statement query limits. Capacity accounting includes value
//! slots and owned payload allocations, not allocator metadata, shared engine
//! state, transient query execution memory or a claim about process RSS.

use super::{Connection, Cx, Failure, Io, Map, Pending, Value};
use std::collections::BTreeMap;
use std::mem::size_of;

const MAX_CURSORS: usize = 64;
const MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;

fn budget(message: &'static str) -> Failure {
    Failure::new("Neo.ClientError.Statement.ExecutionFailed", message)
}

/// All allocation/cancellation failures precede publication of a cursor ID.
pub(super) fn checkpoint(cx: &Cx) -> Result<(), Failure> {
    cx.checkpoint().map_err(|_| {
        Failure::new(
            "Neo.DatabaseError.Statement.ExecutionFailed",
            "Bolt result processing cancelled",
        )
    })
}

pub(super) struct Cursors<T> {
    results: BTreeMap<i64, (T, usize)>,
    last_qid: Option<i64>,
    retained_bytes: usize,
}

impl<T> Cursors<T> {
    pub(super) fn new() -> Self {
        Self {
            results: BTreeMap::new(),
            last_qid: None,
            retained_bytes: 0,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    // Existing causal regressions inspect ownership without consuming rows.
    #[cfg(test)]
    pub(super) fn latest(&self) -> Option<&T> {
        self.results.get(&self.last_qid?).map(|(value, _)| value)
    }

    #[cfg(test)]
    pub(super) fn last_qid(&self) -> Option<i64> {
        self.last_qid
    }

    fn next_qid(&self) -> Result<i64, Failure> {
        match self.last_qid {
            None => Ok(0),
            Some(last) => last
                .checked_add(1)
                .ok_or_else(|| budget("Bolt transaction query IDs exhausted")),
        }
    }

    /// Check the count and ID space before executing another query.
    pub(super) fn check_admission(&self) -> Result<(), Failure> {
        if self.results.len() >= MAX_CURSORS {
            return Err(budget("too many outstanding Bolt transaction results"));
        }
        self.next_qid()?;
        Ok(())
    }

    fn admit_bytes(&self, bytes: usize) -> Result<usize, Failure> {
        self.retained_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_RETAINED_BYTES)
            .ok_or_else(|| budget("Bolt transaction retained-result budget exceeded"))
    }

    /// Failure does not advance the ID or retain any part of the new result.
    pub(super) fn insert(&mut self, value: T, bytes: usize) -> Result<i64, Failure> {
        self.check_admission()?;
        let qid = self.next_qid()?;
        let total = self.admit_bytes(bytes)?;
        self.results.insert(qid, (value, bytes));
        self.last_qid = Some(qid);
        self.retained_bytes = total;
        Ok(qid)
    }

    fn resolve(&self, qid: i64) -> Result<i64, Failure> {
        let qid = match qid {
            -1 => self.last_qid,
            id if id >= 0 => Some(id),
            _ => None,
        }
        .ok_or_else(|| Failure::invalid("invalid Bolt transaction result identifier"))?;
        if !self.results.contains_key(&qid) {
            return Err(Failure::invalid(
                "unknown or consumed Bolt transaction result",
            ));
        }
        Ok(qid)
    }

    /// Move one result into the connection's existing delivery path. That path
    /// holds `&mut Connection` across delivery, so no new RUN can interleave.
    /// If its future is dropped, the occupied active slot prevents reuse until
    /// RESET or failure cleanup; the result is never silently redelivered.
    fn take(&mut self, qid: i64) -> Result<(i64, T, usize), Failure> {
        let qid = self.resolve(qid)?;
        let (value, bytes) = self.results.remove(&qid).expect("resolved cursor");
        self.retained_bytes -= bytes;
        Ok((qid, value, bytes))
    }

    /// Restore a partial result without minting an ID or changing the last
    /// executed statement. Keep its original capacity charge until exhaustion:
    /// partial delivery does not necessarily release the queue allocation.
    fn restore(&mut self, qid: i64, value: T, bytes: usize) -> Result<(), Failure> {
        if qid < 0
            || self.last_qid.is_none_or(|last| qid > last)
            || self.results.contains_key(&qid)
            || self.results.len() >= MAX_CURSORS
        {
            return Err(Failure::invalid("invalid Bolt cursor restoration"));
        }
        let total = self.admit_bytes(bytes)?;
        self.results.insert(qid, (value, bytes));
        self.retained_bytes = total;
        Ok(())
    }
}

impl<'s> Connection<'s> {
    /// Both graph statements and schema procedures publish through one owner.
    pub(super) fn buffer_result(
        &mut self,
        cx: &Cx,
        pending: Pending<'s>,
    ) -> Result<Option<i64>, Failure> {
        checkpoint(cx)?;
        if self.pending.is_some() || (self.transaction.is_some() && pending.write.is_some()) {
            return Err(Failure::invalid("invalid Bolt result ownership transition"));
        }
        if let Some(transaction) = self.transaction.as_mut() {
            let bytes = retained_bytes(cx, &pending)?;
            transaction.cursors.insert(pending, bytes).map(Some)
        } else {
            self.pending = Some(pending);
            Ok(None)
        }
    }

    /// Preserve the existing latest-result entry point for callers/tests.
    pub(super) async fn pull(
        &mut self,
        cx: &Cx,
        io: &mut Io,
        n: i64,
        discard: bool,
    ) -> Result<Map, Failure> {
        self.pull_selected(cx, io, n, -1, discard).await
    }

    pub(super) async fn pull_selected(
        &mut self,
        cx: &Cx,
        io: &mut Io,
        n: i64,
        qid: i64,
        discard: bool,
    ) -> Result<Map, Failure> {
        // Enforce the wire contract here too: tests and other internal callers
        // can construct a Request directly, bypassing the message decoder.
        if (n != -1 && n <= 0) || qid < -1 {
            return Err(Failure::invalid("invalid result identifier or row count"));
        }
        checkpoint(cx)?;
        let Some(transaction) = self.transaction.as_mut() else {
            if qid != -1 {
                return Err(Failure::invalid("autocommit results have no query ID"));
            }
            return self.pull_pending(cx, io, n, discard).await;
        };
        if self.pending.is_some() {
            return Err(Failure::invalid(
                "interrupted Bolt cursor delivery; RESET required",
            ));
        }
        let (id, pending, bytes) = transaction.cursors.take(qid)?;
        self.pending = Some(pending);
        let outcome = self.pull_pending(cx, io, n, discard).await;
        // On error, drop this result. handle() drops all remaining cursors
        // and the pinned transaction before entering FAILED.
        if let Some(pending) = self.pending.take()
            && outcome.is_ok()
        {
            self.transaction
                .as_mut()
                .expect("delivery does not finish a transaction")
                .cursors
                .restore(id, pending, bytes)?;
        }
        outcome
    }
}

fn add(total: &mut usize, bytes: usize) -> Result<(), Failure> {
    *total = total
        .checked_add(bytes)
        .filter(|sum| *sum <= MAX_RETAINED_BYTES)
        .ok_or_else(|| budget("Bolt transaction retained-result budget exceeded"))?;
    Ok(())
}

fn slots<T>(total: &mut usize, capacity: usize) -> Result<(), Failure> {
    let bytes = capacity
        .checked_mul(size_of::<T>())
        .ok_or_else(|| budget("Bolt result capacity overflow"))?;
    add(total, bytes)
}

fn retained_bytes(cx: &Cx, pending: &Pending<'_>) -> Result<usize, Failure> {
    let mut total = size_of::<Pending<'_>>();
    add(&mut total, pending.database.capacity())?;
    slots::<Vec<Value>>(&mut total, pending.rows.capacity())?;
    for row in &pending.rows {
        checkpoint(cx)?;
        slots::<Value>(&mut total, row.capacity())?;
        for value in row {
            value_bytes(cx, value, 0, &mut total)?;
        }
    }
    Ok(total)
}

/// Inline value slots were charged by the enclosing container. Only owned
/// heap allocations are added here; graph structures follow the same rule.
fn value_bytes(cx: &Cx, value: &Value, depth: usize, total: &mut usize) -> Result<(), Failure> {
    checkpoint(cx)?;
    if depth > fgdb_bolt::packstream::MAX_DEPTH {
        return Err(budget("Bolt result nesting limit exceeded"));
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) => Ok(()),
        Value::Bytes(bytes) => add(total, bytes.capacity()),
        Value::String(text) => add(total, text.capacity()),
        Value::List(values) | Value::Struct { fields: values, .. } => {
            slots::<Value>(total, values.capacity())?;
            for value in values {
                value_bytes(cx, value, depth + 1, total)?;
            }
            Ok(())
        }
        Value::Map(entries) => {
            slots::<(String, Value)>(total, entries.capacity())?;
            for (key, value) in entries {
                add(total, key.capacity())?;
                value_bytes(cx, value, depth + 1, total)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
