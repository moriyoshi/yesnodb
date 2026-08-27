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

/// Length of a ticket's **fixed header**.
///
/// This was the whole ticket, and its doc read "Fixed, so a short or long one
/// is rejected outright." That is no longer true and the sentence is replaced
/// rather than corrected beside: a ticket is now the header followed by an
/// **optional** encoded [`crate::SetExpr`], so a longer one is a pushed-down
/// filter rather than corruption. A bare 40-byte ticket still decodes exactly as
/// before, which is what keeps existing clients working.
///
/// A shorter one is still rejected outright.
pub const TICKET_HEADER_LEN: usize = 40;

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
    pub expr: Option<crate::SetExpr>,
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
        }
    }

    /// A ticket carrying a pushed-down filter.
    pub fn with_expr(version: u64, key: u64, expr: crate::SetExpr) -> Self {
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
        let expr = match &b[TICKET_HEADER_LEN..] {
            [] => None,
            rest => Some(crate::SetExpr::decode(rest).ok()?),
        };
        let t = Ticket {
            version: g(0),
            key: g(1),
            prefix_lo: g(2),
            prefix_hi: g(3),
            expr_hash: g(4),
            expr,
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
        };
        assert_eq!(Ticket::decode(&t.encode()), Some(t.clone()));

        // The compatibility property: a bare header is still exactly the old
        // 40 bytes and decodes to a ticket with no expression.
        assert_eq!(t.encode().len(), TICKET_HEADER_LEN);
    }

    /// A ticket carrying a pushed-down filter round-trips, and is longer than
    /// the header — which is what the old `TICKET_LEN` equality check would
    /// have rejected.
    #[test]
    fn a_ticket_can_carry_an_expression() {
        let e = crate::SetExpr::And(vec![crate::SetExpr::Key(42), crate::SetExpr::Range(0, 10)]);
        let t = Ticket::with_expr(3, 42, e.clone());
        let bytes = t.encode();
        assert!(bytes.len() > TICKET_HEADER_LEN);
        assert_eq!(Ticket::decode(&bytes), Some(t));
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
