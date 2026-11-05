//! Exercise the shared CLI parser; invalid maintenance input must refuse
//! before opening any database, and zero limits must remain explicit limits.

use super::*;
use crate::parse;

fn args(flags: &[(&str, &str)]) -> Vec<String> {
    let mut args = vec!["--db", "unused", "--key-file", "unused"];
    for &(flag, value) in flags {
        args.extend([flag, value]);
    }
    args.into_iter().map(str::to_owned).collect()
}

fn dimensions(options: &SealOptions) -> [usize; 10] {
    [
        options.sealing.max_rows,
        options.sealing.max_incidences,
        options.sealing.max_image_bytes,
        options.sealing.max_property_bytes,
        options.source.max_root_bytes,
        options.source.max_source_bytes,
        options.source.max_blocks,
        options.source.max_vertex_patches,
        options.source.max_incidences,
        options.source.max_vertex_versions,
    ]
}

#[test]
fn seal_requires_an_explicit_relation_and_preserves_full_width_coordinates() {
    assert_eq!(parse(&args(&[]), "seal").err().unwrap().code, 2);
    for raw in ["0", "1", "18446744073709551615"] {
        let options = parse(&args(&[("--seal-relation", raw)]), "seal").unwrap();
        assert_eq!(options.seal.relation, Some(raw.parse().unwrap()));
        assert_eq!(options.seal.floor.unwrap_or(0), 0);
        assert_eq!(options.seal.sealing, SealedLimits::default());
        assert_eq!(options.seal.source, RootReadLimits::default());
        assert!(options.text.is_empty());
    }
    let options = parse(
        &args(&[
            ("--seal-relation", "4"),
            ("--retain-from", "18446744073709551615"),
        ]),
        "seal",
    )
    .unwrap();
    assert_eq!(options.seal.floor, Some(u64::MAX));
}

#[test]
fn each_seal_limit_preserves_zero_without_widening_another_dimension() {
    let defaults = dimensions(&SealOptions::default());
    for (at, flag) in SealOptions::FLAGS[2..].iter().enumerate() {
        let options = parse(&args(&[("--seal-relation", "1"), (flag, "0")]), "seal").unwrap();
        let mut expected = defaults;
        expected[at] = 0;
        assert_eq!(dimensions(&options.seal), expected, "{flag}");
    }
}

#[test]
fn seal_flags_refuse_duplicates_signs_whitespace_and_overflow_before_open() {
    for flag in SealOptions::FLAGS {
        let prefix = if flag == "--seal-relation" {
            vec![]
        } else {
            vec![("--seal-relation", "1")]
        };
        let mut duplicate = prefix.clone();
        duplicate.extend([(flag, "1"), (flag, "2")]);
        let error = parse(&args(&duplicate), "seal").err().unwrap();
        assert_eq!(error.code, 2, "{flag}");
        assert!(error.message.contains("only once"), "{flag}");
        for invalid in ["", "-1", "+1", "1.5", " 1", "1 ", "18446744073709551616"] {
            let mut malformed = prefix.clone();
            malformed.push((flag, invalid));
            assert_eq!(
                parse(&args(&malformed), "seal").err().unwrap().code,
                2,
                "{flag} {invalid:?}"
            );
        }
    }
}

#[test]
fn seal_and_query_flags_do_not_silently_cross_commands() {
    for command in ["create", "query", "write", "compact", "scrub", "search"] {
        for flag in SealOptions::FLAGS[..2]
            .iter()
            .chain(&SealOptions::FLAGS[6..])
        {
            assert_eq!(
                parse(&args(&[(flag, "1")]), command).err().unwrap().code,
                2,
                "{command} {flag}"
            );
        }
    }
    for (flag, value) in [
        ("--param", "x=int:1"),
        ("--write-relation", "1"),
        ("--max-work-units", "1"),
        ("--as-of", "1"),
    ] {
        assert_eq!(
            parse(&args(&[("--seal-relation", "1"), (flag, value)]), "seal")
                .err()
                .unwrap()
                .code,
            2,
            "{flag}"
        );
    }
    let mut with_text = args(&[("--seal-relation", "1")]);
    with_text.push("MATCH (n) RETURN n".to_owned());
    assert_eq!(parse(&with_text, "seal").err().unwrap().code, 2);
}

#[test]
fn seal_refusals_preserve_budget_invalid_floor_and_storage_error_classes() {
    let limit = || SealedError::Limit {
        resource: "image bytes",
        requested: 1,
        limit: 0,
    };
    for error in [
        SealPublicationError::Source(limit()),
        SealPublicationError::Run(AdjRunError::Sealed(limit())),
        SealPublicationError::Origin(OriginBirthError::Limit {
            requested: 1,
            limit: 0,
        }),
        SealPublicationError::Source(SealedError::Store(Box::new(
            fgdb_strata::store::StoreError::ObjectTooLarge {
                observed: 1,
                limit: 0,
            },
        ))),
        SealPublicationError::Publication(fgdb::RebuildError::Store(
            fgdb_strata::store::StoreError::ObjectTooLarge {
                observed: 1,
                limit: 0,
            },
        )),
        SealPublicationError::Publication(fgdb::RebuildError::Store(
            fgdb_strata::store::StoreError::MalformedAdjRun(Box::new(AdjRunError::Sealed(
                SealedError::Limit {
                    resource: "durable run bytes",
                    requested: 2,
                    limit: 1,
                },
            ))),
        )),
    ] {
        let failure = seal_failure(error);
        assert_eq!(failure.code, 3);
        assert_eq!(failure.refusal, crate::refusal::Code::Budget);
    }
    let floor = seal_failure(SealPublicationError::Source(SealedError::InvalidFloor));
    assert_eq!(floor.code, 3);
    assert_eq!(floor.refusal, crate::refusal::Code::InvalidValue);
    // Identity corruption has no std::io::Error in its source chain. It must
    // still reach the documented storage/corruption exit rather than query.
    let corrupt = seal_failure(SealPublicationError::Source(SealedError::Store(Box::new(
        fgdb_strata::store::StoreError::IdentityMismatch {
            expected: fgdb_types::ObjectId([1; 32]),
            actual: fgdb_types::ObjectId([2; 32]),
        },
    ))));
    assert_eq!(corrupt.code, 5);
    assert_eq!(corrupt.refusal, crate::refusal::Code::Io);
}
