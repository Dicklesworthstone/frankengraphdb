//! Topology-only CLI input must reach Beacon's graph owner, never a synthetic
//! text query. Storage and command-process coverage lives in the CLI suite.

use super::*;
use fgdb_beacon::WorkBudget;
use fgdb_beacon::expansion::GraphHit;

fn options(parts: &[&str]) -> Options {
    let mut args = vec!["--db", "unused", "--key-file", "unused"];
    args.extend_from_slice(parts);
    crate::parse(
        &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        "search",
    )
    .expect("search arguments")
}

fn graph_query(request: &Prepared) -> GraphHybridQuery<'_> {
    let graph = request.graph.as_ref().expect("graph lane");
    GraphHybridQuery {
        retrieval: request.retrieval().unwrap(),
        graph_candidates: graph.candidates,
        graph_weight: graph.weight,
    }
}

#[test]
fn graph_only_defaults_need_no_properties_or_vector_index() {
    let request = prepare(&options(&["--expand-from", "1", "--max-hops", "2"]))
        .expect("a seed and hop bound are a complete search");
    assert!(request.read.projection.text.is_none());
    assert!(request.read.projection.vector.is_empty());
    assert!(request.read.index.vector.is_none());
    assert!(
        request.search().is_err(),
        "no synthetic index-backed search"
    );
    let query = graph_query(&request);
    assert!(query.is_graph_only());
    assert!(query.graph_enabled());
    assert_eq!(query.candidate_depths().unwrap(), (0, 0, 10));
    assert_eq!(query.retrieval.k, 10);
    assert!(query.retrieval.text.is_empty());
    assert!(query.retrieval.vector.is_empty());
    assert_eq!(query.retrieval.profile.text_weight(), 0);
    assert_eq!(query.retrieval.profile.vector_weight(), 0);
    request.read.validate_graph_only(query).unwrap();
    let graph = request.graph.as_ref().unwrap();
    assert_eq!(graph.seeds, [VId(1)]);
    assert_eq!(graph.max_hops, 2);
    assert_eq!(graph.direction, ExpansionDirection::Outgoing);
    assert!(!graph.include_seeds);
}

#[test]
fn graph_only_binds_direction_relation_selection_and_historical_cut() {
    let largest = u128::MAX.to_string();
    for (direction, expected) in [
        ("out", ExpansionDirection::Outgoing),
        ("in", ExpansionDirection::Incoming),
        ("both", ExpansionDirection::Undirected),
    ] {
        let request = prepare(&options(&[
            "--expand-from",
            &largest,
            "--max-hops",
            "0",
            "--include-seeds",
            "true",
            "--relation",
            "CITES=7",
            "--expand-relation",
            "CITES",
            "--expand-direction",
            direction,
            "--label",
            "Document=3",
            "--vertex-label",
            "Document",
            "--eligible",
            &largest,
            "--as-of",
            "19",
            "--k",
            "2",
            "--candidates",
            "8",
            "--graph-candidates",
            "4",
            "--graph-weight",
            "5",
        ]))
        .unwrap();
        let graph = request.graph.as_ref().unwrap();
        assert_eq!(graph.seeds, [VId(u128::MAX)]);
        assert_eq!(graph.max_hops, 0);
        assert!(graph.include_seeds);
        assert_eq!(graph.direction, expected);
        assert_eq!(graph.relation, Some(RelationId(7)));
        assert_eq!(request.read.vertex_label, Some(LabelId(3)));
        assert_eq!(request.read.as_of, Some(CommitSeq(19)));
        assert_eq!(request.eligible, Some(vec![VId(u128::MAX)]));
        let query = graph_query(&request);
        assert_eq!(query.candidate_depths().unwrap(), (0, 0, 4));
        assert_eq!(query.graph_weight, 5);
        assert_eq!(query.retrieval.k, 2);
    }
}

#[test]
fn graph_only_candidate_defaults_and_overrides_are_not_a_base_lane() {
    for (extra, depth) in [
        (vec![], 2),
        (vec!["--candidates", "7"], 7),
        (vec!["--graph-candidates", "5"], 5),
        (vec!["--candidates", "7", "--graph-candidates", "5"], 5),
    ] {
        let mut parts = vec!["--expand-from", "1", "--max-hops", "1", "--k", "2"];
        parts.extend(extra);
        let request = prepare(&options(&parts)).unwrap();
        let query = graph_query(&request);
        assert_eq!(query.candidate_depths().unwrap(), (0, 0, depth));
        assert_eq!(query.retrieval.k, 2);
    }
    let request = prepare(&options(&[
        "--expand-from",
        "1",
        "--max-hops",
        "0",
        "--eligible",
        "",
    ]))
    .unwrap();
    assert_eq!(request.eligible, Some(vec![]));
}

#[test]
fn graph_only_rejects_incomplete_or_incompatible_inputs_before_open() {
    for parts in [
        vec![],
        vec!["--expand-from", "1"],
        vec!["--max-hops", "1"],
        vec!["--graph-candidates", "1"],
        vec!["--expand-direction", "in"],
    ] {
        let error = prepare(&options(&parts)).err().expect("invalid request");
        assert_eq!(error.code, 2, "{parts:?}: {}", error.message);
    }
    for extra in [
        ["--text", "orphan"],
        ["--text-property", "orphan"],
        ["--text-match", "all"],
        ["--max-expansions", "2"],
        ["--vector", "1"],
        ["--vector-property", "orphan"],
        ["--metric", "cosine"],
        ["--ann", "32"],
        ["--vector-encoding", "packed"],
        ["--expand-relation", "unbound"],
        ["--vertex-label", "unbound"],
    ] {
        let mut parts = vec!["--expand-from", "1", "--max-hops", "1"];
        parts.extend(extra);
        let error = prepare(&options(&parts)).err().expect("invalid request");
        assert_eq!(error.code, 2, "{parts:?}: {}", error.message);
    }
}

#[test]
fn graph_only_validates_native_output_limits_before_open() {
    for depth_flag in ["--candidates", "--graph-candidates"] {
        let error = prepare(&options(&[
            "--expand-from",
            "1",
            "--max-hops",
            "0",
            "--k",
            "2",
            depth_flag,
            "1",
        ]))
        .err()
        .expect("k exceeds the only active candidate depth");
        assert_eq!(error.code, 2);
    }
    let too_many = (ReadPolicy::default().max_result_rows + 1).to_string();
    let error = prepare(&options(&[
        "--expand-from",
        "1",
        "--max-hops",
        "0",
        "--k",
        &too_many,
    ]))
    .err()
    .expect("result policy is not waived for topology-only reads");
    assert_eq!(error.code, 2);
}

#[test]
fn graph_only_uses_native_exact_fusion_eligibility_and_work_refusal() {
    let request = prepare(&options(&[
        "--expand-from",
        "1",
        "--max-hops",
        "3",
        "--k",
        "1",
        "--graph-candidates",
        "3",
        "--graph-weight",
        "5",
        "--eligible",
        "4",
    ]))
    .unwrap();
    let population = [
        GraphHit {
            id: VId(2),
            hops: 1,
        },
        GraphHit {
            id: VId(3),
            hops: 1,
        },
        GraphHit {
            id: VId(4),
            hops: 2,
        },
    ];
    let eligible = |id| {
        request
            .eligible
            .as_ref()
            .unwrap()
            .binary_search(&id)
            .is_ok()
    };
    let query = graph_query(&request);
    let hits = query
        .fuse_graph_only(&population, eligible, &mut WorkBudget::new(10_000))
        .unwrap();
    let oracle = GraphHybridQuery::graph_only(1, 3, 5, 60)
        .unwrap()
        .fuse_graph_only(&population, |id| id == VId(4), &mut WorkBudget::new(10_000))
        .unwrap();
    assert_eq!(hits, oracle);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, VId(4));
    assert_eq!(hits[0].graph_rank.unwrap().get(), 1);
    assert_eq!(hits[0].graph_hops, Some(2));
    assert!(hits[0].text_rank.is_none());
    assert!(hits[0].vector_rank.is_none());
    assert!(hits[0].text_score.is_none());
    assert!(hits[0].vector_distance.is_none());
    assert!(
        query
            .fuse_graph_only(&population, |_| true, &mut WorkBudget::new(0))
            .is_err()
    );
}
