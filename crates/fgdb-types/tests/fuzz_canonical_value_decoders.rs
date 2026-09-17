//! Deterministic encoder-seeded decoder mutation campaign (plan §8.6, §15.4,
//! Appendix A). Replay: FGDB_VALUE_FUZZ_ROUNDS=60000 (default and minimum);
//! increase that knob for a longer campaign. Default target: <90s in debug.
//!
//! Memory evidence is structural, NOT allocator instrumentation: generated
//! inputs are <=4096 bytes; scalar comparable fields and property ordered
//! fields scan present bytes before reserving; text bounds lengths and takes
//! present slices before copying; timestamp bounds zone length before owning
//! it; decimal is allocation-free. Property recursion is bounded by the
//! decoder's MAX_PROPERTY_NESTING_DEPTH. This resolver emits only text.len()
//! key bytes. Elapsed-time assertions detect slow returns, not memory usage,
//! and cannot preempt a hung decoder. No claim of peak-memory measurement.

use fgdb_types::{
    CanonicalDecimal, CanonicalF64, CanonicalList, CanonicalMap, CanonicalMapEntry,
    CanonicalPropertyValue, CanonicalPropertyValueError, CanonicalScalar, CanonicalScalarProfile,
    CanonicalScalarProfileError, CanonicalScalarProfileIdentityVerifier, CanonicalText,
    CanonicalTextError, CanonicalTimestamp, CollationResolver, CollationResolverError,
    DecimalDecodeError, MAX_DECIMAL_COEFFICIENT, MAX_TIMESTAMP_UTC_NANOS, MAX_UTC_OFFSET_SECONDS,
    MIN_DECIMAL_COEFFICIENT, MIN_TIMESTAMP_UTC_NANOS, NonBinaryTextBinding, ObjectId,
    ScalarDecodeError, ScalarField, ScalarProfileArtifactRole, TextArtifactRole, TextField,
    TimestampArtifactError, TimestampConstructionError, TimestampDecodeError, TzdbResolver,
};
use std::fmt::Debug;
use std::time::{Duration, Instant};

const INPUT_BOUND: usize = 4096;
const ACCEPT: Resolver = Resolver(true);
const REFUSE: Resolver = Resolver(false);
const UNICODE: ObjectId = ObjectId([1; 32]);
const NORMALIZATION: ObjectId = ObjectId([2; 32]);
const SEGMENTATION: ObjectId = ObjectId([3; 32]);
const COLLATION: ObjectId = ObjectId([4; 32]);
const TZDB: ObjectId = ObjectId([9; 32]);
const PROFILE: ObjectId = ObjectId([10; 32]);

// Exact, tiny fixture artifact capability; never consults host Unicode/tzdb.
struct Resolver(bool);
impl CollationResolver for Resolver {
    fn artifact_available(&self, oid: &ObjectId) -> bool {
        self.0 && [UNICODE, NORMALIZATION, SEGMENTATION, COLLATION].contains(oid)
    }
    fn canonical_sort_key_len(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
    ) -> Result<usize, CollationResolverError> {
        if !self.0 || *binding != binding_fixture() {
            return Err(CollationResolverError::new(1));
        }
        Ok(text.len())
    }
    fn write_canonical_sort_key(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
        output: &mut [u8],
    ) -> Result<usize, CollationResolverError> {
        let len = self.canonical_sort_key_len(binding, text)?;
        if output.len() != len {
            return Err(CollationResolverError::new(2));
        }
        for (dst, src) in output.iter_mut().zip(text.bytes()) {
            *dst = src.to_ascii_lowercase();
        }
        Ok(len)
    }
    fn canonical_sort_key_matches(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
        candidate: &[u8],
    ) -> Result<bool, CollationResolverError> {
        let len = self.canonical_sort_key_len(binding, text)?;
        Ok(candidate.len() == len
            && candidate
                .iter()
                .copied()
                .eq(text.bytes().map(|b| b.to_ascii_lowercase())))
    }
}
impl TzdbResolver for Resolver {
    fn contains_tzdb(&self, oid: &ObjectId) -> bool {
        self.0 && *oid == TZDB
    }
    fn canonical_utc_offset_seconds(
        &self,
        oid: &ObjectId,
        zone: &str,
        _instant: i128,
    ) -> Option<i32> {
        (self.contains_tzdb(oid) && zone == "Etc/UTC").then_some(0)
    }
}
struct Identity(Vec<u8>);
impl CanonicalScalarProfileIdentityVerifier for Identity {
    fn verify_canonical_scalar_profile_oid(&self, oid: ObjectId, bytes: &[u8]) -> bool {
        oid == PROFILE && bytes == self.0
    }
}
fn binding_fixture() -> NonBinaryTextBinding {
    NonBinaryTextBinding::new(UNICODE, NORMALIZATION, SEGMENTATION, COLLATION)
}
fn profile() -> CanonicalScalarProfile {
    let descriptor = CanonicalScalarProfile::try_canonical_descriptor_bytes(
        UNICODE,
        NORMALIZATION,
        SEGMENTATION,
        TZDB,
        &[COLLATION],
    )
    .expect("fixture descriptor");
    CanonicalScalarProfile::try_new_verified(
        PROFILE,
        UNICODE,
        NORMALIZATION,
        SEGMENTATION,
        TZDB,
        &[COLLATION],
        &Identity(descriptor),
        &ACCEPT,
    )
    .expect("verified fixture profile")
}
fn texts() -> Vec<CanonicalText> {
    let mut values: Vec<_> = ["", "a", "abcdefgh", "abcdefghi", "\0", "é中🦀", "e\u{301}"]
        .into_iter()
        .map(|s| CanonicalText::new_ucs_basic(s).expect("text seed"))
        .collect();
    values.push(
        CanonicalText::new_non_binary("AbÉ中", binding_fixture(), &ACCEPT)
            .expect("artifact-bound text"),
    );
    values
}
fn decimals() -> Vec<CanonicalDecimal> {
    let mut values: Vec<_> = [MIN_DECIMAL_COEFFICIENT, -1, 0, 1, MAX_DECIMAL_COEFFICIENT]
        .into_iter()
        .map(|n| CanonicalDecimal::from_coefficient(n).expect("decimal seed"))
        .collect();
    // Source scales 0 and 38 bracket the accepted scale range; 19 hits ties.
    for (coefficient, scale) in [(1, 0), (15, 19), (25, 19), (-15, 19), (1, 38)] {
        values.push(
            CanonicalDecimal::from_scaled_half_even(coefficient, scale)
                .expect("scale boundary seed"),
        );
    }
    values
}
fn timestamps() -> Vec<CanonicalTimestamp> {
    let mut values: Vec<_> = [
        (MIN_TIMESTAMP_UTC_NANOS, -MAX_UTC_OFFSET_SECONDS),
        (0, 0),
        (1, MAX_UTC_OFFSET_SECONDS),
        (MAX_TIMESTAMP_UTC_NANOS, 0),
    ]
    .into_iter()
    .map(|(n, o)| CanonicalTimestamp::offset_only(n, o).expect("timestamp seed"))
    .collect();
    values.push(
        CanonicalTimestamp::zoned(1_735_689_600_123_456_789, 0, "Etc/UTC", TZDB, &ACCEPT)
            .expect("zoned seed"),
    );
    values
}
fn scalars() -> Vec<CanonicalScalar> {
    let mut values = vec![
        CanonicalScalar::Null,
        CanonicalScalar::Bool(false),
        CanonicalScalar::Bool(true),
    ];
    values.extend([i64::MIN, -1, 0, 1, i64::MAX].map(CanonicalScalar::Int));
    values.extend(decimals().into_iter().map(CanonicalScalar::Decimal));
    values.extend(
        [
            f64::NAN,
            f64::from_bits(0xfff0_0000_0000_0001),
            -0.0,
            0.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::MIN_POSITIVE,
            -1.5,
            1.5,
        ]
        .map(|n| CanonicalScalar::Float(CanonicalF64::new(n))),
    );
    values.extend(texts().into_iter().map(CanonicalScalar::Text));
    values.extend(timestamps().into_iter().map(CanonicalScalar::Timestamp));
    for bytes in [
        vec![],
        vec![0],
        vec![255],
        vec![0; 8],
        vec![255; 9],
        (0..=255).collect(),
    ] {
        values.push(CanonicalScalar::bytes(bytes).expect("bytes seed"));
    }
    values
}
fn properties() -> Vec<CanonicalPropertyValue> {
    let mut values: Vec<_> = scalars()
        .into_iter()
        .map(CanonicalPropertyValue::from)
        .collect();
    let list = CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![
            CanonicalScalar::Int(i64::MIN).into(),
            CanonicalScalar::Text(texts().pop().expect("bound text")).into(),
        ])
        .expect("list seed"),
    );
    let map = CanonicalPropertyValue::Map(
        CanonicalMap::try_new(vec![
            CanonicalMapEntry::new(
                CanonicalText::new_ucs_basic("é").expect("key"),
                list.clone(),
            ),
            CanonicalMapEntry::new(
                texts().pop().expect("artifact key"),
                CanonicalScalar::Timestamp(timestamps().pop().expect("zone")).into(),
            ),
        ])
        .expect("map seed"),
    );
    values.push(CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![]).expect("empty list"),
    ));
    values.push(CanonicalPropertyValue::Map(
        CanonicalMap::try_new(vec![]).expect("empty map"),
    ));
    values.push(list);
    values.push(map.clone());
    values.push(CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![map]).expect("nested map"),
    ));
    values
}

#[test]
fn encoder_seeds_roundtrip_all_value_kinds_and_resolver_policies() {
    let profile = profile();
    for value in scalars() {
        let bytes = value.encode().expect("scalar encode");
        let decoded =
            CanonicalScalar::decode_with_resolver(&bytes, &ACCEPT).expect("scalar decode");
        assert_eq!(decoded, value);
        assert_eq!(decoded.encode().expect("scalar reencode"), bytes);
        let bound = matches!(&value, CanonicalScalar::Text(t) if matches!(t.binding(), fgdb_types::TextBinding::NonBinary(_)))
            || matches!(&value, CanonicalScalar::Timestamp(t) if t.zone().is_some());
        if !bound {
            assert_eq!(
                // ubs:ignore -- exact false match is `CanonicalScalar::decode`, not a JWT decoder.
                CanonicalScalar::decode(&bytes).expect("plain scalar"),
                value
            );
            assert_eq!(
                CanonicalScalar::decode_with_resolver(&bytes, &REFUSE).expect("unbound scalar"),
                value
            );
        }
    }
    for value in texts() {
        let bytes = value.encode().expect("text encode");
        let decoded = CanonicalText::decode_with_resolver(&bytes, &ACCEPT).expect("text decode");
        assert_eq!(decoded, value);
        assert_eq!(decoded.encode().expect("text reencode"), bytes);
        if matches!(value.binding(), fgdb_types::TextBinding::UcsBasic) {
            // ubs:ignore -- exact false match is `CanonicalText::decode`, not a JWT decoder.
            assert_eq!(CanonicalText::decode(&bytes).expect("binary text"), value);
            assert_eq!(
                CanonicalText::decode_with_resolver(&bytes, &REFUSE).expect("binary text"),
                value
            );
        } else {
            assert_eq!(
                // ubs:ignore -- exact false match is `CanonicalText::decode`, not a JWT decoder.
                CanonicalText::decode(&bytes),
                Err(CanonicalTextError::ResolverRequired)
            );
            assert_eq!(
                CanonicalText::decode_with_resolver(&bytes, &REFUSE),
                Err(CanonicalTextError::MissingArtifact {
                    role: TextArtifactRole::UnicodeData,
                    object_id: UNICODE,
                })
            );
            let scalar = CanonicalScalar::Text(value).encode().expect("bound scalar");
            assert_eq!(
                // ubs:ignore -- exact false match is `CanonicalScalar::decode`, not a JWT decoder.
                CanonicalScalar::decode(&scalar),
                Err(ScalarDecodeError::Text(
                    CanonicalTextError::ResolverRequired
                ))
            );
            assert!(matches!(
                CanonicalScalar::decode_with_resolver(&scalar, &REFUSE),
                Err(ScalarDecodeError::Text(
                    CanonicalTextError::MissingArtifact {
                        role: TextArtifactRole::UnicodeData,
                        ..
                    }
                ))
            ));
        }
    }
    for value in decimals() {
        let bytes = value.encode();
        // ubs:ignore -- exact false match is `CanonicalDecimal::decode`, not a JWT decoder.
        let decoded = CanonicalDecimal::decode(&bytes).expect("decimal decode");
    }
    for value in timestamps() {
        let bytes = value.encode().expect("timestamp encode");
        let decoded =
            CanonicalTimestamp::decode_with_resolver(&bytes, &ACCEPT).expect("timestamp decode");
        assert_eq!(decoded, value);
        assert_eq!(decoded.encode().expect("timestamp reencode"), bytes);
        if value.zone().is_none() {
            assert_eq!(
                // ubs:ignore -- exact false match is `CanonicalTimestamp::decode`, not a JWT decoder.
                CanonicalTimestamp::decode(&bytes).expect("offset decode"),
                value
            );
            assert_eq!(
                CanonicalTimestamp::decode_with_resolver(&bytes, &REFUSE).expect("offset decode"),
                value
            );
        } else {
            let missing = TimestampDecodeError::InvalidValue(TimestampConstructionError::Tzdb(
                TimestampArtifactError::MissingTzdbArtifact { required: TZDB },
            ));
            assert_eq!(
                // ubs:ignore -- exact false match is `CanonicalTimestamp::decode`, not a JWT decoder.
                CanonicalTimestamp::decode(&bytes),
                Err(TimestampDecodeError::TzdbResolverRequired)
            );
            assert_eq!(
                CanonicalTimestamp::decode_with_resolver(&bytes, &REFUSE),
                Err(missing)
            );
            let scalar = CanonicalScalar::Timestamp(value)
                .encode()
                .expect("zoned scalar");
            assert_eq!(
                // ubs:ignore -- exact false match is `CanonicalScalar::decode`, not a JWT decoder.
                CanonicalScalar::decode(&scalar),
                Err(ScalarDecodeError::Timestamp(
                    TimestampDecodeError::TzdbResolverRequired
                ))
            );
            assert!(matches!(
                CanonicalScalar::decode_with_resolver(&scalar, &REFUSE),
                Err(ScalarDecodeError::Timestamp(
                    TimestampDecodeError::InvalidValue(TimestampConstructionError::Tzdb(
                        TimestampArtifactError::MissingTzdbArtifact { .. }
                    ))
                ))
            ));
        }
    }
    for value in properties() {
        let bytes = profile
            .encode_value(&value, &ACCEPT)
            .expect("property encode");
        let decoded = profile
            .decode_value_with_resolver(&bytes, &ACCEPT)
            .expect("property decode");
        assert_eq!(decoded, value);
        assert_eq!(
            profile
                .encode_value(&decoded, &ACCEPT)
                .expect("property reencode"),
            bytes
        );
        assert_eq!(
            profile.decode_value_with_resolver(&bytes, &REFUSE),
            Err(CanonicalScalarProfileError::MissingArtifact {
                role: ScalarProfileArtifactRole::UnicodeData,
                object_id: UNICODE,
            })
        );
    }
}

#[test]
fn typed_post_tag_witnesses_reach_payload_and_length_checks() {
    let mut scalar = CanonicalScalar::bytes(vec![1])
        .expect("bytes")
        .encode()
        .expect("encode");
    // ubs:ignore -- fixed test-fixture byte splice, not input-derived indexing.
    scalar[9] = 0;
    for result in [
        // ubs:ignore -- exact false match is `CanonicalScalar::decode`, not a JWT decoder.
        CanonicalScalar::decode(&scalar),
        CanonicalScalar::decode_with_resolver(&scalar, &ACCEPT),
        CanonicalScalar::decode_with_resolver(&scalar, &REFUSE),
    ] {
        assert_eq!(
            result,
            Err(ScalarDecodeError::InvalidComparableMarker {
                tag: 7,
                field: ScalarField::Bytes,
                marker: 0
            })
        );
    }
    let mut text = CanonicalText::new_ucs_basic("x")
        .expect("text")
        .encode()
        .expect("encode");
    // ubs:ignore -- fixed test-fixture byte splice, not input-derived index arithmetic.
    text[2..10].copy_from_slice(&u64::MAX.to_le_bytes());
    for result in [
        // ubs:ignore -- exact false match is `CanonicalText::decode`, not a JWT decoder.
        CanonicalText::decode(&text),
        CanonicalText::decode_with_resolver(&text, &ACCEPT),
        CanonicalText::decode_with_resolver(&text, &REFUSE),
    ] {
        assert!(matches!(
            result,
            Err(CanonicalTextError::LengthOutOfRange {
                field: TextField::Text,
                declared: u64::MAX,
                ..
            })
        ));
    }
    let mut timestamp = timestamps().pop().expect("zone").encode().expect("encode");
    // ubs:ignore -- fixed test-fixture byte splice, not input-derived index arithmetic.
    timestamp[22..24].copy_from_slice(&u16::MAX.to_le_bytes());
    for result in [
        // ubs:ignore -- exact false match is `CanonicalTimestamp::decode`, not a JWT decoder.
        CanonicalTimestamp::decode(&timestamp),
        CanonicalTimestamp::decode_with_resolver(&timestamp, &ACCEPT),
        CanonicalTimestamp::decode_with_resolver(&timestamp, &REFUSE),
    ] {
        assert!(matches!(
            result,
            Err(TimestampDecodeError::ZoneLengthExceedsBound {
                declared: 65535,
                ..
            })
        ));
    }
    assert_eq!(
        // ubs:ignore -- exact false match is `CanonicalDecimal::decode`, not a JWT decoder.
        CanonicalDecimal::decode(&i128::MAX.to_le_bytes()),
        Err(DecimalDecodeError::CoefficientOutOfRange {
            coefficient: i128::MAX
        })
    );
    let profile = profile();
    for value in [
        CanonicalPropertyValue::List(CanonicalList::try_new(vec![]).expect("list")),
        CanonicalPropertyValue::Map(CanonicalMap::try_new(vec![]).expect("map")),
    ] {
        let mut bytes = profile.encode_value(&value, &ACCEPT).expect("encode");
        bytes[1] = 255;
        assert_eq!(
            profile.decode_value_with_resolver(&bytes, &ACCEPT),
            Err(CanonicalScalarProfileError::PropertyValue(
                CanonicalPropertyValueError::InvalidCollectionControl(255)
            ))
        );
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn index(&mut self, len: usize) -> usize {
        (self.next() % len as u64) as usize
    }
}

// Families: ordered scalar, standalone text, standalone decimal, standalone
// timestamp, profile property. Decimal has no tag or declared length: its
// field-inflation operator targets the coefficient instead.
fn mutate(seed: &[u8], family: usize, operator: usize, rng: &mut Rng) -> Vec<u8> {
    let mut bytes = seed.to_vec();
    let index = rng.index(bytes.len());
    match operator {
        0 => bytes[index] ^= 1 << (rng.next() % 8),
        1 => bytes[index] = rng.next() as u8, // replacement can replay a seed
        2 => bytes.truncate(rng.index(bytes.len())),
        3 => match family {
            1 => {
                let offset = if bytes[1] == 0 { 2 } else { 130 };
                bytes[offset..offset + 8].fill(255);
            }
            3 => bytes[22..24].fill(255),
            2 => bytes.fill(127),
            _ => {
                // Inflate an in-band payload/control field without changing tag.
                if bytes.len() == 1 {
                    bytes.push(255);
                } else {
                    let end = bytes.len().min(9);
                    bytes[1..end].fill(255);
                }
            }
        },
        4 => {
            let end = (index + 1 + rng.index(16)).min(seed.len());
            bytes.splice(index..index, seed[index..end].iter().copied());
        }
        5 => {
            for _ in 0..1 + rng.index(16) {
                bytes.push(rng.next() as u8);
            }
        }
        6 => {
            // Keep valid outer tag/version; give it a mismatched payload.
            match family {
                0 | 4 => {
                    bytes[0] = 1;
                    bytes.truncate(1);
                    bytes.push(255);
                }
                1 => {
                    bytes[1] = 0;
                    bytes.truncate(2);
                    bytes.extend_from_slice(&1u64.to_le_bytes());
                    bytes.push(255);
                }
                2 => bytes.copy_from_slice(&i128::MAX.to_le_bytes()),
                3 => bytes[18..22].copy_from_slice(&i32::MAX.to_le_bytes()),
                _ => unreachable!(),
            }
        }
        7 => bytes[index] = seed[index], // explicit preserving replacement replay
        _ => unreachable!(),
    }
    // Every non-replay operator must contribute a genuinely changed input.
    if operator != 7 && bytes == seed {
        bytes[index] ^= 1;
    }
    assert!(bytes.len() <= INPUT_BOUND);
    bytes
}

#[derive(Default, Debug)]
struct Counts {
    ok: usize,
    err: usize,
}
fn check<T: Eq + Debug, E: Debug>(
    bytes: &[u8],
    counts: &mut Counts,
    decode: impl Fn(&[u8]) -> Result<T, E>,
    encode: impl Fn(&T) -> Vec<u8>,
) {
    let started = Instant::now();
    // ubs:ignore -- closure over the canonical decoders under test, not JWT auth.
    match decode(bytes) {
        Ok(value) => {
            counts.ok += 1;
            let canonical = encode(&value);
            assert_eq!(canonical, bytes, "accepted noncanonical bytes");
            // ubs:ignore -- same canonical decode closure, idempotence check, not JWT auth.
            let second = decode(&canonical).expect("successful decode must remain admissible");
            assert_eq!(second, value, "canonical value changed");
            assert_eq!(encode(&second), canonical, "encoding not idempotent");
        }
        Err(_) => counts.err += 1,
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "per-input decoder wall bound exceeded"
    );
}
fn exercise(
    family: usize,
    bytes: &[u8],
    counts: &mut [Counts; 12],
    profile: &CanonicalScalarProfile,
) {
    match family {
        0 => {
            check(bytes, &mut counts[0], CanonicalScalar::decode, |v| {
                v.encode().expect("scalar encode")
            });
            check(
                bytes,
                &mut counts[1],
                |b| CanonicalScalar::decode_with_resolver(b, &ACCEPT),
                |v| v.encode().expect("scalar encode"),
            );
            check(
                bytes,
                &mut counts[2],
                |b| CanonicalScalar::decode_with_resolver(b, &REFUSE),
                |v| v.encode().expect("scalar encode"),
            );
        }
        1 => {
            check(bytes, &mut counts[3], CanonicalText::decode, |v| {
                v.encode().expect("text encode")
            });
            check(
                bytes,
                &mut counts[4],
                |b| CanonicalText::decode_with_resolver(b, &ACCEPT),
                |v| v.encode().expect("text encode"),
            );
            check(
                bytes,
                &mut counts[5],
                |b| CanonicalText::decode_with_resolver(b, &REFUSE),
                |v| v.encode().expect("text encode"),
            );
        }
        2 => check(bytes, &mut counts[6], CanonicalDecimal::decode, |v| {
            v.encode().to_vec()
        }),
        3 => {
            check(bytes, &mut counts[7], CanonicalTimestamp::decode, |v| {
                v.encode().expect("timestamp encode")
            });
            check(
                bytes,
                &mut counts[8],
                |b| CanonicalTimestamp::decode_with_resolver(b, &ACCEPT),
                |v| v.encode().expect("timestamp encode"),
            );
            check(
                bytes,
                &mut counts[9],
                |b| CanonicalTimestamp::decode_with_resolver(b, &REFUSE),
                |v| v.encode().expect("timestamp encode"),
            );
        }
        4 => {
            check(
                bytes,
                &mut counts[10],
                |b| profile.decode_value_with_resolver(b, &ACCEPT),
                |v| profile.encode_value(v, &ACCEPT).expect("property encode"),
            );
            check(
                bytes,
                &mut counts[11],
                |b| profile.decode_value_with_resolver(b, &REFUSE),
                |v| profile.encode_value(v, &REFUSE).expect("property encode"),
            );
        }
        _ => unreachable!(),
    }
}

#[test]
fn fifty_thousand_changed_mutants_preserve_canonical_value_laws() {
    let profile = profile();
    let seeds: [Vec<Vec<u8>>; 5] = [
        scalars()
            .iter()
            .map(|v| v.encode().expect("scalar seed"))
            .collect(),
        texts()
            .iter()
            .map(|v| v.encode().expect("text seed"))
            .collect(),
        decimals().iter().map(|v| v.encode().to_vec()).collect(),
        timestamps()
            .iter()
            .map(|v| v.encode().expect("timestamp seed"))
            .collect(),
        properties()
            .iter()
            .map(|v| profile.encode_value(v, &ACCEPT).expect("property seed"))
            .collect(),
    ];
    let rounds = std::env::var("FGDB_VALUE_FUZZ_ROUNDS")
        .map_or(60_000, |v| v.parse::<usize>().expect("integer rounds")); // ubs:ignore -- knob parse; malformed env value must fail the run loudly.
    assert!(rounds >= 60_000, "campaign knob must not weaken coverage");
    let mut rng = Rng(0xa31c_57e2_409b_861d);
    let mut counts: [Counts; 12] = std::array::from_fn(|_| Counts::default());
    let mut operators = [[0usize; 8]; 5];
    let mut executed = 0usize;
    let mut replayed = 0usize;
    // Every encoder seed also runs through a preserving mutation operator.
    // Such executions establish positives but never count toward the floor.
    for (family, corpus) in seeds.iter().enumerate() {
        for seed in corpus {
            assert!(seed.len() <= INPUT_BOUND);
            let input = mutate(seed, family, 7, &mut rng);
            assert_eq!(&input, seed);
            exercise(family, &input, &mut counts, &profile);
            replayed += 1;
        }
    }
    for round in 0..rounds {
        let family = round % 5;
        let operator = (round / 5) % 8;
        let seed = &seeds[family][rng.index(seeds[family].len())];
        let input = mutate(seed, family, operator, &mut rng);
        exercise(family, &input, &mut counts, &profile);
        operators[family][operator] += 1;
        if input != *seed {
            executed += 1;
        } else {
            replayed += 1;
        }
    }
    assert!(
        executed >= 50_000,
        "only {executed} changed inputs actually executed"
    );
    for (family, operators) in operators.iter().enumerate() {
        assert!(
            operators.iter().all(|&n| n > 0),
            "family {family} missed an operator"
        );
    }
    for (decoder, count) in counts.iter().enumerate() {
        assert!(count.err > 0, "decoder {decoder} lacks rejection coverage");
        if decoder == 11 {
            // A profile-refusing resolver necessarily rejects even Null.
            assert_eq!(count.ok, 0, "profile artifact refusal was bypassed");
        } else {
            assert!(
                count.ok > 0,
                "decoder {decoder} lacks successful canonicalization"
            );
        }
    }
    eprintln!(
        "changed={executed} replayed={replayed}; decoder counters={counts:?}; operators={operators:?}"
    );
}
