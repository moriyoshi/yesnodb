//! `yesno` — talk to a running `yesnod` from a shell.
//!
//! # `count` is not a convenience
//!
//! It issues `get_flight_info` and prints `total_records` **without fetching a
//! single ordinal**. Almost every Flight server answers `-1` there, because
//! counting means executing; yesno answers exactly, from container popcounts in
//! the index. Having that as one command is the cheapest demonstration of the
//! property there is.
//!
//! `get` and `query` print the ticket's snapshot version alongside the count.
//! The ticket pins both operations to the same database version, so the rows
//! and count describe one consistent read even when writes arrive between
//! `get_flight_info` and `do_get`.

use std::process::ExitCode;

use arrow_array::{Array, UInt64Array};
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::{Action, Empty, FlightDescriptor};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use tonic::transport::Channel;

#[derive(Parser, Debug)]
#[command(name = "yesno", about = "Client for a running yesnod", version)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:50051", env = "YESNO_ENDPOINT")]
    endpoint: String,

    /// PEM of the CA that signed the server's certificate.
    #[arg(long, value_name = "FILE", env = "YESNO_CA")]
    ca: Option<std::path::PathBuf>,
    /// Client certificate, for mutual TLS.
    #[arg(long, value_name = "FILE", requires = "key", env = "YESNO_CERT")]
    cert: Option<std::path::PathBuf>,
    #[arg(long, value_name = "FILE", requires = "cert", env = "YESNO_KEY")]
    key: Option<std::path::PathBuf>,
    /// The name to verify the server's certificate against, when it differs
    /// from the host in `--endpoint` — which it does whenever a node is reached
    /// by address but named by DNS in its certificate.
    #[arg(long, value_name = "NAME", env = "YESNO_SERVER_NAME")]
    server_name: Option<String>,

    /// File holding the bearer token.
    ///
    /// A file, and deliberately no `--token`. A secret in argv is readable by
    /// any process on the box through `/proc/<pid>/cmdline`, and it lands in
    /// shell history besides.
    #[arg(long, value_name = "FILE", env = "YESNO_TOKEN_FILE")]
    token_file: Option<std::path::PathBuf>,

    /// Refuse a server serving a leadership term below this.
    ///
    /// The client-facing half of split-brain fencing. A superseded leader does
    /// not know it has been replaced, so it will accept writes that vanish when
    /// it is rebuilt; the caller is the only party that can hold the newer
    /// number. Pass the term the current leader reported — `yesno status`
    /// prints it — and this call refuses anything older.
    #[arg(long, value_name = "N", env = "YESNO_MIN_TERM")]
    min_term: Option<u32>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// The exact cardinality of a key, from the index. Moves no ordinals.
    Count { key: u64 },
    /// Fetch a key's ordinals.
    Get {
        key: u64,
        /// Print at most this many ordinals. The count reported is still the
        /// whole key's.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Evaluate a Boolean expression over stored keys.
    Query {
        /// For example: `and(42,or(7,9))` or `not(range(0,100))`.
        expression: String,
        /// Print at most this many ordinals. The reported count is still exact.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Print only the exact cardinality; fetch no ordinals.
        #[arg(long)]
        count_only: bool,
        /// Read at this database version instead of the newest visible one.
        ///
        /// `put` prints the version it committed at, so this is how a shell
        /// reads back its own write: the server waits briefly for a version
        /// that is merely not visible yet rather than refusing it. The version
        /// is honoured exactly, so a reclaimed one is an error and not a
        /// silently newer answer.
        #[arg(long, value_name = "N")]
        at_version: Option<u64>,
    },
    /// Ingest `key,ordinal` pairs, one per line, from a file or stdin.
    Put {
        /// `-` reads stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Ingest `constituent,ordinal` rows into one packed view key.
    ViewPut {
        /// Physical key that stores the packed view.
        key: u64,
        /// Number of constituent sets in the view.
        #[arg(long)]
        sets: u32,
        /// Select blocked layout with this per-constituent capacity. Without
        /// this option the layout is interleaved.
        #[arg(long)]
        stride: Option<u64>,
        /// `-` reads stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Space and reader counters, as the server reports them.
    Stats,
    /// What this server is and what it will answer.
    Status,
}

/// Let a closed pipe kill this process the way it kills `grep`.
///
/// Rust's runtime ignores `SIGPIPE`, so a write to a closed pipe returns an
/// error and `println!` panics — meaning `yesno get 42 | head` ends in a
/// stack trace rather than in ten lines of output. `get` can print millions of
/// ordinals, so piping it somewhere that stops early is the *expected* use, not
/// an edge case.
#[cfg(unix)]
fn restore_sigpipe() {
    // SAFETY: `signal` with `SIG_DFL` on `SIGPIPE` is async-signal-safe and is
    // called once, before any thread is spawned, so there is no handler to race.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

#[tokio::main]
async fn main() -> ExitCode {
    restore_sigpipe();
    yesno_server::tls::install_crypto_provider();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The **chain**, not just the outermost error. tonic's transport
            // failures render as the bare words "transport error", and every
            // interesting thing — an untrusted certificate, a name that does not
            // match a SAN, a refused connection — is one or more `source()`
            // hops beneath it. Printing only the top is how a TLS
            // misconfiguration turns into an afternoon.
            eprint!("yesno: {e}");
            let mut src = e.source();
            while let Some(s) = src {
                eprint!("\n  caused by: {s}");
                src = s.source();
            }
            eprintln!();
            ExitCode::FAILURE
        }
    }
}

type Fail = Box<dyn std::error::Error>;

/// Adds the bearer token, when there is one.
///
/// A concrete type rather than a closure so that the client has **one** type
/// whether or not a token was supplied — otherwise every helper below would need
/// to be generic over the interceptor for no gain.
#[derive(Clone)]
struct AuthHeader {
    bearer: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
    expect_term: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
}

impl tonic::service::Interceptor for AuthHeader {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(h) = &self.bearer {
            req.metadata_mut().insert("authorization", h.clone());
        }
        if let Some(t) = &self.expect_term {
            req.metadata_mut()
                .insert(yesno_server::guard::EXPECT_TERM, t.clone());
        }
        Ok(req)
    }
}

type Client =
    FlightServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthHeader>>;

async fn connect(cli: &Cli) -> Result<Client, Fail> {
    let mut ep = Channel::from_shared(cli.endpoint.clone())?;

    if cli.ca.is_some() || cli.cert.is_some() || cli.server_name.is_some() {
        let mut tls = tonic::transport::ClientTlsConfig::new();
        if let Some(ca) = &cli.ca {
            tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(std::fs::read(ca)?));
        }
        if let (Some(c), Some(k)) = (&cli.cert, &cli.key) {
            tls = tls.identity(tonic::transport::Identity::from_pem(
                std::fs::read(c)?,
                std::fs::read(k)?,
            ));
        }
        if let Some(n) = &cli.server_name {
            tls = tls.domain_name(n.clone());
        }
        ep = ep.tls_config(tls)?;
    } else if cli.endpoint.starts_with("https://") {
        // Rather than let it fail inside the handshake with something opaque.
        return Err(
            "an https:// endpoint needs --ca ( or --server-name ) so the server's \
                    certificate can be verified against something"
                .into(),
        );
    }

    let ch = ep.connect().await?;

    // The token travels on every call rather than being exchanged once: the
    // server's `handshake` is a whoami probe, not a session mint. See its note
    // in `guard`.
    let token = match &cli.token_file {
        None => None,
        Some(p) => Some(std::fs::read_to_string(p)?.trim().to_owned()),
    };
    let bearer = match token {
        None => None,
        Some(t) => Some(
            format!("Bearer {t}")
                .parse()
                .map_err(|_| "the token file contains characters a header cannot carry")?,
        ),
    };
    let expect_term = cli
        .min_term
        .map(|n| n.to_string().parse().expect("a number is valid ASCII"));
    Ok(FlightServiceClient::with_interceptor(
        ch,
        AuthHeader {
            bearer,
            expect_term,
        },
    ))
}

/// The descriptor shape the server's `key_of` accepts: an 8-byte LE key in
/// `cmd`. Not the path form — that one is `to_string`ed and reparsed, which
/// is fine for a human typing a path but pointless from a program.
fn descriptor(key: u64) -> FlightDescriptor {
    FlightDescriptor::new_cmd(key.to_le_bytes().to_vec())
}
fn expression_descriptor(expr: &yesno_flight::SetExpr) -> FlightDescriptor {
    FlightDescriptor::new_cmd(expr.encode())
}
/// The same expression bound to one database version.
///
/// A different encoding, not an extra field on the same one: a bare
/// `SetExpr` has no version slot, and `QueryRequest` is the form that carries
/// both. The server distinguishes them by magic, so sending the wrong one asks a
/// different question rather than failing.
fn expression_descriptor_at(expr: &yesno_flight::SetExpr, version: u64) -> FlightDescriptor {
    FlightDescriptor::new_cmd(yesno_flight::QueryRequest::at(expr.clone(), version).encode())
}

#[derive(Debug, PartialEq, Eq)]
struct QueryParseError {
    at: usize,
    message: String,
}

impl std::fmt::Display for QueryParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "expression byte {}: {}", self.at + 1, self.message)
    }
}

impl std::error::Error for QueryParseError {}

struct QueryParser<'a> {
    input: &'a str,
    at: usize,
}

impl QueryParser<'_> {
    fn error(&self, message: impl Into<String>) -> QueryParseError {
        QueryParseError {
            at: self.at,
            message: message.into(),
        }
    }

    fn skip_ws(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.at += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        self.skip_ws();
        if self.input.as_bytes().get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), QueryParseError> {
        if self.take(byte) {
            Ok(())
        } else {
            Err(self.error(format!("expected '{}'", char::from(byte))))
        }
    }

    fn number(&mut self) -> Result<u64, QueryParseError> {
        self.skip_ws();
        let start = self.at;
        while self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(|b| b.is_ascii_digit() || *b == b'_')
        {
            self.at += 1;
        }
        if self.at == start {
            return Err(self.error("expected an unsigned integer"));
        }
        let raw = &self.input[start..self.at];
        raw.replace('_', "").parse().map_err(|_| QueryParseError {
            at: start,
            message: format!("'{raw}' is not a u64"),
        })
    }

    fn ident(&mut self) -> Result<&str, QueryParseError> {
        self.skip_ws();
        let start = self.at;
        while self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(*b, b'_' | b'-'))
        {
            self.at += 1;
        }
        if self.at == start {
            Err(self.error("expected a key or operator"))
        } else {
            Ok(&self.input[start..self.at])
        }
    }

    fn binary(
        &mut self,
        build: impl FnOnce(yesno_flight::SetExpr, yesno_flight::SetExpr) -> yesno_flight::SetExpr,
    ) -> Result<yesno_flight::SetExpr, QueryParseError> {
        self.expect(b'(')?;
        let left = self.expr()?;
        self.expect(b',')?;
        let right = self.expr()?;
        self.expect(b')')?;
        Ok(build(left, right))
    }

    fn junction(&mut self, and: bool) -> Result<yesno_flight::SetExpr, QueryParseError> {
        self.expect(b'(')?;
        let mut children = vec![self.expr()?];
        while self.take(b',') {
            children.push(self.expr()?);
        }
        self.expect(b')')?;
        if children.len() < 2 {
            return Err(self.error("AND and OR need at least two operands"));
        }
        Ok(if and {
            yesno_flight::SetExpr::And(children)
        } else {
            yesno_flight::SetExpr::Or(children)
        })
    }

    fn literal(&mut self) -> Result<yesno_flight::SetExpr, QueryParseError> {
        self.expect(b'{')?;
        let mut ordinals = Vec::new();
        if self.take(b'}') {
            return Ok(yesno_flight::SetExpr::Literal(ordinals));
        }
        loop {
            self.skip_ws();
            let at = self.at;
            let ordinal = self.number()?;
            if ordinal == u64::MAX {
                return Err(QueryParseError {
                    at,
                    message: format!("{ordinal} is outside the ordinal universe"),
                });
            }
            ordinals.push(ordinal);
            if self.take(b'}') {
                break;
            }
            self.expect(b',')?;
        }
        yesno_flight::SetExpr::literal(ordinals).map_err(|error| self.error(error.to_string()))
    }

    fn view(&mut self) -> Result<yesno_flight::ViewSpec, QueryParseError> {
        let ident = self.ident()?.to_owned();
        self.expect(b'(')?;
        let at = self.at;
        let sets = u32::try_from(self.number()?).map_err(|_| QueryParseError {
            at,
            message: "view constituent count does not fit in u32".into(),
        })?;
        let view = if ident.eq_ignore_ascii_case("interleaved") {
            self.expect(b')')?;
            yesno_flight::ViewSpec::interleaved(sets)
        } else if ident.eq_ignore_ascii_case("blocked") {
            self.expect(b',')?;
            let stride = self.number()?;
            self.expect(b')')?;
            yesno_flight::ViewSpec::blocked(sets, stride)
        } else {
            return Err(self.error(format!(
                "unknown view layout '{ident}'; expected interleaved or blocked"
            )));
        };
        view.check().map_err(|e| self.error(e.to_string()))?;
        Ok(view)
    }

    /// The fold operator, named for the Boolean operation being folded.
    ///
    /// `or` / `and` / `xor` rather than the old `any` / `all` / `parity`: the
    /// operator's own name makes the pairwise implementation evident, and the
    /// set is closed at three for the reason `FoldOp` documents.
    fn fold_op(&mut self) -> Result<yesno_flight::FoldOp, QueryParseError> {
        let ident = self.ident()?.to_owned();
        if ident.eq_ignore_ascii_case("or") {
            Ok(yesno_flight::FoldOp::Or)
        } else if ident.eq_ignore_ascii_case("and") {
            Ok(yesno_flight::FoldOp::And)
        } else if ident.eq_ignore_ascii_case("xor") {
            Ok(yesno_flight::FoldOp::Xor)
        } else {
            Err(self.error(format!(
                "unknown fold operator '{ident}'; expected or, and, or xor"
            )))
        }
    }

    /// A whole query: a set, or one integer per constituent.
    fn query(&mut self) -> Result<yesno_flight::AnyExpr, QueryParseError> {
        self.skip_ws();
        let save = self.at;
        // Only `map` with an integer body denotes the vector sort, and that is
        // decidable from the text, so try it before falling back to a set.
        if let Ok(id) = self.ident() {
            if id.eq_ignore_ascii_case("map") {
                let probe = self.at;
                if self.expect(b'(').is_ok() {
                    if let Ok(v) = self.vec_expr() {
                        if self.expect(b',').is_ok() && self.peek_body_sort()? == BodySort::Int {
                            let body = self.int_expr()?;
                            self.expect(b')')?;
                            return Ok(yesno_flight::AnyExpr::VecInt(
                                yesno_flight::VecIntExpr::Map(Box::new(v), Box::new(body)),
                            ));
                        }
                    }
                }
                self.at = probe;
            }
        }
        self.at = save;
        self.expr().map(yesno_flight::AnyExpr::Set)
    }

    fn expr(&mut self) -> Result<yesno_flight::SetExpr, QueryParseError> {
        self.skip_ws();
        if self.input.as_bytes().get(self.at) == Some(&b'{') {
            return self.literal();
        }
        // A bracketed vector in a set position must be indexed: `[ a, b ][ 1 ]`.
        if self.input.as_bytes().get(self.at) == Some(&b'[') {
            let v = self.vec_expr()?;
            return self.index_into(v);
        }
        // `_` is the element of the enclosing `map` body. The server refuses
        // one that is not in a body, so the parser need not track scope.
        if self.input.as_bytes().get(self.at) == Some(&b'_') {
            self.at += 1;
            return Ok(yesno_flight::SetExpr::Hole);
        }
        if self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_digit)
        {
            return self.number().map(yesno_flight::SetExpr::Key);
        }

        let ident = self.ident()?.to_owned();
        if ident.eq_ignore_ascii_case("empty") {
            return Ok(yesno_flight::SetExpr::Empty);
        }
        if ident.eq_ignore_ascii_case("key") {
            self.expect(b'(')?;
            let key = self.number()?;
            self.expect(b')')?;
            return Ok(yesno_flight::SetExpr::Key(key));
        }
        if ident.eq_ignore_ascii_case("range") {
            self.expect(b'(')?;
            let lo = self.number()?;
            self.expect(b',')?;
            let hi = self.number()?;
            self.expect(b')')?;
            return Ok(yesno_flight::SetExpr::Range(lo, hi));
        }
        if ident.eq_ignore_ascii_case("and") {
            return self.junction(true);
        }
        if ident.eq_ignore_ascii_case("or") {
            return self.junction(false);
        }
        if ident.eq_ignore_ascii_case("and-not") {
            return self.binary(|a, b| yesno_flight::SetExpr::AndNot(Box::new(a), Box::new(b)));
        }
        if ident.eq_ignore_ascii_case("xor") {
            return self.binary(yesno_flight::SetExpr::xor);
        }
        if ident.eq_ignore_ascii_case("not") {
            self.expect(b'(')?;
            let child = self.expr()?;
            self.expect(b')')?;
            return Ok(yesno_flight::SetExpr::complement(child));
        }
        if ident.eq_ignore_ascii_case("fold") {
            self.expect(b'(')?;
            let v = self.vec_expr()?;
            self.expect(b',')?;
            let op = self.fold_op()?;
            self.expect(b')')?;
            return Ok(yesno_flight::SetExpr::Fold(Box::new(v), op));
        }
        if ident.eq_ignore_ascii_case("pack") {
            self.expect(b'(')?;
            let v = self.vec_expr()?;
            self.expect(b',')?;
            let view = self.view()?;
            self.expect(b')')?;
            if v.arity() != view.sets {
                return Err(self.error(format!(
                    "packing {} sets under a {}-set descriptor",
                    v.arity(),
                    view.sets
                )));
            }
            return Ok(yesno_flight::SetExpr::Pack(Box::new(v), view));
        }
        if ident.eq_ignore_ascii_case("expand") {
            self.expect(b'(')?;
            let input = Box::new(self.expr()?);
            self.expect(b',')?;
            let view = self.view()?;
            self.expect(b')')?;
            return Ok(yesno_flight::SetExpr::Expand(input, view));
        }
        if ident.eq_ignore_ascii_case("view") {
            let v = self.vec_after_view()?;
            return self.index_into(v);
        }
        if ident.eq_ignore_ascii_case("select") {
            self.expect(b'(')?;
            let a = self.expr()?;
            self.expect(b',')?;
            let n = self.number()?;
            self.expect(b')')?;
            return Ok(yesno_flight::SetExpr::Select(Box::new(a), n));
        }
        if ident.eq_ignore_ascii_case("map") {
            // The body's sort decides the result's, and one token of lookahead
            // settles it: `cardinality` / `rank` are integer-valued, `contains`
            // is boolean, anything else is a set.
            self.expect(b'(')?;
            let v = self.vec_expr()?;
            self.expect(b',')?;
            return match self.peek_body_sort()? {
                BodySort::Bool => {
                    let body = self.bool_expr()?;
                    self.expect(b')')?;
                    Ok(yesno_flight::SetExpr::MapBool(Box::new(v), Box::new(body)))
                }
                BodySort::Int => Err(self.error(
                    "map with an integer body denotes a vector of integers, \
                     which is not a set; it can only be a whole query",
                )),
                BodySort::Set => {
                    let body = self.expr()?;
                    self.expect(b')')?;
                    // A vector in a set position must be indexed or folded.
                    let v = yesno_flight::VecSetExpr::Map(Box::new(v), Box::new(body));
                    self.index_into(v)
                }
            };
        }
        Err(self.error(format!("unknown operator '{ident}'")))
    }

    /// One token of lookahead, without consuming it.
    fn peek_body_sort(&mut self) -> Result<BodySort, QueryParseError> {
        self.skip_ws();
        let save = self.at;
        let sort = match self.ident() {
            Ok(id) if id.eq_ignore_ascii_case("cardinality") || id.eq_ignore_ascii_case("rank") => {
                BodySort::Int
            }
            Ok(id) if id.eq_ignore_ascii_case("contains") => BodySort::Bool,
            _ => BodySort::Set,
        };
        self.at = save;
        Ok(sort)
    }

    fn int_expr(&mut self) -> Result<yesno_flight::IntExpr, QueryParseError> {
        self.skip_ws();
        if self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_digit)
        {
            return self.number().map(yesno_flight::IntExpr::Lit);
        }
        let ident = self.ident()?.to_owned();
        if ident.eq_ignore_ascii_case("cardinality") {
            self.expect(b'(')?;
            let a = self.expr()?;
            self.expect(b')')?;
            return Ok(yesno_flight::IntExpr::Cardinality(Box::new(a)));
        }
        if ident.eq_ignore_ascii_case("rank") {
            self.expect(b'(')?;
            let a = self.expr()?;
            self.expect(b',')?;
            let x = self.number()?;
            self.expect(b')')?;
            return Ok(yesno_flight::IntExpr::Rank(Box::new(a), x));
        }
        Err(self.error(format!("'{ident}' is not an integer")))
    }

    fn bool_expr(&mut self) -> Result<yesno_flight::BoolExpr, QueryParseError> {
        let ident = self.ident()?.to_owned();
        if ident.eq_ignore_ascii_case("contains") {
            self.expect(b'(')?;
            let a = self.expr()?;
            self.expect(b',')?;
            let x = self.number()?;
            self.expect(b')')?;
            return Ok(yesno_flight::BoolExpr::Contains(Box::new(a), x));
        }
        Err(self.error(format!("'{ident}' is not a boolean")))
    }

    /// A vector-sorted expression: `[ a, b, .. ]` or `view( expr, spec )`.
    fn vec_expr(&mut self) -> Result<yesno_flight::VecSetExpr, QueryParseError> {
        self.skip_ws();
        if self.input.as_bytes().get(self.at) == Some(&b'[') {
            self.at += 1;
            let mut xs = Vec::new();
            loop {
                xs.push(self.expr()?);
                self.skip_ws();
                match self.input.as_bytes().get(self.at) {
                    Some(&b',') => self.at += 1,
                    Some(&b']') => {
                        self.at += 1;
                        break;
                    }
                    _ => return Err(self.error("expected ',' or ']' in a vector")),
                }
            }
            return Ok(yesno_flight::VecSetExpr::List(xs));
        }
        let ident = self.ident()?.to_owned();
        if ident.eq_ignore_ascii_case("view") {
            return self.vec_after_view();
        }
        if ident.eq_ignore_ascii_case("map") {
            self.expect(b'(')?;
            let v = self.vec_expr()?;
            self.expect(b',')?;
            let body = self.expr()?;
            self.expect(b')')?;
            return Ok(yesno_flight::VecSetExpr::Map(Box::new(v), Box::new(body)));
        }
        Err(self.error(format!("'{ident}' is not a vector")))
    }

    fn vec_after_view(&mut self) -> Result<yesno_flight::VecSetExpr, QueryParseError> {
        self.expect(b'(')?;
        let input = Box::new(self.expr()?);
        self.expect(b',')?;
        let view = self.view()?;
        self.expect(b')')?;
        Ok(yesno_flight::VecSetExpr::View(input, view))
    }

    /// `v[ i ]` -- the only way a vector becomes a set outside `fold` and
    /// `pack`. Zero-based, and the index is checked against the arity here
    /// because the arity is statically known.
    fn index_into(
        &mut self,
        v: yesno_flight::VecSetExpr,
    ) -> Result<yesno_flight::SetExpr, QueryParseError> {
        self.skip_ws();
        self.expect(b'[')?;
        let at = self.at;
        let i = u32::try_from(self.number()?).map_err(|_| QueryParseError {
            at,
            message: "index does not fit in u32".into(),
        })?;
        self.expect(b']')?;
        if i >= v.arity() {
            return Err(self.error("index is at or above the vector's arity"));
        }
        Ok(yesno_flight::SetExpr::At(Box::new(v), i))
    }
}

/// Which sort a `map` body denotes, decided by one token of lookahead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodySort {
    Set,
    Int,
    Bool,
}

fn parse_expression(input: &str) -> Result<yesno_flight::SetExpr, QueryParseError> {
    match parse_query(input)? {
        yesno_flight::AnyExpr::Set(e) => Ok(e),
        yesno_flight::AnyExpr::VecInt(_) => Err(QueryParseError {
            at: 0,
            message: "this query denotes one integer per constituent, not a set".into(),
        }),
    }
}

/// Parse a whole query at whichever sort it denotes.
///
/// Most queries are sets. `map( v, cardinality( .. ) )` is one integer per
/// constituent -- a facet histogram -- and that is the other shape a server can
/// return.
fn parse_query(input: &str) -> Result<yesno_flight::AnyExpr, QueryParseError> {
    let mut parser = QueryParser { input, at: 0 };
    let expr = parser.query()?;
    parser.skip_ws();
    if parser.at != input.len() {
        return Err(parser.error("unexpected trailing input"));
    }
    Ok(expr)
}

async fn print_rows(
    c: &mut Client,
    descriptor: FlightDescriptor,
    subject: &str,
    limit: Option<usize>,
) -> Result<(), Fail> {
    let info = c.get_flight_info(descriptor).await?.into_inner();
    let ep = info
        .endpoint
        .first()
        .ok_or("the server returned no endpoint")?;
    let ticket = ep.ticket.clone().ok_or("the endpoint carries no ticket")?;
    if let Some(t) = yesno_flight::Ticket::decode(&ticket.ticket) {
        eprintln!(
            "# {subject}: {} ordinals at version {}",
            info.total_records, t.version
        );
    }

    let mut stream = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
        c.do_get(ticket)
            .await?
            .into_inner()
            .map(|r| r.map_err(|e| arrow_flight::error::FlightError::Tonic(Box::new(e)))),
    );
    let mut shown = 0usize;
    while let Some(b) = stream.next().await {
        let b = b?;
        let col = b
            .column_by_name("ordinal")
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .ok_or("the server sent no ordinal column")?;
        for i in 0..col.len() {
            if limit.is_some_and(|n| shown >= n) {
                return Ok(());
            }
            println!("{}", col.value(i));
            shown += 1;
        }
    }
    Ok(())
}

fn read_input(file: &str) -> Result<String, Fail> {
    if file == "-" {
        Ok(std::io::read_to_string(std::io::stdin())?)
    } else {
        Ok(std::fs::read_to_string(file)?)
    }
}

async fn put_pairs(c: &mut Client, keys: Vec<u64>, ords: Vec<u64>) -> Result<(), Fail> {
    if keys.is_empty() {
        return Err("nothing to ingest".into());
    }
    debug_assert_eq!(keys.len(), ords.len());
    let n = keys.len();
    let schema = yesno_flight::pairs_schema();
    let batch = arrow_array::RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(UInt64Array::new(keys.into(), None)),
            std::sync::Arc::new(UInt64Array::new(ords.into(), None)),
        ],
    )?;
    // Named explicitly: `do_put` no longer defaults to insert, so that a
    // command it does not recognise cannot be applied as one.
    let input = arrow_flight::encode::FlightDataEncoderBuilder::new()
        .with_schema(schema)
        .with_flight_descriptor(Some(arrow_flight::FlightDescriptor::new_cmd(
            yesno_flight::PUT_INSERT.to_vec(),
        )))
        .build(futures::stream::iter(vec![Ok(batch)]))
        .map(|r| r.expect("locally built batches encode"));
    let mut acked = c.do_put(input).await?.into_inner();
    let mut rows = 0u64;
    let mut version = None;
    while let Some(r) = acked.next().await {
        let r = r?;
        // Rows first, then the commit version since 2026-09-12. The previous
        // `if len == 8` with no `else` printed "ingested 0 of N pairs" against a
        // server that widened the field — wrong, and pointing away from the
        // cause.
        let md = r.app_metadata.as_ref();
        match md.len() {
            8 => rows = u64::from_le_bytes(md.try_into().unwrap()),
            16 => {
                rows = u64::from_le_bytes(md[..8].try_into().unwrap());
                let v = u64::from_le_bytes(md[8..].try_into().unwrap());
                version = (v != 0).then_some(v);
            }
            other => {
                return Err(Fail::from(format!(
                    "ingest acknowledgement has {other} bytes, expected 8 or 16"
                )))
            }
        }
    }
    match version {
        Some(v) => println!("ingested {rows} of {n} pairs at version {v}"),
        None => println!("ingested {rows} of {n} pairs"),
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<(), Fail> {
    let mut c = connect(&cli).await?;
    match cli.cmd {
        Cmd::Count { key } => {
            let info = c.get_flight_info(descriptor(key)).await?.into_inner();
            println!("{}", info.total_records);
        }

        Cmd::Get { key, limit } => {
            print_rows(&mut c, descriptor(key), &format!("key {key}"), limit).await?;
        }

        Cmd::Query {
            expression,
            limit,
            count_only,
            at_version,
        } => {
            let expr = parse_expression(&expression)?;
            let descriptor = match at_version {
                Some(version) => expression_descriptor_at(&expr, version),
                None => expression_descriptor(&expr),
            };
            if count_only {
                let info = c.get_flight_info(descriptor).await?.into_inner();
                println!("{}", info.total_records);
            } else {
                print_rows(&mut c, descriptor, &format!("query {expression}"), limit).await?;
            }
        }

        Cmd::Put { file } => {
            let text = read_input(&file)?;
            // Two `u64`s a line. No dependency, and a format anyone can produce
            // with `awk`.
            let mut keys: Vec<u64> = Vec::new();
            let mut ords: Vec<u64> = Vec::new();
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (k, o) = line
                    .split_once(',')
                    .ok_or_else(|| format!("line {}: expected `key,ordinal`", n + 1))?;
                keys.push(
                    k.trim()
                        .parse::<u64>()
                        .map_err(|e| format!("line {}: {e}", n + 1))?,
                );
                ords.push(
                    o.trim()
                        .parse::<u64>()
                        .map_err(|e| format!("line {}: {e}", n + 1))?,
                );
            }
            put_pairs(&mut c, keys, ords).await?;
        }

        Cmd::ViewPut {
            key,
            sets,
            stride,
            file,
        } => {
            let view = stride.map_or_else(
                || yesno_flight::ViewSpec::interleaved(sets),
                |stride| yesno_flight::ViewSpec::blocked(sets, stride),
            );
            view.check().map_err(|e| format!("invalid view: {e}"))?;
            let text = read_input(&file)?;
            let mut keys = Vec::new();
            let mut ords = Vec::new();
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (set, logical) = line
                    .split_once(',')
                    .ok_or_else(|| format!("line {}: expected `constituent,ordinal`", n + 1))?;
                let set = set
                    .trim()
                    .parse::<u32>()
                    .map_err(|e| format!("line {}: {e}", n + 1))?;
                let logical = logical
                    .trim()
                    .parse::<u64>()
                    .map_err(|e| format!("line {}: {e}", n + 1))?;
                let ordinal = view.ordinal_of(set, logical).ok_or_else(|| {
                    format!(
                        "line {}: constituent {set}, ordinal {logical} is not addressable by this view",
                        n + 1
                    )
                })?;
                keys.push(key);
                ords.push(ordinal);
            }
            put_pairs(&mut c, keys, ords).await?;
        }

        Cmd::Stats => println!("{}", format_stats(&stats(&mut c).await?)),
        Cmd::Status => {
            // `handshake` is the whoami probe: reaching it at all means the
            // server accepted this client's credential, and what comes back is
            // who it thinks that is. Not a session — the token travels on
            // every call. See `guard::handshake`.
            match c
                .handshake(futures::stream::iter(
                    Vec::<arrow_flight::HandshakeRequest>::new(),
                ))
                .await
            {
                Ok(resp) => {
                    let mut s = resp.into_inner();
                    if let Some(Ok(first)) = s.next().await {
                        println!("identity : {}", String::from_utf8_lossy(&first.payload));
                    }
                }
                Err(e) => println!("identity : unavailable ({})", e.code()),
            }
            // The term comes back on **every** response, so any call teaches it.
            // This is what an operator pins with `--min-term` after a
            // failover, and the reason it is printed rather than merely checked.
            let resp = c.list_actions(Empty {}).await?;
            match resp.metadata().get(yesno_server::guard::TERM) {
                Some(t) => println!("term     : {}", t.to_str().unwrap_or("?")),
                None => println!("term     : not reported ( an older server )"),
            }
            let acts: Vec<_> = resp.into_inner().collect().await;
            println!("endpoint : {}", cli.endpoint);
            println!("actions  :");
            for a in acts.into_iter().flatten() {
                println!("  {:<10} {}", a.r#type, a.description);
            }
            println!("stats    : {}", format_stats(&stats(&mut c).await?));
        }
    }
    Ok(())
}

async fn action_bytes(c: &mut Client, name: &str) -> Result<Vec<u8>, Fail> {
    let mut s = c
        .do_action(Action {
            r#type: name.into(),
            body: Default::default(),
        })
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(r) = s.next().await {
        out.extend_from_slice(&r?.body);
    }
    Ok(out)
}

async fn stats(c: &mut Client) -> Result<yesno_flight::ServerStats, Fail> {
    Ok(yesno_flight::ServerStats::decode_protobuf(
        action_bytes(c, "stats").await?.as_slice(),
    )?)
}

fn format_stats(stats: &yesno_flight::ServerStats) -> String {
    format!(
        "allocated_bytes={} deferred_bytes={} wal_bytes={} live_readers={} shards={}",
        stats.allocated_bytes,
        stats.deferred_bytes,
        stats.wal_bytes,
        stats.live_readers,
        stats.shards
    )
}

#[cfg(test)]
mod query_parser_tests {
    use super::*;
    use yesno_flight::SetExpr;

    #[test]
    fn parses_every_documented_operator() {
        assert_eq!(parse_expression("42").unwrap(), SetExpr::Key(42));
        assert_eq!(parse_expression("key(42)").unwrap(), SetExpr::Key(42));
        assert_eq!(
            parse_expression("range(1_000, 2_000)").unwrap(),
            SetExpr::Range(1000, 2000)
        );
        assert_eq!(
            parse_expression("and(1, or(2, 3), and-not(4, 5))").unwrap(),
            SetExpr::And(vec![
                SetExpr::Key(1),
                SetExpr::Or(vec![SetExpr::Key(2), SetExpr::Key(3)]),
                SetExpr::AndNot(Box::new(SetExpr::Key(4)), Box::new(SetExpr::Key(5))),
            ])
        );
        assert_eq!(
            parse_expression("xor(1,2)").unwrap(),
            SetExpr::xor(SetExpr::Key(1), SetExpr::Key(2))
        );
        assert_eq!(
            parse_expression("not(1)").unwrap(),
            SetExpr::complement(SetExpr::Key(1))
        );
        assert_eq!(
            parse_expression("view(9, interleaved(3))[2]").unwrap(),
            SetExpr::At(
                Box::new(yesno_flight::VecSetExpr::View(
                    Box::new(SetExpr::Key(9)),
                    yesno_flight::ViewSpec::interleaved(3),
                )),
                2,
            )
        );
        assert_eq!(
            parse_expression("fold(view(10, blocked(4, 65_536)), xor)").unwrap(),
            SetExpr::Fold(
                Box::new(yesno_flight::VecSetExpr::View(
                    Box::new(SetExpr::Key(10)),
                    yesno_flight::ViewSpec::blocked(4, 65_536),
                )),
                yesno_flight::FoldOp::Xor,
            )
        );
        assert_eq!(
            parse_expression("expand(and(1, 2), interleaved(3))").unwrap(),
            SetExpr::Expand(
                Box::new(SetExpr::And(vec![SetExpr::Key(1), SetExpr::Key(2)])),
                yesno_flight::ViewSpec::interleaved(3),
            )
        );
        // The composition the old leaf nodes could not express, and the two
        // bracket forms: a literal vector, and indexing one.
        assert_eq!(
            parse_expression("fold(view(and(1, 2), interleaved(3)), or)").unwrap(),
            SetExpr::Fold(
                Box::new(yesno_flight::VecSetExpr::View(
                    Box::new(SetExpr::And(vec![SetExpr::Key(1), SetExpr::Key(2)])),
                    yesno_flight::ViewSpec::interleaved(3),
                )),
                yesno_flight::FoldOp::Or,
            )
        );
        assert_eq!(
            parse_expression("[1, 2, 3][1]").unwrap(),
            SetExpr::At(
                Box::new(yesno_flight::VecSetExpr::List(vec![
                    SetExpr::Key(1),
                    SetExpr::Key(2),
                    SetExpr::Key(3),
                ])),
                1,
            )
        );
        assert_eq!(
            parse_expression("pack([1, 2], interleaved(2))").unwrap(),
            SetExpr::Pack(
                Box::new(yesno_flight::VecSetExpr::List(vec![
                    SetExpr::Key(1),
                    SetExpr::Key(2),
                ])),
                yesno_flight::ViewSpec::interleaved(2),
            )
        );
        assert_eq!(parse_expression("empty").unwrap(), SetExpr::Empty);
    }

    /// The parser refuses what the decoder would: an index past the arity and
    /// a `pack` whose vector does not match the descriptor. Both are decidable
    /// from the text alone, so neither should reach the server.
    #[test]
    fn the_parser_checks_arity_and_index_statically() {
        assert!(parse_expression("[1, 2][2]").is_err(), "index == arity");
        assert!(
            parse_expression("view(9, interleaved(3))[3]").is_err(),
            "index == the descriptor's sets"
        );
        assert!(
            parse_expression("pack([1, 2], interleaved(3))").is_err(),
            "arity 2 under a 3-set descriptor"
        );
        assert!(parse_expression("pack([1, 2], interleaved(2))").is_ok());
    }

    #[test]
    fn parses_ordinal_set_literals() {
        assert_eq!(parse_expression("{}").unwrap(), SetExpr::Literal(vec![]));
        assert_eq!(
            parse_expression("{ 9, 1, 9, 65_536, 18_446_744_073_709_551_614 }").unwrap(),
            SetExpr::Literal(vec![1, 9, 65_536, u64::MAX - 1])
        );
        assert_eq!(
            parse_expression("and(42, {1, 5, 9})").unwrap(),
            SetExpr::And(vec![SetExpr::Key(42), SetExpr::Literal(vec![1, 5, 9])])
        );
    }

    #[test]
    fn parses_view_put_layout_options() {
        let interleaved =
            Cli::try_parse_from(["yesno", "view-put", "9", "--sets", "3", "rows.csv"]).unwrap();
        assert!(matches!(
            interleaved.cmd,
            Cmd::ViewPut {
                key: 9,
                sets: 3,
                stride: None,
                file,
            } if file == "rows.csv"
        ));

        let blocked = Cli::try_parse_from([
            "yesno", "view-put", "10", "--sets", "4", "--stride", "65536",
        ])
        .unwrap();
        assert!(matches!(
            blocked.cmd,
            Cmd::ViewPut {
                key: 10,
                sets: 4,
                stride: Some(65_536),
                file,
            } if file == "-"
        ));
    }

    #[test]
    fn errors_name_the_location_and_problem() {
        for bad in [
            "",
            "and(1)",
            "or(1,)",
            "range(1)",
            "wat(1)",
            "1 trailing",
            "18446744073709551616",
            "{1 2}",
            "{1,}",
            "{18446744073709551615}",
            "view(9, interleaved(0))[0]",
            "view(9, interleaved(2))[2]",
            "fold(view(9, blocked(2, 0)), maybe)",
            // A vector where a set belongs, and a set where a vector belongs.
            "view(9, interleaved(2))",
            "fold(9, or)",
            "[]",
            "[1, 2][2]",
            "pack([1, 2], interleaved(3))",
        ] {
            let err = parse_expression(bad).expect_err(bad);
            assert!(!err.message.is_empty(), "{bad}");
            assert!(err.at <= bad.len(), "{bad}: {err:?}");
        }
    }

    /// The facet query, in the surface syntax. It denotes one integer per
    /// constituent, so it is **not** a set and `parse_expression` says so
    /// rather than mis-sorting it.
    #[test]
    fn the_facet_query_parses_at_the_vector_sort() {
        let text = "map(view(9, interleaved(3)), cardinality(and(_, 7)))";
        let want = yesno_flight::AnyExpr::VecInt(yesno_flight::VecIntExpr::Map(
            Box::new(yesno_flight::VecSetExpr::View(
                Box::new(SetExpr::Key(9)),
                yesno_flight::ViewSpec::interleaved(3),
            )),
            Box::new(yesno_flight::IntExpr::Cardinality(Box::new(SetExpr::And(
                vec![SetExpr::Hole, SetExpr::Key(7)],
            )))),
        ));
        assert_eq!(parse_query(text).unwrap(), want);
        assert!(
            parse_expression(text).is_err(),
            "a vector of integers is not a set"
        );

        // A set-valued body stays a vector of sets, so it must be indexed or
        // folded to become a query.
        assert_eq!(
            parse_query("fold(map(view(9, interleaved(3)), and(_, 7)), or)").unwrap(),
            yesno_flight::AnyExpr::Set(SetExpr::Fold(
                Box::new(yesno_flight::VecSetExpr::Map(
                    Box::new(yesno_flight::VecSetExpr::View(
                        Box::new(SetExpr::Key(9)),
                        yesno_flight::ViewSpec::interleaved(3),
                    )),
                    Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(7)])),
                )),
                yesno_flight::FoldOp::Or,
            ))
        );

        // A boolean body asks which constituents satisfy it, which is a set of
        // constituent indices.
        assert_eq!(
            parse_query("map(view(9, interleaved(3)), contains(_, 4))").unwrap(),
            yesno_flight::AnyExpr::Set(SetExpr::MapBool(
                Box::new(yesno_flight::VecSetExpr::View(
                    Box::new(SetExpr::Key(9)),
                    yesno_flight::ViewSpec::interleaved(3),
                )),
                Box::new(yesno_flight::BoolExpr::Contains(Box::new(SetExpr::Hole), 4)),
            ))
        );

        assert_eq!(
            parse_query("select(7, 0)").unwrap(),
            yesno_flight::AnyExpr::Set(SetExpr::Select(Box::new(SetExpr::Key(7)), 0))
        );
    }

    /// `--at-version` must produce a **`QueryRequest`** descriptor, not a bare
    /// `SetExpr` with a version bolted on — a bare expression has no version slot
    /// at all. The server distinguishes the two by magic, so the wrong encoding
    /// asks a different question ( "read at the newest visible version" ) and
    /// answers it successfully, which is the failure a type cannot catch and a
    /// round-trip can.
    #[test]
    fn a_versioned_query_descriptor_round_trips_its_version() {
        use yesno_flight::{QueryRequest, SetExpr};

        let expr = SetExpr::Key(42);
        let bound = expression_descriptor_at(&expr, 7);
        assert!(
            QueryRequest::looks_like_request(&bound.cmd),
            "a versioned descriptor must be recognised as a query request"
        );
        let decoded = QueryRequest::decode(&bound.cmd).expect("it must decode");
        assert_eq!(decoded.version, Some(7));
        assert_eq!(decoded.expression, expr);

        // The negative half: the unversioned form must NOT be a query request,
        // or the two would be indistinguishable and this test would pass for a
        // helper that ignored its argument.
        let plain = expression_descriptor(&expr);
        assert!(
            !QueryRequest::looks_like_request(&plain.cmd),
            "an unversioned descriptor must stay a bare expression"
        );
        assert_ne!(plain.cmd, bound.cmd);
    }
}
