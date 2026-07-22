//! Core value/type spine of FrankenGraphDB (bead `fgdb-w1-foundation-types-tjk`).
//!
//! Pure types only: no I/O, no durable state, `forbid(unsafe_code)`. Durable
//! *encoders* for these types are generated from the registries by
//! `fgdb-w1-generated-parsers` — the encodings here are the canonical value
//! encodings required for FG-INV-12 coherence (equality ⇒ same hash; total
//! order; encode/decode round trip), not wire formats.

#![forbid(unsafe_code)]

pub mod bytes;
pub mod decimal;
pub mod ids;
pub mod refs;
pub mod scalar;
pub mod temporal;
pub mod text;

pub use bytes::{BoundedBytes, BoundedBytesError};
pub use decimal::{
    CanonicalDecimal, DecimalDecodeError, DecimalError, DecimalOperation, MAX_DECIMAL_COEFFICIENT,
    MIN_DECIMAL_COEFFICIENT, STRICT_PORTABLE_DECIMAL_PRECISION, STRICT_PORTABLE_DECIMAL_SCALE,
};
pub use ids::{
    BranchId, CommitSeq, DatabaseId, DatabaseSecurityNamespaceId, EId, GraphId, ObjectId,
    ServiceVisibilityEpoch, VId, WriterFenceEpoch,
};
pub use refs::{
    CommandRef, ConditionalCoordinateRef, ConditionalMarkerAxis, ConditionalMarkerRef,
    LogicalObjectKind, MarkerRef, StrongRef, WeakDigest,
};
pub use scalar::{
    CanonicalBytes, CanonicalF64, CanonicalScalar, CanonicalScalarResolver, MAX_SCALAR_BYTES,
    MAX_SCALAR_PAYLOAD, ScalarDecodeError, ScalarEncodeError, ScalarField,
};
pub use temporal::{
    CanonicalTimestamp, MAX_TIMESTAMP_UTC_NANOS, MAX_UTC_OFFSET_SECONDS, MAX_ZONE_IDENTIFIER_BYTES,
    MIN_TIMESTAMP_UTC_NANOS, TimestampArtifactError, TimestampConstructionError,
    TimestampDecodeError, TimestampEncodeError, TimestampZone, TzdbResolver,
};
pub use text::{
    CanonicalText, CanonicalTextError, CollationResolver, CollationResolverError,
    MAX_CANONICAL_SORT_KEY_BYTES, MAX_CANONICAL_TEXT_BYTES, NonBinaryTextBinding, TextArtifactRole,
    TextBinding, TextField,
};
