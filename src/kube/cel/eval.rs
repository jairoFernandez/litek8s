//! The CEL interpreter and function library. Runtime error messages follow
//! cel-go (`no such key: x`, `division by zero`, ...) because the API
//! server copies them into validation errors.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};

use super::lib::{self, Quantity};
use super::parse::{Comp, Expr, Kind, Program};
use super::Val;

/// Variables visible to an expression, and the runtime cost budget.
#[derive(Default)]
pub struct Env {
    pub vars: BTreeMap<String, Val>,
    /// Cost units spent so far (roughly cel-go's runtime cost: one per
    /// variable, field selection, operator and call, plus string traversal).
    pub cost: std::cell::Cell<u64>,
    /// Stop with [`COST_EXCEEDED`] past this cost (0: unlimited).
    pub cost_limit: u64,
}

/// The error of an evaluation that ran out of its cost limit.
pub const COST_EXCEEDED: &str = "operation cancelled: actual cost limit exceeded";

impl Env {
    fn charge(&self, units: u64) -> Result<(), String> {
        let c = self.cost.get().saturating_add(units);
        self.cost.set(c);
        if self.cost_limit > 0 && c > self.cost_limit {
            return Err(COST_EXCEEDED.into());
        }
        Ok(())
    }
}

/// Traversal cost of a string or bytes value (0.1 per byte, rounded up).
fn traversal(v: &Val) -> u64 {
    match v {
        Val::Str(s) => (s.len() as u64).div_ceil(10),
        Val::Bytes(b) => (b.len() as u64).div_ceil(10),
        _ => 0,
    }
}

impl Program {
    pub fn eval(&self, env: &Env) -> Result<Val, String> {
        eval(&self.root, env, &mut Vec::new())
    }
}

type Scope = Vec<(String, Val)>;

const TYPE_NAMES: &[&str] = &["int", "uint", "double", "bool", "string", "bytes", "list", "map", "null_type", "type"];

fn lookup(env: &Env, scope: &Scope, name: &str) -> Result<Val, String> {
    if let Some((_, v)) = scope.iter().rev().find(|(n, _)| n == name) {
        return Ok(v.clone());
    }
    if let Some(v) = env.vars.get(name) {
        return Ok(v.clone());
    }
    if TYPE_NAMES.contains(&name) {
        return Ok(Val::Type(name.to_string()));
    }
    Err(format!("undeclared reference to '{name}'"))
}

fn truth(v: Val) -> Result<bool, String> {
    match v {
        Val::Bool(b) => Ok(b),
        other => Err(format!("no such overload: expected bool, got {}", other.type_name())),
    }
}

fn no_overload(name: &str, args: &[&Val]) -> String {
    let types: Vec<String> = args.iter().map(|a| a.type_name()).collect();
    format!("no such overload: {name}({})", types.join(", "))
}

fn select(base: Val, field: &str) -> Result<Val, String> {
    match base {
        Val::Map(mut m) => m.remove(field).ok_or_else(|| format!("no such key: {field}")),
        Val::Null => Err(format!("no such key: {field}")),
        other => Err(format!("type '{}' does not support field selection", other.type_name())),
    }
}

fn opt(v: Option<Val>) -> Val {
    Val::Optional(v.map(Box::new))
}

fn list_index(l: &[Val], idx: &Val) -> Result<Option<Val>, String> {
    let i = match idx {
        Val::Int(i) => *i,
        Val::Uint(u) => i64::try_from(*u).unwrap_or(i64::MAX),
        Val::Double(d) if d.fract() == 0.0 => *d as i64,
        other => return Err(no_overload("_[_]", &[&Val::List(vec![]), other])),
    };
    Ok(usize::try_from(i).ok().and_then(|i| l.get(i)).cloned())
}

fn map_key(k: &Val) -> String {
    match k {
        Val::Str(s) => s.clone(),
        other => other.to_string(),
    }
}

pub(super) fn eval(e: &Expr, env: &Env, scope: &mut Scope) -> Result<Val, String> {
    match &e.kind {
        Kind::Lit(v) => Ok(v.clone()),
        Kind::Ident(name) => {
            env.charge(1)?;
            lookup(env, scope, name)
        }
        Kind::Select { operand, field, test: true, .. } => Ok(Val::Bool(match eval(operand, env, scope)? {
            Val::Map(m) => m.contains_key(field),
            Val::Null => false,
            other => return Err(format!("invalid type for field selection: {}", other.type_name())),
        })),
        Kind::Select { operand, field, optional: true, .. } => match eval(operand, env, scope)? {
            Val::Map(mut m) => Ok(opt(m.remove(field))),
            Val::Optional(None) => Ok(Val::Optional(None)),
            Val::Optional(Some(inner)) => match *inner {
                Val::Map(mut m) => Ok(opt(m.remove(field))),
                other => Err(format!("type '{}' does not support field selection", other.type_name())),
            },
            Val::Null => Ok(Val::Optional(None)),
            other => Err(format!("type '{}' does not support field selection", other.type_name())),
        },
        Kind::Select { operand, field, .. } => match {
            env.charge(1)?;
            eval(operand, env, scope)?
        } {
            Val::Optional(None) => Ok(Val::Optional(None)),
            Val::Optional(Some(inner)) => Ok(opt(Some(select(*inner, field)?))),
            base => select(base, field),
        },
        Kind::Index { operand, index, optional } => {
            let base = eval(operand, env, scope)?;
            let idx = eval(index, env, scope)?;
            let base = match (base, optional) {
                (Val::Optional(None), true) => return Ok(Val::Optional(None)),
                (Val::Optional(Some(b)), true) => *b,
                (b, _) => b,
            };
            match (base, *optional) {
                (Val::List(l), false) => list_index(&l, &idx)?.ok_or_else(|| {
                    format!("index '{}' out of range in list size '{}'", idx, l.len())
                }),
                (Val::List(l), true) => Ok(opt(list_index(&l, &idx)?)),
                (Val::Map(mut m), false) => {
                    let k = map_key(&idx);
                    m.remove(&k).ok_or_else(|| format!("no such key: {k}"))
                }
                (Val::Map(mut m), true) => Ok(opt(m.remove(&map_key(&idx)))),
                (b, _) => Err(no_overload("_[_]", &[&b, &idx])),
            }
        }
        Kind::List(items) => Ok(Val::List(items.iter().map(|i| eval(i, env, scope)).collect::<Result<_, _>>()?)),
        Kind::Map(entries) => {
            let mut m = BTreeMap::new();
            for (k, v) in entries {
                let key = map_key(&eval(k, env, scope)?);
                let val = eval(v, env, scope)?;
                if m.insert(key.clone(), val).is_some() {
                    return Err(format!("Failed with repeated key: {key}"));
                }
            }
            Ok(Val::Map(m))
        }
        Kind::Unary("!", inner) => Ok(Val::Bool(!truth(eval(inner, env, scope)?)?)),
        Kind::Unary(_, inner) => match eval(inner, env, scope)? {
            Val::Int(i) => i.checked_neg().map(Val::Int).ok_or_else(|| "integer overflow".into()),
            Val::Double(d) => Ok(Val::Double(-d)),
            Val::Duration(d) => d.checked_neg().map(Val::Duration).ok_or_else(|| "integer overflow".into()),
            other => Err(no_overload("-_", &[&other])),
        },
        Kind::Cond(c, a, b) => {
            if truth(eval(c, env, scope)?)? {
                eval(a, env, scope)
            } else {
                eval(b, env, scope)
            }
        }
        Kind::Binary("&&", a, b) => {
            let left = eval(a, env, scope).and_then(truth);
            if left == Ok(false) {
                return Ok(Val::Bool(false));
            }
            let right = eval(b, env, scope).and_then(truth);
            logical_and(left, right).map(Val::Bool)
        }
        Kind::Binary("||", a, b) => {
            let left = eval(a, env, scope).and_then(truth);
            if left == Ok(true) {
                return Ok(Val::Bool(true));
            }
            let right = eval(b, env, scope).and_then(truth);
            logical_or(left, right).map(Val::Bool)
        }
        Kind::Binary(op, a, b) => {
            let (a, b) = (eval(a, env, scope)?, eval(b, env, scope)?);
            let strings = matches!(*op, "==" | "!=" | "<" | "<=" | ">" | ">=") && traversal(&a).min(traversal(&b)) > 0;
            env.charge(1 + if strings { traversal(&a).min(traversal(&b)) } else if *op == "+" { traversal(&a) + traversal(&b) } else { 0 })?;
            binary(op, a, b)
        }
        Kind::Call { target, func, args } => {
            env.charge(1)?;
            call(target.as_deref(), func, args, env, scope)
        }
        Kind::Comp(c) => comprehension(c, env, scope),
    }
}

fn logical_and(left: Result<bool, String>, right: Result<bool, String>) -> Result<bool, String> {
    match (left, right) {
        (Ok(false), _) | (_, Ok(false)) => Ok(false),
        (Err(e), _) | (_, Err(e)) => Err(e),
        _ => Ok(true),
    }
}

fn logical_or(left: Result<bool, String>, right: Result<bool, String>) -> Result<bool, String> {
    match (left, right) {
        (Ok(true), _) | (_, Ok(true)) => Ok(true),
        (Err(e), _) | (_, Err(e)) => Err(e),
        _ => Ok(false),
    }
}

fn comprehension(c: &Comp, env: &Env, scope: &mut Scope) -> Result<Val, String> {
    let range = eval(&c.range, env, scope)?;
    match c.name {
        "optMap" | "optFlatMap" => {
            let Val::Optional(inner) = range else { return Err(no_overload(c.name, &[&range])) };
            let Some(v) = inner else { return Ok(Val::Optional(None)) };
            scope.push((c.v1.clone(), *v));
            let r = eval(&c.args[0], env, scope);
            scope.pop();
            let r = r?;
            return if c.name == "optMap" {
                Ok(opt(Some(r)))
            } else {
                match r {
                    Val::Optional(_) => Ok(r),
                    other => Err(no_overload("optFlatMap", &[&other])),
                }
            };
        }
        _ => {}
    }
    let pairs: Vec<(Val, Val)> = match range {
        Val::List(l) => l.into_iter().enumerate().map(|(i, v)| (Val::Int(i as i64), v)).collect(),
        Val::Map(m) => m.into_iter().map(|(k, v)| (Val::Str(k), v)).collect(),
        other => return Err(format!("expression of type '{}' cannot be range of a comprehension (must be list, map, or dynamic)", other.type_name())),
    };
    let two = c.v2.is_some();
    let mut acc_bool: Result<bool, String> = Ok(c.name == "all");
    let mut count = 0usize;
    let mut out_list = Vec::new();
    let mut out_map = BTreeMap::new();
    for (k, v) in pairs {
        if c.name == "all" && acc_bool == Ok(false) || c.name == "exists" && acc_bool == Ok(true) {
            break;
        }
        // The loop condition and step of the macro expansion.
        env.charge(2)?;
        let pushed = if two {
            scope.push((c.v1.clone(), k.clone()));
            scope.push((c.v2.clone().unwrap(), v.clone()));
            2
        } else {
            // Over a map, the single variable is the key.
            let item = if matches!(k, Val::Str(_)) { k.clone() } else { v.clone() };
            scope.push((c.v1.clone(), item));
            1
        };
        let item_for_filter = if two { v.clone() } else if matches!(k, Val::Str(_)) { k.clone() } else { v.clone() };
        let r: Result<(), String> = (|| {
            match (c.name, c.args.as_slice()) {
                ("all", [p]) => acc_bool = logical_and(acc_bool.clone(), eval(p, env, scope).and_then(truth)),
                ("exists", [p]) => acc_bool = logical_or(acc_bool.clone(), eval(p, env, scope).and_then(truth)),
                ("exists_one", [p]) => {
                    if truth(eval(p, env, scope)?)? {
                        count += 1;
                    }
                }
                ("map", [f]) | ("transformList", [f]) => out_list.push(eval(f, env, scope)?),
                ("map", [p, f]) | ("transformList", [p, f]) => {
                    if truth(eval(p, env, scope)?)? {
                        out_list.push(eval(f, env, scope)?);
                    }
                }
                ("filter", [p]) => {
                    if truth(eval(p, env, scope)?)? {
                        out_list.push(item_for_filter.clone());
                    }
                }
                ("transformMap", args) => {
                    let keep = match args {
                        [p, _] => truth(eval(p, env, scope)?)?,
                        _ => true,
                    };
                    if keep {
                        out_map.insert(map_key(&k), eval(args.last().unwrap(), env, scope)?);
                    }
                }
                ("transformMapEntry", args) => {
                    let keep = match args {
                        [p, _] => truth(eval(p, env, scope)?)?,
                        _ => true,
                    };
                    if keep {
                        match eval(args.last().unwrap(), env, scope)? {
                            Val::Map(m) if m.len() == 1 => {
                                let (key, val) = m.into_iter().next().unwrap();
                                if out_map.insert(key.clone(), val).is_some() {
                                    return Err(format!("insert failed: key {key} already exists"));
                                }
                            }
                            other => return Err(format!("transformMapEntry: expected a single-entry map, got {}", other.type_name())),
                        }
                    }
                }
                _ => return Err(format!("wrong number of arguments to {}()", c.name)),
            }
            Ok(())
        })();
        for _ in 0..pushed {
            scope.pop();
        }
        r?;
    }
    match c.name {
        "all" | "exists" => acc_bool.map(Val::Bool),
        "exists_one" => Ok(Val::Bool(count == 1)),
        "transformMap" | "transformMapEntry" => Ok(Val::Map(out_map)),
        _ => Ok(Val::List(out_list)),
    }
}

fn num(v: &Val) -> Option<f64> {
    match v {
        Val::Int(i) => Some(*i as f64),
        Val::Uint(u) => Some(*u as f64),
        Val::Double(d) => Some(*d),
        _ => None,
    }
}

/// Ordering across numeric types is exact for int/uint pairs.
fn num_cmp(a: &Val, b: &Val) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => Some(x.cmp(y)),
        (Val::Uint(x), Val::Uint(y)) => Some(x.cmp(y)),
        (Val::Int(x), Val::Uint(y)) => Some(if *x < 0 { std::cmp::Ordering::Less } else { (*x as u64).cmp(y) }),
        (Val::Uint(x), Val::Int(y)) => Some(if *y < 0 { std::cmp::Ordering::Greater } else { x.cmp(&(*y as u64)) }),
        _ => num(a)?.partial_cmp(&num(b)?),
    }
}

pub(super) fn compare(a: &Val, b: &Val) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Val::Str(x), Val::Str(y)) => Some(x.cmp(y)),
        (Val::Bytes(x), Val::Bytes(y)) => Some(x.cmp(y)),
        (Val::Bool(x), Val::Bool(y)) => Some(x.cmp(y)),
        (Val::Duration(x), Val::Duration(y)) => Some(x.cmp(y)),
        (Val::Timestamp(x), Val::Timestamp(y)) => Some(x.cmp(y)),
        (Val::Quantity(x), Val::Quantity(y)) => Some(x.cmp(y)),
        _ => num_cmp(a, b),
    }
}

fn binary(op: &str, a: Val, b: Val) -> Result<Val, String> {
    use Val::*;
    let overload = |a: &Val, b: &Val| no_overload(&format!("_{op}_"), &[a, b]);
    match op {
        "==" => Ok(Bool(equal(&a, &b))),
        "!=" => Ok(Bool(!equal(&a, &b))),
        "<" | "<=" | ">" | ">=" => {
            let ord = compare(&a, &b).ok_or_else(|| overload(&a, &b))?;
            Ok(Bool(match op {
                "<" => ord.is_lt(),
                "<=" => ord.is_le(),
                ">" => ord.is_gt(),
                _ => ord.is_ge(),
            }))
        }
        "in" => match &b {
            List(l) => Ok(Bool(l.iter().any(|x| equal(x, &a)))),
            Map(m) => Ok(Bool(m.contains_key(&map_key(&a)))),
            _ => Err(overload(&a, &b)),
        },
        "+" => match (a, b) {
            (Int(x), Int(y)) => x.checked_add(y).map(Int).ok_or_else(|| "integer overflow".into()),
            (Uint(x), Uint(y)) => x.checked_add(y).map(Uint).ok_or_else(|| "unsigned integer overflow".into()),
            (Double(x), Double(y)) => Ok(Double(x + y)),
            (Str(x), Str(y)) => Ok(Str(x + &y)),
            (Bytes(mut x), Bytes(y)) => {
                x.extend(y);
                Ok(Bytes(x))
            }
            (List(mut x), List(y)) => {
                x.extend(y);
                Ok(List(x))
            }
            (Duration(x), Duration(y)) => x.checked_add(y).map(Duration).ok_or_else(|| "integer overflow".into()),
            (Timestamp(t), Duration(d)) | (Duration(d), Timestamp(t)) => ts_add(t, d),
            (a, b) => Err(overload(&a, &b)),
        },
        "-" => match (a, b) {
            (Int(x), Int(y)) => x.checked_sub(y).map(Int).ok_or_else(|| "integer overflow".into()),
            (Uint(x), Uint(y)) => x.checked_sub(y).map(Uint).ok_or_else(|| "unsigned integer overflow".into()),
            (Double(x), Double(y)) => Ok(Double(x - y)),
            (Duration(x), Duration(y)) => x.checked_sub(y).map(Duration).ok_or_else(|| "integer overflow".into()),
            (Timestamp(t), Duration(d)) => ts_add(t, d.checked_neg().ok_or("integer overflow")?),
            (Timestamp(x), Timestamp(y)) => (x - y).num_nanoseconds().map(Duration).ok_or_else(|| "integer overflow".into()),
            (a, b) => Err(overload(&a, &b)),
        },
        "*" | "/" | "%" => match (a, b) {
            (Int(x), Int(y)) => match op {
                "*" => x.checked_mul(y).ok_or("integer overflow".to_string()),
                "/" if y == 0 => Err("division by zero".into()),
                "/" => x.checked_div(y).ok_or("integer overflow".to_string()),
                _ if y == 0 => Err("modulus by zero".into()),
                _ => x.checked_rem(y).ok_or("integer overflow".to_string()),
            }
            .map(Int),
            (Uint(x), Uint(y)) => match op {
                "*" => x.checked_mul(y).ok_or("unsigned integer overflow".to_string()),
                "/" if y == 0 => Err("division by zero".into()),
                "/" => Ok(x / y),
                _ if y == 0 => Err("modulus by zero".into()),
                _ => Ok(x % y),
            }
            .map(Uint),
            (Double(x), Double(y)) if op != "%" => Ok(Double(if op == "*" { x * y } else { x / y })),
            (a, b) => Err(overload(&a, &b)),
        },
        _ => Err(format!("unsupported operator {op}")),
    }
}

fn ts_add(t: DateTime<Utc>, d: i64) -> Result<Val, String> {
    let r = t.checked_add_signed(chrono::Duration::nanoseconds(d)).ok_or("timestamp overflow")?;
    check_ts_range(r)
}

fn check_ts_range(t: DateTime<Utc>) -> Result<Val, String> {
    if t.year() < 1 || t.year() > 9999 {
        return Err("timestamp overflow".into());
    }
    Ok(Val::Timestamp(t))
}

/// CEL equality: heterogeneous values are unequal, numbers compare across
/// types, lists and maps element-wise.
pub fn equal(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::List(x), Val::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| equal(p, q)),
        (Val::Map(x), Val::Map(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| equal(v, w))),
        (Val::Optional(x), Val::Optional(y)) => match (x, y) {
            (None, None) => true,
            (Some(p), Some(q)) => equal(p, q),
            _ => false,
        },
        (Val::Quantity(x), Val::Quantity(y)) => x.cmp(y).is_eq(),
        (Val::Url(x), Val::Url(y)) => x.raw == y.raw,
        (x, y) if num(x).is_some() && num(y).is_some() => num_cmp(x, y) == Some(std::cmp::Ordering::Equal),
        (Val::Null, Val::Null) => true,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Bytes(x), Val::Bytes(y)) => x == y,
        (Val::Duration(x), Val::Duration(y)) => x == y,
        (Val::Timestamp(x), Val::Timestamp(y)) => x == y,
        (Val::Type(x), Val::Type(y)) => x == y,
        (Val::Ip(x), Val::Ip(y)) => x == y,
        (Val::Cidr(x, a), Val::Cidr(y, b)) => x == y && a == b,
        (Val::Format(x), Val::Format(y)) => x == y,
        _ => false,
    }
}

fn size(v: &Val) -> Result<Val, String> {
    Ok(Val::Int(match v {
        Val::Str(s) => s.chars().count() as i64,
        Val::Bytes(b) => b.len() as i64,
        Val::List(l) => l.len() as i64,
        Val::Map(m) => m.len() as i64,
        other => return Err(no_overload("size", &[other])),
    }))
}

fn regex(pattern: &str) -> Result<Arc<regex::Regex>, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<regex::Regex>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().unwrap().get(pattern) {
        return Ok(r.clone());
    }
    let re = Arc::new(regex::Regex::new(pattern).map_err(|e| format!("error parsing regexp: {}", re2_error(&e)))?);
    let mut c = cache.lock().unwrap();
    if c.len() > 512 {
        c.clear();
    }
    c.insert(pattern.to_string(), re.clone());
    Ok(re)
}

/// A regex compile error in the words of Go's regexp package, where the
/// syntax maps.
pub fn re2_error(e: &regex::Error) -> String {
    let msg = e.to_string();
    if msg.contains("look-around") || msg.contains("lookaround") {
        return "invalid or unsupported Perl syntax".into();
    }
    if msg.contains("unclosed character class") {
        return "missing closing ]".into();
    }
    if msg.contains("unclosed group") {
        return "missing closing )".into();
    }
    if msg.contains("unopened group") {
        return "unexpected )".into();
    }
    msg.lines().last().unwrap_or_default().trim().trim_start_matches("error: ").to_string()
}

/// Whether a regex compiles the way RE2 would accept it.
pub fn valid_regex(pattern: &str) -> Result<(), String> {
    regex(pattern).map(|_| ())
}

fn chars_of(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn int_arg(v: Option<&Val>, name: &str) -> Result<i64, String> {
    match v {
        Some(Val::Int(i)) => Ok(*i),
        Some(other) => Err(no_overload(name, &[other])),
        None => Err(format!("{name}: missing argument")),
    }
}

fn str_arg<'a>(v: Option<&'a Val>, name: &str) -> Result<&'a str, String> {
    match v {
        Some(Val::Str(s)) => Ok(s),
        Some(other) => Err(no_overload(name, &[other])),
        None => Err(format!("{name}: missing argument")),
    }
}

fn split_n(s: &str, sep: &str, n: i64) -> Vec<Val> {
    if n == 0 {
        return vec![];
    }
    let parts: Vec<String> = if sep.is_empty() {
        let cs: Vec<String> = s.chars().map(String::from).collect();
        if n > 0 && cs.len() > n as usize {
            let mut head: Vec<String> = cs[..n as usize - 1].to_vec();
            head.push(cs[n as usize - 1..].concat());
            head
        } else {
            cs
        }
    } else if n < 0 {
        s.split(sep).map(String::from).collect()
    } else {
        s.splitn(n as usize, sep).map(String::from).collect()
    };
    parts.into_iter().map(Val::Str).collect()
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn format_double(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    let a = d.abs();
    if a != 0.0 && !(1e-4..1e21).contains(&a) {
        let s = format!("{d:e}");
        let (m, e) = s.split_once('e').unwrap();
        let (sign, digits) = if let Some(rest) = e.strip_prefix('-') { ('-', rest) } else { ('+', e) };
        return format!("{m}e{sign}{digits:0>2}");
    }
    format!("{d}")
}

pub fn duration_string(nanos: i64) -> String {
    format!("{}s", format_double(nanos as f64 / 1e9).trim_end_matches(".0"))
}

pub fn timestamp_string(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

/// Go's time.ParseDuration, in nanoseconds.
pub fn parse_go_duration(s: &str) -> Result<i64, String> {
    let orig = s;
    let bad = || format!("time: invalid duration \"{orig}\"");
    let mut s = s;
    let mut neg = false;
    if let Some(r) = s.strip_prefix('-') {
        neg = true;
        s = r;
    } else if let Some(r) = s.strip_prefix('+') {
        s = r;
    }
    if s == "0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(bad());
    }
    let mut total: f64 = 0.0;
    while !s.is_empty() {
        let int_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let int_part = &s[..int_end];
        s = &s[int_end..];
        let mut frac_part = "";
        if let Some(r) = s.strip_prefix('.') {
            let fe = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            frac_part = &r[..fe];
            s = &r[fe..];
        }
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(bad());
        }
        let unit_end = s.find(|c: char| c == '.' || c.is_ascii_digit()).unwrap_or(s.len());
        let unit = &s[..unit_end];
        s = &s[unit_end..];
        let scale: f64 = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            "" => return Err(format!("time: missing unit in duration \"{orig}\"")),
            u => return Err(format!("time: unknown unit \"{u}\" in duration \"{orig}\"")),
        };
        let v: f64 = format!("{}.{}", if int_part.is_empty() { "0" } else { int_part }, if frac_part.is_empty() { "0" } else { frac_part })
            .parse()
            .map_err(|_| bad())?;
        total += v * scale;
    }
    if total > i64::MAX as f64 {
        return Err(bad());
    }
    let n = total.round() as i64;
    Ok(if neg { -n } else { n })
}

fn parse_timestamp(s: &str) -> Result<DateTime<Utc>, String> {
    let t = DateTime::parse_from_rfc3339(s).map_err(|_| format!("timestamp: invalid timestamp \"{s}\""))?;
    let t = t.with_timezone(&Utc);
    match check_ts_range(t)? {
        Val::Timestamp(t) => Ok(t),
        _ => unreachable!(),
    }
}

fn tz_offset(tz: Option<&Val>) -> Result<chrono::FixedOffset, String> {
    let utc = chrono::FixedOffset::east_opt(0).unwrap();
    let Some(tz) = tz else { return Ok(utc) };
    let Val::Str(name) = tz else { return Err(no_overload("timezone", &[tz])) };
    if name == "UTC" || name.is_empty() {
        return Ok(utc);
    }
    let s = name.as_str();
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'+') => (1, &s[1..]),
        Some(b'-') => (-1, &s[1..]),
        _ => (1, s),
    };
    if let Some((h, m)) = rest.split_once(':') {
        if let (Ok(h), Ok(m)) = (h.parse::<i32>(), m.parse::<i32>()) {
            return chrono::FixedOffset::east_opt(sign * (h * 3600 + m * 60)).ok_or_else(|| format!("invalid timezone {s}"));
        }
    }
    Err(format!("unknown time zone {s}"))
}

fn ts_getter(t: &DateTime<Utc>, name: &str, tz: Option<&Val>) -> Result<Val, String> {
    let local = t.with_timezone(&tz_offset(tz)?);
    Ok(Val::Int(match name {
        "getFullYear" => local.year() as i64,
        "getMonth" => local.month0() as i64,
        "getDayOfYear" => local.ordinal0() as i64,
        "getDayOfMonth" => local.day0() as i64,
        "getDate" => local.day() as i64,
        "getDayOfWeek" => local.weekday().num_days_from_sunday() as i64,
        "getHours" => local.hour() as i64,
        "getMinutes" => local.minute() as i64,
        "getSeconds" => local.second() as i64,
        "getMilliseconds" => (local.nanosecond() / 1_000_000) as i64,
        _ => return Err(format!("no such function {name}")),
    }))
}

fn to_string(v: &Val) -> Result<String, String> {
    Ok(match v {
        Val::Str(s) => s.clone(),
        Val::Int(_) | Val::Uint(_) | Val::Bool(_) | Val::Duration(_) | Val::Timestamp(_) | Val::Ip(_) | Val::Cidr(..) => v.to_string(),
        Val::Double(d) => format_double(*d),
        Val::Bytes(b) => String::from_utf8(b.clone()).map_err(|_| "invalid UTF-8 in bytes, cannot convert to string".to_string())?,
        other => return Err(no_overload("string", &[other])),
    })
}

/// `"...".format([args])` (cel-go strings extension).
fn string_format(fmt: &str, args: &[Val]) -> Result<String, String> {
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let mut n = 0;
    let shown = |v: &Val, top: bool| -> Result<String, String> {
        fn inner(v: &Val, top: bool) -> Result<String, String> {
            Ok(match v {
                Val::Str(s) if !top => quote(s),
                Val::List(l) => format!("[{}]", l.iter().map(|x| inner(x, false)).collect::<Result<Vec<_>, _>>()?.join(", ")),
                Val::Map(m) => format!(
                    "{{{}}}",
                    m.iter().map(|(k, x)| Ok(format!("{}: {}", quote(k), inner(x, false)?))).collect::<Result<Vec<_>, String>>()?.join(", ")
                ),
                Val::Null => "null".into(),
                Val::Type(t) => t.clone(),
                other => to_string(other)?,
            })
        }
        inner(v, top)
    };
    while i < chars.len() {
        let c = chars[i];
        if c != '%' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if chars.get(i) == Some(&'%') {
            out.push('%');
            i += 1;
            continue;
        }
        let mut precision: Option<usize> = None;
        if chars.get(i) == Some(&'.') {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            precision = chars[start..i].iter().collect::<String>().parse().ok();
        }
        let verb = *chars.get(i).ok_or("unexpected end of format string")?;
        i += 1;
        let arg = args.get(n).ok_or_else(|| format!("index {n} out of range"))?;
        n += 1;
        let p = precision.unwrap_or(6);
        out.push_str(&match (verb, arg) {
            ('s', v) => shown(v, true)?,
            ('d', Val::Int(x)) => x.to_string(),
            ('d', Val::Uint(x)) => x.to_string(),
            ('d', Val::Double(x)) => format!("{}", *x as i64),
            ('f', v) => format!("{:.*}", p, num(v).ok_or("error during formatting: fixed-point clause can only be used on doubles")?),
            ('e', v) => {
                let s = format!("{:.*e}", p, num(v).ok_or("error during formatting: scientific clause can only be used on doubles")?);
                let (m, e) = s.split_once('e').unwrap();
                let (sign, d) = if let Some(r) = e.strip_prefix('-') { ('-', r) } else { ('+', e) };
                format!("{m}e{sign}{d:0>2}")
            }
            ('b', Val::Int(x)) => format!("{x:b}"),
            ('b', Val::Uint(x)) => format!("{x:b}"),
            ('o', Val::Int(x)) => format!("{x:o}"),
            ('o', Val::Uint(x)) => format!("{x:o}"),
            ('x', Val::Int(x)) => format!("{x:x}"),
            ('x', Val::Uint(x)) => format!("{x:x}"),
            ('x', Val::Str(s)) => s.bytes().map(|b| format!("{b:02x}")).collect(),
            ('X', Val::Int(x)) => format!("{x:X}"),
            ('X', Val::Uint(x)) => format!("{x:X}"),
            ('X', Val::Str(s)) => s.bytes().map(|b| format!("{b:02X}")).collect(),
            (v, a) => return Err(format!("error during formatting: unsupported clause %{v} for {}", a.type_name())),
        });
    }
    Ok(out)
}

fn quantity_arg(v: Option<&Val>, name: &str) -> Result<Quantity, String> {
    match v {
        Some(Val::Quantity(q)) => Ok(q.clone()),
        Some(Val::Int(i)) => Quantity::parse(&i.to_string()).ok_or_else(|| "invalid quantity".to_string()),
        Some(other) => Err(no_overload(name, &[other])),
        None => Err(format!("{name}: missing argument")),
    }
}

fn call(target: Option<&Expr>, name: &str, args: &[Expr], env: &Env, scope: &mut Scope) -> Result<Val, String> {
    let recv = target.map(|t| eval(t, env, scope)).transpose()?;
    let args: Vec<Val> = args.iter().map(|a| eval(a, env, scope)).collect::<Result<_, _>>()?;
    if let Some(r) = &recv {
        let t = traversal(r);
        match name {
            "matches" | "find" | "findAll" => env.charge(t.max(1) * traversal(args.first().unwrap_or(&Val::Null)).max(1) * 5 / 2)?,
            "contains" | "lowerAscii" | "upperAscii" | "trim" | "replace" | "split" | "substring" | "indexOf" | "lastIndexOf" => env.charge(t)?,
            _ => {}
        }
    }
    call_values(recv, name, &args)
}

pub(super) fn call_values(recv: Option<Val>, name: &str, args: &[Val]) -> Result<Val, String> {
    use Val::*;
    let a0 = args.first();
    let a1 = args.get(1);
    match (recv, name, args.len()) {
        (None, "size", 1) => size(&args[0]),
        (Some(r), "size", 0) => size(&r),
        // Strings.
        (Some(Str(s)), "startsWith", 1) => Ok(Bool(s.starts_with(str_arg(a0, name)?))),
        (Some(Str(s)), "endsWith", 1) => Ok(Bool(s.ends_with(str_arg(a0, name)?))),
        (Some(Str(s)), "contains", 1) => Ok(Bool(s.contains(str_arg(a0, name)?))),
        (Some(Str(s)), "matches", 1) => Ok(Bool(regex(str_arg(a0, name)?)?.is_match(&s))),
        (None, "matches", 2) => Ok(Bool(regex(str_arg(a1, name)?)?.is_match(str_arg(a0, name)?))),
        (Some(Str(s)), "find", 1) => Ok(Str(regex(str_arg(a0, name)?)?.find(&s).map(|m| m.as_str().to_string()).unwrap_or_default())),
        (Some(Str(s)), "findAll", 1 | 2) => {
            let re = regex(str_arg(a0, name)?)?;
            let n = if args.len() == 2 { int_arg(a1, name)? } else { -1 };
            let all = re.find_iter(&s).map(|m| Str(m.as_str().to_string()));
            Ok(List(if n < 0 { all.collect() } else { all.take(n as usize).collect() }))
        }
        (Some(Str(s)), "lowerAscii", 0) => Ok(Str(s.to_ascii_lowercase())),
        (Some(Str(s)), "upperAscii", 0) => Ok(Str(s.to_ascii_uppercase())),
        (Some(Str(s)), "trim", 0) => Ok(Str(s.trim().to_string())),
        (Some(Str(s)), "charAt", 1) => {
            let cs = chars_of(&s);
            let i = int_arg(a0, name)?;
            match usize::try_from(i) {
                Ok(i) if i < cs.len() => Ok(Str(cs[i].to_string())),
                Ok(i) if i == cs.len() => Ok(Str(String::new())),
                _ => Err(format!("index out of range: {i}")),
            }
        }
        (Some(Str(s)), "indexOf" | "lastIndexOf", 1 | 2) => {
            let cs = chars_of(&s);
            let sub = chars_of(str_arg(a0, name)?);
            let last = name == "lastIndexOf";
            let bound = if args.len() == 2 { Some(int_arg(a1, name)?) } else { None };
            if let Some(b) = bound {
                if b < 0 || b as usize > cs.len() {
                    return Err(format!("index out of range: {b}"));
                }
            }
            if sub.is_empty() {
                return Ok(Int(bound.unwrap_or(if last { cs.len() as i64 } else { 0 })));
            }
            let fits = |i: usize| i + sub.len() <= cs.len() && cs[i..i + sub.len()] == sub[..];
            let found = if last {
                let hi = bound.map(|b| b as usize).unwrap_or(cs.len());
                (0..=hi.min(cs.len())).rev().find(|&i| fits(i))
            } else {
                (bound.unwrap_or(0) as usize..cs.len()).find(|&i| fits(i))
            };
            Ok(Int(found.map(|i| i as i64).unwrap_or(-1)))
        }
        (Some(Str(s)), "replace", 2 | 3) => {
            let from = str_arg(a0, name)?;
            let to = str_arg(a1, name)?;
            let n = if args.len() == 3 { int_arg(args.get(2), name)? } else { -1 };
            Ok(Str(if n < 0 { s.replace(from, to) } else { s.replacen(from, to, n as usize) }))
        }
        (Some(Str(s)), "split", 1 | 2) => {
            let n = if args.len() == 2 { int_arg(a1, name)? } else { -1 };
            Ok(List(split_n(&s, str_arg(a0, name)?, n)))
        }
        (Some(Str(s)), "substring", 1 | 2) => {
            let cs = chars_of(&s);
            let start = int_arg(a0, name)?;
            let end = if args.len() == 2 { int_arg(a1, name)? } else { cs.len() as i64 };
            if start < 0 || start as usize > cs.len() {
                return Err(format!("index out of range: {start}"));
            }
            if end < 0 || end as usize > cs.len() {
                return Err(format!("index out of range: {end}"));
            }
            if start > end {
                return Err(format!("invalid substring range. start: {start}, end: {end}"));
            }
            Ok(Str(cs[start as usize..end as usize].iter().collect()))
        }
        (Some(Str(s)), "format", 1) => match a0 {
            Some(List(l)) => Ok(Str(string_format(&s, l)?)),
            Some(other) => Err(no_overload(name, &[other])),
            None => unreachable!(),
        },
        (None, "strings.quote", 1) => Ok(Str(quote(str_arg(a0, name)?))),
        (Some(List(l)), "join", 0 | 1) => {
            let sep = if args.is_empty() { "" } else { str_arg(a0, name)? };
            let parts: Vec<String> = l.iter().map(|v| match v {
                Str(s) => Ok(s.clone()),
                other => Err(no_overload("join", &[other])),
            }).collect::<Result<_, _>>()?;
            Ok(Str(parts.join(sep)))
        }
        // Lists.
        (Some(List(l)), "isSorted", 0) => {
            for w in l.windows(2) {
                match compare(&w[0], &w[1]) {
                    Some(o) if o.is_gt() => return Ok(Bool(false)),
                    Some(_) => {}
                    None => return Err(no_overload("isSorted", &[&w[0], &w[1]])),
                }
            }
            Ok(Bool(true))
        }
        (Some(List(l)), "sum", 0) => {
            let mut acc: Option<Val> = None;
            for v in l {
                acc = Some(match acc {
                    None => v,
                    Some(a) => binary("+", a, v)?,
                });
            }
            Ok(acc.unwrap_or(Int(0)))
        }
        (Some(List(l)), "min" | "max", 0) => {
            let mut best: Option<Val> = None;
            for v in l {
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let o = compare(&v, &b).ok_or_else(|| no_overload(name, &[&v, &b]))?;
                        if (name == "min" && o.is_lt()) || (name == "max" && o.is_gt()) { v } else { b }
                    }
                });
            }
            best.ok_or_else(|| format!("{name}(list) argument must not be empty"))
        }
        (Some(List(l)), "indexOf", 1) => Ok(Int(l.iter().position(|v| equal(v, &args[0])).map(|i| i as i64).unwrap_or(-1))),
        (Some(List(l)), "lastIndexOf", 1) => Ok(Int(l.iter().rposition(|v| equal(v, &args[0])).map(|i| i as i64).unwrap_or(-1))),
        (Some(List(l)), "first", 0) => Ok(opt(l.first().cloned())),
        (Some(List(l)), "last", 0) => Ok(opt(l.last().cloned())),
        (None, "sets.contains" | "sets.equivalent" | "sets.intersects", 2) => match (a0, a1) {
            (Some(List(a)), Some(List(b))) => {
                let has = |l: &[Val], x: &Val| l.iter().any(|y| equal(x, y));
                Ok(Bool(match name {
                    "sets.contains" => b.iter().all(|x| has(a, x)),
                    "sets.equivalent" => b.iter().all(|x| has(a, x)) && a.iter().all(|x| has(b, x)),
                    _ => b.iter().any(|x| has(a, x)),
                }))
            }
            _ => Err(no_overload(name, &args.iter().collect::<Vec<_>>())),
        },
        // URLs.
        (None, "url", 1) => {
            let s = str_arg(a0, name)?;
            lib::Url::parse_request_uri(s).map(Val::Url).map_err(|e| format!("URL parse error during conversion from string: parse {s:?}: {e}"))
        }
        (None, "isURL", 1) => Ok(Bool(lib::Url::parse_request_uri(str_arg(a0, name)?).is_ok())),
        (Some(Url(u)), "getScheme", 0) => Ok(Str(u.scheme)),
        (Some(Url(u)), "getHost", 0) => Ok(Str(u.host)),
        (Some(Url(u)), "getHostname", 0) => Ok(Str(u.hostname())),
        (Some(Url(u)), "getPort", 0) => Ok(Str(u.port())),
        (Some(Url(u)), "getEscapedPath", 0) => Ok(Str(u.escaped_path())),
        (Some(Url(u)), "getQuery", 0) => Ok(Map(u.query().into_iter().map(|(k, v)| (k, List(v.into_iter().map(Str).collect()))).collect())),
        // Quantities.
        (None, "quantity", 1) => {
            let s = str_arg(a0, name)?;
            lib::Quantity::parse(s).map(Val::Quantity).ok_or_else(|| "quantities must match the regular expression '^([+-]?[0-9.]+)([eEinumkKMGTP]*[-+]?[0-9]*)$'".into())
        }
        (None, "isQuantity", 1) => Ok(Bool(lib::Quantity::parse(str_arg(a0, name)?).is_some())),
        (Some(Val::Quantity(q)), "isInteger", 0) => Ok(Bool(q.is_integer())),
        (Some(Val::Quantity(q)), "asInteger", 0) => q.as_integer().map(Int).ok_or_else(|| "cannot convert value to integer".into()),
        (Some(Val::Quantity(q)), "asApproximateFloat", 0) => Ok(Double(q.as_f64())),
        (Some(Val::Quantity(q)), "add" | "sub", 1) => {
            let other = quantity_arg(a0, name)?;
            q.add(&other, if name == "add" { 1 } else { -1 }).map(Val::Quantity).ok_or_else(|| "quantity overflow".into())
        }
        (Some(Val::Quantity(q)), "isGreaterThan", 1) => Ok(Bool(q.cmp(&quantity_arg(a0, name)?).is_gt())),
        (Some(Val::Quantity(q)), "isLessThan", 1) => Ok(Bool(q.cmp(&quantity_arg(a0, name)?).is_lt())),
        (Some(Val::Quantity(q)), "compareTo", 1) => Ok(Int(match q.cmp(&quantity_arg(a0, name)?) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })),
        // IPs and CIDRs.
        (None, "ip", 1) => lib::parse_ip(str_arg(a0, name)?).map(Ip),
        (None, "isIP", 1) => Ok(Bool(lib::parse_ip(str_arg(a0, name)?).is_ok())),
        (None, "ip.isCanonical", 1) => {
            let s = str_arg(a0, name)?;
            let ip = lib::parse_ip(s)?;
            Ok(Bool(ip.to_string() == s))
        }
        (Some(Ip(ip)), "family", 0) => Ok(Int(if ip.is_ipv4() { 4 } else { 6 })),
        (Some(Ip(ip)), "isUnspecified", 0) => Ok(Bool(ip.is_unspecified())),
        (Some(Ip(ip)), "isLoopback", 0) => Ok(Bool(ip.is_loopback())),
        (Some(Ip(ip)), "isLinkLocalMulticast", 0) => Ok(Bool(lib::is_link_local_multicast(ip))),
        (Some(Ip(ip)), "isLinkLocalUnicast", 0) => Ok(Bool(lib::is_link_local_unicast(ip))),
        (Some(Ip(ip)), "isGlobalUnicast", 0) => Ok(Bool(lib::is_global_unicast(ip))),
        (None, "cidr", 1) => lib::parse_cidr(str_arg(a0, name)?).map(|(ip, b)| Cidr(ip, b)),
        (None, "isCIDR", 1) => Ok(Bool(lib::parse_cidr(str_arg(a0, name)?).is_ok())),
        (Some(Cidr(net, bits)), "containsIP", 1) => {
            let ip = match a0 {
                Some(Ip(ip)) => *ip,
                Some(Str(s)) => lib::parse_ip(s)?,
                Some(other) => return Err(no_overload(name, &[other])),
                None => unreachable!(),
            };
            Ok(Bool(lib::cidr_contains(net, bits, ip)))
        }
        (Some(Cidr(net, bits)), "containsCIDR", 1) => {
            let (ip, b) = match a0 {
                Some(Cidr(ip, b)) => (*ip, *b),
                Some(Str(s)) => lib::parse_cidr(s)?,
                Some(other) => return Err(no_overload(name, &[other])),
                None => unreachable!(),
            };
            Ok(Bool(bits <= b && lib::cidr_contains(net, bits, ip)))
        }
        (Some(Cidr(net, _)), "ip", 0) => Ok(Ip(net)),
        (Some(Cidr(net, bits)), "masked", 0) => Ok(Cidr(lib::mask(net, bits), bits)),
        (Some(Cidr(_, bits)), "prefixLength", 0) => Ok(Int(bits as i64)),
        // Named formats.
        (None, "format.named", 1) => {
            let n = str_arg(a0, name)?;
            Ok(opt(lib::named_format(n, "").map(|_| Format(n.to_string()))))
        }
        (None, f, 0) if f.starts_with("format.") => Ok(Format(f["format.".len()..].to_string())),
        (Some(Format(f)), "validate", 1) => {
            let errs = lib::named_format(&f, str_arg(a0, name)?).unwrap_or_default();
            Ok(opt((!errs.is_empty()).then(|| List(errs.into_iter().map(Str).collect()))))
        }
        // Optionals.
        (None, "optional.of", 1) => Ok(opt(Some(args[0].clone()))),
        (None, "optional.none", 0) => Ok(Optional(None)),
        (None, "optional.ofNonZeroValue", 1) => {
            let zero = match &args[0] {
                Null => true,
                Bool(b) => !b,
                Int(i) => *i == 0,
                Uint(u) => *u == 0,
                Double(d) => *d == 0.0,
                Str(s) => s.is_empty(),
                Bytes(b) => b.is_empty(),
                List(l) => l.is_empty(),
                Map(m) => m.is_empty(),
                Duration(d) => *d == 0,
                _ => false,
            };
            Ok(opt((!zero).then(|| args[0].clone())))
        }
        (Some(Optional(o)), "hasValue", 0) => Ok(Bool(o.is_some())),
        (Some(Optional(o)), "value", 0) => o.map(|v| *v).ok_or_else(|| "optional.none() dereference".into()),
        (Some(Optional(o)), "orValue", 1) => Ok(o.map(|v| *v).unwrap_or_else(|| args[0].clone())),
        (Some(Optional(o)), "or", 1) => match o {
            Some(v) => Ok(Optional(Some(v))),
            None => match &args[0] {
                Optional(_) => Ok(args[0].clone()),
                other => Err(no_overload("or", &[other])),
            },
        },
        // Time.
        (None, "duration", 1) => match a0 {
            Some(Str(s)) => parse_go_duration(s).map(Duration),
            Some(Duration(d)) => Ok(Duration(*d)),
            Some(other) => Err(no_overload(name, &[other])),
            None => unreachable!(),
        },
        (None, "timestamp", 1) => match a0 {
            Some(Str(s)) => parse_timestamp(s).map(Timestamp),
            Some(Int(i)) => Utc.timestamp_opt(*i, 0).single().ok_or_else(|| "timestamp overflow".to_string()).and_then(check_ts_range),
            Some(Timestamp(t)) => Ok(Timestamp(*t)),
            Some(other) => Err(no_overload(name, &[other])),
            None => unreachable!(),
        },
        (Some(Duration(d)), "getHours", 0) => Ok(Int(d / 3_600_000_000_000)),
        (Some(Duration(d)), "getMinutes", 0) => Ok(Int(d / 60_000_000_000)),
        (Some(Duration(d)), "getSeconds", 0) => Ok(Int(d / 1_000_000_000)),
        (Some(Duration(d)), "getMilliseconds", 0) => Ok(Int(d / 1_000_000)),
        (Some(Timestamp(t)), f, 0 | 1) if f.starts_with("get") => ts_getter(&t, f, a0),
        // Conversions.
        (None, "string", 1) => to_string(&args[0]).map(Str),
        (None, "bytes", 1) => match a0 {
            Some(Str(s)) => Ok(Bytes(s.as_bytes().to_vec())),
            Some(Bytes(b)) => Ok(Bytes(b.clone())),
            Some(other) => Err(no_overload(name, &[other])),
            None => unreachable!(),
        },
        (None, "int", 1) => match a0 {
            Some(Int(i)) => Ok(Int(*i)),
            Some(Uint(u)) => i64::try_from(*u).map(Int).map_err(|_| "integer overflow".into()),
            Some(Double(d)) => {
                if d.is_nan() || *d >= 9.223372036854775807e18 || *d < -9.223372036854775808e18 {
                    Err("integer overflow".into())
                } else {
                    Ok(Int(*d as i64))
                }
            }
            Some(Str(s)) => s.parse().map(Int).map_err(|_| format!("strconv.ParseInt: parsing {s:?}: invalid syntax")),
            Some(Timestamp(t)) => Ok(Int(t.timestamp())),
            Some(Duration(d)) => Ok(Int(*d)),
            Some(other) => Err(no_overload(name, &[other])),
            None => unreachable!(),
        },
        (None, "uint", 1) => match a0 {
            Some(Uint(u)) => Ok(Uint(*u)),
            Some(Int(i)) => u64::try_from(*i).map(Uint).map_err(|_| "unsigned integer overflow".into()),
            Some(Double(d)) => {
                if d.is_nan() || *d < 0.0 || *d >= 1.8446744073709552e19 {
                    Err("unsigned integer overflow".into())
                } else {
                    Ok(Uint(*d as u64))
                }
            }
            Some(Str(s)) => s.parse().map(Uint).map_err(|_| format!("strconv.ParseUint: parsing {s:?}: invalid syntax")),
            Some(other) => Err(no_overload(name, &[other])),
            None => unreachable!(),
        },
        (None, "double", 1) => match a0 {
            Some(v) if num(v).is_some() => Ok(Double(num(v).unwrap())),
            Some(Str(s)) => s.parse().map(Double).map_err(|_| format!("strconv.ParseFloat: parsing {s:?}: invalid syntax")),
            Some(other) => Err(no_overload(name, &[other])),
            None => unreachable!(),
        },
        (None, "bool", 1) => match a0 {
            Some(Bool(b)) => Ok(Bool(*b)),
            Some(Str(s)) => match s.as_str() {
                "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(Bool(true)),
                "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(Bool(false)),
                _ => Err(format!("type conversion error from 'string' to 'bool'")),
            },
            Some(other) => Err(no_overload(name, &[other])),
            None => unreachable!(),
        },
        (None, "dyn", 1) => Ok(args[0].clone()),
        (None, "type", 1) => Ok(Type(args[0].type_name())),
        (r, _, _) => {
            let mut all: Vec<&Val> = Vec::new();
            if let Some(r) = &r {
                all.push(r);
            }
            all.extend(args.iter());
            Err(no_overload(name, &all))
        }
    }
}
