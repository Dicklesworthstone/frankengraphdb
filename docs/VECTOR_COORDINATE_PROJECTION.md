# Numeric vector projection

Beacon indexes and searches binary32 (f32) vectors. One property per
coordinate (`vector_properties`, `--vector-property` repeated) uses the
`coordinates-rounded` encoding by default: each stored integer, binary64 or
decimal coordinate rounds once to the nearest f32 (ties to even), the same
way at index build and at query time. Ordinary numeric properties such as
`SET p.e0 = 0.1, p.e1 = 0.2, p.e2 = 0.3` are therefore searchable as written
(fgdb-ekic6, ruling (a), 2026-10-21). Packed little-endian f32 bytes remain the
byte-exact alternative.

```sh
fgdb --robot search --db graph.db --key-file keys \
  --property x=1 --property y=2 \
  --vector 0.1,0.2 --vector-property x --vector-property y --metric l2 --k 10
```

The same default applies in native reads, including the shared server query
path:

```sql
CALL hybrid.search(
  vector => [0.1, 0.2],
  vector_properties => ['x', 'y'],
  metric => 'l2', k => 10
)
YIELD node, vector_distance
RETURN node, vector_distance
ORDER BY vector_distance, node
```

Exact mode stays available as an explicit opt-in: `vector_encoding =>
'coordinates'` (`--vector-encoding coordinates`) accepts only integers and
binary64 values that are exactly representable as finite f32, and one other
value anywhere in the selected corpus refuses the whole search with
`vector float is not finite exact f32` (or the integer/type equivalent). It
was the default before fgdb-ekic6.

`vector_encoding` is an ordinary text argument and can be parameterized.
Omission/null selects the defaults: `vector_properties` selects rounded
numeric coordinates, while singular `vector_property` selects packed f32
bytes. An explicit `packed` encoding requires the singular declaration;
`coordinates` and `coordinates-rounded` require the plural declaration. A
nonnull encoding without a vector, an unknown encoding, and a mismatched
coordinate count are rejected rather than guessed.

## Certificates and replay across the default change

An omitted `vector_encoding` is not written into a plan template, so a result
certificate issued before fgdb-ekic6 carries the old exact default only
implicitly. It still replays identically. Replay runs at the certified
sequence, and a certificate exists only for a successful read. A successful
exact read means every coordinate it projected at that sequence was exactly
representable as f32. Rounding is the identity on exactly representable
values, with the same work charges, so the replayed rows and result digest are
unchanged. Templates that name `vector_encoding` explicitly keep their
meaning, because the names `coordinates`, `coordinates-rounded` and `packed`
did not change.

## Numeric and authority contract

Rounded mode converts finite integers and binary64 values directly to f32,
as `hybrid.search` converts its numeric query vector.
Fixed-scale decimals convert from their canonical decimal spelling directly
to f32, without a binary64 intermediate. This avoids double rounding near an
f32 midpoint. Nonfinite values, overflow and wrong-typed properties refuse the
search; underflow to subnormal values or signed zero is permitted. The original
stored properties and commit history are never rewritten.

A missing/null coordinate omits the vector lane for that document, as before.
Authorization masks properties and vertices before projection. An excluded
property cannot become visible through its numeric type or overflow behavior.
Eligibility filtering remains separate from corpus authorization.

**Exact nearest-neighbor search is exact over the projected f32 vectors**, not
an exact-distance or rank-preservation claim about the original f64/decimal
values. Rounding can merge distinct vectors or change close rankings. `ann`
is still approximate; reciprocal-rank fusion is still over its candidate sets.
Cosine's existing zero-norm refusal is unchanged, including a vector that
underflows entirely to zero. Choose a projection/metric appropriate to the data.

The Rust API uses `VectorEncoding::CoordinatesRoundedF32`, which is
`VectorEncoding::default()` and the encoding `ReadOptions::text` starts from;
exact mode is `VectorEncoding::Coordinates`. Resident, pinned,
commit-maintained and authorized index definitions retain that encoding across
searches and refreshes. A CALL reuses a prepared or maintained index only when
the encodings match, so an index frozen with `Coordinates` serves only CALLs
that name `vector_encoding => 'coordinates'`. This adds no persistent index descriptor, automatic
server index cache, spill mechanism, or new dependency. Existing source,
work, memory-admission and result bounds remain in force; decimal conversion
also charges bounded work before formatting its short canonical spelling.
