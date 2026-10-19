//! A Common Expression Language (CEL) implementation for the expressions
//! Kubernetes evaluates: ValidatingAdmissionPolicy and webhook
//! `matchConditions` (untyped, see [`compile`]) and CRD
//! `x-kubernetes-validations` rules (type-checked against the schema and
//! cost-estimated like the real API server, see [`check`] and [`cost`]).
//!
//! - [`parse`]: lexer and parser, with the source positions cel-go reports
//!   and the standard macros (`has`, `all`, `exists`, `exists_one`, `map`,
//!   `filter`), the two-variable comprehensions (`all(i, v, p)`,
//!   `transformList`, `transformMap`, ...) and optional syntax (`a.?b`,
//!   `m[?k]`).
//! - [`eval`]: the interpreter and the function library: the standard
//!   functions, the cel-go string extensions, and the Kubernetes libraries
//!   (lists, regex, URLs, quantities, IPs and CIDRs, sets, named formats,
//!   optionals, timestamps and durations).
//! - [`check`]: static type checking against declared variable types.
//! - [`cost`]: cel-go's static cost estimate with the Kubernetes size
//!   estimator, which decides whether a CRD rule is admitted.

pub mod check;
pub mod cost;
pub mod eval;
pub mod lib;
pub mod parse;

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value as Json;

pub use eval::Env;
pub use parse::{Expr, Kind, Program};

/// A runtime value.
#[derive(Debug, Clone)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Double(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Val>),
    Map(BTreeMap<String, Val>),
    /// Nanoseconds.
    Duration(i64),
    Timestamp(chrono::DateTime<chrono::Utc>),
    Optional(Option<Box<Val>>),
    /// A type value (`type(x)`, `int`, `string`, ...), by name.
    Type(String),
    Quantity(lib::Quantity),
    Url(lib::Url),
    Ip(std::net::IpAddr),
    Cidr(std::net::IpAddr, u8),
    Format(String),
}

impl PartialEq for Val {
    fn eq(&self, other: &Val) -> bool {
        eval::equal(self, other)
    }
}

impl Val {
    /// A value from JSON without a schema (admission policies).
    pub fn from_json(v: &Json) -> Val {
        match v {
            Json::Null => Val::Null,
            Json::Bool(b) => Val::Bool(*b),
            Json::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
                (Some(i), _, _) => Val::Int(i),
                (None, Some(u), _) => Val::Uint(u),
                (_, _, Some(f)) => Val::Double(f),
                _ => Val::Null,
            },
            Json::String(s) => Val::Str(s.clone()),
            Json::Array(a) => Val::List(a.iter().map(Val::from_json).collect()),
            Json::Object(m) => Val::Map(m.iter().map(|(k, v)| (k.clone(), Val::from_json(v))).collect()),
        }
    }

    pub fn type_name(&self) -> String {
        match self {
            Val::Null => "null_type".into(),
            Val::Bool(_) => "bool".into(),
            Val::Int(_) => "int".into(),
            Val::Uint(_) => "uint".into(),
            Val::Double(_) => "double".into(),
            Val::Str(_) => "string".into(),
            Val::Bytes(_) => "bytes".into(),
            Val::List(_) => "list".into(),
            Val::Map(_) => "map".into(),
            Val::Duration(_) => "google.protobuf.Duration".into(),
            Val::Timestamp(_) => "google.protobuf.Timestamp".into(),
            Val::Optional(_) => "optional_type".into(),
            Val::Type(_) => "type".into(),
            Val::Quantity(_) => "kubernetes.Quantity".into(),
            Val::Url(_) => "kubernetes.URL".into(),
            Val::Ip(_) => "net.IP".into(),
            Val::Cidr(..) => "net.CIDR".into(),
            Val::Format(_) => "kubernetes.NamedFormat".into(),
        }
    }
}

impl fmt::Display for Val {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::Null => write!(f, "null"),
            Val::Bool(b) => write!(f, "{b}"),
            Val::Int(i) => write!(f, "{i}"),
            Val::Uint(u) => write!(f, "{u}"),
            Val::Double(d) => write!(f, "{}", eval::format_double(*d)),
            Val::Str(s) => write!(f, "{s}"),
            Val::Bytes(b) => write!(f, "{}", String::from_utf8_lossy(b)),
            Val::List(l) => {
                write!(f, "[")?;
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Val::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            Val::Duration(d) => write!(f, "{}", eval::duration_string(*d)),
            Val::Timestamp(t) => write!(f, "{}", eval::timestamp_string(t)),
            Val::Optional(None) => write!(f, "optional.none()"),
            Val::Optional(Some(v)) => write!(f, "optional.of({v})"),
            Val::Type(t) => write!(f, "{t}"),
            Val::Quantity(q) => write!(f, "{}", q.raw),
            Val::Url(u) => write!(f, "{}", u.raw),
            Val::Ip(ip) => write!(f, "{ip}"),
            Val::Cidr(ip, bits) => write!(f, "{ip}/{bits}"),
            Val::Format(n) => write!(f, "{n}"),
        }
    }
}

/// Parse an expression for untyped evaluation (admission policies,
/// `matchConditions`): syntax errors only, type errors surface at runtime.
pub fn compile(src: &str) -> Result<Program, String> {
    parse::parse(src).map_err(|e| e.render(src))
}

#[cfg(test)]
mod tests;
