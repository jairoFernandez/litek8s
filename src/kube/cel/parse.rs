//! CEL lexer and parser. Node positions follow cel-go (a select is located
//! at its `.`, a call at its `(`, an operator at the operator), so errors
//! point where the real API server points.

use super::Val;

#[derive(Debug, Clone)]
pub struct Expr {
    pub id: u32,
    /// Character offset in the source.
    pub pos: usize,
    pub kind: Kind,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Lit(Val),
    Ident(String),
    /// `a.b`; `test` for `has(a.b)`, `optional` for `a.?b`.
    Select { operand: Box<Expr>, field: String, test: bool, optional: bool },
    /// `a[b]`; `optional` for `a[?b]`.
    Index { operand: Box<Expr>, index: Box<Expr>, optional: bool },
    /// A function call; qualified library functions (`strings.quote`,
    /// `sets.contains`, ...) are global calls with a dotted name.
    Call { target: Option<Box<Expr>>, func: String, args: Vec<Expr> },
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Comp(Box<Comp>),
}

/// A macro comprehension: `range.all(v1, p)`, `range.map(v1, p, f)`,
/// `range.transformList(v1, v2, f)`, `opt.optMap(v1, f)`,
/// ...
#[derive(Debug, Clone)]
pub struct Comp {
    pub name: &'static str,
    pub range: Expr,
    pub v1: String,
    pub v2: Option<String>,
    pub args: Vec<Expr>,
}

/// A parsed expression with its source.
#[derive(Debug, Clone)]
pub struct Program {
    pub src: String,
    pub root: Expr,
    pub(super) nodes: u32,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub pos: usize,
    pub msg: String,
}

impl Error {
    /// cel-go's error rendering: `ERROR: <input>:1:5: msg` plus the source
    /// line and a caret.
    pub fn render(&self, src: &str) -> String {
        render_errors(src, std::slice::from_ref(self))
    }
}

pub fn render_errors(src: &str, errs: &[Error]) -> String {
    let mut out = Vec::new();
    for e in errs {
        let mut line = 1;
        let mut col = 1;
        let mut line_start = 0;
        for (i, c) in src.chars().enumerate() {
            if i >= e.pos {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 1;
                line_start = i + 1;
            } else {
                col += 1;
            }
        }
        let text: String = src.chars().skip(line_start).take_while(|c| *c != '\n').collect();
        out.push(format!("ERROR: <input>:{line}:{col}: {}\n | {text}\n | {}^", e.msg, ".".repeat(col - 1)));
    }
    out.join("\n")
}

const RESERVED: &[&str] = &[
    "as", "break", "const", "continue", "else", "for", "function", "if", "import", "let", "loop", "package", "namespace",
    "return", "var", "void", "while",
];

/// Library functions called with a qualified name.
const QUALIFIED: &[&str] = &[
    "strings.quote",
    "sets.contains",
    "sets.equivalent",
    "sets.intersects",
    "ip.isCanonical",
    "format.named",
    "format.dns1123Label",
    "format.dns1123Subdomain",
    "format.dns1035Label",
    "format.qualifiedName",
    "format.dns1123LabelPrefix",
    "format.dns1123SubdomainPrefix",
    "format.dns1035LabelPrefix",
    "format.labelValue",
    "format.uri",
    "format.uuid",
    "format.byte",
    "format.date",
    "format.datetime",
    "optional.of",
    "optional.none",
    "optional.ofNonZeroValue",
];

// ---------------------------------------------------------------------------
// Lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Int(u64),
    Uint(u64),
    Double(f64),
    Str(String),
    Bytes(Vec<u8>),
    Ident(String),
    Op(&'static str),
    Eof,
}

impl Tok {
    fn shown(&self) -> String {
        match self {
            Tok::Int(i) => i.to_string(),
            Tok::Uint(u) => format!("{u}u"),
            Tok::Double(d) => d.to_string(),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Bytes(_) => "b''".into(),
            Tok::Ident(s) => s.clone(),
            Tok::Op(o) => o.to_string(),
            Tok::Eof => "<EOF>".into(),
        }
    }
}

const OPS: [&str; 26] = [
    "&&", "||", "==", "!=", "<=", ">=", ".?", "[?", "<", ">", "!", "+", "-", "*", "/", "%", "?", ":", ".", ",", "(", ")", "[", "]",
    "{", "}",
];

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, Error> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let hex = c == '0' && chars.get(i + 1).is_some_and(|c| *c == 'x' || *c == 'X');
            if hex {
                i += 2;
                while i < chars.len() && chars[i].is_ascii_hexdigit() {
                    i += 1;
                }
                let digits: String = chars[start + 2..i].iter().collect();
                let n = u64::from_str_radix(&digits, 16).map_err(|_| Error { pos: start, msg: "Syntax error: invalid int literal".into() })?;
                if i < chars.len() && (chars[i] == 'u' || chars[i] == 'U') {
                    i += 1;
                    out.push((Tok::Uint(n), start));
                } else {
                    out.push((Tok::Int(n), start));
                }
                continue;
            }
            let mut float = false;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i < chars.len() && chars[i] == '.' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                float = true;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                let mut j = i + 1;
                if chars.get(j).is_some_and(|c| *c == '-' || *c == '+') {
                    j += 1;
                }
                if chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
                    float = true;
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().collect();
            let bad = |what: &str| Error { pos: start, msg: format!("Syntax error: invalid {what} literal") };
            if float {
                out.push((Tok::Double(text.parse().map_err(|_| bad("double"))?), start));
            } else if i < chars.len() && (chars[i] == 'u' || chars[i] == 'U') {
                i += 1;
                out.push((Tok::Uint(text.parse().map_err(|_| bad("uint"))?), start));
            } else {
                out.push((Tok::Int(text.parse().map_err(|_| bad("int"))?), start));
            }
            continue;
        }
        // String and bytes literals, with r/b prefixes in either order.
        let mut j = i;
        let (mut raw, mut bytes) = (false, false);
        while j < chars.len() && j - i < 2 {
            match chars[j] {
                'r' | 'R' if !raw => raw = true,
                'b' | 'B' if !bytes => bytes = true,
                _ => break,
            }
            j += 1;
        }
        if j < chars.len() && (chars[j] == '\'' || chars[j] == '"') && (j == i || raw || bytes) {
            i = j;
            let q = chars[i];
            let triple = chars.get(i + 1) == Some(&q) && chars.get(i + 2) == Some(&q);
            i += if triple { 3 } else { 1 };
            let mut buf: Vec<u8> = Vec::new();
            let push_char = |buf: &mut Vec<u8>, ch: char| {
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            };
            loop {
                if i >= chars.len() {
                    return Err(Error { pos: start, msg: "Syntax error: token recognition error at: unterminated string".into() });
                }
                if triple {
                    if chars[i] == q && chars.get(i + 1) == Some(&q) && chars.get(i + 2) == Some(&q) {
                        i += 3;
                        break;
                    }
                } else if chars[i] == q {
                    i += 1;
                    break;
                } else if chars[i] == '\n' {
                    return Err(Error { pos: start, msg: "Syntax error: token recognition error at: newline in string".into() });
                }
                if chars[i] == '\\' && !raw {
                    let esc_pos = i;
                    i += 1;
                    let e = *chars.get(i).ok_or(Error { pos: esc_pos, msg: "Syntax error: invalid escape".into() })?;
                    i += 1;
                    let simple = match e {
                        'a' => Some('\x07'),
                        'b' => Some('\x08'),
                        'f' => Some('\x0c'),
                        'n' => Some('\n'),
                        'r' => Some('\r'),
                        't' => Some('\t'),
                        'v' => Some('\x0b'),
                        '\\' | '\'' | '"' | '`' | '?' => Some(e),
                        _ => None,
                    };
                    if let Some(ch) = simple {
                        push_char(&mut buf, ch);
                        continue;
                    }
                    let take = |i: &mut usize, n: usize, radix: u32| -> Option<u32> {
                        let s: String = chars.get(*i..*i + n)?.iter().collect();
                        *i += n;
                        u32::from_str_radix(&s, radix).ok()
                    };
                    let bad = Error { pos: esc_pos, msg: format!("Syntax error: invalid escape sequence \\{e}") };
                    match e {
                        'x' | 'X' => {
                            let v = take(&mut i, 2, 16).ok_or(bad)?;
                            if bytes {
                                buf.push(v as u8);
                            } else {
                                push_char(&mut buf, char::from_u32(v).unwrap_or('\u{fffd}'));
                            }
                        }
                        'u' => push_char(&mut buf, take(&mut i, 4, 16).and_then(char::from_u32).ok_or(bad)?),
                        'U' => push_char(&mut buf, take(&mut i, 8, 16).and_then(char::from_u32).ok_or(bad)?),
                        '0'..='3' => {
                            i -= 1;
                            let v = take(&mut i, 3, 8).ok_or(bad)?;
                            if bytes {
                                buf.push(v as u8);
                            } else {
                                push_char(&mut buf, char::from_u32(v).unwrap_or('\u{fffd}'));
                            }
                        }
                        _ => return Err(bad),
                    }
                    continue;
                }
                push_char(&mut buf, chars[i]);
                i += 1;
            }
            if bytes {
                out.push((Tok::Bytes(buf), start));
            } else {
                out.push((Tok::Str(String::from_utf8_lossy(&buf).into_owned()), start));
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push((Tok::Ident(chars[start..i].iter().collect()), start));
            continue;
        }
        let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
        match OPS.iter().find(|op| rest.starts_with(**op)) {
            // `.?` only before an identifier, `[?` only as the optional index.
            Some(&".?") if !chars.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic() || *c == '_' || c.is_whitespace()) => {
                out.push((Tok::Op("."), start));
                i += 1;
            }
            Some(op) => {
                out.push((Tok::Op(op), start));
                i += op.chars().count();
            }
            None => return Err(Error { pos: start, msg: format!("Syntax error: token recognition error at: '{c}'") }),
        }
    }
    out.push((Tok::Eof, chars.len()));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Parser

const EXPECTING_PRIMARY: &str =
    "{'[', '{', '(', '.', '-', '!', 'true', 'false', 'null', NUM_FLOAT, NUM_INT, NUM_UINT, STRING, BYTES, IDENTIFIER}";

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    next_id: u32,
    depth: usize,
}

impl Parser {
    fn tok(&self) -> &Tok {
        &self.toks[self.pos].0
    }

    fn at(&self) -> usize {
        self.toks[self.pos].1
    }

    fn node(&mut self, pos: usize, kind: Kind) -> Expr {
        self.next_id += 1;
        Expr { id: self.next_id, pos, kind }
    }

    fn peek_op(&self, op: &str) -> bool {
        matches!(self.tok(), Tok::Op(o) if *o == op)
    }

    fn eat(&mut self, op: &str) -> Option<usize> {
        if self.peek_op(op) {
            let at = self.at();
            self.pos += 1;
            Some(at)
        } else {
            None
        }
    }

    fn expect(&mut self, op: &str) -> Result<usize, Error> {
        self.eat(op).ok_or_else(|| Error {
            pos: self.at(),
            msg: format!("Syntax error: mismatched input '{}' expecting '{op}'", self.tok().shown()),
        })
    }

    fn expr(&mut self) -> Result<Expr, Error> {
        self.depth += 1;
        if self.depth > 250 {
            return Err(Error { pos: self.at(), msg: "expression recursion limit exceeded".into() });
        }
        let cond = self.or()?;
        let out = if let Some(at) = self.eat("?") {
            let a = self.or()?;
            self.expect(":")?;
            let b = self.expr()?;
            self.node(at, Kind::Cond(Box::new(cond), Box::new(a), Box::new(b)))
        } else {
            cond
        };
        self.depth -= 1;
        Ok(out)
    }

    fn or(&mut self) -> Result<Expr, Error> {
        let mut e = self.and()?;
        while let Some(at) = self.eat("||") {
            let r = self.and()?;
            e = self.node(at, Kind::Binary("||", Box::new(e), Box::new(r)));
        }
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr, Error> {
        let mut e = self.relation()?;
        while let Some(at) = self.eat("&&") {
            let r = self.relation()?;
            e = self.node(at, Kind::Binary("&&", Box::new(e), Box::new(r)));
        }
        Ok(e)
    }

    fn relation(&mut self) -> Result<Expr, Error> {
        let mut e = self.additive()?;
        loop {
            let op = ["==", "!=", "<=", ">=", "<", ">"].into_iter().find(|op| self.peek_op(op));
            let op = match op {
                Some(op) => op,
                None if matches!(self.tok(), Tok::Ident(w) if w == "in") => "in",
                None => return Ok(e),
            };
            let at = self.at();
            self.pos += 1;
            let r = self.additive()?;
            e = self.node(at, Kind::Binary(op, Box::new(e), Box::new(r)));
        }
    }

    fn additive(&mut self) -> Result<Expr, Error> {
        let mut e = self.multiplicative()?;
        loop {
            let Some(op) = ["+", "-"].into_iter().find(|op| self.peek_op(op)) else { return Ok(e) };
            let at = self.at();
            self.pos += 1;
            let r = self.multiplicative()?;
            e = self.node(at, Kind::Binary(op, Box::new(e), Box::new(r)));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, Error> {
        let mut e = self.unary()?;
        loop {
            let Some(op) = ["*", "/", "%"].into_iter().find(|op| self.peek_op(op)) else { return Ok(e) };
            let at = self.at();
            self.pos += 1;
            let r = self.unary()?;
            e = self.node(at, Kind::Binary(op, Box::new(e), Box::new(r)));
        }
    }

    fn unary(&mut self) -> Result<Expr, Error> {
        if let Some(at) = self.eat("!") {
            let inner = self.unary()?;
            return Ok(self.node(at, Kind::Unary("!", Box::new(inner))));
        }
        if let Some(at) = self.eat("-") {
            // Negative literals are constants (also i64::MIN).
            match self.tok().clone() {
                Tok::Int(n) if !self.continues_member(1) => {
                    self.pos += 1;
                    let v = if n == 1u64 << 63 {
                        i64::MIN
                    } else {
                        -(i64::try_from(n).map_err(|_| Error { pos: at, msg: "Syntax error: invalid int literal".into() })?)
                    };
                    return Ok(self.node(at, Kind::Lit(Val::Int(v))));
                }
                Tok::Double(d) if !self.continues_member(1) => {
                    self.pos += 1;
                    return Ok(self.node(at, Kind::Lit(Val::Double(-d))));
                }
                _ => {}
            }
            let inner = self.unary()?;
            return Ok(self.node(at, Kind::Unary("-", Box::new(inner))));
        }
        self.member()
    }

    /// Whether the token `ahead` positions away starts a member access.
    fn continues_member(&self, ahead: usize) -> bool {
        matches!(self.toks.get(self.pos + ahead), Some((Tok::Op(".") | Tok::Op(".?") | Tok::Op("[") | Tok::Op("[?"), _)))
    }

    fn args(&mut self, close: &str) -> Result<Vec<Expr>, Error> {
        let mut args = Vec::new();
        if self.eat(close).is_some() {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if self.eat(close).is_some() {
                return Ok(args);
            }
            self.expect(",")?;
        }
    }

    fn member(&mut self) -> Result<Expr, Error> {
        let mut e = self.primary()?;
        loop {
            if let Some(at) = self.eat(".").or_else(|| self.eat(".?")) {
                let optional = matches!(self.toks[self.pos - 1].0, Tok::Op(".?"));
                let Tok::Ident(name) = self.tok().clone() else {
                    return Err(Error {
                        pos: self.at(),
                        msg: format!("Syntax error: mismatched input '{}' expecting IDENTIFIER", self.tok().shown()),
                    });
                };
                self.pos += 1;
                if !optional && self.peek_op("(") {
                    let call_at = self.at();
                    self.pos += 1;
                    let args = self.args(")")?;
                    e = self.call(Some(e), name, args, call_at)?;
                } else {
                    e = self.node(at, Kind::Select { operand: Box::new(e), field: name, test: false, optional });
                }
            } else if let Some(at) = self.eat("[").or_else(|| self.eat("[?")) {
                let optional = matches!(self.toks[self.pos - 1].0, Tok::Op("[?"));
                let idx = self.expr()?;
                self.expect("]")?;
                e = self.node(at, Kind::Index { operand: Box::new(e), index: Box::new(idx), optional });
            } else {
                return Ok(e);
            }
        }
    }

    /// A call, expanding macros.
    fn call(&mut self, target: Option<Expr>, func: String, mut args: Vec<Expr>, at: usize) -> Result<Expr, Error> {
        let var = |e: &Expr| match &e.kind {
            Kind::Ident(n) => Ok(n.clone()),
            _ => Err(Error { pos: e.pos, msg: "argument must be a simple name".into() }),
        };
        // Qualified library functions: `strings.quote(x)` is a global call.
        if let Some(Expr { kind: Kind::Ident(ns), .. }) = &target {
            let q = format!("{ns}.{func}");
            if QUALIFIED.contains(&q.as_str()) {
                return Ok(self.node(at, Kind::Call { target: None, func: q, args }));
            }
        }
        match (&target, func.as_str(), args.len()) {
            (None, "has", 1) => {
                let arg = args.pop().unwrap();
                return match arg.kind {
                    Kind::Select { operand, field, optional: false, .. } => {
                        Ok(self.node(arg.pos, Kind::Select { operand, field, test: true, optional: false }))
                    }
                    _ => Err(Error { pos: at, msg: "invalid argument to has() macro".into() }),
                };
            }
            (Some(_), "all" | "exists" | "exists_one" | "existsOne", 2)
            | (Some(_), "map", 2 | 3)
            | (Some(_), "filter", 2)
            | (Some(_), "optMap" | "optFlatMap", 2) => {
                let v1 = var(&args[0])?;
                let name = macro_name(&func);
                let rest = args.split_off(1);
                let range = target.unwrap();
                return Ok(self.node(at, Kind::Comp(Box::new(Comp { name, range, v1, v2: None, args: rest }))));
            }
            (Some(_), "all" | "exists" | "exists_one" | "existsOne", 3)
            | (Some(_), "transformList" | "transformMap" | "transformMapEntry", 3 | 4) => {
                let v1 = var(&args[0])?;
                let v2 = var(&args[1])?;
                if v1 == v2 {
                    return Err(Error { pos: args[1].pos, msg: format!("duplicate variable name: {v1}") });
                }
                let name = macro_name(&func);
                let rest = args.split_off(2);
                let range = target.unwrap();
                return Ok(self.node(at, Kind::Comp(Box::new(Comp { name, range, v1, v2: Some(v2), args: rest }))));
            }
            _ => {}
        }
        Ok(self.node(at, Kind::Call { target: target.map(Box::new), func, args }))
    }

    fn primary(&mut self) -> Result<Expr, Error> {
        let at = self.at();
        let tok = self.tok().clone();
        let lit = |p: &mut Parser, v: Val| {
            p.pos += 1;
            Ok(p.node(at, Kind::Lit(v)))
        };
        match tok {
            Tok::Int(n) => {
                let v = i64::try_from(n).map_err(|_| Error { pos: at, msg: "Syntax error: invalid int literal".into() })?;
                lit(self, Val::Int(v))
            }
            Tok::Uint(u) => lit(self, Val::Uint(u)),
            Tok::Double(d) => lit(self, Val::Double(d)),
            Tok::Str(s) => lit(self, Val::Str(s)),
            Tok::Bytes(b) => lit(self, Val::Bytes(b)),
            Tok::Ident(id) => {
                self.pos += 1;
                match id.as_str() {
                    "true" => Ok(self.node(at, Kind::Lit(Val::Bool(true)))),
                    "false" => Ok(self.node(at, Kind::Lit(Val::Bool(false)))),
                    "null" => Ok(self.node(at, Kind::Lit(Val::Null))),
                    "in" => Err(Error { pos: at, msg: format!("Syntax error: mismatched input 'in' expecting {EXPECTING_PRIMARY}") }),
                    _ if RESERVED.contains(&id.as_str()) => Err(Error { pos: at, msg: format!("reserved identifier: {id}") }),
                    _ => {
                        if self.peek_op("(") {
                            let call_at = self.at();
                            self.pos += 1;
                            let args = self.args(")")?;
                            self.call(None, id, args, call_at)
                        } else {
                            Ok(self.node(at, Kind::Ident(id)))
                        }
                    }
                }
            }
            Tok::Op("(") => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect(")")?;
                Ok(e)
            }
            Tok::Op("[") => {
                self.pos += 1;
                let items = self.args("]")?;
                Ok(self.node(at, Kind::List(items)))
            }
            Tok::Op("{") => {
                self.pos += 1;
                let mut entries = Vec::new();
                if self.eat("}").is_none() {
                    loop {
                        let k = self.expr()?;
                        self.expect(":")?;
                        let v = self.expr()?;
                        entries.push((k, v));
                        if self.eat("}").is_some() {
                            break;
                        }
                        self.expect(",")?;
                        if self.eat("}").is_some() {
                            break;
                        }
                    }
                }
                Ok(self.node(at, Kind::Map(entries)))
            }
            Tok::Op(".") => {
                // A leading dot names the root container: `.self` is `self`.
                self.pos += 1;
                self.primary()
            }
            other => Err(Error { pos: at, msg: format!("Syntax error: mismatched input '{}' expecting {EXPECTING_PRIMARY}", other.shown()) }),
        }
    }
}

fn macro_name(f: &str) -> &'static str {
    match f {
        "all" => "all",
        "exists" => "exists",
        "exists_one" | "existsOne" => "exists_one",
        "map" => "map",
        "filter" => "filter",
        "optMap" => "optMap",
        "optFlatMap" => "optFlatMap",
        "transformList" => "transformList",
        "transformMap" => "transformMap",
        _ => "transformMapEntry",
    }
}

pub fn parse(src: &str) -> Result<Program, Error> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0, next_id: 0, depth: 0 };
    let root = p.expr()?;
    if p.tok() != &Tok::Eof {
        return Err(Error { pos: p.at(), msg: format!("Syntax error: extraneous input '{}' expecting <EOF>", p.tok().shown()) });
    }
    Ok(Program { src: src.to_string(), root, nodes: p.next_id })
}

impl Program {
    pub fn node_count(&self) -> u32 {
        self.nodes
    }
}
