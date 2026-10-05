# Query language

`yesno query` accepts a compact expression language over stored posting
lists. A query can return ordinals in ascending order or return only the exact
cardinality.

The language is **sorted**. Most expressions denote a set of ordinals, but a
view of a set denotes a **vector of sets** -- its constituents -- and the two
are not interchangeable. A vector becomes a set by being indexed, folded, or
packed, and nothing else accepts one. Sorts are checked before a query runs.

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
      | _
      | VEC [ NUMBER ]
      | fold(VEC, FOLD_OP)
      | pack(VEC, VIEW)
      | expand(EXPR, VIEW)
      | select(EXPR, NUMBER)
      | map(VEC, BOOL)

VEC  := [ EXPR, EXPR, ... ]
      | view(EXPR, VIEW)
      | map(VEC, EXPR)

INTVEC := map(VEC, INT)

INT  := NUMBER
      | cardinality(EXPR)
      | rank(EXPR, NUMBER)

BIG  := SIGNED
      | big(SIGNED)
      | widen(INT)
      | uint(EXPR [, NUMBER])
      | int(EXPR [, NUMBER])
      | neg(BIG)
      | add(BIG, BIG)
      | sub(BIG, BIG)
      | mul(BIG, BIG)
      | div(BIG, BIG)
      | rem(BIG, BIG)
      | truncate(BIG, NUMBER)
      | saturate(BIG, NUMBER)

      | fold(BIGVEC, BIGFOLD)
      | powmod(BIG, BIG, BIG)

BIGVEC := map(VEC, BIG)
        | big([ BIG, BIG, ... ])
        | zip(BIGVEC, BIGVEC, BIGBIN)
        | scale(BIGVEC, BIG, BIGBIN)

BIGBIN := add | sub | mul | div | rem

BIGFOLD := add | mul | min | max

SIGNED := NUMBER | -NUMBER

BOOL := contains(EXPR, NUMBER)

VIEW := interleaved(NUMBER)
      | blocked(NUMBER, NUMBER)

FOLD_OP := or | and | xor
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
| `empty` | the empty set |

### Vectors of sets

| Form | Sort | Result |
|---|---|---|
| `view(a, view)` | vector | `a` read as the view's constituents |
| `[a, b, ...]` | vector | a literal vector of those sets |
| `v[i]` | set | constituent `i` of vector `v`, zero-based |
| `fold(v, or)` | set | union of every element of `v` |
| `fold(v, and)` | set | intersection of every element of `v` |
| `fold(v, xor)` | set | symmetric difference of every element of `v` |
| `pack(v, view)` | set | `v`'s elements packed into one set under `view` |
| `expand(a, view)` | set | every constituent slot for each logical ordinal in `a` |

`view` and `pack` are inverses: `view` reads one set as constituents and `pack`
writes constituents back into one set, so `pack(view(a, v), v)` is `a`. Indexing
is zero-based, and the index must be below the vector's length -- which is known
before the query runs, from the descriptor or the literal, so an out-of-range
index is refused rather than evaluated.

`fold` takes exactly three operators, and the restriction is not arbitrary. A
fold needs an operation that is associative and has an identity, and on single
bits there are only four such operations: `or`, `xor`, `and`, and equivalence.
Equivalence keeps every logical ordinal whose constituents are all absent, so
its result is as large as the whole logical universe however small the input --
which is exactly what a sparse set cannot produce. `and-not` is neither
associative nor commutative. That leaves three.

A view of a *computed* set is allowed: `fold(view(and(1, 2), interleaved(4)),
or)` folds the constituents of an intersection, not of a stored key.

### Applying a query to every constituent

`map(v, body)` applies `body` to each element of `v`, where `_` stands for the
element. It does **not** combine the elements -- that is `fold` -- and the
distinction is the whole reason the language has sorts:

| Form | Result |
|---|---|
| `map(v, and(_, q))` | a vector: each constituent restricted to `q` |
| `map(v, cardinality(and(_, q)))` | one integer per constituent: **facet counts under `q`** |
| `map(v, cardinality(_))` | one integer per constituent: each one's size |
| `map(v, rank(_, x))` | one integer per constituent |
| `map(v, contains(_, x))` | a set: which constituents hold `x` |

The second line is the query most applications want and could not previously
ask for. It counts, per cohort, how many of its members satisfy a filter:

```console
yesno query 'map(view(90,interleaved(3)),cardinality(and(_,42)))'
```

A `map` whose body is an integer denotes **one integer per constituent**, which
is a different answer shape from a set -- it is a whole query rather than
something you can nest inside `and`. A `map` whose body is a boolean asks which
constituents satisfy it, and that *is* a set, because a truth value per
constituent is exactly a subset of the constituent indices.

Every `_` in one body means the same element. A `map` may not appear inside
another `map`'s body, because `_` carries no index and could not say which
element it meant; a `map` in the *vector* position is fine, since that is one
after another rather than one inside another.

`select(a, n)` is the `n`-th smallest ordinal of `a`, as a one-element set -- or
the empty set if `a` holds fewer than `n + 1` ordinals. It returns a set rather
than a number because it is genuinely partial, and an empty set says so without
needing a reserved value that a caller could forget to check.

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

`sets` must be non-zero and at most 4096. The upper bound is not a storage
limit: the server visits every constituent a descriptor declares, so an
unbounded count would let a request a few dozen bytes long ask for an
unbounded amount of work. A blocked stride must be non-zero and is also the
exclusive logical-ordinal capacity of each constituent.

For example, select constituent 2 from an interleaved three-set view stored at
key 90, then compute the union of a blocked four-set view at key 91:

```console
yesno query 'view(90,interleaved(3))[2]'
yesno query 'fold(view(91,blocked(4,65_536)),or)'
```

The operand of `view` is an ordinary expression, so a view can be taken of a
computed set rather than only of a stored key:

```console
yesno query 'fold(view(and(90,91),interleaved(3)),or)'
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
whether they were packed as interleaved or blocked. Counting a single
constituent has a dedicated path that never builds it; folds and expansion are
currently eager transforms.

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

## Arbitrary-precision integers

A set **is** a number: ordinal `j` contributes `2^j`. So a posting list can be
read as an integer, and `uint(EXPR, WIDTH)` is that reading, keeping the low
`WIDTH` bits.

```text
uint(key(4), 128)         the low 128 bits of key 4, as a magnitude
int(key(4), 8)            the same bits read as a signed 8-bit field
```

**Reading narrower is exactly `x mod 2^WIDTH`.** That falls out of
least-significant-bit-first ordering and is the only thing the width does. The
width is required, not optional: it is the only bound on how much work a read
can ask for, and a set with a distant highest member is cheap to store and
expensive to render.

`uint` and `int` are two different numbers from the same bits — `int` treats
the top bit of the width as a sign — and nothing in the bits says which was
meant, so they are separate spellings rather than a flag.

**The width may be omitted where the shape supplies one.** Inside a `map` over
a *blocked* view, a constituent's logical universe is exactly its stride, so
the stride is the width:

```text
map(view(9, blocked(3, 128)), uint(_))        the same as uint(_, 128)
```

Nowhere else. An interleaved constituent's ordinals are unbounded, and a bare
set has no statically known extent, so in both the width stays required — it is
the only cap on what a read can cost. Omitting it is a convenience at the point
of writing: the query sent is identical either way.

Arithmetic is exact: sums and products grow as wide as they need to, and
nothing silently wraps.

```text
mul(uint(key(1), 64), big(-3))
add(widen(cardinality(key(7))), big(1))
div(big(7), big(2))           3, truncating toward zero
rem(big(-7), big(2))          -1, the sign of the dividend
```

Two rules narrow a value into a fixed-width field, and they are different:

```text
truncate(big(300), 8)     44   wraps, two's complement
saturate(big(300), 8)     127  clamps to the largest the field holds
saturate(big(-300), 8)   -128  the negative side reaches one further
```

Division by zero is an error rather than a value. Results print as hexadecimal
with a `0x` prefix.

### One value per cohort, and reducing them

`map` over a view builds one big integer per constituent, the same way it
builds one count per constituent:

```text
map(view(9, interleaved(3)), uint(_, 128))
```

That answers three values — the stored integer in each cohort. `fold` collapses
such a vector to a single value:

```text
fold(map(view(9, interleaved(3)), uint(_, 128)), add)
fold([big(1), big(2), big(3)], mul)
```

The four operators are `add`, `mul`, `min` and `max`. All four are associative
and commutative, so the answer does not depend on the order constituents happen
to be visited in. None of them needs an identity, because a vector is never
empty — which is what lets `min` and `max` be offered at all.

`fold` is also the set-vector operator (`or`, `and`, `xor`); which one you get
is decided by the vector, not the keyword.

### Modular exponentiation

```text
powmod(2, 10, 1000)                       2^10 mod 1000
```

A bare number is a big-integer literal wherever a big integer is **required**,
so `big(...)` is rarely needed for a single value:

```text
add(1, 2)              the same as add(big(1), big(2))
powmod(2, 10, 1000)
```

That does not change what a number means anywhere else. The position decides
the sort before the number is read, so a bare number is still a key in a set
position — including inside `uint` and `int`, whose operand is a set:

```text
42                     key 42, as always
uint(4, 128)           the low 128 bits of key 4
```

**A bracketed list is a vector of sets, everywhere, with no exception.** A
literal vector of big integers is written `big([ ... ])`, which says which sort
it is at the point the vector is written rather than leaving it to an operator
further along:

```text
fold([1, 2, 3], or)        the union of posting lists 1, 2 and 3
fold(big([1, 2, 3]), add)  six
```

Those two would otherwise be the same vector text meaning very different
things, told apart only by the last token.

### Element-wise arithmetic

`zip` combines two vectors position by position, and `scale` applies one value
to every position:

```text
zip(map(view(9, blocked(3, 64)), uint(_)), big([1, 2, 3]), mul)
scale(map(view(9, blocked(3, 64)), uint(_)), 1000, div)
```

**The two vectors must have the same arity, and that is checked when the query
is parsed** — both arities are known without evaluating anything, so a mismatch
names both sides rather than silently pairing off the shorter one.

The scalar in `scale` is the **right** operand: `scale(v, x, sub)` is
`v[i] - x`. That matters for `sub`, `div` and `rem`, and nothing in the query
says it otherwise.

**`zip` is the only way to correlate two vectors by position.** A `map` body
never learns which constituent it is on — the `_` hole is that constituent's
set, not its index — and a `fold` combines elements in no particular order. So
neither can express "pair element `i` with element `i`".

The operators differ from `fold`'s for a reason rather than an oversight.
`fold` offers `min` and `max` because a fold needs an answer that does not
depend on visiting order; `zip` offers `sub`, `div` and `rem` because it visits
each position once and needs no such property. `fold(v, sub)` and
`zip(a, b, min)` are both rejected.

The base enters its residue class first, so the answer is always in
`[0, modulus)` and never carries a sign. A **zero modulus** and a **negative
exponent** are both errors rather than answers: the first has no residues at
all, and the second is a modular inverse, which this engine does not compute.

**There are size limits, and they are separate because one does not imply the
other.** An expression whose single result could exceed roughly one million
bits is refused when it is parsed — multiplication adds the operands' widths,
so a few nested products over wide reads describe a value no server should try
to build, and a `mul` fold over many wide constituents does the same. A *vector*
is additionally capped at about two million bits for the whole answer: the
constituent count and the per-value width are each bounded, but their product is
not bounded by either. Both are refused before anything is evaluated.

There is also a limit on **work**, which the size limits cannot express.
`powmod` returns a value no wider than its modulus, so nothing about its size
is alarming, while its cost grows with the exponent's bit count times the
square of the modulus's. A query estimated to need more than about 270 million
limb operations is refused. The estimate is deliberately pessimistic — it
assumes schoolbook multiplication — so it errs toward refusing work that would
in fact have been affordable, and it is a limit rather than a prediction of how
long anything takes.

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

A newly issued ticket holds a bounded lease on its snapshot version. Once that
lease expires, checkpointing can reclaim the version before the client fetches
it. If that happens, request the query again to obtain a fresh count and ticket.
A ticket must also be used on the same server that issued it; presenting a
leader's ticket to a lagging replica can name a version the replica has never
seen.

## Practical limits

The wire format accepts at most 32 levels of nesting and 4096 expression nodes,
and a big-integer result of at most about one million bits. These are safety
limits, not recommended query sizes. Very broad `or`
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
