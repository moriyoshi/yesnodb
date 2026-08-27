//! The set-expression wire format, shared by the yesno Flight server and its
//! clients.
//!
//! # Why this is its own crate
//!
//! It exists so that **one** definition of the format is compiled into both
//! sides. The server ( `yesno-flight` ) decodes; the PostgreSQL extension
//! ( `yesno-pg` ) encodes. Those two cannot share a crate any other way: the
//! extension deliberately does not depend on the server, and under Bazel they
//! resolve their third-party dependencies through different `crate_universe`
//! hubs, so a type from one is not the same type as from the other.
//!
//! That does not matter here, and the reason is the whole design: **the
//! interface is bytes.** One side calls `encode`, the other calls `decode`, and
//! the two never exchange a Rust value. Compiling this crate twice is therefore
//! harmless, while writing the encoder twice would be the "second decoder that
//! can drift" failure the server avoids by shipping raw WAL frames.
//!
//! This crate must stay dependency-free. Its whole value is being cheap
//! enough to link from anywhere.
//!
//! # This is a parser of untrusted bytes
//!
//! It holds the same contract as `yesno_core::container::codec::decode`: for
//! **any** input, [`SetExpr::decode`] returns `Err` or a valid expression, and
//! never panics. Three bounds make that true regardless of what arrives:
//!
//! - **Depth** is capped, because decoding is recursive and a deeply nested
//!   payload would otherwise overflow the stack — which is not a catchable
//!   error, so it must be prevented rather than handled.
//! - **Node count** is capped, because a small payload can otherwise describe an
//!   enormous tree and turn a request into an allocation attack.
//! - **A view's constituent count** is capped, because the first two bound the
//!   *tree* and this one does not appear in it: the evaluator loops over `sets`
//!   without reference to how much data exists. See [`MAX_VIEW_SETS`].
//!
//! Do not remove any of the three on the grounds that only `yesno-pg` sends
//! these. The server accepts them from any client on the network.

/// `YSNX`, then a version byte, then a reserved byte.
///
/// **The reserved byte is load-bearing, not padding.** The existing
/// descriptor form is a bare 8-byte little-endian key, and a key whose low four
/// bytes happen to spell `YSNX` — there are 2^32 of them, and keys are commonly
/// hashes — would otherwise be indistinguishable from an expression by its
/// prefix alone. Disambiguating by length instead requires that no valid
/// expression is exactly 8 bytes, and with a 5-byte header
/// `AndNot( Empty, Empty )` is exactly 8. The sixth header byte moves every
/// reachable encoding off 8: the possible totals become 7, 9, 10 and up.
///
/// Do not remove it to save a byte. The property it buys is that a key can
/// never be read as an expression, which is a wrong answer rather than an error.
pub const MAGIC: &[u8; 4] = b"YSNX";
pub const VERSION: u8 = 1;
/// Reserved for flags. Must be zero; a non-zero value is rejected so the field
/// stays free for a later version to define.
const RESERVED: u8 = 0;
/// `MAGIC` + version + reserved.
const HEADER_LEN: usize = 6;
/// The legacy descriptor: a bare little-endian `u64` key.
const BARE_KEY_LEN: usize = 8;

/// Versioned query-request envelope magic.
///
/// A plain [`SetExpr`] still means "read at the current version". This
/// envelope exists for coordinators that pair another index generation with a
/// particular yesno snapshot and must refuse rather than silently fall forward.
pub const QUERY_MAGIC: &[u8; 4] = b"YSNQ";
const QUERY_VERSION: u8 = 1;
const QUERY_FLAG_PINNED: u8 = 1;
const QUERY_KNOWN_FLAGS: u8 = QUERY_FLAG_PINNED;
const QUERY_HEADER_LEN: usize = 14;

/// Maximum nesting. A lowered SQL filter is a handful of levels; 32 is far
/// beyond anything a planner produces and far below what overflows a stack.
pub const MAX_DEPTH: usize = 32;

/// Maximum nodes in one expression. A filter with more terms than this is a
/// pathology, and rejecting it costs nothing a real query wanted.
pub const MAX_NODES: usize = 4096;

/// Maximum constituents one view descriptor may pack.
///
/// **The third amplification vector, and the one this file used to miss.**
/// [`MAX_DEPTH`] and [`MAX_NODES`] bound what a small payload can cost by
/// bounding the *tree*; `sets` bounds nothing of the sort, because the server
/// loops over it with no reference to how much data exists. Folding iterates
/// every constituent, and expanding iterates every constituent *per input
/// ordinal* — so an uncapped `u32` let a ~30-byte request ask for four billion
/// iterations, or a one-ordinal input to expand into a 32 GB result.
///
/// 4096 is chosen the way `MAX_NODES` is: far beyond what a real packing wants —
/// cohorts, facets, or the dimensions of a binary code are tens to hundreds —
/// and far below what turns a request into an attack. Above this the work is
/// still proportional to the *output*, which is the honest bound for a caller
/// who really did ask for a large expansion.
pub const MAX_VIEW_SETS: u32 = 4096;

const TAG_EMPTY: u8 = 0;
const TAG_KEY: u8 = 1;
const TAG_RANGE: u8 = 2;
const TAG_AND: u8 = 3;
const TAG_OR: u8 = 4;
const TAG_AND_NOT: u8 = 5;
const TAG_AT: u8 = 6;
const TAG_FOLD: u8 = 7;
const TAG_EXPAND: u8 = 8;
const TAG_LITERAL: u8 = 9;
const TAG_PACK: u8 = 10;
// Vector-sorted nodes share one tag space with the set-sorted ones above, so a
// node appearing where the other sort is required is reported as a sort
// mismatch naming both, rather than silently reinterpreted as whatever that
// byte means in the position it landed in.
const TAG_LIST: u8 = 11;
const TAG_VIEW: u8 = 12;
// Step 3: the hole, `map`, and the scalar queries.
const TAG_HOLE: u8 = 13;
const TAG_SELECT: u8 = 14;
const TAG_MAP_SET: u8 = 15;
const TAG_MAP_INT: u8 = 16;
const TAG_MAP_BOOL: u8 = 17;
const TAG_CARDINALITY: u8 = 18;
const TAG_RANK: u8 = 19;
const TAG_INT_LIT: u8 = 20;
const TAG_INT_AT: u8 = 21;
const TAG_INT_LIST: u8 = 22;
const TAG_CONTAINS: u8 = 23;

const VIEW_INTERLEAVED: u8 = 0;
const VIEW_BLOCKED: u8 = 1;
const FOLD_OR: u8 = 0;
const FOLD_AND: u8 = 1;
const FOLD_XOR: u8 = 2;

/// How a packed view's constituents share the physical ordinal space.
///
/// This is deliberately a dependency-free wire description rather than
/// `yesno_core::view::ViewLayout`: clients must be able to construct a request
/// without linking the storage engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewLayout {
    /// Constituent `i`, logical ordinal `x` is stored at `x * sets + i`.
    Interleaved,
    /// Constituent `i`, logical ordinal `x` is stored at `i * stride + x`.
    Blocked { stride: u64 },
}

/// The descriptor required to interpret one stored key as a packed view.
///
/// The descriptor travels with every query and is not catalogued by the
/// server. Stored bits cannot reveal which descriptor originally produced
/// them, so callers must consistently use the same value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewSpec {
    /// Number of logical constituent sets.
    pub sets: u32,
    /// Physical layout of those constituents.
    pub layout: ViewLayout,
}

impl ViewSpec {
    /// An interleaved descriptor with `sets` constituents.
    pub fn interleaved(sets: u32) -> Self {
        Self {
            sets,
            layout: ViewLayout::Interleaved,
        }
    }

    /// A blocked descriptor with `sets` regions of width `stride`.
    pub fn blocked(sets: u32, stride: u64) -> Self {
        Self {
            sets,
            layout: ViewLayout::Blocked { stride },
        }
    }

    /// Is this descriptor self-consistent?
    ///
    /// A zero constituent count and a zero blocked stride cannot address a
    /// useful view, and a count above [`MAX_VIEW_SETS`] is an amplification
    /// rather than a query — see that constant for why the bound is here and not
    /// left to the evaluator. [`ViewSpec::ordinal_of`] still reports individual
    /// slots that overflow the ordinal universe.
    ///
    /// The decoder calls this while parsing, so an oversized descriptor is
    /// refused **before** anything evaluates it.
    pub fn check(self) -> Result<(), ExprError> {
        if self.sets == 0 {
            return Err(ExprError::ViewHasNoConstituents);
        }
        if self.sets > MAX_VIEW_SETS {
            return Err(ExprError::ViewHasTooManyConstituents);
        }
        if matches!(self.layout, ViewLayout::Blocked { stride: 0 }) {
            return Err(ExprError::ViewHasZeroStride);
        }
        Ok(())
    }

    /// Physical ordinal holding logical ordinal `x` of constituent `set`.
    ///
    /// Returns `None` for an invalid descriptor, an unknown constituent, a
    /// logical ordinal past a blocked stride, arithmetic overflow, or
    /// `u64::MAX`, which is reserved by the core ordinal invariant.
    pub fn ordinal_of(self, set: u32, x: u64) -> Option<u64> {
        if self.check().is_err() || set >= self.sets {
            return None;
        }
        let ordinal = match self.layout {
            ViewLayout::Interleaved => x.checked_mul(self.sets as u64)?.checked_add(set as u64)?,
            ViewLayout::Blocked { stride } => {
                if x >= stride {
                    return None;
                }
                (set as u64).checked_mul(stride)?.checked_add(x)?
            }
        };
        (ordinal != u64::MAX).then_some(ordinal)
    }

    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.sets.to_le_bytes());
        match self.layout {
            ViewLayout::Interleaved => {
                out.push(VIEW_INTERLEAVED);
                // Fixed-width and canonical: an interleaved view has no stride.
                out.extend_from_slice(&0u64.to_le_bytes());
            }
            ViewLayout::Blocked { stride } => {
                out.push(VIEW_BLOCKED);
                out.extend_from_slice(&stride.to_le_bytes());
            }
        }
    }
}

/// The sort a tag belongs to, or `None` if no version defines it.
///
/// **One table, consulted by every decoder's fallback arm.** Enumerating other
/// sorts' tags inside each arm is how a tag ends up reported as unknown in one
/// position and as a sort mismatch in another -- which is what happened while
/// this was being written. A single table keeps all five decoders in step.
fn sort_of_tag(tag: u8) -> Option<Sort> {
    Some(match tag {
        TAG_EMPTY | TAG_KEY | TAG_RANGE | TAG_AND | TAG_OR | TAG_AND_NOT | TAG_AT | TAG_FOLD
        | TAG_EXPAND | TAG_LITERAL | TAG_PACK | TAG_HOLE | TAG_SELECT | TAG_MAP_BOOL => Sort::Set,
        TAG_LIST | TAG_VIEW | TAG_MAP_SET => Sort::VecSet,
        TAG_MAP_INT | TAG_INT_LIST => Sort::VecInt,
        TAG_CARDINALITY | TAG_RANK | TAG_INT_LIT | TAG_INT_AT => Sort::Int,
        TAG_CONTAINS => Sort::Bool,
        _ => return None,
    })
}

/// The error for a tag arriving where `expected` was required.
///
/// A known tag of another sort is a sort mismatch; an undefined one is unknown.
fn misplaced(expected: Sort, tag: u8) -> ExprError {
    match sort_of_tag(tag) {
        Some(_) => ExprError::SortMismatch { expected, tag },
        None => ExprError::UnknownTag(tag),
    }
}

/// Which sort a node denotes.
///
/// The language is **multi-sorted**: a node yields a set of ordinals or a
/// vector of them, and the two are not interchangeable. Sorts are checked while
/// decoding rather than by a separate pass, because the decoder already knows
/// which sort each position requires and can say so when a tag does not fit.
/// **`Vec` does not nest**, so the lattice is finite and closed at five. That is
/// what keeps every index statically checkable and the checker a finite table.
/// `Vec[Bool]` is absent on purpose: a `Bool`-valued vector over the constituent
/// indices *is* a subset of them, so it is a [`Sort::Set`] by the
/// characteristic-function isomorphism, and giving it a separate sort would
/// describe one object twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    /// A set of ordinals.
    Set,
    /// A fixed-arity vector of sets -- a view's constituents, or a literal list.
    VecSet,
    /// A single count or position.
    Int,
    /// One integer per constituent. **Always exactly `sets` long**, which is why
    /// the sort alone says which axis a reduction collapsed: Proposition 27
    /// forbids a reduction linear in the logical universe, so anything indexed
    /// by *ordinal* must stay sparse and only something indexed by
    /// *constituent* may be dense.
    VecInt,
    /// A single truth value. Only a `map` body, never a query result.
    Bool,
}

impl std::fmt::Display for Sort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sort::Set => write!(f, "set"),
            Sort::VecSet => write!(f, "vector of sets"),
            Sort::Int => write!(f, "integer"),
            Sort::VecInt => write!(f, "vector of integers"),
            Sort::Bool => write!(f, "boolean"),
        }
    }
}

/// How [`SetExpr::Fold`] combines a vector's elements.
///
/// **Exactly three, and provably so.** `docs/formal-model.md` Proposition 27
/// shows that on `{0,1}` an associative, unital operation is automatically
/// commutative and is one of four -- `or`, `xor` ( unit 0 ) and `and`, `iff`
/// ( unit 1 ). `iff` is excluded on density: a fibre of zeros is *kept*, so the
/// result's support has the size of the logical universe however small the
/// operand is, which Proposition 3 forbids from a sparse operand. `andnot` is
/// not a candidate at all, being neither associative nor commutative.
///
/// So this is not a list that grows. A fourth entry would have to be one of
/// those two, and both are excluded for stated reasons rather than by taste.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldOp {
    /// Union -- set when **any** element holds the ordinal.
    Or,
    /// Intersection -- set when **every** element holds it.
    And,
    /// Symmetric difference -- set when an **odd** number hold it.
    Xor,
}

/// A set expression, as a client builds it.
///
/// Deliberately **not** `yesno_core::Expr`. That type carries an
/// `Arc<OrdSet>` — a materialized set — which cannot cross a network, and it is
/// `#[non_exhaustive]` so it may gain variants this format has no encoding for.
/// Keeping them separate is what makes the wire format's compatibility a
/// property of this file alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetExpr {
    /// Matches nothing.
    Empty,
    /// A whole posting list.
    Key(u64),
    /// A half-open ordinal range `[lo, hi)`.
    ///
    /// Half-open, matching `Expr::Range` and `Snapshot::len_in_range`, and
    /// **not** `Db::insert_range`'s inclusive convention. Both exist in yesno on
    /// purpose; this is the read side.
    Range(u64, u64),
    /// A materialized ordinal-set literal.
    ///
    /// Values are canonical on the wire: strictly ascending, unique, and at
    /// most `u64::MAX - 1`. Keeping that invariant here makes identical sets
    /// encode identically and prevents the reserved ordinal from reaching
    /// `yesno-core` through a network request. Prefer [`SetExpr::literal`] to
    /// constructing this raw variant directly.
    Literal(Vec<u64>),
    And(Vec<SetExpr>),
    Or(Vec<SetExpr>),
    AndNot(Box<SetExpr>, Box<SetExpr>),
    /// One element of a vector, by zero-based index -- `v[ i ]`.
    ///
    /// **Total on a well-formed expression.** A vector's arity is known from
    /// its shape or its literal length, so an out-of-range index is refused
    /// while decoding rather than discovered while evaluating.
    ///
    /// This replaces the old `ViewSelect`, and the name change is a correction:
    /// under the reading that a view is a curried relation, picking a
    /// constituent is *application*, not a select. `select` in this project
    /// means the succinct-structure operation -- the `n`-th smallest ordinal --
    /// and giving one word to both would collide with it.
    At(Box<VecSetExpr>, u32),
    /// Combine every element of a vector into one set.
    ///
    /// Replaces the old `ViewFold`, which took a bare key and so could only
    /// fold a stored posting list. This takes a [`VecSetExpr`], so
    /// `fold( view( and( key( a ), key( b ) ), shape ), or )` is expressible.
    Fold(Box<VecSetExpr>, FoldOp),
    /// Pack a vector's elements back into one set under `shape`.
    ///
    /// The inverse of [`VecSetExpr::View`]: that curries a set into
    /// constituents, this uncurries them. The vector's arity must equal
    /// `shape.sets`, which is checked while decoding.
    Pack(Box<VecSetExpr>, ViewSpec),
    /// Every constituent's slot, for each logical ordinal in the operand.
    ///
    /// The inverse image, and **not** expressible as `pack` of anything this
    /// format can build -- it is the middle of the adjoint triple and a
    /// homomorphism of the whole Boolean signature, so it stays its own node.
    Expand(Box<SetExpr>, ViewSpec),
    /// `_` -- the element of the enclosing [`VecSetExpr::Map`] body.
    ///
    /// The language has no variables, so this is a hole rather than a name, and
    /// there is no binder, no scope and no closure: a body is an ordinary
    /// expression that may mention it. **Every `_` in one body denotes the same
    /// element**, which has to be said because it is the opposite of the
    /// glyph's most famous precedent -- in Scala `_ + _` is a *binary* function
    /// with two distinct parameters.
    ///
    /// Refused outside a map body ( [`ExprError::HoleOutsideMap`] ), and a map
    /// inside a map body is refused too ( [`ExprError::NestedMap`] ) so that an
    /// un-indexed hole can never silently shadow.
    Hole,
    /// The `n`-th smallest ordinal, as a singleton -- or empty if there is none.
    ///
    /// The succinct-structures `select`, matching `OrdSet::select`. It returns a
    /// **set** rather than an integer because it is genuinely partial -- no type
    /// knows a set's cardinality -- and a singleton composes with the existing
    /// algebra where an option sort or a sentinel would not.
    Select(Box<SetExpr>, u64),
    /// `map` with a `Bool`-sorted body: which constituents satisfy it.
    ///
    /// The result is `Vec[Bool]`, which **is** a set of constituent indices, so
    /// this yields a [`Sort::Set`] rather than a vector sort. It is the column
    /// of the matrix, and the transpose of [`SetExpr::At`]'s row.
    MapBool(Box<VecSetExpr>, Box<BoolExpr>),
}

/// A single count or position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntExpr {
    /// A literal.
    Lit(u64),
    /// How many ordinals a set holds.
    Cardinality(Box<SetExpr>),
    /// How many of a set's ordinals are strictly below a position.
    Rank(Box<SetExpr>, u64),
    /// One element of a vector of integers, by zero-based index.
    At(Box<VecIntExpr>, u32),
}

/// A single truth value. Only ever a `map` body -- never a query result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoolExpr {
    /// Whether a set holds one ordinal.
    Contains(Box<SetExpr>, u64),
}

/// One integer per constituent.
///
/// **Always exactly the vector's arity long.** Proposition 27 forbids a
/// reduction linear in the logical universe, so a result indexed by ordinal must
/// stay sparse and only one indexed by constituent may be dense -- which is what
/// makes this sort unambiguous about which axis was collapsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VecIntExpr {
    /// A literal vector of integers.
    List(Vec<IntExpr>),
    /// A scalar query applied to every element of a vector of sets.
    ///
    /// This is a **map, not a fold**: it does not combine the elements, it
    /// applies a query to each. `map( v, cardinality( _ ) )` is the
    /// per-constituent cardinality; `fold( v, or )` is the union. Confusing the
    /// two is confusing the two marginals of the same matrix, which do not
    /// determine each other.
    Map(Box<VecSetExpr>, Box<IntExpr>),
}

impl VecIntExpr {
    /// How many elements this vector has.
    pub fn arity(&self) -> u32 {
        match self {
            VecIntExpr::List(xs) => xs.len() as u32,
            VecIntExpr::Map(v, _) => v.arity(),
        }
    }
}

/// A fixed-arity vector of sets.
///
/// The sort a view produces. Kept a separate type rather than a variant of
/// [`SetExpr`] so that Rust refuses an ill-sorted tree at construction, leaving
/// the decoder to check only what arrives as bytes.
///
/// **Vectors do not nest**: the element sort is a set, never another vector, so
/// the sort lattice is finite and every index is statically checkable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VecSetExpr {
    /// A literal vector, `[ a, b, c ]`.
    ///
    /// Empty is refused, for the reason an empty `And` or `Or` is: there is no
    /// element sort to infer and no identity to assume.
    List(Vec<SetExpr>),
    /// Read one set as `shape.sets` constituents -- the curry direction.
    View(Box<SetExpr>, ViewSpec),
    /// A set-valued query applied to every element -- `map( v, and( _, q ) )`.
    ///
    /// `map` is functorial for any body, so unlike [`FoldOp`] there is no
    /// closure theorem constraining what may appear here and the body is an
    /// arbitrary expression.
    Map(Box<VecSetExpr>, Box<SetExpr>),
}

impl VecSetExpr {
    /// How many elements this vector has.
    ///
    /// Always statically known, which is what makes [`SetExpr::At`] total: a
    /// literal knows its length and a view takes its arity from the descriptor.
    pub fn arity(&self) -> u32 {
        match self {
            // A list longer than `u32::MAX` cannot be built: `MAX_NODES` caps
            // the element count far below it.
            VecSetExpr::List(xs) => xs.len() as u32,
            VecSetExpr::View(_, spec) => spec.sets,
            // A map preserves shape, which is the functor law that makes the
            // arity of a mapped vector knowable without evaluating it.
            VecSetExpr::Map(v, _) => v.arity(),
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            VecSetExpr::List(xs) => {
                out.push(TAG_LIST);
                // `u16` for the same reason `And` and `Or` use one: `MAX_NODES`
                // caps the element count far below what a wider count encodes.
                out.extend_from_slice(&(xs.len() as u16).to_le_bytes());
                for x in xs {
                    x.write(out);
                }
            }
            VecSetExpr::View(input, view) => {
                out.push(TAG_VIEW);
                view.write(out);
                input.write(out);
            }
            VecSetExpr::Map(v, body) => {
                out.push(TAG_MAP_SET);
                v.write(out);
                body.write(out);
            }
        }
    }
}

impl VecIntExpr {
    fn write(&self, out: &mut Vec<u8>) {
        match self {
            VecIntExpr::List(xs) => {
                out.push(TAG_INT_LIST);
                out.extend_from_slice(&(xs.len() as u16).to_le_bytes());
                for x in xs {
                    x.write(out);
                }
            }
            VecIntExpr::Map(v, body) => {
                out.push(TAG_MAP_INT);
                v.write(out);
                body.write(out);
            }
        }
    }
}

impl IntExpr {
    fn write(&self, out: &mut Vec<u8>) {
        match self {
            IntExpr::Lit(v) => {
                out.push(TAG_INT_LIT);
                out.extend_from_slice(&v.to_le_bytes());
            }
            IntExpr::Cardinality(a) => {
                out.push(TAG_CARDINALITY);
                a.write(out);
            }
            IntExpr::Rank(a, x) => {
                out.push(TAG_RANK);
                out.extend_from_slice(&x.to_le_bytes());
                a.write(out);
            }
            IntExpr::At(v, i) => {
                out.push(TAG_INT_AT);
                v.write(out);
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
    }
}

impl BoolExpr {
    fn write(&self, out: &mut Vec<u8>) {
        match self {
            BoolExpr::Contains(a, x) => {
                out.push(TAG_CONTAINS);
                out.extend_from_slice(&x.to_le_bytes());
                a.write(out);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExprError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u8),
    UnknownTag(u8),
    TooDeep,
    TooManyNodes,
    /// An `And` or `Or` with no children. Rejected rather than treated as an
    /// identity, because the two identities differ ( all / nothing ) and a
    /// client that emitted one is confused about which it meant.
    EmptyJunction,
    ViewHasNoConstituents,
    /// More constituents than [`MAX_VIEW_SETS`]. Rejected at decode because the
    /// evaluator loops over `sets` without reference to how much data exists, so
    /// an uncapped count is an amplification vector rather than a large query.
    ViewHasTooManyConstituents,
    ViewHasZeroStride,
    /// An index at or above the vector's arity. Refused while decoding because
    /// arity is always statically known -- see [`VecSetExpr::arity`].
    IndexOutOfRange,
    /// A node of the wrong sort for the position it appears in.
    SortMismatch {
        /// What the position required.
        expected: Sort,
        /// The tag that arrived.
        tag: u8,
    },
    /// `pack` of a vector whose arity is not the descriptor's constituent count.
    ArityMismatch {
        /// The descriptor's `sets`.
        expected: u32,
        /// The vector's arity.
        found: u32,
    },
    /// A `[ .. ]` with no elements. Refused for the reason [`ExprError::EmptyJunction`]
    /// is: there is no element sort to infer and no identity to assume.
    EmptyVector,
    /// `_` outside any `map` body, where it stands for nothing.
    HoleOutsideMap,
    /// A `map` inside a `map` body.
    ///
    /// Refused rather than allowed to shadow. An un-indexed hole cannot say
    /// which element it means, and refusing is one check in five
    /// implementations where de Bruijn indices would be a binder discipline in
    /// five implementations. A `map` in the *vector* position is fine -- that is
    /// sequential, not nested.
    NestedMap,
    OrdinalOutOfRange(u64),
    NonCanonicalLiteral,
    NonCanonicalView,
    UnknownViewLayout(u8),
    /// A fold operator byte that is none of the three. See [`FoldOp`].
    UnknownFoldOp(u8),
    TrailingBytes,
}

impl std::fmt::Display for ExprError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExprError::Truncated => write!(f, "expression ended mid-node"),
            ExprError::BadMagic => write!(f, "not a yesno expression"),
            ExprError::UnsupportedVersion(v) => write!(f, "unsupported expression version {v}"),
            ExprError::UnknownTag(t) => write!(f, "unknown expression tag {t}"),
            ExprError::TooDeep => write!(f, "expression nested deeper than {MAX_DEPTH}"),
            ExprError::TooManyNodes => write!(f, "expression has more than {MAX_NODES} nodes"),
            ExprError::EmptyJunction => write!(f, "AND/OR with no operands"),
            ExprError::ViewHasNoConstituents => write!(f, "view packs zero constituents"),
            ExprError::ViewHasTooManyConstituents => {
                write!(f, "view packs more than {MAX_VIEW_SETS} constituents")
            }
            ExprError::ViewHasZeroStride => write!(f, "blocked view stride is zero"),
            ExprError::IndexOutOfRange => {
                write!(f, "index is at or above the vector's arity")
            }
            ExprError::SortMismatch { expected, tag } => {
                write!(f, "tag {tag} where a {expected} was required")
            }
            ExprError::ArityMismatch { expected, found } => {
                write!(f, "packing {found} sets under a {expected}-set descriptor")
            }
            ExprError::EmptyVector => write!(f, "vector has no elements"),
            ExprError::HoleOutsideMap => write!(f, "`_` outside a map body"),
            ExprError::NestedMap => write!(f, "a map body may not contain a map"),
            ExprError::OrdinalOutOfRange(v) => {
                write!(f, "{v} is outside the ordinal universe")
            }
            ExprError::NonCanonicalLiteral => {
                write!(
                    f,
                    "ordinal-set literal is not strictly ascending and unique"
                )
            }
            ExprError::NonCanonicalView => write!(f, "view descriptor is not canonical"),
            ExprError::UnknownViewLayout(v) => write!(f, "unknown view layout {v}"),
            ExprError::UnknownFoldOp(v) => write!(f, "unknown fold operator {v}"),
            ExprError::TrailingBytes => write!(f, "trailing bytes after expression"),
        }
    }
}

impl SetExpr {
    /// Build a canonical materialized ordinal-set literal.
    ///
    /// Input order and duplicates do not affect set semantics. The reserved
    /// `u64::MAX` value is rejected before anything can be encoded.
    pub fn literal(ordinals: impl IntoIterator<Item = u64>) -> Result<SetExpr, ExprError> {
        let mut ordinals: Vec<u64> = ordinals.into_iter().collect();
        if ordinals.contains(&u64::MAX) {
            return Err(ExprError::OrdinalOutOfRange(u64::MAX));
        }
        ordinals.sort_unstable();
        ordinals.dedup();
        Ok(SetExpr::Literal(ordinals))
    }

    /// Build `a XOR b` using the v1 wire operators.
    ///
    /// XOR has no dedicated v1 tag. Expressing it as
    /// `(a OR b) AND NOT (a AND b)` keeps old servers compatible while giving
    /// clients the full Boolean surface. The operands are duplicated in the
    /// encoded tree, so callers constructing very large generated expressions
    /// should stay mindful of [`MAX_NODES`].
    pub fn xor(a: SetExpr, b: SetExpr) -> SetExpr {
        SetExpr::AndNot(
            Box::new(SetExpr::Or(vec![a.clone(), b.clone()])),
            Box::new(SetExpr::And(vec![a, b])),
        )
    }

    /// Build the complement of `expr` over yesno's ordinal universe.
    ///
    /// The universe is the half-open range `[0, u64::MAX)`: `u64::MAX` itself
    /// is reserved and is not a storable ordinal. Writing complement in terms
    /// of `Range` and `AndNot` avoids adding a wire tag and remains readable by
    /// every v1 server.
    pub fn complement(expr: SetExpr) -> SetExpr {
        SetExpr::AndNot(Box::new(SetExpr::Range(0, u64::MAX)), Box::new(expr))
    }

    /// Encode, including the magic and version prefix.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(RESERVED);
        self.write(&mut out);
        debug_assert_ne!(
            out.len(),
            BARE_KEY_LEN,
            "an 8-byte encoding would be ambiguous with a bare key"
        );
        out
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            SetExpr::Empty => out.push(TAG_EMPTY),
            SetExpr::Key(k) => {
                out.push(TAG_KEY);
                out.extend_from_slice(&k.to_le_bytes());
            }
            SetExpr::Range(lo, hi) => {
                out.push(TAG_RANGE);
                out.extend_from_slice(&lo.to_le_bytes());
                out.extend_from_slice(&hi.to_le_bytes());
            }
            SetExpr::Literal(ordinals) => {
                out.push(TAG_LITERAL);
                out.extend_from_slice(&(ordinals.len() as u32).to_le_bytes());
                for ordinal in ordinals {
                    out.extend_from_slice(&ordinal.to_le_bytes());
                }
            }
            SetExpr::And(xs) | SetExpr::Or(xs) => {
                out.push(if matches!(self, SetExpr::And(_)) {
                    TAG_AND
                } else {
                    TAG_OR
                });
                // `u16` is enough: `MAX_NODES` is 4096, so a longer list cannot
                // decode anyway and a wider count would only encode garbage.
                out.extend_from_slice(&(xs.len() as u16).to_le_bytes());
                for x in xs {
                    x.write(out);
                }
            }
            SetExpr::AndNot(a, b) => {
                out.push(TAG_AND_NOT);
                a.write(out);
                b.write(out);
            }
            SetExpr::At(v, i) => {
                out.push(TAG_AT);
                v.write(out);
                out.extend_from_slice(&i.to_le_bytes());
            }
            SetExpr::Fold(v, op) => {
                out.push(TAG_FOLD);
                v.write(out);
                out.push(match op {
                    FoldOp::Or => FOLD_OR,
                    FoldOp::And => FOLD_AND,
                    FoldOp::Xor => FOLD_XOR,
                });
            }
            SetExpr::Pack(v, view) => {
                out.push(TAG_PACK);
                view.write(out);
                v.write(out);
            }
            SetExpr::Expand(input, view) => {
                out.push(TAG_EXPAND);
                view.write(out);
                input.write(out);
            }
            SetExpr::Hole => out.push(TAG_HOLE),
            SetExpr::Select(a, n) => {
                out.push(TAG_SELECT);
                out.extend_from_slice(&n.to_le_bytes());
                a.write(out);
            }
            SetExpr::MapBool(v, body) => {
                out.push(TAG_MAP_BOOL);
                v.write(out);
                body.write(out);
            }
        }
    }

    /// Decode a whole payload. Returns `Err` for any malformed input.
    pub fn decode(bytes: &[u8]) -> Result<SetExpr, ExprError> {
        if bytes.len() < HEADER_LEN {
            return Err(ExprError::Truncated);
        }
        if &bytes[..4] != MAGIC {
            return Err(ExprError::BadMagic);
        }
        if bytes[4] != VERSION {
            return Err(ExprError::UnsupportedVersion(bytes[4]));
        }
        if bytes[5] != RESERVED {
            // Rejected rather than ignored, so the field stays genuinely free
            // for a later version to give meaning to.
            return Err(ExprError::UnsupportedVersion(bytes[4]));
        }
        let mut cur = Cursor {
            b: &bytes[HEADER_LEN..],
            at: 0,
            nodes: 0,
            in_map: false,
        };
        let e = cur.expr(0)?;
        if cur.at != cur.b.len() {
            return Err(ExprError::TrailingBytes);
        }
        Ok(e)
    }

    /// Whether a descriptor payload looks like an expression rather than a bare
    /// key. Cheap, and the only thing the dispatch needs.
    pub fn looks_like_expr(bytes: &[u8]) -> bool {
        // The length test comes first and is not redundant. A bare key is
        // exactly 8 bytes and its *contents* are arbitrary, so one can begin
        // with the magic by coincidence. No valid expression is 8 bytes — see
        // `RESERVED` — so length alone settles that case.
        bytes.len() != BARE_KEY_LEN && bytes.len() >= HEADER_LEN && &bytes[..4] == MAGIC
    }

    /// The set of keys this expression reads, for callers that need to know
    /// what it touches before evaluating it.
    pub fn keys(&self, out: &mut Vec<u64>) {
        match self {
            SetExpr::Empty | SetExpr::Range(_, _) | SetExpr::Literal(_) => {}
            SetExpr::Key(k) => out.push(*k),
            SetExpr::And(xs) | SetExpr::Or(xs) => xs.iter().for_each(|x| x.keys(out)),
            SetExpr::AndNot(a, b) => {
                a.keys(out);
                b.keys(out);
            }
            SetExpr::Expand(input, _) => input.keys(out),
            // The view nodes used to name a key directly; now they wrap an
            // arbitrary set expression, so the walk recurses instead of
            // reading a field. A caller relying on this to find what a query
            // touches gets *more* keys than before, never fewer.
            SetExpr::At(v, _) | SetExpr::Fold(v, _) | SetExpr::Pack(v, _) => v.keys(out),
            SetExpr::MapBool(v, body) => {
                v.keys(out);
                body.keys(out);
            }
            SetExpr::Select(a, _) => a.keys(out),
            // The hole names no key: it stands for an element of the vector the
            // enclosing map walks, and that vector is walked separately.
            SetExpr::Hole => {}
        }
    }
}

impl IntExpr {
    /// The set of keys this integer expression reads. See [`SetExpr::keys`].
    pub fn keys(&self, out: &mut Vec<u64>) {
        match self {
            IntExpr::Lit(_) => {}
            IntExpr::Cardinality(a) | IntExpr::Rank(a, _) => a.keys(out),
            IntExpr::At(v, _) => v.keys(out),
        }
    }
}

impl BoolExpr {
    /// The set of keys this boolean expression reads. See [`SetExpr::keys`].
    pub fn keys(&self, out: &mut Vec<u64>) {
        match self {
            BoolExpr::Contains(a, _) => a.keys(out),
        }
    }
}

impl VecIntExpr {
    /// The set of keys this vector reads. See [`SetExpr::keys`].
    pub fn keys(&self, out: &mut Vec<u64>) {
        match self {
            VecIntExpr::List(xs) => xs.iter().for_each(|x| x.keys(out)),
            VecIntExpr::Map(v, body) => {
                v.keys(out);
                body.keys(out);
            }
        }
    }
}

impl VecSetExpr {
    /// The set of keys this vector reads. See [`SetExpr::keys`].
    pub fn keys(&self, out: &mut Vec<u64>) {
        match self {
            VecSetExpr::List(xs) => xs.iter().for_each(|x| x.keys(out)),
            VecSetExpr::View(input, _) => input.keys(out),
            VecSetExpr::Map(v, body) => {
                v.keys(out);
                body.keys(out);
            }
        }
    }
}

/// A whole query, at whichever sort it denotes.
///
/// A query used to be a set and nothing else. It still usually is -- but
/// `map( view( k, shape ), cardinality( _ ) )` denotes one integer per
/// constituent, which is a facet histogram and is the reason this sort exists.
///
/// Only the two sorts a server can actually *return* appear here. `Vec[Set]` is
/// not among them: a vector of sets has no single answer shape on the wire, and
/// it always reaches a query result through `fold`, `pack` or an index anyway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnyExpr {
    /// The usual case: a set of ordinals.
    Set(SetExpr),
    /// One integer per constituent.
    VecInt(VecIntExpr),
}

impl AnyExpr {
    /// The sort this query denotes.
    pub fn sort(&self) -> Sort {
        match self {
            AnyExpr::Set(_) => Sort::Set,
            AnyExpr::VecInt(_) => Sort::VecInt,
        }
    }

    /// Encode with the `YSNX` header, exactly as [`SetExpr::encode`] does.
    ///
    /// The header carries no sort byte: the first tag already determines it, and
    /// a redundant second statement of the same fact could disagree with itself.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(RESERVED);
        match self {
            AnyExpr::Set(e) => e.write(&mut out),
            AnyExpr::VecInt(e) => e.write(&mut out),
        }
        debug_assert_ne!(
            out.len(),
            BARE_KEY_LEN,
            "an 8-byte encoding would be ambiguous with a bare key"
        );
        out
    }

    /// Decode a whole payload at whichever sort it carries.
    pub fn decode(bytes: &[u8]) -> Result<AnyExpr, ExprError> {
        let mut cur = Self::open(bytes)?;
        // The leading tag decides the sort, and the two decoders reject each
        // other's tags, so this dispatch cannot silently pick the wrong one.
        let out = match cur.b.first() {
            Some(&TAG_INT_LIST) | Some(&TAG_MAP_INT) => AnyExpr::VecInt(cur.vec_int_expr(0)?),
            _ => AnyExpr::Set(cur.expr(0)?),
        };
        if cur.at != cur.b.len() {
            return Err(ExprError::TrailingBytes);
        }
        Ok(out)
    }

    fn open(bytes: &[u8]) -> Result<Cursor<'_>, ExprError> {
        if bytes.len() < HEADER_LEN {
            return Err(ExprError::Truncated);
        }
        if &bytes[..4] != MAGIC {
            return Err(ExprError::BadMagic);
        }
        if bytes[4] != VERSION || bytes[5] != RESERVED {
            return Err(ExprError::UnsupportedVersion(bytes[4]));
        }
        Ok(Cursor {
            b: &bytes[HEADER_LEN..],
            at: 0,
            nodes: 0,
            in_map: false,
        })
    }
}

/// A remotely executable expression and the snapshot version it requires.
///
/// `version = None` has the same meaning as a legacy bare [`SetExpr`]. A
/// named version is strict: the server must evaluate exactly that version or
/// return an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryRequest {
    pub expression: SetExpr,
    pub version: Option<u64>,
}

impl QueryRequest {
    pub fn current(expression: SetExpr) -> Self {
        Self {
            expression,
            version: None,
        }
    }

    pub fn at(expression: SetExpr, version: u64) -> Self {
        Self {
            expression,
            version: Some(version),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(QUERY_HEADER_LEN + 32);
        out.extend_from_slice(QUERY_MAGIC);
        out.push(QUERY_VERSION);
        out.push(if self.version.is_some() {
            QUERY_FLAG_PINNED
        } else {
            0
        });
        out.extend_from_slice(&self.version.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.expression.encode());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ExprError> {
        if bytes.len() < QUERY_HEADER_LEN {
            return Err(ExprError::Truncated);
        }
        if &bytes[..4] != QUERY_MAGIC {
            return Err(ExprError::BadMagic);
        }
        if bytes[4] != QUERY_VERSION {
            return Err(ExprError::UnsupportedVersion(bytes[4]));
        }
        let flags = bytes[5];
        if flags & !QUERY_KNOWN_FLAGS != 0 {
            return Err(ExprError::UnsupportedVersion(bytes[4]));
        }
        let raw_version = u64::from_le_bytes(
            bytes[6..QUERY_HEADER_LEN]
                .try_into()
                .expect("query header contains exactly eight version bytes"),
        );
        let pinned = flags & QUERY_FLAG_PINNED != 0;
        if !pinned && raw_version != 0 {
            return Err(ExprError::TrailingBytes);
        }
        Ok(Self {
            expression: SetExpr::decode(&bytes[QUERY_HEADER_LEN..])?,
            version: pinned.then_some(raw_version),
        })
    }

    pub fn looks_like_request(bytes: &[u8]) -> bool {
        bytes.len() >= QUERY_HEADER_LEN && &bytes[..4] == QUERY_MAGIC
    }
}
struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
    nodes: usize,
    /// Whether decoding is currently inside a `map` body.
    ///
    /// A bool rather than a counter because nesting is *refused*, not tracked:
    /// see [`ExprError::NestedMap`]. Set only while a body is being decoded, so
    /// a `map` in a vector position -- which is sequential rather than nested --
    /// is unaffected.
    in_map: bool,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], ExprError> {
        let end = self.at.checked_add(n).ok_or(ExprError::Truncated)?;
        if end > self.b.len() {
            return Err(ExprError::Truncated);
        }
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u64(&mut self) -> Result<u64, ExprError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes(s.try_into().expect("took exactly 8")))
    }

    fn u16(&mut self) -> Result<u16, ExprError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes(s.try_into().expect("took exactly 2")))
    }

    fn u32(&mut self) -> Result<u32, ExprError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes(s.try_into().expect("took exactly 4")))
    }

    fn view(&mut self) -> Result<ViewSpec, ExprError> {
        let sets = self.u32()?;
        let layout = *self.take(1)?.first().expect("took exactly 1");
        let stride = self.u64()?;
        let view = match layout {
            VIEW_INTERLEAVED if stride == 0 => ViewSpec::interleaved(sets),
            VIEW_INTERLEAVED => return Err(ExprError::NonCanonicalView),
            VIEW_BLOCKED => ViewSpec::blocked(sets, stride),
            other => return Err(ExprError::UnknownViewLayout(other)),
        };
        view.check()?;
        Ok(view)
    }

    fn expr(&mut self, depth: usize) -> Result<SetExpr, ExprError> {
        if depth > MAX_DEPTH {
            return Err(ExprError::TooDeep);
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ExprError::TooManyNodes);
        }
        let tag = *self.take(1)?.first().expect("took exactly 1");
        Ok(match tag {
            TAG_EMPTY => SetExpr::Empty,
            TAG_KEY => SetExpr::Key(self.u64()?),
            TAG_RANGE => {
                let lo = self.u64()?;
                let hi = self.u64()?;
                SetExpr::Range(lo, hi)
            }
            TAG_LITERAL => {
                let n = self.u32()? as usize;
                // Validate the payload length before reserving attacker-chosen
                // capacity. The request bytes already occupy `8 * n`; decoding
                // may mirror them once, but a short payload cannot demand a
                // large allocation for free.
                let byte_len = n.checked_mul(8).ok_or(ExprError::Truncated)?;
                let bytes = self.take(byte_len)?;
                let mut ordinals = Vec::with_capacity(n);
                let mut previous = None;
                for &raw in bytes.as_chunks::<8>().0 {
                    let ordinal = u64::from_le_bytes(raw);
                    if ordinal == u64::MAX {
                        return Err(ExprError::OrdinalOutOfRange(ordinal));
                    }
                    if previous.is_some_and(|p| p >= ordinal) {
                        return Err(ExprError::NonCanonicalLiteral);
                    }
                    ordinals.push(ordinal);
                    previous = Some(ordinal);
                }
                SetExpr::Literal(ordinals)
            }
            TAG_AND | TAG_OR => {
                let n = self.u16()? as usize;
                if n == 0 {
                    return Err(ExprError::EmptyJunction);
                }
                // Checked *before* reserving. `n` is attacker-controlled up to
                // 65535, and `Vec::with_capacity(n)` on an otherwise 3-byte
                // payload is a free allocation for whoever sent it.
                if self.nodes + n > MAX_NODES {
                    return Err(ExprError::TooManyNodes);
                }
                let mut xs = Vec::with_capacity(n);
                for _ in 0..n {
                    xs.push(self.expr(depth + 1)?);
                }
                if tag == TAG_AND {
                    SetExpr::And(xs)
                } else {
                    SetExpr::Or(xs)
                }
            }
            TAG_AND_NOT => {
                let a = self.expr(depth + 1)?;
                let b = self.expr(depth + 1)?;
                SetExpr::AndNot(Box::new(a), Box::new(b))
            }
            TAG_AT => {
                let v = self.vec_expr(depth + 1)?;
                let i = self.u32()?;
                // Arity is statically known, so this is the whole of what makes
                // `At` total: an out-of-range index never reaches evaluation.
                if i >= v.arity() {
                    return Err(ExprError::IndexOutOfRange);
                }
                SetExpr::At(Box::new(v), i)
            }
            TAG_FOLD => {
                let v = self.vec_expr(depth + 1)?;
                let op = match *self.take(1)?.first().expect("took exactly 1") {
                    FOLD_OR => FoldOp::Or,
                    FOLD_AND => FoldOp::And,
                    FOLD_XOR => FoldOp::Xor,
                    other => return Err(ExprError::UnknownFoldOp(other)),
                };
                SetExpr::Fold(Box::new(v), op)
            }
            TAG_PACK => {
                let view = self.view()?;
                let v = self.vec_expr(depth + 1)?;
                if v.arity() != view.sets {
                    return Err(ExprError::ArityMismatch {
                        expected: view.sets,
                        found: v.arity(),
                    });
                }
                SetExpr::Pack(Box::new(v), view)
            }
            TAG_EXPAND => {
                let view = self.view()?;
                let input = self.expr(depth + 1)?;
                SetExpr::Expand(Box::new(input), view)
            }
            TAG_HOLE => {
                if !self.in_map {
                    return Err(ExprError::HoleOutsideMap);
                }
                SetExpr::Hole
            }
            TAG_SELECT => {
                let n = self.u64()?;
                SetExpr::Select(Box::new(self.expr(depth + 1)?), n)
            }
            TAG_MAP_BOOL => {
                let v = self.vec_expr(depth + 1)?;
                let body = self.body(depth + 1, Cursor::bool_expr)?;
                SetExpr::MapBool(Box::new(v), Box::new(body))
            }
            other => return Err(misplaced(Sort::Set, other)),
        })
    }

    /// Decode a `map` body, which is the only place [`SetExpr::Hole`] is legal.
    ///
    /// Refuses a nested body outright and restores the flag afterwards, so a
    /// `map` in a *vector* position -- sequential rather than nested -- still
    /// decodes.
    fn body<T>(
        &mut self,
        depth: usize,
        f: impl FnOnce(&mut Self, usize) -> Result<T, ExprError>,
    ) -> Result<T, ExprError> {
        if self.in_map {
            return Err(ExprError::NestedMap);
        }
        self.in_map = true;
        let out = f(self, depth);
        self.in_map = false;
        out
    }

    fn int_expr(&mut self, depth: usize) -> Result<IntExpr, ExprError> {
        if depth > MAX_DEPTH {
            return Err(ExprError::TooDeep);
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ExprError::TooManyNodes);
        }
        let tag = *self.take(1)?.first().expect("took exactly 1");
        Ok(match tag {
            TAG_INT_LIT => IntExpr::Lit(self.u64()?),
            TAG_CARDINALITY => IntExpr::Cardinality(Box::new(self.expr(depth + 1)?)),
            TAG_RANK => {
                let x = self.u64()?;
                IntExpr::Rank(Box::new(self.expr(depth + 1)?), x)
            }
            TAG_INT_AT => {
                let v = self.vec_int_expr(depth + 1)?;
                let i = self.u32()?;
                if i >= v.arity() {
                    return Err(ExprError::IndexOutOfRange);
                }
                IntExpr::At(Box::new(v), i)
            }
            other => return Err(misplaced(Sort::Int, other)),
        })
    }

    fn bool_expr(&mut self, depth: usize) -> Result<BoolExpr, ExprError> {
        if depth > MAX_DEPTH {
            return Err(ExprError::TooDeep);
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ExprError::TooManyNodes);
        }
        let tag = *self.take(1)?.first().expect("took exactly 1");
        Ok(match tag {
            TAG_CONTAINS => {
                let x = self.u64()?;
                BoolExpr::Contains(Box::new(self.expr(depth + 1)?), x)
            }
            other => return Err(misplaced(Sort::Bool, other)),
        })
    }

    fn vec_int_expr(&mut self, depth: usize) -> Result<VecIntExpr, ExprError> {
        if depth > MAX_DEPTH {
            return Err(ExprError::TooDeep);
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ExprError::TooManyNodes);
        }
        let tag = *self.take(1)?.first().expect("took exactly 1");
        Ok(match tag {
            TAG_INT_LIST => {
                let n = self.u16()? as usize;
                if n == 0 {
                    return Err(ExprError::EmptyVector);
                }
                if self.nodes + n > MAX_NODES {
                    return Err(ExprError::TooManyNodes);
                }
                let mut xs = Vec::with_capacity(n);
                for _ in 0..n {
                    xs.push(self.int_expr(depth + 1)?);
                }
                VecIntExpr::List(xs)
            }
            TAG_MAP_INT => {
                let v = self.vec_expr(depth + 1)?;
                let body = self.body(depth + 1, Cursor::int_expr)?;
                VecIntExpr::Map(Box::new(v), Box::new(body))
            }
            other => return Err(misplaced(Sort::VecInt, other)),
        })
    }

    /// Decode a vector-sorted node.
    ///
    /// The mirror of [`Cursor::expr`], and the reason the two tag ranges share
    /// one space: a set-sorted tag arriving here is reported as a sort
    /// mismatch naming both sides, rather than being reinterpreted as whatever
    /// that byte happens to mean in this position.
    fn vec_expr(&mut self, depth: usize) -> Result<VecSetExpr, ExprError> {
        if depth > MAX_DEPTH {
            return Err(ExprError::TooDeep);
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ExprError::TooManyNodes);
        }
        let tag = *self.take(1)?.first().expect("took exactly 1");
        Ok(match tag {
            TAG_LIST => {
                let n = self.u16()? as usize;
                if n == 0 {
                    return Err(ExprError::EmptyVector);
                }
                // Checked *before* reserving, exactly as the junctions are:
                // `n` is attacker-controlled and a short payload must not buy a
                // large allocation.
                if self.nodes + n > MAX_NODES {
                    return Err(ExprError::TooManyNodes);
                }
                let mut xs = Vec::with_capacity(n);
                for _ in 0..n {
                    xs.push(self.expr(depth + 1)?);
                }
                VecSetExpr::List(xs)
            }
            TAG_VIEW => {
                let view = self.view()?;
                let input = self.expr(depth + 1)?;
                VecSetExpr::View(Box::new(input), view)
            }
            TAG_MAP_SET => {
                let v = self.vec_expr(depth + 1)?;
                let body = self.body(depth + 1, Cursor::expr)?;
                VecSetExpr::Map(Box::new(v), Box::new(body))
            }
            other => return Err(misplaced(Sort::VecSet, other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fold( view( key( k ), spec ), or )` -- the shape the old `ViewFold`
    /// node encoded as a leaf.
    fn fold_of(k: u64, view: ViewSpec) -> SetExpr {
        SetExpr::Fold(
            Box::new(VecSetExpr::View(Box::new(SetExpr::Key(k)), view)),
            FoldOp::Or,
        )
    }

    fn sample() -> SetExpr {
        SetExpr::And(vec![
            SetExpr::Key(42),
            SetExpr::Literal(vec![1, 65_536, u64::MAX - 1]),
            SetExpr::Or(vec![SetExpr::Range(0, 10), SetExpr::Range(100, u64::MAX)]),
            SetExpr::AndNot(Box::new(SetExpr::Key(7)), Box::new(SetExpr::Empty)),
            SetExpr::At(
                Box::new(VecSetExpr::View(
                    Box::new(SetExpr::Key(50)),
                    ViewSpec::interleaved(3),
                )),
                1,
            ),
            // The composition the old `ViewFold` could not express: the folded
            // operand is computed rather than a bare key.
            SetExpr::Fold(
                Box::new(VecSetExpr::View(
                    Box::new(SetExpr::AndNot(
                        Box::new(SetExpr::Key(60)),
                        Box::new(SetExpr::Key(61)),
                    )),
                    ViewSpec::blocked(3, 65_536),
                )),
                FoldOp::Or,
            ),
            SetExpr::Expand(Box::new(SetExpr::Key(70)), ViewSpec::interleaved(2)),
            SetExpr::Pack(
                Box::new(VecSetExpr::List(vec![SetExpr::Key(80), SetExpr::Key(81)])),
                ViewSpec::interleaved(2),
            ),
        ])
    }

    /// The query this whole language exists to make expressible: for each
    /// constituent, how many of its ordinals survive `q`. A facet histogram.
    ///
    /// It is a **map, not a fold** -- it applies a query to each element rather
    /// than combining them -- and its result is one integer per constituent,
    /// which is the row marginal. `fold` gives the column one, and the two do
    /// not determine each other.
    #[test]
    fn the_facet_query_round_trips() {
        let q = SetExpr::Key(7);
        let e = AnyExpr::VecInt(VecIntExpr::Map(
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::Key(9)),
                ViewSpec::interleaved(4),
            )),
            Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
                SetExpr::Hole,
                q,
            ])))),
        ));
        assert_eq!(AnyExpr::decode(&e.encode()), Ok(e.clone()));
        assert_eq!(e.sort(), Sort::VecInt);

        // The vector's arity is the descriptor's, because a map preserves shape.
        let AnyExpr::VecInt(v) = &e else { panic!() };
        assert_eq!(v.arity(), 4);

        // Both operands are reachable: the hole names no key, the view's does.
        let mut ks = Vec::new();
        v.keys(&mut ks);
        ks.sort_unstable();
        assert_eq!(ks, vec![7, 9]);
    }

    /// `_` is legal only inside a map body, and a map body may not contain
    /// another map. Both are decode-time rules because an un-indexed hole
    /// cannot say which element it means.
    #[test]
    fn the_hole_is_scoped_to_a_map_body() {
        // A bare hole is not a query.
        assert_eq!(
            SetExpr::decode(&SetExpr::Hole.encode()),
            Err(ExprError::HoleOutsideMap)
        );
        // Nor is one merely near a map rather than inside its body.
        let outside = SetExpr::And(vec![
            SetExpr::Hole,
            SetExpr::Fold(
                Box::new(VecSetExpr::List(vec![SetExpr::Key(1)])),
                FoldOp::Or,
            ),
        ]);
        assert_eq!(
            SetExpr::decode(&outside.encode()),
            Err(ExprError::HoleOutsideMap)
        );

        let v = || Box::new(VecSetExpr::List(vec![SetExpr::Key(1), SetExpr::Key(2)]));

        // Inside a body it is fine, and every `_` in one body is the same
        // element -- `xor( _, _ )` is `s △ s`, not a binary function, which is
        // the opposite of the glyph's most famous precedent.
        let ok = SetExpr::Fold(
            Box::new(VecSetExpr::Map(
                v(),
                Box::new(SetExpr::xor(SetExpr::Hole, SetExpr::Hole)),
            )),
            FoldOp::Or,
        );
        assert_eq!(SetExpr::decode(&ok.encode()), Ok(ok));

        // A map in a body is refused ..
        let nested = VecSetExpr::Map(
            v(),
            Box::new(SetExpr::Fold(
                Box::new(VecSetExpr::Map(v(), Box::new(SetExpr::Hole))),
                FoldOp::Or,
            )),
        );
        assert_eq!(
            SetExpr::decode(&SetExpr::Fold(Box::new(nested), FoldOp::Or).encode()),
            Err(ExprError::NestedMap)
        );

        // .. but a map in a *vector* position is sequential, not nested, and
        // decodes. Neither body can see the other's element.
        let sequential = VecSetExpr::Map(
            Box::new(VecSetExpr::Map(v(), Box::new(SetExpr::Hole))),
            Box::new(SetExpr::Hole),
        );
        let e = SetExpr::Fold(Box::new(sequential), FoldOp::Or);
        assert_eq!(SetExpr::decode(&e.encode()), Ok(e));
    }

    /// Every defined tag has exactly one sort, and the highest defined tag is
    /// the boundary between "wrong sort" and "unknown".
    ///
    /// This table is the single thing keeping five decoders' fallback arms in
    /// step. Before it existed each arm enumerated the other sorts' tags by
    /// hand, and tag 22 was reported as unknown in a set position while being a
    /// sort mismatch everywhere else.
    #[test]
    fn every_defined_tag_has_exactly_one_sort() {
        let defined = [
            TAG_EMPTY,
            TAG_KEY,
            TAG_RANGE,
            TAG_AND,
            TAG_OR,
            TAG_AND_NOT,
            TAG_AT,
            TAG_FOLD,
            TAG_EXPAND,
            TAG_LITERAL,
            TAG_PACK,
            TAG_LIST,
            TAG_VIEW,
            TAG_HOLE,
            TAG_SELECT,
            TAG_MAP_SET,
            TAG_MAP_INT,
            TAG_MAP_BOOL,
            TAG_CARDINALITY,
            TAG_RANK,
            TAG_INT_LIT,
            TAG_INT_AT,
            TAG_INT_LIST,
            TAG_CONTAINS,
        ];
        // Contiguous from zero, so "defined" is decidable by comparison.
        let mut sorted = defined;
        sorted.sort_unstable();
        for (i, tag) in sorted.iter().enumerate() {
            assert_eq!(*tag as usize, i, "tags must be contiguous from 0");
            assert!(sort_of_tag(*tag).is_some(), "tag {tag} has no sort");
        }
        // And nothing above them is claimed.
        for tag in defined.len() as u8..=u8::MAX {
            assert_eq!(sort_of_tag(tag), None, "tag {tag} should be undefined");
        }
        // A tag of another sort is a mismatch; an undefined one is unknown.
        assert_eq!(
            misplaced(Sort::Set, TAG_INT_LIST),
            ExprError::SortMismatch {
                expected: Sort::Set,
                tag: TAG_INT_LIST
            }
        );
        assert_eq!(misplaced(Sort::Set, 200), ExprError::UnknownTag(200));
    }

    /// The sorts reject each other's tags in both directions, so a payload
    /// cannot be read at the wrong sort by accident.
    #[test]
    fn the_new_sorts_reject_each_others_tags() {
        // An integer vector is not a set.
        let counts = AnyExpr::VecInt(VecIntExpr::List(vec![IntExpr::Lit(1)]));
        assert_eq!(
            SetExpr::decode(&counts.encode()),
            Err(ExprError::SortMismatch {
                expected: Sort::Set,
                tag: TAG_INT_LIST,
            })
        );
        // And a set is not an integer vector: swap the leading tag of a
        // well-formed integer-vector payload for a set-sorted one.
        let mut swapped = counts.encode();
        swapped[HEADER_LEN] = TAG_EMPTY;
        assert!(matches!(
            AnyExpr::decode(&swapped),
            Ok(AnyExpr::Set(SetExpr::Empty)) | Err(_)
        ));
    }

    /// A fixed byte vector, mirrored verbatim in the Python, Go and Java
    /// clients' own tests. Five implementations of one format drift silently
    /// otherwise: each can round-trip against itself while disagreeing with the
    /// others, and only a shared constant catches that.
    #[test]
    fn the_cross_implementation_wire_vector_is_stable() {
        let e = SetExpr::At(
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::Key(9)),
                ViewSpec::interleaved(3),
            )),
            1,
        );
        let hex: String = e.encode().iter().map(|b| format!("{b:02x}")).collect();
        // header(6) TAG_AT TAG_VIEW sets(4) layout stride(8) TAG_KEY key(8) index(4)
        assert_eq!(
            hex,
            "59534e580100060c0300000000000000000000000001090000000000000001000000"
        );
        assert_eq!(e.encode().len(), 34);
        assert_eq!(SetExpr::decode(&e.encode()), Ok(e));
    }

    #[test]
    fn round_trip() {
        let e = sample();
        assert_eq!(SetExpr::decode(&e.encode()).unwrap(), e);
    }

    /// The ambiguity this format was redesigned to remove. A bare key is 8
    /// arbitrary bytes, so one *can* begin with `YSNX` — there are 2^32 such
    /// keys, and keys are commonly hashes. Reading one as an expression would be
    /// a wrong answer, not an error.
    #[test]
    fn a_bare_key_is_never_mistaken_for_an_expression() {
        let colliding = u64::from_le_bytes(*b"YSNX\0\0\0\0");
        assert_eq!(
            &colliding.to_le_bytes()[..4],
            MAGIC,
            "the collision is real"
        );

        for k in [0u64, 1, u64::MAX, colliding] {
            assert!(
                !SetExpr::looks_like_expr(&k.to_le_bytes()),
                "key {k} must not look like an expression"
            );
        }
        assert!(SetExpr::looks_like_expr(&sample().encode()));
    }

    /// The property `RESERVED` exists to guarantee: nothing this encoder can
    /// produce is 8 bytes, so length alone separates the two descriptor forms.
    ///
    /// Exhaustive over the shapes that could plausibly be small, because the
    /// dangerous case is the *smallest* encoding and it is easy to reintroduce
    /// by adding a compact node kind.
    #[test]
    fn no_encoding_is_eight_bytes() {
        let smallest = [
            SetExpr::Empty,
            SetExpr::Key(0),
            SetExpr::Range(0, 0),
            SetExpr::Literal(Vec::new()),
            SetExpr::And(vec![SetExpr::Empty]),
            SetExpr::Or(vec![SetExpr::Empty]),
            SetExpr::AndNot(Box::new(SetExpr::Empty), Box::new(SetExpr::Empty)),
            // The sorted nodes, at their smallest: a one-element list is the
            // shortest vector, so these are the shortest trees reaching each
            // of the four vector-consuming tags.
            SetExpr::At(Box::new(VecSetExpr::List(vec![SetExpr::Empty])), 0),
            SetExpr::Fold(Box::new(VecSetExpr::List(vec![SetExpr::Empty])), FoldOp::Or),
            SetExpr::Pack(
                Box::new(VecSetExpr::List(vec![SetExpr::Empty])),
                ViewSpec::interleaved(1),
            ),
            SetExpr::Expand(Box::new(SetExpr::Empty), ViewSpec::interleaved(1)),
        ];
        for e in smallest {
            let n = e.encode().len();
            assert_ne!(n, BARE_KEY_LEN, "{e:?} encodes to exactly 8 bytes");
        }
    }

    /// The sorts are checked while decoding, and a tag in the wrong position is
    /// named rather than reinterpreted. This is why the two tag ranges share
    /// one space: `TAG_LIST` in a set position must be an error, not whatever
    /// byte 11 would otherwise mean there.
    #[test]
    fn a_node_of_the_wrong_sort_is_refused() {
        // A vector where a set belongs: replace the whole payload with a bare
        // `[ Empty ]`, which is well formed but vector-sorted.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(VERSION);
        bytes.push(RESERVED);
        VecSetExpr::List(vec![SetExpr::Empty]).write(&mut bytes);
        assert_eq!(
            SetExpr::decode(&bytes),
            Err(ExprError::SortMismatch {
                expected: Sort::Set,
                tag: TAG_LIST,
            })
        );

        // A set where a vector belongs: `fold` expects one, so overwrite the
        // vector tag that follows `TAG_FOLD` with a set tag.
        let mut swapped = fold_of(1, ViewSpec::interleaved(2)).encode();
        swapped[7] = TAG_EMPTY;
        assert_eq!(
            SetExpr::decode(&swapped),
            Err(ExprError::SortMismatch {
                expected: Sort::VecSet,
                tag: TAG_EMPTY,
            })
        );
    }

    /// `pack` is the uncurry direction, so the vector it packs must have
    /// exactly the constituents the descriptor describes. Arity is static on
    /// both sides, so this is decided before evaluation.
    #[test]
    fn packing_checks_arity_against_the_descriptor() {
        let two = || VecSetExpr::List(vec![SetExpr::Key(1), SetExpr::Key(2)]);
        let ok = SetExpr::Pack(Box::new(two()), ViewSpec::interleaved(2));
        assert_eq!(SetExpr::decode(&ok.encode()), Ok(ok));

        let bad = SetExpr::Pack(Box::new(two()), ViewSpec::interleaved(3));
        assert_eq!(
            SetExpr::decode(&bad.encode()),
            Err(ExprError::ArityMismatch {
                expected: 3,
                found: 2,
            })
        );

        // A view's arity comes from its descriptor, so this direction agrees.
        let from_view = VecSetExpr::View(Box::new(SetExpr::Key(1)), ViewSpec::blocked(4, 16));
        assert_eq!(from_view.arity(), 4);
        assert_eq!(two().arity(), 2);
    }

    /// An empty vector has no element sort to infer and no identity to assume —
    /// the same reason an empty `And` or `Or` is refused rather than read as
    /// one of the two different identities a client might have meant.
    #[test]
    fn an_empty_vector_is_refused() {
        let empty = SetExpr::Fold(Box::new(VecSetExpr::List(Vec::new())), FoldOp::Or);
        assert_eq!(
            SetExpr::decode(&empty.encode()),
            Err(ExprError::EmptyVector)
        );
    }

    /// The capability the composition buys: the old `ViewFold` took a bare
    /// `u64`, so only a stored key could be folded. This is the shape that was
    /// unreachable at any cost.
    #[test]
    fn a_view_can_be_taken_of_a_computed_set() {
        let e = SetExpr::Fold(
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::And(vec![SetExpr::Key(1), SetExpr::Key(2)])),
                ViewSpec::interleaved(4),
            )),
            FoldOp::Xor,
        );
        assert_eq!(SetExpr::decode(&e.encode()), Ok(e.clone()));
        let mut ks = Vec::new();
        e.keys(&mut ks);
        ks.sort_unstable();
        assert_eq!(ks, vec![1, 2], "both operands must be reachable");
    }

    /// The never-panic contract. Every prefix of a valid encoding is a
    /// truncated payload, and every one must produce an error rather than an
    /// index panic.
    #[test]
    fn every_truncation_errs_rather_than_panicking() {
        let full = sample().encode();
        for n in 0..full.len() {
            assert!(
                SetExpr::decode(&full[..n]).is_err(),
                "prefix of length {n} decoded as valid"
            );
        }
        assert!(SetExpr::decode(&full).is_ok());
    }

    #[test]
    fn corrupting_any_single_byte_never_panics() {
        let full = sample().encode();
        for i in 0..full.len() {
            for bit in 0..8 {
                let mut b = full.clone();
                b[i] ^= 1 << bit;
                // Either an error or a valid expression — never a panic, and
                // never a hang.
                let _ = SetExpr::decode(&b);
            }
        }
    }

    #[test]
    fn depth_is_bounded() {
        // Build a payload nested past the limit by hand, since the encoder
        // would happily produce one.
        let mut deep = SetExpr::Key(1);
        for _ in 0..(MAX_DEPTH + 5) {
            deep = SetExpr::And(vec![deep]);
        }
        assert_eq!(SetExpr::decode(&deep.encode()), Err(ExprError::TooDeep));
    }

    /// The allocation bound. A three-byte junction header claiming 65535
    /// children must be rejected *before* the `Vec` is reserved.
    #[test]
    fn a_lying_child_count_does_not_allocate() {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.push(VERSION);
        b.push(RESERVED);
        b.push(TAG_AND);
        b.extend_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(SetExpr::decode(&b), Err(ExprError::TooManyNodes));
    }

    /// A literal count is also attacker-controlled. It must be checked against
    /// the remaining bytes before reserving the vector it names.
    #[test]
    fn a_lying_literal_count_does_not_allocate() {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.push(VERSION);
        b.push(RESERVED);
        b.push(TAG_LITERAL);
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(SetExpr::decode(&b), Err(ExprError::Truncated));
    }

    #[test]
    fn literals_are_canonical_and_obey_the_ordinal_ceiling() {
        assert_eq!(
            SetExpr::literal([2, 0, 2, 1]).unwrap(),
            SetExpr::Literal(vec![0, 1, 2])
        );
        assert_eq!(
            SetExpr::literal([u64::MAX]),
            Err(ExprError::OrdinalOutOfRange(u64::MAX))
        );
        for ordinals in [vec![2, 1], vec![1, 1]] {
            assert_eq!(
                SetExpr::decode(&SetExpr::Literal(ordinals).encode()),
                Err(ExprError::NonCanonicalLiteral)
            );
        }
        assert_eq!(
            SetExpr::decode(&SetExpr::Literal(vec![u64::MAX]).encode()),
            Err(ExprError::OrdinalOutOfRange(u64::MAX))
        );
        let valid = SetExpr::Literal(vec![0, 1, u64::MAX - 1]);
        assert_eq!(SetExpr::decode(&valid.encode()).unwrap(), valid);
    }

    #[test]
    fn literal_fixed_vector_matches_language_clients() {
        let literal = SetExpr::literal([0, 2, 65_536, u64::MAX - 1]).unwrap();
        let expected = [
            0x59, 0x53, 0x4e, 0x58, 0x01, 0x00, 0x09, // header and literal tag
            0x04, 0x00, 0x00, 0x00, // member count
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 2
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, // 65_536
            0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // u64::MAX - 1
        ];
        assert_eq!(literal.encode(), expected);
        assert_eq!(SetExpr::decode(&expected).unwrap(), literal);
    }

    #[test]
    fn an_empty_junction_is_rejected() {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.push(VERSION);
        b.push(RESERVED);
        b.push(TAG_OR);
        b.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(SetExpr::decode(&b), Err(ExprError::EmptyJunction));
    }

    #[test]
    fn bad_magic_and_version_are_distinguished() {
        assert_eq!(
            SetExpr::decode(b"XXXX\x01\x00\x00"),
            Err(ExprError::BadMagic)
        );
        let mut b = MAGIC.to_vec();
        b.push(VERSION + 1);
        b.push(RESERVED);
        b.push(TAG_EMPTY);
        assert_eq!(
            SetExpr::decode(&b),
            Err(ExprError::UnsupportedVersion(VERSION + 1))
        );
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut b = SetExpr::Key(1).encode();
        b.push(0);
        assert_eq!(SetExpr::decode(&b), Err(ExprError::TrailingBytes));
    }

    /// The walk must reach through the view nodes, which no longer name a key
    /// in a field: `fold` and `at` wrap an arbitrary set expression now, so 61
    /// and the two packed keys are only found by recursing.
    #[test]
    fn keys_are_collected_from_every_branch() {
        let mut ks = Vec::new();
        sample().keys(&mut ks);
        ks.sort_unstable();
        assert_eq!(ks, vec![7, 42, 50, 60, 61, 70, 80, 81]);
    }

    #[test]
    fn view_specs_map_logical_ordinals_without_wrapping() {
        let interleaved = ViewSpec::interleaved(3);
        assert_eq!(interleaved.ordinal_of(2, 5), Some(17));
        assert_eq!(interleaved.ordinal_of(3, 0), None);
        assert_eq!(interleaved.ordinal_of(0, u64::MAX), None);

        let blocked = ViewSpec::blocked(3, 100);
        assert_eq!(blocked.ordinal_of(2, 5), Some(205));
        assert_eq!(blocked.ordinal_of(0, 100), None);
        assert_eq!(ViewSpec::blocked(u32::MAX, u64::MAX).ordinal_of(1, 0), None);
        assert_eq!(ViewSpec::interleaved(0).ordinal_of(0, 0), None);
        assert_eq!(ViewSpec::blocked(1, 0).ordinal_of(0, 0), None);
    }

    /// **The amplification bound.** `sets` does not appear in the node count or
    /// the depth, so neither of the other two caps sees it: a descriptor is a
    /// fixed 13 bytes whether it declares 8 constituents or four billion, while
    /// the server loops over every one of them. `MAX_VIEW_SETS` is the third
    /// bound, and it is enforced during `decode` so an oversized request is
    /// refused before anything evaluates it.
    #[test]
    fn a_view_with_too_many_constituents_is_refused_at_decode() {
        for view in [
            ViewSpec::interleaved(u32::MAX),
            ViewSpec::blocked(MAX_VIEW_SETS + 1, 1),
        ] {
            let bytes = fold_of(1, view).encode();
            // The whole point: the payload is tiny and the work it asks for is
            // not, so the refusal cannot be left to a size limit.
            assert!(bytes.len() < 64, "a {}-byte request", bytes.len());
            assert_eq!(
                SetExpr::decode(&bytes),
                Err(ExprError::ViewHasTooManyConstituents),
                "{view:?}"
            );
        }

        // The boundary itself is legal -- the cap rejects above it, not at it.
        let ok = fold_of(1, ViewSpec::interleaved(MAX_VIEW_SETS));
        assert_eq!(SetExpr::decode(&ok.encode()), Ok(ok));
    }

    #[test]
    fn malformed_view_descriptors_are_rejected() {
        assert_eq!(
            SetExpr::decode(&fold_of(1, ViewSpec::interleaved(0)).encode()),
            Err(ExprError::ViewHasNoConstituents)
        );
        assert_eq!(
            SetExpr::decode(&fold_of(1, ViewSpec::blocked(2, 0)).encode()),
            Err(ExprError::ViewHasZeroStride)
        );

        // An index at the arity is out of range; arity comes from the shape.
        let bad_index = SetExpr::At(
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::Key(1)),
                ViewSpec::interleaved(2),
            )),
            2,
        );
        assert_eq!(
            SetExpr::decode(&bad_index.encode()),
            Err(ExprError::IndexOutOfRange)
        );

        let mut noncanonical = fold_of(1, ViewSpec::interleaved(2)).encode();
        // `fold( view( key( 1 ), spec ), or )` lays out as
        //   header(6) TAG_FOLD(1) TAG_VIEW(1) sets(4) layout(1) stride(8)
        //   TAG_KEY(1) key(8) foldop(1)
        // so the layout byte is at 12 and the reserved stride at 13..21.
        const LAYOUT_AT: usize = 12;
        const STRIDE_AT: usize = 13;
        assert_eq!(
            noncanonical.len(),
            31,
            "the offsets below assume this shape"
        );
        noncanonical[STRIDE_AT] = 1;
        assert_eq!(
            SetExpr::decode(&noncanonical),
            Err(ExprError::NonCanonicalView)
        );

        let mut unknown_layout = noncanonical;
        unknown_layout[LAYOUT_AT] = 99;
        assert_eq!(
            SetExpr::decode(&unknown_layout),
            Err(ExprError::UnknownViewLayout(99))
        );

        // The fold operator is the trailing byte.
        let mut unknown_op = fold_of(1, ViewSpec::interleaved(2)).encode();
        *unknown_op.last_mut().unwrap() = 99;
        assert_eq!(
            SetExpr::decode(&unknown_op),
            Err(ExprError::UnknownFoldOp(99))
        );
    }

    #[test]
    fn derived_boolean_operators_use_only_v1_nodes_and_round_trip() {
        let xor = SetExpr::xor(SetExpr::Key(1), SetExpr::Key(2));
        let complement = SetExpr::complement(SetExpr::Key(3));

        assert_eq!(SetExpr::decode(&xor.encode()).unwrap(), xor);
        assert_eq!(SetExpr::decode(&complement.encode()).unwrap(), complement);

        let mut keys = Vec::new();
        xor.keys(&mut keys);
        keys.sort_unstable();
        assert_eq!(keys, vec![1, 1, 2, 2]);

        assert_eq!(
            complement,
            SetExpr::AndNot(
                Box::new(SetExpr::Range(0, u64::MAX)),
                Box::new(SetExpr::Key(3))
            )
        );
    }
    #[test]
    fn query_requests_round_trip_current_and_pinned_versions() {
        let expression = sample();
        for request in [
            QueryRequest::current(expression.clone()),
            QueryRequest::at(expression.clone(), 42),
            QueryRequest::at(expression.clone(), 0),
        ] {
            let bytes = request.encode();
            assert!(QueryRequest::looks_like_request(&bytes));
            assert!(!SetExpr::looks_like_expr(&bytes));
            assert_eq!(QueryRequest::decode(&bytes).unwrap(), request);
        }
    }

    #[test]
    fn malformed_query_requests_are_rejected() {
        let full = QueryRequest::at(sample(), 7).encode();
        for n in 0..full.len() {
            assert!(QueryRequest::decode(&full[..n]).is_err(), "prefix {n}");
        }

        let mut unknown_flags = full.clone();
        unknown_flags[5] = 0x80;
        assert!(QueryRequest::decode(&unknown_flags).is_err());

        let mut unflagged_version = QueryRequest::current(sample()).encode();
        unflagged_version[6] = 1;
        assert_eq!(
            QueryRequest::decode(&unflagged_version),
            Err(ExprError::TrailingBytes)
        );
    }

    #[test]
    fn a_bare_key_cannot_look_like_a_query_request() {
        let colliding = u64::from_le_bytes(*b"YSNQ\0\0\0\0");
        assert!(!QueryRequest::looks_like_request(&colliding.to_le_bytes()));
    }
}
