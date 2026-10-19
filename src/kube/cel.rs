//! A small Common Expression Language (CEL) interpreter for
//! ValidatingAdmissionPolicy expressions over JSON objects.
//!
//! Covers the core language: literals (int, uint, double, string, bytes as
//! strings, bool, null, lists, maps), field selection and indexing, the
//! arithmetic, comparison, logical, `in` and ternary operators, `has()`, the
//! `all`/`exists`/`exists_one`/`map`/`filter` macros, and the common string
//! and list functions. No static type checking: type errors surface when an
//! expression is evaluated.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value as Json;

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Double(f64),
    Str(String),
    List(Vec<Val>),
    Map(BTreeMap<String, Val>),
}

impl Val {
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

    fn type_name(&self) -> &'static str {
        match self {
            Val::Null => "null_type",
            Val::Bool(_) => "bool",
            Val::Int(_) => "int",
            Val::Uint(_) => "uint",
            Val::Double(_) => "double",
            Val::Str(_) => "string",
            Val::List(_) => "list",
            Val::Map(_) => "map",
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
            Val::Double(d) => write!(f, "{d}"),
            Val::Str(s) => write!(f, "{s}"),
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
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Int(i64),
    Uint(u64),
    Double(f64),
    Str(String),
    Ident(String),
    Op(&'static str),
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    const OPS: [&str; 22] = [
        "&&", "||", "==", "!=", "<=", ">=", "<", ">", "!", "+", "-", "*", "/", "%", "?", ":", ".", ",", "(", ")", "[", "]",
    ];
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            let hex = c == '0' && chars.get(i + 1).is_some_and(|c| *c == 'x' || *c == 'X');
            if hex {
                i += 2;
                while i < chars.len() && chars[i].is_ascii_hexdigit() {
                    i += 1;
                }
                let n = i64::from_str_radix(&chars[start + 2..i].iter().collect::<String>(), 16).map_err(|e| e.to_string())?;
                out.push(Tok::Int(n));
                continue;
            }
            let mut float = false;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E') {
                if chars[i] == '.' {
                    // `1.size()` is not a float; a dot must be followed by a digit.
                    if !chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                        break;
                    }
                    float = true;
                }
                if chars[i] == 'e' || chars[i] == 'E' {
                    float = true;
                    if chars.get(i + 1).is_some_and(|c| *c == '-' || *c == '+') {
                        i += 1;
                    }
                }
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            if i < chars.len() && (chars[i] == 'u' || chars[i] == 'U') {
                i += 1;
                out.push(Tok::Uint(text.parse().map_err(|_| format!("bad uint {text}"))?));
            } else if float {
                out.push(Tok::Double(text.parse().map_err(|_| format!("bad double {text}"))?));
            } else {
                out.push(Tok::Int(text.parse().map_err(|_| format!("bad int {text}"))?));
            }
            continue;
        }
        let raw = (c == 'r' || c == 'R') && chars.get(i + 1).is_some_and(|q| *q == '\'' || *q == '"');
        if c == '\'' || c == '"' || raw {
            if raw {
                i += 1;
            }
            let q = chars[i];
            let triple = chars.get(i + 1) == Some(&q) && chars.get(i + 2) == Some(&q);
            i += if triple { 3 } else { 1 };
            let mut s = String::new();
            loop {
                if i >= chars.len() {
                    return Err("unterminated string".into());
                }
                if triple {
                    if chars[i] == q && chars.get(i + 1) == Some(&q) && chars.get(i + 2) == Some(&q) {
                        i += 3;
                        break;
                    }
                } else if chars[i] == q {
                    i += 1;
                    break;
                }
                if chars[i] == '\\' && !raw {
                    i += 1;
                    let e = *chars.get(i).ok_or("bad escape")?;
                    s.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        '`' => '`',
                        '?' => '?',
                        other => return Err(format!("unsupported escape \\{other}")),
                    });
                    i += 1;
                    continue;
                }
                s.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
            continue;
        }
        if c == '{' || c == '}' {
            out.push(Tok::Op(if c == '{' { "{" } else { "}" }));
            i += 1;
            continue;
        }
        let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
        match OPS.iter().find(|op| rest.starts_with(**op)) {
            Some(op) => {
                out.push(Tok::Op(op));
                i += op.len();
            }
            None => return Err(format!("unexpected character '{c}'")),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Parser

#[derive(Debug, Clone)]
enum Expr {
    Lit(Val),
    Ident(String),
    Select(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Call(Option<Box<Expr>>, String, Vec<Expr>),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek_op(&self, op: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(Tok::Op(o)) if *o == op)
    }

    fn eat(&mut self, op: &str) -> bool {
        if self.peek_op(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, op: &str) -> Result<(), String> {
        if self.eat(op) { Ok(()) } else { Err(format!("expected '{op}'")) }
    }

    fn expr(&mut self) -> Result<Expr, String> {
        let cond = self.or()?;
        if self.eat("?") {
            let a = self.expr()?;
            self.expect(":")?;
            let b = self.expr()?;
            return Ok(Expr::Cond(Box::new(cond), Box::new(a), Box::new(b)));
        }
        Ok(cond)
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut e = self.and()?;
        while self.eat("||") {
            e = Expr::Binary("||", Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut e = self.relation()?;
        while self.eat("&&") {
            e = Expr::Binary("&&", Box::new(e), Box::new(self.relation()?));
        }
        Ok(e)
    }

    fn relation(&mut self) -> Result<Expr, String> {
        let mut e = self.additive()?;
        loop {
            let op = ["==", "!=", "<=", ">=", "<", ">"].into_iter().find(|op| self.peek_op(op));
            if let Some(op) = op {
                self.pos += 1;
                e = Expr::Binary(op, Box::new(e), Box::new(self.additive()?));
            } else if matches!(self.toks.get(self.pos), Some(Tok::Ident(w)) if w == "in") {
                self.pos += 1;
                e = Expr::Binary("in", Box::new(e), Box::new(self.additive()?));
            } else {
                return Ok(e);
            }
        }
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut e = self.multiplicative()?;
        loop {
            let op = ["+", "-"].into_iter().find(|op| self.peek_op(op));
            let Some(op) = op else { return Ok(e) };
            self.pos += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, String> {
        let mut e = self.unary()?;
        loop {
            let op = ["*", "/", "%"].into_iter().find(|op| self.peek_op(op));
            let Some(op) = op else { return Ok(e) };
            self.pos += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat("!") {
            return Ok(Expr::Unary("!", Box::new(self.unary()?)));
        }
        if self.eat("-") {
            return Ok(Expr::Unary("-", Box::new(self.unary()?)));
        }
        self.member()
    }

    fn args(&mut self, close: &str) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        if self.eat(close) {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if self.eat(close) {
                return Ok(args);
            }
            self.expect(",")?;
            if self.eat(close) {
                return Ok(args);
            }
        }
    }

    fn member(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            if self.eat(".") {
                let Some(Tok::Ident(name)) = self.toks.get(self.pos).cloned() else {
                    return Err("expected a field name after '.'".into());
                };
                self.pos += 1;
                if self.eat("(") {
                    let args = self.args(")")?;
                    e = Expr::Call(Some(Box::new(e)), name, args);
                } else {
                    e = Expr::Select(Box::new(e), name);
                }
            } else if self.eat("[") {
                let idx = self.expr()?;
                self.expect("]")?;
                e = Expr::Index(Box::new(e), Box::new(idx));
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let tok = self.toks.get(self.pos).cloned().ok_or("unexpected end of expression")?;
        self.pos += 1;
        Ok(match tok {
            Tok::Int(i) => Expr::Lit(Val::Int(i)),
            Tok::Uint(u) => Expr::Lit(Val::Uint(u)),
            Tok::Double(d) => Expr::Lit(Val::Double(d)),
            Tok::Str(s) => Expr::Lit(Val::Str(s)),
            Tok::Ident(id) => match id.as_str() {
                "true" => Expr::Lit(Val::Bool(true)),
                "false" => Expr::Lit(Val::Bool(false)),
                "null" => Expr::Lit(Val::Null),
                _ if self.eat("(") => Expr::Call(None, id, self.args(")")?),
                _ => Expr::Ident(id),
            },
            Tok::Op("(") => {
                let e = self.expr()?;
                self.expect(")")?;
                e
            }
            Tok::Op("[") => Expr::List(self.args("]")?),
            Tok::Op("{") => {
                let mut entries = Vec::new();
                if !self.eat("}") {
                    loop {
                        let k = self.expr()?;
                        self.expect(":")?;
                        let v = self.expr()?;
                        entries.push((k, v));
                        if self.eat("}") {
                            break;
                        }
                        self.expect(",")?;
                        if self.eat("}") {
                            break;
                        }
                    }
                }
                Expr::Map(entries)
            }
            other => return Err(format!("unexpected token {other:?}")),
        })
    }
}

/// A parsed expression, ready to evaluate many times.
#[derive(Debug, Clone)]
pub struct Program(Expr);

pub fn compile(src: &str) -> Result<Program, String> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    let e = p.expr()?;
    if p.pos != p.toks.len() {
        return Err(format!("unexpected trailing input in {src:?}"));
    }
    Ok(Program(e))
}

// ---------------------------------------------------------------------------
// Evaluation

/// Variables visible to an expression. `lazy` entries (VAP `variables`) are
/// compiled programs evaluated on first use.
#[derive(Default)]
pub struct Env {
    pub vars: BTreeMap<String, Val>,
}

impl Program {
    pub fn eval(&self, env: &Env) -> Result<Val, String> {
        eval(&self.0, env, &mut Vec::new())
    }
}

type Scope = Vec<(String, Val)>;

fn lookup(env: &Env, scope: &Scope, name: &str) -> Result<Val, String> {
    if let Some((_, v)) = scope.iter().rev().find(|(n, _)| n == name) {
        return Ok(v.clone());
    }
    env.vars.get(name).cloned().ok_or_else(|| format!("undeclared reference to '{name}'"))
}

fn truth(v: Val) -> Result<bool, String> {
    match v {
        Val::Bool(b) => Ok(b),
        other => Err(format!("no such overload: expected bool, got {}", other.type_name())),
    }
}

fn select(base: Val, field: &str) -> Result<Val, String> {
    match base {
        Val::Map(m) => m.get(field).cloned().ok_or_else(|| format!("no such key: {field}")),
        Val::Null => Err(format!("no such key: {field}")),
        other => Err(format!("type '{}' does not support field selection", other.type_name())),
    }
}

fn eval(e: &Expr, env: &Env, scope: &mut Scope) -> Result<Val, String> {
    match e {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Ident(name) => lookup(env, scope, name),
        Expr::Select(base, field) => select(eval(base, env, scope)?, field),
        Expr::Index(base, idx) => {
            let base = eval(base, env, scope)?;
            let idx = eval(idx, env, scope)?;
            match (base, idx) {
                (Val::List(l), Val::Int(i)) => l.get(usize::try_from(i).map_err(|_| "index out of range")?).cloned().ok_or_else(|| format!("index out of range: {i}")),
                (Val::List(l), Val::Uint(i)) => l.get(i as usize).cloned().ok_or_else(|| format!("index out of range: {i}")),
                (Val::Map(m), Val::Str(k)) => m.get(&k).cloned().ok_or_else(|| format!("no such key: {k}")),
                (b, i) => Err(format!("found no matching overload for '_[_]' applied to '({}, {})'", b.type_name(), i.type_name())),
            }
        }
        Expr::List(items) => Ok(Val::List(items.iter().map(|i| eval(i, env, scope)).collect::<Result<_, _>>()?)),
        Expr::Map(entries) => {
            let mut m = BTreeMap::new();
            for (k, v) in entries {
                let key = match eval(k, env, scope)? {
                    Val::Str(s) => s,
                    other => other.to_string(),
                };
                m.insert(key, eval(v, env, scope)?);
            }
            Ok(Val::Map(m))
        }
        Expr::Unary("!", inner) => Ok(Val::Bool(!truth(eval(inner, env, scope)?)?)),
        Expr::Unary(_, inner) => match eval(inner, env, scope)? {
            Val::Int(i) => i.checked_neg().map(Val::Int).ok_or_else(|| "integer overflow".into()),
            Val::Double(d) => Ok(Val::Double(-d)),
            other => Err(format!("found no matching overload for '-_' applied to '({})'", other.type_name())),
        },
        Expr::Cond(c, a, b) => {
            if truth(eval(c, env, scope)?)? {
                eval(a, env, scope)
            } else {
                eval(b, env, scope)
            }
        }
        Expr::Binary("&&", a, b) => {
            // CEL's commutative logic: false wins over errors on either side.
            let left = eval(a, env, scope).and_then(truth);
            if left == Ok(false) {
                return Ok(Val::Bool(false));
            }
            let right = eval(b, env, scope).and_then(truth);
            match (left, right) {
                (_, Ok(false)) => Ok(Val::Bool(false)),
                (Err(e), _) | (_, Err(e)) => Err(e),
                _ => Ok(Val::Bool(true)),
            }
        }
        Expr::Binary("||", a, b) => {
            let left = eval(a, env, scope).and_then(truth);
            if left == Ok(true) {
                return Ok(Val::Bool(true));
            }
            let right = eval(b, env, scope).and_then(truth);
            match (left, right) {
                (_, Ok(true)) => Ok(Val::Bool(true)),
                (Err(e), _) | (_, Err(e)) => Err(e),
                _ => Ok(Val::Bool(false)),
            }
        }
        Expr::Binary(op, a, b) => binary(op, eval(a, env, scope)?, eval(b, env, scope)?),
        Expr::Call(target, name, args) => call(target.as_deref(), name, args, env, scope),
    }
}

fn no_overload(op: &str, a: &Val, b: &Val) -> String {
    format!("found no matching overload for '_{op}_' applied to '({}, {})'", a.type_name(), b.type_name())
}

fn num(v: &Val) -> Option<f64> {
    match v {
        Val::Int(i) => Some(*i as f64),
        Val::Uint(u) => Some(*u as f64),
        Val::Double(d) => Some(*d),
        _ => None,
    }
}

fn binary(op: &str, a: Val, b: Val) -> Result<Val, String> {
    use Val::*;
    match op {
        "==" => Ok(Bool(equal(&a, &b))),
        "!=" => Ok(Bool(!equal(&a, &b))),
        "<" | "<=" | ">" | ">=" => {
            let ord = match (&a, &b) {
                (Str(x), Str(y)) => x.partial_cmp(y),
                (Bool(x), Bool(y)) => x.partial_cmp(y),
                (x, y) if num(x).is_some() && num(y).is_some() => num(x).unwrap().partial_cmp(&num(y).unwrap()),
                _ => return Err(no_overload(op, &a, &b)),
            }
            .ok_or_else(|| no_overload(op, &a, &b))?;
            Ok(Bool(match op {
                "<" => ord.is_lt(),
                "<=" => ord.is_le(),
                ">" => ord.is_gt(),
                _ => ord.is_ge(),
            }))
        }
        "in" => match &b {
            List(l) => Ok(Bool(l.iter().any(|x| equal(x, &a)))),
            Map(m) => match &a {
                Str(k) => Ok(Bool(m.contains_key(k))),
                _ => Ok(Bool(false)),
            },
            _ => Err(no_overload("in", &a, &b)),
        },
        "+" => match (a, b) {
            (Int(x), Int(y)) => x.checked_add(y).map(Int).ok_or_else(|| "integer overflow".into()),
            (Uint(x), Uint(y)) => x.checked_add(y).map(Uint).ok_or_else(|| "integer overflow".into()),
            (Double(x), Double(y)) => Ok(Double(x + y)),
            (Str(x), Str(y)) => Ok(Str(x + &y)),
            (List(mut x), List(y)) => {
                x.extend(y);
                Ok(List(x))
            }
            (a, b) => Err(no_overload("+", &a, &b)),
        },
        "-" | "*" | "/" | "%" => match (a, b) {
            (Int(x), Int(y)) => match op {
                "-" => x.checked_sub(y),
                "*" => x.checked_mul(y),
                "/" if y == 0 => return Err("division by zero".into()),
                "/" => x.checked_div(y),
                _ if y == 0 => return Err("modulus by zero".into()),
                _ => x.checked_rem(y),
            }
            .map(Int)
            .ok_or_else(|| "integer overflow".into()),
            (Uint(x), Uint(y)) => match op {
                "-" => x.checked_sub(y),
                "*" => x.checked_mul(y),
                "/" => x.checked_div(y),
                _ => x.checked_rem(y),
            }
            .map(Uint)
            .ok_or_else(|| "integer overflow or division by zero".into()),
            (Double(x), Double(y)) if op != "%" => Ok(Double(match op {
                "-" => x - y,
                "*" => x * y,
                _ => x / y,
            })),
            (a, b) => Err(no_overload(op, &a, &b)),
        },
        _ => Err(format!("unsupported operator {op}")),
    }
}

fn equal(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::List(x), Val::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| equal(p, q)),
        (Val::Map(x), Val::Map(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| equal(v, w))),
        (x, y) if num(x).is_some() && num(y).is_some() => num(x) == num(y),
        (x, y) => x == y,
    }
}

fn size(v: &Val) -> Result<Val, String> {
    Ok(Val::Int(match v {
        Val::Str(s) => s.chars().count() as i64,
        Val::List(l) => l.len() as i64,
        Val::Map(m) => m.len() as i64,
        other => return Err(format!("found no matching overload for 'size' applied to '({})'", other.type_name())),
    }))
}

fn call(target: Option<&Expr>, name: &str, args: &[Expr], env: &Env, scope: &mut Scope) -> Result<Val, String> {
    // Macros take unevaluated arguments.
    match (target, name, args) {
        (None, "has", [Expr::Select(base, field)]) => {
            return Ok(Val::Bool(match eval(base, env, scope)? {
                Val::Map(m) => m.get(field).is_some_and(|v| *v != Val::Null),
                _ => false,
            }));
        }
        (Some(t), "all" | "exists" | "exists_one" | "map" | "filter", [Expr::Ident(var), rest @ ..]) if !rest.is_empty() => {
            let items: Vec<Val> = match eval(t, env, scope)? {
                Val::List(l) => l,
                Val::Map(m) => m.into_keys().map(Val::Str).collect(),
                other => return Err(format!("{name}() is not supported on {}", other.type_name())),
            };
            let mut results = Vec::new();
            let mut count = 0;
            for item in items {
                scope.push((var.clone(), item.clone()));
                let r = (|| -> Result<(), String> {
                    match (name, rest) {
                        ("map", [f]) => results.push(eval(f, env, scope)?),
                        ("map", [p, f]) => {
                            if truth(eval(p, env, scope)?)? {
                                results.push(eval(f, env, scope)?);
                            }
                        }
                        ("filter", [p]) => {
                            if truth(eval(p, env, scope)?)? {
                                results.push(item.clone());
                            }
                        }
                        (_, [p]) => {
                            if truth(eval(p, env, scope)?)? {
                                count += 1;
                            } else if name == "all" {
                                results.push(Val::Bool(false));
                            }
                        }
                        _ => return Err(format!("wrong number of arguments to {name}()")),
                    }
                    Ok(())
                })();
                scope.pop();
                r?;
            }
            return Ok(match name {
                "all" => Val::Bool(results.is_empty()),
                "exists" => Val::Bool(count > 0),
                "exists_one" => Val::Bool(count == 1),
                _ => Val::List(results),
            });
        }
        _ => {}
    }
    let recv = target.map(|t| eval(t, env, scope)).transpose()?;
    let args: Vec<Val> = args.iter().map(|a| eval(a, env, scope)).collect::<Result<_, _>>()?;
    let str_arg = |i: usize| match args.get(i) {
        Some(Val::Str(s)) => Ok(s.clone()),
        _ => Err(format!("{name}() expects a string argument")),
    };
    match (recv, name) {
        (None, "size") => size(args.first().ok_or("size() needs an argument")?),
        (Some(r), "size") => size(&r),
        (Some(Val::Str(s)), "startsWith") => Ok(Val::Bool(s.starts_with(&str_arg(0)?))),
        (Some(Val::Str(s)), "endsWith") => Ok(Val::Bool(s.ends_with(&str_arg(0)?))),
        (Some(Val::Str(s)), "contains") => Ok(Val::Bool(s.contains(&str_arg(0)?))),
        (Some(Val::Str(s)), "lowerAscii") => Ok(Val::Str(s.to_ascii_lowercase())),
        (Some(Val::Str(s)), "upperAscii") => Ok(Val::Str(s.to_ascii_uppercase())),
        (Some(Val::Str(s)), "trim") => Ok(Val::Str(s.trim().to_string())),
        (Some(Val::Str(s)), "split") => Ok(Val::List(s.split(str_arg(0)?.as_str()).map(|p| Val::Str(p.into())).collect())),
        (Some(Val::List(l)), "join") => {
            let sep = if args.is_empty() { String::new() } else { str_arg(0)? };
            Ok(Val::Str(l.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(&sep)))
        }
        (None, "string") => Ok(Val::Str(args.first().ok_or("string() needs an argument")?.to_string())),
        (None, "int") => match args.first() {
            Some(Val::Int(i)) => Ok(Val::Int(*i)),
            Some(Val::Uint(u)) => i64::try_from(*u).map(Val::Int).map_err(|_| "int overflow".into()),
            Some(Val::Double(d)) => Ok(Val::Int(*d as i64)),
            Some(Val::Str(s)) => s.parse().map(Val::Int).map_err(|_| format!("cannot convert {s:?} to int")),
            _ => Err("int() needs a number or string".into()),
        },
        (None, "double") => match args.first().and_then(num) {
            Some(d) => Ok(Val::Double(d)),
            None => match args.first() {
                Some(Val::Str(s)) => s.parse().map(Val::Double).map_err(|_| format!("cannot convert {s:?} to double")),
                _ => Err("double() needs a number or string".into()),
            },
        },
        (None, "bool") => match args.first() {
            Some(Val::Bool(b)) => Ok(Val::Bool(*b)),
            Some(Val::Str(s)) => s.parse().map(Val::Bool).map_err(|_| format!("cannot convert {s:?} to bool")),
            _ => Err("bool() needs a bool or string".into()),
        },
        (None, "type") => Ok(Val::Str(args.first().map(Val::type_name).unwrap_or("null_type").to_string())),
        (r, _) => Err(format!(
            "found no matching overload for '{name}'{}",
            r.map(|r| format!(" on '{}'", r.type_name())).unwrap_or_default()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(src: &str, object: Json) -> Result<Val, String> {
        let mut env = Env::default();
        env.vars.insert("object".into(), Val::from_json(&object));
        compile(src)?.eval(&env)
    }

    #[test]
    fn expressions() {
        let d = json!({"metadata": {"name": "web", "labels": {"app": "x"}}, "spec": {"replicas": 3, "tags": ["a", "b"]}});
        assert_eq!(run("object.spec.replicas > 1", d.clone()), Ok(Val::Bool(true)));
        assert_eq!(run("object.spec.replicas % 2 == 1 && object.metadata.name == 'web'", d.clone()), Ok(Val::Bool(true)));
        assert_eq!(run("'got ' + string(object.spec.replicas)", d.clone()), Ok(Val::Str("got 3".into())));
        assert!(run("'got ' + object.spec.replicas", d.clone()).unwrap_err().contains("'(string, int)'"));
        assert_eq!(run("has(object.metadata.labels) && 'app' in object.metadata.labels", d.clone()), Ok(Val::Bool(true)));
        assert_eq!(run("!has(object.spec.missing)", d.clone()), Ok(Val::Bool(true)));
        assert_eq!(run("object.spec.tags.all(t, t.size() == 1)", d.clone()), Ok(Val::Bool(true)));
        assert_eq!(run("object.spec.tags.exists(t, t == 'b') ? 1 : 2", d.clone()), Ok(Val::Int(1)));
        assert_eq!(run("size(object.spec.tags.map(t, t + t)) == 2", d.clone()), Ok(Val::Bool(true)));
        assert_eq!(run("[1, 2, 3].filter(x, x > 1)", d.clone()), Ok(Val::List(vec![Val::Int(2), Val::Int(3)])));
        assert!(run("object.spec.missing < 10", d.clone()).is_err());
        assert_eq!(run("object.spec.missing < 10 || true", d), Ok(Val::Bool(true)));
    }
}
