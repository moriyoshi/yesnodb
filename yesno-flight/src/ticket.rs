//! The ticket format, fixed now so it stays forward-compatible.
//!
//! v1 returns exactly one endpoint, so a ticket could have been a bare key.
//! It is not, because the field that makes multi-endpoint parallel fetch
//! *consistent* is the snapshot version, and adding it later would be a wire
//! break for every client already issuing tickets.
//!
//! With `{version, key, prefix range}` in hand, a coordinator can hand N
//! endpoints — potentially N read-only followers — disjoint prefix ranges of the
//! *same* snapshot, and the union is exactly the answer. Without the version
//! each endpoint would answer from whatever it could see, and the union would be
//! a set that never existed at any instant.

/// Length of a ticket's **fixed header**: six little-endian `u64`s, in the order the
/// struct declares them.
///
/// This was the whole ticket, and its doc read "Fixed, so a short or long one
/// is rejected outright." That is no longer true and the sentence is replaced
/// rather than corrected beside: a ticket is now the header followed by an
/// **optional** encoded [`crate::AnyExpr`], so a longer one is a pushed-down
/// filter rather than corruption. A shorter one is still rejected outright.
///
/// It was 40 bytes until the set representation was added on 2026-09-30. **There is
/// no published release, so the field went in the header rather than behind a
/// magic-marked extension block**: a block is what one writes to leave an already
/// shipped 40-byte layout byte-identical, and nothing here needs that. Widening the
/// header keeps the expression the unambiguous tail, which is the property that
/// actually matters -- the remainder can be handed to `AnyExpr::decode` whole, with
/// no length prefix and no sniffing.
pub const TICKET_HEADER_LEN: usize = 48;

/// How a **set** result is encoded in the `DoGet` stream.
///
/// Only sets have a choice: a vector or a scalar answers one small batch whose
/// shape no representation question applies to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SetWire {
    /// One `u64` per ordinal, `ordinals_schema`. The default, because a consumer
    /// that has not asked for containers may have no decoder for them.
    #[default]
    Ordinals,
    /// Container payloads, `containers_schema` -- byte-identical to what the page
    /// store and a `.roaring` file hold.
    ///
    /// **Not merely smaller: adaptive.** Roaring has already chosen per chunk, so
    /// an array costs `2n` bytes, a bitmap 8192 and a run `4 * intervals`, and the
    /// stream is never worse than either fixed choice. A dense chunk is 8 KiB here
    /// against 512 KiB of ordinals -- 64x -- while a chunk holding a hundred
    /// ordinals is 200 bytes against 800.
    ///
    /// The cost is that the consumer must be able to decode a container payload, so
    /// this is **opt-in and never inferred from density**: a server that switched
    /// representation because the data got denser would hand container bytes to a
    /// client that cannot read them.
    Containers,
    /// A **wholly materialized bitvector** over the ticket's prefix window:
    /// `bitvector_schema`, one bit per ordinal position, gaps included.
    ///
    /// Carried as `Binary` -- one value per chunk -- rather than as a boolean column. A
    /// boolean column's validity bitmap is one bit per **row**, and a row there is an
    /// ordinal, so arrow-rs doubled the wire: 65 536 rows serialized to 16 712 bytes for
    /// 8 192 bytes of payload, and identically whether the field was declared nullable or
    /// not ( measured 2026-10-01 ). With a chunk per value, validity costs one bit per
    /// 65 536 positions and the wire is the payload again.
    ///
    /// The simplest thing a consumer can receive -- a boolean column usable as an
    /// Arrow selection mask with no roaring decoder, no chunk reassembly and no
    /// nested types. Array and run chunks are expanded to bits, and chunks the set
    /// does not touch are emitted as all-zero, so row `n` of the whole stream is
    /// ordinal `( prefix_lo << 16 ) + n` by arithmetic alone.
    ///
    /// **Its size is a function of the prefix window, not of the cardinality**, which
    /// is what makes it the one representation a server must refuse rather than
    /// merely discourage: a default whole-key ticket spans `2^48` chunks, which is
    /// two pebibytes of bits to describe a set that may hold three ordinals. The
    /// window has to be narrowed by the caller -- which is what a coordinator
    /// handing out prefix ranges is already doing.
    Bitvector,
}

impl SetWire {
    fn from_u64(v: u64) -> Option<SetWire> {
        match v {
            0 => Some(SetWire::Ordinals),
            1 => Some(SetWire::Containers),
            2 => Some(SetWire::Bitvector),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    /// The snapshot this result is pinned to. **The consistency field.**
    pub version: u64,
    pub key: u64,
    /// Half-open prefix range this endpoint is responsible for.
    pub prefix_lo: u64,
    pub prefix_hi: u64,
    /// Identifies the expression that produced this plan. Opaque to the server;
    /// reserved so a cached or federated plan can be recognized later.
    pub expr_hash: u64,
    /// The pushed-down filter, when there is one.
    ///
    /// `key` above is still meaningful when this is `Some`: it names the
    /// primary posting list, so a coordinator can route without decoding the
    /// expression. The expression is the authority on *what to return*.
    pub expr: Option<crate::AnyExpr>,
    /// How a set result is encoded. Carried in the extension block, absent from
    /// every v1 ticket, and [`SetWire::Ordinals`] when absent.
    pub wire: SetWire,
}

impl Ticket {
    pub fn whole_key(version: u64, key: u64) -> Self {
        Ticket {
            version,
            key,
            prefix_lo: 0,
            prefix_hi: 1 << 48,
            expr_hash: 0,
            expr: None,
            wire: SetWire::Ordinals,
        }
    }

    /// The same ticket, answering in `wire`.
    pub fn with_wire(self, wire: SetWire) -> Self {
        Ticket { wire, ..self }
    }

    /// A ticket carrying a pushed-down filter.
    pub fn with_expr(version: u64, key: u64, expr: crate::AnyExpr) -> Self {
        Ticket {
            expr: Some(expr),
            ..Ticket::whole_key(version, key)
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(TICKET_HEADER_LEN);
        for v in [
            self.version,
            self.key,
            self.prefix_lo,
            self.prefix_hi,
            self.expr_hash,
            self.wire as u64,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        if let Some(e) = &self.expr {
            b.extend_from_slice(&e.encode());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < TICKET_HEADER_LEN {
            return None;
        }
        let g = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        // Not `decode(..).ok()`. A trailing payload that fails to parse means
        // the client asked for a filter this server cannot apply; answering
        // without it would return a **superset** — more rows than the query
        // asked for, silently. An unparseable expression must reject the ticket.
        // An unrecognised representation is an **error**, never a fallback to the
        // default: answering in a representation the caller did not ask for is a
        // corrupt stream to it, and indistinguishable from a server that understood.
        let wire = SetWire::from_u64(g(5))?;
        let expr = match &b[TICKET_HEADER_LEN..] {
            [] => None,
            rest => Some(crate::AnyExpr::decode(rest).ok()?),
        };
        let t = Ticket {
            version: g(0),
            key: g(1),
            prefix_lo: g(2),
            prefix_hi: g(3),
            expr_hash: g(4),
            expr,
            wire,
        };
        // An inverted range would silently return nothing, which is worse than
        // an error: the caller cannot tell it from a genuinely empty key.
        if t.prefix_lo > t.prefix_hi {
            return None;
        }
        Some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_round_trips() {
        let t = Ticket {
            version: 9,
            key: 42,
            prefix_lo: 1,
            prefix_hi: 1 << 20,
            expr_hash: 7,
            expr: None,
            wire: SetWire::Ordinals,
        };
        assert_eq!(Ticket::decode(&t.encode()), Some(t.clone()));

        // A bare header is exactly `TICKET_HEADER_LEN` bytes and decodes to a
        // ticket with no expression. It was 40 bytes until the representation
        // field widened it to 48 on 2026-09-30; the sentence that said so is
        // replaced rather than corrected beside it, because a comment naming
        // the old width reads as a promise the format no longer makes.
        assert_eq!(t.encode().len(), TICKET_HEADER_LEN);
    }

    /// A ticket carrying a pushed-down filter round-trips, and is longer than
    /// the header — which is what the old `TICKET_LEN` equality check would
    /// have rejected.
    #[test]
    fn a_ticket_can_carry_an_expression() {
        let e = crate::AnyExpr::Set(crate::SetExpr::And(vec![
            crate::SetExpr::Key(42),
            crate::SetExpr::Range(0, 10),
        ]));
        let t = Ticket::with_expr(3, 42, e.clone());
        let bytes = t.encode();
        assert!(bytes.len() > TICKET_HEADER_LEN);
        assert_eq!(Ticket::decode(&bytes), Some(t));
    }

    /// The representation round-trips inside the fixed header.
    #[test]
    fn a_ticket_can_ask_for_container_payloads() {
        let plain = Ticket::whole_key(3, 42);
        assert_eq!(plain.wire, SetWire::Ordinals);
        assert_eq!(plain.encode().len(), TICKET_HEADER_LEN);

        let dense = plain.clone().with_wire(SetWire::Containers);
        let bytes = dense.encode();
        assert_eq!(
            bytes.len(),
            TICKET_HEADER_LEN,
            "the representation lives in the header, so it costs no extra bytes"
        );
        assert_eq!(Ticket::decode(&bytes), Some(dense));
    }

    /// A fixed byte vector for the **header**, mirrored verbatim in the Python,
    /// Go and Java clients' own tests, in the same discipline as
    /// `the_cross_implementation_wire_vector_is_stable` in `yesno-wire`.
    ///
    /// This test exists because the header widened from 40 to 48 bytes on
    /// 2026-09-30 and the three satellite clients were not widened with it. Each
    /// kept round-tripping against its own 40-byte constant -- so every client
    /// test stayed green -- while every real ticket from the server now had eight
    /// bytes they fed to an expression decoder. The Go and Python integration
    /// suites caught it against a live daemon; Java has no live daemon in its
    /// gate and could not. A shared constant is what catches it in all four
    /// without one.
    ///
    /// Both representations are pinned, so the field's **position** is checked and
    /// not merely its presence: a vector for `Ordinals` alone would pass if the
    /// field moved, because its bytes are zero.
    #[test]
    fn the_cross_implementation_ticket_header_is_stable() {
        let t = Ticket {
            version: 9,
            key: 42,
            prefix_lo: 1,
            prefix_hi: 1 << 20,
            expr_hash: 7,
            expr: None,
            wire: SetWire::Ordinals,
        };
        let hex =
            |t: &Ticket| -> String { t.encode().iter().map(|b| format!("{b:02x}")).collect() };
        // version(8) key(8) prefix_lo(8) prefix_hi(8) expr_hash(8) wire(8)
        assert_eq!(
            hex(&t),
            "09000000000000002a000000000000000100000000000000000010000000000007000000000000000000000000000000"
        );
        assert_eq!(
            hex(&t.clone().with_wire(SetWire::Containers)),
            "09000000000000002a000000000000000100000000000000000010000000000007000000000000000100000000000000"
        );
        assert_eq!(t.encode().len(), TICKET_HEADER_LEN);
        assert_eq!(Ticket::decode(&t.encode()), Some(t));
    }

    /// The representation and a pushed-down filter coexist.
    ///
    /// The header carries the representation and the expression is the tail, so the
    /// remainder can go to `AnyExpr::decode` whole -- no length prefix, no sniffing.
    #[test]
    fn a_ticket_carries_both_a_representation_and_a_filter() {
        let e = crate::AnyExpr::Set(crate::SetExpr::Key(42));
        let t = Ticket::with_expr(3, 42, e.clone()).with_wire(SetWire::Containers);
        let got = Ticket::decode(&t.encode()).expect("both halves must decode");
        assert_eq!(got.wire, SetWire::Containers);
        assert_eq!(got.expr, Some(e));
    }

    /// An unrecognised representation rejects the ticket rather than defaulting.
    ///
    /// Defaulting would answer in a representation the caller did not ask for, and
    /// would be indistinguishable to it from a server that understood the request.
    #[test]
    fn an_unknown_representation_is_an_error_not_a_fallback() {
        let mut bytes = Ticket::whole_key(1, 1).encode();
        bytes[40..48].copy_from_slice(&200u64.to_le_bytes());
        assert_eq!(Ticket::decode(&bytes), None);

        // And the two it does know still decode, so the check is not vacuous.
        for (raw, want) in [(0u64, SetWire::Ordinals), (1, SetWire::Containers)] {
            let mut ok = Ticket::whole_key(1, 1).encode();
            ok[40..48].copy_from_slice(&raw.to_le_bytes());
            assert_eq!(Ticket::decode(&ok).map(|t| t.wire), Some(want));
        }
    }

    /// A trailing payload that will not parse must reject the whole ticket.
    /// Answering without the filter would return a **superset** — more rows than
    /// the query asked for — and nothing downstream would notice.
    #[test]
    fn an_unparseable_expression_rejects_the_ticket_rather_than_ignoring_it() {
        let mut bytes = Ticket::whole_key(1, 1).encode();
        bytes.extend_from_slice(b"not an expression");
        assert_eq!(Ticket::decode(&bytes), None);
    }

    #[test]
    fn a_malformed_ticket_is_rejected_not_guessed() {
        assert_eq!(Ticket::decode(&[]), None);
        assert_eq!(Ticket::decode(&[0u8; TICKET_HEADER_LEN - 1]), None);
        // One byte past the header is no longer "too long" — it is a
        // truncated expression, and must fail as one rather than as a length
        // check.
        assert_eq!(Ticket::decode(&[0u8; TICKET_HEADER_LEN + 1]), None);

        let mut bad = Ticket::whole_key(1, 1);
        bad.prefix_lo = 10;
        bad.prefix_hi = 5;
        assert_eq!(
            Ticket::decode(&bad.encode()),
            None,
            "an inverted range must error, not return an empty result"
        );
    }
}
