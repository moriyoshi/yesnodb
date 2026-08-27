# Data modeling

yesnodb stores one relationship:

```text
u64 key -> set of u64 ordinals
```

The key usually identifies a term, tag, feature, cohort, or predicate. The
ordinals are stable identifiers for the things that satisfy it. A document
catalog, for example, might assign one ordinal to every document and one key to
every tag.

## Choose one ordinal space

An ordinal should mean the same thing for every key that can appear in one
query. If ordinal `731` means a document under one key and a customer under
another, intersecting those keys produces a syntactically valid but meaningless
answer.

Good ordinal assignments are:

- stable for the lifetime of the indexed object;
- unique within the data set;
- cheap to translate back to an application record; and
- independent of mutable attributes such as a display name.

A database sequence or another durable integer identifier is usually suitable.
If the application's identifiers are strings or UUIDs, maintain a dictionary
between them and ordinals. Reusing an ordinal for a different object can make
old posting-list membership appear to describe the new object.

The valid ordinal universe is `0` through `u64::MAX - 1`. The value `u64::MAX`
is reserved. Keys use the full `u64` range; the restriction applies only to set
members.

## Give keys a namespace

Keys are numbers on disk, even when the application thinks in names. Define a
stable mapping and treat it as part of the data format. Two common approaches
are:

1. Store a collision-free dictionary from `(kind, value)` to key.
2. Partition the key space by assigning ranges or high-bit prefixes to kinds,
   then allocate within each partition.

For example, tags, tenant membership, and lifecycle state can occupy distinct
namespaces. Record the mapping outside yesnodb and back it up with the database.
Without it, the posting lists remain readable but their meaning is lost.

A 64-bit hash is convenient when rare collisions are acceptable or separately
detected. It is not a collision-free dictionary by itself. Do not silently let
two terms that hash alike share a posting list.

## Represent common relationships

### Tags and facets

Use one key per tag or facet value and insert the object's ordinal into every
matching set:

```text
42 (language=rust) -> {1, 5, 9}
43 (status=active) -> {1, 2, 5, 8, 9}
```

The conjunction of those keys finds active Rust objects. A multi-valued facet
needs no special representation; one ordinal can belong to any number of sets.

### Tenants

If ordinals are globally unique, add a tenant-membership key and include it in
every tenant-scoped query. If ordinals are only unique within a tenant, use a
separate database per tenant. Merely namespacing keys does not prevent two
tenants from colliding in the ordinal space.

### Time windows

For coarse time filtering, maintain keys for bounded buckets such as day or
hour and combine them with `or`. Use `range(lo, hi)` only when ordinal order
itself carries the desired meaning. It filters ordinal values; it does not read
a timestamp attribute.

### Negative attributes

Prefer a positive set such as `status=disabled` and subtract it from a bounded
candidate set:

```text
and-not(1_007, 2_003)
```

An unbounded `not(2_003)` is mathematically valid but spans almost the entire
64-bit ordinal universe. Here key `1_007` is tenant 7 and key `2_003` is the
disabled set. An unbounded complement is usually useful for a count, not for
returning rows.

## Pack related sets into one view

A view packs several logical sets into one physical posting list. This is useful
when the constituent count is fixed and the application often asks either for
one constituent or for a union, intersection, or parity fold across all of them.
Each logical member is addressed by `( constituent, logical ordinal )`.

Two layouts are available:

- `interleaved(n)` stores constituent `i`, logical ordinal `x` at `x * n + i`.
  The slots for one logical ordinal are adjacent, which favors folds.
- `blocked(n, stride)` stores it at `i * stride + x`. Each constituent is a
  contiguous region, which favors selection. The stride is also the exclusive
  capacity of every constituent.

A blocked stride that is a multiple of 65,536 aligns constituent boundaries to
container boundaries and makes selection especially cheap. Choose the layout
from the read pattern, then treat `(sets, layout, stride)` as durable application
schema. The server stores the packed bits under an ordinary key but does not
catalog the descriptor; using the wrong descriptor produces a different,
well-formed interpretation rather than a missing-key error.

`yesno view-put KEY --sets N [--stride STRIDE] FILE` accepts
`constituent,ordinal` rows and performs the mapping. Query the result with the
view forms in the [query-language reference](query-language.md).

## Plan updates as set changes

Membership is set-valued. Inserting an existing `(key, ordinal)` pair is
idempotent, and removing an absent pair has no visible effect. This makes
replaying an application-level change safe, provided the surrounding operation
does not assign a new ordinal or key.

Use one write batch when several memberships must become visible together. A
reader sees either side of a committed batch, never a partially visible subset.
When using Arrow Flight, each Arrow record batch is a separate database write
batch; split a stream only at boundaries where separate visibility is
acceptable.

The `yesno put` command inserts pairs. Removal is available through the
embedded API and through Flight `DoPut` with the `remove` command, but there is
currently no `yesno remove` subcommand.

If an object's attributes change, add its new memberships and remove its old
memberships in the same batch. Deleting the record in the primary system does
not automatically remove it from yesnodb.

## Design for query shape

The most selective positive sets should describe the initial candidate
population. Then use intersection and subtraction to refine it. This keeps
queries conceptually bounded and avoids enumerating huge complements.

Keep these distinctions in mind:

- A key identifies a stored set; an ordinal identifies a member.
- An absent key behaves like an empty set; it is not a lookup error.
- `range(lo, hi)` is a generated set over ordinal values, not a key range.
- Cardinality and row retrieval answer the same expression, but cardinality can
  avoid constructing and transporting the result.

Before loading a full corpus, build a small example with known answers. Check
single-key counts, representative intersections, and one query that should be
empty. These become useful acceptance checks after backup restoration or a
replica promotion.

For expression syntax, continue with the [query-language reference](query-language.md).
For durability and backup considerations, continue with the
[operations guide](operations.md).
