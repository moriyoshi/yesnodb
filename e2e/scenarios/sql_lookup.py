# SQL over a real database, through DataFusion's `yesno_lookup`.
#
# # What this covers that nothing did
#
# Until `SnapshotSource` landed, `PostingSource` had exactly one implementation —
# `MapSource`, "for tests and small fixtures" — so `yesno_lookup` could be
# pointed at a `HashMap` filled by hand and at nothing else. The trait comment
# said "a caller can back the function with a snapshot"; nothing did.
#
# So the sequence here is the one an operator performs and no Rust test made:
# write to a database, **checkpoint and reopen**, then query it in SQL. The
# reopen matters for the same reason it does in `arrow_surface.py` — a read on a
# live database is answered from the memtable and never reaches the store.
#
# The term encoder is a hash and is deliberately not injective, so a scenario
# cannot guess which key a term reads. `df_key` asks the shipped encoder. Writing
# under a key chosen any other way would make every assertion below vacuous.
#
#   yesno-e2e --show-output --arg n=5000 sql_lookup.py

N = yn_arg("n", 600)

db = db_open("sql", shards=2)

rust = set(i * 3 for i in range(N))
db_slow = set(i * 5 for i in range(N))          # overlaps `rust` on multiples of 15
empty_term = "nobody-indexed-this"

db_insert_many(db, df_key("rust"), sorted(rust))
db_insert_many(db, df_key("slow"), sorted(db_slow))

# ---- through the store, not the memtable
assert db_checkpoint(db) > 0
db_close(db)
db = db_open("sql", shards=2)
s = db_snapshot(db)

# ---- one term
got = df_sql(s, "SELECT ordinal FROM yesno_lookup('rust') ORDER BY ordinal")
assert got == sorted(rust), "the posting list SQL returned is not the one stored"

# ---- the aggregate path, which never materializes the ordinals in Python
n = df_sql(s, "SELECT CAST(count(*) AS BIGINT UNSIGNED) FROM yesno_lookup('rust')")
assert n == [len(rust)], n

# ---- two terms in one statement
#
# This is what the single held snapshot buys: both lookups answer from the
# same instant, so the intersection is a set the database actually held. With a
# source rebuilt per lookup a concurrent write could land between them and the
# result would be a blend of two instants that nothing downstream could detect.
both = df_sql(
    s,
    "SELECT a.ordinal FROM yesno_lookup('rust') a "
    "JOIN yesno_lookup('slow') b ON a.ordinal = b.ordinal "
    "ORDER BY a.ordinal",
)
assert both == sorted(rust & db_slow), "the join did not agree with the set oracle"
assert len(both) > 0, "the corpus must actually overlap or this proves nothing"

either = df_sql(
    s,
    "SELECT ordinal FROM yesno_lookup('rust') "
    "UNION SELECT ordinal FROM yesno_lookup('slow') ORDER BY ordinal",
)
assert either == sorted(rust | db_slow)

only_rust = df_sql(
    s,
    "SELECT ordinal FROM yesno_lookup('rust') "
    "EXCEPT SELECT ordinal FROM yesno_lookup('slow') ORDER BY ordinal",
)
assert only_rust == sorted(rust - db_slow)

# ---- a term nobody indexed is an empty table, not an error
#
# A query for something absent is a legitimate query. Raising here would make
# every application wrap every lookup in a try.
assert df_sql(s, "SELECT ordinal FROM yesno_lookup('" + empty_term + "')") == []
assert df_sql(
    s,
    "SELECT CAST(count(*) AS BIGINT UNSIGNED) FROM yesno_lookup('" + empty_term + "')",
) == [0]

# ---- SQL's own filters compose with the posting list
half = df_sql(
    s,
    "SELECT ordinal FROM yesno_lookup('rust') WHERE ordinal < 300 ORDER BY ordinal",
)
assert half == sorted(o for o in rust if o < 300)

snap_release(s)

# ---- a write after the snapshot is invisible to it, and visible to the next
#
# The point is that `SnapshotSource` holds an *instant*. A source that
# re-read the database per lookup would show the new ordinal here and the
# scenario would have no way to tell.
db_insert_many(db, df_key("rust"), [999999])
s2 = db_snapshot(db)
assert df_sql(s2, "SELECT ordinal FROM yesno_lookup('rust') WHERE ordinal = 999999") == [
    999999
]
snap_release(s2)

db_close(db)
