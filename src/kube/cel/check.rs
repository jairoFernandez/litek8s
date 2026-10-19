//! Static type checking of CEL expressions against declared variables, as
//! the API server does for CRD validation rules: undefined fields,
//! function overloads, homogeneous literals, regex/duration/timestamp
//! literals and the result type. Error messages and positions follow
//! cel-go.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use serde_json::Value as Json;

use super::parse::{Comp, Error, Expr, Kind, Program};
use super::Val;

/// A CEL type.
#[derive(Debug, Clone)]
pub enum Ty {
    Dyn,
    /// The type of an expression that already failed to check; matches
    /// anything so errors do not cascade.
    Error,
    Null,
    Bool,
    Int,
    Uint,
    Double,
    Str,
    Bytes,
    Duration,
    Timestamp,
    List(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Obj(Rc<Decl>),
    Opt(Box<Ty>),
    Type(Box<Ty>),
    Quantity,
    Url,
    Ip,
    Cidr,
    Format,
}

impl Ty {
    pub fn name(&self) -> String {
        match self {
            Ty::Dyn | Ty::Error => "dyn".into(),
            Ty::Null => "null".into(),
            Ty::Bool => "bool".into(),
            Ty::Int => "int".into(),
            Ty::Uint => "uint".into(),
            Ty::Double => "double".into(),
            Ty::Str => "string".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Duration => "google.protobuf.Duration".into(),
            Ty::Timestamp => "google.protobuf.Timestamp".into(),
            Ty::List(t) => format!("list({})", t.name()),
            Ty::Map(k, v) => format!("map({}, {})", k.name(), v.name()),
            Ty::Obj(d) => d.name.clone(),
            Ty::Opt(t) => format!("optional_type({})", t.name()),
            Ty::Type(t) => format!("type({})", t.name()),
            Ty::Quantity => "kubernetes.Quantity".into(),
            Ty::Url => "kubernetes.URL".into(),
            Ty::Ip => "net.IP".into(),
            Ty::Cidr => "net.CIDR".into(),
            Ty::Format => "kubernetes.NamedFormat".into(),
        }
    }

    fn is_dyn(&self) -> bool {
        matches!(self, Ty::Dyn | Ty::Error)
    }

    fn same_kind(&self, other: &Ty) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// Whether a value of type `a` may be used where `b` is expected (and the
/// other way round): equal types, `dyn`, element-wise for containers, and
/// `null` for objects and optionals.
pub fn assignable(a: &Ty, b: &Ty) -> bool {
    match (a, b) {
        (x, y) if x.is_dyn() || y.is_dyn() => true,
        (Ty::List(x), Ty::List(y)) => assignable(x, y),
        (Ty::Map(k1, v1), Ty::Map(k2, v2)) => assignable(k1, k2) && assignable(v1, v2),
        (Ty::Opt(x), Ty::Opt(y)) => assignable(x, y),
        (Ty::Type(_), Ty::Type(_)) => true,
        (Ty::Obj(x), Ty::Obj(y)) => Rc::ptr_eq(x, y) || x.name == y.name,
        (Ty::Null, Ty::Obj(_) | Ty::Opt(_) | Ty::Null) | (Ty::Obj(_) | Ty::Opt(_), Ty::Null) => true,
        (x, y) => x.same_kind(y) && !matches!(x, Ty::List(_) | Ty::Map(..) | Ty::Opt(_) | Ty::Obj(_) | Ty::Type(_)),
    }
}

/// The more specific of two assignable types.
fn join(a: &Ty, b: &Ty) -> Ty {
    match (a, b) {
        (Ty::Error, _) | (_, Ty::Error) => Ty::Error,
        (Ty::Dyn, x) | (x, Ty::Dyn) if !x.is_dyn() => {
            // A dyn element widens the whole type.
            let _ = x;
            Ty::Dyn
        }
        (Ty::List(x), Ty::List(y)) => Ty::List(Box::new(join(x, y))),
        (Ty::Map(k1, v1), Ty::Map(k2, v2)) => Ty::Map(Box::new(join(k1, k2)), Box::new(join(v1, v2))),
        (Ty::Null, x) | (x, Ty::Null) => x.clone(),
        (x, _) => x.clone(),
    }
}

// ---------------------------------------------------------------------------
// Declarations derived from OpenAPI schemas

/// A schema-derived declared type: the CEL type plus what the cost
/// estimator needs (maximum size, minimum serialized size, children).
#[derive(Debug)]
pub struct Decl {
    pub name: String,
    pub kind: DeclKind,
    /// Maximum length (strings, in bytes), items (lists) or entries (maps).
    pub max_elements: u64,
    pub min_size: i64,
}

#[derive(Debug)]
pub enum DeclKind {
    Scalar(Ty),
    Object(BTreeMap<String, Rc<Decl>>),
    List(Rc<Decl>),
    Map(Rc<Decl>, Rc<Decl>),
}

pub const MAX_REQUEST_SIZE_BYTES: i64 = 3 * 1024 * 1024;

impl Decl {
    pub fn ty(self: &Rc<Self>) -> Ty {
        match &self.kind {
            DeclKind::Scalar(t) => t.clone(),
            DeclKind::Object(_) => Ty::Obj(self.clone()),
            DeclKind::List(e) => Ty::List(Box::new(e.ty())),
            DeclKind::Map(k, v) => Ty::Map(Box::new(k.ty()), Box::new(v.ty())),
        }
    }

    pub fn field(&self, name: &str) -> Option<&Rc<Decl>> {
        match &self.kind {
            DeclKind::Object(f) => f.get(name),
            _ => None,
        }
    }

    pub fn elem(&self) -> Option<&Rc<Decl>> {
        match &self.kind {
            DeclKind::List(e) | DeclKind::Map(_, e) => Some(e),
            _ => None,
        }
    }

    pub fn key(&self) -> Option<&Rc<Decl>> {
        match &self.kind {
            DeclKind::Map(k, _) => Some(k),
            _ => None,
        }
    }

    fn scalar(ty: Ty, max_elements: u64, min_size: i64) -> Rc<Decl> {
        Rc::new(Decl { name: ty.name(), kind: DeclKind::Scalar(ty), max_elements, min_size })
    }

    /// `SchemaDeclType`: the declared type of a structural schema node, or
    /// `None` when rules cannot see it (no type, not int-or-string).
    pub fn from_schema(s: &Json, resource_root: bool, name: &str) -> Option<Rc<Decl>> {
        let flag = |k: &str| s.get(k).and_then(Json::as_bool) == Some(true);
        if flag("x-kubernetes-int-or-string") {
            return Some(Decl::scalar(Ty::Dyn, (MAX_REQUEST_SIZE_BYTES - 2) as u64, 1));
        }
        let ty = s.get("type").and_then(Json::as_str).unwrap_or_default();
        let max_len = s.get("maxLength").and_then(Json::as_i64);
        match ty {
            "array" => {
                let items = Decl::from_schema(s.get("items")?, s.pointer("/items/x-kubernetes-embedded-resource").and_then(Json::as_bool) == Some(true), &format!("{name}.@idx"))?;
                let max = match s.get("maxItems").and_then(Json::as_i64) {
                    Some(m) => m.max(0),
                    None => (MAX_REQUEST_SIZE_BYTES - 2) / (items.min_size + 1),
                };
                Some(Rc::new(Decl { name: "list".into(), kind: DeclKind::List(items), max_elements: max as u64, min_size: 2 }))
            }
            "object" => {
                if let Some(add) = s.get("additionalProperties").filter(|a| a.is_object()) {
                    let embedded = add.get("x-kubernetes-embedded-resource").and_then(Json::as_bool) == Some(true);
                    let vals = Decl::from_schema(add, embedded, &format!("{name}.@idx"))?;
                    let max = match s.get("maxProperties").and_then(Json::as_i64) {
                        Some(m) => m.max(0),
                        // A map entry is at least `"":` and `,` plus the value.
                        None => (MAX_REQUEST_SIZE_BYTES - 2) / (vals.min_size + 6),
                    };
                    let key = Decl::scalar(Ty::Str, 0, 2);
                    return Some(Rc::new(Decl { name: "map".into(), kind: DeclKind::Map(key, vals), max_elements: max as u64, min_size: 2 }));
                }
                let mut fields = BTreeMap::new();
                let required: Vec<&str> = s.get("required").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str).collect();
                let mut min_size = 2;
                if let Some(props) = s.get("properties").and_then(Json::as_object) {
                    for (k, p) in props {
                        let embedded = p.get("x-kubernetes-embedded-resource").and_then(Json::as_bool) == Some(true);
                        let Some(d) = Decl::from_schema(p, embedded, &format!("{name}.{k}")) else { continue };
                        if required.contains(&k.as_str()) && p.get("default").is_none() {
                            min_size += k.len() as i64 + d.min_size + 4;
                        }
                        if let Some(esc) = escape(k) {
                            fields.insert(esc, d);
                        }
                    }
                }
                if resource_root {
                    let string = || Decl::scalar(Ty::Str, (MAX_REQUEST_SIZE_BYTES - 2) as u64, 2);
                    fields.entry("apiVersion".into()).or_insert_with(string);
                    fields.entry("kind".into()).or_insert_with(string);
                    let mut meta = BTreeMap::new();
                    meta.insert("name".to_string(), string());
                    meta.insert("generateName".to_string(), string());
                    fields.insert(
                        "metadata".into(),
                        Rc::new(Decl { name: format!("{name}.metadata"), kind: DeclKind::Object(meta), max_elements: 0, min_size: 2 }),
                    );
                }
                Some(Rc::new(Decl { name: name.to_string(), kind: DeclKind::Object(fields), max_elements: 0, min_size }))
            }
            "string" => {
                let per_request = |fmt: &str| match fmt {
                    "duration" => 32,
                    "date" => 12,
                    "date-time" => 32,
                    _ => (MAX_REQUEST_SIZE_BYTES - 2) as u64,
                };
                let format = s.get("format").and_then(Json::as_str).unwrap_or_default();
                match format {
                    "byte" => {
                        let max = max_len.map(|m| m.max(0) as u64).unwrap_or_else(|| per_request(format));
                        Some(Decl::scalar(Ty::Bytes, max, 2))
                    }
                    "duration" => Some(Decl::scalar(Ty::Duration, per_request(format), 3)),
                    "date" => Some(Decl::scalar(Ty::Timestamp, per_request(format), 12)),
                    "date-time" => Some(Decl::scalar(Ty::Timestamp, per_request(format), 21)),
                    _ => {
                        let max = match max_len {
                            Some(m) => m.max(0) as u64 * 4,
                            None => match s.get("enum").and_then(Json::as_array).filter(|e| !e.is_empty()) {
                                Some(values) => values.iter().filter_map(Json::as_str).map(|v| v.len() as u64).max().unwrap_or(0),
                                None => per_request(format),
                            },
                        };
                        Some(Decl::scalar(Ty::Str, max, 2))
                    }
                }
            }
            "boolean" => Some(Decl::scalar(Ty::Bool, 0, 4)),
            "number" => Some(Decl::scalar(Ty::Double, 0, 1)),
            "integer" => Some(Decl::scalar(Ty::Int, 0, 1)),
            _ => None,
        }
    }
}

const CEL_RESERVED: &[&str] = &[
    "true", "false", "null", "in", "as", "break", "const", "continue", "else", "for", "function", "if", "import", "let", "loop",
    "package", "namespace", "return", "var", "void", "while",
];

/// How a property name is spelled in CEL (`my-field` is `my__dash__field`),
/// or `None` if it cannot be accessed.
pub fn escape(name: &str) -> Option<String> {
    let first = name.chars().next()?;
    if first.is_ascii_digit() {
        return None;
    }
    if CEL_RESERVED.contains(&name) {
        return Some(format!("__{name}__"));
    }
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '_' && chars.get(i + 1) == Some(&'_') {
            out.push_str("__underscores__");
            i += 2;
            continue;
        }
        match c {
            '.' => out.push_str("__dot__"),
            '-' => out.push_str("__dash__"),
            '/' => out.push_str("__slash__"),
            c if c.is_ascii_alphanumeric() || c == '_' => out.push(c),
            _ => return None,
        }
        i += 1;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The checker

/// A declared variable: its type and, for schema-derived values, the
/// declaration the cost estimator sizes it by.
#[derive(Clone)]
pub struct Var {
    pub ty: Ty,
    pub decl: Option<Rc<Decl>>,
}

/// The result of checking: each node's type and schema declaration.
pub struct Checked {
    pub types: HashMap<u32, Ty>,
    pub decls: HashMap<u32, Rc<Decl>>,
    pub result: Ty,
    /// Whether the expression references any of these identifiers.
    pub refs: Vec<String>,
    /// The size of `self` itself, which the Kubernetes size estimator
    /// returns for values it cannot place in the schema.
    pub self_max: u64,
}

struct Checker<'a> {
    vars: &'a BTreeMap<String, Var>,
    scope: Vec<(String, Var)>,
    errors: Vec<Error>,
    types: HashMap<u32, Ty>,
    decls: HashMap<u32, Rc<Decl>>,
    refs: Vec<String>,
}

const TYPE_IDENTS: &[(&str, Ty)] = &[
    ("int", Ty::Int),
    ("uint", Ty::Uint),
    ("double", Ty::Double),
    ("bool", Ty::Bool),
    ("string", Ty::Str),
    ("bytes", Ty::Bytes),
    ("null_type", Ty::Null),
];

/// Every function the environment declares; a call to any other name is an
/// undeclared reference.
const FUNCTIONS: &[&str] = &[
    "size", "contains", "startsWith", "endsWith", "matches", "charAt", "indexOf", "lastIndexOf", "lowerAscii", "upperAscii", "replace",
    "split", "substring", "trim", "join", "format", "strings.quote", "find", "findAll", "isSorted", "sum", "min", "max", "first", "last",
    "sets.contains", "sets.equivalent", "sets.intersects", "url", "isURL", "getScheme", "getHost", "getHostname", "getPort",
    "getEscapedPath", "getQuery", "quantity", "isQuantity", "isInteger", "asInteger", "asApproximateFloat", "add", "sub",
    "isGreaterThan", "isLessThan", "compareTo", "sign", "ip", "isIP", "ip.isCanonical", "family", "isUnspecified", "isLoopback",
    "isLinkLocalMulticast", "isLinkLocalUnicast", "isGlobalUnicast", "cidr", "isCIDR", "containsIP", "containsCIDR", "masked",
    "prefixLength", "format.named", "format.dns1123Label", "format.dns1123Subdomain", "format.dns1035Label", "format.qualifiedName",
    "format.dns1123LabelPrefix", "format.dns1123SubdomainPrefix", "format.dns1035LabelPrefix", "format.labelValue", "format.uri",
    "format.uuid", "format.byte", "format.date", "format.datetime", "validate", "optional.of", "optional.none",
    "optional.ofNonZeroValue", "hasValue", "value", "orValue", "or", "duration", "timestamp", "getHours", "getMinutes", "getSeconds",
    "getMilliseconds", "getFullYear", "getMonth", "getDayOfYear", "getDayOfMonth", "getDate", "getDayOfWeek", "string", "bytes", "int",
    "uint", "double", "bool", "dyn", "type",
];

fn is_num(t: &Ty) -> bool {
    matches!(t, Ty::Int | Ty::Uint | Ty::Double)
}

fn comparable(t: &Ty) -> bool {
    matches!(t, Ty::Int | Ty::Uint | Ty::Double | Ty::Str | Ty::Bytes | Ty::Bool | Ty::Duration | Ty::Timestamp) || t.is_dyn()
}

fn fits(actual: &Ty, expected: &Ty) -> bool {
    assignable(actual, expected)
}

fn elem_of(t: &Ty) -> Ty {
    match t {
        Ty::List(e) => (**e).clone(),
        _ => Ty::Dyn,
    }
}

impl Checker<'_> {
    fn err(&mut self, pos: usize, msg: impl Into<String>) -> Ty {
        self.errors.push(Error { pos, msg: msg.into() });
        Ty::Error
    }

    fn lookup(&self, name: &str) -> Option<Var> {
        if let Some((_, v)) = self.scope.iter().rev().find(|(n, _)| n == name) {
            return Some(v.clone());
        }
        self.vars.get(name).cloned()
    }

    fn check(&mut self, e: &Expr) -> Ty {
        let t = self.check_inner(e);
        self.types.insert(e.id, t.clone());
        t
    }

    fn set_decl(&mut self, e: &Expr, d: Option<Rc<Decl>>) {
        if let Some(d) = d {
            self.decls.insert(e.id, d);
        }
    }

    fn check_inner(&mut self, e: &Expr) -> Ty {
        match &e.kind {
            Kind::Lit(v) => match v {
                Val::Null => Ty::Null,
                Val::Bool(_) => Ty::Bool,
                Val::Int(_) => Ty::Int,
                Val::Uint(_) => Ty::Uint,
                Val::Double(_) => Ty::Double,
                Val::Str(_) => Ty::Str,
                Val::Bytes(_) => Ty::Bytes,
                _ => Ty::Dyn,
            },
            Kind::Ident(name) => {
                if let Some(v) = self.lookup(name) {
                    if !self.refs.contains(name) {
                        self.refs.push(name.clone());
                    }
                    self.set_decl(e, v.decl.clone());
                    return v.ty;
                }
                if let Some((_, t)) = TYPE_IDENTS.iter().find(|(n, _)| n == name) {
                    return Ty::Type(Box::new(t.clone()));
                }
                match name.as_str() {
                    "list" => Ty::Type(Box::new(Ty::List(Box::new(Ty::Dyn)))),
                    "map" => Ty::Type(Box::new(Ty::Map(Box::new(Ty::Dyn), Box::new(Ty::Dyn)))),
                    "type" => Ty::Type(Box::new(Ty::Dyn)),
                    _ => self.err(e.pos, format!("undeclared reference to '{name}' (in container '')")),
                }
            }
            Kind::Select { operand, field, test, optional } => {
                let base = self.check(operand);
                let base_decl = self.decls.get(&operand.id).cloned();
                // Selecting on an optional (with `.?` or `.`) stays optional.
                let (base, opt_base) = match base {
                    Ty::Opt(inner) => (*inner, true),
                    b => (b, false),
                };
                let ft = match &base {
                    Ty::Obj(d) => match d.field(field) {
                        Some(fd) => {
                            if !optional && !opt_base {
                                self.set_decl(e, Some(fd.clone()));
                            }
                            fd.ty()
                        }
                        None => return self.err(e.pos, format!("undefined field '{field}'")),
                    },
                    Ty::Map(k, v) if k.is_dyn() || matches!(**k, Ty::Str) => {
                        if !optional {
                            self.set_decl(e, base_decl.as_ref().and_then(|d| d.elem().cloned()));
                        }
                        (**v).clone()
                    }
                    t if t.is_dyn() => Ty::Dyn,
                    other => return self.err(e.pos, format!("type '{}' does not support field selection", other.name())),
                };
                if *test {
                    Ty::Bool
                } else if *optional || opt_base {
                    Ty::Opt(Box::new(ft))
                } else {
                    ft
                }
            }
            Kind::Index { operand, index, optional } => {
                let base = self.check(operand);
                let idx = self.check(index);
                let base_decl = self.decls.get(&operand.id).cloned();
                let base = match (base, optional) {
                    (Ty::Opt(inner), true) => *inner,
                    (b, _) => b,
                };
                let r = match &base {
                    Ty::List(el) if fits(&idx, &Ty::Int) && !matches!(idx, Ty::Uint) => {
                        if !optional {
                            self.set_decl(e, base_decl.as_ref().and_then(|d| d.elem().cloned()));
                        }
                        (**el).clone()
                    }
                    Ty::Map(k, v) if fits(&idx, k) => {
                        if !optional {
                            self.set_decl(e, base_decl.as_ref().and_then(|d| d.elem().cloned()));
                        }
                        (**v).clone()
                    }
                    t if t.is_dyn() => Ty::Dyn,
                    _ => {
                        let name = if *optional { "_[?_]" } else { "_[_]" };
                        return self.err(e.pos, format!("found no matching overload for '{name}' applied to '({}, {})'", base.name(), idx.name()));
                    }
                };
                if *optional { Ty::Opt(Box::new(r)) } else { r }
            }
            Kind::List(items) => {
                let mut elem: Option<Ty> = None;
                for item in items {
                    let t = self.check(item);
                    elem = Some(match elem {
                        None => t,
                        Some(prev) if assignable(&prev, &t) => join(&prev, &t),
                        Some(prev) => {
                            self.err(item.pos, format!("expected type '{}' but found '{}'", prev.name(), t.name()));
                            prev
                        }
                    });
                }
                Ty::List(Box::new(elem.unwrap_or(Ty::Dyn)))
            }
            Kind::Map(entries) => {
                let (mut kt, mut vt): (Option<Ty>, Option<Ty>) = (None, None);
                for (k, v) in entries {
                    let a = self.check(k);
                    let b = self.check(v);
                    if !matches!(a, Ty::Str | Ty::Int | Ty::Uint | Ty::Bool) && !a.is_dyn() {
                        self.err(k.pos, format!("unsupported map key type: {}", a.name()));
                    }
                    for (slot, t, node) in [(&mut kt, a, k), (&mut vt, b, v)] {
                        *slot = Some(match slot.take() {
                            None => t,
                            Some(prev) if assignable(&prev, &t) => join(&prev, &t),
                            Some(prev) => {
                                self.errors.push(Error { pos: node.pos, msg: format!("expected type '{}' but found '{}'", prev.name(), t.name()) });
                                prev
                            }
                        });
                    }
                }
                Ty::Map(Box::new(kt.unwrap_or(Ty::Dyn)), Box::new(vt.unwrap_or(Ty::Dyn)))
            }
            Kind::Unary(op, inner) => {
                let t = self.check(inner);
                let ok = match *op {
                    "!" => matches!(t, Ty::Bool) || t.is_dyn(),
                    _ => matches!(t, Ty::Int | Ty::Double) || t.is_dyn(),
                };
                if !ok {
                    return self.err(e.pos, format!("found no matching overload for '{op}_' applied to '({})'", t.name()));
                }
                if *op == "!" { Ty::Bool } else if t.is_dyn() { Ty::Dyn } else { t }
            }
            Kind::Cond(c, a, b) => {
                let ct = self.check(c);
                let at = self.check(a);
                let bt = self.check(b);
                if !(matches!(ct, Ty::Bool) || ct.is_dyn()) || !assignable(&at, &bt) {
                    return self.err(e.pos, format!("found no matching overload for '_?_:_' applied to '({}, {}, {})'", ct.name(), at.name(), bt.name()));
                }
                join(&at, &bt)
            }
            Kind::Binary(op, a, b) => {
                let at = self.check(a);
                let bt = self.check(b);
                if at.is_dyn() && matches!(at, Ty::Error) || matches!(bt, Ty::Error) {
                    return if matches!(*op, "&&" | "||" | "==" | "!=" | "<" | "<=" | ">" | ">=" | "in") { Ty::Bool } else { Ty::Error };
                }
                match binary_type(op, &at, &bt) {
                    Some(t) => t,
                    None => {
                        let name = if *op == "in" { "@in".to_string() } else { format!("_{op}_") };
                        self.err(e.pos, format!("found no matching overload for '{name}' applied to '({}, {})'", at.name(), bt.name()))
                    }
                }
            }
            Kind::Call { target, func, args } => self.call(e, target.as_deref(), func, args),
            Kind::Comp(c) => self.comprehension(e, c),
        }
    }

    fn comprehension(&mut self, e: &Expr, c: &Comp) -> Ty {
        let rt = self.check(&c.range);
        let range_decl = self.decls.get(&c.range.id).cloned();
        if matches!(c.name, "optMap" | "optFlatMap") {
            let inner = match &rt {
                Ty::Opt(t) => (**t).clone(),
                t if t.is_dyn() => Ty::Dyn,
                other => return self.err(e.pos, format!("found no matching overload for '{}' applied to '{}.()'", c.name, other.name())),
            };
            self.scope.push((c.v1.clone(), Var { ty: inner, decl: None }));
            let ft = self.check(&c.args[0]);
            self.scope.pop();
            return match c.name {
                "optMap" => Ty::Opt(Box::new(ft)),
                _ => match ft {
                    Ty::Opt(_) => ft,
                    t if t.is_dyn() => Ty::Opt(Box::new(Ty::Dyn)),
                    other => self.err(c.args[0].pos, format!("expected type 'optional_type(dyn)' but found '{}'", other.name())),
                },
            };
        }
        let (k, v, kd, vd) = match &rt {
            Ty::List(el) => (Ty::Int, (**el).clone(), None, range_decl.as_ref().and_then(|d| d.elem().cloned())),
            Ty::Map(kt, vt) => {
                ((**kt).clone(), (**vt).clone(), range_decl.as_ref().and_then(|d| d.key().cloned()), range_decl.as_ref().and_then(|d| d.elem().cloned()))
            }
            t if t.is_dyn() => (Ty::Dyn, Ty::Dyn, None, None),
            other => {
                return self.err(
                    c.range.pos,
                    format!("expression of type '{}' cannot be range of a comprehension (must be list, map, or dynamic)", other.name()),
                );
            }
        };
        let is_map = matches!(rt, Ty::Map(..));
        let mut pushed = 1;
        match &c.v2 {
            Some(v2) => {
                self.scope.push((c.v1.clone(), Var { ty: k.clone(), decl: if is_map { kd.clone() } else { None } }));
                self.scope.push((v2.clone(), Var { ty: v.clone(), decl: vd.clone() }));
                pushed = 2;
            }
            None => {
                let (t, d) = if is_map { (k.clone(), kd.clone()) } else { (v.clone(), vd.clone()) };
                self.scope.push((c.v1.clone(), Var { ty: t, decl: d }));
            }
        }
        let var_ty = if c.v2.is_none() && is_map { k.clone() } else { v.clone() };
        let mut arg_types = Vec::new();
        for a in &c.args {
            arg_types.push(self.check(a));
        }
        for _ in 0..pushed {
            self.scope.pop();
        }
        let need_bool = |me: &mut Self, i: usize| {
            let t = &arg_types[i];
            if !(matches!(t, Ty::Bool) || t.is_dyn()) {
                me.err(c.args[i].pos, format!("expected type 'bool' but found '{}'", t.name()));
            }
        };
        match (c.name, c.args.len()) {
            ("all" | "exists" | "exists_one", 1) => {
                need_bool(self, 0);
                Ty::Bool
            }
            ("map" | "transformList", n) => {
                if n == 2 {
                    need_bool(self, 0);
                }
                Ty::List(Box::new(arg_types[n - 1].clone()))
            }
            ("filter", _) => {
                need_bool(self, 0);
                Ty::List(Box::new(var_ty))
            }
            ("transformMap", n) => {
                if n == 2 {
                    need_bool(self, 0);
                }
                Ty::Map(Box::new(k), Box::new(arg_types[n - 1].clone()))
            }
            ("transformMapEntry", n) => {
                if n == 2 {
                    need_bool(self, 0);
                }
                match &arg_types[n - 1] {
                    t @ Ty::Map(..) => t.clone(),
                    t if t.is_dyn() => Ty::Map(Box::new(Ty::Dyn), Box::new(Ty::Dyn)),
                    other => self.err(c.args[n - 1].pos, format!("expected type 'map(dyn, dyn)' but found '{}'", other.name())),
                }
            }
            _ => self.err(e.pos, format!("wrong number of arguments to {}", c.name)),
        }
    }

    fn call(&mut self, e: &Expr, target: Option<&Expr>, func: &str, args: &[Expr]) -> Ty {
        let recv = target.map(|t| self.check(t));
        let arg_types: Vec<Ty> = args.iter().map(|a| self.check(a)).collect();
        if !FUNCTIONS.contains(&func) {
            return self.err(e.pos, format!("undeclared reference to '{func}' (in container '')"));
        }
        if recv.as_ref().is_some_and(|r| matches!(r, Ty::Error)) || arg_types.iter().any(|t| matches!(t, Ty::Error)) {
            return Ty::Error;
        }
        // Literal arguments the API server validates at compile time.
        let lit_str = |i: usize| match args.get(i).map(|a| &a.kind) {
            Some(Kind::Lit(Val::Str(s))) => Some(s.clone()),
            _ => None,
        };
        match (func, target.is_some()) {
            ("matches", true) | ("find", true) | ("findAll", true) => {
                if let Some(re) = lit_str(0) {
                    if super::eval::valid_regex(&re).is_err() {
                        self.err(args[0].pos, format!("invalid {func} argument"));
                    }
                }
            }
            ("matches", false) => {
                if let Some(re) = lit_str(1) {
                    if super::eval::valid_regex(&re).is_err() {
                        self.err(args[1].pos, "invalid matches argument");
                    }
                }
            }
            ("duration", false) => {
                if let Some(s) = lit_str(0) {
                    if super::eval::parse_go_duration(&s).is_err() {
                        self.err(args[0].pos, "invalid duration argument");
                    }
                }
            }
            ("timestamp", false) => {
                if let Some(s) = lit_str(0) {
                    if super::eval::call_values(None, "timestamp", &[Val::Str(s)]).is_err() {
                        self.err(args[0].pos, "invalid timestamp argument");
                    }
                }
            }
            _ => {}
        }
        match call_type(recv.as_ref(), func, &arg_types) {
            Some(t) => t,
            None => {
                let types: Vec<String> = arg_types.iter().map(Ty::name).collect();
                let applied = match &recv {
                    Some(r) => format!("{}.({})", r.name(), types.join(", ")),
                    None => format!("({})", types.join(", ")),
                };
                self.err(e.pos, format!("found no matching overload for '{func}' applied to '{applied}'"))
            }
        }
    }
}

/// The result type of a binary operator, or `None` when no overload fits.
fn binary_type(op: &str, a: &Ty, b: &Ty) -> Option<Ty> {
    let dyn_either = a.is_dyn() || b.is_dyn();
    match op {
        "&&" | "||" => ((matches!(a, Ty::Bool) || a.is_dyn()) && (matches!(b, Ty::Bool) || b.is_dyn())).then_some(Ty::Bool),
        "==" | "!=" => assignable(a, b).then_some(Ty::Bool),
        "<" | "<=" | ">" | ">=" => {
            let ok = (dyn_either && comparable(a) && comparable(b))
                || (is_num(a) && is_num(b))
                || (a.same_kind(b) && comparable(a))
                || (matches!(a, Ty::Quantity) && matches!(b, Ty::Quantity));
            ok.then_some(Ty::Bool)
        }
        "in" => match b {
            Ty::List(el) => assignable(a, el).then_some(Ty::Bool),
            Ty::Map(k, _) => assignable(a, k).then_some(Ty::Bool),
            t if t.is_dyn() => Some(Ty::Bool),
            _ => None,
        },
        "+" => match (a, b) {
            (x, y) if x.is_dyn() && y.is_dyn() => Some(Ty::Dyn),
            (x, y) if x.is_dyn() || y.is_dyn() => {
                let other = if x.is_dyn() { y } else { x };
                matches!(other, Ty::Int | Ty::Uint | Ty::Double | Ty::Str | Ty::Bytes | Ty::List(_) | Ty::Duration | Ty::Timestamp)
                    .then(|| if matches!(other, Ty::Duration) { Ty::Dyn } else { other.clone() })
            }
            (Ty::List(x), Ty::List(y)) => assignable(x, y).then(|| Ty::List(Box::new(join(x, y)))),
            (Ty::Timestamp, Ty::Duration) | (Ty::Duration, Ty::Timestamp) => Some(Ty::Timestamp),
            (x, y) if x.same_kind(y) && matches!(x, Ty::Int | Ty::Uint | Ty::Double | Ty::Str | Ty::Bytes | Ty::Duration) => Some(x.clone()),
            _ => None,
        },
        "-" => match (a, b) {
            (x, y) if x.is_dyn() || y.is_dyn() => Some(Ty::Dyn),
            (Ty::Timestamp, Ty::Timestamp) => Some(Ty::Duration),
            (Ty::Timestamp, Ty::Duration) => Some(Ty::Timestamp),
            (x, y) if x.same_kind(y) && matches!(x, Ty::Int | Ty::Uint | Ty::Double | Ty::Duration) => Some(x.clone()),
            _ => None,
        },
        "*" | "/" => match (a, b) {
            (x, y) if x.is_dyn() && y.is_dyn() => Some(Ty::Dyn),
            (x, y) if x.is_dyn() || y.is_dyn() => {
                let other = if x.is_dyn() { y } else { x };
                is_num(other).then(|| other.clone())
            }
            (x, y) if x.same_kind(y) && is_num(x) => Some(x.clone()),
            _ => None,
        },
        "%" => match (a, b) {
            (x, y) if x.is_dyn() && y.is_dyn() => Some(Ty::Dyn),
            (x, y) if x.is_dyn() || y.is_dyn() => {
                let other = if x.is_dyn() { y } else { x };
                matches!(other, Ty::Int | Ty::Uint).then(|| other.clone())
            }
            (x, y) if x.same_kind(y) && matches!(x, Ty::Int | Ty::Uint) => Some(x.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// The result type of a library function call, or `None` when no overload
/// fits.
pub fn call_type(recv: Option<&Ty>, func: &str, args: &[Ty]) -> Option<Ty> {
    use Ty::*;
    let s = |t: &Ty| fits(t, &Str);
    let int = |t: &Ty| fits(t, &Int) && !matches!(t, Uint | Double);
    let is = |t: &Ty, want: &Ty| t.is_dyn() || t.same_kind(want);
    let sized = |t: &Ty| matches!(t, Str | Bytes | List(_) | Map(..)) || t.is_dyn();
    match (recv, func, args) {
        (None, "size", [a]) if sized(a) => Some(Int),
        (Some(r), "size", []) if sized(r) => Some(Int),
        (Some(r), "contains" | "startsWith" | "endsWith" | "matches", [a]) if is(r, &Str) && s(a) => Some(Bool),
        (None, "matches", [a, b]) if s(a) && s(b) => Some(Bool),
        (Some(r), "find", [a]) if is(r, &Str) && s(a) => Some(Str),
        (Some(r), "findAll", [a]) if is(r, &Str) && s(a) => Some(List(Box::new(Str))),
        (Some(r), "findAll", [a, n]) if is(r, &Str) && s(a) && int(n) => Some(List(Box::new(Str))),
        (Some(r), "charAt", [i]) if is(r, &Str) && int(i) => Some(Str),
        (Some(r), "indexOf" | "lastIndexOf", [a]) if is(r, &Str) && s(a) && !r.is_dyn() => Some(Int),
        (Some(r), "indexOf" | "lastIndexOf", [a, i]) if is(r, &Str) && s(a) && int(i) => Some(Int),
        (Some(List(el)), "indexOf" | "lastIndexOf", [a]) if assignable(el, a) => Some(Int),
        (Some(r), "indexOf" | "lastIndexOf", [_]) if r.is_dyn() => Some(Int),
        (Some(r), "lowerAscii" | "upperAscii" | "trim", []) if is(r, &Str) => Some(Str),
        (Some(r), "replace", [a, b]) if is(r, &Str) && s(a) && s(b) => Some(Str),
        (Some(r), "replace", [a, b, n]) if is(r, &Str) && s(a) && s(b) && int(n) => Some(Str),
        (Some(r), "split", [a]) if is(r, &Str) && s(a) => Some(List(Box::new(Str))),
        (Some(r), "split", [a, n]) if is(r, &Str) && s(a) && int(n) => Some(List(Box::new(Str))),
        (Some(r), "substring", [i]) if is(r, &Str) && int(i) => Some(Str),
        (Some(r), "substring", [i, j]) if is(r, &Str) && int(i) && int(j) => Some(Str),
        (Some(r), "format", [l]) if is(r, &Str) && (matches!(l, List(_)) || l.is_dyn()) => Some(Str),
        (None, "strings.quote", [a]) if s(a) => Some(Str),
        (Some(r), "join", []) if fits(r, &List(Box::new(Str))) => Some(Str),
        (Some(r), "join", [a]) if fits(r, &List(Box::new(Str))) && s(a) => Some(Str),
        (Some(r), "isSorted", []) if matches!(r, List(_)) && comparable(&elem_of(r)) || r.is_dyn() => Some(Bool),
        (Some(r), "sum", []) if matches!(elem_of(r), Int | Uint | Double | Duration | Dyn | Error) && (matches!(r, List(_)) || r.is_dyn()) => {
            Some(if r.is_dyn() { Dyn } else { elem_of(r) })
        }
        (Some(r), "min" | "max", []) if (matches!(r, List(_)) && comparable(&elem_of(r))) || r.is_dyn() => Some(if r.is_dyn() { Dyn } else { elem_of(r) }),
        (Some(r), "first" | "last", []) if matches!(r, List(_)) || r.is_dyn() => Some(Opt(Box::new(elem_of(r)))),
        (None, "sets.contains" | "sets.equivalent" | "sets.intersects", [a, b])
            if (matches!(a, List(_)) || a.is_dyn()) && (matches!(b, List(_)) || b.is_dyn()) && assignable(a, b) =>
        {
            Some(Bool)
        }
        (None, "url", [a]) if s(a) => Some(Url),
        (None, "isURL", [a]) if s(a) => Some(Bool),
        (Some(r), "getScheme" | "getHost" | "getHostname" | "getPort" | "getEscapedPath", []) if is(r, &Url) => Some(Str),
        (Some(r), "getQuery", []) if is(r, &Url) => Some(Map(Box::new(Str), Box::new(List(Box::new(Str))))),
        (None, "quantity", [a]) if s(a) => Some(Quantity),
        (None, "isQuantity", [a]) if s(a) => Some(Bool),
        (Some(r), "isInteger", []) if is(r, &Quantity) => Some(Bool),
        (Some(r), "asInteger", []) if is(r, &Quantity) => Some(Int),
        (Some(r), "asApproximateFloat", []) if is(r, &Quantity) => Some(Double),
        (Some(r), "add" | "sub", [a]) if is(r, &Quantity) && (is(a, &Quantity) || int(a)) => Some(Quantity),
        (Some(r), "isGreaterThan" | "isLessThan", [a]) if is(r, &Quantity) && is(a, &Quantity) => Some(Bool),
        (Some(r), "compareTo", [a]) if is(r, &Quantity) && is(a, &Quantity) => Some(Int),
        (None, "ip", [a]) if s(a) => Some(Ip),
        (None, "isIP" | "ip.isCanonical", [a]) if s(a) => Some(Bool),
        (Some(r), "family", []) if is(r, &Ip) => Some(Int),
        (Some(r), "isUnspecified" | "isLoopback" | "isLinkLocalMulticast" | "isLinkLocalUnicast" | "isGlobalUnicast", []) if is(r, &Ip) => Some(Bool),
        (None, "cidr", [a]) if s(a) => Some(Cidr),
        (None, "isCIDR", [a]) if s(a) => Some(Bool),
        (Some(r), "containsIP", [a]) if is(r, &Cidr) && (is(a, &Ip) || s(a)) => Some(Bool),
        (Some(r), "containsCIDR", [a]) if is(r, &Cidr) && (is(a, &Cidr) || s(a)) => Some(Bool),
        (Some(r), "ip", []) if is(r, &Cidr) => Some(Ip),
        (Some(r), "masked", []) if is(r, &Cidr) => Some(Cidr),
        (Some(r), "prefixLength", []) if is(r, &Cidr) => Some(Int),
        (None, "format.named", [a]) if s(a) => Some(Opt(Box::new(Format))),
        (None, f, []) if f.starts_with("format.") && f != "format.named" => Some(Format),
        (Some(r), "validate", [a]) if is(r, &Format) && s(a) => Some(Opt(Box::new(List(Box::new(Str))))),
        (None, "optional.of" | "optional.ofNonZeroValue", [a]) => Some(Opt(Box::new(a.clone()))),
        (None, "optional.none", []) => Some(Opt(Box::new(Dyn))),
        (Some(Opt(_)), "hasValue", []) => Some(Bool),
        (Some(Opt(t)), "value", []) => Some((**t).clone()),
        (Some(Opt(t)), "orValue", [a]) if assignable(t, a) => Some(join(t, a)),
        (Some(Opt(t)), "or", [Opt(u)]) if assignable(t, u) => Some(Opt(Box::new(join(t, u)))),
        (Some(r), "hasValue", []) if r.is_dyn() => Some(Bool),
        (Some(r), "value" | "orValue" | "or", _) if r.is_dyn() => Some(Dyn),
        (None, "duration", [a]) if s(a) || is(a, &Duration) => Some(Duration),
        (None, "timestamp", [a]) if s(a) || is(a, &Timestamp) || int(a) => Some(Timestamp),
        (Some(r), "getHours" | "getMinutes" | "getSeconds" | "getMilliseconds", []) if is(r, &Duration) || is(r, &Timestamp) => Some(Int),
        (Some(r), "getHours" | "getMinutes" | "getSeconds" | "getMilliseconds", [tz]) if is(r, &Timestamp) && s(tz) => Some(Int),
        (Some(r), "getFullYear" | "getMonth" | "getDayOfYear" | "getDayOfMonth" | "getDate" | "getDayOfWeek", []) if is(r, &Timestamp) => Some(Int),
        (Some(r), "getFullYear" | "getMonth" | "getDayOfYear" | "getDayOfMonth" | "getDate" | "getDayOfWeek", [tz]) if is(r, &Timestamp) && s(tz) => {
            Some(Int)
        }
        (None, "string", [a])
            if a.is_dyn() || matches!(a, Int | Uint | Double | Bool | Bytes | Str | Timestamp | Duration | Ip | Cidr) =>
        {
            Some(Str)
        }
        (None, "bytes", [a]) if a.is_dyn() || matches!(a, Str | Bytes) => Some(Bytes),
        (None, "int", [a]) if a.is_dyn() || matches!(a, Int | Uint | Double | Str | Timestamp | Duration) => Some(Int),
        (None, "uint", [a]) if a.is_dyn() || matches!(a, Int | Uint | Double | Str) => Some(Uint),
        (None, "double", [a]) if a.is_dyn() || matches!(a, Int | Uint | Double | Str) => Some(Double),
        (None, "bool", [a]) if a.is_dyn() || matches!(a, Bool | Str) => Some(Bool),
        (None, "dyn", [_]) => Some(Dyn),
        (None, "type", [a]) => Some(Type(Box::new(a.clone()))),
        _ => None,
    }
}

/// Type-check `program` with these variables. Errors are rendered the
/// cel-go way (`ERROR: <input>:1:5: undefined field 'foo'` ...).
pub fn check(program: &Program, vars: &BTreeMap<String, Var>) -> Result<Checked, String> {
    let mut c = Checker { vars, scope: Vec::new(), errors: Vec::new(), types: HashMap::new(), decls: HashMap::new(), refs: Vec::new() };
    let result = c.check(&program.root);
    if !c.errors.is_empty() {
        c.errors.sort_by_key(|e| e.pos);
        return Err(super::parse::render_errors(&program.src, &c.errors));
    }
    let self_max = vars.get("self").and_then(|v| v.decl.as_ref()).map(|d| d.max_elements).unwrap_or(0);
    Ok(Checked { types: c.types, decls: c.decls, result, refs: c.refs, self_max })
}
