# Query language

`yesno query` accepts a compact expression language over stored posting
lists. Every expression denotes a set of ordinals. A query can return those
ordinals in ascending order or return only the exact cardinality.

## Grammar

```text
EXPR := NUMBER
      | key(NUMBER)
      | range(NUMBER, NUMBER)
      | { NUMBER, NUMBER, ... }
      | {}
      | and(EXPR, EXPR, ...)
      | or(EXPR, EXPR, ...)
      | xor(EXPR, EXPR)
      | and-not(EXPR, EXPR)
      | not(EXPR)
      | empty
      | view-select(NUMBER, VIEW, NUMBER)
      | view-fold(NUMBER, VIEW, REDUCTION)
      | view-expand(EXPR, VIEW)

VIEW := interleaved(NUMBER)
      | blocked(NUMBER, NUMBER)

REDUCTION := any | all | parity
```

Operator names are case-insensitive. ASCII whitespace can appear between
tokens, and underscores can group digits in numbers. A bare number is shorthand
for `key(NUMBER)`:

```text
42
key(42)
key(1_000_042)
AND(42, OR(7, 9))
```

Keys can use the full unsigned 64-bit range. Ordinals range from `0` through
`u64::MAX - 1`; `u64::MAX` is reserved as an exclusive range bound.

## Operators

| Form | Result |
|---|---|
| `key(k)` or `k` | the stored posting list for key `k` |
| `range(lo, hi)` | every ordinal `x` such that `lo <= x < hi` |
| `{a, b, ...}` | exactly the listed ordinals |
| `and(a, b, ...)` | ordinals present in every operand |
| `or(a, b, ...)` | ordinals present in at least one operand |
| `xor(a, b)` | ordinals present in exactly one operand |
| `and-not(a, b)` | ordinals in `a` but not in `b` |
| `not(a)` | every valid ordinal not in `a` |
| `view-select(k, view, i)` | logical ordinals in constituent `i` of packed key `k` |
| `view-fold(k, view, any)` | union of all constituents in packed key `k` |
| `view-fold(k, view, all)` | intersection of all constituents in packed key `k` |
| `view-fold(k, view, parity)` | symmetric difference of all constituents in packed key `k` |
| `view-expand(a, view)` | every physical constituent slot for each logical ordinal in `a` |
| `empty` | the empty set |

`and` and `or` need at least two operands and may take more. `xor` and
`and-not` take exactly two. All grouping is explicit, so there is no operator
precedence to remember.

An absent key denotes an empty set. If key `404` is absent, `and(1,404)` is
empty, while `or(1,404)` equals key `1`.

Ranges are half-open. `range(10, 13)` contains `10`, `11`, and `12`. The full
ordinal universe is `range(0, 18446744073709551615)`, which is also the universe
used by `not`.

An ordinal-set literal is unordered and duplicate-insensitive. For example,
`{9, 1, 9, 5}` denotes the same set as `{1, 5, 9}`, and `{}` denotes the empty
set. Literal members may use numeric underscores, but may not be
`18446744073709551615`, because that value is reserved as the exclusive range
bound.

## Packed views

A packed view stores several logical sets under one physical key. The view
layout is request metadata, not a server-side catalog entry, so every query
names the descriptor explicitly:

```text
interleaved(sets)       physical = logical * sets + constituent
blocked(sets, stride)   physical = constituent * stride + logical
```

`sets` must fit in `u32` and be non-zero. A blocked stride must be non-zero and
is also the exclusive logical-ordinal capacity of each constituent. The
constituent passed to `view-select` is zero-based and must be less than `sets`.

For example, select constituent 2 from an interleaved three-set view stored at
key 90, then compute the union of a blocked four-set view at key 91:

```console
yesno query 'view-select(90,interleaved(3),2)'
yesno query 'view-fold(91,blocked(4,65_536),any)'
```

To ingest logical rows without calculating physical ordinals yourself, provide
`constituent,ordinal` rows to `view-put`. Omitting `--stride` selects the
interleaved layout; providing it selects blocked layout:

```console
printf '0,10\n1,10\n2,25\n' | yesno view-put 90 --sets 3 -
printf '0,10\n1,10\n2,25\n' | yesno view-put 91 --sets 3 --stride 65536 -
```

The server persists only the packed set at the key. Keep the layout alongside
your application schema and use it consistently: the stored bits cannot reveal
whether they were packed as interleaved or blocked. `view-select` has a dedicated
exact-count path; folds and expansion are currently eager transforms.

## Examples

Find objects that have key `42` and either key `7` or key `9`:

```console
yesno query 'and(42,or(7,9))'
```

Restrict a posting list to the first million ordinals:

```console
yesno query 'and(42,range(0,1_000_000))'
```

Restrict a posting list to an explicit collection of ordinals:

```console
yesno query 'and(42,{1,5,9})'
```

Remove disabled objects from a tenant population:

```console
yesno query 'and-not(1_000,2_003)'
```

Find membership in exactly one of two sets:

```console
yesno query 'xor(12,13)'
```

Count every valid ordinal that is not in key `42`:

```console
yesno query --count-only 'not(42)'
```

## Count, limit, and fetch

Use `--count-only` when the count is the answer:

```console
yesno query --count-only 'and(42,or(7,9))'
```

The server returns the exact cardinality without sending the ordinal stream.
This is materially different from fetching every row and counting in the
client.

Use `-n` or `--limit` to inspect a prefix while retaining the exact total in the
diagnostic header:

```console
yesno query -n 20 'or(7,9)'
```

The limit is applied by the client after the server has described the complete
result. It limits printed rows, not the reported cardinality, and it is not an
offset or pagination cursor.

For one stored key, `yesno count KEY` and `yesno get KEY` are shorter
forms of the cardinality and retrieval operations.

## Snapshot semantics

The server evaluates one query against one database version. Before returning
rows it reports the exact count and a ticket for that version, so concurrent
writes do not make the count and rows describe different states.

A ticket is not a lease. A checkpoint can reclaim the named version before the
client fetches it. If that happens, request the query again to obtain a fresh
count and ticket. A ticket must also be used on the same server that issued it;
presenting a leader's ticket to a lagging replica can name a version the replica
has never seen.

## Practical limits

The wire format accepts at most 32 levels of nesting and 4096 expression nodes.
These are safety limits, not recommended query sizes. Very broad `or`
expressions are often easier to maintain as a precomputed posting list.

Complement deserves special care. `not(a)` is usually enormous even when `a`
is large, because the universe contains nearly `2^64` ordinals. Prefer one of:

- `--count-only` when only the number matters;
- `-n LIMIT` for inspection; or
- `and-not(candidate, excluded)` when the application has a bounded candidate
  population.

Quoting the expression is recommended. Parentheses have meaning to many shells,
and shells commonly expand an unquoted `{1,5,9}` before `yesno` receives it. A
quoted argument reaches `yesno` unchanged.

Parse errors report a one-based byte position and the expected token. If the
expression passes local parsing but the server rejects it, check the depth and
node limits and confirm that the client and server support the same expression
version.
