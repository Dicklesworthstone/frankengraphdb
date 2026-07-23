#![forbid(unsafe_code)]

//! Deterministic calibration primitives for FrankenGraphDB.
//!
//! This crate binds the statistical cores supplied by asupersync to complete,
//! immutable FrankenGraphDB trial identities. It does not implement a second
//! statistical engine.

pub mod conformal;
pub mod eprocess;
pub mod exploration;
pub mod ope;
pub mod policy_epoch;
pub mod progress;
pub mod regime;
