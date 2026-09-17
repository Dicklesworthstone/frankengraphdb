//! Immutable, content-addressed timezone transition tables.
//!
//! Callers supply the canonical zone set and its complete transition history;
//! this module neither loads a host tzdb nor interprets aliases or recurrence
//! rules. The initial offset applies before the first transition, and each
//! transition's offset applies from its UTC second (inclusive) until the next.
//!
//! The canonical artifact is `fgdb:tzdb:transition-table\0`, a big-endian `u16`
//! version (1), and a big-endian `u32` zone count. Each zone contains a `u16`
//! UTF-8 identifier length, identifier bytes, an `i32` initial offset, a `u32`
//! transition count, and `(i64 UTC second, i32 offset)` transition pairs. All
//! integers are big-endian. Zones are strictly lexicographically ordered by
//! identifier bytes, and transitions strictly ordered by UTC second. The full
//! artifact, including the domain and version, is hashed with unkeyed BLAKE3.
//! Artifacts are limited to 64 MiB, 16,384 zones and 1,048,576 total transitions.

use fgdb_types::{
    CollationResolver, CollationResolverError, MAX_UTC_OFFSET_SECONDS, MAX_ZONE_IDENTIFIER_BYTES,
    NonBinaryTextBinding, ObjectId, TzdbResolver,
};

const DOMAIN: &[u8] = b"fgdb:tzdb:transition-table\0";
const VERSION: u16 = 1;
const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;
const MAX_ZONES: usize = 16_384;
const MAX_TRANSITIONS: usize = 1_048_576;
const TRANSITION_BYTES: usize = 12;
const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// One UTC transition; the new offset takes effect at this exact second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TzdbTransition {
    pub instant_utc_seconds: i64,
    pub offset_seconds: i32,
}

/// Caller-supplied canonical zone history, without aliases or host fallbacks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TzdbZone {
    pub identifier: String,
    pub initial_offset_seconds: i32,
    pub transitions: Vec<TzdbTransition>,
}

/// A validated immutable artifact implementing `CanonicalScalarResolver`.
///
/// Only exact, case-sensitive identifiers present in this artifact resolve.
/// Callers declare these names canonical when supplying the table; there is
/// no independent host authority or alias table. Nonbinary collation artifacts
/// are never provided by this resolver.
#[derive(Debug)]
pub struct PinnedTzdb {
    zones: Vec<TzdbZone>,
    bytes: Vec<u8>,
    object_id: ObjectId,
}

/// Bounded, deterministic artifact construction and decoding failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TzdbArtifactError {
    InvalidDomain,
    UnsupportedVersion(u16),
    Truncated,
    TrailingBytes,
    ArtifactTooLarge,
    TooManyZones,
    TooManyTransitions,
    InvalidZoneIdentifier {
        zone: usize,
    },
    ZoneOrder {
        zone: usize,
    },
    TransitionOrder {
        zone: usize,
        transition: usize,
    },
    OffsetOutOfRange {
        zone: usize,
        transition: Option<usize>,
        seconds: i32,
    },
    AllocationFailed,
}

impl std::fmt::Display for TzdbArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDomain => f.write_str("invalid pinned tzdb artifact domain"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported pinned tzdb artifact version {version}")
            }
            Self::Truncated => f.write_str("truncated pinned tzdb artifact"),
            Self::TrailingBytes => f.write_str("trailing bytes in pinned tzdb artifact"),
            Self::ArtifactTooLarge => f.write_str("pinned tzdb artifact exceeds 64 MiB"),
            Self::TooManyZones => f.write_str("pinned tzdb artifact exceeds 16384 zones"),
            Self::TooManyTransitions => {
                f.write_str("pinned tzdb artifact exceeds 1048576 transitions")
            }
            Self::InvalidZoneIdentifier { zone } => {
                write!(f, "invalid canonical identifier at tzdb zone {zone}")
            }
            Self::ZoneOrder { zone } => {
                write!(
                    f,
                    "tzdb zone {zone} is not strictly ordered after its predecessor"
                )
            }
            Self::TransitionOrder { zone, transition } => write!(
                f,
                "tzdb zone {zone} transition {transition} is not strictly ordered"
            ),
            Self::OffsetOutOfRange {
                zone,
                transition,
                seconds,
            } => write!(
                f,
                "tzdb zone {zone} transition {transition:?} has illegal offset {seconds}"
            ),
            Self::AllocationFailed => f.write_str("unable to allocate pinned tzdb artifact"),
        }
    }
}

impl std::error::Error for TzdbArtifactError {}

impl PinnedTzdb {
    /// Validates the supplied order and values without sorting or repairing them.
    ///
    /// Offsets must be within +/-18 hours. Identifiers follow the canonical
    /// timestamp syntax: at most 255 ASCII bytes, nonempty slash-separated
    /// components starting with a letter, then letters, digits, `_`, `-`, `+`.
    pub fn new(zones: Vec<TzdbZone>) -> Result<Self, TzdbArtifactError> {
        let encoded_len = validate_zones(&zones)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(encoded_len)
            .map_err(|_| TzdbArtifactError::AllocationFailed)?;
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&VERSION.to_be_bytes());
        let zone_count = u32::try_from(zones.len()).map_err(|_| TzdbArtifactError::TooManyZones)?;
        bytes.extend_from_slice(&zone_count.to_be_bytes());
        for zone in &zones {
            let identifier_len = u16::try_from(zone.identifier.len())
                .map_err(|_| TzdbArtifactError::ArtifactTooLarge)?;
            bytes.extend_from_slice(&identifier_len.to_be_bytes());
            bytes.extend_from_slice(zone.identifier.as_bytes());
            bytes.extend_from_slice(&zone.initial_offset_seconds.to_be_bytes());
            let transition_count = u32::try_from(zone.transitions.len())
                .map_err(|_| TzdbArtifactError::TooManyTransitions)?;
            bytes.extend_from_slice(&transition_count.to_be_bytes());
            for transition in &zone.transitions {
                bytes.extend_from_slice(&transition.instant_utc_seconds.to_be_bytes());
                bytes.extend_from_slice(&transition.offset_seconds.to_be_bytes());
            }
        }
        let object_id = ObjectId(fgdb_crypto::hash(&bytes).0);
        Ok(Self {
            zones,
            bytes,
            object_id,
        })
    }

    /// Decodes only canonical artifacts, bounding lengths before allocation.
    ///
    /// Unknown versions, trailing bytes, unordered or duplicate records, and
    /// illegal identifiers or offsets are rejected, never normalized.
    pub fn decode(bytes: &[u8]) -> Result<Self, TzdbArtifactError> {
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(TzdbArtifactError::ArtifactTooLarge);
        }
        let mut reader = ArtifactReader { remaining: bytes };
        if reader.take(DOMAIN.len())? != DOMAIN {
            return Err(TzdbArtifactError::InvalidDomain);
        }
        let version = u16::from_be_bytes(reader.array()?);
        if version != VERSION {
            return Err(TzdbArtifactError::UnsupportedVersion(version));
        }
        let zone_count = usize::try_from(u32::from_be_bytes(reader.array()?))
            .map_err(|_| TzdbArtifactError::TooManyZones)?;
        if zone_count > MAX_ZONES {
            return Err(TzdbArtifactError::TooManyZones);
        }
        // Even an empty history needs a length, one name byte, offset and count.
        if zone_count > reader.remaining.len() / 11 {
            return Err(TzdbArtifactError::Truncated);
        }
        let mut zones = Vec::new();
        zones
            .try_reserve_exact(zone_count)
            .map_err(|_| TzdbArtifactError::AllocationFailed)?;
        let mut total_transitions = 0usize;
        for zone_index in 0..zone_count {
            let name_len = usize::from(u16::from_be_bytes(reader.array()?));
            if name_len == 0 || name_len > MAX_ZONE_IDENTIFIER_BYTES {
                return Err(TzdbArtifactError::InvalidZoneIdentifier { zone: zone_index });
            }
            let name = std::str::from_utf8(reader.take(name_len)?)
                .map_err(|_| TzdbArtifactError::InvalidZoneIdentifier { zone: zone_index })?;
            if !valid_identifier(name) {
                return Err(TzdbArtifactError::InvalidZoneIdentifier { zone: zone_index });
            }
            let initial_offset_seconds = i32::from_be_bytes(reader.array()?);
            let transition_count = usize::try_from(u32::from_be_bytes(reader.array()?))
                .map_err(|_| TzdbArtifactError::TooManyTransitions)?;
            total_transitions = total_transitions
                .checked_add(transition_count)
                .filter(|count| *count <= MAX_TRANSITIONS)
                .ok_or(TzdbArtifactError::TooManyTransitions)?;
            if transition_count > reader.remaining.len() / TRANSITION_BYTES {
                return Err(TzdbArtifactError::Truncated);
            }
            let mut transitions = Vec::new();
            transitions
                .try_reserve_exact(transition_count)
                .map_err(|_| TzdbArtifactError::AllocationFailed)?;
            for _ in 0..transition_count {
                transitions.push(TzdbTransition {
                    instant_utc_seconds: i64::from_be_bytes(reader.array()?),
                    offset_seconds: i32::from_be_bytes(reader.array()?),
                });
            }
            let mut identifier = String::new();
            identifier
                .try_reserve_exact(name_len)
                .map_err(|_| TzdbArtifactError::AllocationFailed)?;
            identifier.push_str(name);
            zones.push(TzdbZone {
                identifier,
                initial_offset_seconds,
                transitions,
            });
        }
        if !reader.remaining.is_empty() {
            return Err(TzdbArtifactError::TrailingBytes);
        }
        validate_zones(&zones)?;
        // Fixed-width fields and validation ensure these exact bytes are the
        // canonical representation; decoding need not re-encode the table.
        let mut canonical = Vec::new();
        canonical
            .try_reserve_exact(bytes.len())
            .map_err(|_| TzdbArtifactError::AllocationFailed)?;
        canonical.extend_from_slice(bytes);
        let object_id = ObjectId(fgdb_crypto::hash(bytes).0);
        Ok(Self {
            zones,
            bytes: canonical,
            object_id,
        })
    }

    /// Borrows the exact versioned artifact whose BLAKE3 digest is `object_id`.
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the full content address computed from the canonical artifact.
    pub fn object_id(&self) -> ObjectId {
        self.object_id
    }
}

impl TzdbResolver for PinnedTzdb {
    fn contains_tzdb(&self, tzdb_oid: &ObjectId) -> bool {
        *tzdb_oid == self.object_id
    }

    fn canonical_utc_offset_seconds(
        &self,
        tzdb_oid: &ObjectId,
        zone_identifier: &str,
        instant_utc_nanos: i128,
    ) -> Option<i32> {
        if !self.contains_tzdb(tzdb_oid) {
            return None;
        }
        let index = self
            .zones
            .binary_search_by(|zone| zone.identifier.as_str().cmp(zone_identifier))
            .ok()?;
        let zone = self.zones.get(index)?;
        let after = zone.transitions.partition_point(|transition| {
            // Every i64 second multiplied by 10^9 fits i128. Compare exact
            // nanoseconds rather than truncating negative fractional seconds.
            i128::from(transition.instant_utc_seconds) * NANOS_PER_SECOND <= instant_utc_nanos
        });
        Some(
            after
                .checked_sub(1)
                .and_then(|index| zone.transitions.get(index))
                .map_or(zone.initial_offset_seconds, |transition| {
                    transition.offset_seconds
                }),
        )
    }
}

impl CollationResolver for PinnedTzdb {
    fn artifact_available(&self, _: &ObjectId) -> bool {
        false
    }

    fn canonical_sort_key_len(
        &self,
        _: &NonBinaryTextBinding,
        _: &str,
    ) -> Result<usize, CollationResolverError> {
        Err(CollationResolverError::new(1))
    }

    fn write_canonical_sort_key(
        &self,
        _: &NonBinaryTextBinding,
        _: &str,
        _: &mut [u8],
    ) -> Result<usize, CollationResolverError> {
        Err(CollationResolverError::new(1))
    }

    fn canonical_sort_key_matches(
        &self,
        _: &NonBinaryTextBinding,
        _: &str,
        _: &[u8],
    ) -> Result<bool, CollationResolverError> {
        Err(CollationResolverError::new(1))
    }
}

fn validate_zones(zones: &[TzdbZone]) -> Result<usize, TzdbArtifactError> {
    if zones.len() > MAX_ZONES {
        return Err(TzdbArtifactError::TooManyZones);
    }
    let mut encoded_len = DOMAIN.len() + 2 + 4;
    let mut total_transitions = 0usize;
    let mut previous_name: Option<&str> = None;
    for (zone_index, zone) in zones.iter().enumerate() {
        if !valid_identifier(&zone.identifier) {
            return Err(TzdbArtifactError::InvalidZoneIdentifier { zone: zone_index });
        }
        if previous_name.is_some_and(|previous| previous >= zone.identifier.as_str()) {
            return Err(TzdbArtifactError::ZoneOrder { zone: zone_index });
        }
        previous_name = Some(&zone.identifier);
        validate_offset(zone.initial_offset_seconds, zone_index, None)?;
        total_transitions = total_transitions
            .checked_add(zone.transitions.len())
            .filter(|count| *count <= MAX_TRANSITIONS)
            .ok_or(TzdbArtifactError::TooManyTransitions)?;
        let transition_bytes = zone
            .transitions
            .len()
            .checked_mul(TRANSITION_BYTES)
            .ok_or(TzdbArtifactError::ArtifactTooLarge)?;
        encoded_len = encoded_len
            .checked_add(2 + zone.identifier.len() + 4 + 4)
            .and_then(|length| length.checked_add(transition_bytes))
            .filter(|length| *length <= MAX_ARTIFACT_BYTES)
            .ok_or(TzdbArtifactError::ArtifactTooLarge)?;
        let mut previous_second = None;
        for (transition_index, transition) in zone.transitions.iter().enumerate() {
            if previous_second.is_some_and(|previous| previous >= transition.instant_utc_seconds) {
                return Err(TzdbArtifactError::TransitionOrder {
                    zone: zone_index,
                    transition: transition_index,
                });
            }
            previous_second = Some(transition.instant_utc_seconds);
            validate_offset(
                transition.offset_seconds,
                zone_index,
                Some(transition_index),
            )?;
        }
    }
    Ok(encoded_len)
}

fn validate_offset(
    seconds: i32,
    zone: usize,
    transition: Option<usize>,
) -> Result<(), TzdbArtifactError> {
    if !(-MAX_UTC_OFFSET_SECONDS..=MAX_UTC_OFFSET_SECONDS).contains(&seconds) {
        return Err(TzdbArtifactError::OffsetOutOfRange {
            zone,
            transition,
            seconds,
        });
    }
    Ok(())
}

// Equivalent to fgdb_types::temporal::validate_zone_identifier, which is
// crate-private. No case folding, alias resolution or Unicode normalization.
fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= MAX_ZONE_IDENTIFIER_BYTES
        && identifier.split('/').all(|component| {
            component
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+'))
        })
}

struct ArtifactReader<'a> {
    remaining: &'a [u8],
}

impl<'a> ArtifactReader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], TzdbArtifactError> {
        let (taken, remaining) = self
            .remaining
            .split_at_checked(length)
            .ok_or(TzdbArtifactError::Truncated)?;
        self.remaining = remaining;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], TzdbArtifactError> {
        self.take(N)?
            .try_into()
            .map_err(|_| TzdbArtifactError::Truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> TzdbZone {
        TzdbZone {
            identifier: "Test/History".into(),
            initial_offset_seconds: -1_800,
            transitions: vec![
                TzdbTransition {
                    instant_utc_seconds: -2,
                    offset_seconds: 3_600,
                },
                TzdbTransition {
                    instant_utc_seconds: 0,
                    offset_seconds: 0,
                },
                TzdbTransition {
                    instant_utc_seconds: 10,
                    offset_seconds: -3_600,
                },
            ],
        }
    }

    #[test]
    fn transitions_apply_at_exact_nanosecond_boundaries() {
        let table = PinnedTzdb::new(vec![history()]).unwrap();
        for (instant, expected) in [
            (i128::MIN, -1_800),
            (-2_000_000_001, -1_800),
            (-2_000_000_000, 3_600),
            (-1, 3_600),
            (0, 0),
            (9_999_999_999, 0),
            (10_000_000_000, -3_600),
            (i128::MAX, -3_600),
        ] {
            assert_eq!(
                table.canonical_utc_offset_seconds(&table.object_id(), "Test/History", instant),
                Some(expected),
                "instant {instant}"
            );
        }
        let constant = PinnedTzdb::new(vec![TzdbZone {
            identifier: "Etc/UTC".into(),
            initial_offset_seconds: 0,
            transitions: vec![],
        }])
        .unwrap();
        assert_eq!(
            constant.canonical_utc_offset_seconds(&constant.object_id(), "Etc/UTC", i128::MIN),
            Some(0)
        );
    }

    #[test]
    fn exact_artifact_and_zone_identity_are_required() {
        let table = PinnedTzdb::new(vec![history()]).unwrap();
        let mut wrong_id = table.object_id();
        wrong_id.0[0] ^= 1;
        assert!(!table.contains_tzdb(&wrong_id));
        assert_eq!(
            table.canonical_utc_offset_seconds(&wrong_id, "Test/History", 0),
            None
        );
        for name in ["test/History", "Test/History/", "Alias/History", "UTC"] {
            assert_eq!(
                table.canonical_utc_offset_seconds(&table.object_id(), name, 0),
                None
            );
        }
        assert!(!table.artifact_available(&table.object_id()));
    }

    #[test]
    fn canonical_round_trip_hashes_the_entire_artifact() {
        let table = PinnedTzdb::new(vec![history()]).unwrap();
        let decoded = PinnedTzdb::decode(table.canonical_bytes()).unwrap();
        assert_eq!(decoded.canonical_bytes(), table.canonical_bytes());
        assert_eq!(decoded.object_id(), table.object_id());
        assert_eq!(
            table.object_id(),
            ObjectId(fgdb_crypto::hash(table.canonical_bytes()).0)
        );
        assert_eq!(
            decoded.canonical_utc_offset_seconds(&table.object_id(), "Test/History", -1),
            Some(3_600)
        );
        let mut changed = history();
        changed.transitions[0].offset_seconds += 1;
        assert_ne!(
            PinnedTzdb::new(vec![changed]).unwrap().object_id(),
            table.object_id()
        );
        let empty = PinnedTzdb::new(vec![]).unwrap();
        assert_eq!(
            PinnedTzdb::decode(empty.canonical_bytes())
                .unwrap()
                .object_id(),
            empty.object_id()
        );
    }

    #[test]
    fn malformed_artifacts_fail_closed_before_unbounded_allocation() {
        let table = PinnedTzdb::new(vec![history()]).unwrap();
        let bytes = table.canonical_bytes();
        for end in 0..bytes.len() {
            assert!(PinnedTzdb::decode(&bytes[..end]).is_err(), "prefix {end}");
        }
        let mut malformed = bytes.to_vec();
        malformed.push(0);
        assert!(matches!(
            PinnedTzdb::decode(&malformed),
            Err(TzdbArtifactError::TrailingBytes)
        ));
        malformed = bytes.to_vec();
        malformed[0] ^= 1;
        assert!(matches!(
            PinnedTzdb::decode(&malformed),
            Err(TzdbArtifactError::InvalidDomain)
        ));
        malformed = bytes.to_vec();
        malformed[DOMAIN.len()..DOMAIN.len() + 2].copy_from_slice(&2u16.to_be_bytes());
        assert!(matches!(
            PinnedTzdb::decode(&malformed),
            Err(TzdbArtifactError::UnsupportedVersion(2))
        ));
        malformed = bytes.to_vec();
        malformed[DOMAIN.len() + 2..DOMAIN.len() + 6].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            PinnedTzdb::decode(&malformed),
            Err(TzdbArtifactError::TooManyZones)
        ));
        let name_start = DOMAIN.len() + 6 + 2;
        malformed = bytes.to_vec();
        malformed[name_start] = 0xff;
        assert!(matches!(
            PinnedTzdb::decode(&malformed),
            Err(TzdbArtifactError::InvalidZoneIdentifier { .. })
        ));
        let offset_start = name_start + "Test/History".len();
        malformed = bytes.to_vec();
        malformed[offset_start..offset_start + 4].copy_from_slice(&i32::MIN.to_be_bytes());
        assert!(matches!(
            PinnedTzdb::decode(&malformed),
            Err(TzdbArtifactError::OffsetOutOfRange { .. })
        ));
        let count_start = offset_start + 4;
        malformed = bytes.to_vec();
        malformed[count_start..count_start + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            PinnedTzdb::decode(&malformed),
            Err(TzdbArtifactError::TooManyTransitions)
        ));
        let second_transition = count_start + 4 + TRANSITION_BYTES;
        malformed = bytes.to_vec();
        malformed[second_transition..second_transition + 8].copy_from_slice(&(-2i64).to_be_bytes());
        assert!(matches!(
            PinnedTzdb::decode(&malformed),
            Err(TzdbArtifactError::TransitionOrder { .. })
        ));
    }

    #[test]
    fn constructor_rejects_noncanonical_tables() {
        assert!(matches!(
            PinnedTzdb::new(vec![history(), history()]),
            Err(TzdbArtifactError::ZoneOrder { .. })
        ));
        let mut earlier = history();
        earlier.identifier = "Earlier/Zone".into();
        assert!(matches!(
            PinnedTzdb::new(vec![history(), earlier]),
            Err(TzdbArtifactError::ZoneOrder { .. })
        ));
        for identifier in [
            "",
            "/Test",
            "Test/",
            "Test//Zone",
            "Test/1Zone",
            "Test/Zo.ne",
            "Tést/Zone",
        ] {
            let mut zone = history();
            zone.identifier = identifier.into();
            assert!(matches!(
                PinnedTzdb::new(vec![zone]),
                Err(TzdbArtifactError::InvalidZoneIdentifier { .. })
            ));
        }
        let mut zone = history();
        zone.transitions[1].instant_utc_seconds = -3;
        assert!(matches!(
            PinnedTzdb::new(vec![zone]),
            Err(TzdbArtifactError::TransitionOrder { .. })
        ));
        let mut zone = history();
        zone.transitions[0].offset_seconds = MAX_UTC_OFFSET_SECONDS + 1;
        assert!(matches!(
            PinnedTzdb::new(vec![zone]),
            Err(TzdbArtifactError::OffsetOutOfRange { .. })
        ));
    }
}
