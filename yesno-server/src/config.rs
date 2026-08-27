//! What `yesnod` is told, and how the ways of telling it are ranked.
//!
//! # Field names mirror the core structs, deliberately
//!
//! `interval_secs` is `interval_secs` and not `interval = "60s"`, `shards` is
//! `shards`. The translation layer between a config file and a policy struct is
//! exactly where a key silently comes to mean something other than what the
//! engine does with it, and the cheapest defence is to refuse to translate.
//!
//! # Precedence
//!
//! built-in defaults < config file < `YESNOD_*` environment < command line.
//!
//! The middle two come from `clap`'s `env` feature: a flag that is absent but
//! whose variable is set resolves to `Some( .. )`, which then overrides the
//! file the same way an explicit flag would.
//!
//! # `--check-config` opens nothing
//!
//! It resolves the whole chain, validates, prints the effective configuration
//! and exits — **without touching the database directory**. That is what makes
//! it safe as a systemd `ExecStartPre=`, where the service is not running yet
//! and taking the lock would be actively wrong.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use yesno_core::checkpoint::CheckpointPolicy;
use yesno_core::{DbOptions, SpaceAmpPolicy};

/// A byte count that may be written `1048576` or `"1MiB"`.
///
/// Binary units only, and `"1MB"` is **refused** rather than guessed at.
/// Whether `MB` means 10^6 or 2^20 is a genuine disagreement in the world, and a
/// config key that silently picks one is worse than a config key that makes the
/// operator say which they meant. Ambiguity is not a service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Bytes(pub u64);

impl Bytes {
    pub fn parse(s: &str) -> Result<Bytes, String> {
        let t = s.trim();
        if t.is_empty() {
            return Err("empty byte size".into());
        }
        // Split the digits from whatever follows them.
        let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
        let (num, unit) = t.split_at(split);
        if num.is_empty() {
            return Err(format!("`{s}` does not start with a number"));
        }
        let n: u64 = num
            .parse()
            .map_err(|_| format!("`{num}` is not a byte count"))?;
        let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "kib" => 1 << 10,
            "mib" => 1 << 20,
            "gib" => 1 << 30,
            "tib" => 1 << 40,
            other => {
                return Err(format!(
                    "unknown unit `{other}` in `{s}`; use a bare number of bytes or one of \
                     KiB, MiB, GiB, TiB. ( `MB` is refused on purpose: binary and decimal \
                     megabytes differ by 5% and guessing which was meant is not this \
                     program's call )"
                ))
            }
        };
        n.checked_mul(mult)
            .map(Bytes)
            .ok_or_else(|| format!("`{s}` overflows a u64"))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(Bytes(n)),
            Raw::Str(s) => Bytes::parse(&s).map_err(serde::de::Error::custom),
        }
    }
}

/// What this node is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Leader,
    /// A standby that follows a leader's WAL. It may stay cold or hold a
    /// read-only `Db` open when `follower.serve_reads` is enabled.
    Follower,
}

/// Transport security for a listener. Absent means plaintext.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    /// Presence enables mTLS: clients must present a certificate this CA signed.
    pub client_ca: Option<PathBuf>,
    /// When `client_ca` is set, whether a client certificate is mandatory.
    /// `false` makes it optional, which is what a deployment mixing certificate
    /// and bearer-token clients wants.
    pub require_client_auth: bool,
}

impl TlsConfig {
    pub fn is_enabled(&self) -> bool {
        self.cert.is_some() || self.key.is_some()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlightConfig {
    pub listen: String,
    pub tls: TlsConfig,
}

/// What a principal may do on Flight. `Admin` contains `Writer` contains
/// `Reader`; `Replica` is disjoint. Shared control-endpoint access is granted
/// independently by [`AuthzRule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalRole {
    Reader,
    Writer,
    Admin,
    Replica,
}

/// One credential the server will accept.
///
/// **Only digests are stored, never the secret.** A plaintext bearer token in
/// a config file would be the worst thing in this design: config files get
/// copied into tickets, backups and chat.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrincipalConfig {
    pub name: String,
    pub role: PrincipalRole,
    /// Lowercase hex SHA-256 of the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_sha256: Option<String>,
    /// Lowercase hex SHA-256 of the client certificate's DER leaf.
    ///
    /// A fingerprint, deliberately not a Subject CN. Matching a CN needs an
    /// X.509 parser — five more crates — and CN-versus-SAN is a footgun with a
    /// long history. The cost is that rotating a certificate is a config edit,
    /// which is an honest trade for v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_sha256: Option<String>,
}

/// What an unauthenticated caller may do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Anonymous {
    #[default]
    None,
    /// Reads without a credential. This is the public-read-replica case, and it
    /// is a deployment choice rather than a special mode.
    Read,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub anonymous: Anonymous,
    #[serde(rename = "principal")]
    pub principals: Vec<PrincipalConfig>,
    /// Ordered, first-match authorization rules for the shared control-plane
    /// endpoint. No matching row means deny.
    #[serde(rename = "rule")]
    pub rules: Vec<AuthzRule>,
}

/// A permission class on the shared control-plane endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointCapability {
    ControlRead,
    ControlAdmin,
    Replication,
}

/// The decision made by a matching authorization row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Deny,
}

/// Connection channel selected before a shared-endpoint authorization row.
///
/// This follows PostgreSQL's `pg_hba.conf` vocabulary: `host` covers either
/// TCP transport, while the two narrower host variants distinguish TLS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthzChannel {
    Local,
    #[default]
    Host,
    Hostssl,
    Hostnossl,
}

impl AuthzChannel {
    pub(crate) fn matches(self, actual: AuthzChannel) -> bool {
        self == actual
            || (self == AuthzChannel::Host
                && matches!(actual, AuthzChannel::Hostssl | AuthzChannel::Hostnossl))
    }

    pub(crate) fn permits_host(self) -> bool {
        matches!(
            self,
            AuthzChannel::Host | AuthzChannel::Hostssl | AuthzChannel::Hostnossl
        )
    }
}

/// One row in the shared endpoint's pg_hba-style authorization table.
///
/// Rows are evaluated from top to bottom. `channel` first selects a local Unix
/// connection or a TCP/TLS variant. `principal = "all"` and `address = "all"`
/// are wildcards; an address may otherwise be an IP address or CIDR network and
/// is ignored for `local` rows. The first matching row decides, and no match
/// denies.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthzRule {
    #[serde(default)]
    pub channel: AuthzChannel,
    pub principal: String,
    #[serde(default = "all_address")]
    pub address: String,
    pub capability: EndpointCapability,
    pub action: RuleAction,
}

fn all_address() -> String {
    "all".into()
}

impl AuthConfig {
    /// Whether this deployment authenticates at all.
    ///
    /// **Configuring no principals means "open", not "closed".** That looks
    /// like the wrong default and is the right one, because of what the two
    /// mistakes cost:
    ///
    /// * closed-by-default makes a first run refuse every request, including
    ///   `yesno status`, with a message about credentials the operator has
    ///   not been asked for yet — so the first thing anyone learns is how to
    ///   turn the feature off;
    /// * open-by-default is only reachable on a **loopback** bind, because
    ///   [`Config::validate`] refuses a public one with no principals. A
    ///   process on the same host can already read the database files.
    ///
    /// So the exposure is unchanged from a build with no auth at all, and the
    /// moment a deployment is reachable from elsewhere it must say who may call.
    /// `anonymous` is about the *other* case: what a caller with no credential
    /// may do in a deployment that does have principals.
    pub fn is_enforcing(&self) -> bool {
        !self.principals.is_empty()
    }
}

impl Default for FlightConfig {
    fn default() -> Self {
        // Loopback, because a database that binds the world by default is a
        // database somebody exposes by accident.
        FlightConfig {
            listen: "127.0.0.1:50051".into(),
            tls: TlsConfig::default(),
        }
    }
}

/// Where a client's own certificate and trust come from.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientTls {
    /// PEM of the CA that signed the leader's certificate.
    pub ca: Option<PathBuf>,
    /// This node's own certificate, for mutual TLS. A leader configured the way
    /// `yesnod` defaults to will demand one.
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    /// The name to verify the leader's certificate against, when it differs from
    /// the host in `leader` — which it does whenever a node is reached by
    /// address and named by DNS in its certificate.
    pub domain: Option<String>,
}

impl ClientTls {
    pub fn is_enabled(&self) -> bool {
        self.ca.is_some() || self.cert.is_some() || self.domain.is_some()
    }
}

/// What a `role = "follower"` node needs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FollowerConfig {
    /// The leader's **replication** endpoint, e.g. `https://a.internal:50052`.
    pub leader: String,
    /// How long to wait after a pass that shipped nothing.
    pub poll_interval_secs: u64,
    /// `0` asks the leader for its own default.
    pub max_batch_bytes: u32,
    /// Cap on the reconnect backoff.
    pub max_backoff_secs: u64,
    /// Serve reads from this standby while it follows.
    ///
    /// The node then holds the database **open** and applies shipped frames
    /// into it, rather than writing files behind a closed one. Reads are served
    /// from a consistent snapshot at whatever the standby has applied — which
    /// lags the leader by at least a round trip, and always will: replication
    /// here is asynchronous, so "read from a replica" means "read slightly old
    /// data" and nothing can make it not.
    ///
    /// It also means a `server.flight.listen` is needed, and that rebuilding
    /// after falling off the leader's log takes the database away for a while —
    /// during which reads answer `unavailable` rather than the port closing.
    pub serve_reads: bool,
    pub tls: ClientTls,
}

impl Default for FollowerConfig {
    fn default() -> Self {
        FollowerConfig {
            leader: String::new(),
            // A second, because the leader's own `subscribe` already sleeps 50 ms
            // between polls when caught up — this is the interval between
            // *passes*, and a shorter one would mostly re-ask questions the
            // leader has just answered.
            poll_interval_secs: 1,
            max_batch_bytes: 0,
            max_backoff_secs: 30,
            serve_reads: false,
            tls: ClientTls::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Empty disables the listener entirely.
    ///
    /// Loopback by default and it should stay there: a metrics endpoint is
    /// conventionally unauthenticated, and this one reports version watermarks,
    /// reader counts and space figures. It is a separate socket from Flight for
    /// exactly that reason — so it can be reachable by a scraper without being
    /// reachable by a client.
    pub listen: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        MetricsConfig {
            listen: "127.0.0.1:9750".into(),
        }
    }
}

/// The shared Protobuf control-plane endpoint.
///
/// Lifecycle/event RPCs and WAL/image replication use the same service on its
/// configured TCP and Unix listeners. Empty `listen`, `unix_socket`, and
/// `journal_dir` disables the endpoint.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControlConfig {
    /// TCP listener. Empty disables the remote channel.
    pub listen: String,
    /// Unix-domain listener. Filesystem ownership and mode are its admission
    /// boundary; `local` authorization rows still decide each capability.
    pub unix_socket: Option<PathBuf>,
    /// Explicit mode for that socket, as an octal string such as `"0660"`.
    ///
    /// Unset means the socket keeps whatever the process umask leaves, which
    /// is what `bind(2)` does and what every deployment got before this option
    /// existed. That default is deliberate — changing it would silently move an
    /// admission boundary under existing deployments — but it is also umask
    /// dependent, and connecting to a Unix socket needs the **write** bit. At
    /// the usual umask of 022 the socket lands on 0755, which lets the owner
    /// connect, lets a `CAP_DAC_OVERRIDE` holder connect, and refuses a group
    /// member. Set `"0660"` when a separately accounted peer — the privileged
    /// snapshot agent is the one in tree — must reach the daemon through group
    /// membership rather than by holding `CAP_DAC_OVERRIDE`.
    pub unix_socket_mode: Option<String>,
    /// Kept separately from the database directory so a data-volume failure
    /// need not also destroy the event stream that reports it.
    pub journal_dir: PathBuf,
    pub tls: TlsConfig,
}

impl ControlConfig {
    /// The configured socket mode, already parsed, or `None` to keep the
    /// umask-derived mode `bind(2)` produces.
    #[must_use]
    pub fn socket_mode(&self) -> Option<u32> {
        self.unix_socket_mode
            .as_deref()
            .and_then(|mode| parse_socket_mode(mode).ok())
    }
}

/// Filesystem primitive used to create an immutable database image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotBackend {
    /// Use a portable server-staged copy behind the control-plane lease.
    #[default]
    Disabled,
    Zfs,
    Btrfs,
    Lvm,
    Ebs,
}

/// Local LVM snapshot and mount policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LvmSnapshotConfig {
    /// Mount root of the origin logical volume; `server.data_dir` is below it.
    pub source_mount: PathBuf,
    /// Volume group containing the origin logical volume.
    pub volume_group: String,
    /// Origin logical volume name within `volume_group`.
    pub logical_volume: String,
    /// Private parent directory for mounted read-only snapshot LVs.
    pub mount_dir: PathBuf,
    /// Filesystem stored in the logical volume: `ext4` or `xfs`.
    pub filesystem: String,
    /// Copy-on-write capacity allocated to each classic LVM snapshot.
    pub snapshot_size_gib: u64,
    /// Maximum wait for the privileged local snapshot agent per operation.
    pub operation_timeout_secs: u64,
}

impl Default for LvmSnapshotConfig {
    fn default() -> Self {
        Self {
            source_mount: PathBuf::new(),
            volume_group: String::new(),
            logical_volume: String::new(),
            mount_dir: PathBuf::new(),
            filesystem: "ext4".into(),
            snapshot_size_gib: 8,
            operation_timeout_secs: 300,
        }
    }
}

/// Where an EBS snapshot is materialized for the archive lease.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EbsMaterialization {
    /// Restore and attach a temporary volume to this EC2 instance.
    #[default]
    Local,
    /// Return the completed EBS snapshot as a provisional lease. The archiver
    /// is responsible for materializing it before publication.
    Deferred,
}

/// Amazon EBS resources used to materialize one snapshot lease.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EbsSnapshotConfig {
    /// Attach locally or return a provisional lease for the archiver.
    pub materialization: EbsMaterialization,
    /// Region containing the source volume and this instance.
    pub region: String,
    /// EBS volume containing `server.data_dir`.
    pub volume_id: String,
    /// EC2 instance to which temporary snapshot volumes are attached.
    /// Required only for `materialization = "local"`.
    pub instance_id: String,
    /// Availability Zone shared by the instance and temporary volumes.
    /// Required only for `materialization = "local"`.
    pub availability_zone: String,
    /// Mount root of `volume_id`; `server.data_dir` must be below it.
    pub source_mount: PathBuf,
    /// Private parent directory for temporary read-only mounts in local mode.
    pub mount_dir: PathBuf,
    /// Filesystem on the EBS volume. The backend supports `ext4` and `xfs`.
    pub filesystem: String,
    /// Partition number, or none when the filesystem occupies the whole volume.
    pub partition: Option<u32>,
    /// EC2 attachment names available to concurrent leases.
    /// Used only for `materialization = "local"`.
    pub device_names: Vec<String>,
    /// Maximum wait for each AWS state transition or local device appearance.
    pub operation_timeout_secs: u64,
    /// Additional tags copied to every temporary snapshot and restored volume.
    /// The provider-owned `yesno:database` and `yesno:lease` keys are reserved.
    pub resource_tags: BTreeMap<String, String>,
}

impl Default for EbsSnapshotConfig {
    fn default() -> Self {
        Self {
            materialization: EbsMaterialization::Local,
            region: String::new(),
            volume_id: String::new(),
            instance_id: String::new(),
            availability_zone: String::new(),
            source_mount: PathBuf::new(),
            mount_dir: PathBuf::new(),
            filesystem: "ext4".into(),
            partition: None,
            device_names: (b'f'..=b'p')
                .map(|letter| format!("/dev/sd{}", char::from(letter)))
                .collect(),
            operation_timeout_secs: 3_600,
            resource_tags: BTreeMap::new(),
        }
    }
}

/// Server-owned base-snapshot provider policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotConfig {
    pub backend: SnapshotBackend,
    /// Dataset containing `server.data_dir`, for the ZFS backend.
    pub zfs_dataset: Option<String>,
    /// Directory outside the live data subvolume for Btrfs snapshots.
    pub btrfs_snapshot_dir: Option<PathBuf>,
    /// Local LVM provider settings, required only for the LVM backend.
    pub lvm: Option<LvmSnapshotConfig>,
    /// Amazon EBS provider settings, required only for the EBS backend.
    pub ebs: Option<EbsSnapshotConfig>,
    /// Permit a client that explicitly asks to receive snapshot-local paths.
    pub allow_direct_path: bool,
    /// Abandoned leases are destroyed after this many seconds without renewal.
    pub lease_ttl_secs: u64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            backend: SnapshotBackend::Disabled,
            zfs_dataset: None,
            btrfs_snapshot_dir: None,
            lvm: None,
            ebs: None,
            allow_direct_path: false,
            lease_ttl_secs: 300,
        }
    }
}

/// OpenTelemetry trace sampling policy.
///
/// Names mirror the SDK's standard sampler vocabulary, with underscores added
/// where they make the configuration easier to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceSampler {
    /// Record every trace.
    AlwaysOn,
    /// Drop every trace.
    AlwaysOff,
    /// Sample new traces deterministically at `sample_ratio`.
    #[serde(alias = "traceidratio")]
    TraceIdRatio,
    /// Preserve a parent's decision and record every new root trace.
    #[default]
    ParentBasedAlwaysOn,
    /// Preserve a parent's decision and drop every new root trace.
    ParentBasedAlwaysOff,
    /// Preserve a parent's decision and sample new root traces at `sample_ratio`.
    #[serde(alias = "parentbased_traceidratio")]
    ParentBasedTraceIdRatio,
}

impl TraceSampler {
    pub(crate) fn uses_ratio(self) -> bool {
        matches!(
            self,
            TraceSampler::TraceIdRatio | TraceSampler::ParentBasedTraceIdRatio
        )
    }
}

/// OpenTelemetry trace export owned by the daemon.
///
/// Export is opt-in so embedding `yesno-server` or running an unconfigured
/// daemon never starts a network client. When enabled without an explicit
/// endpoint, the OTLP exporter follows its standard environment variables and
/// otherwise defaults to the local collector.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Whether to construct an SDK provider and export spans.
    pub enabled: bool,
    /// OTLP/gRPC collector endpoint.
    pub endpoint: Option<String>,
    /// OpenTelemetry `service.name`.
    pub service_name: String,
    /// SDK sampling policy.
    pub sampler: TraceSampler,
    /// Root trace probability for either ratio sampler, inclusive from 0 to 1.
    pub sample_ratio: f64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            service_name: "yesnod".into(),
            sampler: TraceSampler::ParentBasedAlwaysOn,
            sample_ratio: 1.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub role: Role,
    pub data_dir: Option<PathBuf>,
    pub flight: FlightConfig,
    pub metrics: MetricsConfig,
    pub control: ControlConfig,
    pub snapshot: SnapshotConfig,
    pub telemetry: TelemetryConfig,
    /// How long teardown may spend draining in-flight readers before it gives
    /// up and says so.
    pub shutdown_grace_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            role: Role::default(),
            data_dir: None,
            flight: FlightConfig::default(),
            metrics: MetricsConfig::default(),
            control: ControlConfig::default(),
            snapshot: SnapshotConfig::default(),
            telemetry: TelemetryConfig::default(),
            shutdown_grace_secs: 30,
        }
    }
}

/// Mirrors [`CheckpointPolicy`] key for key.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointConfig {
    pub dirty_bytes: Bytes,
    pub max_dirty_bytes: Bytes,
    pub wal_bytes: Bytes,
    pub interval_secs: u64,
    pub max_wal_bytes: Bytes,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        // Taken from the engine rather than restated, so the two cannot drift.
        let p = CheckpointPolicy::default();
        CheckpointConfig {
            dirty_bytes: Bytes(p.dirty_bytes as u64),
            max_dirty_bytes: Bytes(p.max_dirty_bytes as u64),
            wal_bytes: Bytes(p.wal_bytes),
            interval_secs: p.interval_secs,
            max_wal_bytes: Bytes(p.max_wal_bytes),
        }
    }
}

impl From<CheckpointConfig> for CheckpointPolicy {
    fn from(c: CheckpointConfig) -> Self {
        CheckpointPolicy {
            dirty_bytes: c.dirty_bytes.0 as usize,
            max_dirty_bytes: c.max_dirty_bytes.0 as usize,
            wal_bytes: c.wal_bytes.0,
            interval_secs: c.interval_secs,
            max_wal_bytes: c.max_wal_bytes.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceAmp {
    AbortOldestReader,
    StallWriters,
}

impl From<SpaceAmp> for SpaceAmpPolicy {
    fn from(s: SpaceAmp) -> Self {
        match s {
            SpaceAmp::AbortOldestReader => SpaceAmpPolicy::AbortOldestReader,
            SpaceAmp::StallWriters => SpaceAmpPolicy::StallWriters,
        }
    }
}

/// Mirrors [`DbOptions`] key for key.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbConfig {
    /// **A creation parameter only.** On an existing database the persisted
    /// MANIFEST's shard count wins, because the count is part of the routing
    /// function and there is no way to serve four shards' data as eight. The
    /// daemon warns at startup when the two disagree, which turns a silent
    /// adoption into something an operator can see.
    pub shards: usize,
    pub max_readers: usize,
    pub commit_ring: usize,
    pub evacuate_per_checkpoint: usize,
    pub on_space_amp: SpaceAmp,
    /// Amplification, in parts per thousand, at which a soft-threshold event is
    /// emitted. `0` disables it. Observation only — nothing is evicted for
    /// crossing it; `on_space_amp` remains the only intervention.
    pub space_amp_soft_permille: u32,
    /// Seconds a snapshot may stay open before an observation event names it.
    /// `0` disables. Observation only; nothing is evicted for it.
    pub snapshot_soft_age_secs: u64,
    pub rebuild_alloc_on_open: bool,
    pub checkpoint: CheckpointConfig,
}

impl Default for DbConfig {
    fn default() -> Self {
        let d = DbOptions::default();
        DbConfig {
            shards: d.shards,
            max_readers: d.max_readers,
            commit_ring: d.commit_ring,
            evacuate_per_checkpoint: d.evacuate_per_checkpoint,
            on_space_amp: SpaceAmp::AbortOldestReader,
            space_amp_soft_permille: d.space_amp_soft_permille,
            snapshot_soft_age_secs: d.snapshot_soft_age_secs,
            rebuild_alloc_on_open: d.rebuild_alloc_on_open,
            checkpoint: CheckpointConfig::default(),
        }
    }
}

impl From<DbConfig> for DbOptions {
    fn from(c: DbConfig) -> Self {
        DbOptions {
            shards: c.shards,
            policy: c.checkpoint.into(),
            evacuate_per_checkpoint: c.evacuate_per_checkpoint,
            max_readers: c.max_readers,
            on_space_amp: c.on_space_amp.into(),
            space_amp_soft_permille: c.space_amp_soft_permille,
            snapshot_soft_age_secs: c.snapshot_soft_age_secs,
            commit_ring: c.commit_ring,
            rebuild_alloc_on_open: c.rebuild_alloc_on_open,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub db: DbConfig,
    pub auth: AuthConfig,
    pub follower: FollowerConfig,
}

/// The command line. Deliberately **not** a flag per config key: a flag for
/// every field is a maintenance tax, and an operator who wants
/// `evacuate_per_checkpoint` on the command line is doing something a file
/// should be recording.
#[derive(Clone, Debug, clap::Parser)]
#[command(name = "yesnod", about = "The yesno database daemon", version)]
pub struct Cli {
    #[arg(long, value_name = "FILE", env = "YESNOD_CONFIG")]
    pub config: Option<PathBuf>,

    #[arg(long, value_name = "DIR", env = "YESNOD_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    #[arg(long, value_name = "ADDR", env = "YESNOD_FLIGHT_LISTEN")]
    pub flight_listen: Option<String>,

    #[arg(long, value_name = "ADDR", env = "YESNOD_CONTROL_LISTEN")]
    pub control_listen: Option<String>,

    #[arg(long, value_name = "DIR", env = "YESNOD_CONTROL_JOURNAL_DIR")]
    pub control_journal_dir: Option<PathBuf>,

    #[arg(
        long,
        value_name = "FILTER",
        env = "YESNOD_LOG",
        default_value = "info"
    )]
    pub log: String,

    /// Resolve and validate the configuration, print it, and exit. Opens no
    /// database and takes no lock.
    #[arg(long)]
    pub check_config: bool,

    /// Permit binding a non-loopback address with no transport security.
    #[arg(long)]
    pub insecure: bool,

    /// Permit an authorization rule to expose replication without TLS and an
    /// authenticated principal.
    ///
    /// Its own flag rather than part of `--insecure`, because the exposures
    /// are not comparable: an open Flight port lets an attacker ask for keys, an
    /// open replication capability hands over the whole database in one call.
    #[arg(long)]
    pub insecure_replication: bool,
}

#[derive(Debug)]
pub enum ConfigError {
    Read(PathBuf, std::io::Error),
    Parse(PathBuf, toml::de::Error),
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Read(p, e) => write!(f, "cannot read {}: {e}", p.display()),
            ConfigError::Parse(p, e) => write!(f, "cannot parse {}: {e}", p.display()),
            ConfigError::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Whether two configured listeners would actually fight over a socket.
///
/// **Port 0 never collides.** It means "let the OS choose", so two listeners
/// both asking for it get two different ports. Comparing addresses naively
/// rejects a perfectly good configuration — and it is the configuration every
/// test in this crate uses, so the mistake shows up immediately and then gets
/// made again on the next listener. Hence one helper rather than a comparison
/// per pair.
fn collides(a: SocketAddr, b: SocketAddr) -> bool {
    a == b && a.port() != 0
}

/// Parse a filesystem mode written the way an operator writes one.
///
/// TOML has no octal literal, so the option is a string and is read as octal
/// whether or not it carries a `0o` prefix. Never read it as decimal: `660`
/// decimal is `0o1224`, which sets the setuid bit and clears owner write — a
/// silent, dangerous reinterpretation of the most likely thing to be typed.
fn parse_socket_mode(value: &str) -> Result<u32, String> {
    let digits = value
        .trim()
        .strip_prefix("0o")
        .unwrap_or_else(|| value.trim());
    let parsed = u32::from_str_radix(digits, 8)
        .map_err(|_| format!("`server.control.unix_socket_mode` is `{value}`; expected an octal mode such as \"0660\""))?;
    if parsed > 0o777 {
        return Err(format!(
            "`server.control.unix_socket_mode` is `{value}`; expected at most `0777`"
        ));
    }
    Ok(parsed)
}

fn valid_lvm_name(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'_' | b'.' | b'-'))
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Config, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.to_path_buf(), e))?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))
    }

    /// Defaults, then the file, then environment and flags.
    pub fn resolve(cli: &Cli) -> Result<Config, ConfigError> {
        let mut cfg = match &cli.config {
            Some(p) => Config::from_file(p)?,
            None => Config::default(),
        };
        if let Some(d) = &cli.data_dir {
            cfg.server.data_dir = Some(d.clone());
        }
        if let Some(a) = &cli.flight_listen {
            cfg.server.flight.listen = a.clone();
        }
        if let Some(address) = &cli.control_listen {
            cfg.server.control.listen = address.clone();
        }
        if let Some(path) = &cli.control_journal_dir {
            cfg.server.control.journal_dir = path.clone();
        }
        cfg.validate_with(cli.insecure, cli.insecure_replication)?;
        Ok(cfg)
    }

    /// The address the Flight service will bind, parsed.
    pub fn flight_addr(&self) -> Result<SocketAddr, ConfigError> {
        self.server.flight.listen.parse().map_err(|_| {
            ConfigError::Invalid(format!(
                "`server.flight.listen` is `{}`, which is not a socket address like \
                 `127.0.0.1:50051`",
                self.server.flight.listen
            ))
        })
    }

    /// The shared control and replication address, or `None` when disabled.
    pub fn control_addr(&self) -> Result<Option<SocketAddr>, ConfigError> {
        if self.server.control.listen.trim().is_empty() {
            return Ok(None);
        }
        self.server.control.listen.parse().map(Some).map_err(|_| {
            ConfigError::Invalid(format!(
                "`server.control.listen` is `{}`, which is not a socket address like \
                     `127.0.0.1:50052`. Leave it empty to disable control and replication.",
                self.server.control.listen
            ))
        })
    }

    /// The local shared control and replication socket, when configured.
    pub fn control_unix_socket(&self) -> Option<&Path> {
        self.server.control.unix_socket.as_deref()
    }

    /// The configured control-socket mode, already parsed.
    ///
    /// `None` keeps `bind(2)`'s umask-derived mode, which is what every
    /// deployment had before the option existed.
    #[must_use]
    pub fn control_unix_socket_mode(&self) -> Option<u32> {
        self.server.control.socket_mode()
    }

    /// Whether at least one shared control-plane channel is configured.
    pub fn control_enabled(&self) -> bool {
        !self.server.control.listen.trim().is_empty() || self.server.control.unix_socket.is_some()
    }

    /// The metrics address, or `None` when the listener is disabled.
    pub fn metrics_addr(&self) -> Result<Option<SocketAddr>, ConfigError> {
        if self.server.metrics.listen.trim().is_empty() {
            return Ok(None);
        }
        self.server.metrics.listen.parse().map(Some).map_err(|_| {
            ConfigError::Invalid(format!(
                "`server.metrics.listen` is `{}`, which is not a socket address like \
                     `127.0.0.1:9750`. Leave it empty to disable the listener.",
                self.server.metrics.listen
            ))
        })
    }

    /// Configured control journal directory, or `None` when disabled.
    pub fn control_journal_dir(&self) -> Option<&Path> {
        self.control_enabled()
            .then_some(self.server.control.journal_dir.as_path())
    }

    fn validate_principals(&self) -> Result<(), ConfigError> {
        // Every principal must be reachable by something. This applies to both
        // roles: followers may expose the shared endpoint or serve reads.
        for principal in &self.auth.principals {
            if principal.name.trim().is_empty() {
                return Err(ConfigError::Invalid("a principal has an empty name".into()));
            }
            if principal.token_sha256.is_none() && principal.cert_sha256.is_none() {
                return Err(ConfigError::Invalid(format!(
                    "principal `{}` has neither `token_sha256` nor `cert_sha256`, so no \
                     credential could ever match it",
                    principal.name
                )));
            }
            for (what, digest) in [
                ("token_sha256", &principal.token_sha256),
                ("cert_sha256", &principal.cert_sha256),
            ] {
                if let Some(digest) = digest {
                    // A typo'd digest is otherwise a principal that silently
                    // never authenticates, and the symptom points at the client.
                    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(ConfigError::Invalid(format!(
                            "principal `{}`: `{what}` must be 64 hex characters ( a SHA-256 \
                             digest ), got {} character(s)",
                            principal.name,
                            digest.len()
                        )));
                    }
                }
            }
        }
        let mut names: Vec<&str> = self
            .auth
            .principals
            .iter()
            .map(|principal| principal.name.as_str())
            .collect();
        names.sort_unstable();
        if let Some(duplicate) = names.windows(2).find(|window| window[0] == window[1]) {
            return Err(ConfigError::Invalid(format!(
                "two principals are both named `{}`; a name is how the logs and the \
                 retention floor tell them apart",
                duplicate[0]
            )));
        }
        Ok(())
    }

    /// Everything that can be known without opening anything.
    pub fn validate(&self, insecure: bool) -> Result<(), ConfigError> {
        self.validate_with(insecure, false)
    }

    /// The full form. `insecure_replication` is its own flag on purpose — see
    /// the shared-endpoint checks below.
    pub fn validate_with(
        &self,
        insecure: bool,
        insecure_replication: bool,
    ) -> Result<(), ConfigError> {
        if self.server.data_dir.is_none() {
            return Err(ConfigError::Invalid(
                "no database directory: set `server.data_dir` in the config file or pass \
                 `--data-dir`"
                    .into(),
            ));
        }
        let snapshot = &self.server.snapshot;
        if snapshot.lease_ttl_secs == 0 {
            return Err(ConfigError::Invalid(
                "`server.snapshot.lease_ttl_secs` must be positive".into(),
            ));
        }
        match snapshot.backend {
            SnapshotBackend::Disabled => {
                if snapshot.zfs_dataset.is_some()
                    || snapshot.btrfs_snapshot_dir.is_some()
                    || snapshot.lvm.is_some()
                    || snapshot.ebs.is_some()
                    || snapshot.allow_direct_path
                {
                    return Err(ConfigError::Invalid(
                        "disabled `server.snapshot.backend` cannot have provider paths or enable direct paths"
                            .into(),
                    ));
                }
            }
            SnapshotBackend::Zfs => {
                let dataset = snapshot.zfs_dataset.as_deref().unwrap_or_default();
                if dataset.is_empty() || dataset.starts_with('-') || dataset.contains('@') {
                    return Err(ConfigError::Invalid(
                        "ZFS snapshots need `server.snapshot.zfs_dataset`; it must not start with '-' or contain '@'"
                            .into(),
                    ));
                }
                if snapshot.btrfs_snapshot_dir.is_some() {
                    return Err(ConfigError::Invalid(
                        "a ZFS snapshot backend cannot also set `btrfs_snapshot_dir`".into(),
                    ));
                }
                if snapshot.ebs.is_some() {
                    return Err(ConfigError::Invalid(
                        "a ZFS snapshot backend cannot also set `ebs`".into(),
                    ));
                }
                if snapshot.lvm.is_some() {
                    return Err(ConfigError::Invalid(
                        "a ZFS snapshot backend cannot also set `lvm`".into(),
                    ));
                }
            }
            SnapshotBackend::Btrfs => {
                if snapshot.btrfs_snapshot_dir.is_none() {
                    return Err(ConfigError::Invalid(
                        "Btrfs snapshots need `server.snapshot.btrfs_snapshot_dir`".into(),
                    ));
                }
                if snapshot.zfs_dataset.is_some() {
                    return Err(ConfigError::Invalid(
                        "a Btrfs snapshot backend cannot also set `zfs_dataset`".into(),
                    ));
                }
                if snapshot.ebs.is_some() {
                    return Err(ConfigError::Invalid(
                        "a Btrfs snapshot backend cannot also set `ebs`".into(),
                    ));
                }
                if snapshot.lvm.is_some() {
                    return Err(ConfigError::Invalid(
                        "a Btrfs snapshot backend cannot also set `lvm`".into(),
                    ));
                }
            }
            SnapshotBackend::Lvm => {
                if snapshot.zfs_dataset.is_some()
                    || snapshot.btrfs_snapshot_dir.is_some()
                    || snapshot.ebs.is_some()
                {
                    return Err(ConfigError::Invalid(
                        "an LVM snapshot backend cannot set ZFS, Btrfs, or EBS provider settings"
                            .into(),
                    ));
                }
                let lvm = snapshot.lvm.as_ref().ok_or_else(|| {
                    ConfigError::Invalid("LVM snapshots need `server.snapshot.lvm` settings".into())
                })?;
                if self.server.control.unix_socket.is_none() {
                    return Err(ConfigError::Invalid(
                        "LVM snapshots need an absolute `server.control.unix_socket` for the privileged snapshot agent"
                            .into(),
                    ));
                }
                if lvm.source_mount.as_os_str().is_empty()
                    || lvm.mount_dir.as_os_str().is_empty()
                    || lvm.source_mount == lvm.mount_dir
                    || lvm.mount_dir.starts_with(&lvm.source_mount)
                {
                    return Err(ConfigError::Invalid(
                        "LVM snapshots need non-empty paths and `mount_dir` must be outside `source_mount`"
                            .into(),
                    ));
                }
                for (field, value) in [
                    ("volume_group", lvm.volume_group.as_str()),
                    ("logical_volume", lvm.logical_volume.as_str()),
                ] {
                    if !valid_lvm_name(value) {
                        return Err(ConfigError::Invalid(format!(
                            "`server.snapshot.lvm.{field}` must start with an ASCII alphanumeric and contain only ASCII alphanumerics, '+', '_', '.', or '-'"
                        )));
                    }
                }
                if !matches!(lvm.filesystem.as_str(), "ext4" | "xfs") {
                    return Err(ConfigError::Invalid(
                        "`server.snapshot.lvm.filesystem` must be `ext4` or `xfs`".into(),
                    ));
                }
                if lvm.snapshot_size_gib == 0 {
                    return Err(ConfigError::Invalid(
                        "`server.snapshot.lvm.snapshot_size_gib` must be positive".into(),
                    ));
                }
                if lvm.operation_timeout_secs == 0 {
                    return Err(ConfigError::Invalid(
                        "`server.snapshot.lvm.operation_timeout_secs` must be positive".into(),
                    ));
                }
            }
            SnapshotBackend::Ebs => {
                if snapshot.zfs_dataset.is_some()
                    || snapshot.btrfs_snapshot_dir.is_some()
                    || snapshot.lvm.is_some()
                {
                    return Err(ConfigError::Invalid(
                        "an EBS snapshot backend cannot set ZFS, Btrfs, or LVM provider settings"
                            .into(),
                    ));
                }
                let ebs = snapshot.ebs.as_ref().ok_or_else(|| {
                    ConfigError::Invalid("EBS snapshots need `server.snapshot.ebs` settings".into())
                })?;
                for (field, value) in [
                    ("region", ebs.region.as_str()),
                    ("volume_id", ebs.volume_id.as_str()),
                ] {
                    if value.is_empty()
                        || value.starts_with('-')
                        || value.bytes().any(|byte| {
                            !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                        })
                    {
                        return Err(ConfigError::Invalid(format!(
                            "`server.snapshot.ebs.{field}` must contain only lowercase ASCII letters, digits, and hyphens"
                        )));
                    }
                }
                for (field, value, prefix) in [("volume_id", ebs.volume_id.as_str(), "vol-")] {
                    if value.len() == prefix.len()
                        || !value.starts_with(prefix)
                        || !value[prefix.len()..]
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                    {
                        return Err(ConfigError::Invalid(format!(
                            "`server.snapshot.ebs.{field}` is not a valid {prefix} identifier"
                        )));
                    }
                }
                if ebs.source_mount.as_os_str().is_empty() {
                    return Err(ConfigError::Invalid(
                        "EBS snapshots need a non-empty `source_mount` path".into(),
                    ));
                }
                if !matches!(ebs.filesystem.as_str(), "ext4" | "xfs") {
                    return Err(ConfigError::Invalid(
                        "`server.snapshot.ebs.filesystem` must be `ext4` or `xfs`".into(),
                    ));
                }
                if ebs.operation_timeout_secs == 0 {
                    return Err(ConfigError::Invalid(
                        "`server.snapshot.ebs.operation_timeout_secs` must be positive".into(),
                    ));
                }
                if let Some(key) = ebs.resource_tags.keys().find(|key| {
                    key.is_empty() || matches!(key.as_str(), "yesno:database" | "yesno:lease")
                }) {
                    return Err(ConfigError::Invalid(format!(
                        "`server.snapshot.ebs.resource_tags` contains empty or reserved key `{key}`"
                    )));
                }
                if let Some((key, _)) = ebs
                    .resource_tags
                    .iter()
                    .find(|(key, value)| key.len() > 128 || value.len() > 256)
                {
                    return Err(ConfigError::Invalid(format!(
                        "`server.snapshot.ebs.resource_tags.{key}` exceeds the AWS tag length limit"
                    )));
                }
                match ebs.materialization {
                    EbsMaterialization::Local => {
                        // Local materialization mounts, and the mount belongs
                        // to the privileged agent, which reaches the daemon
                        // only over this socket.
                        if self.server.control.unix_socket.is_none() {
                            return Err(ConfigError::Invalid(
                                "local EBS materialization needs an absolute `server.control.unix_socket` for the privileged snapshot agent"
                                    .into(),
                            ));
                        }
                        if ebs.mount_dir.as_os_str().is_empty() || ebs.source_mount == ebs.mount_dir
                        {
                            return Err(ConfigError::Invalid(
                                "local EBS materialization needs a non-empty `mount_dir` distinct from `source_mount`"
                                    .into(),
                            ));
                        }
                        for (field, value) in [
                            ("instance_id", ebs.instance_id.as_str()),
                            ("availability_zone", ebs.availability_zone.as_str()),
                        ] {
                            if value.is_empty()
                                || value.starts_with('-')
                                || value.bytes().any(|byte| {
                                    !(byte.is_ascii_lowercase()
                                        || byte.is_ascii_digit()
                                        || byte == b'-')
                                })
                            {
                                return Err(ConfigError::Invalid(format!(
                                    "`server.snapshot.ebs.{field}` must contain only lowercase ASCII letters, digits, and hyphens"
                                )));
                            }
                        }
                        if ebs.instance_id.len() == "i-".len()
                            || !ebs.instance_id.starts_with("i-")
                            || !ebs.instance_id["i-".len()..]
                                .bytes()
                                .all(|byte| byte.is_ascii_hexdigit())
                        {
                            return Err(ConfigError::Invalid(
                                "`server.snapshot.ebs.instance_id` is not a valid i- identifier"
                                    .into(),
                            ));
                        }
                        if ebs.partition == Some(0) {
                            return Err(ConfigError::Invalid(
                                "`server.snapshot.ebs.partition` must be positive when set".into(),
                            ));
                        }
                        let mut device_names = std::collections::HashSet::new();
                        if ebs.device_names.is_empty()
                            || ebs.device_names.iter().any(|name| {
                                !name.starts_with("/dev/sd")
                                    || name.len() != 8
                                    || !name.as_bytes()[7].is_ascii_lowercase()
                                    || !device_names.insert(name)
                            })
                        {
                            return Err(ConfigError::Invalid(
                                "`server.snapshot.ebs.device_names` must contain unique `/dev/sdX` attachment names"
                                    .into(),
                            ));
                        }
                    }
                    EbsMaterialization::Deferred => {
                        if ebs.partition.is_some() {
                            return Err(ConfigError::Invalid(
                                "deferred EBS materialization requires a whole-volume filesystem; `server.snapshot.ebs.partition` must be omitted"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        }
        let telemetry = &self.server.telemetry;
        if !telemetry.sample_ratio.is_finite() || !(0.0..=1.0).contains(&telemetry.sample_ratio) {
            return Err(ConfigError::Invalid(format!(
                "`server.telemetry.sample_ratio` must be between 0 and 1, got {}",
                telemetry.sample_ratio
            )));
        }
        if !telemetry.sampler.uses_ratio() && telemetry.sample_ratio != 1.0 {
            return Err(ConfigError::Invalid(format!(
                "`server.telemetry.sample_ratio` is set to {} but sampler `{:?}` does not \
                 use a ratio",
                telemetry.sample_ratio, telemetry.sampler
            )));
        }
        if !telemetry.enabled && telemetry.endpoint.is_some() {
            return Err(ConfigError::Invalid(
                "`server.telemetry.endpoint` is set while OpenTelemetry is disabled".into(),
            ));
        }
        if telemetry.enabled {
            if telemetry.service_name.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "enabled `server.telemetry` needs a non-empty `service_name`".into(),
                ));
            }
            if let Some(endpoint) = telemetry.endpoint.as_deref() {
                if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
                    return Err(ConfigError::Invalid(format!(
                        "`server.telemetry.endpoint` is `{endpoint}`; expected an \
                         `http://` or `https://` OTLP/gRPC collector endpoint"
                    )));
                }
            }
        }

        self.validate_principals()?;
        let control = &self.server.control;
        if self.control_enabled() != !control.journal_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "a `server.control.listen` or `server.control.unix_socket` and \
                 `server.control.journal_dir` must either both be set or both be empty"
                    .into(),
            ));
        }
        if let Some(path) = &control.unix_socket {
            if !path.is_absolute() {
                return Err(ConfigError::Invalid(format!(
                    "`server.control.unix_socket` is `{}`; use an absolute path",
                    path.display()
                )));
            }
        }
        if let Some(mode) = &control.unix_socket_mode {
            let parsed = parse_socket_mode(mode).map_err(ConfigError::Invalid)?;
            if control.unix_socket.is_none() {
                return Err(ConfigError::Invalid(
                    "`server.control.unix_socket_mode` needs a `server.control.unix_socket`".into(),
                ));
            }
            // A socket nobody may open is a configuration mistake that presents
            // as a hang at connect time, so refuse it here instead.
            if parsed & 0o600 != 0o600 {
                return Err(ConfigError::Invalid(format!(
                    "`server.control.unix_socket_mode` is `{mode}`; the owner needs read and \
                     write on its own socket"
                )));
            }
        }
        let metrics = self.metrics_addr()?;
        let control_addr = self.control_addr()?;
        if let Some(endpoint) = control_addr {
            if metrics.is_some_and(|m| collides(m, endpoint)) {
                return Err(ConfigError::Invalid(format!(
                    "`server.control.listen` and `server.metrics.listen` are both {endpoint}"
                )));
            }
            let tls = &control.tls;
            match (&tls.cert, &tls.key) {
                (Some(_), Some(_)) | (None, None) => {}
                _ => {
                    return Err(ConfigError::Invalid(
                        "`server.control.tls` needs both `cert` and `key`, or neither".into(),
                    ));
                }
            }
            if tls.client_ca.is_some() && !tls.is_enabled() {
                return Err(ConfigError::Invalid(
                    "`server.control.tls.client_ca` needs a server `cert` and `key`".into(),
                ));
            }
            if tls.require_client_auth && tls.client_ca.is_none() {
                return Err(ConfigError::Invalid(
                    "`server.control.tls.require_client_auth` needs `client_ca`".into(),
                ));
            }
            if !endpoint.ip().is_loopback() && !tls.is_enabled() && !insecure {
                return Err(ConfigError::Invalid(format!(
                    "refusing to bind shared control endpoint {endpoint} without TLS; it serves \
                     lifecycle commands and complete database images. Configure \
                     `server.control.tls` or pass `--insecure`."
                )));
            }
        }
        if self.control_enabled() {
            if self.auth.rules.is_empty() {
                return Err(ConfigError::Invalid(
                    "the shared control endpoint needs at least one `[[auth.rule]]`; unmatched \
                     requests are denied"
                        .into(),
                ));
            }
            for rule in &self.auth.rules {
                crate::auth::parse_hba_address(&rule.address).map_err(ConfigError::Invalid)?;
                if rule.principal != "all"
                    && !(rule.channel == AuthzChannel::Local
                        && rule
                            .principal
                            .strip_prefix("uid:")
                            .is_some_and(|uid| uid.parse::<u32>().is_ok()))
                    && !self
                        .auth
                        .principals
                        .iter()
                        .any(|p| p.name == rule.principal)
                {
                    return Err(ConfigError::Invalid(format!(
                        "authorization rule names unknown principal `{}`",
                        rule.principal
                    )));
                }
            }
            let replication_open = self.auth.rules.iter().any(|rule| {
                rule.capability == EndpointCapability::Replication
                    && rule.action == RuleAction::Allow
                    && rule.channel.permits_host()
            });
            if replication_open && !insecure_replication {
                if let Some(endpoint) = control_addr {
                    let tls = &control.tls;
                    if !tls.is_enabled() || !self.auth.is_enforcing() {
                        return Err(ConfigError::Invalid(format!(
                            "refusing to authorize replication on {endpoint} without TLS and an \
                             authenticated principal; `FetchBaseSnapshot` streams whole database \
                             images. Configure both, or pass `--insecure-replication`."
                        )));
                    }
                    if self.auth.rules.iter().any(|rule| {
                        rule.capability == EndpointCapability::Replication
                            && rule.action == RuleAction::Allow
                            && rule.channel.permits_host()
                            && rule.principal == "all"
                    }) {
                        return Err(ConfigError::Invalid(
                            "a replication allow rule for principal `all` needs \
                             `--insecure-replication`"
                                .into(),
                        ));
                    }
                }
            }
        }

        // ---- a follower does not own the leader's Flight listener
        if self.server.role == Role::Follower {
            if self.follower.leader.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "`server.role = \"follower\"` needs `follower.leader`, the leader's \
                     replication endpoint — for example `https://a.internal:50052`"
                        .into(),
                ));
            }
            if !self.follower.leader.starts_with("http://")
                && !self.follower.leader.starts_with("https://")
            {
                return Err(ConfigError::Invalid(format!(
                    "`follower.leader` is `{}`; it must be a URL beginning `http://` or \
                     `https://`",
                    self.follower.leader
                )));
            }
            // A deployment may demand a client certificate, so a follower with
            // a CA and half an identity of its own should fail here, not during
            // a handshake.
            match (&self.follower.tls.cert, &self.follower.tls.key) {
                (Some(_), Some(_)) | (None, None) => {}
                _ => {
                    return Err(ConfigError::Invalid(
                        "`follower.tls` needs both `cert` and `key`, or neither".into(),
                    ))
                }
            }
            if self.follower.leader.starts_with("https://") && !self.follower.tls.is_enabled() {
                return Err(ConfigError::Invalid(
                    "`follower.leader` is an https:// endpoint but `follower.tls` is empty, \
                     so there is nothing to verify the leader's certificate against"
                        .into(),
                ));
            }
            if self.db.shards == 0 {
                return Err(ConfigError::Invalid(
                    "`db.shards` must be at least 1".into(),
                ));
            }
            if !self.follower.serve_reads {
                return Ok(());
            }
        }

        let addr = self.flight_addr()?;
        // Port 0 is exempt, and not as a special case for tests: it means
        // "let the OS choose", so two listeners asking for it get two different
        // ports and collide with nothing. Comparing the configured strings would
        // reject a perfectly good configuration.
        if metrics.is_some_and(|m| collides(m, addr)) {
            return Err(ConfigError::Invalid(format!(
                "`server.metrics.listen` and `server.flight.listen` are both {addr}; they \
                 are different protocols and cannot share a socket"
            )));
        }

        if control_addr.is_some_and(|control| collides(control, addr)) {
            return Err(ConfigError::Invalid(format!(
                "`server.control.listen` and `server.flight.listen` are both {addr}"
            )));
        }

        let tls = &self.server.flight.tls;

        // A plaintext, unauthenticated Flight port is an open write surface:
        // `do_put` ingests. Refusing a public bind without either defence is the
        // only honest default; `--insecure` exists so that "I know" is
        // something an operator has to say out loud.
        if !addr.ip().is_loopback() && !tls.is_enabled() && !insecure {
            return Err(ConfigError::Invalid(format!(
                "refusing to bind {addr}: it is not a loopback address and no TLS is \
                 configured, so `do_put` would be open in \
                 the clear to anyone who can reach the port. Configure \
                 `server.flight.tls`, bind a loopback address, or pass `--insecure` if the \
                 network is genuinely trusted."
            )));
        }

        // A half-configured identity is worse than none: it looks like TLS.
        match (&tls.cert, &tls.key) {
            (Some(_), Some(_)) | (None, None) => {}
            (Some(_), None) => {
                return Err(ConfigError::Invalid(
                    "`server.flight.tls.cert` is set without `key`".into(),
                ))
            }
            (None, Some(_)) => {
                return Err(ConfigError::Invalid(
                    "`server.flight.tls.key` is set without `cert`".into(),
                ))
            }
        }
        if tls.client_ca.is_some() && !tls.is_enabled() {
            return Err(ConfigError::Invalid(
                "`server.flight.tls.client_ca` asks clients for certificates, but the \
                 server has none of its own — mutual TLS is still TLS. Set `cert` and `key`."
                    .into(),
            ));
        }
        if tls.require_client_auth && tls.client_ca.is_none() {
            return Err(ConfigError::Invalid(
                "`server.flight.tls.require_client_auth` is set with no `client_ca`, so \
                 there is nothing to verify a client certificate against and every client \
                 would be refused"
                    .into(),
            ));
        }

        // An unauthenticated server is a loopback-only server. See
        // `AuthConfig::is_enforcing` for why "no principals" means open rather
        // than closed; this is the other half of that bargain, and without it
        // the friendly default would become a public one the moment somebody
        // configured TLS.
        if !self.auth.is_enforcing() && !addr.ip().is_loopback() && !insecure {
            return Err(ConfigError::Invalid(format!(
                "refusing to bind {addr}: no `[[auth.principal]]` is configured, so every \
                 caller would be an unauthenticated administrator and `do_put` ingests. \
                 Configure a principal, bind a \
                 loopback address, or pass `--insecure`."
            )));
        }

        if self.db.shards == 0 {
            return Err(ConfigError::Invalid(
                "`db.shards` must be at least 1".into(),
            ));
        }
        if self.db.max_readers == 0 {
            return Err(ConfigError::Invalid(
                "`db.max_readers` must be at least 1".into(),
            ));
        }
        // **`0` reads as "off" and means "always".** `CheckpointPolicy::
        // should_checkpoint` is `.. || elapsed_secs >= self.interval_secs`, so
        // zero makes that arm unconditionally true and **every commit
        // checkpoints** — serializing dirty chunks, rebuilding the index and
        // fsyncing once per shard, per write. An operator setting this to
        // disable background checkpointing would get the most expensive
        // possible behaviour instead, and would get it silently.
        //
        // There is deliberately no "disable" value: the daemon's ticker is the
        // only thing that makes this trigger fire at all, and a database that
        // never checkpoints grows its memtable and its log without bound.
        if self.db.checkpoint.interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "`db.checkpoint.interval_secs` is 0, which the engine reads as \"every \
                 commit\" rather than \"never\": the elapsed-time trigger is a `>=`, so \
                 zero makes it always true and every single write would take a full \
                 checkpoint. Set a positive number of seconds."
                    .into(),
            ));
        }
        if self.db.checkpoint.max_dirty_bytes.0 < self.db.checkpoint.dirty_bytes.0 {
            return Err(ConfigError::Invalid(format!(
                "`db.checkpoint.max_dirty_bytes` ( {} ) is below `dirty_bytes` ( {} ); the \
                 write stall would fire before the checkpoint it is supposed to backstop",
                self.db.checkpoint.max_dirty_bytes.0, self.db.checkpoint.dirty_bytes.0
            )));
        }
        if self.db.checkpoint.max_wal_bytes.0 < self.db.checkpoint.wal_bytes.0 {
            return Err(ConfigError::Invalid(format!(
                "`db.checkpoint.max_wal_bytes` ( {} ) is below `wal_bytes` ( {} ); the hard \
                 log bound would fire before the checkpoint trigger",
                self.db.checkpoint.max_wal_bytes.0, self.db.checkpoint.wal_bytes.0
            )));
        }
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        // `validate` has already refused a config without one.
        self.server
            .data_dir
            .as_deref()
            .expect("validated config has a data_dir")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_sampler_is_configurable_and_validated() {
        let mut config: Config = toml::from_str(
            r#"
                [server]
                data_dir = "/tmp/does-not-need-to-exist"

                [server.telemetry]
                enabled = true
                endpoint = "http://collector:4317"
                service_name = "yesnod-test"
                sampler = "parent_based_trace_id_ratio"
                sample_ratio = 0.125
            "#,
        )
        .unwrap();
        config.validate(false).unwrap();
        assert_eq!(
            config.server.telemetry.sampler,
            TraceSampler::ParentBasedTraceIdRatio
        );
        assert_eq!(config.server.telemetry.sample_ratio, 0.125);

        config.server.telemetry.sample_ratio = 1.01;
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("between 0 and 1"), "{error}");

        config.server.telemetry.sample_ratio = 0.5;
        config.server.telemetry.sampler = TraceSampler::AlwaysOn;
        let error = config.validate(false).unwrap_err();
        assert!(
            error.to_string().contains("does not use a ratio"),
            "{error}"
        );

        config.server.telemetry.sample_ratio = 1.0;
        config.server.telemetry.enabled = false;
        let error = config.validate(false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("while OpenTelemetry is disabled"),
            "{error}"
        );
    }

    #[test]
    fn byte_sizes_accept_what_they_should_and_refuse_what_is_ambiguous() {
        assert_eq!(Bytes::parse("1024").unwrap().0, 1024);
        assert_eq!(Bytes::parse("0").unwrap().0, 0);
        assert_eq!(Bytes::parse("1B").unwrap().0, 1);
        assert_eq!(Bytes::parse("1KiB").unwrap().0, 1024);
        assert_eq!(Bytes::parse("256MiB").unwrap().0, 256 << 20);
        assert_eq!(Bytes::parse("1GiB").unwrap().0, 1 << 30);
        assert_eq!(Bytes::parse(" 4 MiB ").unwrap().0, 4 << 20);
        assert_eq!(Bytes::parse("2mib").unwrap().0, 2 << 20);

        // The point of the whole type: `MB` is a disagreement, not a unit.
        assert!(Bytes::parse("1MB").is_err());
        assert!(Bytes::parse("1kb").is_err());
        assert!(Bytes::parse("1G").is_err());
        assert!(Bytes::parse("").is_err());
        assert!(Bytes::parse("MiB").is_err());
        assert!(Bytes::parse("many").is_err());
        assert!(Bytes::parse("18446744073709551615TiB").is_err());
    }

    #[test]
    fn the_defaults_are_the_engines_own() {
        // Not a restatement: if `DbOptions::default` moves, this catches the
        // config drifting away from it rather than silently overriding it.
        let d = DbOptions::default();
        let c: DbOptions = DbConfig::default().into();
        assert_eq!(c.shards, d.shards);
        assert_eq!(c.max_readers, d.max_readers);
        assert_eq!(c.commit_ring, d.commit_ring);
        assert_eq!(c.rebuild_alloc_on_open, d.rebuild_alloc_on_open);
        assert_eq!(c.evacuate_per_checkpoint, d.evacuate_per_checkpoint);

        let p = CheckpointPolicy::default();
        assert_eq!(c.policy.dirty_bytes, p.dirty_bytes);
        assert_eq!(c.policy.max_dirty_bytes, p.max_dirty_bytes);
        assert_eq!(c.policy.wal_bytes, p.wal_bytes);
        assert_eq!(c.policy.interval_secs, p.interval_secs);
        assert_eq!(c.policy.max_wal_bytes, p.max_wal_bytes);
    }

    fn with_dir(mut c: Config) -> Config {
        c.server.data_dir = Some(PathBuf::from("/tmp/does-not-need-to-exist"));
        c
    }

    #[test]
    fn filesystem_snapshot_backends_are_explicit_and_provider_specific() {
        let mut config = with_dir(Config::default());
        config.server.snapshot.allow_direct_path = true;
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("disabled"), "{error}");

        config.server.snapshot.backend = SnapshotBackend::Zfs;
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("zfs_dataset"), "{error}");
        config.server.snapshot.zfs_dataset = Some("tank/yesno".into());
        config.validate(false).unwrap();

        config.server.snapshot.backend = SnapshotBackend::Btrfs;
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("btrfs_snapshot_dir"), "{error}");
        config.server.snapshot.zfs_dataset = None;
        config.server.snapshot.btrfs_snapshot_dir = Some("/srv/yesno-snapshots".into());
        config.validate(false).unwrap();

        config.server.snapshot.backend = SnapshotBackend::Lvm;
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("LVM"), "{error}");
        config.server.snapshot.btrfs_snapshot_dir = None;
        config.server.snapshot.lvm = Some(LvmSnapshotConfig {
            source_mount: "/var/lib/yesno".into(),
            volume_group: "data-vg".into(),
            logical_volume: "yesno".into(),
            mount_dir: "/var/lib/yesno-lvm-snapshots".into(),
            filesystem: "xfs".into(),
            snapshot_size_gib: 16,
            operation_timeout_secs: 300,
        });
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("control.unix_socket"), "{error}");
        config.server.control.unix_socket = Some("/run/yesno/control.sock".into());
        config.server.control.journal_dir = "/var/lib/yesno-control".into();
        config.auth.rules.push(AuthzRule {
            channel: AuthzChannel::Local,
            principal: "all".into(),
            address: "all".into(),
            capability: EndpointCapability::Replication,
            action: RuleAction::Allow,
        });
        config.validate(false).unwrap();
        config.server.snapshot.lvm.as_mut().unwrap().logical_volume = "-unsafe".into();
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("logical_volume"), "{error}");
        config.server.snapshot.lvm.as_mut().unwrap().logical_volume = "yesno".into();
        config.server.snapshot.lvm.as_mut().unwrap().mount_dir = "/var/lib/yesno/snapshots".into();
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("outside"), "{error}");

        config.server.snapshot.backend = SnapshotBackend::Ebs;
        config.server.snapshot.lvm = None;
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("snapshot.ebs"), "{error}");
        config.server.snapshot.ebs = Some(EbsSnapshotConfig {
            region: "ap-northeast-1".into(),
            volume_id: "vol-0123456789abcdef0".into(),
            instance_id: "i-0123456789abcdef0".into(),
            availability_zone: "ap-northeast-1a".into(),
            source_mount: "/var/lib/yesno".into(),
            mount_dir: "/var/lib/yesno-ebs-snapshots".into(),
            ..EbsSnapshotConfig::default()
        });
        config.validate(false).unwrap();
        config
            .server
            .snapshot
            .ebs
            .as_mut()
            .unwrap()
            .resource_tags
            .insert("yesno:database".into(), "not-allowed".into());
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("reserved"), "{error}");
        config
            .server
            .snapshot
            .ebs
            .as_mut()
            .unwrap()
            .resource_tags
            .clear();
        config.server.snapshot.ebs.as_mut().unwrap().instance_id = "i-'".into();
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("instance_id"), "{error}");
        config.server.snapshot.ebs.as_mut().unwrap().instance_id = "i-0123456789abcdef0".into();

        let ebs = config.server.snapshot.ebs.as_mut().unwrap();
        ebs.materialization = EbsMaterialization::Deferred;
        ebs.instance_id.clear();
        ebs.availability_zone.clear();
        ebs.mount_dir = PathBuf::new();
        ebs.device_names.clear();
        config.validate(false).unwrap();
        config.server.snapshot.ebs.as_mut().unwrap().partition = Some(1);
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("whole-volume"), "{error}");
        config.server.snapshot.ebs.as_mut().unwrap().partition = None;

        config.server.snapshot.lease_ttl_secs = 0;
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("must be positive"), "{error}");
    }

    #[test]
    fn a_config_without_a_data_dir_is_refused() {
        let e = Config::default().validate(false).unwrap_err();
        assert!(format!("{e}").contains("data_dir"), "{e}");
    }

    #[test]
    fn a_public_bind_without_security_is_refused_unless_said_out_loud() {
        let mut c = with_dir(Config::default());
        c.server.flight.listen = "0.0.0.0:50051".into();
        let e = c.validate(false).unwrap_err();
        assert!(format!("{e}").contains("refusing to bind"), "{e}");
        // And the escape hatch works, because an operator on a trusted network
        // must not be stuck.
        c.validate(true).expect("--insecure must permit it");
    }

    #[test]
    fn loopback_needs_no_flag() {
        with_dir(Config::default()).validate(false).unwrap();
    }

    #[test]
    fn a_listen_address_that_is_not_one_is_refused() {
        let mut c = with_dir(Config::default());
        c.server.flight.listen = "not-an-address".into();
        assert!(c.validate(false).is_err());
    }

    #[test]
    fn a_follower_is_validated_against_its_own_rules_not_the_leaders() {
        // A standby serves nothing, so the leader's bind rules do not apply to
        // it — and applying them anyway would refuse a perfectly good standby for
        // having no Flight listener. What it needs instead is somewhere to
        // follow.
        let mut c = with_dir(Config::default());
        c.server.role = Role::Follower;
        let e = c.validate(false).unwrap_err();
        assert!(format!("{e}").contains("follower.leader"), "{e}");

        c.follower.leader = "http://a.internal:50052".into();
        c.validate(false)
            .expect("a standby with a leader needs nothing else");

        // And the leader's own checks still bite a leader.
        let mut l = with_dir(Config::default());
        l.server.flight.listen = "0.0.0.0:50051".into();
        assert!(l.validate(false).is_err());
    }

    #[test]
    fn a_follower_still_validates_shared_endpoint_principals() {
        let mut config = with_dir(Config::default());
        config.server.role = Role::Follower;
        config.follower.leader = "http://a.internal:50052".into();
        config.auth.principals.push(PrincipalConfig {
            name: "standby-operator".into(),
            role: PrincipalRole::Admin,
            token_sha256: Some("not-a-sha256".into()),
            cert_sha256: None,
        });

        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("64 hex characters"), "{error}");
    }

    #[test]
    fn a_read_serving_follower_still_validates_its_flight_listener() {
        let mut config = with_dir(Config::default());
        config.server.role = Role::Follower;
        config.follower.leader = "http://a.internal:50052".into();
        config.follower.serve_reads = true;
        config.server.flight.listen = "0.0.0.0:50051".into();

        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("refusing to bind"), "{error}");
    }

    #[test]
    fn two_listeners_on_the_same_real_port_are_refused_but_port_zero_is_not() {
        let mut c = with_dir(Config::default());
        c.server.flight.listen = "127.0.0.1:9999".into();
        c.server.metrics.listen = "127.0.0.1:9999".into();
        assert!(c.validate(false).is_err());

        // Both asking for port 0 is fine: the OS hands out two different
        // ports. Rejecting it would refuse a valid configuration, and it is
        // what every test in this crate uses.
        c.server.flight.listen = "127.0.0.1:0".into();
        c.server.metrics.listen = "127.0.0.1:0".into();
        c.validate(false)
            .expect("port 0 twice must be permitted; it is not a collision");
    }

    #[test]
    fn a_zero_checkpoint_interval_is_refused() {
        // The engine reads 0 as "always", not "never" — see `validate`.
        let mut c = with_dir(Config::default());
        c.db.checkpoint.interval_secs = 0;
        let e = c.validate(false).unwrap_err();
        assert!(format!("{e}").contains("every commit"), "{e}");
    }

    #[test]
    fn the_metrics_listener_can_be_disabled_by_emptying_it() {
        let mut c = with_dir(Config::default());
        c.server.metrics.listen = String::new();
        c.validate(false).unwrap();
        assert!(c.metrics_addr().unwrap().is_none());
    }

    #[test]
    fn inverted_checkpoint_thresholds_are_refused() {
        let mut c = with_dir(Config::default());
        c.db.checkpoint.max_dirty_bytes = Bytes(1);
        assert!(c.validate(false).is_err());

        let mut c = with_dir(Config::default());
        c.db.checkpoint.max_wal_bytes = Bytes(1);
        assert!(c.validate(false).is_err());
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        // `deny_unknown_fields`, because a typo in a config key is otherwise
        // a setting that silently does not apply — the single most common way
        // an operator's intent is lost.
        let e = toml::from_str::<Config>("[db]\nshardz = 4\n").unwrap_err();
        assert!(format!("{e}").contains("shardz"), "{e}");
    }

    #[test]
    fn a_file_round_trips_through_the_effective_form() {
        let text = r#"
[server]
data_dir = "/var/lib/yesno"

[server.flight]
listen = "127.0.0.1:7000"

[db]
shards = 4

[db.checkpoint]
dirty_bytes = "8MiB"
interval_secs = 5
"#;
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.db.shards, 4);
        assert_eq!(c.db.checkpoint.dirty_bytes.0, 8 << 20);
        assert_eq!(c.db.checkpoint.interval_secs, 5);
        // Untouched keys keep the engine's defaults rather than becoming zero.
        assert_eq!(
            c.db.max_readers,
            DbOptions::default().max_readers,
            "an unset key must inherit the default, not be zeroed"
        );
        c.validate(false).unwrap();

        // What `--check-config` prints must itself be loadable.
        let printed = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&printed).unwrap();
        assert_eq!(back.db.shards, 4);
        assert_eq!(back.db.checkpoint.dirty_bytes.0, 8 << 20);
    }

    #[test]
    fn control_endpoint_and_journal_are_configured_as_one_unit() {
        let mut config = with_dir(Config::default());
        config.server.control.listen = "127.0.0.1:50052".into();
        let error = config.validate(false).unwrap_err();
        assert!(error.to_string().contains("must either both be set"));

        config.server.control.journal_dir = "/var/lib/yesno-control".into();
        config.auth.rules.push(AuthzRule {
            channel: AuthzChannel::Host,
            principal: "all".into(),
            address: "127.0.0.0/8".into(),
            capability: EndpointCapability::ControlRead,
            action: RuleAction::Allow,
        });
        config.validate(false).unwrap();
        assert_eq!(
            config.control_addr().unwrap(),
            Some("127.0.0.1:50052".parse().unwrap())
        );
        assert_eq!(
            config.control_journal_dir(),
            Some(Path::new("/var/lib/yesno-control"))
        );

        config.server.control.listen = String::new();
        assert!(config.validate(false).is_err());
    }

    #[test]
    fn a_unix_socket_can_be_the_only_control_channel() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = with_dir(Config::default());
        config.server.control.unix_socket = Some(dir.path().join("control.sock"));
        config.server.control.journal_dir = dir.path().join("journal");
        config.auth.rules.push(AuthzRule {
            channel: AuthzChannel::Local,
            principal: "uid:1234".into(),
            address: "all".into(),
            capability: EndpointCapability::ControlRead,
            action: RuleAction::Allow,
        });

        config.validate(false).unwrap();
        assert_eq!(
            config.control_unix_socket(),
            Some(dir.path().join("control.sock").as_path())
        );
        assert_eq!(config.control_addr().unwrap(), None);
    }

    #[test]
    fn the_control_socket_mode_is_octal_and_must_leave_the_owner_able_to_use_it() {
        assert_eq!(parse_socket_mode("0660").unwrap(), 0o660);
        assert_eq!(parse_socket_mode("660").unwrap(), 0o660);
        assert_eq!(parse_socket_mode("0o660").unwrap(), 0o660);
        assert_eq!(parse_socket_mode(" 0770 ").unwrap(), 0o770);

        // The dangerous misreading: 660 decimal is 0o1224, which sets setuid
        // and clears owner write. The parser is octal precisely so the most
        // likely thing an operator types cannot mean that.
        assert_ne!(parse_socket_mode("660").unwrap(), 660);
        assert!(parse_socket_mode("0999").is_err());
        assert!(parse_socket_mode("1777").is_err());
        assert!(parse_socket_mode("rw-rw----").is_err());
    }

    #[test]
    fn a_control_socket_mode_needs_a_socket_and_an_owner_who_can_open_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = with_dir(Config::default());
        // A TCP listener keeps the endpoint enabled while the socket is absent,
        // so the pairing rule does not mask the check under test.
        config.server.control.listen = "127.0.0.1:0".into();
        config.server.control.journal_dir = dir.path().join("journal");
        config.server.control.unix_socket_mode = Some("0660".into());
        config.auth.rules.push(AuthzRule {
            channel: AuthzChannel::Local,
            principal: "uid:0".into(),
            address: "all".into(),
            capability: EndpointCapability::ControlRead,
            action: RuleAction::Allow,
        });

        // A mode without a socket is a mistake that would otherwise be silently
        // ignored, because nothing else reads the field.
        let error = config.validate(false).unwrap_err().to_string();
        assert!(
            error.contains("needs a `server.control.unix_socket`"),
            "{error}"
        );

        config.server.control.unix_socket = Some(dir.path().join("control.sock"));
        config.validate(false).unwrap();
        assert_eq!(config.control_unix_socket_mode(), Some(0o660));

        // A socket its own owner cannot open presents as a hang at connect
        // time, so it is refused at configuration time instead.
        config.server.control.unix_socket_mode = Some("0060".into());
        let error = config.validate(false).unwrap_err().to_string();
        assert!(error.contains("owner needs read and write"), "{error}");
    }

    /// The unified container image's ENTRYPOINT is a dispatcher: a first
    /// argument naming one of the shipped binaries selects it, and anything
    /// else is `yesnod`'s own argv. That disambiguation is only sound because
    /// `Cli` has no positional argument -- every option is a `--flag` -- so a
    /// bare word in the first position can never be something `yesnod` wanted.
    ///
    /// Adding a positional argument to `Cli` silently breaks the image:
    /// `docker run <image> mydb` would dispatch on `mydb` instead of passing
    /// it through, and an ECS command override beginning with a binary name
    /// would become ambiguous. If a positional argument is genuinely needed,
    /// `dist/entrypoint.sh` has to grow an explicit separator first.
    #[test]
    fn yesnod_takes_no_positional_argument() {
        use clap::CommandFactory as _;

        let positionals: Vec<_> = Cli::command()
            .get_arguments()
            .filter(|arg| arg.is_positional())
            .map(|arg| arg.get_id().to_string())
            .collect();

        assert!(
            positionals.is_empty(),
            "`Cli` grew positional argument(s) {positionals:?}; the container \
             image's entrypoint dispatcher in `dist/entrypoint.sh` distinguishes \
             a binary name from yesnod's argv by exactly this property"
        );
    }
}
