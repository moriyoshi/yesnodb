//! Who is calling, plus ordered authorization for the shared control endpoint.
//! Flight method authorization remains in [`crate::guard`].
//!
//! # Why authentication and authorization are separate layers
//!
//! **A tonic `Interceptor` cannot see which RPC it is guarding.** This is
//! mechanical, not stylistic: `InterceptedService::call` extracts the URI,
//! method and version, hands the interceptor a `Request<()>` carrying only
//! metadata and extensions, and reattaches them afterwards. tonic's own comment
//! says it — *"Tonic requests do not preserve the URI, HTTP version, and HTTP
//! method of the HTTP request"*.
//!
//! A `tower::Layer` on the server *does* see the URI, and still cannot do the
//! job: `do_action( "stats" )` and `do_action( "clear" )` arrive on the **same
//! URI** and are a read and an admin operation respectively.
//!
//! So this layer answers "who", puts a [`Principal`] in the request extensions,
//! and stops. The service wrapper answers "may they".
//!
//! # Two credentials, both optional per deployment
//!
//! * **mTLS**, identified by the SHA-256 of the client certificate's DER leaf.
//!   A fingerprint rather than a Subject CN: matching a CN needs an X.509
//!   parser and CN-versus-SAN is a footgun with a long history. Rotating a
//!   certificate becomes a config edit, which is the honest trade.
//! * **A bearer token** in `authorization`, matched by digest.
//!
//! When both are present mTLS wins, and a *disagreement* between them is
//! refused rather than silently resolved — a caller presenting one identity's
//! certificate and another's token is a situation with no good interpretation.

use std::sync::Arc;

use ring::digest::{digest, SHA256};
use subtle::ConstantTimeEq;
use tonic::{Request, Status};

use crate::config::{
    Anonymous, AuthConfig, AuthzChannel, AuthzRule, EndpointCapability, PrincipalRole, RuleAction,
};

/// What an authenticated caller may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Perm {
    Read,
    Write,
    Admin,
}

/// A resolved caller. Placed in the request extensions by [`Authenticator`].
#[derive(Clone, Debug)]
pub struct Principal {
    pub name: Arc<str>,
    pub role: PrincipalRole,
    /// True when this deployment configures **no** credentials at all.
    ///
    /// It is not the same as holding every Flight role. Authorization for the
    /// shared control endpoint is evaluated independently by [`Authorizer`].
    ///
    /// What this actually means is "authentication is switched off", and the
    /// safety of that rests entirely on `Config::validate` confining it to a
    /// loopback bind ( or an explicit `--insecure` / `--insecure-replication` ).
    /// Once a single principal is configured this is never set again.
    pub unconfigured: bool,
}

impl Principal {
    /// The anonymous caller, which exists only when the deployment says so.
    fn anonymous(role: PrincipalRole, unconfigured: bool) -> Principal {
        Principal {
            name: Arc::from("anonymous"),
            role,
            unconfigured,
        }
    }

    /// `Admin` contains `Writer` contains `Reader`. `Replica` is disjoint: it
    /// exists for WAL shipping and must not imply the ability to read a key or
    /// force a checkpoint.
    pub fn may(&self, p: Perm) -> bool {
        if self.unconfigured {
            return true;
        }
        match self.role {
            PrincipalRole::Admin => true,
            PrincipalRole::Writer => p <= Perm::Write,
            PrincipalRole::Reader => p == Perm::Read,
            PrincipalRole::Replica => false,
        }
    }
}

/// The digest an operator puts in `token_sha256`, from the token itself.
#[derive(Clone, Copy, Debug)]
enum HbaAddress {
    All,
    V4 { network: u32, prefix: u8 },
    V6 { network: u128, prefix: u8 },
}

impl HbaAddress {
    fn contains(self, address: Option<std::net::IpAddr>) -> bool {
        match (self, address) {
            (HbaAddress::All, _) => true,
            (HbaAddress::V4 { network, prefix }, Some(std::net::IpAddr::V4(address))) => {
                masked_v4(u32::from(address), prefix) == network
            }
            (HbaAddress::V6 { network, prefix }, Some(std::net::IpAddr::V6(address))) => {
                masked_v6(u128::from(address), prefix) == network
            }
            _ => false,
        }
    }
}

fn masked_v4(address: u32, prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        address & (u32::MAX << (32 - prefix))
    }
}

fn masked_v6(address: u128, prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        address & (u128::MAX << (128 - prefix))
    }
}

fn hba_address(value: &str) -> Result<HbaAddress, String> {
    if value == "all" {
        return Ok(HbaAddress::All);
    }
    let (address, prefix) = match value.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (value, None),
    };
    let address: std::net::IpAddr = address
        .parse()
        .map_err(|_| format!("authorization address `{value}` is not an IP address or CIDR"))?;
    match address {
        std::net::IpAddr::V4(address) => {
            let prefix = match prefix {
                Some(prefix) => prefix.parse::<u8>().map_err(|_| {
                    format!("authorization address `{value}` has an invalid prefix")
                })?,
                None => 32,
            };
            if prefix > 32 {
                return Err(format!(
                    "authorization address `{value}` has an IPv4 prefix above 32"
                ));
            }
            Ok(HbaAddress::V4 {
                network: masked_v4(u32::from(address), prefix),
                prefix,
            })
        }
        std::net::IpAddr::V6(address) => {
            let prefix = match prefix {
                Some(prefix) => prefix.parse::<u8>().map_err(|_| {
                    format!("authorization address `{value}` has an invalid prefix")
                })?,
                None => 128,
            };
            if prefix > 128 {
                return Err(format!(
                    "authorization address `{value}` has an IPv6 prefix above 128"
                ));
            }
            Ok(HbaAddress::V6 {
                network: masked_v6(u128::from(address), prefix),
                prefix,
            })
        }
    }
}

/// Validate an authorization rule's address during configuration loading.
pub(crate) fn parse_hba_address(value: &str) -> Result<(), String> {
    hba_address(value).map(|_| ())
}

#[derive(Clone, Debug)]
struct CompiledRule {
    channel: AuthzChannel,
    principal: Arc<str>,
    address: HbaAddress,
    capability: EndpointCapability,
    action: RuleAction,
}

/// Ordered authorization policy for the shared control-plane endpoint.
#[derive(Clone, Debug)]
pub struct Authorizer {
    rules: Arc<[CompiledRule]>,
}

impl Authorizer {
    pub fn new(rules: &[AuthzRule]) -> Result<Authorizer, String> {
        let rules = rules
            .iter()
            .map(|rule| {
                Ok(CompiledRule {
                    channel: rule.channel,
                    principal: Arc::from(rule.principal.as_str()),
                    address: hba_address(&rule.address)?,
                    capability: rule.capability,
                    action: rule.action,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Authorizer {
            rules: rules.into(),
        })
    }

    /// Apply the first row matching channel, principal, source address, and
    /// capability. Local rows deliberately have no address column.
    pub fn authorize<T>(
        &self,
        request: &Request<T>,
        capability: EndpointCapability,
    ) -> Result<(), Status> {
        let principal = request
            .extensions()
            .get::<Principal>()
            .ok_or_else(|| Status::unauthenticated("no principal on this request"))?;
        let channel = request
            .extensions()
            .get::<AuthzChannel>()
            .copied()
            .ok_or_else(|| Status::internal("no connection channel on this request"))?;
        let address = request.remote_addr().map(|address| address.ip());
        let decision = self.rules.iter().find(|rule| {
            rule.channel.matches(channel)
                && rule.capability == capability
                && (rule.principal.as_ref() == "all"
                    || rule.principal.as_ref() == principal.name.as_ref())
                && (channel == AuthzChannel::Local || rule.address.contains(address))
        });
        match decision.map(|rule| rule.action) {
            Some(RuleAction::Allow) => Ok(()),
            Some(RuleAction::Deny) => Err(Status::permission_denied(format!(
                "authorization rule denies `{}` {:?}",
                principal.name, capability
            ))),
            None => Err(Status::permission_denied(format!(
                "no authorization rule permits `{}` {:?}",
                principal.name, capability
            ))),
        }
    }
}
///
/// Exposed because the recipe has to be reproducible outside this process —
/// `printf 'the-token' | sha256sum` is how it is documented — and one
/// implementation of it beats two that agree until they do not.
pub fn token_digest(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// The digest for a client certificate, from its DER encoding.
pub fn cert_digest(der: &[u8]) -> String {
    sha256_hex(der)
}

/// Lowercase hex SHA-256.
fn sha256_hex(bytes: &[u8]) -> String {
    let d = digest(&SHA256, bytes);
    let mut s = String::with_capacity(64);
    for b in d.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// One configured credential, with its digests already lowercased.
#[derive(Debug)]
struct Entry {
    name: Arc<str>,
    role: PrincipalRole,
    token_sha256: Option<String>,
    cert_sha256: Option<String>,
}

/// The credential store, built once at startup.
#[derive(Debug)]
pub struct Authenticator {
    entries: Vec<Entry>,
    anonymous: Anonymous,
    /// False when no principal is configured at all. See
    /// [`crate::config::AuthConfig::is_enforcing`]: that deployment is
    /// loopback-only by `validate`, and everyone on it is an administrator —
    /// which is exactly what a build with no authentication did.
    enforcing: bool,
}

impl Authenticator {
    pub fn new(cfg: &AuthConfig) -> Authenticator {
        Authenticator {
            entries: cfg
                .principals
                .iter()
                .map(|p| Entry {
                    name: Arc::from(p.name.as_str()),
                    role: p.role,
                    token_sha256: p.token_sha256.as_ref().map(|s| s.to_ascii_lowercase()),
                    cert_sha256: p.cert_sha256.as_ref().map(|s| s.to_ascii_lowercase()),
                })
                .collect(),
            anonymous: cfg.anonymous,
            enforcing: cfg.is_enforcing(),
        }
    }

    /// What a caller with no credential may be, if anything.
    pub fn anonymous_role(&self) -> Option<PrincipalRole> {
        if !self.enforcing {
            return Some(PrincipalRole::Admin);
        }
        match self.anonymous {
            Anonymous::None => None,
            Anonymous::Read => Some(PrincipalRole::Reader),
        }
    }

    fn by_token(&self, token: &str) -> Option<&Entry> {
        let want = sha256_hex(token.as_bytes());
        self.entries.iter().find(|e| {
            e.token_sha256
                .as_ref()
                // Constant time. These are digests rather than raw secrets,
                // so the leak this closes is narrow — but it costs one crate
                // already in the tree and a byte-at-a-time comparison against a
                // value the caller controls is not a habit worth having.
                .is_some_and(|d| bool::from(d.as_bytes().ct_eq(want.as_bytes())))
        })
    }

    fn by_cert(&self, der: &[u8]) -> Option<&Entry> {
        let want = sha256_hex(der);
        self.entries.iter().find(|e| {
            e.cert_sha256
                .as_ref()
                .is_some_and(|d| bool::from(d.as_bytes().ct_eq(want.as_bytes())))
        })
    }

    /// Resolve a request to a principal, or say why not.
    ///
    /// Answers `unauthenticated` for "no usable credential" and leaves
    /// `permission_denied` to the service's authorizer. Conflating them is how a
    /// client ends up retrying forever with a token that will never work.
    pub fn resolve(&self, req: &Request<()>) -> Result<Principal, Status> {
        // The certificate, if the peer presented one this listener accepted.
        let by_cert = req.peer_certs().and_then(|certs| {
            certs
                .first()
                .and_then(|c| self.by_cert(c.as_ref()))
                .map(|e| Principal {
                    name: e.name.clone(),
                    role: e.role,
                    unconfigured: false,
                })
        });

        let by_token = match req.metadata().get("authorization") {
            None => None,
            Some(v) => {
                let raw = v
                    .to_str()
                    .map_err(|_| Status::unauthenticated("the authorization header is not text"))?;
                let token = raw
                    .strip_prefix("Bearer ")
                    .or_else(|| raw.strip_prefix("bearer "))
                    .ok_or_else(|| {
                        Status::unauthenticated("expected an `Authorization: Bearer <token>`")
                    })?;
                match self.by_token(token.trim()) {
                    Some(e) => Some(Principal {
                        name: e.name.clone(),
                        role: e.role,
                        unconfigured: false,
                    }),
                    // A token was offered and is not one we know. Not falling
                    // through to anonymous: a caller who presents a credential
                    // is asserting an identity, and quietly downgrading them to
                    // anonymous turns "my token is wrong" into "some of my
                    // requests work", which is far harder to diagnose.
                    None => return Err(Status::unauthenticated("unknown bearer token")),
                }
            }
        };

        match (by_cert, by_token) {
            // Refused rather than resolved. There is no reading of "Alice's
            // certificate and Bob's token" that is safe to guess at.
            (Some(c), Some(t)) if c.name != t.name => Err(Status::unauthenticated(format!(
                "the client certificate identifies `{}` and the bearer token identifies \
                 `{}`; present one identity",
                c.name, t.name
            ))),
            (Some(c), _) => Ok(c),
            (None, Some(t)) => Ok(t),
            (None, None) => match self.anonymous_role() {
                Some(role) => Ok(Principal::anonymous(role, !self.enforcing)),
                None => Err(Status::unauthenticated(
                    "no credential: present a client certificate or an \
                     `Authorization: Bearer <token>` header",
                )),
            },
        }
    }
}

/// The interceptor tonic installs. Resolves a [`Principal`] into the request's
/// extensions, where Flight guards or the shared-endpoint authorizer read it.
pub fn interceptor(
    auth: Arc<Authenticator>,
) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |mut req: Request<()>| {
        let p = auth.resolve(&req)?;
        req.extensions_mut().insert(p);
        Ok(req)
    }
}

/// Authenticate a TCP request and record whether its listener is encrypted.
pub fn host_interceptor(
    auth: Arc<Authenticator>,
    tls: bool,
) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone {
    let channel = if tls {
        AuthzChannel::Hostssl
    } else {
        AuthzChannel::Hostnossl
    };
    move |mut req: Request<()>| {
        let p = auth.resolve(&req)?;
        req.extensions_mut().insert(p);
        req.extensions_mut().insert(channel);
        Ok(req)
    }
}

/// Resolve a Unix peer from kernel credentials and mark the request local.
///
/// The socket's filesystem permissions decide who may connect. The stable
/// principal spelling lets a rule narrow that admission boundary to one uid.
pub fn local_interceptor() -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |mut req: Request<()>| {
        let info = req
            .extensions()
            .get::<crate::local_transport::LocalConnectInfo>()
            .cloned()
            .ok_or_else(|| Status::unauthenticated("the local peer has no kernel credentials"))?;
        req.extensions_mut().insert(Principal {
            name: Arc::from(format!("uid:{}", info.uid)),
            role: PrincipalRole::Replica,
            unconfigured: false,
        });
        req.extensions_mut().insert(AuthzChannel::Local);
        if info.privileged_snapshot_agent {
            req.extensions_mut().insert(SnapshotAgentCredential);
        }
        Ok(req)
    }
}

/// Marker inserted only after the Unix transport verifies explicit, peer-matched
/// root `SCM_CREDENTIALS`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SnapshotAgentCredential;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PrincipalConfig;

    fn cfg(anonymous: Anonymous, principals: Vec<PrincipalConfig>) -> AuthConfig {
        AuthConfig {
            anonymous,
            principals,
            rules: Vec::new(),
        }
    }

    fn p(name: &str, role: PrincipalRole, token: &str) -> PrincipalConfig {
        PrincipalConfig {
            name: name.into(),
            role,
            token_sha256: Some(sha256_hex(token.as_bytes())),
            cert_sha256: None,
        }
    }

    fn req_with_token(token: &str) -> Request<()> {
        let mut r = Request::new(());
        r.metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        r
    }

    #[test]
    fn the_role_lattice_is_what_it_claims() {
        let mk = |n: &str, role| Principal {
            name: Arc::from(n),
            role,
            unconfigured: false,
        };
        let admin = mk("a", PrincipalRole::Admin);
        let writer = mk("w", PrincipalRole::Writer);
        let reader = mk("r", PrincipalRole::Reader);
        let replica = mk("s", PrincipalRole::Replica);

        for perm in [Perm::Read, Perm::Write, Perm::Admin] {
            assert!(admin.may(perm), "admin must contain {perm:?}");
        }
        assert!(writer.may(Perm::Read) && writer.may(Perm::Write));
        assert!(!writer.may(Perm::Admin), "a writer must not be an admin");
        assert!(reader.may(Perm::Read));
        assert!(!reader.may(Perm::Write) && !reader.may(Perm::Admin));

        // The disjoint one. A replication credential exists to ship a WAL and
        // must not thereby be able to read a key or force a checkpoint.
        for perm in [Perm::Read, Perm::Write, Perm::Admin] {
            assert!(!replica.may(perm), "a replica must not hold {perm:?}");
        }
    }

    #[test]
    fn a_known_token_resolves_and_an_unknown_one_is_refused() {
        let a = Authenticator::new(&cfg(
            Anonymous::None,
            vec![p("analytics", PrincipalRole::Reader, "s3cret")],
        ));
        let got = a.resolve(&req_with_token("s3cret")).unwrap();
        assert_eq!(&*got.name, "analytics");
        assert_eq!(got.role, PrincipalRole::Reader);

        let e = a.resolve(&req_with_token("wrong")).unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn a_wrong_token_is_not_quietly_downgraded_to_anonymous() {
        // Even where anonymous reads are permitted. A caller who presents a
        // credential is asserting an identity; downgrading them turns "my token
        // is wrong" into "some of my requests work".
        let a = Authenticator::new(&cfg(
            Anonymous::Read,
            vec![p("analytics", PrincipalRole::Reader, "s3cret")],
        ));
        let e = a.resolve(&req_with_token("wrong")).unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);

        // With no credential at all, the same deployment does allow a reader.
        let anon = a.resolve(&Request::new(())).unwrap();
        assert_eq!(anon.role, PrincipalRole::Reader);
        assert_eq!(&*anon.name, "anonymous");
    }

    #[test]
    fn no_credential_is_unauthenticated_when_anonymous_is_off() {
        // With a principal configured. An **empty** principal list is the
        // unconfigured deployment, which is open — see the next test.
        let a = Authenticator::new(&cfg(
            Anonymous::None,
            vec![p("analytics", PrincipalRole::Reader, "s3cret")],
        ));
        let e = a.resolve(&Request::new(())).unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
    }

    /// Configuring nothing means **open**, not closed, and the safety of
    /// that rests entirely on `Config::validate` refusing a non-loopback bind in
    /// the same state. The two halves are far apart in the source, so each says
    /// so and each is pinned; deleting either one alone is how a friendly local
    /// default becomes a public one.
    #[test]
    fn a_deployment_with_no_principals_is_open_rather_than_closed() {
        let a = Authenticator::new(&cfg(Anonymous::None, vec![]));
        let who = a
            .resolve(&Request::new(()))
            .expect("an unconfigured server serves");
        assert_eq!(who.role, PrincipalRole::Admin);
        assert!(who.may(Perm::Admin));

        // And adding a single principal flips it closed for everyone else.
        let a = Authenticator::new(&cfg(
            Anonymous::None,
            vec![p("analytics", PrincipalRole::Reader, "s3cret")],
        ));
        assert!(a.resolve(&Request::new(())).is_err());
    }

    #[test]
    fn a_malformed_authorization_header_is_refused_not_ignored() {
        let a = Authenticator::new(&cfg(Anonymous::Read, vec![]));
        let mut r = Request::new(());
        r.metadata_mut()
            .insert("authorization", "Basic aGk6dGhlcmU=".parse().unwrap());
        let e = a.resolve(&r).unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn the_digest_is_the_one_an_operator_would_compute() {
        // Taken from `printf 's3cret' | sha256sum`, not from this code. That
        // is the whole value of the test: an operator computes the digest with
        // a shell and pastes it into a config file, so the two recipes have to
        // agree, and a digest generated by the implementation under test would
        // agree with itself no matter what it did.
        assert_eq!(
            sha256_hex(b"s3cret"),
            "1ec1c26b50d5d3c58d9583181af8076655fe00756bf7285940ba3670f99fcba0"
        );
        // Lowercase hex, 64 characters — the shape `validate` insists on.
        assert_eq!(sha256_hex(b"").len(), 64);
        assert!(sha256_hex(b"x").chars().all(|c| c.is_ascii_hexdigit()));
    }

    fn rule(action: RuleAction) -> AuthzRule {
        AuthzRule {
            channel: AuthzChannel::Host,
            principal: "all".into(),
            address: "all".into(),
            capability: EndpointCapability::Replication,
            action,
        }
    }

    fn principal_request() -> Request<()> {
        let mut request = Request::new(());
        request.extensions_mut().insert(Principal {
            name: Arc::from("standby"),
            role: PrincipalRole::Admin,
            unconfigured: false,
        });
        request.extensions_mut().insert(AuthzChannel::Hostnossl);
        request
    }

    #[test]
    fn authorization_is_first_match_and_default_deny() {
        let deny_first =
            Authorizer::new(&[rule(RuleAction::Deny), rule(RuleAction::Allow)]).unwrap();
        let error = deny_first
            .authorize(&principal_request(), EndpointCapability::Replication)
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::PermissionDenied);
        assert!(error.message().contains("denies"));

        let allow_first =
            Authorizer::new(&[rule(RuleAction::Allow), rule(RuleAction::Deny)]).unwrap();
        allow_first
            .authorize(&principal_request(), EndpointCapability::Replication)
            .unwrap();
        let error = allow_first
            .authorize(&principal_request(), EndpointCapability::ControlAdmin)
            .unwrap_err();
        assert!(error.message().contains("no authorization rule"));
    }

    #[test]
    fn authorization_addresses_are_real_cidr_networks() {
        let network = hba_address("10.20.0.0/16").unwrap();
        assert!(network.contains(Some("10.20.4.5".parse().unwrap())));
        assert!(!network.contains(Some("10.21.4.5".parse().unwrap())));

        let host = hba_address("2001:db8::1").unwrap();
        assert!(host.contains(Some("2001:db8::1".parse().unwrap())));
        assert!(!host.contains(Some("2001:db8::2".parse().unwrap())));

        assert!(hba_address("10.0.0.0/33").is_err());
        assert!(hba_address("2001:db8::/129").is_err());
        assert!(hba_address("not-a-network").is_err());
    }

    #[test]
    fn authorization_selects_the_connection_channel_before_other_columns() {
        let mut local = rule(RuleAction::Allow);
        local.channel = AuthzChannel::Local;
        local.address = "192.0.2.1".into();
        let authorizer = Authorizer::new(&[local]).unwrap();

        let mut request = principal_request();
        request.extensions_mut().insert(AuthzChannel::Local);
        authorizer
            .authorize(&request, EndpointCapability::Replication)
            .unwrap();

        request.extensions_mut().insert(AuthzChannel::Hostnossl);
        assert!(authorizer
            .authorize(&request, EndpointCapability::Replication)
            .is_err());
    }
}
