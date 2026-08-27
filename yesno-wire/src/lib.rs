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
//! never panics. Two bounds make that true regardless of what arrives:
//!
//! - **Depth** is capped, because decoding is recursive and a deeply nested
//!   payload would otherwise overflow the stack — which is not a catchable
//!   error, so it must be prevented rather than handled.
//! - **Node count** is capped, because a small payload can otherwise describe an
//!   enormous tree and turn a request into an allocation attack.
//!
//! Do not remove either bound on the grounds that only `yesno-pg` sends these.
//! The server accepts them from any client on the network.

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

const TAG_EMPTY: u8 = 0;
const TAG_KEY: u8 = 1;
const TAG_RANGE: u8 = 2;
const TAG_AND: u8 = 3;
const TAG_OR: u8 = 4;
const TAG_AND_NOT: u8 = 5;
const TAG_VIEW_SELECT: u8 = 6;
const TAG_VIEW_FOLD: u8 = 7;
const TAG_VIEW_EXPAND: u8 = 8;
const TAG_LITERAL: u8 = 9;

const VIEW_INTERLEAVED: u8 = 0;
const VIEW_BLOCKED: u8 = 1;
const REDUCE_ANY: u8 = 0;
const REDUCE_ALL: u8 = 1;
const REDUCE_PARITY: u8 = 2;

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
    /// useful view. Large descriptors remain valid; [`ViewSpec::ordinal_of`]
    /// reports individual slots that overflow the ordinal universe.
    pub fn check(self) -> Result<(), ExprError> {
        if self.sets == 0 {
            return Err(ExprError::ViewHasNoConstituents);
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

/// Reduction across every constituent of a packed view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewReduce {
    Any,
    All,
    Parity,
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
    /// Extract one logical constituent from a packed key.
    ViewSelect {
        key: u64,
        view: ViewSpec,
        set: u32,
    },
    /// Reduce all constituents of a packed key into one logical set.
    ViewFold {
        key: u64,
        view: ViewSpec,
        reduce: ViewReduce,
    },
    /// Map every logical ordinal in `input` to every constituent's physical
    /// slot under `view`.
    ViewExpand {
        input: Box<SetExpr>,
        view: ViewSpec,
    },
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
    ViewHasZeroStride,
    ViewConstituentOutOfRange,
    OrdinalOutOfRange(u64),
    NonCanonicalLiteral,
    NonCanonicalView,
    UnknownViewLayout(u8),
    UnknownViewReduce(u8),
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
            ExprError::ViewHasZeroStride => write!(f, "blocked view stride is zero"),
            ExprError::ViewConstituentOutOfRange => {
                write!(f, "view constituent is outside the descriptor")
            }
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
            ExprError::UnknownViewReduce(v) => write!(f, "unknown view reduction {v}"),
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
            SetExpr::ViewSelect { key, view, set } => {
                out.push(TAG_VIEW_SELECT);
                out.extend_from_slice(&key.to_le_bytes());
                view.write(out);
                out.extend_from_slice(&set.to_le_bytes());
            }
            SetExpr::ViewFold { key, view, reduce } => {
                out.push(TAG_VIEW_FOLD);
                out.extend_from_slice(&key.to_le_bytes());
                view.write(out);
                out.push(match reduce {
                    ViewReduce::Any => REDUCE_ANY,
                    ViewReduce::All => REDUCE_ALL,
                    ViewReduce::Parity => REDUCE_PARITY,
                });
            }
            SetExpr::ViewExpand { input, view } => {
                out.push(TAG_VIEW_EXPAND);
                view.write(out);
                input.write(out);
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
            SetExpr::Key(k)
            | SetExpr::ViewSelect { key: k, .. }
            | SetExpr::ViewFold { key: k, .. } => out.push(*k),
            SetExpr::And(xs) | SetExpr::Or(xs) => xs.iter().for_each(|x| x.keys(out)),
            SetExpr::AndNot(a, b) => {
                a.keys(out);
                b.keys(out);
            }
            SetExpr::ViewExpand { input, .. } => input.keys(out),
        }
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
            TAG_VIEW_SELECT => {
                let key = self.u64()?;
                let view = self.view()?;
                let set = self.u32()?;
                if set >= view.sets {
                    return Err(ExprError::ViewConstituentOutOfRange);
                }
                SetExpr::ViewSelect { key, view, set }
            }
            TAG_VIEW_FOLD => {
                let key = self.u64()?;
                let view = self.view()?;
                let reduce = match *self.take(1)?.first().expect("took exactly 1") {
                    REDUCE_ANY => ViewReduce::Any,
                    REDUCE_ALL => ViewReduce::All,
                    REDUCE_PARITY => ViewReduce::Parity,
                    other => return Err(ExprError::UnknownViewReduce(other)),
                };
                SetExpr::ViewFold { key, view, reduce }
            }
            TAG_VIEW_EXPAND => {
                let view = self.view()?;
                let input = self.expr(depth + 1)?;
                SetExpr::ViewExpand {
                    input: Box::new(input),
                    view,
                }
            }
            other => return Err(ExprError::UnknownTag(other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SetExpr {
        SetExpr::And(vec![
            SetExpr::Key(42),
            SetExpr::Literal(vec![1, 65_536, u64::MAX - 1]),
            SetExpr::Or(vec![SetExpr::Range(0, 10), SetExpr::Range(100, u64::MAX)]),
            SetExpr::AndNot(Box::new(SetExpr::Key(7)), Box::new(SetExpr::Empty)),
            SetExpr::ViewSelect {
                key: 50,
                view: ViewSpec::interleaved(3),
                set: 1,
            },
            SetExpr::ViewFold {
                key: 60,
                view: ViewSpec::blocked(3, 65_536),
                reduce: ViewReduce::Any,
            },
            SetExpr::ViewExpand {
                input: Box::new(SetExpr::Key(70)),
                view: ViewSpec::interleaved(2),
            },
        ])
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
        ];
        for e in smallest {
            let n = e.encode().len();
            assert_ne!(n, BARE_KEY_LEN, "{e:?} encodes to exactly 8 bytes");
        }
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

    #[test]
    fn keys_are_collected_from_every_branch() {
        let mut ks = Vec::new();
        sample().keys(&mut ks);
        ks.sort_unstable();
        assert_eq!(ks, vec![7, 42, 50, 60, 70]);
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

    #[test]
    fn malformed_view_descriptors_are_rejected() {
        let no_sets = SetExpr::ViewSelect {
            key: 1,
            view: ViewSpec::interleaved(0),
            set: 0,
        };
        assert_eq!(
            SetExpr::decode(&no_sets.encode()),
            Err(ExprError::ViewHasNoConstituents)
        );

        let zero_stride = SetExpr::ViewFold {
            key: 1,
            view: ViewSpec::blocked(2, 0),
            reduce: ViewReduce::All,
        };
        assert_eq!(
            SetExpr::decode(&zero_stride.encode()),
            Err(ExprError::ViewHasZeroStride)
        );

        let bad_set = SetExpr::ViewSelect {
            key: 1,
            view: ViewSpec::interleaved(2),
            set: 2,
        };
        assert_eq!(
            SetExpr::decode(&bad_set.encode()),
            Err(ExprError::ViewConstituentOutOfRange)
        );

        let mut noncanonical = SetExpr::ViewSelect {
            key: 1,
            view: ViewSpec::interleaved(2),
            set: 0,
        }
        .encode();
        // Header + tag + key + sets + layout precede the reserved stride.
        noncanonical[20] = 1;
        assert_eq!(
            SetExpr::decode(&noncanonical),
            Err(ExprError::NonCanonicalView)
        );

        let mut unknown_layout = noncanonical;
        unknown_layout[19] = 99;
        assert_eq!(
            SetExpr::decode(&unknown_layout),
            Err(ExprError::UnknownViewLayout(99))
        );

        let mut unknown_reduce = SetExpr::ViewFold {
            key: 1,
            view: ViewSpec::interleaved(2),
            reduce: ViewReduce::Parity,
        }
        .encode();
        *unknown_reduce.last_mut().unwrap() = 99;
        assert_eq!(
            SetExpr::decode(&unknown_reduce),
            Err(ExprError::UnknownViewReduce(99))
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
