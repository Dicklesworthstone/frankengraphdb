# Graph-only neighborhood search

`fgdb search` can retrieve a bounded neighborhood without text documents,
embeddings, or property bindings. A seed list and an explicit hop bound are a
complete request. This uses the existing indexed graph expansion and exact
reciprocal-rank fusion implementation, not an empty text or vector search.

```sh
fgdb --robot search --db graph.db --key-file keys \
  --expand-from 1,2 --max-hops 3 --k 20
```

Replace the example IDs with vertex identities from the database. Schema names
still need explicit bindings when used:

```sh
fgdb --robot search --db graph.db --key-file keys \
  --relation CITES=1 --expand-relation CITES \
  --expand-from 1 --max-hops 3 --expand-direction out \
  --eligible 40,50 --k 2 --graph-candidates 2
```

The seed and intermediate vertices need not be eligible. `--eligible` restricts
result endpoints **before** the candidate limit, so an excluded nearer endpoint
does not consume one of those two slots. It is not an authorization check.
In contrast, `--vertex-label` selects the induced graph: excluded vertices cannot
be seeds, results, or transit vertices. Relation selection also constrains
traversal. Direction is `out`, `in`, or `both`, with `out` the default.

## Seeds, ranking, and limits

Seeds are excluded from results by default, including when a cycle returns to
them. To return admitted seeds, including vertices isolated in the selected
relation, without expanding edges:

```sh
fgdb --robot search --db graph.db --key-file keys \
  --expand-from 1,2 --max-hops 0 --include-seeds true --k 2
```

Duplicate seeds do not duplicate results. Missing and out-of-selection seeds
behave as absent. An empty `--eligible` value produces an empty successful answer;
it does not disable input validation or grant an unlimited resource allowance.
Seed input is bounded before its vector grows past the native expansion ceiling
(currently 10,000 occurrences). Duplicates count toward that ingress ceiling,
even at zero hops and with an empty endpoint filter.

Graph candidates are ordered by minimum hop count, then vertex identity. Their
ranks are dense among eligible endpoints. Graph-only scores use the same exact
RRF arithmetic as hybrid retrieval: graph weight divided by `60 + graph_rank`,
with the usual canonical Decimal rendering. `--graph-weight` defaults to 1 and
must be a positive `u16`.

`--k` defaults to 10. The graph candidate depth is `--graph-candidates`, otherwise
`--candidates`, otherwise `--k`. `k` cannot exceed that depth in graph-only mode;
invalid depth and output-limit requests refuse before storage opens. Traversal
still retains the native vertex, incidence-visit, seed, scratch, and cumulative
work ceilings. A small final `k` is not permission to traverse an unbounded graph.
A resource refusal is an error, not a truncated successful answer.

## Historical reads and output

`--as-of <sequence>` selects an admitted committed cut, including its historical
edges and vertex selection. Without it, search pins the opened view's frontier.
The terminal result reports the sequence that answered, not a refreshed frontier.
Search is a local-owner read-only command; capability-scoped applications should
use the existing authorized native query APIs rather than treating these filters
as access controls.

Graph-only output uses the existing eight-column graph-search schema:
`vertex`, `score`, `vector_rank`, `text_rank`, `graph_rank`, `vector_distance`,
`text_score`, and `graph_hops`. All text/vector rank and score/distance cells are
null; vertex identities, Decimal scores, and integer graph ranks/hops remain typed.
A complete robot invocation ends with `kind=searched`, the selected sequence,
and the delivered row count. A refusal or missing successful terminal record is
not a complete result.

Graph-only retrieval does not project stored properties or build a text/vector
index. It still prepares the selected vertex directory and traverses reached
historical incidence below the hop bound; it is not a claim of constant-memory
or disk-streaming graph search. Adding text or vector arguments continues to use
the existing hybrid path, with its existing candidate-limited and ANN semantics.
