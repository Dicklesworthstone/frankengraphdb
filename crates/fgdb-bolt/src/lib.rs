//! `fgdb-bolt`: the Bolt-compat subset, `BoltCompatProfileV1` (plan §13.5).
//!
//! An explicit negotiated downgrade, never a drop-in-Neo4j claim: read-only
//! autocommit and read-only explicit transactions. Every RUN is executed
//! through a capability-authorized read session, which cannot express a
//! write, so a mutating statement is refused before any graph access. Writes,
//! subscriptions and the rest of the native surface stay on FGP and HTTP.
//!
//! This crate is the pure protocol: PackStream values, chunk framing,
//! version negotiation and the typed messages of Bolt 5.0. It performs no
//! I/O and owns no database state; `fgdb-server` hosts the listener and maps
//! each message onto the same execution path as every other surface.

#![forbid(unsafe_code)]

pub mod message;
pub mod packstream;

/// The negotiated profile, published in HELLO's SUCCESS metadata.
pub const PROFILE: &str = "BoltCompatProfileV1";
