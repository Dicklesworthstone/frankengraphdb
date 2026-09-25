//! `search`: Beacon text, vector and exact-RRF hybrid retrieval over one
//! committed generation, through the embedded `Database::beacon_search`.
//!
//! The corpus is an EXPLICIT projection chosen by flags, never inferred:
//! - `--text <query>` with `--text-property <property>` runs BM25 over that
//!   text property (`--text-match any|all|phrase`, default any, or the
//!   typo-tolerant `fuzzy1|fuzzy2` / `fuzzy1-all|fuzzy2-all` within that edit
//!   distance, bounded by `--max-expansions <n>` vocabulary terms, default
//!   64; exceeding the bound refuses rather than truncating);
//! - `--vector <x,y,...>` with one `--vector-property <property>` per
//!   coordinate, in order, runs a nearest-neighbour search over those numeric
//!   properties (`--metric l2|cosine|dot`, default l2; exact unless
//!   `--ann <ef_search>` asks for the approximate HNSW walk);
//! - both lanes together run the exact reciprocal-rank fusion of the two
//!   candidate sets (`--candidates <n>` per lane, default `--k`), which is
//!   NOT an exhaustive hybrid answer;
//! - `--k <n>` bounds the hits (default 10), `--vertex-label <label>`
//!   restricts the corpus and `--as-of <seq>` pins a committed sequence.
//!
//! The index is built for this one call from the selected generation, so a
//! result always describes exactly the reported `seq`.
use super::{Failure, Options, float_text, render_rows};
use asupersync::fs::Vfs;
use fgdb::Database;
use fgdb_beacon::read::{Projection, ReadOptions, ReadPolicy, Rows, Search};
use fgdb_beacon::{
    DistanceMetric, EditDistance, ExactHybridQuery, ExactRrfProfile, HnswConfig, IndexConfig,
    TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::collections::BTreeMap;
use std::io::Write;

/// Flags as given; symbol names resolve against the bindings once every flag
/// is parsed.
#[derive(Default)]
pub(super) struct SearchFlags {
    text: Option<String>,
    text_property: Option<String>,
    text_match: Option<TextMatch>,
    max_expansions: Option<usize>,
    vector: Option<Vec<f32>>,
    vector_properties: Vec<String>,
    metric: Option<DistanceMetric>,
    ann: Option<usize>,
    k: Option<usize>,
    candidates: Option<u32>,
    vertex_label: Option<String>,
    as_of: Option<CommitSeq>,
}

/// Vocabulary terms a fuzzy text match may expand to unless
/// `--max-expansions` says otherwise. Beacon refuses a query that exceeds it.
const DEFAULT_MAX_EXPANSIONS: usize = 64;

impl SearchFlags {
    pub(super) const FLAGS: [&str; 12] = [
        "--text",
        "--text-property",
        "--text-match",
        "--max-expansions",
        "--vector",
        "--vector-property",
        "--metric",
        "--ann",
        "--k",
        "--candidates",
        "--vertex-label",
        "--as-of",
    ];

    /// Every flag but `--vector-property` (one per coordinate) is given once.
    pub(super) fn set(&mut self, flag: &str, value: &str) -> Result<(), Failure> {
        fn once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), Failure> {
            if slot.replace(value).is_some() {
                return Err(Failure::usage(format!("{flag} is allowed once")));
            }
            Ok(())
        }
        fn positive(value: &str, flag: &str) -> Result<usize, Failure> {
            match value.parse() {
                Ok(n) if n > 0 => Ok(n),
                _ => Err(Failure::usage(format!("{flag} must be a positive integer"))),
            }
        }
        match flag {
            "--text" => once(&mut self.text, value.to_owned(), flag),
            "--text-property" => once(&mut self.text_property, value.to_owned(), flag),
            "--text-match" => {
                let fuzzy = |distance, require_all| TextMatch::Fuzzy {
                    distance,
                    require_all,
                    max_expansions: DEFAULT_MAX_EXPANSIONS,
                };
                let mode = match value {
                    "any" => TextMatch::Any,
                    "all" => TextMatch::All,
                    "phrase" => TextMatch::Phrase,
                    "fuzzy1" => fuzzy(EditDistance::One, false),
                    "fuzzy2" => fuzzy(EditDistance::Two, false),
                    "fuzzy1-all" => fuzzy(EditDistance::One, true),
                    "fuzzy2-all" => fuzzy(EditDistance::Two, true),
                    _ => {
                        return Err(Failure::usage(
                            "--text-match is any, all, phrase, fuzzy1, fuzzy2, fuzzy1-all or fuzzy2-all",
                        ));
                    }
                };
                once(&mut self.text_match, mode, flag)
            }
            "--max-expansions" => {
                let bound = value
                    .parse()
                    .map_err(|_| Failure::usage("--max-expansions must be a count"))?;
                once(&mut self.max_expansions, bound, flag)
            }
            "--vector" => {
                let coordinates = value
                    .split(',')
                    .map(|raw| match raw.trim().parse::<f32>() {
                        Ok(x) if x.is_finite() => Ok(x),
                        _ => Err(Failure::usage("--vector is comma-separated finite numbers")),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                once(&mut self.vector, coordinates, flag)
            }
            "--vector-property" => {
                if self.vector_properties.iter().any(|name| name == value) {
                    return Err(Failure::usage("a --vector-property is named once"));
                }
                self.vector_properties.push(value.to_owned());
                Ok(())
            }
            "--metric" => {
                let metric = match value {
                    "l2" => DistanceMetric::SquaredEuclidean,
                    "cosine" => DistanceMetric::Cosine,
                    "dot" => DistanceMetric::NegativeDotProduct,
                    _ => return Err(Failure::usage("--metric is l2, cosine or dot")),
                };
                once(&mut self.metric, metric, flag)
            }
            "--ann" => once(&mut self.ann, positive(value, flag)?, flag),
            "--k" => once(&mut self.k, positive(value, flag)?, flag),
            "--candidates" => {
                let n = u32::try_from(positive(value, flag)?)
                    .map_err(|_| Failure::usage("--candidates must fit in u32"))?;
                once(&mut self.candidates, n, flag)
            }
            "--vertex-label" => once(&mut self.vertex_label, value.to_owned(), flag),
            "--as-of" => {
                let seq = value
                    .parse()
                    .map_err(|_| Failure::usage("--as-of must be a commit sequence"))?;
                once(&mut self.as_of, CommitSeq(seq), flag)
            }
            _ => Err(Failure::usage("unknown search flag")),
        }
    }
}

/// The lane(s) requested, owning the query text and coordinates.
enum Lanes {
    Text(String, TextMatch),
    Vector(Vec<f32>, VectorSearch),
    Hybrid {
        text: String,
        text_mode: TextMatch,
        vector: Vec<f32>,
        vector_mode: VectorSearch,
        candidates: u32,
    },
}

/// A validated search, resolved before the database opens so a refused input
/// never touches storage.
pub(super) struct Prepared {
    read: ReadOptions<PropertyKeyId, LabelId>,
    lanes: Lanes,
    k: usize,
}

fn property(name: &str, bindings: &BTreeMap<String, u32>) -> Result<PropertyKeyId, Failure> {
    bindings
        .get(name)
        .map(|id| PropertyKeyId(u64::from(*id)))
        .ok_or_else(|| Failure::usage(format!("unbound property {name:?}")))
}

pub(super) fn prepare(options: &Options) -> Result<Prepared, Failure> {
    let flags = &options.search;
    let k = flags.k.unwrap_or(10);
    let text = match (&flags.text, &flags.text_property) {
        (Some(query), Some(name)) => Some((query.clone(), property(name, &options.properties)?)),
        (None, None) => None,
        _ => {
            return Err(Failure::usage(
                "--text and --text-property are given together",
            ));
        }
    };
    let vector = match (&flags.vector, flags.vector_properties.is_empty()) {
        (Some(coordinates), false) => {
            if coordinates.len() != flags.vector_properties.len() {
                return Err(Failure::usage(
                    "--vector needs exactly one --vector-property per coordinate",
                ));
            }
            let keys = flags
                .vector_properties
                .iter()
                .map(|name| property(name, &options.properties))
                .collect::<Result<Vec<_>, _>>()?;
            Some((coordinates.clone(), keys))
        }
        (None, true) => None,
        _ => {
            return Err(Failure::usage(
                "--vector and --vector-property are given together",
            ));
        }
    };
    if text.is_none() && flags.text_match.is_some() {
        return Err(Failure::usage("--text-match needs --text"));
    }
    if vector.is_none() && (flags.metric.is_some() || flags.ann.is_some()) {
        return Err(Failure::usage("--metric and --ann need --vector"));
    }
    let hybrid = text.is_some() && vector.is_some();
    if !hybrid && flags.candidates.is_some() {
        return Err(Failure::usage(
            "--candidates applies only to a text and vector search",
        ));
    }
    let vertex_label = flags
        .vertex_label
        .as_ref()
        .map(|name| {
            options
                .labels
                .get(name)
                .map(|id| LabelId(u64::from(*id)))
                .ok_or_else(|| Failure::usage(format!("unbound label {name:?}")))
        })
        .transpose()?;
    let text_mode = match (
        flags.text_match.unwrap_or(TextMatch::Any),
        flags.max_expansions,
    ) {
        (
            TextMatch::Fuzzy {
                distance,
                require_all,
                ..
            },
            Some(max_expansions),
        ) => TextMatch::Fuzzy {
            distance,
            require_all,
            max_expansions,
        },
        (mode, None) => mode,
        (_, Some(_)) => {
            return Err(Failure::usage(
                "--max-expansions applies only to a fuzzy --text-match",
            ));
        }
    };
    let vector_mode =
        flags
            .ann
            .map_or(VectorSearch::Exact, |ef_search| VectorSearch::Approximate {
                ef_search,
            });
    let index = IndexConfig {
        vector: vector.as_ref().map(|(coordinates, _)| {
            HnswConfig::new(
                coordinates.len(),
                flags.metric.unwrap_or(DistanceMetric::SquaredEuclidean),
            )
        }),
        ..IndexConfig::default()
    };
    let read = ReadOptions {
        as_of: flags.as_of,
        vertex_label,
        projection: Projection {
            text: text.as_ref().map(|(_, key)| *key),
            vector: vector
                .as_ref()
                .map(|(_, keys)| keys.clone())
                .unwrap_or_default(),
        },
        index,
        policy: ReadPolicy::default(),
    };
    let lanes = match (text, vector) {
        (Some((text, _)), Some((vector, _))) => {
            let candidates = match flags.candidates {
                Some(n) => n,
                None => u32::try_from(k).map_err(|_| Failure::usage("--k must fit in u32"))?,
            };
            Lanes::Hybrid {
                text,
                text_mode,
                vector,
                vector_mode,
                candidates,
            }
        }
        (Some((text, _)), None) => Lanes::Text(text, text_mode),
        (None, Some((vector, _))) => Lanes::Vector(vector, vector_mode),
        (None, None) => {
            return Err(Failure::usage(
                "search needs --text with --text-property, --vector with --vector-property, or both",
            ));
        }
    };
    Ok(Prepared { read, lanes, k })
}

impl Prepared {
    fn search(&self) -> Search<'_> {
        match &self.lanes {
            Lanes::Text(query, mode) => Search::Text {
                query,
                k: self.k,
                mode: *mode,
            },
            Lanes::Vector(query, mode) => Search::Vector {
                query,
                k: self.k,
                mode: *mode,
            },
            Lanes::Hybrid {
                text,
                text_mode,
                vector,
                vector_mode,
                candidates,
            } => Search::Hybrid(ExactHybridQuery {
                vector,
                text,
                k: self.k,
                vector_candidates: *candidates,
                text_candidates: *candidates,
                vector_mode: *vector_mode,
                text_mode: *text_mode,
                profile: ExactRrfProfile::default(),
            }),
        }
    }
}

pub(super) fn run<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    // Pin the generation explicitly, so the reported seq is exactly the one
    // searched rather than a second, later read of the frontier.
    let mut read = prepared.read.clone();
    let seq = match read.as_of {
        Some(seq) => seq,
        None => db.frontier().map_err(Failure::io)?,
    };
    read.as_of = Some(seq);
    let rows = db
        .beacon_search(cx, &read, prepared.search())
        .map_err(Failure::query)?;
    let (columns, cells): (&[&str], Vec<Vec<String>>) = match &rows {
        Rows::Text(hits) => (
            &["vertex", "score"],
            hits.iter()
                .map(|hit| vec![vertex(hit.id, robot), float(Some(hit.score), robot)])
                .collect(),
        ),
        Rows::Vector(hits) => (
            &["vertex", "distance"],
            hits.iter()
                .map(|hit| vec![vertex(hit.id, robot), float(Some(hit.distance), robot)])
                .collect(),
        ),
        Rows::Hybrid(hits) => (
            &[
                "vertex",
                "score",
                "vector_rank",
                "text_rank",
                "vector_distance",
                "text_score",
            ],
            hits.iter()
                .map(|hit| {
                    vec![
                        vertex(hit.id, robot),
                        if robot {
                            format!(r#"{{"type":"decimal","value":"{}"}}"#, hit.decimal_score)
                        } else {
                            hit.decimal_score.to_string()
                        },
                        rank(hit.vector_rank.map(|r| r.get()), robot),
                        rank(hit.text_rank.map(|r| r.get()), robot),
                        float(hit.vector_distance, robot),
                        float(hit.text_score, robot),
                    ]
                })
                .collect(),
        ),
    };
    let columns: Vec<String> = columns.iter().map(|c| (*c).to_owned()).collect();
    render_rows(&columns, cells, seq.0, "searched", robot, out)
}

/// The query cell grammar: identities are decimal text, floats shortest
/// round-trip text, and an absent lane value is null.
fn vertex(id: VId, robot: bool) -> String {
    if robot {
        format!(r#"{{"type":"vertex","value":"{}"}}"#, id.0)
    } else {
        format!("vertex {}", id.0)
    }
}
fn float(value: Option<f64>, robot: bool) -> String {
    match (value, robot) {
        (Some(v), true) => format!(r#"{{"type":"float","value":"{}"}}"#, float_text(v)),
        (Some(v), false) => float_text(v),
        (None, true) => r#"{"type":"null"}"#.to_owned(),
        (None, false) => "NULL".to_owned(),
    }
}
fn rank(value: Option<u32>, robot: bool) -> String {
    match (value, robot) {
        (Some(v), true) => format!(r#"{{"type":"int","value":"{v}"}}"#),
        (Some(v), false) => v.to_string(),
        (None, true) => r#"{"type":"null"}"#.to_owned(),
        (None, false) => "NULL".to_owned(),
    }
}
