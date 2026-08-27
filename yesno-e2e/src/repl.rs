//! The replication half of the verb surface: a real leader, a real follower,
//! a real socket.
//!
//! # Why this exists, and why it is not what `TODO.md` said it would cost
//!
//! `yesno-e2e` covered `yesno-core` only. The operational sequence that matters
//! most in M7 — stand a leader up, bootstrap a follower from its image, ship the
//! log, ask whether the two hold the same sets — was scripted nowhere, and the
//! recorded reason was that both M7 crates are `async`, so the harness "would
//! need monty's async host-call path ( `FunctionCall::resume_pending` ->
//! `RunProgress::ResolveFutures` ) rather than the synchronous `resume` it uses
//! today".
//!
//! **That is only true of a scenario that wants to await two things at
//! once.** A host call is synchronous *from the VM's side*: monty calls out, the
//! host does whatever it likes, the host returns a value. Nothing stops the host
//! from owning a runtime and blocking in it. `ResolveFutures` is what you need
//! to suspend the *interpreter* on a future, and a scripted operational sequence
//! — write, serve, bootstrap, catch up, compare — never wants that. It wants
//! each step finished before the next line runs, which is exactly what
//! `Handle::block_on` gives.
//!
//! So the harness owns one multi-threaded runtime, created on the first
//! `repl_*` call and never otherwise. A scenario that does not replicate does
//! not pay for it.
//!
//! # What a scenario gets, and what it must not become
//!
//! The verbs here are transport and bookkeeping only: they drive the *shipped*
//! [`LeaderService`] and the *shipped* [`FollowerClient`]. There is
//! deliberately no verb that hands a scenario WAL bytes to move itself. A
//! scenario shipping frames in Python would be a second follower written in the
//! scenario language, and `TESTING.md` §4 is about what happens then: the
//! assertion ends up phrased as a property of the Python walk, and an unrelated
//! change to that walk "fixes" it.
//!
//! The comparison, on the other hand, belongs in Python and is the whole payoff.
//! A replica is a directory under the scenario's root, so once it has caught up
//! the scenario opens it with the ordinary `db_open` and every existing
//! `snap_*` verb applies. Set equality is then `snap_load( a, k ) ==
//! snap_load( b, k )` against Python's own `list` — an oracle rather than a
//! second implementation of it.
//!
//! # Two things the verbs refuse to smooth over
//!
//! * **Seeding the MANIFEST is its own verb.** A follower needs the leader's
//!   MANIFEST — not just its identity, but the shard count and the
//!   `vshard -> shard` map, without which it would look for keys in shards that
//!   do not hold them. Folding the copy into `repl_follower` would hide the one
//!   step an operator has to get right.
//! * **`repl_catch_up` takes an explicit `after_lsn`.** Resuming from zero and
//!   resuming from a cursor are different operations with different failure
//!   modes, and a verb that picked for you would make the difference
//!   unscriptable.

use std::path::PathBuf;

use monty_types::{MontyException, MontyObject};
use tonic::transport::{Channel, Server};
use yesno_server::replication::pb::replication_client::ReplicationClient;
use yesno_server::replication::pb::replication_server::ReplicationServer;
use yesno_server::replication::{pb, FollowerClient, LeaderService};

use crate::convert::{db_err, dict, int_obj, opt_int_obj, tuple, type_err, value_err, Args};
use crate::world::{stale_handle, HandleKind, World};

/// The verbs this module dispatches. Merged into [`crate::world::NAMES`].
pub const OWNS: &[&str] = &[
    "repl_serve",
    "repl_stop",
    "repl_status",
    "repl_follower",
    "repl_seed",
    "repl_bootstrap",
    "repl_catch_up",
    "repl_visible",
    "repl_cursor",
    "repl_ack",
];

/// A leader service on loopback, plus a client connected to it.
struct Leader {
    client: ReplicationClient<Channel>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    dir: PathBuf,
}

/// A follower's local directory and its shipped client.
struct Replica {
    client: FollowerClient,
    dir: PathBuf,
}

/// Everything the `repl_*` verbs own, kept out of `World`'s own fields so that
/// `world.rs` does not have to know tonic exists.
#[derive(Default)]
pub struct ReplState {
    /// Created on the first `repl_*` call. A scenario that never replicates
    /// never starts a runtime.
    rt: Option<tokio::runtime::Runtime>,
    leaders: Vec<Option<Leader>>,
    followers: Vec<Option<Replica>>,
}

impl World {
    /// A runtime handle, starting the runtime if this is the first call.
    ///
    /// Returns an owned [`tokio::runtime::Handle`] rather than a borrow of the
    /// runtime on purpose: every verb below needs the handle *and* a mutable
    /// borrow of a leader or follower at the same time, and a borrow of
    /// `self.repl.rt` would rule that out. Cloning a handle is an `Arc` bump.
    pub(crate) fn rt_handle(
        &mut self,
        verb: &str,
    ) -> Result<tokio::runtime::Handle, MontyException> {
        if self.repl.rt.is_none() {
            // Multi-threaded because the server runs as a spawned task while the
            // verb blocks on a client call. A current-thread runtime would
            // deadlock: nothing would drive the server while `block_on` held the
            // only thread.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| db_err(verb, e))?;
            self.repl.rt = Some(rt);
        }
        Ok(self.repl.rt.as_ref().unwrap().handle().clone())
    }

    fn leader(&mut self, h: usize, verb: &str) -> Result<&mut Leader, MontyException> {
        let i = self.slot(h, HandleKind::Leader, verb)?;
        self.repl.leaders[i]
            .as_mut()
            .ok_or_else(|| stale_handle(verb, "leader", h))
    }

    fn replica(&mut self, h: usize, verb: &str) -> Result<&mut Replica, MontyException> {
        let i = self.slot(h, HandleKind::Follower, verb)?;
        self.repl.followers[i]
            .as_mut()
            .ok_or_else(|| stale_handle(verb, "follower", h))
    }

    /// Borrow a follower and a leader at once, which every shipping verb needs.
    ///
    /// Two `&mut` borrows out of the same `World`, so it goes through raw
    /// indices and `split_at_mut`-free reasoning: the two live in *different*
    /// `Vec`s, so the compiler accepts the pair only if they are taken from
    /// disjoint fields. Resolving both handles first keeps the error messages
    /// pointing at the right argument.
    fn pair(
        &mut self,
        f: usize,
        l: usize,
        verb: &str,
    ) -> Result<(&mut Replica, &mut ReplicationClient<Channel>), MontyException> {
        let fi = self.slot(f, HandleKind::Follower, verb)?;
        let li = self.slot(l, HandleKind::Leader, verb)?;
        if self.repl.followers[fi].is_none() {
            return Err(stale_handle(verb, "follower", f));
        }
        if self.repl.leaders[li].is_none() {
            return Err(stale_handle(verb, "leader", l));
        }
        let ReplState {
            leaders, followers, ..
        } = &mut self.repl;
        Ok((
            followers[fi].as_mut().unwrap(),
            &mut leaders[li].as_mut().unwrap().client,
        ))
    }

    pub(crate) fn call_repl(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            // Stand a leader service over an open database's directory.
            //
            // The service reads files and deliberately does not retain a
            // `Db`, so the leader stays independently open and writable. It
            // does share the database's `RetentionFloor`: without that one
            // piece of coordination an E2E ack could report lag but could not
            // protect the WAL bytes the follower still needs.
            "repl_serve" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let db_handle = a.handle(0)?;
                let retention = self.db_clone(db_handle, verb)?.retention_floor();
                let (dir, shards) = self.db_location(db_handle, verb)?;
                let rt = self.rt_handle(verb)?;
                let svc = LeaderService::with_retention(&dir, shards, retention);
                let (client, shutdown) = rt
                    .block_on(async move {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                        let addr = listener.local_addr()?;
                        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                        tokio::spawn(async move {
                            let _ = Server::builder()
                                .add_service(ReplicationServer::new(svc))
                                .serve_with_incoming_shutdown(
                                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                                    async {
                                        let _ = rx.await;
                                    },
                                )
                                .await;
                        });
                        let client = ReplicationClient::connect(format!("http://{addr}"))
                            .await
                            .map_err(std::io::Error::other)?;
                        Ok::<_, std::io::Error>((client, tx))
                    })
                    .map_err(|e| db_err(verb, e))?;
                self.repl.leaders.push(Some(Leader {
                    client,
                    shutdown: Some(shutdown),
                    dir,
                }));
                let idx = self.repl.leaders.len() - 1;
                Ok(self.mint(HandleKind::Leader, idx))
            }

            // Shut the service down. Invalidating rather than merely stopping,
            // so a scenario that keeps using the handle says so loudly.
            "repl_stop" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let i = self.slot(h, HandleKind::Leader, verb)?;
                match self.repl.leaders[i].take() {
                    None => Err(stale_handle(verb, "leader", h)),
                    Some(mut l) => {
                        if let Some(tx) = l.shutdown.take() {
                            let _ = tx.send(());
                        }
                        Ok(MontyObject::None)
                    }
                }
            }

            // The handshake, as a dict. `uuid` is a tuple of bytes so that two
            // leaders can be told apart from a scenario without the harness
            // deciding what "different" means.
            "repl_status" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let rt = self.rt_handle(verb)?;
                let client = &mut self.leader(a.handle(0)?, verb)?.client;
                let st = rt
                    .block_on(client.status(pb::StatusRequest {}))
                    .map_err(|e| db_err(verb, e))?
                    .into_inner();
                Ok(dict(vec![
                    ("shards", int_obj(u64::from(st.shard_count))),
                    (
                        "end_lsn",
                        tuple(st.end_lsn.iter().map(|&l| int_obj(l)).collect()),
                    ),
                    (
                        "uuid",
                        tuple(st.db_uuid.iter().map(|&b| int_obj(u64::from(b))).collect()),
                    ),
                ]))
            }

            // A replica directory under the scenario's root, empty. It is a
            // plain directory name for one reason: once the follower has caught
            // up, `db_open( name )` opens it as an ordinary database and every
            // `snap_*` verb applies, which is what makes the leader/follower
            // comparison Python's own rather than the harness's.
            "repl_follower" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let name = a.str_at(0)?.to_owned();
                let dir = self.scenario_path(&name, verb)?;
                std::fs::create_dir_all(&dir).map_err(|e| db_err(verb, e))?;
                self.repl.followers.push(Some(Replica {
                    client: FollowerClient::new(&dir, 0, []),
                    dir,
                }));
                let idx = self.repl.followers.len() - 1;
                Ok(self.mint(HandleKind::Follower, idx))
            }

            // Hand the follower the leader's MANIFEST.
            //
            // Its own verb, and not folded into `repl_follower`, because it
            // is the step an operator gets wrong. The MANIFEST carries the shard
            // count and the `vshard -> shard` map as well as the identity; a
            // follower given only the identity routes keys to shards that do not
            // hold them, and a follower given nothing invents its own.
            "repl_seed" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (fh, lh) = (a.handle(0)?, a.handle(1)?);
                let from = self.leader(lh, verb)?.dir.join("MANIFEST");
                let to = self.replica(fh, verb)?.dir.join("MANIFEST");
                std::fs::copy(&from, &to).map_err(|e| db_err(verb, e))?;
                Ok(MontyObject::None)
            }

            // Copy one shard's base image, and learn where replay must begin.
            "repl_bootstrap" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (fh, lh, shard) = (a.handle(0)?, a.handle(1)?, a.u64(2)?);
                let shard = shard_arg(verb, shard)?;
                let rt = self.rt_handle(verb)?;
                let (f, client) = self.pair(fh, lh, verb)?;
                let off = rt
                    .block_on(f.client.bootstrap_shard(client, shard))
                    .map_err(|e| db_err(verb, e))?;
                Ok(int_obj(off))
            }

            // Ship one shard's log from `after_lsn` until the leader reports
            // itself caught up, and say what moved.
            //
            // `max_bytes` of 0 means the service's default batch size; a small
            // value forces the publisher's record-boundary cut, which is a
            // property a scenario should be able to exercise on a real log.
            "repl_catch_up" => {
                a.exact(5)?;
                a.no_kwargs()?;
                let (fh, lh) = (a.handle(0)?, a.handle(1)?);
                let shard = shard_arg(verb, a.u64(2)?)?;
                let after = a.u64(3)?;
                let max = a.u64(4)?;
                let max = u32::try_from(max).map_err(|_| {
                    value_err(format!("{verb}(): max_bytes {max} does not fit in a u32"))
                })?;
                let rt = self.rt_handle(verb)?;
                let (f, client) = self.pair(fh, lh, verb)?;
                let moved = rt
                    .block_on(f.client.catch_up_shard(client, shard, after, max))
                    .map_err(|e| db_err(verb, e))?;
                Ok(dict(vec![
                    ("shard", int_obj(u64::from(moved.shard))),
                    ("bytes", int_obj(moved.bytes)),
                    ("records", int_obj(moved.records)),
                    ("next_lsn", int_obj(moved.next_lsn)),
                ]))
            }

            // The follower's watermark — versions at or below it are complete.
            //
            // Not "how much log arrived". A multi-shard commit counts only
            // once every participant's records are in hand, so this is the one
            // number that can tell a half-shipped batch from a whole one.
            "repl_visible" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let v = self.replica(a.handle(0)?, verb)?.client.visible();
                Ok(int_obj(v))
            }

            // Where a reconnect would resume this shard, or `None` if the
            // follower has never seen it.
            "repl_cursor" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (fh, shard) = (a.handle(0)?, a.u64(1)?);
                let shard = shard_arg(verb, shard)?;
                let c = self.replica(fh, verb)?.client.cursor(shard);
                Ok(opt_int_obj(c.map(|c| c.next_lsn)))
            }

            // Tell the leader how far this follower has applied, and learn the
            // lag it reports — in bytes of un-applied log, which is what drives
            // its retention floor.
            "repl_ack" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (fh, lh) = (a.handle(0)?, a.handle(1)?);
                let shard = shard_arg(verb, a.u64(2)?)?;
                let rt = self.rt_handle(verb)?;
                let (f, client) = self.pair(fh, lh, verb)?;
                let lag = rt
                    .block_on(f.client.ack(client, shard))
                    .map_err(|e| db_err(verb, e))?;
                Ok(int_obj(lag))
            }

            _ => Err(type_err(format!("{verb}() is not a replication verb"))),
        }
    }
}

fn shard_arg(verb: &str, s: u64) -> Result<u32, MontyException> {
    u32::try_from(s).map_err(|_| value_err(format!("{verb}(): shard {s} is not a shard id")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime is lazy, and staying lazy is the argument for putting it
    /// here rather than in `World::temporary`.
    #[test]
    fn a_world_that_never_replicates_starts_no_runtime() {
        let mut w = World::temporary().unwrap();
        w.call("db_open", &[], &[]).unwrap();
        assert!(
            w.repl.rt.is_none(),
            "a scenario with no repl_* call must not pay for a tokio runtime"
        );
    }

    #[test]
    fn a_shard_id_that_does_not_fit_is_refused() {
        assert!(shard_arg("repl_catch_up", u64::from(u32::MAX) + 1).is_err());
        assert_eq!(shard_arg("repl_catch_up", 3).unwrap(), 3);
    }
}
