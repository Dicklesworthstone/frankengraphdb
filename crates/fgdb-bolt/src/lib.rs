//! `fgdb-bolt`: the Bolt-compat subset, `BoltCompatProfileV1` (plan §13.5).
//!
//! An explicit negotiated downgrade, never a drop-in-Neo4j claim. The server
//! retains read-only explicit transactions and advertises its bounded
//! no-RETURN autocommit-write extension separately in HELLO metadata.
//! Execution rights come from signed capabilities, never protocol negotiation.
//! Returning writes, subscriptions and other native surfaces use FGP/HTTP;
//! explicit write transactions remain unsupported by the server.
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
