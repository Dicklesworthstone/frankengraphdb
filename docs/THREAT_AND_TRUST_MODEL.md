<!-- GENERATED FILE — DO NOT EDIT BY HAND.
     Source: registries/threat_model.toml
     Regenerate: cargo run -p registry-check --bin threat-check -- --root . --write
     Verify:     cargo run -p registry-check --bin threat-check -- --root .
-->

# FrankenGraphDB — Threat and Trust Model

This document is the frame in which every later security claim is scoped. It is generated from `registries/threat_model.toml`; the registry is the master, this rendering is a projection, and the checker fails if they disagree.

**What a reader should take from it.** The baseline trusts the executing database process and the active key boundary. A compromised server process can exfiltrate what it can decrypt, and no claim here contradicts that. Witnessed transparency and audit detect their scoped history and administrative attacks; they never undo disclosure. Ordinary Raft tolerates crash faults, not Byzantine replicas. Everything else below is the detail of those four sentences.

## 1. Actors

The eight actors of §12.1, in source order. Each is considered adversarially: the disposition tables in §3 state what the model defends against this actor, not what the actor is expected to do.

| # | Actor | Trust class | In boundary | Summary |
| --- | --- | --- | --- | --- |
| 1 | `untrusted_client` | untrusted | no | Any principal presenting a token over a protocol surface. Authenticated is not trusted: every observation and effect is gated by EffectiveAuthority before retrieval, statistics, traversal, or expansion. |
| 2 | `mutually_hostile_tenant` | hostile | no | A co-resident tenant assumed adversarial to every other tenant. Isolation is EffectiveAuthority plus descriptor masking, not deployment separation: a capability that cannot see an edge type cannot observe its existence via degree either. |
| 3 | `honest_but_curious_storage` | honest_but_curious | no | Storage media, volumes, and the operators of them. Follows the protocol and returns the bytes it was given, but reads everything it holds and observes object sizes and access order. |
| 4 | `crash_fault_replica` | crash_fault | yes | A consensus member that may stop, lag, lose its tail, or restart, but never lies. Ordinary Raft tolerates exactly this failure class; a Byzantine replica is a registered out-of-scope failure, not a covered one. |
| 5 | `malicious_or_stale_donor` | potentially_malicious | no | A replication, seeding, or backup donor supplying object bytes. Donor bytes are verified against content-addressed identity and the receiver's own closure requirements; a decode proof alone establishes neither completeness nor semantic validity. |
| 6 | `independent_transparency_witness` | independent_verifier | no | An external co-signer of history commitments. Considered adversarially it may withhold or equivocate about inclusion, which is why non-equivocation is a conditional claim under declared witnessed-transparency premises; it never observes plaintext, because it is shown commitments. |
| 7 | `trusted_embedded_host_code` | trusted | yes | Native code in the embedding process: the host application and its native Rust procedures. It is inside the trust boundary by construction — it shares the address space with the key boundary — so the model defends no asset against it. Native procedures are therefore barred from replicated logical effects, merge/apply, constraints, and replay-guaranteed queries. |
| 8 | `compromised_operator_or_server` | trusted | yes | The executing database process, or an operator with control of it, after compromise. The baseline TRUSTS this boundary: a compromised server process can exfiltrate everything it can decrypt, and this model claims no TEE or Byzantine-confidentiality magic against it. What survives is scoped detection — witnessed transparency and external anti-rollback heads detect their scoped history and administrative attacks; they do not undo disclosure. |

## 2. Protected assets

| # | Asset | Primary claim | Class | Summary |
| --- | --- | --- | --- | --- |
| 1 | `plaintext_graph_data` | FG-INV-20 | invariant | Vertex, edge, and property plaintext, and any projection of it, within and outside an authorized scope. |
| 2 | `authority_and_policy_state` | FG-INV-20 | invariant | SecurityPolicyRoot, RevocationRoot, SecurityStateBinding, issued tokens, leases, and the scope assignments that decide every authorization. |
| 3 | `key_material` | FG-INV-20 | invariant | The key-envelope DAG, issuer MAC key epochs, signer sets, and any wrap that a decrypting path must open. |
| 4 | `history_order_and_nonequivocation` | FG-INV-08 | invariant | The single global commit order, its non-forking lineage across incarnations and branches, and the absence of two authenticated histories for one namespace. |
| 5 | `audit_completeness` | FG-INV-08 | invariant | The gap-free audit ticket, admission-visibility, and resolution pipeline, and the terminal gates that make a Required operation's audit position a precondition of its visibility. |
| 6 | `identity_and_allocation_continuity` | FG-INV-20 | invariant | Never-recycled VId and AllocationEpoch values and the cluster incarnation that scopes them — the guarantee that a rolled-back closure cannot reissue a spent identity. |
| 7 | `derived_state_and_observable_metadata` | FG-EVID-05 | statistical | Statistics, degrees, cardinalities, cache and plan behaviour, timing, and error classes — everything observable that is not the plaintext itself. Mitigations are padding, quantization, pools, and bounded errors; the residual channel is measured, never proved absent. |
| 8 | `result_and_replay_evidence` | FG-INV-20 | invariant | Canonical results, plan and replay certificates, evidence-slot closures, and durable artifact outputs. |

## 3. The exposure matrix

Every actor-asset cell is dispositioned exactly once and names the assumption that carries it. `defended` means the model defends the asset against that actor; `conditional` means it does so only under the named assumption's stated bounds; `undefended` means it does not, and says so rather than leaving a gap a reader would fill with optimism.

| Actor \\ Asset | `plaintext_graph_data` | `authority_and_policy_state` | `key_material` | `history_order_and_nonequivocation` | `audit_completeness` | `identity_and_allocation_continuity` | `derived_state_and_observable_metadata` | `result_and_replay_evidence` |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `untrusted_client` | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | defended (A-PROCESS-TRUSTED) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | conditional (A-LEAKAGE-MEASURED-NOT-PROVED) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) |
| `mutually_hostile_tenant` | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | defended (A-PROCESS-TRUSTED) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) | conditional (A-LEAKAGE-MEASURED-NOT-PROVED) | defended (A-TENANT-ISOLATION-BY-AUTHORITY) |
| `honest_but_curious_storage` | defended (A-STORAGE-CIPHERTEXT-ONLY) | defended (A-STORAGE-CIPHERTEXT-ONLY) | defended (A-PROCESS-TRUSTED) | conditional (A-WITNESS-DETECT-ONLY) | conditional (A-WITNESS-DETECT-ONLY) | conditional (A-EXTERNAL-AUTHORITY-IN-BOUNDARY) | conditional (A-LEAKAGE-MEASURED-NOT-PROVED) | defended (A-STORAGE-CIPHERTEXT-ONLY) |
| `crash_fault_replica` | defended (A-RAFT-CRASH-ONLY) | defended (A-RAFT-CRASH-ONLY) | defended (A-PROCESS-TRUSTED) | defended (A-RAFT-CRASH-ONLY) | defended (A-RAFT-CRASH-ONLY) | defended (A-EXTERNAL-AUTHORITY-IN-BOUNDARY) | conditional (A-LEAKAGE-MEASURED-NOT-PROVED) | defended (A-RAFT-CRASH-ONLY) |
| `malicious_or_stale_donor` | defended (A-DONOR-UNTRUSTED-BYTES) | defended (A-DONOR-UNTRUSTED-BYTES) | defended (A-PROCESS-TRUSTED) | defended (A-DONOR-UNTRUSTED-BYTES) | defended (A-DONOR-UNTRUSTED-BYTES) | defended (A-EXTERNAL-AUTHORITY-IN-BOUNDARY) | conditional (A-LEAKAGE-MEASURED-NOT-PROVED) | defended (A-DONOR-UNTRUSTED-BYTES) |
| `independent_transparency_witness` | defended (A-STORAGE-CIPHERTEXT-ONLY) | defended (A-STORAGE-CIPHERTEXT-ONLY) | defended (A-PROCESS-TRUSTED) | conditional (A-WITNESS-DETECT-ONLY) | conditional (A-WITNESS-DETECT-ONLY) | defended (A-EXTERNAL-AUTHORITY-IN-BOUNDARY) | undefended (A-LEAKAGE-MEASURED-NOT-PROVED) | defended (A-STORAGE-CIPHERTEXT-ONLY) |
| `trusted_embedded_host_code` | undefended (A-HOST-CODE-TRUSTED) | undefended (A-HOST-CODE-TRUSTED) | undefended (A-HOST-CODE-TRUSTED) | undefended (A-HOST-CODE-TRUSTED) | undefended (A-HOST-CODE-TRUSTED) | undefended (A-HOST-CODE-TRUSTED) | undefended (A-HOST-CODE-TRUSTED) | undefended (A-HOST-CODE-TRUSTED) |
| `compromised_operator_or_server` | undefended (A-NO-TEE) | undefended (A-NO-TEE) | undefended (A-NO-TEE) | conditional (A-WITNESS-DETECT-ONLY) | conditional (A-WITNESS-DETECT-ONLY) | conditional (A-EXTERNAL-AUTHORITY-IN-BOUNDARY) | undefended (A-NO-TEE) | undefended (A-NO-TEE) |

## 4. Trust assumptions

- **A-PROCESS-TRUSTED** (§12.1) — The baseline trusts the executing database process and the active key boundary to enforce plaintext confidentiality and policy.
  - *Bounds*: Holds only while that process is uncompromised; A-NO-TEE states the consequence when it is not.
- **A-NO-TEE** (§12.1) — A compromised server process can exfiltrate what it can decrypt, and this plan claims no TEE or Byzantine-confidentiality magic.
  - *Bounds*: This is a limitation, not a defense. No normative text may claim confidentiality against a compromised server process.
- **A-RAFT-CRASH-ONLY** (§12.1) — Ordinary Raft tolerates crash faults, not Byzantine replicas.
  - *Bounds*: A replica that lies is out of scope (OOS-BYZANTINE-CONSENSUS), not a covered failure.
- **A-WITNESS-DETECT-ONLY** (§12.1) — Witnessed transparency and audit may detect only their scoped history and administrative attacks, not undo disclosure.
  - *Bounds*: Non-equivocation claims are conditional on the declared witnessed-transparency premises of FG-INV-08; detection is not prevention and never restores confidentiality.
- **A-HOST-CODE-TRUSTED** (§12.1) — Embedded host code sharing the process address space is inside the trust boundary.
  - *Bounds*: This is why native procedures are categorically barred from replicated logical effects, merge/apply, constraints, and replay-guaranteed queries: a conformance test cannot prove arbitrary native code deterministic.
- **A-EXTERNAL-AUTHORITY-IN-BOUNDARY** (§16.8) — The external authorities are inside the declared trust boundary and carry their own registered HA and durability contract: threshold signer sets with registered rotation, durable CAS heads with an explicit anti-rollback story, and a specified successor-authority migration.
  - *Bounds*: Availability of an authority is an slo row, never an invariant; a permanently dead authority is escaped only by a threshold-authorized succession transition.
- **A-DONOR-UNTRUSTED-BYTES** (§12.1) — Donor-supplied bytes are verified against content-addressed identity and the receiver's own closure requirements before use.
  - *Bounds*: Decode proofs alone establish neither backup completeness nor semantic validity.
- **A-STORAGE-CIPHERTEXT-ONLY** (§12.1) — A party that holds or witnesses stored bytes observes ciphertext and declared metadata, never plaintext.
  - *Bounds*: Object sizes, counts, and access order remain observable; that residual is governed by A-LEAKAGE-MEASURED-NOT-PROVED.
- **A-TENANT-ISOLATION-BY-AUTHORITY** (§12.1) — Isolation between principals and tenants is enforced by EffectiveAuthority and descriptor masking before any observation or effect, not by deployment separation or post-filtering.
  - *Bounds*: Caveats compile to mandatory planner predicates; security applies before expansion, never as a post-filter (FG-INV-20).
- **A-LEAKAGE-MEASURED-NOT-PROVED** (§12.6) — Timing, cardinality, and error leakage is acknowledged; padding, quantization, pools, and bounded errors are mitigations, and any mutual-information or channel-capacity result is a deployment-specific StatisticalClaim.
  - *Bounds*: Adversarial CI fixtures do not prove universal noninterference or a global bits-per-query ceiling. Carried by FG-EVID-05.

## 5. Registered out-of-scope failures

- **OOS-FHE-MPC-ORAM** — FHE, MPC, and ORAM are rejected from the initial threat model. (§3.4 (i))
  - *Reason*: Their cost model is incompatible with the §17 performance contract, and none of them changes the compromised-server exposure that A-NO-TEE already declares.
- **OOS-TEE** — No claim rests on a trusted execution environment. (§12.1)
  - *Reason*: The baseline already trusts the executing process; a TEE would move that boundary without the plan owning its attestation, rollback, and side-channel surface.
- **OOS-BYZANTINE-CONSENSUS** — Byzantine-fault-tolerant consensus is out of scope; ordinary Raft tolerates crash faults only. (§12.1)
  - *Reason*: A lying replica is not a covered failure. Detection of scoped history attacks comes from witnessed transparency, which is a different mechanism with a different claim class.
- **OOS-TRUETIME** — TrueTime-class time infrastructure is rejected for 1.0. (§3.4 (h))
  - *Reason*: HLC plus consensus-fenced read contracts supplies the semantics actually specified, not arbitrary follower linearizability.
- **OOS-NONINTERFERENCE-PROOF** — Differential privacy and leakage measurements are scoped mechanisms, never substitutes for noninterference. (§3.4)
  - *Reason*: A measured channel bound under one fixture, workload, and concurrency assumption is a statistical claim; promoting it to a semantic guarantee is exactly the claim-class violation the constitution's lattice forbids.

## 6. Stable security identities

| # | Identity | Kind | Epoch domain | Rust newtype | Wire tag |
| --- | --- | --- | --- | --- | --- |
| 1 | `TenantId` | stable_identity | none | `TenantId` | `fgdb:security-identity:tenant-id:v1` |
| 2 | `PrincipalId` | stable_identity | none | `PrincipalId` | `fgdb:security-identity:principal-id:v1` |
| 3 | `IssuerId` | stable_identity | none | `IssuerId` | `fgdb:security-identity:issuer-id:v1` |
| 4 | `TokenId` | stable_identity | none | `TokenId` | `fgdb:security-identity:token-id:v1` |
| 5 | `DatabaseId` | stable_identity | none | `DatabaseId` | `fgdb:security-identity:database-id:v1` |
| 6 | `SecurityPolicyEpoch` | security_epoch | security | `SecurityPolicyEpoch` | `fgdb:security-epoch:security-policy-epoch:v1` |
| 7 | `RevocationIndex` | monotone_index | security | `RevocationIndex` | `fgdb:security-epoch:revocation-index:v1` |
| 8 | `DecisionPolicyEpoch` | adaptive_epoch | adaptive | `DecisionPolicyEpoch` | `fgdb:adaptive-epoch:decision-policy-epoch:v1` |
| 9 | `KeyEpoch` | security_epoch | security | `KeyEpoch` | `fgdb:security-epoch:key-epoch:v1` |

The security and adaptive epoch types have distinct wire tags and distinct Rust newtypes and are never comparable or substitutable. `SecurityPolicyEpoch` sits in the `security` epoch domain; `DecisionPolicyEpoch` sits in `adaptive`.

## 7. Operation classes

The closed set of sixteen, in §12.1 source order.

| # | Class | Summary |
| --- | --- | --- |
| 1 | `Read` | Observe committed graph state through an authorized cursor or view. |
| 2 | `Mutate` | Produce logical effects against a branch coordinate. |
| 3 | `Ddl` | Change schema objects: labels, edge types, property definitions, constraints, indexes. |
| 4 | `Subscribe` | Register and consume a standing query or change subscription. |
| 5 | `Analytics` | Run Prism-mediated analytics over an authorized projection. |
| 6 | `Replay` | Re-execute a plan certificate against its evidence closure. |
| 7 | `ExecuteProcedure` | Invoke a registered procedure or UDF under a declared invocation mode. |
| 8 | `InstallModule` | Install or activate a module artifact by ObjectId. |
| 9 | `ExternalIo` | Perform an effect crossing the database boundary under a declared external-I/O class. |
| 10 | `Export` | Emit a durable artifact output projection of canonical result rows. |
| 11 | `Backup` | Pin, certify, seal, and publish a backup closure. |
| 12 | `Restore` | Stage, validate, and promote a restore from a published artifact. |
| 13 | `Observe` | Read the system graph, telemetry, and transparency proofs. |
| 14 | `Admin` | Administer policy, revocation, configuration, and maintenance. |
| 15 | `KeyManage` | Rotate, wrap, grant, and stage the destruction of key material. |
| 16 | `Replicate` | Participate in consensus, seeding, and donor service. |

## 8. The EffectiveAuthority lattice

The lattice has no independent order: its order is exactly the conjunction of these per-dimension narrowing operators under the attenuation law of §9.

| # | Dimension | Narrowing operator | Source | Summary |
| --- | --- | --- | --- | --- |
| 1 | `tenant` | `fixed` | §12.1 | The tenancy the authority is bound to. |
| 2 | `subject` | `fixed` | §12.1 | The principal the authority speaks for. |
| 3 | `issuer` | `fixed` | §12.1 | The root issuer identity. |
| 4 | `audience` | `fixed` | §12.1 | The audience the root was minted for. |
| 5 | `database_security_namespace` | `fixed` | §12.1 | The database security namespace the authority is scoped to. |
| 6 | `cluster_incarnation` | `fixed` | §12.1 | The cluster incarnation the namespace is scoped by. |
| 7 | `token_epoch` | `fixed` | §12.1 | The token's MAC key epoch, retained from the immutable root tuple. |
| 8 | `security_policy_epoch` | `fixed` | §12.1 | The security-policy epoch the authority is verified against. |
| 9 | `revocation_index` | `current_state_monotone` | §12.1 | The current revocation index. Supplied by authenticated current state, not by the chain: a token cannot carry a stale index forward as if it were narrowing. |
| 10 | `operation_classes` | `intersect` | §12.1 | The subset of the sixteen operation classes the authority admits. |
| 11 | `graph_scope` | `intersect` | §12.1 | The graph coordinates the authority reaches. |
| 12 | `branch_scope` | `intersect` | §12.1 | The branch coordinates the authority reaches. |
| 13 | `label_scope` | `intersect` | §12.1 | The vertex labels the authority may observe or affect. |
| 14 | `edge_scope` | `intersect` | §12.1 | The edge types the authority may observe or affect, including their contribution to degree. |
| 15 | `property_scope` | `intersect` | §12.1 | The property slots the authority may observe or affect. |
| 16 | `system_field_scope` | `intersect` | §12.1 | The reserved system fields the authority may observe. |
| 17 | `temporal_scope` | `intersect` | §12.1 | The transaction-time and valid-time intervals the authority may select. |
| 18 | `procedure_udf_module_object_ids` | `intersect` | §12.1 | The finite allow-list of procedure, UDF, and module ObjectIds. |
| 19 | `invocation_mode` | `intersect` | §12.1 | The permitted invocation modes for those modules, including invoker-versus-definer rights. |
| 20 | `external_io_classes` | `intersect` | §12.1 | The external-I/O classes the authority admits. |
| 21 | `replay_evidence_resolver_closure` | `intersect` | §12.1 | The replay evidence slots and resolver closure the authority may draw on. |
| 22 | `redaction_profile` | `restrict_disclosure_only` | §12.1 | The redaction profile applied to observations and errors. A link may redact more; it can never widen disclosure. |
| 23 | `resource_ceiling` | `lower_only` | §12.1 | The resource ceiling the authority may consume. |
| 24 | `effective_not_before` | `raise_only` | §12.2 | The effective not-before instant. A link may only raise it. |
| 25 | `effective_expires_at` | `lower_only` | §12.2 | The effective expiry instant. A link may only lower it; maximum TTL is the explicit revocation-enforcement bound. |
| 26 | `accepted_caveats` | `append_only` | §12.2 | Accepted caveats, which compile to the versioned deterministic total resource-bounded PolicyIr and to mandatory planner predicates. |
| 27 | `presentation_binding` | `restrict_binding_only` | §12.2 | The presentation binding class the holder must satisfy. A link may preserve it or move to a strictly higher binding rank. |

## 9. The attenuation law

| Law | Class | Statement | Governs | Negative fixture |
| --- | --- | --- | --- | --- |
| `ATT-P1` | permitted | A chain link may raise the effective not-before. | effective_not_before | `tm_neg_time_extension` |
| `ATT-P2` | permitted | A chain link may lower the effective expiry. | effective_expires_at | `tm_neg_time_extension` |
| `ATT-P3` | permitted | A chain link may intersect authority scopes. | operation_classes, graph_scope, branch_scope, label_scope, edge_scope, property_scope, system_field_scope, temporal_scope, procedure_udf_module_object_ids, invocation_mode, external_io_classes, replay_evidence_resolver_closure | `tm_neg_scope_widening` |
| `ATT-P4` | permitted | A chain link may intersect resource scopes. | resource_ceiling | `tm_neg_scope_widening` |
| `ATT-P5` | permitted | A chain link may append accepted caveats. | accepted_caveats | `tm_neg_caveat_removal` |
| `ATT-P6` | permitted | A chain link may preserve or further restrict the presentation binding. | presentation_binding | `tm_neg_binding_weakening` |
| `ATT-X1` | prohibited | A chain link cannot extend time. | effective_not_before, effective_expires_at | `tm_neg_time_extension` |
| `ATT-X2` | prohibited | A chain link cannot change the authority domain. | tenant, subject, issuer, audience, database_security_namespace, cluster_incarnation, token_epoch, security_policy_epoch | `tm_neg_domain_change` |
| `ATT-X3` | prohibited | A chain link cannot widen disclosure. | redaction_profile | `tm_neg_disclosure_widening` |
| `ATT-X4` | prohibited | A chain link cannot move cohorts; every link retains the complete root tuple, profile, and cohort. | issuer, token_epoch | `tm_neg_cohort_move` |
| `ATT-X5` | prohibited | A chain link cannot change Session or Channel binding to a less restrictive class. | presentation_binding | `tm_neg_binding_weakening` |

### 9.1 Presentation binding narrowing

| Class | Rank | Summary |
| --- | --- | --- |
| `ServerInternal` | 0 | No presentation binding: the token is exercised inside the server with no session or channel proof. |
| `SessionBound` | 1 | Bound to a session binding and auth generation; presentation requires that live session. |
| `ChannelBound` | 2 | Bound to a transport exporter digest; presentation requires that exact channel. |

The complete transition matrix. A link may preserve the binding or move to a strictly higher rank; every other cell is illegal and is declared here rather than merely absent.

| from \\ to | `ServerInternal` | `SessionBound` | `ChannelBound` |
| --- | --- | --- | --- |
| `ServerInternal` | legal (preserve) | legal (further_restrict) | legal (further_restrict) |
| `SessionBound` | **illegal** (weakened_binding) | legal (preserve) | legal (further_restrict) |
| `ChannelBound` | **illegal** (weakened_binding) | **illegal** (weakened_binding) | legal (preserve) |

## 10. Postures and the product-space closure

A posture is an admissible cell of a declared product space, not a name on a list. Every cell below is registered, deferred to a named owner bead, or excluded by a named law.

| Law | Statement | Source | Reason |
| --- | --- | --- | --- |
| `PX-1` | role_posture = Sharded implies continuity_profile = ExternalCas. | §5.1 | W12 requires ExternalCas; the AllocationContinuityProfile is bound one-to-one to the slot's IncarnationContinuityProfile, so a sharded DirectoryBound slot cannot exist. |
| `PX-2` | service_class = ArchiveReadOnly implies continuity_profile = NotApplicable. | §13.7 | An archive open performs no incarnation takeover, mints no writer, and mints no service-promotion receipt, so it consumes no incarnation or allocation continuity fence at all. |
| `PX-3` | service_class = Operational implies continuity_profile != NotApplicable. | §5.1 | Every operational slot is fenced by exactly one registered IncarnationContinuityProfile arm before it may write. |

| Service class | Role posture | Continuity profile | Resolution | Resolved by |
| --- | --- | --- | --- | --- |
| Operational | Local | DirectoryBound | registered | `local_directory_bound` |
| Operational | Local | ExternalCas | registered | `local_external_cas` |
| Operational | Local | NotApplicable | excluded | `PX-3` |
| Operational | Sharded | DirectoryBound | excluded | `PX-1` |
| Operational | Sharded | ExternalCas | deferred | `sharded_external_cas` |
| Operational | Sharded | NotApplicable | excluded | `PX-1` |
| ArchiveReadOnly | Local | DirectoryBound | excluded | `PX-2` |
| ArchiveReadOnly | Local | ExternalCas | excluded | `PX-2` |
| ArchiveReadOnly | Local | NotApplicable | deferred | `archive_read_only_open` |
| ArchiveReadOnly | Sharded | DirectoryBound | excluded | `PX-1` |
| ArchiveReadOnly | Sharded | ExternalCas | excluded | `PX-2` |
| ArchiveReadOnly | Sharded | NotApplicable | excluded | `PX-1` |

- **Deferred** `sharded_external_cas` — owner `fgdb-w12-meta-authority-transfer`. Sharding is W12, after 1.0. The cell is admissible under the product-space laws and its footprint is a W12 deliverable; freezing Meta-side and shard-side sync-path positions here would manufacture a G0 fact from an unbuilt protocol.
- **Deferred** `archive_read_only_open` — owner `fgdb-g0-format-arms-90z`. §13.7 states in terms that the open-class, posture, and filesystem-profile-variant registry rows are reserved in the §19 G0 decision batch, which g0-format-arms owns. This registry supplies the posture vocabulary that bead consumes.

## 11. The external-authority footprint

Eleven authorities, in §16 item 8 source order.

| # | Authority | Records | Summary |
| --- | --- | --- | --- |
| 1 | `identity_allocation_continuity` | `IdentityContinuityRecord`, `AllocationEpochReservation` | The externally anti-rollback head that fences an allocation epoch before it may issue an identity. |
| 2 | `cluster_incarnation` | `ClusterIncarnationContinuityRecord` | The continuity head that scopes a database incarnation and its member leases. |
| 3 | `time_authority` | `PortableTimeAuthorityObservation` | The profiled authority supplying AuthorityInstant observations with validation evidence. |
| 4 | `audit_continuity` | `AuditContinuityRecord` | The externally monotone chain that gates ticket issuance, admission visibility, and batched resolution. |
| 5 | `dp_registry` | `PrivacyContinuityRecord` | The registry that makes a DP charge irreversible before access. |
| 6 | `archive_grant` | `ArchiveRetentionGrant` | The external grant that authorizes archive retention and its hold completion. |
| 7 | `reservation` | `ReservationAuthorityHead` | The head that reserves externally visible names and slots for a publication. |
| 8 | `catalog` | `CatalogAuthorityHead` | The head that publishes and authenticates externally visible catalog state. |
| 9 | `restore_dispatch_journal` | `ExternalCasContinuity` | The restore dispatch journal's ExternalCasContinuity head. |
| 10 | `transparency_witness` | `WitnessPolicy` | The independent witnesses whose co-signatures scope the non-equivocation claim. |
| 11 | `kms_hsm` |  | The external key custodian consulted when a wrap cannot be satisfied locally. |

### 11.x Local, DirectoryBound (the embedded posture)

**Empty footprint declaration.** The embedded DirectoryBound posture's footprint table is empty: it consumes zero external authorities. The epoch fence is a local allocation-continuity journal record ordered under the same whole-inode fence and fixed local continuity digest the cluster-incarnation record registers for that profile.

All eleven authority cells are explicitly empty for this posture.

### 11.x Local, ExternalCas

| Authority | Synchronous | Touches | Where on the path | Operation-class basis |
| --- | --- | --- | --- | --- |
| `identity_allocation_continuity` | yes | 1 | Only at allocation-epoch mint: the threshold-signed or witnessed IdentityContinuityRecord is durably CAS-published outside the rollbackable database closure before the epoch may issue an ID. | trigger_site_only |
| `cluster_incarnation` | yes | 1 | Only at ExternalCas lease acquisition and renewal. | trigger_site_only |
| `time_authority` | yes | 2 | Per protected-output activation tick, batch-subject amortized under §12.8, and per IdRangeLease use window. Deliberately kept off the audit prefix, which is why delivery availability and not visibility availability multiplies in its per-tick availability. | trigger_site_only |
| `audit_continuity` | yes | 3 | Three times on a Required operation: the strictly serial one-at-a-time TicketIssued issuance CAS, the admission visibility advance, and its share of one batched TicketsResolvedBatch resolution. Only the Protocol-plane AuditVisibilityAdvanceSpec can release a Required entry, so visibility availability multiplies in this authority's availability while apply-durability availability does not. | trigger_site_only |
| `dp_registry` | yes | 1 | Only at DP charges, which are irreversible before access. | trigger_site_only |
| `archive_grant` | yes | 1 | Only in backup and restore flows. | trigger_site_only |
| `reservation` | yes | 1 | Only in backup and restore flows. | trigger_site_only |
| `catalog` | yes | 1 | Only in backup and restore flows. | trigger_site_only |
| `restore_dispatch_journal` | yes | 1 | Only in backup and restore flows. | trigger_site_only |
| `transparency_witness` | yes | 1 | Only at transparency-proof freshness. | trigger_site_only |
| `kms_hsm` | yes | 1 | Only at a key-unwrap miss. | trigger_site_only |

Every cell carries `operation_class_basis = trigger_site_only`: §16 item 8 states where each authority sits on the synchronous path but never names which of the sixteen operation classes the realized path uses. Binding realized operation classes is `fgdb-w9-authority-surface-gp9j`'s deliverable, measured against implemented paths rather than inferred here.

## 12. Checked source

The normative plan text this model is derived from, embedded verbatim. The checker re-reads both sides and fails on any drift.

<!-- CHECKED-SOURCE-BEGIN id="plan-external-authority-surface-v1" -->
> Source: `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md` lines 1226–1230 — §16 item 8 — the external-authority surface

8. **The external-authority surface.** Every registered posture publishes its authority footprint table: for each external authority — identity/allocation continuity (`IdentityContinuityRecord`/`AllocationEpochReservation`), cluster incarnation (`ClusterIncarnationContinuityRecord`), the time authority (`PortableTimeAuthorityObservation`), audit continuity (`AuditContinuityRecord`), the DP registry (`PrivacyContinuityRecord`), archive/grant (`ArchiveRetentionGrant`), reservation (`ReservationAuthorityHead`), catalog (`CatalogAuthorityHead`), the restore dispatch journal's `ExternalCasContinuity` head, transparency witnesses (`WitnessPolicy`), and KMS/HSM — the table states whether the authority sits on a synchronous operation path and exactly where. The v1 footprint: audit continuity touches a Required operation three times (the strictly serial one-at-a-time `TicketIssued` issuance CAS, the admission visibility advance, and its share of one batched `TicketsResolvedBatch` resolution); the time authority is consumed per protected-output activation tick — batch-subject amortized under §12.8 — and per `IdRangeLease` use window; incarnation continuity only at `ExternalCas` lease acquisition/renewal; KMS/HSM only at a key-unwrap miss; identity/allocation continuity only at allocation-epoch mint; the DP registry only at DP charges; archive, reservation, catalog, and the restore journal only in backup/restore flows; witnesses only at transparency-proof freshness. The embedded `DirectoryBound` posture's footprint table is empty: it consumes zero external authorities.

   Availability is stated per operation class as three lines, never as one product. **Apply-durability availability** is local storage plus Raft only — applies continue behind a stalled Required entry up to §12.8's pipeline limits, then degrade to deterministic backpressure, so a stalled audit authority cannot lose or block durable ordering. **Visibility availability** multiplies in the audit authority's availability under Required profiles, because only the Protocol-plane `AuditVisibilityAdvanceSpec` can release a Required entry. **Delivery availability** multiplies in the time authority's per-tick availability, which the design deliberately keeps off the audit prefix. Per-posture numbers for all three lines are `slo.toml` rows, never invariants; the §20 authority-availability risk row tracks them.

   The external authorities are inside the declared trust boundary and carry their own registered HA/durability contract: threshold signer sets with registered rotation, durable CAS heads with an explicit anti-rollback story, and a specified successor-authority migration. A threshold-authorized authority-succession transition — recorded as a permanent continuity event in the affected chain, format reserved at G0 — is the only path off a permanently dead authority; the completeness-transition escape hatch is not one, because its `CompletenessChanged` successor is CAS-installed by the same authority it would have to replace. §15.2's authority-chain checking runs the same no-fork lineage verification over succession events as over ordinary direct CAS successors.

<!-- CHECKED-SOURCE-END id="plan-external-authority-surface-v1" -->

<!-- CHECKED-SOURCE-BEGIN id="plan-registered-rejections-v1" -->
> Source: `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md` lines 153–157 — §3.4 — what we deliberately reject

Generic KV underlays (JanusGraph/Nebula tax); Gremlin as the primary language (imperative, optimizer-hostile — provided only as a later compat shim if ever); RDF-first modeling (property graph first; RDF import/view later); JVM anything; mmap-as-the-durability-story; PMA global rebalancing; eventual consistency as a default; external crates (constraint #1).

Four seductive alternatives were evaluated in depth and rejected with recorded reasons: (a) native hyperedges; (b) an unbounded per-fragment representation zoo; (c) a separate full GraphBLAS executor; and (d) plan racing as the default. Racing survives only behind deterministic-output buffering and registered safe cutovers (§8.5). Weaver-style refinable timestamps were set aside for 1.0 because one global scalar commit order simplifies Chronicle, branches, Ripple, and replay; post-1.0 sharding workstream W12 must preserve that contract through a meta-Raft global transaction record or explicitly reopen the decision.

Further rejections: (e) a second Calvin-style transaction lane unless measured aborts justify reopening it; (f) WASM as a foreign UDF runtime; (g) external-codegen JIT; (h) TrueTime-class infrastructure for 1.0—HLC plus consensus-fenced read contracts supplies the semantics actually specified, not arbitrary follower linearizability; (i) FHE/MPC/ORAM in the initial threat model; (j) learned indexes as authoritative access paths; and (k) a spatial index in 1.0 — `Point(2D/3D)` values are storable scalars but are not orderable and not indexable, a declared index over a `Point` property fails closed as a typed error, and the canonical `Point` byte encoding exists for digests and equality only and is explicitly not a spatial order; the recorded post-1.0 route is a Z-order/Hilbert-curve mapping over the existing B-tree machinery, with an in-house R-tree admitted only if profiling defeats the curve mapping, and the Neo4j differential corpus documents the divergence (`point()`, `distance()`, point indexes) as a registered non-goal. DP and leakage measurements remain scoped mechanisms, not substitutes for noninterference.

<!-- CHECKED-SOURCE-END id="plan-registered-rejections-v1" -->

<!-- CHECKED-SOURCE-BEGIN id="plan-threat-and-authority-model-v1" -->
> Source: `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md` lines 860–864 — §12.1 — threat and authority model

### 12.1 Threat and authority model

The trust matrix distinguishes untrusted clients, mutually hostile tenants, honest-but-curious storage, crash-fault replicas, malicious/stale donors, independent transparency witnesses, trusted embedded host code, and a compromised operator/server. The baseline trusts the executing database process and active key boundary to enforce plaintext confidentiality and policy; a compromised server process can exfiltrate data it can decrypt, and this plan claims no TEE/Byzantine-confidentiality magic. Witnessed transparency/audit may detect only their scoped history/administrative attacks, not undo disclosure. Ordinary Raft tolerates crash faults, not Byzantine replicas. Every deployment profile declares actors, protected assets, assumptions, and out-of-scope failures before selecting a claim.

Stable identities include `TenantId`, `PrincipalId`, `IssuerId`, `TokenId`, `DatabaseId`, `SecurityPolicyEpoch`, `RevocationIndex`, `DecisionPolicyEpoch`, and `KeyEpoch`. The security and adaptive epoch types have distinct wire tags and Rust newtypes and are never comparable or substitutable. `EffectiveAuthority` binds tenant/subject/issuer/audience, database security namespace and cluster incarnation, token/security-policy epochs, current revocation index, operation classes, graph/branch/label/edge/property/system-field/temporal scopes, procedure/UDF/module ObjectIds and invocation mode, external-I/O classes, Replay evidence/resolver closure, redaction profile, and resource ceiling. Operation classes are `Read, Mutate, Ddl, Subscribe, Analytics, Replay, ExecuteProcedure, InstallModule, ExternalIo, Export, Backup, Restore, Observe, Admin, KeyManage, Replicate`.

<!-- CHECKED-SOURCE-END id="plan-threat-and-authority-model-v1" -->

<!-- CHECKED-SOURCE-BEGIN id="plan-token-attenuation-law-v1" -->
> Source: `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md` lines 866–868 — §12.2 — token lifecycle, root issuance, and the attenuation law

### 12.2 Token lifecycle and restricted policy IR

Macaroons follow `Issued -> Active -> Attenuated -> Revoked|Expired`. Root issuance creates exact portable `MacaroonTokenBytes {format_version,root_header:{issuer_id,mac_key_id,mac_key_epoch,root_issuance_sequence,root_issuance_receipt_digest,root_token_id,time_authority_profile_oid,root_not_before,root_expires_at,audience,database_security_namespace_id,cluster_incarnation,service_visibility_epoch,tenant,subject,security_state_binding_public_commitment,root_non_widenable_authority_ceiling_commitment,root_caveats,attenuation_semantics_oid,presentation_binding:ServerInternal|SessionBound{session_binding,auth_generation}|ChannelBound{transport_exporter_digest}},attenuation_chain[{prior_authenticator_digest,narrowed_not_before?,narrowed_expires_at?,authority_scope_intersection?,resource_scope_intersection?,additional_caveats,presentation_binding_restriction?,authenticator}],effective_projection,final_mac}`. `root_token_id = H("fgdb:macaroon-root-token:v1",issuer_id,mac_key_id,mac_key_epoch,root_issuance_sequence,root_issuance_receipt_digest)` is immutable and is the `RevocationSubject::Token` identity; attenuation never creates a new token ID. Every link MACs the prior authenticator, retains the complete root tuple/profile/cohort, may only raise the effective not-before, lower the effective expiry, intersect authority/resource scopes, append accepted caveats, and preserve or further restrict presentation binding. It cannot extend time, change authority domain, widen disclosure, move cohorts, or change Session/Channel binding to a less restrictive class. The derived effective projection is verified from the immutable root plus complete chain and is never trusted as an independent authority input. Each online issuance/replay record retains its exact `AuthorityBindingRef`; offline root issuance is allowed only from a non-exportable counter-backed issuer-key epoch and is covered by the fenced cohort/retirement machinery in §16.6. A post-fence attenuation of an already issued root remains legal and is accounted for by that cohort; minting a new root after the issuer-epoch fence is not. An `AuthorizationLease` is a short-lived, authority-leader-signed capability binding token ID, `AuthorityBoundHeader`, non-widenable ceiling, consensus/leader epoch, profiled window, permitted snapshot/read scope, disclosure ceiling, and the same exact presentation-binding union. Signatures/MACs cover the complete binding commitment and window. Long queries/streams reauthorize before lease expiry or a security-boundary observation; unsent output is discarded on failure. Maximum TTL is the explicit revocation-enforcement bound.

<!-- CHECKED-SOURCE-END id="plan-token-attenuation-law-v1" -->

## 13. Provenance

- Registry: `registries/threat_model.toml` (schema 1)
- Replay: `cargo run -p registry-check --bin threat-check -- --root .`
- Bound invariants: FG-INV-08, FG-INV-20
- Bound evidence: FG-EVID-05
- Identity-table hash: `fnv1a64:48faccd46e13140a`
- Semantic-contract hash: `fnv1a64:a005965fbe24413f`
