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
    Flight { endpoint: String },
    Local { data_dir: String },
}

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
}

impl ServerOptions {
    pub fn parse(opts: &[(String, String)]) -> Result<Self, OptionError> {
        let mut endpoint = None;
        let mut data_dir = None;
        let mut dictionary = None;
        let mut term_column = None;
        let mut key_column = None;
        let mut batch_rows = DEFAULT_BATCH_ROWS;

        for (name, value) in opts {
            match name.as_str() {
                "endpoint" => endpoint = Some(value.clone()),
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
                _ => {
                    return Err(OptionError::Unknown {
                        catalog: "SERVER",
                        name: name.clone(),
                    })
                }
            }
        }

        let transport = match (endpoint, data_dir) {
            (Some(_), Some(_)) => {
                return Err(OptionError::Conflict(
                    "options \"endpoint\" and \"data_dir\" are mutually exclusive: \
                     one names a yesnod to connect to, the other a database directory \
                     to open in-process",
                ))
            }
            (Some(e), None) => Transport::Flight { endpoint: e },
            (None, Some(d)) => Transport::Local { data_dir: d },
            (None, None) => {
                return Err(OptionError::Missing(
                    "one of \"endpoint\" or \"data_dir\" is required",
                ))
            }
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
        })
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
}
