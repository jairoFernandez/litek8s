//! A PromQL parser for the subset litek8s evaluates: selectors with label
//! matchers, offsets and ranges, functions, aggregations with `by` /
//! `without`, arithmetic, comparisons (with `bool`), `and` / `or` /
//! `unless`, and vector matching with `on` / `ignoring` / `group_left` /
//! `group_right`. Subqueries and `@` are not supported.

use anyhow::{Result, bail};

use crate::obs::tsdb::{MatchOp, Matcher};

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Number(f64),
    Str(String),
    /// An instant vector selector.
    Selector(Selector),
    /// A range vector: a selector over `range` milliseconds.
    Matrix(Selector, i64),
    Call(String, Vec<Expr>),
    Aggregate { op: AggOp, expr: Box<Expr>, param: Option<Box<Expr>>, grouping: Grouping },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, return_bool: bool, matching: Matching },
    Neg(Box<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selector {
    pub name: Option<String>,
    pub matchers: Vec<Matcher>,
    pub offset: i64,
}

impl Selector {
    /// Every matcher, the name included.
    pub fn all_matchers(&self) -> Vec<Matcher> {
        let mut m = self.matchers.clone();
        if let Some(n) = &self.name {
            m.insert(0, Matcher::eq("__name__", n));
        }
        m
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggOp {
    Sum,
    Avg,
    Min,
    Max,
    Count,
    Group,
    Stddev,
    Stdvar,
    Topk,
    Bottomk,
    Quantile,
    CountValues,
}

impl AggOp {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sum" => AggOp::Sum,
            "avg" => AggOp::Avg,
            "min" => AggOp::Min,
            "max" => AggOp::Max,
            "count" => AggOp::Count,
            "group" => AggOp::Group,
            "stddev" => AggOp::Stddev,
            "stdvar" => AggOp::Stdvar,
            "topk" => AggOp::Topk,
            "bottomk" => AggOp::Bottomk,
            "quantile" => AggOp::Quantile,
            "count_values" => AggOp::CountValues,
            _ => return None,
        })
    }

    fn has_param(self) -> bool {
        matches!(self, AggOp::Topk | AggOp::Bottomk | AggOp::Quantile | AggOp::CountValues)
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum Grouping {
    #[default]
    None,
    By(Vec<String>),
    Without(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Atan2,
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    And,
    Or,
    Unless,
}

impl BinOp {
    pub fn is_comparison(self) -> bool {
        matches!(self, BinOp::Eq | BinOp::Ne | BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le)
    }

    pub fn is_set(self) -> bool {
        matches!(self, BinOp::And | BinOp::Or | BinOp::Unless)
    }

    fn precedence(self) -> u8 {
        match self {
            BinOp::Or => 1,
            BinOp::And | BinOp::Unless => 2,
            BinOp::Eq | BinOp::Ne | BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le => 3,
            BinOp::Add | BinOp::Sub => 4,
            BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Atan2 => 5,
            BinOp::Pow => 6,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Matching {
    /// `on(...)` (true) or `ignoring(...)` (false); `None`: neither given.
    pub on: Option<bool>,
    pub labels: Vec<String>,
    /// `group_left` (Left) / `group_right` (Right) with extra labels.
    pub group: Option<(Side, Vec<String>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Num(f64),
    Dur(i64),
    Str(String),
    P(&'static str),
    Eof,
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' || c == '\'' || c == '`' {
            let q = b[i];
            i += 1;
            let mut s = String::new();
            while i < b.len() && b[i] != q {
                if b[i] == b'\\' && q != b'`' && i + 1 < b.len() {
                    i += 1;
                    match b[i] {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'\\' => s.push('\\'),
                        b'"' => s.push('"'),
                        b'\'' => s.push('\''),
                        other => {
                            s.push('\\');
                            s.push(other as char);
                        }
                    }
                    i += 1;
                    continue;
                }
                let ch = src[i..].chars().next().unwrap();
                s.push(ch);
                i += ch.len_utf8();
            }
            if i >= b.len() {
                bail!("unterminated string");
            }
            i += 1;
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && i + 1 < b.len() && (b[i + 1] as char).is_ascii_digit()) {
            let start = i;
            if c == '0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                i += 2;
                while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                out.push(Tok::Num(i64::from_str_radix(&src[start + 2..i], 16)? as f64));
                continue;
            }
            while i < b.len() && ((b[i] as char).is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            // Duration: digits followed by units (5m, 1h30m, 500ms).
            if i < b.len() && matches!(b[i], b's' | b'm' | b'h' | b'd' | b'w' | b'y') && !src[start..i].contains('.') {
                let mut j = i;
                while j < b.len() && ((b[j] as char).is_ascii_alphanumeric()) {
                    j += 1;
                }
                let d = crate::obs::tsdb::parse_duration(&src[start..j])?;
                out.push(Tok::Dur(d.as_millis() as i64));
                i = j;
                continue;
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                if j < b.len() && (b[j] as char).is_ascii_digit() {
                    while j < b.len() && (b[j] as char).is_ascii_digit() {
                        j += 1;
                    }
                    i = j;
                }
            }
            out.push(Tok::Num(src[start..i].parse()?));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' || c == ':' {
            let start = i;
            while i < b.len() && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b':') {
                i += 1;
            }
            let word = &src[start..i];
            match word.to_ascii_lowercase().as_str() {
                "inf" => out.push(Tok::Num(f64::INFINITY)),
                "nan" => out.push(Tok::Num(f64::NAN)),
                _ => out.push(Tok::Ident(word.to_string())),
            }
            continue;
        }
        let two = src.get(i..i + 2).unwrap_or("");
        let p: &'static str = match two {
            "==" => "==",
            "!=" => "!=",
            "=~" => "=~",
            "!~" => "!~",
            ">=" => ">=",
            "<=" => "<=",
            _ => match c {
                '(' => "(",
                ')' => ")",
                '{' => "{",
                '}' => "}",
                '[' => "[",
                ']' => "]",
                ',' => ",",
                '=' => "=",
                '+' => "+",
                '-' => "-",
                '*' => "*",
                '/' => "/",
                '%' => "%",
                '^' => "^",
                '>' => ">",
                '<' => "<",
                '@' => "@",
                _ => bail!("unexpected character {c:?}"),
            },
        };
        i += p.len();
        out.push(Tok::P(p));
    }
    out.push(Tok::Eof);
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

pub fn parse(src: &str) -> Result<Expr> {
    let mut p = Parser { toks: lex(src)?, pos: 0 };
    let e = p.expr(0)?;
    if p.peek() != &Tok::Eof {
        bail!("unexpected {:?} after the expression", p.peek());
    }
    Ok(e)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, p: &str) -> bool {
        if matches!(self.peek(), Tok::P(x) if *x == p) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, p: &str) -> Result<()> {
        if !self.eat(p) {
            bail!("expected {p:?}, found {:?}", self.peek());
        }
        Ok(())
    }

    fn peek_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(w) if w.eq_ignore_ascii_case(kw))
    }

    fn binop(&self) -> Option<BinOp> {
        Some(match self.peek() {
            Tok::P("+") => BinOp::Add,
            Tok::P("-") => BinOp::Sub,
            Tok::P("*") => BinOp::Mul,
            Tok::P("/") => BinOp::Div,
            Tok::P("%") => BinOp::Mod,
            Tok::P("^") => BinOp::Pow,
            Tok::P("==") => BinOp::Eq,
            Tok::P("!=") => BinOp::Ne,
            Tok::P(">") => BinOp::Gt,
            Tok::P("<") => BinOp::Lt,
            Tok::P(">=") => BinOp::Ge,
            Tok::P("<=") => BinOp::Le,
            Tok::Ident(w) => match w.to_ascii_lowercase().as_str() {
                "and" => BinOp::And,
                "or" => BinOp::Or,
                "unless" => BinOp::Unless,
                "atan2" => BinOp::Atan2,
                _ => return None,
            },
            _ => return None,
        })
    }

    fn expr(&mut self, min_prec: u8) -> Result<Expr> {
        let mut lhs = self.unary()?;
        while let Some(op) = self.binop() {
            let prec = op.precedence();
            if prec < min_prec {
                break;
            }
            self.next();
            let return_bool = if self.peek_keyword("bool") {
                if !op.is_comparison() {
                    bail!("bool modifier on a non-comparison operator");
                }
                self.next();
                true
            } else {
                false
            };
            let matching = self.matching()?;
            // `^` is right associative.
            let next_min = if op == BinOp::Pow { prec } else { prec + 1 };
            let rhs = self.expr(next_min)?;
            lhs = Expr::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs), return_bool, matching };
        }
        Ok(lhs)
    }

    fn matching(&mut self) -> Result<Matching> {
        let mut m = Matching::default();
        if self.peek_keyword("on") || self.peek_keyword("ignoring") {
            let on = self.peek_keyword("on");
            self.next();
            m.on = Some(on);
            m.labels = self.label_list()?;
            if self.peek_keyword("group_left") || self.peek_keyword("group_right") {
                let side = if self.peek_keyword("group_left") { Side::Left } else { Side::Right };
                self.next();
                let extra = if matches!(self.peek(), Tok::P("(")) { self.label_list()? } else { Vec::new() };
                m.group = Some((side, extra));
            }
        }
        Ok(m)
    }

    fn label_list(&mut self) -> Result<Vec<String>> {
        self.expect("(")?;
        let mut out = Vec::new();
        loop {
            match self.next() {
                Tok::P(")") => break,
                Tok::Ident(w) => {
                    out.push(w);
                    if self.eat(",") {
                        continue;
                    }
                    self.expect(")")?;
                    break;
                }
                Tok::Str(s) => {
                    out.push(s);
                    if self.eat(",") {
                        continue;
                    }
                    self.expect(")")?;
                    break;
                }
                other => bail!("expected a label name, found {other:?}"),
            }
        }
        Ok(out)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat("-") {
            // Unary minus binds looser than ^: -2^2 is -4.
            let e = self.expr(BinOp::Pow.precedence())?;
            return Ok(match e {
                Expr::Number(n) => Expr::Number(-n),
                other => Expr::Neg(Box::new(other)),
            });
        }
        if self.eat("+") {
            return self.expr(BinOp::Pow.precedence());
        }
        let e = self.primary()?;
        self.postfix(e)
    }

    fn postfix(&mut self, mut e: Expr) -> Result<Expr> {
        loop {
            if matches!(self.peek(), Tok::P("[")) {
                self.next();
                let range = match self.next() {
                    Tok::Dur(d) => d,
                    Tok::Num(n) => (n * 1000.0) as i64,
                    other => bail!("expected a duration in [...], found {other:?}"),
                };
                if self.eat(":") || matches!(self.peek(), Tok::Ident(w) if w.starts_with(':')) {
                    bail!("subqueries are not supported");
                }
                self.expect("]")?;
                e = match e {
                    Expr::Selector(s) => Expr::Matrix(s, range),
                    _ => bail!("ranges are only allowed on vector selectors (subqueries are not supported)"),
                };
                continue;
            }
            if self.peek_keyword("offset") {
                self.next();
                let neg = self.eat("-");
                let d = match self.next() {
                    Tok::Dur(d) => d,
                    other => bail!("expected a duration after offset, found {other:?}"),
                };
                let d = if neg { -d } else { d };
                match &mut e {
                    Expr::Selector(s) | Expr::Matrix(s, _) => s.offset = d,
                    _ => bail!("offset applies to selectors only"),
                }
                continue;
            }
            if matches!(self.peek(), Tok::P("@")) {
                bail!("the @ modifier is not supported");
            }
            return Ok(e);
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.next() {
            Tok::Num(n) => Ok(Expr::Number(n)),
            Tok::Dur(d) => Ok(Expr::Number(d as f64 / 1000.0)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::P("(") => {
                let e = self.expr(0)?;
                self.expect(")")?;
                Ok(e)
            }
            Tok::P("{") => {
                let matchers = self.matchers()?;
                if matchers.iter().all(|m| m.matches("")) {
                    bail!("vector selector must contain at least one non-empty matcher");
                }
                Ok(Expr::Selector(Selector { name: None, matchers, offset: 0 }))
            }
            Tok::Ident(word) => {
                if let Some(op) = AggOp::parse(&word.to_ascii_lowercase()) {
                    if matches!(self.peek(), Tok::P("(")) || self.peek_keyword("by") || self.peek_keyword("without") {
                        return self.aggregate(op);
                    }
                }
                if matches!(self.peek(), Tok::P("(")) {
                    self.next();
                    let mut args = Vec::new();
                    if !self.eat(")") {
                        loop {
                            args.push(self.expr(0)?);
                            if self.eat(",") {
                                continue;
                            }
                            self.expect(")")?;
                            break;
                        }
                    }
                    return Ok(Expr::Call(word, args));
                }
                let matchers = if self.eat("{") { self.matchers()? } else { Vec::new() };
                Ok(Expr::Selector(Selector { name: Some(word), matchers, offset: 0 }))
            }
            other => bail!("unexpected {other:?}"),
        }
    }

    fn aggregate(&mut self, op: AggOp) -> Result<Expr> {
        let mut grouping = self.grouping()?;
        self.expect("(")?;
        let param = if op.has_param() {
            let p = self.expr(0)?;
            self.expect(",")?;
            Some(Box::new(p))
        } else {
            None
        };
        let expr = self.expr(0)?;
        self.expect(")")?;
        if grouping == Grouping::None {
            grouping = self.grouping()?;
        }
        Ok(Expr::Aggregate { op, expr: Box::new(expr), param, grouping })
    }

    fn grouping(&mut self) -> Result<Grouping> {
        if self.peek_keyword("by") {
            self.next();
            return Ok(Grouping::By(self.label_list()?));
        }
        if self.peek_keyword("without") {
            self.next();
            return Ok(Grouping::Without(self.label_list()?));
        }
        Ok(Grouping::None)
    }

    fn matchers(&mut self) -> Result<Vec<Matcher>> {
        let mut out = Vec::new();
        loop {
            let name = match self.next() {
                Tok::P("}") => break,
                Tok::Ident(w) => w,
                Tok::Str(s) => s,
                other => bail!("expected a label name, found {other:?}"),
            };
            let op = match self.next() {
                Tok::P("=") => MatchOp::Eq,
                Tok::P("!=") => MatchOp::Ne,
                Tok::P("=~") => MatchOp::Re,
                Tok::P("!~") => MatchOp::Nre,
                other => bail!("expected a match operator, found {other:?}"),
            };
            let value = match self.next() {
                Tok::Str(s) => s,
                other => bail!("expected a string, found {other:?}"),
            };
            out.push(Matcher::new(&name, op, &value)?);
            if self.eat(",") {
                continue;
            }
            self.expect("}")?;
            break;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_dashboard_queries() {
        for q in [
            r#"sum(rate(container_cpu_usage_seconds_total{namespace="default", container!=""}[5m])) by (pod)"#,
            r#"topk(5, sum by (pod) (container_memory_working_set_bytes{image!=""}))"#,
            r#"histogram_quantile(0.99, sum by (le, verb) (rate(apiserver_request_duration_seconds_bucket[5m])))"#,
            r#"kube_pod_container_status_waiting_reason{reason="CrashLoopBackOff"} == 1"#,
            r#"1 - node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes > bool 0.9"#,
            r#"sum(container_memory_working_set_bytes) by (namespace) * on(namespace) group_left(team) kube_namespace_labels"#,
            r#"increase(kube_pod_container_status_restarts_total[15m]) > 3 unless on(pod) kube_pod_info{namespace="kube-system"}"#,
            r#"rate(x[5m] offset 1h) + -2 ^ 2"#,
            r#"{__name__=~"up|process_.*"}"#,
            "predict_linear(node_filesystem_avail_bytes[6h], 4 * 3600) < 0",
            "1 + 1",
            "count_values(\"version\", build_info)",
        ] {
            parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        }
    }

    #[test]
    fn precedence_and_errors() {
        match parse("1 + 2 * 3").unwrap() {
            Expr::Binary { op: BinOp::Add, rhs, .. } => assert!(matches!(*rhs, Expr::Binary { op: BinOp::Mul, .. })),
            other => panic!("{other:?}"),
        }
        assert_eq!(parse("-2 ^ 2").unwrap(), Expr::Neg(Box::new(Expr::Binary { op: BinOp::Pow, lhs: Box::new(Expr::Number(2.0)), rhs: Box::new(Expr::Number(2.0)), return_bool: false, matching: Matching::default() })));
        match parse("a or b and c").unwrap() {
            Expr::Binary { op: BinOp::Or, .. } => {}
            other => panic!("{other:?}"),
        }
        assert!(parse("rate(x[5m:1m])").is_err());
        assert!(parse("{}").is_err());
        assert!(parse("sum(x").is_err());
        match parse("x[1h30m] offset 5m").unwrap() {
            Expr::Matrix(s, r) => assert_eq!((r, s.offset), (5_400_000, 300_000)),
            other => panic!("{other:?}"),
        }
        match parse("sum without (pod) (x)").unwrap() {
            Expr::Aggregate { grouping: Grouping::Without(l), .. } => assert_eq!(l, vec!["pod"]),
            other => panic!("{other:?}"),
        }
    }
}
