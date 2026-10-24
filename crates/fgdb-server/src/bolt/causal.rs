//! Local Bolt causal admission over the existing authorized read-session owner.
//!
//! A bookmark is the existing adapter's database-qualified lower bound, NOT a
//! signed RYW token, cluster read proof or authority to select another database.
//! Check the frontier pinned by read_session before exposing a transaction or
//! result. Do not poll a newer frontier to bless an older transaction snapshot.
//! A future local bookmark refuses promptly; this adapter has no causal wait
//! queue and never silently drops a requested timeout or impersonated identity.

use super::{CommitSeq, Cx, Failure, Generation, Map, ReadSession, Served, Value, get};
use fgdb_warden::CapabilityToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccessMode {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Options {
    pub(super) mode: AccessMode,
    pub(super) minimum_seq: CommitSeq,
}

/// RUN and BEGIN share their option grammar; executing a write still needs
/// the native writer and signed Write rights. Mode is a routing hint, never
/// permission to write through an explicit read-only transaction.
pub(super) fn options(extra: &Map, database: &str) -> Result<Options, Failure> {
    // Also defend direct in-process callers: get() must not silently select
    // one of two conflicting causal/authority requirements.
    for key in ["db", "mode", "bookmarks", "tx_timeout", "imp_user"] {
        if extra.iter().filter(|(name, _)| name == key).take(2).count() > 1 {
            return Err(Failure::invalid(format!(
                "duplicate transaction option {key}"
            )));
        }
    }
    let mode = match get(extra, "mode") {
        None => AccessMode::Write,
        Some(Value::String(mode)) if mode == "w" => AccessMode::Write,
        Some(Value::String(mode)) if mode == "r" => AccessMode::Read,
        _ => return Err(Failure::invalid("mode must be 'r' or 'w'")),
    };
    for option in ["tx_timeout", "imp_user"] {
        if !matches!(get(extra, option), None | Some(Value::Null)) {
            return Err(Failure::invalid(format!(
                "{option} is not supported by the Bolt transaction adapter"
            )));
        }
    }
    let mut minimum = 0;
    match get(extra, "bookmarks") {
        None | Some(Value::Null) => {}
        Some(Value::List(bookmarks)) => {
            let prefix = format!("fgdb:{database}:");
            for bookmark in bookmarks {
                let sequence = bookmark
                    .as_str()
                    .and_then(|bookmark| bookmark.strip_prefix(&prefix))
                    .filter(|sequence| {
                        !sequence.is_empty()
                            && sequence.len() <= 20
                            && sequence.bytes().all(|byte| byte.is_ascii_digit())
                            && (sequence.len() == 1 || !sequence.starts_with('0'))
                    })
                    .and_then(|sequence| sequence.parse::<u64>().ok())
                    .ok_or_else(|| Failure::invalid("invalid or foreign database bookmark"))?;
                minimum = minimum.max(sequence);
            }
        }
        Some(_) => return Err(Failure::invalid("bookmarks must be a list of strings")),
    }
    Ok(Options {
        mode,
        minimum_seq: CommitSeq(minimum),
    })
}

fn require_frontier(pinned: CommitSeq, minimum: CommitSeq) -> Result<(), Failure> {
    if pinned < minimum {
        // No current position is disclosed. No statement has executed, and
        // the caller does not retain this insufficient session for a retry.
        return Err(Failure::new(
            "Neo.TransientError.General.DatabaseUnavailable",
            "the local bookmark has not been reached; no read was executed",
        ));
    }
    Ok(())
}

/// read_session obtains frontier and authorized snapshot from the SAME view.
/// The returned owner, not a second lookup, is used for the subsequent query.
/// An explicit FOR SYSTEM_TIME still selects intentional history within that
/// session; a bookmark constrains admission, not the statement's time selector.
pub(super) async fn session<'s>(
    cx: &Cx,
    db: &'s Served,
    token: &CapabilityToken,
    database: &str,
    extra: &Map,
) -> Result<(ReadSession<'s>, CommitSeq), Failure> {
    let minimum = options(extra, database)?.minimum_seq;
    let (session, frontier) = super::read_session(cx, db, token)
        .await
        .map_err(Failure::from_refusal)?;
    session
        .generation()
        .check()
        .map_err(|error| Failure::from_refusal(error.into()))?;
    require_frontier(frontier, minimum)?;
    Ok((session, frontier))
}

/// Schema procedures enumerate scoped operator bindings rather than graph
/// records. Preserve that source and its no-bookmark response; a nonzero
/// incoming bookmark still requires causal admission on an authorized session.
/// Without a positive floor, preserve the existing graph-free schema path.
pub(super) async fn schema_generation(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    database: &str,
    extra: &Map,
) -> Result<Generation, Failure> {
    let minimum = options(extra, database)?.minimum_seq;
    if minimum == CommitSeq(0) {
        return db
            .db
            .generation()
            .map_err(|error| Failure::from_refusal(error.into()));
    }
    let (session, frontier) = super::read_session(cx, db, token)
        .await
        .map_err(Failure::from_refusal)?;
    require_frontier(frontier, minimum)?;
    Ok(session.generation())
}

/// Bolt normally sends an empty extra map for RUN inside BEGIN. For existing
/// in-process callers, accept redundant options only when they cannot replace
/// the pinned database or demand a newer snapshot. Never silently reattach the
/// transaction to a different database or let a concurrent commit satisfy it.
pub(super) fn in_transaction(
    extra: &Map,
    database: &str,
    pinned: CommitSeq,
) -> Result<(), Failure> {
    let minimum = options(extra, database)?.minimum_seq;
    match get(extra, "db") {
        None | Some(Value::Null) => {}
        Some(Value::String(name)) if name.is_empty() || name == database => {}
        Some(Value::String(_)) => {
            return Err(Failure::invalid(
                "RUN cannot change an open transaction's database",
            ));
        }
        Some(_) => return Err(Failure::invalid("db must be a string")),
    }
    if pinned < minimum {
        // Unlike a new autocommit read, this pinned owner can never catch up.
        // Refuse rather than invite retries against the very same snapshot.
        return Err(Failure::invalid(
            "bookmark is newer than the fixed transaction snapshot; begin a new transaction",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
