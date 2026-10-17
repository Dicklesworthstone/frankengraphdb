# Explicit numeric vector projection

Beacon indexes and searches binary32 (f32) vectors. The default
`coordinates` encoding still accepts only integers and binary64 values that
are exactly representable as finite f32; it never silently changes the stored
coordinate. Packed little-endian f32 bytes remain the byte-exact alternative.

Use `coordinates-rounded` when the application deliberately wants to project
ordinary numeric properties (including values such as 0.1) onto f32:

```sh
fgdb --robot search --db graph.db --key-file keys \
  --property x=1 --property y=2 \
  --vector 0.1,0.2 --vector-property x --vector-property y \
  --vector-encoding coordinates-rounded --metric l2 --k 10
```

The same choice is available in native reads, including the shared server
query path:

```sql
CALL hybrid.search(
  vector => [0.1, 0.2],
  vector_properties => ['x', 'y'],
  vector_encoding => 'coordinates-rounded',
  metric => 'l2', k => 10
)
YIELD node, vector_distance
RETURN node, vector_distance
ORDER BY vector_distance, node
```

`vector_encoding` is an ordinary text argument and can be parameterized.
Omission/null preserves the existing defaults: `vector_properties` selects
exact numeric coordinates, while singular `vector_property` selects packed
f32 bytes. An explicit `packed` encoding requires the singular declaration;
`coordinates` and `coordinates-rounded` require the plural declaration. A
nonnull encoding without a vector, an unknown encoding, and a mismatched
coordinate count are rejected rather than guessed.

## Numeric and authority contract

Rounded mode converts finite integers and binary64 values directly to f32.
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

The Rust API uses `VectorEncoding::CoordinatesRoundedF32`. Resident, pinned,
commit-maintained and authorized index definitions retain that encoding across
searches and refreshes. This adds no persistent index descriptor, automatic
server index cache, spill mechanism, or new dependency. Existing source,
work, memory-admission and result bounds remain in force; decimal conversion
also charges bounded work before formatting its short canonical spelling.
