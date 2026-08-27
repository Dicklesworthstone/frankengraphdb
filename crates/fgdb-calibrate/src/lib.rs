// #[must_use] on functions already returning a must_use type; cosmetic only.
#![allow(clippy::double_must_use)]

#![forbid(unsafe_code)]

//! Deterministic calibration primitives for FrankenGraphDB.
//!
//! This crate binds statistical cores supplied by asupersync to complete,
//! immutable FrankenGraphDB trial identities. The one local exception is the
//! plan-mandated, model-qualified BOCPD plus Shiryaev--Roberts regime signal;
//! it uses bounded checked fixed-point arithmetic and remains advisory.

pub mod ann_recall;
pub mod conformal;
pub mod eprocess;
pub mod exploration;
pub mod log;
pub mod lyapunov;
pub mod no_regret;
pub mod ope;
pub mod policy_epoch;
pub mod progress;
pub mod regime;
pub mod sprt;
