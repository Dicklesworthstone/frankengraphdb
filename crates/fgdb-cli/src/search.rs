//! `search`: Beacon text, vector and exact-RRF hybrid retrieval over one
//! committed generation, through the embedded `EmbeddedReadView::beacon_search`
//! on a read-only open.
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
//! - `--vector-encoding packed --vector-property <embedding>` instead reads
//!   one packed property, as written by `--param e=vector:<x,y,...>`. Its
//!   bytes must contain exactly one finite little-endian f32 per query
//!   coordinate. `--vector-encoding coordinates` is the default. The CLI
//!   never guesses an encoding from dimensions or the first stored value;
//! - `--vector-encoding coordinates-rounded` explicitly rounds finite stored
//!   integers, floats and decimals to f32. Distances describe that projection,
//!   not the original numbers; overflow refuses and underflow is permitted.
//! - both lanes together run the exact reciprocal-rank fusion of the two
//!   candidate sets (`--candidates <n>` per lane, default `--k`), which is
//!   NOT an exhaustive hybrid answer;
//! - `--expand-from <vid,...>` with an explicit `--max-hops <n>` adds the
//!   graph lane: a bounded expansion from those seed vertices
//!   (`--expand-relation <relation>`, default every relation;
//!   `--expand-direction out|in|both`, default out; `--include-seeds
//!   true|false`, default false), fused as a third reciprocal-rank lane
//!   (`--graph-candidates`, default `--candidates`; `--graph-weight`,
//!   default 1) with any requested text/vector lanes. With neither, it runs
//!   graph-only retrieval without projecting properties or creating an index.
//!   Zero hops plus `--include-seeds true` returns admitted seeds, including
//!   isolates, without opening edge cursors. The expansion
//!   reads the indexed incidence of the vertices it reaches, so its cost
//!   follows the reached neighbourhood, not the whole selected edge set;
//! - `--k <n>` bounds the hits (default 10), `--vertex-label <label>`
//!   restricts the corpus and `--as-of <seq>` pins a committed sequence.
//! - `--eligible <vid,...>` restricts result endpoints before candidate limits.
//!   Omission is unrestricted; an empty string selects no results. Corpus
//!   statistics and graph/ANN transit still use the whole selected corpus.
//!
//! Requested text/vector indexes are built for this one call. Graph-only
//! retrieval uses topology at the same cut; every result describes the reported
//! `seq`. All modes retain native work, staging and output limits.
use super::{Failure, Options, float_text, render_rows};
use fgdb::EmbeddedReadView;
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{Projection, ReadOptions, ReadPolicy, Rows, Search, VectorEncoding};
use fgdb_beacon::{
    DistanceMetric, EditDistance, ExactHybridQuery, ExactRrfProfile, GraphHybridQuery, HnswConfig,
    IndexConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
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
    vector_encoding: Option<VectorEncoding>,
    metric: Option<DistanceMetric>,
    ann: Option<usize>,
    k: Option<usize>,
    candidates: Option<u32>,
    vertex_label: Option<String>,
    eligible: Option<Vec<VId>>,
    as_of: Option<CommitSeq>,
    expand_from: Option<Vec<VId>>,
    expand_relation: Option<String>,
    expand_direction: Option<ExpansionDirection>,
    max_hops: Option<u32>,
    include_seeds: Option<bool>,
    graph_candidates: Option<u32>,
    graph_weight: Option<u16>,
}

/// Vocabulary terms a fuzzy text match may expand to unless
/// `--max-expansions` says otherwise. Beacon refuses a query that exceeds it.
const DEFAULT_MAX_EXPANSIONS: usize = 64;

/// Bound ingress occurrences before allocating the sorted membership set.
/// Duplicate IDs count toward this limit even though membership is a set.
const MAX_ELIGIBLE_IDS: usize = 100_000;

impl SearchFlags {
    pub(super) const FLAGS: [&str; 21] = [
        "--text",
        "--text-property",
        "--text-match",
        "--max-expansions",
        "--vector",
        "--vector-property",
        "--vector-encoding",
        "--metric",
        "--ann",
        "--k",
        "--candidates",
        "--vertex-label",
        "--eligible",
        "--as-of",
        "--expand-from",
        "--expand-relation",
        "--expand-direction",
        "--max-hops",
        "--include-seeds",
        "--graph-candidates",
        "--graph-weight",
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
            "--vector-encoding" => {
                let encoding = match value {
                    "coordinates" => VectorEncoding::Coordinates,
                    "coordinates-rounded" => VectorEncoding::CoordinatesRoundedF32,
                    "packed" => VectorEncoding::PackedF32Le,
                    _ => {
                        return Err(Failure::usage(
                            "--vector-encoding is coordinates, coordinates-rounded or packed",
                        ));
                    }
                };
                once(&mut self.vector_encoding, encoding, flag)
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
            "--eligible" => {
                let mut ids = Vec::new();
                if !value.trim().is_empty() {
                    for raw in value.split(',') {
                        if ids.len() == MAX_ELIGIBLE_IDS {
                            return Err(Failure::usage(format!(
                                "--eligible accepts at most {MAX_ELIGIBLE_IDS} vertex identities",
                            )));
                        }
                        let raw = raw.trim();
                        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                            return Err(Failure::usage(
                                "--eligible is comma-separated unsigned decimal vertex identities",
                            ));
                        }
                        let id = raw.parse::<u128>().map_err(|_| {
                            Failure::usage("--eligible vertex identities must fit in u128")
                        })?;
                        ids.push(VId(id));
                    }
                }
                ids.sort_unstable();
                ids.dedup();
                once(&mut self.eligible, ids, flag)
            }
            "--as-of" => {
                let seq = value
                    .parse()
                    .map_err(|_| Failure::usage("--as-of must be a commit sequence"))?;
                once(&mut self.as_of, CommitSeq(seq), flag)
            }
            "--expand-from" => {
                let seeds = value
                    .split(',')
                    .map(|raw| {
                        raw.trim().parse::<u128>().map(VId).map_err(|_| {
                            Failure::usage("--expand-from is comma-separated vertex identities")
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                once(&mut self.expand_from, seeds, flag)
            }
            "--expand-relation" => once(&mut self.expand_relation, value.to_owned(), flag),
            "--expand-direction" => {
                let direction = match value {
                    "out" => ExpansionDirection::Outgoing,
                    "in" => ExpansionDirection::Incoming,
                    "both" => ExpansionDirection::Undirected,
                    _ => return Err(Failure::usage("--expand-direction is out, in or both")),
                };
                once(&mut self.expand_direction, direction, flag)
            }
            "--max-hops" => {
                let hops = value
                    .parse()
                    .map_err(|_| Failure::usage("--max-hops must be a hop count"))?;
                once(&mut self.max_hops, hops, flag)
            }
            "--include-seeds" => {
                let include = match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(Failure::usage("--include-seeds is true or false")),
                };
                once(&mut self.include_seeds, include, flag)
            }
            "--graph-candidates" => {
                let n = u32::try_from(positive(value, flag)?)
                    .map_err(|_| Failure::usage("--graph-candidates must fit in u32"))?;
                once(&mut self.graph_candidates, n, flag)
            }
            "--graph-weight" => {
                let weight = value
                    .parse::<u16>()
                    .ok()
                    .filter(|weight| *weight > 0)
                    .ok_or_else(|| Failure::usage("--graph-weight is a positive u16"))?;
                once(&mut self.graph_weight, weight, flag)
            }
            _ => Err(Failure::usage("unknown search flag")),
        }
    }
}

/// The lane(s) requested, owning the query text and coordinates.
enum Lanes {
    /// Only GraphHybridQuery may construct this neutral text/vector input.
    /// The complete graph request is validated before opening storage.
    GraphOnly(ExactHybridQuery<'static>),
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

/// The graph lane: an explicit expansion from seed vertices, fused as a third
/// reciprocal-rank lane with whichever text/vector lanes were requested.
struct GraphLane {
    seeds: Vec<VId>,
    relation: Option<RelationId>,
    direction: ExpansionDirection,
    max_hops: u32,
    include_seeds: bool,
    candidates: u32,
    weight: u16,
}

/// A validated search, resolved before the database opens so a refused input
/// never touches storage.
pub(super) struct Prepared {
    read: ReadOptions<PropertyKeyId, LabelId>,
    lanes: Lanes,
    k: usize,
    /// Per-lane candidate depth of a fused search (`--candidates`, default k).
    candidates: u32,
    graph: Option<GraphLane>,
    eligible: Option<Vec<VId>>,
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
    let encoding = flags.vector_encoding.unwrap_or_default();
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
            match encoding {
                VectorEncoding::Coordinates | VectorEncoding::CoordinatesRoundedF32
                    if coordinates.len() != flags.vector_properties.len() =>
                {
                    return Err(Failure::usage(
                        "--vector needs exactly one --vector-property per coordinate; \
                         use --vector-encoding packed for one packed embedding property",
                    ));
                }
                VectorEncoding::PackedF32Le if flags.vector_properties.len() != 1 => {
                    return Err(Failure::usage(
                        "--vector-encoding packed needs exactly one --vector-property",
                    ));
                }
                _ => {}
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
    if vector.is_none() && flags.vector_encoding.is_some() {
        return Err(Failure::usage("--vector-encoding needs --vector"));
    }
    let graph = match (&flags.expand_from, flags.max_hops) {
        (Some(seeds), Some(max_hops)) => {
            let relation = flags
                .expand_relation
                .as_ref()
                .map(|name| {
                    options
                        .relations
                        .get(name)
                        .map(|id| RelationId(u64::from(*id)))
                        .ok_or_else(|| Failure::usage(format!("unbound relation {name:?}")))
                })
                .transpose()?;
            Some((seeds.clone(), relation, max_hops))
        }
        (Some(_), None) => {
            return Err(Failure::usage("--expand-from needs an explicit --max-hops"));
        }
        (None, _) => {
            if flags.max_hops.is_some()
                || flags.expand_relation.is_some()
                || flags.expand_direction.is_some()
                || flags.include_seeds.is_some()
                || flags.graph_candidates.is_some()
                || flags.graph_weight.is_some()
            {
                return Err(Failure::usage("graph lane flags need --expand-from"));
            }
            None
        }
    };
    let fused = (text.is_some() && vector.is_some()) || graph.is_some();
    if !fused && flags.candidates.is_some() {
        return Err(Failure::usage(
            "--candidates applies only to a fused search: text and vector, or a graph lane",
        ));
    }
    let candidates = match flags.candidates {
        Some(n) => n,
        None => u32::try_from(k).map_err(|_| Failure::usage("--k must fit in u32"))?,
    };
    let graph = graph.map(|(seeds, relation, max_hops)| GraphLane {
        seeds,
        relation,
        direction: flags
            .expand_direction
            .unwrap_or(ExpansionDirection::Outgoing),
        max_hops,
        include_seeds: flags.include_seeds.unwrap_or(false),
        candidates: flags.graph_candidates.unwrap_or(candidates),
        weight: flags.graph_weight.unwrap_or(1),
    });
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
            let config = HnswConfig::new(
                coordinates.len(),
                flags.metric.unwrap_or(DistanceMetric::SquaredEuclidean),
            );
            // The index lives for this one call: an exact search never reads
            // HNSW links, so do not build them (fgdb-43k4t).
            if flags.ann.is_some() {
                config
            } else {
                config.exact_only()
            }
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
            encoding,
        },
        index,
        policy: ReadPolicy::default(),
    };
    let lanes = match (text, vector) {
        (Some((text, _)), Some((vector, _))) => Lanes::Hybrid {
            text,
            text_mode,
            vector,
            vector_mode,
            candidates,
        },
        (Some((text, _)), None) => Lanes::Text(text, text_mode),
        (None, Some((vector, _))) => Lanes::Vector(vector, vector_mode),
        (None, None) => {
            let graph = graph.as_ref().ok_or_else(|| {
                Failure::usage(
                    "search needs --text with --text-property, --vector with --vector-property, \
                     or --expand-from with an explicit --max-hops",
                )
            })?;
            let query = GraphHybridQuery::graph_only(k, graph.candidates, graph.weight, 60)
                .map_err(Failure::usage)?;
            read.validate_graph_only(query).map_err(Failure::usage)?;
            Lanes::GraphOnly(query.retrieval)
        }
    };
    Ok(Prepared {
        read,
        lanes,
        k,
        candidates,
        graph,
        eligible: flags.eligible.clone(),
    })
}

impl Prepared {
    /// Only index-backed lanes can become a Search. Never manufacture an
    /// empty text/vector query to make topology-only input fit that interface.
    fn search(&self) -> Result<Search<'_>, Failure> {
        Ok(match &self.lanes {
            Lanes::GraphOnly(_) => {
                return Err(Failure::usage("graph-only search requires graph expansion"));
            }
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
        })
    }

    /// The text/vector part of a graph-fused search. A lane that was not
    /// requested gets weight zero and no candidates, so it is never evaluated.
    fn retrieval(&self) -> Result<ExactHybridQuery<'_>, Failure> {
        let profile =
            |vector: u16, text: u16| ExactRrfProfile::new(60, vector, text).map_err(Failure::usage);
        Ok(match &self.lanes {
            Lanes::GraphOnly(query) => *query,
            Lanes::Text(query, mode) => ExactHybridQuery {
                vector: &[],
                text: query,
                k: self.k,
                vector_candidates: 0,
                text_candidates: self.candidates,
                vector_mode: VectorSearch::Exact,
                text_mode: *mode,
                profile: profile(0, 1)?,
            },
            Lanes::Vector(query, mode) => ExactHybridQuery {
                vector: query,
                text: "",
                k: self.k,
                vector_candidates: self.candidates,
                text_candidates: 0,
                vector_mode: *mode,
                text_mode: TextMatch::Any,
                profile: profile(1, 0)?,
            },
            Lanes::Hybrid {
                text,
                text_mode,
                vector,
                vector_mode,
                candidates,
            } => ExactHybridQuery {
                vector,
                text,
                k: self.k,
                vector_candidates: *candidates,
                text_candidates: *candidates,
                vector_mode: *vector_mode,
                text_mode: *text_mode,
                profile: ExactRrfProfile::default(),
            },
        })
    }
}

pub(super) fn run(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    // Pin the generation explicitly, so the reported seq is exactly the one
    // searched rather than a second, later read of the frontier.
    let mut read = prepared.read.clone();
    let seq = read.as_of.unwrap_or(view.frontier());
    read.as_of = Some(seq);
    let eligible = |id| {
        prepared
            .eligible
            .as_ref()
            .is_none_or(|ids| ids.binary_search(&id).is_ok())
    };
    if let Some(graph) = &prepared.graph {
        let query = GraphHybridQuery {
            retrieval: prepared.retrieval()?,
            graph_candidates: graph.candidates,
            graph_weight: graph.weight,
        };
        let expansion = ExpansionSpec {
            seeds: &graph.seeds,
            relation: graph.relation,
            direction: graph.direction,
            max_hops: graph.max_hops,
            include_seeds: graph.include_seeds,
            limits: ExpansionLimits::default(),
        };
        // The indexed sibling expands through snapshot incidence of the
        // reached vertices only; semantics are the whole-graph API's, which
        // tests/cli_robot.rs keeps as its oracle.
        let hits = view
            .beacon_search_graph_indexed_filtered(cx, &read, query, expansion, eligible)
            .map_err(Failure::query)?;
        let columns: Vec<String> = [
            "vertex",
            "score",
            "vector_rank",
            "text_rank",
            "graph_rank",
            "vector_distance",
            "text_score",
            "graph_hops",
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
        let cells = hits
            .iter()
            .map(|hit| {
                vec![
                    vertex(hit.id, robot),
                    decimal(&hit.decimal_score.to_string(), robot),
                    rank(hit.vector_rank.map(|r| r.get()), robot),
                    rank(hit.text_rank.map(|r| r.get()), robot),
                    rank(hit.graph_rank.map(|r| r.get()), robot),
                    float(hit.vector_distance, robot),
                    float(hit.text_score, robot),
                    rank(hit.graph_hops, robot),
                ]
            })
            .collect();
        return render_rows(&columns, cells, seq.0, "searched", robot, out);
    }
    let rows = view
        .beacon_search_filtered(cx, &read, prepared.search()?, eligible)
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
                        decimal(&hit.decimal_score.to_string(), robot),
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
fn decimal(value: &str, robot: bool) -> String {
    if robot {
        format!(r#"{{"type":"decimal","value":"{value}"}}"#)
    } else {
        value.to_owned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_beacon::{BeaconIndex, IndexDocument, WorkBudget};
    use fgdb_types::{CanonicalScalar, bytes::BoundedBytes};

    fn options(parts: &[&str]) -> Options {
        let mut args = vec![
            "--db",
            "unused",
            "--key-file",
            "unused",
            "--property",
            "x=1",
            "--property",
            "y=2",
            "--property",
            "embedding=3",
            "--property",
            "bio=4",
            "--label",
            "Document=1",
        ];
        args.extend_from_slice(parts);
        crate::parse(
            &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            "search",
        )
        .expect("search arguments")
    }

    fn bytes(value: Vec<u8>) -> CanonicalScalar {
        CanonicalScalar::Bytes(BoundedBytes::new(value).unwrap())
    }

    fn vector_request(encoding: &str, metric: &str) -> Prepared {
        let properties: &[&str] = if encoding == "packed" {
            &["--vector-property", "embedding"]
        } else {
            &["--vector-property", "x", "--vector-property", "y"]
        };
        let mut parts = vec![
            "--vector",
            "1,0",
            "--vector-encoding",
            encoding,
            "--metric",
            metric,
            "--k",
            "4",
        ];
        parts.extend_from_slice(properties);
        prepare(&options(&parts)).expect("vector request")
    }

    #[test]
    fn packed_flag_binds_one_property_without_changing_query_dimensions_or_mode() {
        for ann in [false, true] {
            let mut parts = vec![
                "--vector",
                "1,2,3",
                "--vector-property",
                "embedding",
                "--vector-encoding",
                "packed",
                "--metric",
                "cosine",
                "--as-of",
                "7",
                "--eligible",
                "2,1,2",
                "--vertex-label",
                "Document",
            ];
            if ann {
                parts.extend_from_slice(&["--ann", "32"]);
            }
            let request = prepare(&options(&parts)).unwrap();
            assert_eq!(
                request.read.projection.encoding,
                VectorEncoding::PackedF32Le
            );
            assert_eq!(request.read.projection.vector, [PropertyKeyId(3)]);
            assert_eq!(request.read.index.vector.as_ref().unwrap().dimensions, 3);
            assert_eq!(request.read.as_of, Some(CommitSeq(7)));
            assert_eq!(request.read.vertex_label, Some(LabelId(1)));
            assert_eq!(request.eligible, Some(vec![VId(1), VId(2)]));
            let vector = match request.search().unwrap() {
                Search::Vector { query, mode, .. } => Some((query, mode)),
                _ => None,
            };
            let (query, mode) = vector.expect("expected vector search");
            assert_eq!(query, [1.0, 2.0, 3.0]);
            assert_eq!(
                matches!(mode, VectorSearch::Approximate { ef_search: 32 }),
                ann
            );
            assert!(request.read.config_for(request.search().unwrap()).is_ok());
        }
    }

    #[test]
    fn packed_encoding_is_explicit_and_invalid_projection_shapes_refuse() {
        let default = prepare(&options(&["--vector", "1", "--vector-property", "x"])).unwrap();
        assert_eq!(
            default.read.projection.encoding,
            VectorEncoding::Coordinates
        );
        for parts in [
            vec!["--vector", "1,2", "--vector-property", "embedding"],
            vec![
                "--vector",
                "1,2",
                "--vector-property",
                "x",
                "--vector-property",
                "y",
                "--vector-encoding",
                "packed",
            ],
            vec!["--vector", "1,2", "--vector-encoding", "packed"],
            vec![
                "--vector-property",
                "embedding",
                "--vector-encoding",
                "packed",
            ],
            vec![
                "--text",
                "graph",
                "--text-property",
                "bio",
                "--vector-encoding",
                "packed",
            ],
            vec![
                "--text",
                "graph",
                "--text-property",
                "bio",
                "--vector-encoding",
                "coordinates",
            ],
            vec![
                "--vector",
                "1,2",
                "--vector-property",
                "unknown",
                "--vector-encoding",
                "packed",
            ],
        ] {
            let error = prepare(&options(&parts))
                .err()
                .expect("invalid search must refuse");
            assert_eq!(error.code, 2, "{}", error.message);
        }
        for invalid in ["auto", "f64", "PACKED", ""] {
            assert!(
                SearchFlags::default()
                    .set("--vector-encoding", invalid)
                    .is_err()
            );
        }
        let mut flags = SearchFlags::default();
        flags.set("--vector-encoding", "packed").unwrap();
        assert!(flags.set("--vector-encoding", "coordinates").is_err());
    }

    #[test]
    fn packed_projection_uses_native_finite_exact_length_admission() {
        let request = vector_request("packed", "l2");
        let config = request.read.config_for(request.search().unwrap()).unwrap();
        for value in [
            CanonicalScalar::Int(1),
            bytes(vec![]),
            bytes(vec![0; 7]),
            bytes(vec![0; 9]),
            bytes([f32::NAN.to_le_bytes(), 0.0_f32.to_le_bytes()].concat()),
            bytes([f32::INFINITY.to_le_bytes(), 0.0_f32.to_le_bytes()].concat()),
        ] {
            assert!(
                request
                    .read
                    .projection
                    .project(VId(1), &config, |_| Some(&value), &mut WorkBudget::new(100),)
                    .is_err()
            );
        }
        for value in [None, Some(CanonicalScalar::Null)] {
            let document = request
                .read
                .projection
                .project(
                    VId(1),
                    &config,
                    |_| value.as_ref(),
                    &mut WorkBudget::new(100),
                )
                .unwrap();
            assert!(document.vector.is_none());
        }
        let value = bytes([2.0_f32.to_le_bytes(), (-3.0_f32).to_le_bytes()].concat());
        let document = request
            .read
            .projection
            .project(VId(1), &config, |_| Some(&value), &mut WorkBudget::new(100))
            .unwrap();
        assert_eq!(document.vector, Some(vec![2.0, -3.0]));
        assert!(
            request
                .read
                .projection
                .project(VId(1), &config, |_| Some(&value), &mut WorkBudget::new(0),)
                .is_err(),
            "packed decoding must retain the native work budget"
        );
    }

    fn native_hits(request: &Prepared) -> Rows {
        let config = request.read.config_for(request.search().unwrap()).unwrap();
        let documents: Vec<IndexDocument> = [[6_i16, 0], [2, 0], [3, 2], [2, 0]]
            .into_iter()
            .enumerate()
            .map(|(ordinal, coordinates)| {
                let properties = BTreeMap::from([
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::Int(i64::from(coordinates[0])),
                    ),
                    (
                        PropertyKeyId(2),
                        CanonicalScalar::Int(i64::from(coordinates[1])),
                    ),
                    (
                        PropertyKeyId(3),
                        bytes(
                            coordinates
                                .into_iter()
                                .flat_map(|value| f32::from(value).to_le_bytes())
                                .collect(),
                        ),
                    ),
                ]);
                request
                    .read
                    .projection
                    .project(
                        VId(u128::try_from(ordinal).unwrap() + 1),
                        &config,
                        |key| properties.get(&key),
                        &mut WorkBudget::new(1_000),
                    )
                    .unwrap()
            })
            .collect();
        let snapshot = BeaconIndex::build(config, documents, &mut WorkBudget::new(100_000))
            .unwrap()
            .snapshot();
        request
            .search()
            .unwrap()
            .execute(&snapshot, &mut WorkBudget::new(100_000))
            .unwrap()
    }

    #[test]
    fn packed_search_matches_coordinate_ids_order_and_scores_for_every_metric() {
        for metric in ["l2", "cosine", "dot"] {
            assert_eq!(
                native_hits(&vector_request("packed", metric)),
                native_hits(&vector_request("coordinates", metric)),
                "metric={metric}",
            );
        }
        let hits = match native_hits(&vector_request("packed", "l2")) {
            Rows::Vector(hits) => Some(hits),
            _ => None,
        }
        .expect("expected vector hits");
        assert_eq!(
            hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            [VId(2), VId(4), VId(3), VId(1)]
        );
        assert_eq!(
            hits.iter().map(|hit| hit.distance).collect::<Vec<_>>(),
            [1.0, 1.0, 8.0, 25.0]
        );
    }

    #[test]
    fn packed_encoding_reaches_hybrid_and_graph_retrieval_without_changing_fusion() {
        for graph in [false, true] {
            let mut parts = vec![
                "--text",
                "graph",
                "--text-property",
                "bio",
                "--vector",
                "1,0",
                "--vector-property",
                "embedding",
                "--vector-encoding",
                "packed",
                "--k",
                "2",
                "--candidates",
                "4",
            ];
            if graph {
                parts.extend_from_slice(&["--expand-from", "1", "--max-hops", "2"]);
            }
            let request = prepare(&options(&parts)).unwrap();
            assert_eq!(
                request.read.projection.encoding,
                VectorEncoding::PackedF32Le
            );
            assert_eq!(request.graph.is_some(), graph);
            assert!(request.read.config_for(request.search().unwrap()).is_ok());
            let query = request.retrieval().unwrap();
            assert_eq!(query.vector, [1.0, 0.0]);
            assert_eq!(query.text, "graph");
            assert_eq!(query.k, 2);
            assert_eq!((query.vector_candidates, query.text_candidates), (4, 4));
        }
    }

    #[test]
    fn eligibility_distinguishes_omission_empty_and_full_width_membership() {
        let mut flags = SearchFlags::default();
        assert!(flags.eligible.is_none());
        flags
            .set("--eligible", &format!("{}, 0, 7, 7", u128::MAX))
            .unwrap();
        assert_eq!(flags.eligible, Some(vec![VId(0), VId(7), VId(u128::MAX)]));
        assert!(flags.set("--eligible", "7").is_err());
        for empty in ["", "   "] {
            let mut flags = SearchFlags::default();
            flags.set("--eligible", empty).unwrap();
            assert_eq!(flags.eligible, Some(Vec::new()));
        }
        for invalid in [
            "-1",
            "+1",
            "1.0",
            "1,,2",
            ",1",
            "1,",
            "null",
            "340282366920938463463374607431768211456",
        ] {
            assert!(
                SearchFlags::default().set("--eligible", invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn eligibility_bounds_occurrences_before_duplicate_reduction() {
        let at_limit = std::iter::repeat_n("7", MAX_ELIGIBLE_IDS)
            .collect::<Vec<_>>()
            .join(",");
        let mut flags = SearchFlags::default();
        flags.set("--eligible", &at_limit).unwrap();
        assert_eq!(flags.eligible, Some(vec![VId(7)]));
        let too_many = format!("{at_limit},7");
        assert!(SearchFlags::default().set("--eligible", &too_many).is_err());
    }

    #[test]
    fn rounded_encoding_reaches_the_projection_and_retains_exact_search_options() {
        for ann in [false, true] {
            let mut parts = vec![
                "--vector",
                "0.1,0.2",
                "--vector-property",
                "x",
                "--vector-property",
                "y",
                "--vector-encoding",
                "coordinates-rounded",
                "--as-of",
                "7",
                "--eligible",
                "2,1",
                "--vertex-label",
                "Document",
            ];
            if ann {
                parts.extend_from_slice(&["--ann", "32"]);
            }
            let request = prepare(&options(&parts)).unwrap();
            assert_eq!(
                request.read.projection.encoding,
                VectorEncoding::CoordinatesRoundedF32
            );
            assert_eq!(
                request.read.projection.vector,
                [PropertyKeyId(1), PropertyKeyId(2)]
            );
            assert_eq!(request.read.as_of, Some(CommitSeq(7)));
            assert_eq!(request.eligible, Some(vec![VId(1), VId(2)]));
            assert_eq!(request.read.vertex_label, Some(LabelId(1)));
            let Search::Vector { query, mode, .. } = request.search().unwrap() else {
                panic!("vector search");
            };
            assert_eq!(query, [0.1_f32, 0.2_f32]);
            assert_eq!(
                matches!(mode, VectorSearch::Approximate { ef_search: 32 }),
                ann
            );
            let config = request.read.config_for(request.search().unwrap()).unwrap();
            let value = CanonicalScalar::Float(fgdb_types::CanonicalF64::new(0.1));
            let document = request
                .read
                .projection
                .project(
                    VId(1),
                    &config,
                    |_| Some(&value),
                    &mut WorkBudget::new(1000),
                )
                .unwrap();
            assert_eq!(document.vector, Some(vec![0.1_f32, 0.1_f32]));
        }
        for metric in ["l2", "cosine", "dot"] {
            assert_eq!(
                native_hits(&vector_request("coordinates-rounded", metric)),
                native_hits(&vector_request("coordinates", metric)),
            );
        }
    }

    #[test]
    fn rounded_encoding_checks_dimensions_and_cannot_be_a_text_only_option() {
        for parts in [
            vec![
                "--vector",
                "1,2",
                "--vector-property",
                "x",
                "--vector-encoding",
                "coordinates-rounded",
            ],
            vec![
                "--vector",
                "1",
                "--vector-property",
                "x",
                "--vector-property",
                "y",
                "--vector-encoding",
                "coordinates-rounded",
            ],
            vec![
                "--text",
                "graph",
                "--text-property",
                "bio",
                "--vector-encoding",
                "coordinates-rounded",
            ],
        ] {
            assert_eq!(
                prepare(&options(&parts)).err().expect("invalid shape").code,
                2
            );
        }
        let mut flags = SearchFlags::default();
        flags
            .set("--vector-encoding", "coordinates-rounded")
            .unwrap();
        assert!(flags.set("--vector-encoding", "packed").is_err());
    }
}

#[cfg(test)]
#[path = "search/graph_only_tests.rs"]
mod graph_only_tests;
