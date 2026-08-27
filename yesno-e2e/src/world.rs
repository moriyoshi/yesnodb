//! The scenario world: every handle a script can hold, every verb it can call.
//!
//! Scenarios are Python, so the verb surface is deliberately flat free
//! functions over integer handles rather than anything object-shaped — monty
//! has no user classes, and a handle table gives the harness something a
//! Python object would not: the ability to **invalidate**. A closed database
//! handle raises on next use instead of quietly resurrecting a stale `Db`.
//!
//! Three naming rules, each of which exists because breaking it produced a
//! scenario that could not fail:
//!
//! 1. **No verb may collide with a Python builtin.** `open`, `min`, `max` and
//!    `len` are all natural names here and all wrong: monty resolves builtins
//!    before asking the host, so `min(snap, 7)` would silently return the
//!    smaller of two handles instead of the key's minimum ordinal, and
//!    `open("db")` would become a sandboxed filesystem call. Hence the
//!    `db_` / `snap_` / `q_` prefixes, which also keep the surface readable.
//! 2. **An unknown verb is a `NameError`, never a `None`.** A typo'd verb that
//!    returned nothing would let a scenario "pass" having done nothing.
//! 3. **An unknown keyword argument is an error**, for the same reason —
//!    `db_open("x", shard=4)` must not silently open the default 8.
//!
//! A note on what the `q_*` verbs do and do not cover: yesno's lazy leaves are
//! `OrdSet` streams, so `q_key` materializes the key through `Snapshot::load`
//! and the expression tree above it is genuinely lazy. These verbs exercise
//! the stream operators and the cardinality identities; they are not a test of
//! a disk-lazy leaf, because the public API has none.
//!
//! # The handle kinds, and why one counter mints all of them
//!
//! A database, a snapshot, a batch, a query, a stream, a set, a container and a
//! set builder are all "an integer" to a scenario, and they are **not**
//! interchangeable: passing a set where a query belongs must be an error, not a
//! lookup into the wrong table that happens to succeed.
//!
//! Per-kind `Vec`s do not give that, and believing they did was wrong for a
//! day. Every table starts at index 0, so `q_cardinality( a_set_handle )`
//! found *query* 0 — a different object, a plausible answer, no error. Both
//! tables having a live entry 0 is the normal case, not a corner.
//!
//! So [`HandleKind`] tags every handle and one counter mints them all: a handle
//! is an index into [`World::handles`], which says which table it belongs to
//! and where in it. A transposed argument now reads
//! `q_cardinality(): handle 3 is a set, not a query`, which names the mistake
//! instead of answering a different question.
//!
//! Five kinds — database, snapshot, batch, stream, set builder — can also be
//! *invalidated*, and that is the other reason a handle table beats a Python
//! object: a closed database must raise on next use rather than resurrect.
//! Sets, containers and queries cannot be invalidated, because they are frozen
//! values with nothing to release.
//!
//! # Where the verbs live
//!
//! This module owns the database — lifecycle, writes, batches, snapshots,
//! diagnostics — plus the handle tables and dispatch. The verb families that
//! reach other shipped surfaces live next door:
//!
//! * [`crate::eager`] — `sb_*` / `set_*` / `ct_*` / `ops_*`: building `OrdSet`s
//!   from arithmetic progressions, taking them apart chunk by chunk, and
//!   running the container kernels by hand.
//! * [`crate::lazy`] — `q_*` / `st_*`: expression trees, the planner, and raw
//!   `ChunkStream` cursors.
//! * [`crate::operator`] — `op_*`: a kind-backed controller lifecycle whose
//!   Kubernetes observations are asserted by an opt-in Python scenario.
//! * [`crate::filesystems`] — `fs_*`: real server-owned ZFS, Btrfs, and LVM
//!   snapshot leases in a disposable QEMU/KVM guest.
//!
//! `all_names()` stays the single source of truth across modules, and
//! `every_advertised_verb_is_dispatched` keeps it honest.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use monty_types::{MontyException, MontyObject};
use yesno_core::stream::BoxedStream;
use yesno_core::{Container, Db, DbOptions, Expr, OrdSet, Snapshot};

use crate::convert::{
    db_err, dict, handle_obj, int_obj, opt_int_obj, tuple, type_err, value_err, whole_obj, Args,
};

/// Every verb the harness answers. `NAMES` is the single source of truth for
/// name resolution; `call` is checked against it by a test, so a verb cannot be
/// advertised and then not dispatched.
pub const NAMES: &[&str] = &[
    // lifecycle
    "db_open",
    "db_close",
    "db_reopen",
    "db_checkpoint",
    // writes
    "db_insert",
    "db_insert_many",
    "db_insert_range",
    "db_insert_set",
    "db_remove",
    "db_remove_range",
    // batches
    "batch",
    "batch_insert",
    "batch_remove",
    "batch_insert_range",
    "batch_remove_range",
    "batch_delete_key",
    "batch_store_set",
    "batch_commit",
    "batch_rollback",
    // reads
    "db_snapshot",
    "snap_release",
    "snap_version",
    "snap_contains",
    "snap_cardinality",
    "snap_is_empty",
    "snap_load",
    "snap_load_set",
    "snap_min",
    "snap_max",
    "snap_rank",
    "snap_select",
    // lazy expressions
    "q_key",
    "q_range",
    "q_empty",
    "q_and",
    "q_or",
    "q_xor",
    "q_andnot",
    "q_cardinality",
    "q_collect",
    // diagnostics
    "db_stats",
    "db_wal_layout",
    "db_fsck",
    "db_shard_of",
    "db_epoch",
    "db_is_durable",
    "db_slabs_by_class",
    "db_live_fractions",
    "db_slab_states",
    // the harness itself
    "yn_const",
    "yn_class_for",
    "yn_arg",
    "clock_ns",
];

/// Every verb, across all three modules. The single source of truth for name
/// resolution: [`World::is_verb`] answers from this, and a name absent from it
/// raises `NameError` in the scenario rather than quietly doing nothing.
///
/// Assembled at first use rather than as a `const`, because concatenating
/// `&[&str]` slices in a `const` needs a macro that would obscure the three
/// lists it is built from.
pub fn all_names() -> &'static [&'static str] {
    static ALL: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut v: Vec<&'static str> = Vec::new();
        v.extend_from_slice(NAMES);
        v.extend_from_slice(crate::eager::OWNS);
        v.extend_from_slice(crate::lazy::OWNS);
        v.extend_from_slice(crate::matrix::OWNS);
        v.extend_from_slice(crate::view::OWNS);
        v.extend_from_slice(crate::bignum::OWNS);
        v.extend_from_slice(crate::repl::OWNS);
        v.extend_from_slice(crate::flight::OWNS);
        v.extend_from_slice(crate::fixture::OWNS);
        v.extend_from_slice(crate::server::OWNS);
        v.extend_from_slice(crate::operator::OWNS);
        v.extend_from_slice(crate::filesystems::OWNS);
        v.extend_from_slice(crate::aws::OWNS);
        v.extend_from_slice(crate::cloud::OWNS);
        v.extend_from_slice(crate::search::OWNS);
        v.extend_from_slice(crate::arrow::OWNS);
        v.extend_from_slice(crate::datafusion::OWNS);
        v
    })
}

/// What a handle points at.
///
/// The tag is the whole point: without it a handle is just a small integer that
/// happens to be in range for several tables at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleKind {
    Database,
    Snapshot,
    Batch,
    Query,
    Set,
    Container,
    Stream,
    Builder,
    /// A running `LeaderService` and a client connected to it.
    Leader,
    /// A replica directory and the shipped follower driving it.
    Follower,
    /// A dense `BitMatrix` value. Frozen like a set: never invalidated.
    Matrix,
    /// A `View` descriptor: how several sets share one ordinal space. A plain
    /// value, so like `Matrix` it is never invalidated.
    View,
    /// A running `YesnoFlightService` and a client connected to it.
    Flight,
    /// A generic managed child process owned by an E2E fixture.
    FixtureProcess,
    /// A running `yesnod`, which owns its own database.
    Server,
    /// A `yesnod` configuration being built up before it is started.
    ServerConfig,
    /// A running `yesno-archive` sidecar.
    Archive,
}

impl HandleKind {
    /// How the kind is named in an error, as an English noun phrase.
    fn name(self) -> &'static str {
        match self {
            HandleKind::Database => "database",
            HandleKind::Snapshot => "snapshot",
            HandleKind::Batch => "batch",
            HandleKind::Query => "query",
            HandleKind::Set => "set",
            HandleKind::Container => "container",
            HandleKind::Stream => "stream",
            HandleKind::Builder => "set builder",
            HandleKind::Leader => "leader",
            HandleKind::Follower => "follower",
            HandleKind::Matrix => "matrix",
            HandleKind::View => "view",
            HandleKind::Flight => "flight endpoint",
            HandleKind::FixtureProcess => "fixture process",
            HandleKind::Server => "server",
            HandleKind::ServerConfig => "server config",
            HandleKind::Archive => "archive sidecar",
        }
    }
}

struct OpenDb {
    db: Db,
    dir: PathBuf,
    opts: DbOptions,
    /// Number of live snapshot handles taken from this database. A `Snapshot`
    /// keeps the whole store — and therefore the directory's exclusive lock —
    /// alive, so closing underneath one would leave the next `db_open` of the
    /// same directory failing with a lock error that points nowhere near the
    /// actual mistake.
    live_snaps: usize,
    /// Flight services serving this database. Each holds a `Db` clone, and a
    /// `Db` clone holds the directory's exclusive lock — the same hazard
    /// `live_snaps` guards, arriving through a different door.
    live_flights: usize,
}

struct OpenSnap {
    snap: Snapshot,
    /// The *handle* of the database this came from, not a table index, so
    /// releasing goes back through the same tagged lookup as everything else.
    db: usize,
}

/// One scenario's whole mutable state.
pub struct World {
    pub(crate) root: PathBuf,
    pub(crate) fixture: crate::fixture::FixtureState,
    _tmp: Option<tempfile::TempDir>,
    dbs: Vec<Option<OpenDb>>,
    snaps: Vec<Option<OpenSnap>>,
    batches: Vec<Option<(usize, Vec<Mutation>)>>,
    exprs: Vec<Expr>,
    /// Staged ordinals awaiting `sb_build`. `None` once built.
    pub(crate) builders: Vec<Option<Vec<u64>>>,
    /// Frozen values, so no slot is ever invalidated.
    sets: Vec<Arc<OrdSet>>,
    pub(crate) matrices: Vec<yesno_core::matrix::BitMatrix>,
    pub(crate) views: Vec<yesno_core::view::View>,
    containers: Vec<Container>,
    /// Cursors, which *are* invalidated: a stream is one-shot and `st_release`
    /// makes that visible rather than leaving a drained cursor callable.
    pub(crate) streams: Vec<Option<BoxedStream>>,
    /// Whole-number knobs from the runner's `--arg name=value`, read by
    /// `yn_arg`. Empty under `cargo test`, so every scenario runs at its
    /// default ( small ) corpus unless someone asks for more.
    args: Vec<(String, u64)>,
    /// Origin for `clock_ns`. Per-world rather than a process-wide epoch, so a
    /// scenario's timings start near zero and stay inside an `i64`.
    started: Instant,
    /// Every handle ever issued, as `( kind, index into that kind's table )`.
    ///
    /// One counter for all eight tables, which is what makes a handle
    /// self-describing. Never shrinks — a released handle keeps its entry so
    /// the *kind* is still known and "already released" can be told from
    /// "never issued" and from "that is a set, not a query".
    handles: Vec<(HandleKind, usize)>,
    /// Leaders, followers and the tokio runtime that drives them. Owned by
    /// [`crate::repl`] so that this module need not know tonic exists, and
    /// empty — runtime included — until a scenario calls a `repl_*` verb.
    pub(crate) repl: crate::repl::ReplState,
    /// Flight endpoints. Owned by [`crate::flight`] for the same reason `repl`
    /// is owned by its module, and sharing that module's runtime rather than
    /// starting a second one.
    pub(crate) flight: crate::flight::FlightState,
    /// Daemons started by `srv_*`. Owned by [`crate::server`] for the same
    /// reason the two above are owned by theirs.
    pub(crate) server_state: crate::server::ServerState,
    /// Live kind cluster and operator resources owned by `op_*` verbs.
    pub(crate) operator: crate::operator::OperatorState,
    /// QEMU/KVM guest used only by the opt-in native-filesystem scenarios.
    pub(crate) filesystems: crate::filesystems::FilesystemState,
    /// Terraform-owned EC2/EBS fixture used only by the opt-in AWS scenario.
    pub(crate) aws: crate::aws::AwsState,
    /// The host side of that gate: Terraform, the ECR push, and the Systems
    /// Manager round trips that put a scenario on the runner. Empty, and
    /// refusing to act, unless the gate opted in.
    pub(crate) cloud: crate::cloud::CloudState,
    /// Java helper and disposable search engine resources owned by `search_*`.
    pub(crate) search: crate::search::SearchState,
    /// SQL calls into the ABI-pinned PostgreSQL cluster supplied by gate-pg.
    calls: u64,
}

/// A staged batch mutation.
///
/// Staged rather than applied against a live `WriteBatch`, because
/// `WriteBatch` borrows the `Db` and holding one across the interpreter's
/// suspension points would borrow the world for the rest of the scenario.
/// Replaying at commit is equivalent: `WriteBatch` accumulates and applies
/// at `commit()` anyway.
enum Mutation {
    Insert(u64, u64),
    Remove(u64, u64),
    InsertRange(u64, u64, u64),
    RemoveRange(u64, u64, u64),
    DeleteKey(u64),
    /// Replace a key's whole contents with a set built in the scenario.
    StoreSet(u64, Arc<OrdSet>),
}

include!("scenario_prefix.rs");

impl World {
    /// A world rooted at a fresh temporary directory, removed on drop.
    pub fn temporary() -> std::io::Result<Self> {
        Self::temporary_with(Vec::new())
    }

    /// A world carrying the runner's `--arg` knobs, readable via `yn_arg`.
    pub fn temporary_with(args: Vec<(String, u64)>) -> std::io::Result<Self> {
        Self::temporary_labelled("adhoc", args)
    }

    /// [`Self::temporary_with`], naming the root after the scenario that owns it.
    ///
    /// See [`scenario_prefix`] for why the name matters: a failing scenario's
    /// root is deliberately retained, and an anonymous one cannot be found again.
    pub fn temporary_labelled(label: &str, args: Vec<(String, u64)>) -> std::io::Result<Self> {
        let tmp = tempfile::Builder::new()
            .prefix(&scenario_prefix(label))
            .tempdir()?;
        Ok(World {
            root: tmp.path().to_path_buf(),
            fixture: Default::default(),
            _tmp: Some(tmp),
            dbs: Vec::new(),
            snaps: Vec::new(),
            batches: Vec::new(),
            exprs: Vec::new(),
            builders: Vec::new(),
            sets: Vec::new(),
            matrices: Vec::new(),
            views: Vec::new(),
            containers: Vec::new(),
            streams: Vec::new(),
            args,
            started: Instant::now(),
            handles: Vec::new(),
            repl: Default::default(),
            flight: Default::default(),
            server_state: Default::default(),
            operator: Default::default(),
            filesystems: Default::default(),
            aws: Default::default(),
            cloud: Default::default(),
            search: Default::default(),
            calls: 0,
        })
    }

    /// Issue a handle for `idx` in `kind`'s table.
    pub(crate) fn mint(&mut self, kind: HandleKind, idx: usize) -> MontyObject {
        self.handles.push((kind, idx));
        handle_obj(self.handles.len() - 1)
    }

    /// Resolve a handle to an index in `want`'s table, or say precisely what is
    /// wrong with it.
    ///
    /// Three distinguishable failures, and telling them apart is the reason
    /// this exists: a handle nobody issued, a handle of the wrong kind, and —
    /// for the invalidatable kinds, checked by the caller — one already
    /// released.
    pub(crate) fn slot(
        &self,
        h: usize,
        want: HandleKind,
        verb: &str,
    ) -> Result<usize, MontyException> {
        match self.handles.get(h) {
            None => Err(value_err(format!(
                "{verb}(): {h} is not a handle issued by this scenario"
            ))),
            Some((got, _)) if *got != want => Err(value_err(format!(
                "{verb}(): handle {h} is a {}, not a {}",
                got.name(),
                want.name()
            ))),
            Some((_, idx)) => Ok(*idx),
        }
    }

    /// How many verbs the scenario actually invoked.
    ///
    /// The runner refuses a scenario that called none: a script that never
    /// reached the database cannot be testing it, however many assertions it
    /// makes about its own arithmetic.
    pub fn calls(&self) -> u64 {
        self.calls
    }

    pub fn is_verb(name: &str) -> bool {
        all_names().contains(&name)
    }

    /// Dispatch one verb call.
    pub fn call(
        &mut self,
        verb: &str,
        pos: &[MontyObject],
        kw: &[(MontyObject, MontyObject)],
    ) -> Result<MontyObject, MontyException> {
        self.calls += 1;
        let a = Args::new(verb, pos, kw);
        if crate::eager::OWNS.contains(&verb) {
            return self.call_eager(verb, &a);
        }
        if crate::lazy::OWNS.contains(&verb) {
            return self.call_lazy(verb, &a);
        }
        if crate::repl::OWNS.contains(&verb) {
            return self.call_repl(verb, &a);
        }
        if crate::matrix::OWNS.contains(&verb) {
            return self.call_matrix(verb, &a);
        }
        if crate::view::OWNS.contains(&verb) {
            return self.call_view(verb, &a);
        }
        if crate::bignum::OWNS.contains(&verb) {
            return self.call_bignum(verb, &a);
        }
        if crate::fixture::OWNS.contains(&verb) {
            return self.call_fixture(verb, &a);
        }
        if crate::flight::OWNS.contains(&verb) {
            return self.call_flight(verb, &a);
        }
        if crate::datafusion::OWNS.contains(&verb) {
            return self.call_datafusion(verb, &a);
        }
        if crate::arrow::OWNS.contains(&verb) {
            return self.call_arrow(verb, &a);
        }
        if crate::operator::OWNS.contains(&verb) {
            return self.call_operator(verb, &a);
        }
        if crate::filesystems::OWNS.contains(&verb) {
            return self.call_filesystems(verb, &a);
        }
        if crate::aws::OWNS.contains(&verb) {
            return self.call_aws(verb, &a);
        }
        if crate::cloud::OWNS.contains(&verb) {
            return self.call_cloud(verb, &a);
        }
        if crate::search::OWNS.contains(&verb) {
            return self.call_search(verb, &a);
        }
        if crate::server::OWNS.contains(&verb) {
            return self.call_server(verb, &a);
        }
        match verb {
            "db_open" => self.db_open(&a),
            "db_close" => self.db_close(&a),
            "db_reopen" => self.db_reopen(&a),
            "db_checkpoint" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let db = self.db(a.handle(0)?, verb)?;
                let v = db.db.checkpoint().map_err(|e| db_err(verb, e))?;
                Ok(int_obj(v))
            }

            "db_insert" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (h, key, ord) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                let db = self.db(h, verb)?;
                let changed = db.db.insert(key, ord).map_err(|e| db_err(verb, e))?;
                Ok(MontyObject::Bool(changed))
            }
            "db_insert_many" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (h, key, vals) = (a.handle(0)?, a.u64(1)?, a.u64_list(2)?);
                let db = self.db(h, verb)?;
                let n = db.db.insert_many(key, &vals).map_err(|e| db_err(verb, e))?;
                Ok(int_obj(n))
            }
            "db_insert_range" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let (h, key, lo, hi) = (a.handle(0)?, a.u64(1)?, a.u64(2)?, a.u64(3)?);
                check_range(verb, lo, hi)?;
                let db = self.db(h, verb)?;
                let n = db
                    .db
                    .insert_range(key, lo, hi)
                    .map_err(|e| db_err(verb, e))?;
                Ok(int_obj(n))
            }
            // Ingest a set the scenario built, without moving its ordinals
            // through the VM as a list. This is what makes a corpus of a few
            // million ordinals a one-line scenario rather than an impossible
            // one.
            "db_insert_set" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (h, key, sh) = (a.handle(0)?, a.u64(1)?, a.handle(2)?);
                let vals: Vec<u64> = self.set(sh, verb)?.iter().collect();
                let db = self.db(h, verb)?;
                let n = db.db.insert_many(key, &vals).map_err(|e| db_err(verb, e))?;
                Ok(int_obj(n))
            }
            "db_remove" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (h, key, ord) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                let db = self.db(h, verb)?;
                let changed = db.db.remove(key, ord).map_err(|e| db_err(verb, e))?;
                Ok(MontyObject::Bool(changed))
            }
            "db_remove_range" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let (h, key, lo, hi) = (a.handle(0)?, a.u64(1)?, a.u64(2)?, a.u64(3)?);
                check_range(verb, lo, hi)?;
                let db = self.db(h, verb)?;
                let n = db
                    .db
                    .remove_range(key, lo, hi)
                    .map_err(|e| db_err(verb, e))?;
                Ok(int_obj(n))
            }

            "batch" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                self.db(h, verb)?;
                self.batches.push(Some((h, Vec::new())));
                let idx = self.batches.len() - 1;
                Ok(self.mint(HandleKind::Batch, idx))
            }
            "batch_insert" => {
                a.exact(3)?;
                let (b, key, ord) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                self.stage(b, verb, Mutation::Insert(key, ord))
            }
            "batch_remove" => {
                a.exact(3)?;
                let (b, key, ord) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                self.stage(b, verb, Mutation::Remove(key, ord))
            }
            "batch_insert_range" => {
                a.exact(4)?;
                let (b, key, lo, hi) = (a.handle(0)?, a.u64(1)?, a.u64(2)?, a.u64(3)?);
                check_range(verb, lo, hi)?;
                self.stage(b, verb, Mutation::InsertRange(key, lo, hi))
            }
            "batch_remove_range" => {
                a.exact(4)?;
                let (b, key, lo, hi) = (a.handle(0)?, a.u64(1)?, a.u64(2)?, a.u64(3)?);
                check_range(verb, lo, hi)?;
                self.stage(b, verb, Mutation::RemoveRange(key, lo, hi))
            }
            "batch_delete_key" => {
                a.exact(2)?;
                let (b, key) = (a.handle(0)?, a.u64(1)?);
                self.stage(b, verb, Mutation::DeleteKey(key))
            }
            // `WriteBatch::store_set` replaces a key's whole contents. Staged
            // like every other mutation, and the set is captured by handle
            // *now* rather than at commit, so a scenario that builds a fresh
            // set into the same variable afterwards stages what it wrote.
            "batch_store_set" => {
                a.exact(3)?;
                let (b, key, sh) = (a.handle(0)?, a.u64(1)?, a.handle(2)?);
                let set = self.set(sh, verb)?.clone();
                self.stage(b, verb, Mutation::StoreSet(key, set))
            }
            "batch_commit" => self.batch_commit(&a),
            "batch_rollback" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let b = a.handle(0)?;
                self.take_batch(b, verb)?;
                Ok(MontyObject::None)
            }

            "db_snapshot" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let snap = self
                    .db(h, verb)?
                    .db
                    .snapshot()
                    .map_err(|e| db_err(verb, e))?;
                self.db(h, verb)?.live_snaps += 1;
                self.snaps.push(Some(OpenSnap { snap, db: h }));
                let idx = self.snaps.len() - 1;
                Ok(self.mint(HandleKind::Snapshot, idx))
            }
            "snap_release" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let s = a.handle(0)?;
                let i = self.slot(s, HandleKind::Snapshot, verb)?;
                let Some(open) = self.snaps[i].take() else {
                    return Err(stale_handle(verb, "snapshot", s));
                };
                let db = open.db;
                // Dropped before the database's counter is decremented: the
                // `Snapshot` is what holds the store open, so the count must
                // not fall to zero while one is still alive.
                drop(open);
                if let Ok(d) = self.db(db, verb) {
                    d.live_snaps -= 1;
                }
                Ok(MontyObject::None)
            }
            "snap_version" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let s = a.handle(0)?;
                Ok(int_obj(self.snap(s, verb)?.version()))
            }
            "snap_contains" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (s, key, ord) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                Ok(MontyObject::Bool(
                    self.snap(s, verb)?
                        .contains(key, ord)
                        .map_err(|e| db_err(verb, e))?,
                ))
            }
            "snap_cardinality" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                Ok(int_obj(
                    self.snap(s, verb)?
                        .cardinality(key)
                        .map_err(|e| db_err(verb, e))?,
                ))
            }
            "snap_is_empty" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                Ok(MontyObject::Bool(
                    self.snap(s, verb)?
                        .is_empty(key)
                        .map_err(|e| db_err(verb, e))?,
                ))
            }
            "snap_load" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;
                Ok(MontyObject::List(set.iter().map(int_obj).collect()))
            }
            // The same load, as a set handle. A key holding a million ordinals
            // is a list a scenario cannot afford and a set handle it can, and
            // it is what lets stored data reach the `set_*` and `q_set` verbs.
            "snap_load_set" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;
                Ok(self.push_set(set))
            }
            "snap_min" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                Ok(opt_int_obj(
                    self.snap(s, verb)?.min(key).map_err(|e| db_err(verb, e))?,
                ))
            }
            "snap_max" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                Ok(opt_int_obj(
                    self.snap(s, verb)?.max(key).map_err(|e| db_err(verb, e))?,
                ))
            }
            // `rank` is strictly-less-than and `select` is zero-based, so the
            // two invert. Mirrored from `OrdSet` without adjustment.
            "snap_rank" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (s, key, ord) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                Ok(int_obj(self.snap(s, verb)?.load(key).unwrap().rank(ord)))
            }
            "snap_select" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (s, key, n) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                Ok(opt_int_obj(
                    self.snap(s, verb)?.load(key).unwrap().select(n),
                ))
            }

            "q_key" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;
                Ok(self.push_expr(Expr::set(set)))
            }
            // Half-open `[lo, hi)`, mirroring `Expr::Range` exactly.
            //
            // This is the opposite convention to `db_insert_range`, which is
            // `[lo, hi]` inclusive. The inconsistency is yesno-core's, not the
            // harness's, and it is mirrored rather than smoothed over so that
            // `e2e/scenarios/ranges.py` can pin both conventions; a harness that
            // quietly normalized them would make the scenarios a worse oracle
            // than the library they test.
            //
            // An inverted range is allowed through, because `RangeStream`
            // documents it as yielding nothing and that is a behaviour worth
            // being able to assert.
            "q_range" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (lo, hi) = (a.u64(0)?, a.u64(1)?);
                Ok(self.push_expr(Expr::Range(lo, hi)))
            }
            "q_empty" => {
                a.exact(0)?;
                a.no_kwargs()?;
                Ok(self.push_expr(Expr::Empty))
            }
            "q_and" | "q_or" | "q_xor" | "q_andnot" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (l, r) = (a.handle(0)?, a.handle(1)?);
                let (le, re) = (self.expr(l, verb)?, self.expr(r, verb)?);
                let out = match verb {
                    "q_and" => le.and(re),
                    "q_or" => le.or(re),
                    "q_xor" => le.xor(re),
                    _ => le.and_not(re),
                };
                Ok(self.push_expr(out))
            }
            "q_cardinality" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let e = self.expr(a.handle(0)?, verb)?;
                Ok(int_obj(e.cardinality().map_err(|err| db_err(verb, err))?))
            }
            "q_collect" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let e = self.expr(a.handle(0)?, verb)?;
                let set: OrdSet = e.collect_set().map_err(|err| db_err(verb, err))?;
                Ok(MontyObject::List(set.iter().map(int_obj).collect()))
            }

            "db_stats" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let db = &self.db(a.handle(0)?, verb)?.db;
                Ok(dict(vec![
                    ("shards", int_obj(db.shard_count() as u64)),
                    ("epoch", int_obj(db.epoch())),
                    ("visible", int_obj(db.visible())),
                    ("safe_version", int_obj(db.safe_version())),
                    ("live_readers", int_obj(db.live_readers() as u64)),
                    ("dirty_bytes", int_obj(db.dirty_bytes() as u64)),
                    ("wal_bytes", int_obj(db.wal_bytes())),
                    ("wal_syncs", int_obj(db.wal_syncs())),
                    ("deferred_bytes", int_obj(db.deferred_bytes())),
                    ("deferred_extents", int_obj(db.deferred_extents() as u64)),
                    ("allocated_bytes", int_obj(db.allocated_bytes())),
                    ("used_extents", int_obj(db.used_extents())),
                    ("freed_extents", int_obj(db.freed_extents())),
                    ("pinned_extents", int_obj(db.pinned_extents() as u64)),
                    ("slabs", int_obj(db.slab_count() as u64)),
                    ("index_nodes_written", int_obj(db.index_nodes_written())),
                    // Written minus freed is what the index currently costs.
                    // `written` alone is a monotone counter and says nothing
                    // about the standing footprint, which is the number the
                    // aged-state measurement is about.
                    ("index_nodes_freed", int_obj(db.index_nodes_freed())),
                    ("evacuated_chunks", int_obj(db.evacuated_chunks())),
                    (
                        "space_amplification",
                        MontyObject::Float(db.space_amplification()),
                    ),
                ]))
            }
            // Physical WAL layout, deliberately narrower than a filesystem
            // verb. Scenarios need to distinguish a sealed generation from a
            // rewritten active file, but they must not gain ambient authority
            // over their temporary root.
            "db_wal_layout" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let db = self.db(a.handle(0)?, verb)?;
                let mut layout = Vec::with_capacity(db.db.shard_count());
                for shard in 0..db.db.shard_count() {
                    let active_name = format!("shard-{shard:04}.wal");
                    let active = db.dir.join(&active_name);
                    let active_bytes = std::fs::metadata(&active)
                        .map(|meta| meta.len())
                        .unwrap_or(0);
                    let generation_prefix = format!("{active_name}.");
                    let mut sealed_generations = 0u64;
                    let mut sealed_bytes = 0u64;
                    for entry in std::fs::read_dir(&db.dir).map_err(|e| db_err(verb, e))? {
                        let entry = entry.map_err(|e| db_err(verb, e))?;
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        let Some(suffix) = name.strip_prefix(&generation_prefix) else {
                            continue;
                        };
                        if suffix.len() != 20 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                            continue;
                        }
                        sealed_generations += 1;
                        sealed_bytes += entry.metadata().map_err(|e| db_err(verb, e))?.len();
                    }
                    layout.push(dict(vec![
                        ("shard", int_obj(shard as u64)),
                        ("sealed_generations", int_obj(sealed_generations)),
                        ("sealed_bytes", int_obj(sealed_bytes)),
                        ("active_bytes", int_obj(active_bytes)),
                        ("retained_bytes", int_obj(sealed_bytes + active_bytes)),
                    ]));
                }
                Ok(MontyObject::List(layout))
            }
            // Returns both `clean` and `consistent`. `clean` is the strong
            // form — nothing leaked, nothing dangling, no packed-page
            // mismatch — and it is only meaningful because the rebuild now
            // marks the index's *own* nodes and tells retention from waste. It
            // used to be structurally false for any database with an index,
            // since every live node was counted as a leaked slot.
            //
            // `consistent` is the narrower question: is anything actually
            // corrupt. A scenario asserting on `clean` is asserting that space
            // accounting is exact as well.
            "db_fsck" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let db = &self.db(a.handle(0)?, verb)?.db;
                let reports = db.verify().map_err(|e| db_err(verb, e))?;
                let clean = reports
                    .iter()
                    .all(yesno_core::store::fsck::FsckReport::is_clean);
                // `errors` carries corruption only: hard errors, dangling chunk
                // references and dangling index nodes. Leaked slots are counted
                // separately, because "space unaccounted for" and "a live
                // reference into free space" want different reactions.
                let mut errors: Vec<MontyObject> = Vec::new();
                let (mut chunks, mut inline, mut nodes) = (0u64, 0u64, 0u64);
                let (mut leaked, mut pending, mut dangling) = (0usize, 0usize, 0usize);
                for (shard, r) in reports.iter().enumerate() {
                    chunks += r.chunks;
                    inline += r.inline_chunks;
                    nodes += r.index_nodes;
                    leaked += r.leaked.len();
                    pending += r.pending;
                    dangling += r.dangling.len() + r.dangling_nodes.len();
                    for e in &r.errors {
                        errors.push(MontyObject::String(format!("shard {shard}: {e}")));
                    }
                    for (ck, cell) in &r.dangling {
                        errors.push(MontyObject::String(format!(
                            "shard {shard}: dangling chunk {ck:?} at cell {cell}"
                        )));
                    }
                    for cell in &r.dangling_nodes {
                        errors.push(MontyObject::String(format!(
                            "shard {shard}: index node at cell {cell} sits in a slot the allocator calls free"
                        )));
                    }
                    for (page, claimed, live) in &r.packed_live_mismatch {
                        errors.push(MontyObject::String(format!(
                            "shard {shard}: packed page {page} claims {claimed} live bytes, index says {live}"
                        )));
                    }
                }
                Ok(dict(vec![
                    ("clean", MontyObject::Bool(clean)),
                    (
                        "consistent",
                        MontyObject::Bool(dangling == 0 && errors.is_empty()),
                    ),
                    ("chunks", int_obj(chunks)),
                    ("inline_chunks", int_obj(inline)),
                    ("index_nodes", int_obj(nodes)),
                    ("leaked", int_obj(leaked as u64)),
                    ("pending", int_obj(pending as u64)),
                    ("dangling", int_obj(dangling as u64)),
                    ("errors", MontyObject::List(errors)),
                ]))
            }
            "db_shard_of" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, key) = (a.handle(0)?, a.u64(1)?);
                Ok(int_obj(self.db(h, verb)?.db.shard_of(key) as u64))
            }
            "db_epoch" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(int_obj(self.db(a.handle(0)?, verb)?.db.epoch()))
            }
            "db_is_durable" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(MontyObject::Bool(
                    self.db(a.handle(0)?, verb)?.db.is_durable(),
                ))
            }
            // Which size class is holding the space, as `( class, slabs )`.
            //
            // The slab total conflates index retention with chunk-extent
            // retention, and those are the two candidates the cost model names
            // — so a measurement that cannot separate them cannot say which
            // one to work on.
            "db_slabs_by_class" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let by = self.db(a.handle(0)?, verb)?.db.slabs_by_class();
                Ok(MontyObject::List(
                    by.into_iter()
                        .map(|(c, n)| tuple(vec![int_obj(u64::from(c)), whole_obj(n)]))
                        .collect(),
                ))
            }
            // `( used, capacity )` per slab of one class: how full the slabs
            // holding the space actually are, which is what decides whether
            // compaction has anything to compact.
            "db_live_fractions" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, class) = (a.handle(0)?, a.u64(1)?);
                let class = u8::try_from(class).map_err(|_| {
                    value_err(format!("{verb}(): {class} is not a size class ( 0-255 )"))
                })?;
                let fr = self.db(h, verb)?.db.live_fractions(class);
                Ok(MontyObject::List(
                    fr.into_iter()
                        .map(|(u, c)| tuple(vec![int_obj(u64::from(u)), int_obj(u64::from(c))]))
                        .collect(),
                ))
            }
            // `in_use` rather than the slab total is the honest denominator
            // for a space measurement: a `Free` slab is reusable and does not
            // grow the file, so counting it under-credits anything whose whole
            // job is turning in-use slabs back into free ones.
            "db_slab_states" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let (free, in_use, opaque) = self.db(a.handle(0)?, verb)?.db.slab_states();
                Ok(dict(vec![
                    ("free", whole_obj(free)),
                    ("in_use", whole_obj(in_use)),
                    ("opaque", whole_obj(opaque)),
                ]))
            }

            // A named constant from `yesno_core`. One verb rather than one verb
            // per constant, so the surface does not grow every time a fixture
            // needs another number — and an unknown name is an error, so a
            // typo'd constant cannot read as zero.
            "yn_const" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let name = a.str_at(0)?;
                let v = match name {
                    "CHUNK_BITS" => u64::from(yesno_core::CHUNK_BITS),
                    "CHUNK_CARD" => u64::from(yesno_core::CHUNK_CARD),
                    "ARRAY_MAX" => yesno_core::ARRAY_MAX as u64,
                    "BITMAP_BYTES" => yesno_core::BITMAP_BYTES as u64,
                    "BITMAP_DEMOTE" => u64::from(yesno_core::BITMAP_DEMOTE),
                    "RUN_MAX_INTERVALS" => u64::from(yesno_core::RUN_MAX_INTERVALS),
                    "ORDINAL_MAX" => yesno_core::ORDINAL_MAX,
                    "SLAB_SIZE" => yesno_core::store::SLAB_SIZE,
                    "INDEX_NODE" => yesno_core::store::INDEX_NODE as u64,
                    other => {
                        return Err(value_err(format!(
                            "{verb}(): '{other}' is not a constant this harness exposes"
                        )))
                    }
                };
                Ok(int_obj(v))
            }
            // The allocator's size-class ladder, so a scenario can ask which
            // class its own payload would land in — `INDEX_NODE`'s class is how
            // the aged-state measurement separates index slabs from the rest.
            "yn_class_for" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let bytes = a.usize_at(0)?;
                Ok(yesno_core::store::extent::class_for(bytes)
                    .map_or(MontyObject::None, |c| int_obj(u64::from(c))))
            }
            // A whole-number knob from the runner's `--arg name=value`.
            //
            // This is what replaces an example's `--big` / `--sparse` command
            // line. Absent means `default`, so every scenario runs at a
            // gate-sized corpus under `cargo test` and can be scaled up by hand
            // without editing the file.
            "yn_arg" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let name = a.str_at(0)?.to_owned();
                let default = a.u64(1)?;
                Ok(int_obj(
                    self.args
                        .iter()
                        .rev()
                        .find(|(k, _)| *k == name)
                        .map_or(default, |(_, v)| *v),
                ))
            }
            // Monotonic nanoseconds since the world was created.
            //
            // Good enough for the millisecond-scale columns ( ingest rates,
            // checkpoint latency ) and **not** for the nanosecond-scale ones:
            // a Python loop pays a host call per iteration, which is larger
            // than the quantity `rule_economics` measures. Use `q_time` there.
            "clock_ns" => {
                a.exact(0)?;
                a.no_kwargs()?;
                Ok(int_obj(self.started.elapsed().as_nanos() as u64))
            }

            // Unreachable while `NAMES` and this match agree, which
            // `every_advertised_verb_is_dispatched` enforces.
            _ => Err(type_err(format!("{verb}() is not a harness verb"))),
        }
    }

    // ---- verb helpers -----------------------------------------------------

    fn db_open(&mut self, a: &Args<'_>) -> Result<MontyObject, MontyException> {
        a.between(0, 1)?;
        let defaults = DbOptions::default();
        a.kw_allowed(&["shards", "evacuate"])?;
        let shards = a.kw_usize_min("shards", defaults.shards, 1)?;
        // `min` is 0 here: disabling evacuation entirely is the A/B control
        // the aged-state measurement is built around, and a `> 0` check would
        // make that column unreachable from a scenario.
        let evacuate = a.kw_usize_min("evacuate", defaults.evacuate_per_checkpoint, 0)?;
        let name = a
            .opt_str(0)?
            .unwrap_or_else(|| format!("db{}", self.dbs.len()));
        let dir = self.scenario_path(&name, "db_open")?;
        std::fs::create_dir_all(&dir).map_err(|e| db_err("db_open", e))?;
        let opts = DbOptions {
            shards,
            evacuate_per_checkpoint: evacuate,
            ..defaults
        };
        let db = Db::open_with(&dir, opts).map_err(|e| db_err("db_open", e))?;
        self.dbs.push(Some(OpenDb {
            db,
            dir,
            opts,
            live_snaps: 0,
            live_flights: 0,
        }));
        let idx = self.dbs.len() - 1;
        Ok(self.mint(HandleKind::Database, idx))
    }

    fn db_close(&mut self, a: &Args<'_>) -> Result<MontyObject, MontyException> {
        a.exact(1)?;
        a.no_kwargs()?;
        self.take_db(a.handle(0)?, "db_close")?;
        Ok(MontyObject::None)
    }

    fn db_reopen(&mut self, a: &Args<'_>) -> Result<MontyObject, MontyException> {
        a.exact(1)?;
        a.no_kwargs()?;
        let OpenDb {
            db: old, dir, opts, ..
        } = self.take_db(a.handle(0)?, "db_reopen")?;
        // Explicit, and load-bearing. A `..` pattern does **not** drop the
        // fields it skips at the `let`; they live until the end of the
        // enclosing scope. Leaving the old `Db` alive here held the
        // directory's exclusive lock across the reopen below, so every
        // `db_reopen` failed with `AlreadyOpen` — against itself.
        drop(old);
        let db = Db::open_with(&dir, opts).map_err(|e| db_err("db_reopen", e))?;
        self.dbs.push(Some(OpenDb {
            db,
            dir,
            opts,
            live_snaps: 0,
            live_flights: 0,
        }));
        let idx = self.dbs.len() - 1;
        Ok(self.mint(HandleKind::Database, idx))
    }

    fn batch_commit(&mut self, a: &Args<'_>) -> Result<MontyObject, MontyException> {
        a.exact(1)?;
        a.no_kwargs()?;
        let (db_h, muts) = self.take_batch(a.handle(0)?, "batch_commit")?;
        let db = &self.db(db_h, "batch_commit")?.db;
        let mut wb = db.batch();
        for m in &muts {
            match *m {
                Mutation::Insert(k, o) => wb.insert(k, o),
                Mutation::Remove(k, o) => wb.remove(k, o),
                Mutation::InsertRange(k, lo, hi) => wb.insert_range(k, lo, hi),
                Mutation::RemoveRange(k, lo, hi) => wb.remove_range(k, lo, hi),
                Mutation::DeleteKey(k) => wb.delete_key(k),
                Mutation::StoreSet(k, ref s) => wb.store_set(k, s),
            };
        }
        let c = wb.commit().map_err(|e| db_err("batch_commit", e))?;
        Ok(dict(vec![
            ("version", int_obj(c.version)),
            ("changed", int_obj(c.changed)),
            ("shards", int_obj(c.shards as u64)),
        ]))
    }

    fn stage(&mut self, b: usize, verb: &str, m: Mutation) -> Result<MontyObject, MontyException> {
        let i = self.slot(b, HandleKind::Batch, verb)?;
        match self.batches[i] {
            None => Err(stale_handle(verb, "batch", b)),
            Some((_, ref mut muts)) => {
                muts.push(m);
                Ok(MontyObject::None)
            }
        }
    }

    /// A directory inside the scenario's temporary root, refusing anything that
    /// would escape it.
    ///
    /// Shared by `db_open` and `repl_follower` so that a replica directory obeys
    /// exactly the same rule as a database one — which is what lets a scenario
    /// catch a follower up and then `db_open` it by the same name.
    pub(crate) fn scenario_path(&self, name: &str, verb: &str) -> Result<PathBuf, MontyException> {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
            return Err(value_err(format!(
                "{verb}(): '{name}' must be a plain directory name inside the scenario's temporary root"
            )));
        }
        Ok(self.root.join(name))
    }

    /// A `Db` clone for a handle, for a service that needs to own one.
    ///
    /// A clone shares the `Arc<DbInner>`, so it is the same store and the
    /// same directory lock — not a second database. Callers that keep one must
    /// pair it with [`World::hold_db_for_flight`].
    pub(crate) fn db_clone(&mut self, h: usize, verb: &str) -> Result<Db, MontyException> {
        Ok(self.db(h, verb)?.db.clone())
    }

    /// Record that something outside the handle table now holds a `Db` clone.
    pub(crate) fn hold_db_for_flight(
        &mut self,
        h: usize,
        verb: &str,
    ) -> Result<(), MontyException> {
        self.db(h, verb)?.live_flights += 1;
        Ok(())
    }

    /// Release such a hold. Silent when the database is already gone: a
    /// scenario that closed first has been told about it by `db_close`, and a
    /// second complaint here would name the wrong verb.
    pub(crate) fn release_db_for_flight(&mut self, h: usize) {
        if let Ok(i) = self.slot(h, HandleKind::Database, "flight_stop") {
            if let Some(Some(d)) = self.dbs.get_mut(i) {
                d.live_flights = d.live_flights.saturating_sub(1);
            }
        }
    }

    /// Where an open database lives and how many shards it actually has.
    ///
    /// The *actual* count, not `opts.shards`: since the MANIFEST began carrying
    /// it, reopening with a different request adopts the persisted number, and a
    /// leader service told the requested one would advertise shards that are not
    /// there.
    pub(crate) fn db_location(
        &mut self,
        h: usize,
        verb: &str,
    ) -> Result<(PathBuf, usize), MontyException> {
        let d = self.db(h, verb)?;
        Ok((d.dir.clone(), d.db.shard_count()))
    }

    // ---- handle resolution ------------------------------------------------

    fn db(&mut self, h: usize, verb: &str) -> Result<&mut OpenDb, MontyException> {
        let i = self.slot(h, HandleKind::Database, verb)?;
        self.dbs[i]
            .as_mut()
            .ok_or_else(|| stale_handle(verb, "database", h))
    }

    pub(crate) fn snap(&self, h: usize, verb: &str) -> Result<&Snapshot, MontyException> {
        let i = self.slot(h, HandleKind::Snapshot, verb)?;
        self.snaps[i]
            .as_ref()
            .map(|s| &s.snap)
            .ok_or_else(|| stale_handle(verb, "snapshot", h))
    }

    pub(crate) fn expr(&self, h: usize, verb: &str) -> Result<Expr, MontyException> {
        let i = self.slot(h, HandleKind::Query, verb)?;
        Ok(self.exprs[i].clone())
    }

    pub(crate) fn push_expr(&mut self, e: Expr) -> MontyObject {
        self.exprs.push(e);
        let idx = self.exprs.len() - 1;
        self.mint(HandleKind::Query, idx)
    }

    /// A set by handle. Sets are frozen and never invalidated, so the only
    /// failure is a handle this world never issued.
    pub(crate) fn set(&self, h: usize, verb: &str) -> Result<&Arc<OrdSet>, MontyException> {
        let i = self.slot(h, HandleKind::Set, verb)?;
        Ok(&self.sets[i])
    }

    pub(crate) fn push_set(&mut self, s: OrdSet) -> MontyObject {
        self.push_set_arc(Arc::new(s))
    }

    /// Interns an `Arc` that already exists, so handing a leaf's set back to a
    /// scenario is a refcount bump rather than a copy of every chunk.
    pub(crate) fn push_set_arc(&mut self, s: Arc<OrdSet>) -> MontyObject {
        self.sets.push(s);
        let idx = self.sets.len() - 1;
        self.mint(HandleKind::Set, idx)
    }

    pub(crate) fn container(&self, h: usize, verb: &str) -> Result<&Container, MontyException> {
        let i = self.slot(h, HandleKind::Container, verb)?;
        Ok(&self.containers[i])
    }

    pub(crate) fn push_container(&mut self, c: Container) -> MontyObject {
        self.containers.push(c);
        let idx = self.containers.len() - 1;
        self.mint(HandleKind::Container, idx)
    }

    /// `Option<Container>` -> a handle or Python `None`.
    ///
    /// The `None` arm is the point: `ops::and` returning `None` means the
    /// result is empty, and that is the condition every early-out in the
    /// migrated k-way walks tests.
    pub(crate) fn push_opt_container(&mut self, c: Option<Container>) -> MontyObject {
        c.map_or(MontyObject::None, |c| self.push_container(c))
    }

    /// Remove a database handle, refusing while a snapshot from it is alive.
    ///
    /// The refusal is the useful part. A `Snapshot` transitively holds the
    /// store open, so closing here and reopening the same directory would fail
    /// deep inside `Db::open` with a lock error, several statements away from
    /// the `db_snapshot` call that actually caused it.
    fn take_db(&mut self, h: usize, verb: &str) -> Result<OpenDb, MontyException> {
        let i = self.slot(h, HandleKind::Database, verb)?;
        match self.dbs.get(i) {
            None => return Err(bad_handle(verb, "database", h)),
            Some(None) => return Err(stale_handle(verb, "database", h)),
            Some(Some(d)) if d.live_snaps > 0 => {
                return Err(value_err(format!(
                    "{verb}(): database {h} still has {} live snapshot(s). A snapshot holds the \
                     directory's exclusive lock open; call snap_release() on each before closing.",
                    d.live_snaps
                )))
            }
            Some(Some(d)) if d.live_flights > 0 => {
                return Err(value_err(format!(
                    "{verb}(): database {h} is still served by {} Flight endpoint(s). The \
                     service holds a Db clone, and that clone holds the directory's exclusive \
                     lock open; call flight_stop() on each before closing.",
                    d.live_flights
                )))
            }
            Some(Some(_)) => {}
        }
        Ok(self.dbs[i].take().expect("checked present just above"))
    }

    fn take_batch(
        &mut self,
        h: usize,
        verb: &str,
    ) -> Result<(usize, Vec<Mutation>), MontyException> {
        let i = self.slot(h, HandleKind::Batch, verb)?;
        self.batches[i]
            .take()
            .ok_or_else(|| stale_handle(verb, "batch", h))
    }
}

fn check_range(verb: &str, lo: u64, hi: u64) -> Result<(), MontyException> {
    if lo > hi {
        return Err(value_err(format!(
            "{verb}(): range is inverted, lo={lo} > hi={hi}"
        )));
    }
    Ok(())
}

pub(crate) fn bad_handle(verb: &str, kind: &str, h: usize) -> MontyException {
    value_err(format!(
        "{verb}(): {h} is not a {kind} handle issued by this scenario"
    ))
}

pub(crate) fn stale_handle(verb: &str, kind: &str, h: usize) -> MontyException {
    value_err(format!(
        "{verb}(): {kind} handle {h} has already been closed or released"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NAMES` drives name resolution and the `match` in `call` drives
    /// behaviour. If they drift, a scenario calling the advertised verb gets a
    /// `TypeError` about an unknown verb — confusing, but at least loud. This
    /// test makes it impossible instead.
    ///
    /// Calling each verb with no arguments is enough to tell the two apart: a
    /// dispatched verb either succeeds (`db_open`, `q_empty` need no
    /// arguments) or complains about arity, while an undispatched one falls
    /// through to the catch-all arm.
    #[test]
    fn every_advertised_verb_is_dispatched() {
        let mut w = World::temporary().unwrap();
        for name in all_names() {
            if let Err(err) = w.call(name, &[], &[]) {
                assert!(
                    !err.summary().contains("is not a harness verb"),
                    "{name} is advertised in NAMES but has no arm in call()"
                );
            }
        }
    }

    /// No verb may be shadowed by a Python builtin, because monty resolves
    /// builtins without ever asking the host — the call would silently do
    /// something else entirely.
    #[test]
    fn no_verb_collides_with_a_python_builtin() {
        const BUILTINS: &[&str] = &[
            "abs",
            "all",
            "any",
            "bin",
            "bool",
            "bytes",
            "chr",
            "dict",
            "dir",
            "divmod",
            "enumerate",
            "filter",
            "float",
            "format",
            "frozenset",
            "getattr",
            "hasattr",
            "hash",
            "hex",
            "id",
            "input",
            "int",
            "isinstance",
            "issubclass",
            "iter",
            "len",
            "list",
            "map",
            "max",
            "min",
            "next",
            "object",
            "oct",
            "open",
            "ord",
            "pow",
            "print",
            "range",
            "repr",
            "reversed",
            "round",
            "set",
            "setattr",
            "slice",
            "sorted",
            "str",
            "sum",
            "tuple",
            "type",
            "vars",
            "zip",
        ];
        for name in all_names() {
            assert!(
                !BUILTINS.contains(name),
                "verb {name} collides with a Python builtin and would never reach the host"
            );
        }
    }

    /// Duplicates across the three lists, not merely within one. Two modules
    /// both claiming a name would make dispatch depend on the order `call`
    /// happens to test them in.
    #[test]
    fn names_are_unique() {
        let mut sorted: Vec<_> = all_names().to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(
            before,
            sorted.len(),
            "a verb name is claimed by more than one module"
        );
    }

    /// Every verb must be routed to the module that implements it. `call`
    /// tests the two `OWNS` lists and falls through to the database arm, so a
    /// verb listed in `eager::OWNS` but implemented in `world` — or the
    /// reverse — reaches a catch-all and reports itself as not a verb at all.
    #[test]
    fn each_verb_is_owned_by_exactly_one_module() {
        for name in crate::eager::OWNS {
            assert!(
                !NAMES.contains(name) && !crate::lazy::OWNS.contains(name),
                "{name} is claimed by eager and by another module"
            );
        }
        for name in crate::lazy::OWNS {
            assert!(
                !NAMES.contains(name),
                "{name} is claimed by lazy and by world"
            );
        }
    }

    /// `db_open`'s knobs must actually reach `DbOptions`. Both were silently
    /// ignorable: `shards` because the default is 8 and most scenarios want 8,
    /// and `evacuate` because 0 and the default both "work".
    #[test]
    fn db_open_keywords_reach_the_options() {
        let mut w = World::temporary().unwrap();
        let kw = [(
            MontyObject::String("shards".to_owned()),
            MontyObject::Int(3),
        )];
        let h = w.call("db_open", &[], &kw).unwrap();
        let MontyObject::Dict(stats) = w.call("db_stats", &[h], &[]).unwrap() else {
            panic!("db_stats must return a dict")
        };
        let shards = (&stats)
            .into_iter()
            .find(|(k, _)| matches!(k, MontyObject::String(s) if s == "shards"))
            .map(|(_, v)| v.clone());
        assert_eq!(shards, Some(MontyObject::Int(3)));

        // 0 is a meaningful setting, not an out-of-range one.
        let kw = [(
            MontyObject::String("evacuate".to_owned()),
            MontyObject::Int(0),
        )];
        assert!(w.call("db_open", &[], &kw).is_ok());
    }

    /// A knob absent from the runner must fall back to the scenario's default,
    /// so `cargo test` runs the gate-sized corpus.
    #[test]
    fn an_absent_arg_falls_back_to_the_default() {
        let mut w = World::temporary().unwrap();
        let got = w
            .call(
                "yn_arg",
                &[MontyObject::String("keys".to_owned()), MontyObject::Int(7)],
                &[],
            )
            .unwrap();
        assert_eq!(got, MontyObject::Int(7));

        let mut w = World::temporary_with(vec![("keys".to_owned(), 400)]).unwrap();
        let got = w
            .call(
                "yn_arg",
                &[MontyObject::String("keys".to_owned()), MontyObject::Int(7)],
                &[],
            )
            .unwrap();
        assert_eq!(got, MontyObject::Int(400));
    }

    /// A mistyped constant must raise. Returning 0 would silently rescale a
    /// fixture built on `CHUNK_CARD`.
    #[test]
    fn an_unknown_constant_is_refused() {
        let mut w = World::temporary().unwrap();
        assert_eq!(
            w.call(
                "yn_const",
                &[MontyObject::String("CHUNK_CARD".to_owned())],
                &[]
            )
            .unwrap(),
            MontyObject::Int(65536)
        );
        assert!(w
            .call(
                "yn_const",
                &[MontyObject::String("CHUNK_KARD".to_owned())],
                &[]
            )
            .is_err());
    }

    /// A handle of the wrong kind must raise, not resolve into another table.
    ///
    /// This is a regression test for a real defect, not a hypothetical. With
    /// one `Vec` per kind and each starting at index 0, `q_cardinality( a_set
    /// handle )` found *query* 0 and returned its cardinality — a different
    /// object, a plausible answer, no error. The first set and the first query
    /// both being handle 0 is the **normal** case, so this was not a corner.
    #[test]
    fn a_handle_of_the_wrong_kind_is_refused() {
        let mut w = World::temporary().unwrap();
        let list = MontyObject::List(vec![MontyObject::Int(1), MontyObject::Int(2)]);
        let s = w.call("set_of", &[list], &[]).unwrap();
        let q = w.call("q_set", std::slice::from_ref(&s), &[]).unwrap();
        // The two really do collide as bare integers; that is the point.
        assert_eq!(s, MontyObject::Int(0));
        assert_eq!(q, MontyObject::Int(1));

        let err = w
            .call("q_cardinality", std::slice::from_ref(&s), &[])
            .unwrap_err();
        assert!(
            err.summary().contains("is a set, not a query"),
            "{}",
            err.summary()
        );
        let err = w
            .call("set_len", std::slice::from_ref(&q), &[])
            .unwrap_err();
        assert!(
            err.summary().contains("is a query, not a set"),
            "{}",
            err.summary()
        );
        // And the right-kind calls still work, so the tag is not simply
        // refusing everything.
        assert_eq!(w.call("set_len", &[s], &[]).unwrap(), MontyObject::Int(2));
        assert_eq!(
            w.call("q_cardinality", &[q], &[]).unwrap(),
            MontyObject::Int(2)
        );
    }

    /// Handles are minted from one counter, so no two kinds share a value.
    #[test]
    fn handles_are_unique_across_every_kind() {
        let mut w = World::temporary().unwrap();
        let db = w.call("db_open", &[], &[]).unwrap();
        let query = w.call("q_empty", &[], &[]).unwrap();
        let seen = vec![
            db.clone(),
            w.call("sb_new", &[], &[]).unwrap(),
            w.call("set_of", &[MontyObject::List(vec![])], &[]).unwrap(),
            query.clone(),
            w.call("ct_full", &[], &[]).unwrap(),
            w.call("db_snapshot", std::slice::from_ref(&db), &[])
                .unwrap(),
            w.call("batch", std::slice::from_ref(&db), &[]).unwrap(),
            w.call("st_open", std::slice::from_ref(&query), &[])
                .unwrap(),
        ];
        let mut sorted: Vec<i64> = seen
            .iter()
            .map(|h| match h {
                MontyObject::Int(v) => *v,
                other => panic!("a handle must be an int, got {other:?}"),
            })
            .collect();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            seen.len(),
            "two kinds share a handle value: {seen:?}"
        );
    }

    /// A closed handle must raise rather than resolve to something plausible.
    #[test]
    fn a_closed_database_handle_is_refused() {
        let mut w = World::temporary().unwrap();
        let h = w.call("db_open", &[], &[]).unwrap();
        w.call("db_close", std::slice::from_ref(&h), &[]).unwrap();
        let err = w
            .call(
                "db_insert",
                &[h.clone(), MontyObject::Int(1), MontyObject::Int(1)],
                &[],
            )
            .unwrap_err();
        assert!(
            err.summary().contains("already been closed"),
            "{}",
            err.summary()
        );
    }

    /// Closing under a live snapshot must name the real cause rather than
    /// failing later with a lock error from `db_open`.
    #[test]
    fn closing_under_a_live_snapshot_names_the_snapshot() {
        let mut w = World::temporary().unwrap();
        let h = w.call("db_open", &[], &[]).unwrap();
        let _s = w
            .call("db_snapshot", std::slice::from_ref(&h), &[])
            .unwrap();
        let err = w
            .call("db_close", std::slice::from_ref(&h), &[])
            .unwrap_err();
        assert!(err.summary().contains("live snapshot"), "{}", err.summary());
    }

    /// The same hazard through a different door.
    ///
    /// A `YesnoFlightService` holds a `Db` clone, and a `Db` clone holds the
    /// directory's exclusive lock — so closing underneath one leaves the next
    /// `db_open` of that directory failing with `AlreadyOpen`, several
    /// statements away from the `flight_serve` that caused it. Refusing here is
    /// what turns that into a message naming the actual mistake.
    #[test]
    fn closing_under_a_live_flight_service_names_the_service() {
        let mut w = World::temporary().unwrap();
        let h = w.call("db_open", &[], &[]).unwrap();
        let f = w
            .call("flight_serve", std::slice::from_ref(&h), &[])
            .unwrap();
        let err = w
            .call("db_close", std::slice::from_ref(&h), &[])
            .unwrap_err();
        assert!(
            err.summary().contains("Flight endpoint"),
            "{}",
            err.summary()
        );

        // And stopping releases the hold, so the refusal is a guard rather than
        // a trap: without this half the test would pass on a counter that only
        // ever goes up.
        w.call("flight_stop", std::slice::from_ref(&f), &[])
            .unwrap();
        w.call("db_close", std::slice::from_ref(&h), &[]).unwrap();
    }

    /// The retained-directory name must carry the scenario and sort by age.
    ///
    /// Checked here rather than by inspecting `/tmp`, because a scenario's
    /// root is dropped even when the scenario *fails* — the retention this name
    /// exists for happens on paths that skip the drop entirely, which a unit test
    /// cannot arrange.
    #[test]
    fn a_scenario_root_is_named_after_the_scenario_that_owns_it() {
        let prefix = super::scenario_prefix("e2e/scenarios/pitr_retention.py");
        assert!(
            prefix.starts_with("yesno-e2e-pitr-retention-"),
            "the scenario must be greppable in the name, got {prefix:?}"
        );
        let stamp: u64 = prefix
            .trim_end_matches('-')
            .rsplit('-')
            .next()
            .unwrap()
            .parse()
            .expect("the name must end in epoch seconds so it sorts by age");
        assert!(stamp > 1_700_000_000, "implausible timestamp {stamp}");

        // A path separator or a dot in the label must not become a directory
        // separator or hide the name — the whole point is a *findable* directory.
        let nasty = super::scenario_prefix("../weird name/x.py");
        assert!(
            !nasty.contains('/') && !nasty.contains('.'),
            "the label must be sanitized, got {nasty:?}"
        );

        // And it must actually be usable as a directory prefix.
        let world = World::temporary_labelled("lifecycle.py", Vec::new()).unwrap();
        let name = world
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_owned();
        assert!(
            name.starts_with("yesno-e2e-lifecycle-"),
            "the created root is named {name:?}"
        );
    }
}
