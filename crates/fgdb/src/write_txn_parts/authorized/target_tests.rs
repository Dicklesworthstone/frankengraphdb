//! Hidden and absent mutation targets have the same caller-controlled limits.
//! Every attempt uses a real token and native workspace, including a valid
//! prefix which must be discarded on refusal. Timing is outside this law.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{RelationId, SchemaEpoch};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};

const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xb6; 32]);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xb4; 32], NAMESPACE, [0xb5; 32])
}

fn authority() -> Authority {
    Authority::new(
        AuthKey::from_seed(0xb601),
        NAMESPACE,
        "graph",
        SchemaEpoch(1),
        1,
    )
    .unwrap()
}

fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        // Target admission, not authority to erase hidden properties, is the
        // discriminator here. A property-only delete gate must not mask it.
        properties: Scope::All,
        rights: Rights::Write,
        limits: QueryLimits {
            max_nodes: 4096,
            max_work: 4096,
            max_rows: 0,
        },
        expires_at_ms: 10_000,
    }
}

fn under_lab<Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static)
where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root)).await;
    });
    assert!(
        report.lab_test_passed(),
        "lab invariants/quiescence: {report:?}"
    );
}

#[derive(Clone, Copy, Debug)]
enum HiddenTarget {
    Vertex,
    Relation,
    Source,
    Destination,
    SelfLoop,
}

async fn fixture(cx: &CommitCx, hidden: Option<HiddenTarget>) -> Database<MemVfs> {
    let mut db = Database::<MemVfs>::open_memory(cx, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![L], vec![]);
    seed.create_vertex(VId(2), vec![L], vec![]);
    if hidden.is_some() {
        seed.create_vertex(VId(9), vec![HIDDEN], vec![(P, CanonicalScalar::Int(9))]);
    }
    let endpoints = match hidden {
        Some(HiddenTarget::Source) => Some((VId(9), VId(1))),
        Some(HiddenTarget::Destination) => Some((VId(1), VId(9))),
        Some(HiddenTarget::SelfLoop) => Some((VId(9), VId(9))),
        _ => None,
    };
    if let Some((src, dst)) = endpoints {
        seed.add_edge(EId(9), src, dst, vec![(P, CanonicalScalar::Int(9))]);
    }
    db.write(cx, seed).await.unwrap();
    if matches!(hidden, Some(HiddenTarget::Relation)) {
        let mut secret = WriteBatch::new(S);
        secret.add_edge(EId(9), VId(1), VId(2), vec![(P, CanonicalScalar::Int(9))]);
        db.write(cx, secret).await.unwrap();
    }
    db
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    VertexSet,
    VertexCas,
    VertexLabel,
    EdgeSet,
    EdgeCas,
    EdgeDelete,
    EdgeDeleteIfPresent,
    CreateTo,
    EnsureTo,
    CreateFrom,
}

fn request(mutation: Mutation) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(50), vec![L], vec![]);
    match mutation {
        Mutation::VertexSet => {
            batch.set_vertex_property(VId(9), P, Some(CanonicalScalar::Int(7)));
        }
        Mutation::VertexCas => {
            batch.compare_and_set_vertex_property(
                VId(9),
                P,
                Some(CanonicalScalar::Int(9)),
                CanonicalScalar::Int(7),
                WriteMismatchPolicy::AbortWrite,
            );
        }
        Mutation::VertexLabel => {
            batch.set_vertex_label(VId(9), L, true);
        }
        Mutation::EdgeSet => {
            batch.set_edge_property(EId(9), P, Some(CanonicalScalar::Int(7)));
        }
        Mutation::EdgeCas => {
            batch.compare_and_set_edge_property(
                EId(9),
                P,
                Some(CanonicalScalar::Int(9)),
                CanonicalScalar::Int(7),
                WriteMismatchPolicy::AbortWrite,
            );
        }
        Mutation::EdgeDelete => {
            batch.delete_edge(EId(9));
        }
        Mutation::EdgeDeleteIfPresent => {
            batch.delete_edge_if_present(EId(9));
        }
        Mutation::CreateTo => {
            batch.add_edge(EId(50), VId(1), VId(9), vec![]);
        }
        Mutation::EnsureTo => {
            batch.ensure_edge_by_triple(EId(50), VId(1), VId(9), vec![]);
        }
        Mutation::CreateFrom => {
            batch.add_edge(EId(50), VId(9), VId(1), vec![]);
        }
    }
    batch
}

#[derive(Clone, Copy, Debug)]
enum Meter {
    Work,
    Nodes,
}

/// True iff the request reached the common ScopeDenied refusal, rather than
/// exhausting the selected allowance. Any other error or success is a bug.
async fn reaches_denial(
    db: &mut Database<MemVfs>,
    contexts: &PurposeContexts,
    authority: &Authority,
    token: &CapabilityToken,
    mutation: Mutation,
    meter: Meter,
    limit: u64,
) -> bool {
    let cx = contexts.commit();
    let txn = contexts.txn();
    let frontier = db.frontier().unwrap();
    let baseline = txn.outstanding_obligations();
    let restriction = match meter {
        Meter::Work => Restriction::MaxWork(limit),
        Meter::Nodes => Restriction::MaxNodes(limit),
    };
    // Attenuation is available to the holder without the issuer key.
    let limited = token.attenuate(restriction).unwrap();
    let result = db
        .write_authorized(
            &txn,
            &cx,
            authority,
            &limited,
            "main",
            request(mutation),
            || NOW,
        )
        .await;
    assert_eq!(
        db.frontier().unwrap(),
        frontier,
        "{mutation:?} {meter:?}={limit}"
    );
    assert_eq!(txn.outstanding_obligations(), baseline);
    assert!(db.vertex(VId(50)).unwrap().is_none());
    assert!(db.edge_at(EId(50), frontier).unwrap().is_none());
    match (result, meter) {
        (Err(WriteTxnError::Authorization(Error::ScopeDenied)), _) => true,
        (
            Err(WriteTxnError::Authorization(Error::LimitExceeded(LimitDimension::Work))),
            Meter::Work,
        )
        | (
            Err(WriteTxnError::Authorization(Error::LimitExceeded(LimitDimension::Nodes))),
            Meter::Nodes,
        ) => false,
        (other, _) => panic!("{mutation:?} {meter:?}={limit}: {other:?}"),
    }
}

async fn threshold(
    db: &mut Database<MemVfs>,
    contexts: &PurposeContexts,
    authority: &Authority,
    token: &CapabilityToken,
    mutation: Mutation,
    meter: Meter,
) -> u64 {
    let (mut low, mut high) = (0, 4096);
    assert!(reaches_denial(db, contexts, authority, token, mutation, meter, high).await);
    assert!(!reaches_denial(db, contexts, authority, token, mutation, meter, low).await);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if reaches_denial(db, contexts, authority, token, mutation, meter, middle).await {
            high = middle;
        } else {
            low = middle;
        }
    }
    // Explicitly witness both sides of the discovered boundary.
    assert!(reaches_denial(db, contexts, authority, token, mutation, meter, high).await);
    assert!(!reaches_denial(db, contexts, authority, token, mutation, meter, high - 1).await);
    high
}

#[test]
fn hidden_vertices_and_endpoints_share_absent_target_refusal_thresholds() {
    under_lab(0xb601, |contexts| async move {
        let cx = contexts.commit();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut absent = fixture(&cx, None).await;
        let mut hidden = fixture(&cx, Some(HiddenTarget::Vertex)).await;
        for mutation in [
            Mutation::VertexSet,
            Mutation::VertexCas,
            Mutation::VertexLabel,
            Mutation::CreateTo,
            Mutation::EnsureTo,
            Mutation::CreateFrom,
        ] {
            for meter in [Meter::Work, Meter::Nodes] {
                assert_eq!(
                    threshold(&mut absent, &contexts, &authority, &token, mutation, meter).await,
                    threshold(&mut hidden, &contexts, &authority, &token, mutation, meter).await,
                    "hidden vertex: {mutation:?}, {meter:?}"
                );
            }
        }
        assert_eq!(
            hidden.vertex(VId(9)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(9))]
        );
    });
}

#[test]
fn hidden_edge_relations_and_endpoints_share_absent_target_refusal_thresholds() {
    under_lab(0xb602, |contexts| async move {
        let cx = contexts.commit();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut absent = fixture(&cx, None).await;
        for target in [
            HiddenTarget::Relation,
            HiddenTarget::Source,
            HiddenTarget::Destination,
            HiddenTarget::SelfLoop,
        ] {
            let mut hidden = fixture(&cx, Some(target)).await;
            for mutation in [
                Mutation::EdgeSet,
                Mutation::EdgeCas,
                Mutation::EdgeDelete,
                Mutation::EdgeDeleteIfPresent,
            ] {
                for meter in [Meter::Work, Meter::Nodes] {
                    assert_eq!(
                        threshold(&mut absent, &contexts, &authority, &token, mutation, meter)
                            .await,
                        threshold(&mut hidden, &contexts, &authority, &token, mutation, meter)
                            .await,
                        "{target:?}: {mutation:?}, {meter:?}"
                    );
                }
            }
            assert_eq!(
                hidden
                    .edge_at(EId(9), hidden.frontier().unwrap())
                    .unwrap()
                    .unwrap()
                    .props,
                vec![(P, CanonicalScalar::Int(9))]
            );
        }
    });
}

#[test]
fn admitted_updates_keep_original_hidden_fields_and_refuse_after_image_scope_escape() {
    under_lab(0xb603, |contexts| async move {
        let cx = contexts.commit();
        let txn = contexts.txn();
        let baseline = txn.outstanding_obligations();
        let mut db = Database::<MemVfs>::open_memory(&cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L, HIDDEN],
            vec![
                (P, CanonicalScalar::Int(1)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
        db.write(&cx, seed).await.unwrap();
        let authority = authority();
        let token = authority
            .issue_at(&grant(), NOW)
            .unwrap()
            .attenuate(Restriction::Properties(Scope::only([P])))
            .unwrap();
        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
        let frontier = db
            .write_authorized(&txn, &cx, &authority, &token, "main", update, || NOW)
            .await
            .unwrap();
        let before = db.vertex(VId(1)).unwrap().unwrap();
        assert_eq!(before.labels, vec![L, HIDDEN]);
        assert_eq!(
            before.props,
            vec![
                (P, CanonicalScalar::Int(7)),
                (SECRET, CanonicalScalar::Int(99))
            ]
        );
        let mut escape = WriteBatch::new(R);
        escape.create_vertex(VId(50), vec![L], vec![]);
        escape.set_vertex_label(VId(1), L, false);
        assert!(matches!(
            db.write_authorized(&txn, &cx, &authority, &token, "main", escape, || NOW)
                .await,
            Err(WriteTxnError::Authorization(Error::ScopeDenied))
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(50)).unwrap().is_none());
        let after = db.vertex(VId(1)).unwrap().unwrap();
        assert_eq!(after.labels, before.labels);
        assert_eq!(after.props, before.props);
        assert_eq!(txn.outstanding_obligations(), baseline);
    });
}
