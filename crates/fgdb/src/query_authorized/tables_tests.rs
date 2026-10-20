//! fgdb-61p1n: admitted tables are sorted vectors. Sealing must give exactly
//! the ordered map the tables replaced: ascending identities, and the LAST
//! admission of a repeated identity, for any admission order.

use super::seal_by_key;
use std::collections::BTreeMap;

/// xorshift64*: deterministic, dependency-free.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

#[test]
fn sealing_equals_the_ordered_map_it_replaced() {
    let mut state = 0x61b1_7ab1_e5ee_d001;
    let (mut sorted_inputs, mut repeated) = (0, 0);
    for trial in 0..20_000 {
        let len = (next(&mut state) % 40) as usize;
        let span = 1 + next(&mut state) % 60;
        let ascending = next(&mut state).is_multiple_of(3);
        let mut items: Vec<(u64, usize)> = (0..len)
            .map(|at| {
                let key = if ascending {
                    at as u64 * 2 + next(&mut state) % 2
                } else {
                    next(&mut state) % span
                };
                (key, at)
            })
            .collect();
        let mut map = BTreeMap::new();
        for &(key, at) in &items {
            map.insert(key, at);
        }
        sorted_inputs += usize::from(items.windows(2).all(|pair| pair[0].0 < pair[1].0));
        repeated += usize::from(map.len() < items.len());
        seal_by_key(&mut items, |&(key, _)| key);
        assert_eq!(items, map.into_iter().collect::<Vec<_>>(), "trial {trial}");
    }
    assert!(
        sorted_inputs > 5_000 && repeated > 5_000,
        "{sorted_inputs} {repeated}"
    );
}

/// A scan's remembered row answers every lookup sequence as the table does:
/// the vertex just yielded, a missing identity, and identities the scan has
/// passed or not reached, in any interleaving.
#[test]
fn a_remembered_row_answers_every_lookup_as_the_table_does() {
    use super::Tables;
    use crate::VertexRow;
    use fgdb_types::{CommitSeq, VId};
    use std::cell::Cell;
    let rows: Vec<VertexRow> = (0..50_u128)
        .map(|at| VertexRow {
            vid: VId(at * 3 + 1),
            birth_ordinal: at as u64,
            created_at: CommitSeq(1),
            retired_at: None,
            labels: Vec::new(),
            props: Vec::new(),
        })
        .collect();
    let mut tables = Tables::new();
    tables.vertices = rows.iter().map(|row| (row.vid, row)).collect();
    let mut state = 0x7ab1_e5ca_9a11_0001;
    for _ in 0..200 {
        let last = Cell::new(None);
        let mut scan = tables.scan(&last);
        for _ in 0..40 {
            let vid = match next(&mut state) % 3 {
                0 => match scan.next() {
                    Some(vid) => vid,
                    None => break,
                },
                1 => last.get().map_or(VId(0), |(vid, _)| vid),
                _ => VId(u128::from(next(&mut state) % 160)),
            };
            let expected = tables.vertex(vid).map(|row| row as *const VertexRow);
            let found = tables
                .vertex_after(&last, vid)
                .map(|row| row as *const VertexRow);
            assert_eq!(found, expected, "{vid:?}");
        }
    }
}

/// Predicates of a verified grant that sees `relations`, and everything else.
fn predicates(
    relations: fgdb_warden::Scope<fgdb_delta_types::RelationId>,
) -> super::PlannerPredicates {
    use asupersync::security::key::AuthKey;
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_types::DatabaseSecurityNamespaceId;
    use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};
    let namespace = DatabaseSecurityNamespaceId([0x5a; 32]);
    let authority = Authority::new(
        AuthKey::from_seed(5151),
        namespace,
        "graph",
        SchemaEpoch(1),
        1,
    )
    .unwrap();
    let grant = Grant {
        branch: "main".into(),
        labels: Scope::All,
        relations,
        properties: Scope::All,
        rights: Rights::Read,
        limits: QueryLimits {
            max_nodes: 1_000_000,
            max_work: 1_000_000_000,
            max_rows: 1000,
        },
        expires_at_ms: 10_000,
    };
    let token = authority.issue_at(&grant, 100).unwrap();
    authority
        .verify_at(&token, "main", 100)
        .unwrap()
        .predicates()
        .clone()
}

/// An edge of a complete bound closure, whose endpoints are admitted,
/// admits without looking them up (fgdb-61p1n): the same events in the same
/// order, the same records and the same edge table as `admit_edge`, for
/// tables small and large enough to be positioned, every relation visible
/// or one hidden.
#[test]
fn closure_edges_admit_exactly_as_checked_edges() {
    use super::{Fault, POSITIONED_VERTICES, SourceEvent, Tables};
    use crate::VertexRow;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CommitSeq, EId, VId};
    use fgdb_warden::Scope;
    let mut state = 0xc105_ed9e_0000_0001;
    let mut admitted = 0;
    for vertices in [40_u128, POSITIONED_VERTICES as u128 + 500] {
        let rows: Vec<VertexRow> = (0..vertices)
            .map(|at| VertexRow {
                vid: VId(at * 7 + 3),
                birth_ordinal: at as u64,
                created_at: CommitSeq(1),
                retired_at: None,
                labels: Vec::new(),
                props: Vec::new(),
            })
            .collect();
        let edges: Vec<super::Edge<'_>> = (0..3 * vertices)
            .map(|eid| {
                let mut endpoint = || rows[(next(&mut state) % vertices as u64) as usize].vid;
                let (src, dst) = (endpoint(), endpoint());
                let relation = if next(&mut state).is_multiple_of(2) {
                    RelationId(1)
                } else {
                    RelationId(2)
                };
                ((EId(eid), src, relation, dst), &[][..])
            })
            .collect();
        for scope in [Scope::All, Scope::only([RelationId(1)])] {
            let predicates = predicates(scope);
            let run = |closure: bool| {
                let mut log = Vec::new();
                let mut tables = Tables::new();
                let mut control = |event: SourceEvent| -> Result<(), Fault> {
                    log.push(event);
                    Ok(())
                };
                for row in &rows {
                    let admitted = tables
                        .admit_vertex(row, &predicates, &mut || Ok::<_, Fault>(()), &mut control)
                        .unwrap();
                    assert!(admitted.is_some());
                }
                for &edge in &edges {
                    if closure {
                        tables
                            .admit_closure_edge(edge, &predicates, true, &mut control)
                            .unwrap();
                    } else {
                        tables.admit_edge(edge, &predicates, &mut control).unwrap();
                    }
                }
                let identities: Vec<_> =
                    tables.edges.iter().map(|&(identity, _)| identity).collect();
                (log, tables.records, identities)
            };
            let checked = run(false);
            assert_eq!(run(true), checked, "{vertices} vertices");
            admitted += checked.2.len();
        }
    }
    assert!(admitted > 10_000, "{admitted}");
}
