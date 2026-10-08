//! `SERVER`, `FOREIGN TABLE` and `USER MAPPING` options.
//!
//! # Why the parsing is pure
//!
//! Everything here takes `&[(name, value)]` rather than a `List *` of
//! `DefElem`. PostgreSQL's option lists are trivial to convert and impossible to
//! construct in a unit test, so the conversion lives at the callback boundary
//! and every rule about what a valid option set *is* stays testable without a
//! backend. That matters more than it looks: the validator is the only thing
//! standing between a typo and a silent default, and a rule nobody can test is a
//! rule nobody checks.
//!
//! # Validation happens at `CREATE`, not at query time
//!
//! PostgreSQL calls the FDW's validator when a `SERVER`, `FOREIGN TABLE` or
//! `USER MAPPING` is created or altered. Rejecting an unknown option there is
//! what turns `OPTIONS ( endpiont '...' )` into an error the user sees
//! immediately, rather than a default that quietly takes effect on the first
//! query. Do not relax [`ServerOptions::parse`] into ignoring unrecognised
//! keys.

use std::fmt;

/// Rows requested per `RecordBatch`. Matches `yesno-flight`'s `BATCH_ROWS`, and
/// deliberately so: a different value here would re-chunk every batch on
/// arrival for no gain.
pub const DEFAULT_BATCH_ROWS: usize = 8192;

pub const DEFAULT_TERM_COLUMN: &str = "term";
pub const DEFAULT_KEY_COLUMN: &str = "key";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionError {
    Unknown {
        catalog: &'static str,
        name: String,
    },
    Missing(&'static str),
    Conflict(&'static str),
    BadValue {
        name: &'static str,
        value: String,
        why: &'static str,
    },
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OptionError::Unknown { catalog, name } => {
                write!(f, "unrecognised option \"{name}\" for {catalog}")
            }
            OptionError::Missing(what) => write!(f, "{what}"),
            OptionError::Conflict(what) => write!(f, "{what}"),
            OptionError::BadValue { name, value, why } => {
                write!(f, "invalid value \"{value}\" for option \"{name}\": {why}")
            }
        }
    }
}

/// How the extension reaches yesno.
///
/// The two are mutually exclusive because they are genuinely different
/// deployments, not two spellings of one. `Flight` talks to a running `yesnod`
/// over gRPC; `Local` would open the database directory in-process, which
/// **does not work yet** — `Db::open` takes an exclusive `flock` and PostgreSQL
/// forks one backend per connection, so N backends means N-1 failures. See
/// `multiprocess-read-only-reader` in `TODO.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Flight {
        endpoint: String,
    },
    /// A `yesnod` plugin channel on a Unix socket.
    ///
    /// **This is the answer to `Local`'s problem rather than another spelling
    /// of it.** The reason an in-process transport cannot work is that one
    /// process must hold the directory lock while PostgreSQL forks a backend
    /// per connection; a socket inverts that -- one owner, N connections --
    /// without needing a multi-process reader in the engine.
    ///
    /// It used to cost filter pushdown, and no longer does. The channel grew
    /// `SnapshotEvalCardinality` and `SnapshotEvalLoad` on 2026-10-09, answered
    /// by the same evaluator the Flight surface uses, so a lowered qual reaches
    /// a channel server and is evaluated there. One evaluator for both
    /// transports is what makes the choice between them a deployment decision
    /// rather than a correctness one.
    Channel {
        socket: String,
    },
    Local {
        data_dir: String,
    },
}

impl Transport {
    /// A stable per-server identity, for keying this backend's pending writes.
    ///
    /// The buffer that makes `INSERT` visible to a later `SELECT` in the same
    /// transaction is keyed by `( server, key )`, and "server" was the Flight
    /// endpoint string because Flight was the only transport. A channel server
    /// has no endpoint, so without this the buffer would be keyed by nothing
    /// and **uncommitted writes would be invisible to a scan** -- a wrong
    /// answer, not a missing feature.
    ///
    /// The write path and the scan overlay must agree on it, which is why it
    /// lives here rather than being derived at each of the two call sites.
    ///
    /// # It names its transport, and that is not decoration
    ///
    /// The pre-commit flush reopens a connection with **nothing but this
    /// string**: the buffer outlives the `ModifyTable` node and the server
    /// options are long out of scope by then. An untagged identity therefore
    /// has to be guessed at -- a `grpc://` prefix means Flight, a leading
    /// slash means a socket -- and a guess at commit time is a wrong answer
    /// waiting for the first endpoint that does not look like one. Tagging it
    /// here makes [`Transport::from_buffer_key`] a decode rather than a sniff.
    pub fn buffer_key(&self) -> Option<String> {
        match self {
            Transport::Flight { endpoint } => Some(format!("{TAG_FLIGHT}{endpoint}")),
            Transport::Channel { socket } => Some(format!("{TAG_CHANNEL}{socket}")),
            // Not reachable: this transport refuses to connect at all.
            Transport::Local { .. } => None,
        }
    }

    /// The inverse of [`Transport::buffer_key`].
    ///
    /// `None` for a string this did not produce. The caller is the flush, and
    /// an identity it cannot decode is a bug rather than a configuration
    /// error, so it reports instead of falling back to a transport of its
    /// choosing -- committing a transaction to the wrong server is worse than
    /// refusing to commit it.
    pub fn from_buffer_key(key: &str) -> Option<Transport> {
        if let Some(endpoint) = key.strip_prefix(TAG_FLIGHT) {
            Some(Transport::Flight {
                endpoint: endpoint.to_owned(),
            })
        } else {
            key.strip_prefix(TAG_CHANNEL)
                .map(|socket| Transport::Channel {
                    socket: socket.to_owned(),
                })
        }
    }
}

/// Buffer-key tags. A colon cannot start a `grpc://` endpoint or an absolute
/// path, so neither tag can be confused with the identity that follows it.
const TAG_FLIGHT: &str = "flight:";
const TAG_CHANNEL: &str = "channel:";

/// Where a term's `u64` key comes from.
///
/// yesno has no key catalogue — its key space is the whole `u64` domain and
/// nothing enumerates the populated subset — so the mapping from a human name
/// to a key lives in an ordinary PostgreSQL table that the user owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dictionary {
    /// Qualified relation name, e.g. `public.yesno_terms`.
    pub relation: String,
    pub term_column: String,
    pub key_column: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOptions {
    pub transport: Transport,
    pub dictionary: Option<Dictionary>,
    pub batch_rows: usize,
    /// Whether the planner may lower quals into a pushed-down expression.
    ///
    /// # Why this is a server option and not a probe
    ///
    /// Both transports evaluate expressions, but a plugin-channel `yesnod` can
    /// be configured not to -- `channel_max_expr_bytes = 0` -- and it says so
    /// only in its greeting, which is to say only once something has connected.
    /// `plan_pushdown` runs in the planner and holds no connection, so the fact
    /// has to be *declared* rather than discovered. An operator who turned
    /// evaluation off on the server sets this to `off` on the matching `SERVER`.
    ///
    /// Probing instead was considered and rejected. The planner would have to
    /// connect once per planned scan -- three call sites reach `plan_pushdown`
    /// -- to learn a fact that cannot change between statements, and planning
    /// would start to fail when the server was merely unreachable.
    ///
    /// # Default, and which way the failure points
    ///
    /// `true`, because every server evaluates expressions unless configured
    /// otherwise, so the default is right for every deployment that did not go
    /// out of its way. The mismatch -- on here, zero there -- is a query that
    /// **fails** naming this option, not a wrong answer. The reverse default
    /// would have made the mismatch a silent loss of every filter, which is the
    /// failure that cannot be noticed.
    pub pushdown: bool,
}

impl ServerOptions {
    pub fn parse(opts: &[(String, String)]) -> Result<Self, OptionError> {
        let mut endpoint = None;
        let mut socket = None;
        let mut data_dir = None;
        let mut dictionary = None;
        let mut term_column = None;
        let mut key_column = None;
        let mut batch_rows = DEFAULT_BATCH_ROWS;
        let mut pushdown = true;

        for (name, value) in opts {
            match name.as_str() {
                "endpoint" => endpoint = Some(value.clone()),
                "socket" => socket = Some(value.clone()),
                "data_dir" => data_dir = Some(value.clone()),
                "dictionary" => dictionary = Some(value.clone()),
                "term_column" => term_column = Some(value.clone()),
                "key_column" => key_column = Some(value.clone()),
                "batch_rows" => {
                    batch_rows =
                        value
                            .parse::<usize>()
                            .ok()
                            .filter(|n| *n > 0)
                            .ok_or_else(|| OptionError::BadValue {
                                name: "batch_rows",
                                value: value.clone(),
                                why: "expected a positive integer",
                            })?
                }
                "pushdown" => pushdown = parse_bool("pushdown", value)?,
                _ => {
                    return Err(OptionError::Unknown {
                        catalog: "SERVER",
                        name: name.clone(),
                    })
                }
            }
        }

        // Counted rather than enumerated: three options make six pairwise
        // conflicts, and a match over tuples would grow quadratically while
        // saying the same thing.
        let named = [endpoint.is_some(), socket.is_some(), data_dir.is_some()]
            .into_iter()
            .filter(|named| *named)
            .count();
        if named > 1 {
            return Err(OptionError::Conflict(
                "options \"endpoint\", \"socket\" and \"data_dir\" are mutually \
                 exclusive: they are three different deployments, not three \
                 spellings of one -- a yesnod over gRPC, a yesnod over a Unix \
                 socket, or a database directory opened in-process",
            ));
        }
        let transport = if let Some(e) = endpoint {
            Transport::Flight { endpoint: e }
        } else if let Some(s) = socket {
            Transport::Channel { socket: s }
        } else if let Some(d) = data_dir {
            Transport::Local { data_dir: d }
        } else {
            return Err(OptionError::Missing(
                "one of \"endpoint\", \"socket\" or \"data_dir\" is required",
            ));
        };

        // `term_column` and `key_column` describe the dictionary, so naming
        // them without one is a mistake worth reporting rather than ignoring —
        // it almost always means the `dictionary` option was misspelled and the
        // user is about to wonder why IMPORT FOREIGN SCHEMA finds nothing.
        let dictionary = match dictionary {
            Some(relation) => Some(Dictionary {
                relation,
                term_column: term_column.unwrap_or_else(|| DEFAULT_TERM_COLUMN.into()),
                key_column: key_column.unwrap_or_else(|| DEFAULT_KEY_COLUMN.into()),
            }),
            None => {
                if term_column.is_some() || key_column.is_some() {
                    return Err(OptionError::Missing(
                        "\"term_column\" and \"key_column\" require \"dictionary\"",
                    ));
                }
                None
            }
        };

        Ok(ServerOptions {
            transport,
            dictionary,
            batch_rows,
            pushdown,
        })
    }
}

/// A boolean option, in the spellings PostgreSQL's own `defGetBoolean` accepts.
///
/// Those spellings and no others. A user who writes `pushdown 'yes'` is writing
/// something every other PostgreSQL option would accept, and an FDW that took
/// only `'true'` would be the odd one out for no reason. Equally, a value
/// outside the set is **refused rather than read as false**: a typo that
/// silently turned pushdown off would show up as a query that got slower, which
/// is the hardest kind of misconfiguration to trace back to its cause.
fn parse_bool(name: &'static str, value: &str) -> Result<bool, OptionError> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(OptionError::BadValue {
            name,
            value: value.to_owned(),
            why: "expected a boolean: on, off, true, false, yes, no, 1 or 0",
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableOptions {
    /// The yesno key whose ordinal set this table exposes.
    pub key: u64,
}

impl TableOptions {
    pub fn parse(opts: &[(String, String)]) -> Result<Self, OptionError> {
        let mut key = None;
        for (name, value) in opts {
            match name.as_str() {
                "key" => key = Some(value.clone()),
                _ => {
                    return Err(OptionError::Unknown {
                        catalog: "FOREIGN TABLE",
                        name: name.clone(),
                    })
                }
            }
        }
        let raw = key.ok_or(OptionError::Missing(
            "option \"key\" is required on a yesno foreign table",
        ))?;

        // Parsed from a **decimal string**, and taken as a string option
        // rather than through any numeric path, because a key is a full `u64`
        // and PostgreSQL's `bigint` cannot hold the top half without the same
        // sign reinterpretation the ordinal column needs. A key is an
        // identifier, not a quantity — there is no arithmetic on it — so a
        // string is the honest representation.
        let key = raw.parse::<u64>().map_err(|_| OptionError::BadValue {
            name: "key",
            value: raw.clone(),
            why: "expected an unsigned 64-bit integer in decimal",
        })?;

        Ok(TableOptions { key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect()
    }

    #[test]
    fn a_flight_server_needs_only_an_endpoint() {
        let s = ServerOptions::parse(&o(&[("endpoint", "grpc://127.0.0.1:50051")])).unwrap();
        assert_eq!(
            s.transport,
            Transport::Flight {
                endpoint: "grpc://127.0.0.1:50051".into()
            }
        );
        assert_eq!(s.batch_rows, DEFAULT_BATCH_ROWS);
        assert!(s.dictionary.is_none());
    }

    #[test]
    fn endpoint_and_data_dir_are_mutually_exclusive() {
        let err = ServerOptions::parse(&o(&[("endpoint", "grpc://x"), ("data_dir", "/var/lib/y")]))
            .unwrap_err();
        assert!(matches!(err, OptionError::Conflict(_)), "{err}");
    }

    #[test]
    fn a_server_with_neither_transport_is_rejected() {
        let err = ServerOptions::parse(&o(&[])).unwrap_err();
        assert!(matches!(err, OptionError::Missing(_)), "{err}");
    }

    /// The rule that turns a typo into an error at `CREATE SERVER` time
    /// instead of a silent default at query time.
    #[test]
    fn an_unknown_server_option_is_an_error_not_a_default() {
        let err =
            ServerOptions::parse(&o(&[("endpoint", "grpc://x"), ("endpiont", "y")])).unwrap_err();
        match err {
            OptionError::Unknown { catalog, ref name } => {
                assert_eq!(catalog, "SERVER");
                assert_eq!(name, "endpiont");
            }
            other => panic!("expected Unknown, got {other}"),
        }
    }

    #[test]
    fn dictionary_columns_default_but_require_a_dictionary() {
        let s = ServerOptions::parse(&o(&[
            ("endpoint", "grpc://x"),
            ("dictionary", "public.yesno_terms"),
        ]))
        .unwrap();
        let d = s.dictionary.unwrap();
        assert_eq!(d.relation, "public.yesno_terms");
        assert_eq!(d.term_column, DEFAULT_TERM_COLUMN);
        assert_eq!(d.key_column, DEFAULT_KEY_COLUMN);

        // Naming a column without a dictionary is almost always a misspelled
        // `dictionary`, so it is reported rather than ignored.
        let err = ServerOptions::parse(&o(&[("endpoint", "grpc://x"), ("term_column", "t")]))
            .unwrap_err();
        assert!(matches!(err, OptionError::Missing(_)), "{err}");
    }

    #[test]
    fn batch_rows_must_be_a_positive_integer() {
        for bad in ["0", "-1", "many", ""] {
            let err = ServerOptions::parse(&o(&[("endpoint", "grpc://x"), ("batch_rows", bad)]))
                .unwrap_err();
            assert!(
                matches!(err, OptionError::BadValue { .. }),
                "batch_rows={bad:?} should be rejected, got {err}"
            );
        }
        let s =
            ServerOptions::parse(&o(&[("endpoint", "grpc://x"), ("batch_rows", "512")])).unwrap();
        assert_eq!(s.batch_rows, 512);
    }

    /// The reason `key` is a string option: keys in the top half of the `u64`
    /// range must round-trip, and `bigint` cannot express them without the same
    /// sign reinterpretation the ordinal column needs.
    #[test]
    fn a_key_spans_the_whole_u64_domain() {
        for k in [0u64, 1, (1 << 63) - 1, 1 << 63, u64::MAX - 1, u64::MAX] {
            let t = TableOptions::parse(&o(&[("key", &k.to_string())])).unwrap();
            assert_eq!(t.key, k);
        }
    }

    #[test]
    fn a_table_without_a_key_is_rejected() {
        assert!(matches!(
            TableOptions::parse(&o(&[])).unwrap_err(),
            OptionError::Missing(_)
        ));
    }

    #[test]
    fn a_non_numeric_or_out_of_range_key_is_rejected() {
        for bad in ["-1", "18446744073709551616", "0x10", "", "1.0"] {
            let err = TableOptions::parse(&o(&[("key", bad)])).unwrap_err();
            assert!(
                matches!(err, OptionError::BadValue { .. }),
                "key={bad:?} should be rejected, got {err}"
            );
        }
    }

    #[test]
    fn an_unknown_table_option_is_an_error() {
        let err = TableOptions::parse(&o(&[("key", "1"), ("keyy", "2")])).unwrap_err();
        assert!(matches!(err, OptionError::Unknown { .. }), "{err}");
    }

    /// Pushdown is on unless the operator says otherwise.
    ///
    /// The default is load-bearing and not a convenience: it is right for every
    /// deployment that did not go out of its way, and the reverse default would
    /// have turned a mismatch between this and the daemon's own setting into a
    /// silent loss of every filter rather than a query that fails and says why.
    #[test]
    fn pushdown_defaults_to_on() {
        let s = ServerOptions::parse(&o(&[("endpoint", "grpc://x")])).unwrap();
        assert!(s.pushdown);
    }

    /// Every spelling `defGetBoolean` takes, because a user writing
    /// `pushdown 'yes'` is writing what every other PostgreSQL option accepts.
    #[test]
    fn pushdown_takes_every_postgresql_boolean_spelling() {
        for yes in ["on", "true", "yes", "1", "ON", "True", "YES"] {
            let s = ServerOptions::parse(&o(&[("endpoint", "grpc://x"), ("pushdown", yes)]))
                .unwrap_or_else(|e| panic!("{yes:?} should parse: {e}"));
            assert!(s.pushdown, "{yes:?} should be true");
        }
        for no in ["off", "false", "no", "0", "OFF", "False", "No"] {
            let s = ServerOptions::parse(&o(&[("endpoint", "grpc://x"), ("pushdown", no)]))
                .unwrap_or_else(|e| panic!("{no:?} should parse: {e}"));
            assert!(!s.pushdown, "{no:?} should be false");
        }
    }

    /// A value outside the set is refused, **not read as false**.
    ///
    /// A typo that silently turned pushdown off would surface as a query that
    /// got slower, which is the hardest misconfiguration to trace to its cause.
    #[test]
    fn a_misspelled_pushdown_value_is_refused_rather_than_read_as_off() {
        for bad in ["", "nope", "offf", "2", "-1", "t"] {
            let err = ServerOptions::parse(&o(&[("endpoint", "grpc://x"), ("pushdown", bad)]))
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    OptionError::BadValue {
                        name: "pushdown",
                        ..
                    }
                ),
                "pushdown={bad:?} should be rejected, got {err}"
            );
        }
    }

    /// It applies to a channel server, which is the case it exists for, and to
    /// a Flight one, which is where it is a diagnostic rather than a necessity.
    #[test]
    fn pushdown_is_independent_of_the_transport() {
        for (name, value) in [("endpoint", "grpc://x"), ("socket", "/run/y.sock")] {
            let s = ServerOptions::parse(&o(&[(name, value), ("pushdown", "off")])).unwrap();
            assert!(!s.pushdown, "{name} should honour pushdown");
        }
    }

    /// The pre-commit flush reopens a connection from a buffer key alone, so a
    /// key that cannot be decoded back into the transport it came from is a
    /// transaction with nowhere to commit.
    #[test]
    fn a_buffer_key_decodes_back_to_its_own_transport() {
        for t in [
            Transport::Flight {
                endpoint: "grpc://127.0.0.1:50051".into(),
            },
            Transport::Channel {
                socket: "/run/yesno/plugin.sock".into(),
            },
            // A socket path that looks like an endpoint, and an endpoint that
            // looks like a path. Either one defeats a sniff; the tag does not
            // care what follows it.
            Transport::Channel {
                socket: "grpc://not-a-host".into(),
            },
            Transport::Flight {
                endpoint: "/unix/socket/shaped/endpoint".into(),
            },
        ] {
            let key = t.buffer_key().expect("both transports have an identity");
            assert_eq!(Transport::from_buffer_key(&key), Some(t), "key={key:?}");
        }
    }

    #[test]
    fn the_two_transports_never_share_a_buffer_key() {
        let flight = Transport::Flight {
            endpoint: "x".into(),
        }
        .buffer_key();
        let channel = Transport::Channel { socket: "x".into() }.buffer_key();
        assert_ne!(flight, channel);
    }

    /// `data_dir` has no identity because it cannot connect, and the write
    /// path reports rather than buffering under a key no flush could reopen.
    #[test]
    fn an_in_process_transport_has_no_buffer_key() {
        assert_eq!(
            Transport::Local {
                data_dir: "/var/lib/yesno".into()
            }
            .buffer_key(),
            None
        );
    }

    #[test]
    fn an_untagged_identity_is_not_decoded() {
        for bad in [
            "grpc://127.0.0.1:50051",
            "/run/yesno/plugin.sock",
            "",
            "x:y",
        ] {
            assert_eq!(Transport::from_buffer_key(bad), None, "{bad:?}");
        }
    }
}
