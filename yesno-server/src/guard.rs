//! What a caller may do, checked per RPC.
//!
//! # Why this is a service wrapper and not a layer
//!
//! It **cannot** be an interceptor: tonic hands one a `Request<()>` with the
//! URI stripped, so an interceptor does not know which RPC it is guarding. See
//! [`crate::auth`].
//!
//! And it cannot be a `tower::Layer` either, which is the less obvious half.
//! A layer does see the URI — but `do_action( "stats" )` and
//! `do_action( "clear" )` arrive on the **same** URI,
//! `/arrow.flight.protocol.FlightService/DoAction`, and they are a read and an
//! admin operation. No URI-based check can separate them, so the decision has to
//! happen where the action name is already parsed.
//!
//! So: nine short methods, each pulling the [`Principal`] out of the extensions,
//! checking one permission, and delegating. Nine methods is more typing than a
//! layer and it is the right shape — an authorization table you can read top to
//! bottom in one screen is the property worth having here.
//!
//! **`yesno-flight` is untouched by this.** Its "no authentication handshake
//! in v1" contract stays honest, and policy lives in the crate that owns the
//! deployment's configuration.

use arrow_flight::flight_service_server::FlightService;
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use std::sync::Arc;

use futures::stream::BoxStream;
use tonic::{Request, Response, Status, Streaming};
use yesno_core::Db;
use yesno_flight::YesnoFlightService;

use crate::auth::{Perm, Principal};

/// Request metadata: the lowest leadership term a caller will accept.
pub const EXPECT_TERM: &str = "yesno-expect-term";
/// Response metadata: the term this server is serving at.
pub const TERM: &str = "yesno-term";

/// The database a listener is currently serving, if any.
///
/// **A slot rather than a handle, and only a live replica needs it to be.** A
/// leader's database is fixed for the life of its process. A replica that falls
/// off its leader's retained log has to **close** the database to re-bootstrap —
/// `bootstrap_shard` truncates the shard image, and truncating under a live
/// mapping raises `SIGBUS`, which I6 states is not catchable as a `Result` — so
/// there is a real interval with no database at all. Without a slot the port
/// would have to close with it, dropping every connection for a condition the
/// caller could simply have been told about.
pub type DbSlot = Arc<std::sync::RwLock<Option<Arc<Db>>>>;

pub fn slot_of(db: Arc<Db>) -> DbSlot {
    Arc::new(std::sync::RwLock::new(Some(db)))
}

#[derive(Clone)]
pub struct GuardedFlight {
    db: DbSlot,
    version: &'static str,
}

impl GuardedFlight {
    pub fn new(db: DbSlot) -> GuardedFlight {
        GuardedFlight {
            db,
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// The service and the term to answer with, or a refusal saying why not.
    ///
    /// Read **per request**, and that is a change from the fixed-handle
    /// version: it used to be captured once on the argument that
    /// `promote_database` refuses while the database is open, so a term could
    /// not move under a running server. With a slot the database itself can be
    /// replaced — a replica rebuilds and reopens — so the term must be asked of
    /// whatever is in the slot now. Building the service is an `Arc` clone.
    fn current(&self) -> Result<(YesnoFlightService, u32), Status> {
        let g = self
            .db
            .read()
            .map_err(|_| Status::internal("the database slot is poisoned"))?;
        match g.as_ref() {
            Some(db) => Ok((YesnoFlightService::new(db.clone()), db.term())),
            None => Err(Status::unavailable(
                "this node is rebuilding its copy of the database from its leader and is \
                 not serving. Retry, or read from the leader.",
            )),
        }
    }

    /// Refuse a caller that has seen a **newer** leadership than this server is.
    ///
    /// This is the half of split-brain fencing that faces clients rather than
    /// followers. A superseded leader does not know it has been replaced —
    /// nothing tells it — so it will happily accept writes that vanish when it
    /// is wiped and rebuilt. The caller is the only party that can hold the
    /// newer number, so the caller has to be the one to assert it.
    ///
    /// Honest about its reach: it catches a client that has **already seen**
    /// the new leadership and then reaches the old one — a stale connection in a
    /// pool, a retry against a cached address. A client that has only ever
    /// talked to the zombie cannot know, and nothing here can tell it.
    ///
    /// Only `self.term < want` is an error. A server *ahead* of the caller is
    /// the normal case after a failover; the response header teaches the caller
    /// the higher number rather than refusing it.
    fn check_term<T>(&self, r: &Request<T>, term: u32) -> Result<(), Status> {
        let Some(v) = r.metadata().get(EXPECT_TERM) else {
            return Ok(());
        };
        let want: u32 = v
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| {
                Status::invalid_argument(format!("`{EXPECT_TERM}` must be a decimal number"))
            })?;
        if term < want {
            return Err(Status::failed_precondition(format!(
                "this server is serving leadership term {term} and the caller requires at \
                 least {want}. It is an older leadership of the same database — a leader \
                 that was replaced — so writes accepted here would be lost. Find the \
                 current leader."
            )));
        }
        Ok(())
    }

    /// Put this server's term on the way out, so a caller learns it without
    /// having to ask.
    ///
    /// On **every** response, not just on a whoami. A client only builds the
    /// monotonic memory that protects it from the values it has actually seen,
    /// and a client that only ever calls `do_get` would otherwise never see one.
    fn stamp<T>(&self, mut r: Response<T>, term: u32) -> Response<T> {
        if let Ok(v) = term.to_string().parse() {
            r.metadata_mut().insert(TERM, v);
        }
        r
    }

    /// The caller, or `unauthenticated` if the interceptor did not seat one.
    ///
    /// The `None` arm is unreachable when the service is installed behind the
    /// interceptor, and is a refusal rather than a `expect` because the failure
    /// mode of getting the wiring wrong must be "nobody can do anything" and not
    /// "everybody can do everything".
    pub(crate) fn who<T>(r: &Request<T>) -> Result<&Principal, Status> {
        r.extensions()
            .get::<Principal>()
            .ok_or_else(|| Status::unauthenticated("no principal on this request"))
    }

    fn require<T>(&self, r: &Request<T>, p: Perm, term: u32) -> Result<(), Status> {
        // Before the permission check, deliberately. A caller reaching a
        // superseded leader has a *worse* problem than a missing permission, and
        // telling it "you may not" would send it looking in the wrong place.
        self.check_term(r, term)?;
        let who = Self::who(r)?;
        if who.may(p) {
            return Ok(());
        }
        // `permission_denied`, never `unauthenticated`. The caller proved who
        // they are and simply may not; telling them otherwise invites an
        // infinite retry with a credential that will never work.
        Err(Status::permission_denied(format!(
            "`{}` is a {:?} and this call needs {:?}",
            who.name, who.role, p
        )))
    }
}

#[tonic::async_trait]
impl FlightService for GuardedFlight {
    type HandshakeStream = BoxStream<'static, Result<HandshakeResponse, Status>>;
    type ListFlightsStream = BoxStream<'static, Result<FlightInfo, Status>>;
    type DoGetStream = BoxStream<'static, Result<FlightData, Status>>;
    type DoPutStream = BoxStream<'static, Result<PutResult, Status>>;
    type DoExchangeStream = BoxStream<'static, Result<FlightData, Status>>;
    type DoActionStream = BoxStream<'static, Result<arrow_flight::Result, Status>>;
    type ListActionsStream = BoxStream<'static, Result<ActionType, Status>>;

    /// A whoami probe, and deliberately nothing more.
    ///
    /// The service beneath answers `unimplemented` here. This one answers the
    /// only question a handshake can usefully answer without state: *who does
    /// this server think I am*. Reaching it at all requires a credential the
    /// interceptor accepted, so it needs no permission of its own — that is the
    /// point of it.
    ///
    /// Explicitly **not** a token exchange. The classic Flight handshake
    /// trades a password for a short-lived session token, which needs a session
    /// table: in memory it dies on restart and does not survive a failover, and
    /// anything better is shared state this project does not have. It buys
    /// little over a per-call bearer token, which is what modern deployments use
    /// anyway.
    async fn handshake(
        &self,
        r: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        let (_svc, term) = self.current()?;
        self.check_term(&r, term)?;
        let who = Self::who(&r)?;
        let body = format!(
            "{{\"principal\":\"{}\",\"role\":\"{:?}\",\"server\":\"yesnod/{}\",\"term\":{}}}",
            who.name, who.role, self.version, term
        )
        .into_bytes();
        let out = futures::stream::once(async move {
            Ok(HandshakeResponse {
                protocol_version: 0,
                payload: body.into(),
            })
        });
        Ok(self.stamp(Response::new(Box::pin(out)), term))
    }

    async fn list_flights(
        &self,
        r: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        let (svc, term) = self.current()?;
        self.require(&r, Perm::Read, term)?;
        svc.list_flights(r).await.map(|r| self.stamp(r, term))
    }

    async fn get_flight_info(
        &self,
        r: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let (svc, term) = self.current()?;
        self.require(&r, Perm::Read, term)?;
        svc.get_flight_info(r).await.map(|r| self.stamp(r, term))
    }

    async fn poll_flight_info(
        &self,
        r: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        let (svc, term) = self.current()?;
        self.require(&r, Perm::Read, term)?;
        svc.poll_flight_info(r).await.map(|r| self.stamp(r, term))
    }

    async fn get_schema(
        &self,
        r: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        let (svc, term) = self.current()?;
        self.require(&r, Perm::Read, term)?;
        svc.get_schema(r).await.map(|r| self.stamp(r, term))
    }

    async fn do_get(&self, r: Request<Ticket>) -> Result<Response<Self::DoGetStream>, Status> {
        let (svc, term) = self.current()?;
        self.require(&r, Perm::Read, term)?;
        svc.do_get(r).await.map(|r| self.stamp(r, term))
    }

    async fn do_put(
        &self,
        r: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        let (svc, term) = self.current()?;
        self.require(&r, Perm::Write, term)?;
        svc.do_put(r).await.map(|r| self.stamp(r, term))
    }

    async fn do_exchange(
        &self,
        r: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        let (svc, term) = self.current()?;
        self.require(&r, Perm::Write, term)?;
        svc.do_exchange(r).await.map(|r| self.stamp(r, term))
    }

    /// **The method the URI cannot distinguish.** `stats` is a read while
    /// mutation actions require administrator permission; both arrive here.
    async fn do_action(
        &self,
        r: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        let needed = match r.get_ref().r#type.as_str() {
            "stats" => Perm::Read,
            // Everything else, including an action this build does not know, is
            // admin. Default-deny by shape: a new action added to
            // `yesno-flight` later lands in the strictest bucket until someone
            // deliberately moves it, rather than inheriting whatever the
            // fall-through happened to be.
            _ => Perm::Admin,
        };
        let (svc, term) = self.current()?;
        self.require(&r, needed, term)?;
        svc.do_action(r).await.map(|r| self.stamp(r, term))
    }

    async fn list_actions(
        &self,
        r: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        let (svc, term) = self.current()?;
        self.require(&r, Perm::Read, term)?;
        svc.list_actions(r).await.map(|r| self.stamp(r, term))
    }
}
