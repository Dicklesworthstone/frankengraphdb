//! Canonical timestamp values under the `STRICT_PORTABLE` scalar profile.
//!
//! A timestamp records an exact UTC instant in nanoseconds and the UTC offset
//! that was stored with the value.  A zoned timestamp additionally records a
//! normalized IANA-style zone identifier and the content address of the tzdb
//! artifact used to interpret it.  The pair is all-or-nothing: a zone name
//! without an artifact identity, or an artifact identity without a zone name,
//! is rejected.
//!
//! This module never consults the host clock or host timezone database. Zoned
//! construction and decode instead require a caller-provided [`TzdbResolver`]
//! capability that binds one exact artifact identity to one canonical offset
//! at the encoded instant.

use crate::ids::ObjectId;

/// Maximum absolute UTC offset admitted by `STRICT_PORTABLE`.
///
/// Eighteen hours is the closed range used by ISO-style fixed-offset temporal
/// values.  Offset seconds are retained exactly; they are not rounded to a
/// minute.
pub const MAX_UTC_OFFSET_SECONDS: i32 = 18 * 60 * 60;

/// Maximum canonical timezone identifier length, in bytes.
pub const MAX_ZONE_IDENTIFIER_BYTES: usize = 255;

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const MAX_OFFSET_NANOS: i128 = MAX_UTC_OFFSET_SECONDS as i128 * NANOS_PER_SECOND;

/// Lowest UTC nanosecond instant accepted by the profile.
///
/// The small exclusion at the edge of `i128` guarantees that applying any
/// legal stored offset is representable, so local-wall projection cannot
/// overflow.
pub const MIN_TIMESTAMP_UTC_NANOS: i128 = i128::MIN + MAX_OFFSET_NANOS;

/// Highest UTC nanosecond instant accepted by the profile.
pub const MAX_TIMESTAMP_UTC_NANOS: i128 = i128::MAX - MAX_OFFSET_NANOS;

const ENCODING_VERSION: u8 = 1;
const FLAG_ZONE_PRESENT: u8 = 0x01;
const FIXED_ENCODING_BYTES: usize = 1 + 1 + 16 + 4 + 2;
const OBJECT_ID_BYTES: usize = 32;

/// Explicit capability for deterministic timezone resolution.
///
/// Implementations must answer from the exact content-addressed tzdb object
/// named by `tzdb_oid`. Both methods must observe one stable artifact snapshot
/// for the duration of a construction or decode call. Implementations must not
/// fall back to a host timezone database or wall clock.
pub trait TzdbResolver {
    /// Reports whether the exact content-addressed tzdb artifact is available.
    fn contains_tzdb(&self, tzdb_oid: &ObjectId) -> bool;

    /// Resolves the canonical UTC offset for one canonical zone identifier and
    /// UTC instant from the named tzdb artifact. Implementations must return
    /// `None` for aliases and non-canonical spellings as well as absent zones;
    /// they must not rewrite an alias or fall back to a host default.
    fn canonical_utc_offset_seconds(
        &self,
        tzdb_oid: &ObjectId,
        zone_identifier: &str,
        instant_utc_nanos: i128,
    ) -> Option<i32>;
}

/// The canonical zone metadata stored with a zoned timestamp.
///
/// Fields are private so this type can only exist after syntax validation and
/// with its mandatory tzdb artifact identity.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TimestampZone {
    identifier: Box<str>,
    tzdb_oid: ObjectId,
}

impl TimestampZone {
    /// Returns the resolver-certified canonical, case-sensitive identifier.
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// Returns the exact content address of the tzdb artifact for this zone.
    pub const fn tzdb_oid(&self) -> ObjectId {
        self.tzdb_oid
    }
}

/// A `STRICT_PORTABLE` canonical timestamp.
///
/// Equality, hashing, and ordering cover every stored semantic component in
/// field order: UTC instant, stored offset, then optional zone metadata.  Thus
/// equal values necessarily have identical canonical encodings.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalTimestamp {
    instant_utc_nanos: i128,
    utc_offset_seconds: i32,
    zone: Option<TimestampZone>,
}

impl CanonicalTimestamp {
    /// Constructs an offset-only timestamp without timezone resolution.
    pub fn offset_only(
        instant_utc_nanos: i128,
        utc_offset_seconds: i32,
    ) -> Result<Self, TimestampConstructionError> {
        validate_instant(instant_utc_nanos)?;
        validate_offset(utc_offset_seconds)?;
        Ok(Self {
            instant_utc_nanos,
            utc_offset_seconds,
            zone: None,
        })
    }

    /// Constructs a zoned timestamp under an exact tzdb capability.
    ///
    /// The identifier must already use canonical syntax. The resolver must
    /// contain `tzdb_oid`, must recognize the zone in that exact artifact, and
    /// must resolve `utc_offset_seconds` for this precise UTC instant. No
    /// public constructor can create a zoned timestamp without these checks.
    pub fn zoned<R: TzdbResolver + ?Sized>(
        instant_utc_nanos: i128,
        utc_offset_seconds: i32,
        zone_identifier: &str,
        tzdb_oid: ObjectId,
        resolver: &R,
    ) -> Result<Self, TimestampConstructionError> {
        validate_instant(instant_utc_nanos)?;
        validate_offset(utc_offset_seconds)?;
        validate_zone_identifier(zone_identifier)?;
        validate_zone_resolution(
            instant_utc_nanos,
            utc_offset_seconds,
            zone_identifier,
            tzdb_oid,
            resolver,
        )
        .map_err(TimestampConstructionError::Tzdb)?;

        Ok(Self {
            instant_utc_nanos,
            utc_offset_seconds,
            zone: Some(TimestampZone {
                identifier: zone_identifier.into(),
                tzdb_oid,
            }),
        })
    }

    /// Returns the exact stored UTC instant in nanoseconds.
    pub const fn instant_utc_nanos(&self) -> i128 {
        self.instant_utc_nanos
    }

    /// Returns the exact stored UTC offset in seconds.
    pub const fn utc_offset_seconds(&self) -> i32 {
        self.utc_offset_seconds
    }

    /// Returns the zoned metadata, if this is not an offset-only timestamp.
    pub fn zone(&self) -> Option<&TimestampZone> {
        self.zone.as_ref()
    }

    /// Returns the local-wall nanosecond coordinate represented by the stored
    /// instant and offset.
    ///
    /// Construction's instant range makes this addition total and exact.
    pub fn local_wall_nanos(&self) -> i128 {
        self.instant_utc_nanos + i128::from(self.utc_offset_seconds) * NANOS_PER_SECOND
    }

    /// Revalidates this value under an exact tzdb capability.
    ///
    /// Offset-only timestamps need no resolver lookup. Zoned timestamps must
    /// still resolve to their exact stored offset; mere artifact membership is
    /// insufficient.
    pub fn validate_tzdb_binding<R: TzdbResolver + ?Sized>(
        &self,
        resolver: &R,
    ) -> Result<(), TimestampArtifactError> {
        let Some(zone) = &self.zone else {
            return Ok(());
        };
        validate_zone_resolution(
            self.instant_utc_nanos,
            self.utc_offset_seconds,
            &zone.identifier,
            zone.tzdb_oid,
            resolver,
        )
    }

    /// Encodes one timestamp in the unique version-1 canonical form.
    ///
    /// Layout: `version | flags | instant:i128-le | offset:i32-le |
    /// zone_len:u16-le | zone_utf8? | tzdb_oid?`.  The final two fields are
    /// present together exactly when `FLAG_ZONE_PRESENT` is set.
    pub fn encode(&self) -> Result<Vec<u8>, TimestampEncodeError> {
        let zone_len = self.zone.as_ref().map_or(0, |zone| zone.identifier.len());
        let extra = if self.zone.is_some() {
            zone_len
                .checked_add(OBJECT_ID_BYTES)
                .ok_or(TimestampEncodeError::EncodedSizeOverflow)?
        } else {
            0
        };
        let requested = FIXED_ENCODING_BYTES
            .checked_add(extra)
            .ok_or(TimestampEncodeError::EncodedSizeOverflow)?;
        let mut out = Vec::new();
        out.try_reserve_exact(requested)
            .map_err(|_| TimestampEncodeError::AllocationFailed { requested })?;
        out.push(ENCODING_VERSION);
        out.push(if self.zone.is_some() {
            FLAG_ZONE_PRESENT
        } else {
            0
        });
        out.extend_from_slice(&self.instant_utc_nanos.to_le_bytes());
        out.extend_from_slice(&self.utc_offset_seconds.to_le_bytes());
        let encoded_zone_len = u16::try_from(zone_len).map_err(|_| {
            TimestampEncodeError::ZoneLengthInvariantViolation {
                length: zone_len,
                maximum: MAX_ZONE_IDENTIFIER_BYTES,
            }
        })?;
        out.extend_from_slice(&encoded_zone_len.to_le_bytes());
        if let Some(zone) = &self.zone {
            out.extend_from_slice(zone.identifier.as_bytes());
            out.extend_from_slice(zone.tzdb_oid.as_bytes());
        }
        Ok(out)
    }

    /// Decodes an offset-only canonical timestamp.
    ///
    /// Zoned bytes are structurally validated but rejected with
    /// [`TimestampDecodeError::TzdbResolverRequired`]. Use
    /// [`Self::decode_with_resolver`] for zoned values.
    pub fn decode(bytes: &[u8]) -> Result<Self, TimestampDecodeError> {
        let parts = parse_timestamp_bytes(bytes)?;
        if parts.zone.is_some() {
            return Err(TimestampDecodeError::TzdbResolverRequired);
        }
        Self::offset_only(parts.instant_utc_nanos, parts.utc_offset_seconds)
            .map_err(TimestampDecodeError::InvalidValue)
    }

    /// Decodes one entire canonical timestamp under an exact tzdb capability.
    ///
    /// The declared zone length and exact total input length are checked before
    /// UTF-8 validation or allocation. Zoned values are then reconstructed only
    /// after the resolver recomputes and byte-semantically verifies their stored
    /// offset for the encoded instant.
    pub fn decode_with_resolver<R: TzdbResolver + ?Sized>(
        bytes: &[u8],
        resolver: &R,
    ) -> Result<Self, TimestampDecodeError> {
        let parts = parse_timestamp_bytes(bytes)?;
        let Some(zone) = parts.zone else {
            return Self::offset_only(parts.instant_utc_nanos, parts.utc_offset_seconds)
                .map_err(TimestampDecodeError::InvalidValue);
        };
        Self::zoned(
            parts.instant_utc_nanos,
            parts.utc_offset_seconds,
            zone.identifier,
            zone.tzdb_oid,
            resolver,
        )
        .map_err(TimestampDecodeError::InvalidValue)
    }
}

#[derive(Clone, Copy)]
struct DecodedTimestamp<'a> {
    instant_utc_nanos: i128,
    utc_offset_seconds: i32,
    zone: Option<DecodedZone<'a>>,
}

#[derive(Clone, Copy)]
struct DecodedZone<'a> {
    identifier: &'a str,
    tzdb_oid: ObjectId,
}

fn parse_timestamp_bytes(bytes: &[u8]) -> Result<DecodedTimestamp<'_>, TimestampDecodeError> {
    if bytes.len() < FIXED_ENCODING_BYTES {
        return Err(TimestampDecodeError::TooShort {
            minimum: FIXED_ENCODING_BYTES,
            got: bytes.len(),
        });
    }

    let version = bytes[0];
    if version != ENCODING_VERSION {
        return Err(TimestampDecodeError::UnsupportedVersion(version));
    }
    let flags = bytes[1];
    if flags & !FLAG_ZONE_PRESENT != 0 {
        return Err(TimestampDecodeError::UnknownFlags(flags));
    }

    let instant_raw = array_at::<16>(bytes, 2).ok_or(TimestampDecodeError::TooShort {
        minimum: FIXED_ENCODING_BYTES,
        got: bytes.len(),
    })?;
    let offset_raw = array_at::<4>(bytes, 18).ok_or(TimestampDecodeError::TooShort {
        minimum: FIXED_ENCODING_BYTES,
        got: bytes.len(),
    })?;
    let zone_len_raw = array_at::<2>(bytes, 22).ok_or(TimestampDecodeError::TooShort {
        minimum: FIXED_ENCODING_BYTES,
        got: bytes.len(),
    })?;

    let instant_utc_nanos = i128::from_le_bytes(instant_raw);
    let utc_offset_seconds = i32::from_le_bytes(offset_raw);
    validate_instant(instant_utc_nanos).map_err(TimestampDecodeError::InvalidValue)?;
    validate_offset(utc_offset_seconds).map_err(TimestampDecodeError::InvalidValue)?;

    let zone_len = usize::from(u16::from_le_bytes(zone_len_raw));
    if zone_len > MAX_ZONE_IDENTIFIER_BYTES {
        return Err(TimestampDecodeError::ZoneLengthExceedsBound {
            declared: zone_len,
            maximum: MAX_ZONE_IDENTIFIER_BYTES,
        });
    }

    let zoned = flags == FLAG_ZONE_PRESENT;
    if (!zoned && zone_len != 0) || (zoned && zone_len == 0) {
        return Err(TimestampDecodeError::NonCanonicalZoneMetadata { flags, zone_len });
    }

    let expected = FIXED_ENCODING_BYTES
        .checked_add(zone_len)
        .and_then(|len| len.checked_add(if zoned { OBJECT_ID_BYTES } else { 0 }))
        .ok_or(TimestampDecodeError::LengthOverflow)?;
    if bytes.len() != expected {
        return Err(TimestampDecodeError::WrongLength {
            expected,
            got: bytes.len(),
        });
    }

    if !zoned {
        return Ok(DecodedTimestamp {
            instant_utc_nanos,
            utc_offset_seconds,
            zone: None,
        });
    }

    let zone_end = FIXED_ENCODING_BYTES
        .checked_add(zone_len)
        .ok_or(TimestampDecodeError::LengthOverflow)?;
    let zone_bytes =
        bytes
            .get(FIXED_ENCODING_BYTES..zone_end)
            .ok_or(TimestampDecodeError::WrongLength {
                expected,
                got: bytes.len(),
            })?;
    let identifier =
        std::str::from_utf8(zone_bytes).map_err(|_| TimestampDecodeError::InvalidZoneUtf8)?;
    validate_zone_identifier(identifier).map_err(TimestampDecodeError::InvalidValue)?;
    let oid_raw =
        array_at::<OBJECT_ID_BYTES>(bytes, zone_end).ok_or(TimestampDecodeError::WrongLength {
            expected,
            got: bytes.len(),
        })?;
    Ok(DecodedTimestamp {
        instant_utc_nanos,
        utc_offset_seconds,
        zone: Some(DecodedZone {
            identifier,
            tzdb_oid: ObjectId(oid_raw),
        }),
    })
}

fn array_at<const N: usize>(bytes: &[u8], start: usize) -> Option<[u8; N]> {
    let end = start.checked_add(N)?;
    bytes.get(start..end)?.try_into().ok()
}

fn validate_instant(instant_utc_nanos: i128) -> Result<(), TimestampConstructionError> {
    if !(MIN_TIMESTAMP_UTC_NANOS..=MAX_TIMESTAMP_UTC_NANOS).contains(&instant_utc_nanos) {
        return Err(TimestampConstructionError::InstantOutOfRange {
            value: instant_utc_nanos,
            minimum: MIN_TIMESTAMP_UTC_NANOS,
            maximum: MAX_TIMESTAMP_UTC_NANOS,
        });
    }
    Ok(())
}

fn validate_offset(utc_offset_seconds: i32) -> Result<(), TimestampConstructionError> {
    if !(-MAX_UTC_OFFSET_SECONDS..=MAX_UTC_OFFSET_SECONDS).contains(&utc_offset_seconds) {
        return Err(TimestampConstructionError::OffsetOutOfRange {
            seconds: utc_offset_seconds,
            maximum_absolute_seconds: MAX_UTC_OFFSET_SECONDS,
        });
    }
    Ok(())
}

fn validate_zone_resolution<R: TzdbResolver + ?Sized>(
    instant_utc_nanos: i128,
    stored_offset_seconds: i32,
    zone_identifier: &str,
    tzdb_oid: ObjectId,
    resolver: &R,
) -> Result<(), TimestampArtifactError> {
    if !resolver.contains_tzdb(&tzdb_oid) {
        return Err(TimestampArtifactError::MissingTzdbArtifact { required: tzdb_oid });
    }
    let Some(resolved_offset_seconds) =
        resolver.canonical_utc_offset_seconds(&tzdb_oid, zone_identifier, instant_utc_nanos)
    else {
        return Err(TimestampArtifactError::UnknownZone { tzdb_oid });
    };
    if resolved_offset_seconds != stored_offset_seconds {
        return Err(TimestampArtifactError::UtcOffsetMismatch {
            tzdb_oid,
            instant_utc_nanos,
            stored_offset_seconds,
            resolved_offset_seconds,
        });
    }
    Ok(())
}

pub(crate) fn validate_zone_identifier(identifier: &str) -> Result<(), TimestampConstructionError> {
    let bytes = identifier.as_bytes();
    if bytes.is_empty() {
        return Err(TimestampConstructionError::EmptyZoneIdentifier);
    }
    if bytes.len() > MAX_ZONE_IDENTIFIER_BYTES {
        return Err(TimestampConstructionError::ZoneIdentifierTooLong {
            length: bytes.len(),
            maximum: MAX_ZONE_IDENTIFIER_BYTES,
        });
    }

    let mut component_start = 0;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte == b'/' {
            validate_zone_component(bytes, component_start, index)?;
            component_start = index + 1;
            continue;
        }
        if !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+')) {
            return Err(TimestampConstructionError::InvalidZoneIdentifierByte { index, byte });
        }
    }
    validate_zone_component(bytes, component_start, bytes.len())
}

fn validate_zone_component(
    identifier: &[u8],
    start: usize,
    end: usize,
) -> Result<(), TimestampConstructionError> {
    if start == end {
        return Err(TimestampConstructionError::EmptyZoneComponent { index: start });
    }
    let first = identifier[start];
    if !first.is_ascii_alphabetic() {
        return Err(TimestampConstructionError::InvalidZoneComponentStart {
            index: start,
            byte: first,
        });
    }
    Ok(())
}

/// Typed construction-time rejections for [`CanonicalTimestamp`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimestampConstructionError {
    InstantOutOfRange {
        value: i128,
        minimum: i128,
        maximum: i128,
    },
    OffsetOutOfRange {
        seconds: i32,
        maximum_absolute_seconds: i32,
    },
    EmptyZoneIdentifier,
    ZoneIdentifierTooLong {
        length: usize,
        maximum: usize,
    },
    EmptyZoneComponent {
        index: usize,
    },
    InvalidZoneComponentStart {
        index: usize,
        byte: u8,
    },
    InvalidZoneIdentifierByte {
        index: usize,
        byte: u8,
    },
    Tzdb(TimestampArtifactError),
}

impl std::fmt::Display for TimestampConstructionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InstantOutOfRange {
                value,
                minimum,
                maximum,
            } => write!(
                f,
                "UTC instant {value}ns is outside canonical range {minimum}..={maximum}"
            ),
            Self::OffsetOutOfRange {
                seconds,
                maximum_absolute_seconds,
            } => write!(
                f,
                "UTC offset {seconds}s exceeds canonical ±{maximum_absolute_seconds}s bound"
            ),
            Self::EmptyZoneIdentifier => write!(f, "zone identifier is empty"),
            Self::ZoneIdentifierTooLong { length, maximum } => write!(
                f,
                "zone identifier length {length} exceeds canonical bound {maximum}"
            ),
            Self::EmptyZoneComponent { index } => {
                write!(f, "zone identifier has an empty component at byte {index}")
            }
            Self::InvalidZoneComponentStart { index, byte } => write!(
                f,
                "zone component starts with invalid byte {byte:#04x} at byte {index}"
            ),
            Self::InvalidZoneIdentifierByte { index, byte } => write!(
                f,
                "zone identifier has invalid byte {byte:#04x} at byte {index}"
            ),
            Self::Tzdb(source) => write!(f, "invalid timezone binding: {source}"),
        }
    }
}

impl std::error::Error for TimestampConstructionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tzdb(source) => Some(source),
            _ => None,
        }
    }
}

/// Fail-closed tzdb artifact and resolution rejection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimestampArtifactError {
    MissingTzdbArtifact {
        required: ObjectId,
    },
    UnknownZone {
        tzdb_oid: ObjectId,
    },
    UtcOffsetMismatch {
        tzdb_oid: ObjectId,
        instant_utc_nanos: i128,
        stored_offset_seconds: i32,
        resolved_offset_seconds: i32,
    },
}

impl std::fmt::Display for TimestampArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTzdbArtifact { required } => {
                write!(f, "required tzdb artifact {required:?} is unavailable")
            }
            Self::UnknownZone { tzdb_oid } => {
                write!(f, "zone is absent from tzdb artifact {tzdb_oid:?}")
            }
            Self::UtcOffsetMismatch {
                tzdb_oid,
                instant_utc_nanos,
                stored_offset_seconds,
                resolved_offset_seconds,
            } => write!(
                f,
                "stored UTC offset {stored_offset_seconds}s differs from tzdb artifact {tzdb_oid:?} offset {resolved_offset_seconds}s at {instant_utc_nanos}ns"
            ),
        }
    }
}

impl std::error::Error for TimestampArtifactError {}

/// Typed encode-time rejections for [`CanonicalTimestamp::encode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimestampEncodeError {
    EncodedSizeOverflow,
    ZoneLengthInvariantViolation { length: usize, maximum: usize },
    AllocationFailed { requested: usize },
}

impl std::fmt::Display for TimestampEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EncodedSizeOverflow => write!(f, "timestamp encoded size overflow"),
            Self::ZoneLengthInvariantViolation { length, maximum } => write!(
                f,
                "timestamp zone length {length} exceeds the construction invariant {maximum}"
            ),
            Self::AllocationFailed { requested } => write!(
                f,
                "failed to reserve {requested} bytes for canonical timestamp encoding"
            ),
        }
    }
}

impl std::error::Error for TimestampEncodeError {}

/// Typed decode-time rejections for [`CanonicalTimestamp::decode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimestampDecodeError {
    TooShort { minimum: usize, got: usize },
    UnsupportedVersion(u8),
    UnknownFlags(u8),
    ZoneLengthExceedsBound { declared: usize, maximum: usize },
    NonCanonicalZoneMetadata { flags: u8, zone_len: usize },
    LengthOverflow,
    WrongLength { expected: usize, got: usize },
    InvalidZoneUtf8,
    TzdbResolverRequired,
    InvalidValue(TimestampConstructionError),
}

impl std::fmt::Display for TimestampDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort { minimum, got } => {
                write!(
                    f,
                    "timestamp encoding has {got} bytes, minimum is {minimum}"
                )
            }
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported timestamp encoding version {version}")
            }
            Self::UnknownFlags(flags) => {
                write!(f, "timestamp encoding has unknown flags {flags:#04x}")
            }
            Self::ZoneLengthExceedsBound { declared, maximum } => write!(
                f,
                "declared zone length {declared} exceeds canonical bound {maximum}"
            ),
            Self::NonCanonicalZoneMetadata { flags, zone_len } => write!(
                f,
                "zone flag {flags:#04x} and declared length {zone_len} are inconsistent"
            ),
            Self::LengthOverflow => write!(f, "timestamp encoded length overflow"),
            Self::WrongLength { expected, got } => write!(
                f,
                "timestamp encoding has {got} bytes, expected exactly {expected}"
            ),
            Self::InvalidZoneUtf8 => write!(f, "zone identifier is not valid UTF-8"),
            Self::TzdbResolverRequired => {
                write!(f, "zoned timestamp decode requires a TzdbResolver")
            }
            Self::InvalidValue(source) => write!(f, "invalid canonical timestamp: {source}"),
        }
    }
}

impl std::error::Error for TimestampDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidValue(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    const TZDB_OID: ObjectId = ObjectId([0xA5; OBJECT_ID_BYTES]);
    const WINTER_INSTANT: i128 = 1_735_689_600_123_456_789;
    const SUMMER_INSTANT: i128 = 1_751_328_000_123_456_789;

    struct FixtureResolver {
        available: bool,
    }

    impl TzdbResolver for FixtureResolver {
        fn contains_tzdb(&self, tzdb_oid: &ObjectId) -> bool {
            self.available && tzdb_oid == &TZDB_OID
        }

        fn canonical_utc_offset_seconds(
            &self,
            tzdb_oid: &ObjectId,
            zone_identifier: &str,
            instant_utc_nanos: i128,
        ) -> Option<i32> {
            if tzdb_oid != &TZDB_OID {
                return None;
            }
            match zone_identifier {
                "UTC" => Some(0),
                "America/New_York" if instant_utc_nanos == SUMMER_INSTANT => Some(-14_400),
                "America/New_York" => Some(-18_000),
                "Etc/GMT-14" => Some(50_400),
                "Etc/GMT+12" => Some(-43_200),
                "Etc/GMT+1" => Some(-3_600),
                "America/Argentina/Buenos_Aires" => Some(-10_800),
                "America/Port-au-Prince" => Some(-18_000),
                _ => None,
            }
        }
    }

    const AVAILABLE: FixtureResolver = FixtureResolver { available: true };
    const MISSING: FixtureResolver = FixtureResolver { available: false };

    fn zoned() -> CanonicalTimestamp {
        CanonicalTimestamp::zoned(
            WINTER_INSTANT,
            -5 * 60 * 60,
            "America/New_York",
            TZDB_OID,
            &AVAILABLE,
        )
        .unwrap_or_else(|error| panic!("test fixture must be valid: {error}"))
    }

    #[test]
    fn stores_exact_instant_offset_and_zone_binding() {
        let timestamp = zoned();
        assert_eq!(timestamp.instant_utc_nanos(), WINTER_INSTANT);
        assert_eq!(timestamp.utc_offset_seconds(), -18_000);
        assert_eq!(
            timestamp.local_wall_nanos(),
            WINTER_INSTANT - 18_000 * NANOS_PER_SECOND
        );
        let zone = timestamp
            .zone()
            .unwrap_or_else(|| panic!("fixture is zoned"));
        assert_eq!(zone.identifier(), "America/New_York");
        assert_eq!(zone.tzdb_oid(), TZDB_OID);
    }

    #[test]
    fn zoned_construction_rejects_missing_unknown_and_seasonally_wrong_bindings() {
        assert!(matches!(
            CanonicalTimestamp::zoned(
                WINTER_INSTANT,
                -18_000,
                "America/New_York",
                TZDB_OID,
                &MISSING
            ),
            Err(TimestampConstructionError::Tzdb(
                TimestampArtifactError::MissingTzdbArtifact { required: TZDB_OID }
            ))
        ));
        assert!(matches!(
            CanonicalTimestamp::zoned(
                WINTER_INSTANT,
                0,
                "America/Nonexistent",
                TZDB_OID,
                &AVAILABLE
            ),
            Err(TimestampConstructionError::Tzdb(
                TimestampArtifactError::UnknownZone { tzdb_oid: TZDB_OID }
            ))
        ));
        assert!(matches!(
            CanonicalTimestamp::zoned(WINTER_INSTANT, -18_000, "US/Eastern", TZDB_OID, &AVAILABLE),
            Err(TimestampConstructionError::Tzdb(
                TimestampArtifactError::UnknownZone { tzdb_oid: TZDB_OID }
            ))
        ));
        assert!(matches!(
            CanonicalTimestamp::zoned(
                SUMMER_INSTANT,
                -18_000,
                "America/New_York",
                TZDB_OID,
                &AVAILABLE,
            ),
            Err(TimestampConstructionError::Tzdb(
                TimestampArtifactError::UtcOffsetMismatch {
                    stored_offset_seconds: -18_000,
                    resolved_offset_seconds: -14_400,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn instant_and_offset_boundaries_are_exact() {
        for instant in [MIN_TIMESTAMP_UTC_NANOS, 0, MAX_TIMESTAMP_UTC_NANOS] {
            for offset in [-MAX_UTC_OFFSET_SECONDS, 0, MAX_UTC_OFFSET_SECONDS] {
                let timestamp = CanonicalTimestamp::offset_only(instant, offset)
                    .unwrap_or_else(|error| panic!("boundary must be valid: {error}"));
                let _ = timestamp.local_wall_nanos();
            }
        }
        for instant in [i128::MIN, i128::MAX] {
            assert!(matches!(
                CanonicalTimestamp::offset_only(instant, 0),
                Err(TimestampConstructionError::InstantOutOfRange { .. })
            ));
        }
        for offset in [
            -MAX_UTC_OFFSET_SECONDS - 1,
            MAX_UTC_OFFSET_SECONDS + 1,
            i32::MIN,
            i32::MAX,
        ] {
            assert!(matches!(
                CanonicalTimestamp::offset_only(0, offset),
                Err(TimestampConstructionError::OffsetOutOfRange { .. })
            ));
        }
    }

    #[test]
    fn zone_syntax_is_normalized_and_bounded() {
        for (valid, offset) in [
            ("UTC", 0),
            ("Etc/GMT+12", -43_200),
            ("America/Argentina/Buenos_Aires", -10_800),
            ("America/Port-au-Prince", -18_000),
        ] {
            assert!(
                CanonicalTimestamp::zoned(WINTER_INSTANT, offset, valid, TZDB_OID, &AVAILABLE,)
                    .is_ok(),
                "expected valid zone {valid}"
            );
        }

        let invalid = [
            "",
            "/UTC",
            "UTC/",
            "America//New_York",
            " America/New_York",
            "America/New York",
            "America\\New_York",
            "../UTC",
            "3/UTC",
            "América/New_York",
        ];
        for zone in invalid {
            assert!(
                CanonicalTimestamp::zoned(0, 0, zone, TZDB_OID, &AVAILABLE).is_err(),
                "expected invalid zone {zone:?}"
            );
        }

        let too_long = "A".repeat(MAX_ZONE_IDENTIFIER_BYTES + 1);
        assert!(matches!(
            CanonicalTimestamp::zoned(0, 0, &too_long, TZDB_OID, &AVAILABLE),
            Err(TimestampConstructionError::ZoneIdentifierTooLong { .. })
        ));
    }

    #[test]
    fn tzdb_binding_revalidation_is_explicit_and_fail_closed() {
        let timestamp = zoned();
        assert!(timestamp.validate_tzdb_binding(&AVAILABLE).is_ok());
        assert_eq!(
            timestamp.validate_tzdb_binding(&MISSING),
            Err(TimestampArtifactError::MissingTzdbArtifact { required: TZDB_OID })
        );

        struct MustNotResolve;
        impl TzdbResolver for MustNotResolve {
            fn contains_tzdb(&self, _: &ObjectId) -> bool {
                panic!("offset-only validation must not query tzdb availability")
            }

            fn canonical_utc_offset_seconds(&self, _: &ObjectId, _: &str, _: i128) -> Option<i32> {
                panic!("offset-only validation must not resolve a timezone")
            }
        }
        let offset_only = CanonicalTimestamp::offset_only(0, 0)
            .unwrap_or_else(|error| panic!("fixture must be valid: {error}"));
        assert!(offset_only.validate_tzdb_binding(&MustNotResolve).is_ok());
    }

    #[test]
    fn canonical_encoding_round_trips_and_is_unique() {
        let samples = [
            CanonicalTimestamp::offset_only(MIN_TIMESTAMP_UTC_NANOS, -64_800)
                .unwrap_or_else(|error| panic!("fixture must be valid: {error}")),
            CanonicalTimestamp::offset_only(0, 0)
                .unwrap_or_else(|error| panic!("fixture must be valid: {error}")),
            zoned(),
            CanonicalTimestamp::zoned(
                MAX_TIMESTAMP_UTC_NANOS,
                50_400,
                "Etc/GMT-14",
                TZDB_OID,
                &AVAILABLE,
            )
            .unwrap_or_else(|error| panic!("fixture must be valid: {error}")),
        ];
        let mut encodings = HashSet::new();
        for sample in &samples {
            let encoded = sample
                .encode()
                .unwrap_or_else(|error| panic!("fixture encoding must succeed: {error}"));
            let decoded = if sample.zone().is_some() {
                CanonicalTimestamp::decode_with_resolver(&encoded, &AVAILABLE)
            } else {
                CanonicalTimestamp::decode(&encoded)
            };
            assert_eq!(decoded, Ok(sample.clone()));
            assert!(encodings.insert(encoded));
        }
    }

    #[test]
    fn equality_hash_order_and_encoding_are_coherent() {
        let base = zoned();
        let equal = base.clone();
        assert_eq!(base, equal);
        let base_encoding = base
            .encode()
            .unwrap_or_else(|error| panic!("fixture encoding must succeed: {error}"));
        let equal_encoding = equal
            .encode()
            .unwrap_or_else(|error| panic!("fixture encoding must succeed: {error}"));
        assert_eq!(base_encoding, equal_encoding);

        let variants = [
            CanonicalTimestamp::offset_only(base.instant_utc_nanos(), -18_000)
                .unwrap_or_else(|error| panic!("fixture must be valid: {error}")),
            base.clone(),
            CanonicalTimestamp::zoned(base.instant_utc_nanos(), 0, "UTC", TZDB_OID, &AVAILABLE)
                .unwrap_or_else(|error| panic!("fixture must be valid: {error}")),
            CanonicalTimestamp::zoned(
                base.instant_utc_nanos(),
                -18_000,
                "America/Port-au-Prince",
                TZDB_OID,
                &AVAILABLE,
            )
            .unwrap_or_else(|error| panic!("fixture must be valid: {error}")),
        ];
        assert_eq!(variants.iter().cloned().collect::<BTreeSet<_>>().len(), 4);
        assert_eq!(variants.iter().cloned().collect::<HashSet<_>>().len(), 4);
    }

    #[test]
    fn property_style_round_trip_and_total_order() {
        let mut state = 0xD1B5_4A32_D192_ED03_u64;
        let mut samples = Vec::new();
        for index in 0..2_048_u64 {
            state = state
                .wrapping_add(0x9E37_79B9_7F4A_7C15)
                .rotate_left(17)
                .wrapping_mul(0xBF58_476D_1CE4_E5B9);
            let instant = i128::from(state as i64) * NANOS_PER_SECOND + i128::from(index);
            let offset = (state % (u64::from(MAX_UTC_OFFSET_SECONDS as u32) * 2 + 1)) as i32
                - MAX_UTC_OFFSET_SECONDS;
            let value = CanonicalTimestamp::offset_only(instant, offset)
                .unwrap_or_else(|error| panic!("generated fixture must be valid: {error}"));
            let encoding = value
                .encode()
                .unwrap_or_else(|error| panic!("generated encoding must succeed: {error}"));
            assert_eq!(CanonicalTimestamp::decode(&encoding), Ok(value.clone()));
            samples.push(value);
        }

        for window in samples.windows(3) {
            let a = &window[0];
            let b = &window[1];
            let c = &window[2];
            assert_eq!(a.partial_cmp(b), Some(a.cmp(b)));
            if a <= b && b <= c {
                assert!(a <= c);
            }
        }
    }

    #[test]
    fn zoned_decode_requires_and_rechecks_exact_tzdb_resolution() {
        let winter = zoned();
        let winter_bytes = winter
            .encode()
            .unwrap_or_else(|error| panic!("fixture encoding must succeed: {error}"));
        assert_eq!(
            CanonicalTimestamp::decode(&winter_bytes),
            Err(TimestampDecodeError::TzdbResolverRequired)
        );
        assert_eq!(
            CanonicalTimestamp::decode_with_resolver(&winter_bytes, &AVAILABLE),
            Ok(winter)
        );
        assert!(matches!(
            CanonicalTimestamp::decode_with_resolver(&winter_bytes, &MISSING),
            Err(TimestampDecodeError::InvalidValue(
                TimestampConstructionError::Tzdb(TimestampArtifactError::MissingTzdbArtifact {
                    required: TZDB_OID
                })
            ))
        ));

        let mut nonexistent_zone = winter_bytes.clone();
        nonexistent_zone[FIXED_ENCODING_BYTES..FIXED_ENCODING_BYTES + 16]
            .copy_from_slice(b"America/Los_York");
        assert!(matches!(
            CanonicalTimestamp::decode_with_resolver(&nonexistent_zone, &AVAILABLE),
            Err(TimestampDecodeError::InvalidValue(
                TimestampConstructionError::Tzdb(TimestampArtifactError::UnknownZone {
                    tzdb_oid: TZDB_OID
                })
            ))
        ));

        let summer = CanonicalTimestamp::zoned(
            SUMMER_INSTANT,
            -14_400,
            "America/New_York",
            TZDB_OID,
            &AVAILABLE,
        )
        .unwrap_or_else(|error| panic!("summer fixture must be valid: {error}"));
        let mut wrong_seasonal_offset = summer
            .encode()
            .unwrap_or_else(|error| panic!("fixture encoding must succeed: {error}"));
        wrong_seasonal_offset[18..22].copy_from_slice(&(-18_000_i32).to_le_bytes());
        assert!(matches!(
            CanonicalTimestamp::decode_with_resolver(&wrong_seasonal_offset, &AVAILABLE),
            Err(TimestampDecodeError::InvalidValue(
                TimestampConstructionError::Tzdb(TimestampArtifactError::UtcOffsetMismatch {
                    stored_offset_seconds: -18_000,
                    resolved_offset_seconds: -14_400,
                    ..
                })
            ))
        ));
    }

    #[test]
    fn malformed_encodings_fail_without_panicking() {
        let valid = zoned()
            .encode()
            .unwrap_or_else(|error| panic!("fixture encoding must succeed: {error}"));
        for end in 0..valid.len() {
            assert!(CanonicalTimestamp::decode(&valid[..end]).is_err());
        }

        let mut bad_version = valid.clone();
        bad_version[0] = 2;
        assert!(matches!(
            CanonicalTimestamp::decode(&bad_version),
            Err(TimestampDecodeError::UnsupportedVersion(2))
        ));

        let mut bad_flags = valid.clone();
        bad_flags[1] = 0x80;
        assert!(matches!(
            CanonicalTimestamp::decode(&bad_flags),
            Err(TimestampDecodeError::UnknownFlags(0x80))
        ));

        let mut oversized_zone = valid.clone();
        oversized_zone[22..24].copy_from_slice(&256_u16.to_le_bytes());
        assert!(matches!(
            CanonicalTimestamp::decode(&oversized_zone),
            Err(TimestampDecodeError::ZoneLengthExceedsBound { .. })
        ));

        let mut missing_zone_flag = valid.clone();
        missing_zone_flag[1] = 0;
        assert!(matches!(
            CanonicalTimestamp::decode(&missing_zone_flag),
            Err(TimestampDecodeError::NonCanonicalZoneMetadata { .. })
        ));

        let mut empty_zoned = CanonicalTimestamp::offset_only(0, 0)
            .unwrap_or_else(|error| panic!("fixture must be valid: {error}"))
            .encode()
            .unwrap_or_else(|error| panic!("fixture encoding must succeed: {error}"));
        empty_zoned[1] = FLAG_ZONE_PRESENT;
        assert!(matches!(
            CanonicalTimestamp::decode(&empty_zoned),
            Err(TimestampDecodeError::NonCanonicalZoneMetadata { .. })
        ));

        let mut invalid_utf8 = valid.clone();
        invalid_utf8[FIXED_ENCODING_BYTES] = 0xFF;
        assert!(matches!(
            CanonicalTimestamp::decode(&invalid_utf8),
            Err(TimestampDecodeError::InvalidZoneUtf8)
        ));

        let mut invalid_zone = valid.clone();
        invalid_zone[FIXED_ENCODING_BYTES] = b'3';
        assert!(matches!(
            CanonicalTimestamp::decode(&invalid_zone),
            Err(TimestampDecodeError::InvalidValue(
                TimestampConstructionError::InvalidZoneComponentStart { .. }
            ))
        ));

        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(matches!(
            CanonicalTimestamp::decode(&trailing),
            Err(TimestampDecodeError::WrongLength { .. })
        ));
    }

    #[test]
    fn arbitrary_short_and_bounded_bytes_are_rejected_or_canonical() {
        let mut state = 0x6A09_E667_F3BC_C909_u64;
        for len in 0..=400_usize {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes.push(state as u8);
            }
            if let Ok(decoded) = CanonicalTimestamp::decode(&bytes) {
                assert_eq!(decoded.encode(), Ok(bytes));
            }
        }
    }
}
