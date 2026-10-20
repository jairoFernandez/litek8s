//! Static cost estimation, ported from cel-go's `checker/cost.go` with the
//! Kubernetes size estimator (sizes come from `maxLength`, `maxItems` and
//! `maxProperties`, or from the 3 MiB request limit when unbounded) and
//! the Kubernetes library call costs. The API server rejects CRD rules
//! whose estimate exceeds its budget, so the numbers matter: an unbounded
//! list iterated twice or a `string(int)` concatenated in a
//! messageExpression is rejected, a bounded one is not.
//!
//! cel-go is Copyright Google LLC and the Kubernetes CEL libraries Copyright
//! The Kubernetes Authors, both under the Apache License 2.0 (see NOTICE).

use std::collections::HashMap;

use super::check::{Checked, Ty};
use super::parse::{Comp, Expr, Kind, Program};
use super::Val;

const STRING_TRAVERSAL: f64 = 0.1;
const REGEX_LENGTH: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Est {
    pub min: u64,
    pub max: u64,
}

impl Est {
    const ZERO: Est = Est { min: 0, max: 0 };
    const ONE: Est = Est { min: 1, max: 1 };
    const UNKNOWN: Est = Est { min: 0, max: u64::MAX };

    fn n(v: u64) -> Est {
        Est { min: v, max: v }
    }

    fn add(self, o: Est) -> Est {
        Est { min: self.min.saturating_add(o.min), max: self.max.saturating_add(o.max) }
    }

    fn mul(self, o: Est) -> Est {
        Est { min: self.min.saturating_mul(o.min), max: self.max.saturating_mul(o.max) }
    }

    fn union(self, o: Est) -> Est {
        Est { min: self.min.min(o.min), max: self.max.max(o.max) }
    }

    fn factor(self, f: f64) -> Est {
        let m = |x: u64| {
            let v = (x as f64 * f).ceil();
            if v >= u64::MAX as f64 { u64::MAX } else { v as u64 }
        };
        Est { min: m(self.min), max: m(self.max) }
    }
}

struct Coster<'a> {
    checked: &'a Checked,
    computed: HashMap<u32, Est>,
    /// Comprehension variables and the size of one element of their range.
    scopes: Vec<(String, Option<Est>)>,
}

impl Coster<'_> {
    fn ty(&self, e: &Expr) -> Ty {
        self.checked.types.get(&e.id).cloned().unwrap_or(Ty::Dyn)
    }

    /// The size of a value: literals, computed results, or the declared
    /// schema size; unknown otherwise.
    fn size(&self, e: &Expr) -> Est {
        match &e.kind {
            Kind::Lit(Val::Str(s)) => return Est::n(s.chars().count() as u64),
            Kind::Lit(Val::Bytes(b)) => return Est::n(b.len() as u64),
            Kind::Lit(Val::Null | Val::Bool(_) | Val::Int(_) | Val::Uint(_) | Val::Double(_)) => return Est::ONE,
            Kind::List(items) => return Est::n(items.len() as u64),
            Kind::Map(entries) => return Est::n(entries.len() as u64),
            _ => {}
        }
        if let Some(s) = self.computed.get(&e.id) {
            return *s;
        }
        // Values of scalar types have a constant size.
        if matches!(self.ty(e), Ty::Bool | Ty::Double | Ty::Duration | Ty::Int | Ty::Timestamp | Ty::Uint | Ty::Opt(_)) {
            return Est::ONE;
        }
        if let Kind::Ident(name) = &e.kind {
            if let Some((_, Some(s))) = self.scopes.iter().rev().find(|(n, _)| n == name) {
                return *s;
            }
        }
        match self.checked.decls.get(&e.id) {
            Some(d) => Est { min: 0, max: d.max_elements },
            None => Est::UNKNOWN,
        }
    }

    fn elem_size(&self, e: &Expr) -> Option<Est> {
        self.checked.decls.get(&e.id).and_then(|d| d.elem()).map(|d| Est { min: 0, max: d.max_elements })
    }

    fn cost(&mut self, e: &Expr) -> Est {
        match &e.kind {
            Kind::Lit(_) => Est::ZERO,
            Kind::Ident(_) => Est::ONE,
            Kind::Select { operand, test: true, .. } => self.cost(operand),
            Kind::Select { operand, optional: true, .. } => Est::ONE.add(self.cost(operand)),
            Kind::Select { operand, .. } => {
                let c = self.cost(operand);
                match self.ty(operand) {
                    Ty::Map(..) | Ty::Obj(_) | Ty::Dyn | Ty::Error => c.add(Est::ONE),
                    _ => c,
                }
            }
            Kind::Index { operand, index, .. } => {
                let c = self.cost(operand).add(self.cost(index));
                if !self.checked.decls.contains_key(&operand.id) && matches!(self.ty(e), Ty::Str | Ty::Bytes | Ty::List(_) | Ty::Map(..)) {
                    self.computed.entry(e.id).or_insert(Est { min: 0, max: self.checked.self_max });
                }
                Est::ONE.add(c)
            }
            Kind::List(items) => items.iter().fold(Est::n(10), |acc, i| acc.add(self.cost(i))),
            Kind::Map(entries) => entries.iter().fold(Est::n(30), |acc, (k, v)| {
                let c = self.cost(k).add(self.cost(v));
                acc.add(c)
            }),
            Kind::Unary(_, inner) => Est::ONE.add(self.cost(inner)),
            Kind::Cond(c, a, b) => {
                let cc = self.cost(c);
                let ac = self.cost(a);
                let bc = self.cost(b);
                let size = self.size(a).union(self.size(b));
                self.computed.insert(e.id, size);
                cc.add(ac.union(bc))
            }
            Kind::Binary(op, a, b) => {
                let ac = self.cost(a);
                let bc = self.cost(b);
                let args = ac.add(bc);
                match *op {
                    "&&" | "||" => Est { min: ac.min, max: ac.add(bc).max },
                    "==" | "!=" => self.equality(a, b).add(args),
                    "<" | "<=" | ">" | ">=" => {
                        let (at, bt) = (self.ty(a), self.ty(b));
                        if matches!((&at, &bt), (Ty::Str, Ty::Str) | (Ty::Bytes, Ty::Bytes)) {
                            self.equality(a, b).add(args)
                        } else {
                            Est::ONE.add(args)
                        }
                    }
                    "in" => match self.ty(b) {
                        Ty::List(_) => self.size(b).factor(1.0).add(args),
                        _ => Est::ONE.add(args),
                    },
                    "+" => {
                        let (at, bt) = (self.ty(a), self.ty(b));
                        match (&at, &bt) {
                            (Ty::Str, Ty::Str) | (Ty::Bytes, Ty::Bytes) => {
                                let size = self.size(a).add(self.size(b));
                                self.computed.insert(e.id, size);
                                size.factor(STRING_TRAVERSAL).add(args)
                            }
                            (Ty::List(_), Ty::List(_)) => {
                                let size = self.size(a).add(self.size(b));
                                self.computed.insert(e.id, size);
                                Est::ONE.add(args)
                            }
                            _ => Est::ONE.add(args),
                        }
                    }
                    _ => Est::ONE.add(args),
                }
            }
            Kind::Call { target, func, args } => self.call(e, target.as_deref(), func, args),
            Kind::Comp(c) => self.comprehension(e, c),
        }
    }

    fn equality(&self, a: &Expr, b: &Expr) -> Est {
        let smallest = self.size(a).max.min(self.size(b).max);
        Est { min: if smallest > 0 { 1 } else { 0 }, max: smallest }.factor(STRING_TRAVERSAL)
    }

    fn call(&mut self, e: &Expr, target: Option<&Expr>, func: &str, args: &[Expr]) -> Est {
        let arg_costs: Vec<Est> = args.iter().map(|a| self.cost(a)).collect();
        let arg_sum = arg_costs.iter().fold(Est::ZERO, |acc, c| acc.add(*c));
        let target_cost = target.map(|t| self.cost(t)).unwrap_or(Est::ZERO);
        let ts = || target.map(|t| self.size(t)).unwrap_or(Est::UNKNOWN);
        let a0 = || args.first().map(|a| self.size(a)).unwrap_or(Est::UNKNOWN);
        let mut result_size: Option<Est> = None;
        let fn_cost = match (func, target.is_some()) {
            // Kubernetes library estimates.
            ("isSorted" | "sum" | "max" | "min" | "indexOf" | "lastIndexOf", true) => {
                let t = target.unwrap();
                match self.ty(t) {
                    Ty::List(el) => {
                        let mut el_cost = Est::ONE;
                        if matches!(*el, Ty::Str | Ty::Bytes) {
                            el_cost = el_cost.add(self.elem_size(t).unwrap_or(Est::UNKNOWN).factor(STRING_TRAVERSAL));
                        }
                        ts().mul(el_cost)
                    }
                    _ => ts().factor(STRING_TRAVERSAL),
                }
            }
            ("url" | "quantity" | "isQuantity" | "cidr" | "isIP" | "isCIDR" | "ip" | "ip.isCanonical", false) => a0().factor(STRING_TRAVERSAL),
            ("lowerAscii" | "upperAscii" | "substring" | "trim", true) => {
                let sz = ts();
                result_size = Some(sz);
                sz.factor(STRING_TRAVERSAL)
            }
            ("replace", true) if args.len() >= 2 => {
                let sz = ts();
                let to_replace = self.size(&args[0]);
                let replace_with = self.size(&args[1]);
                let (mut count, mut retained) = (Est::ZERO, Est::ZERO);
                if to_replace.min == 0 {
                    count.max = sz.max.saturating_add(if sz.max < u64::MAX { 1 } else { 0 });
                    retained.max = sz.max;
                } else if replace_with.max <= to_replace.min {
                    retained.max = sz.max;
                } else {
                    count.max = (sz.max as f64 / to_replace.min as f64).ceil() as u64;
                }
                if to_replace.max == 0 {
                    count.min = sz.min.saturating_add(if sz.min < u64::MAX { 1 } else { 0 });
                    retained.min = sz.min;
                } else if to_replace.max <= replace_with.min {
                    retained.min = sz.min;
                } else {
                    count.min = (sz.min as f64 / to_replace.max as f64).ceil() as u64;
                }
                result_size = Some(count.mul(replace_with).add(retained));
                sz.factor(2.0 * STRING_TRAVERSAL)
            }
            ("split", true) => {
                let sz = ts();
                let mut max = sz.max;
                if let Some(Expr { kind: Kind::Lit(Val::Int(n)), .. }) = args.get(1) {
                    max = *n as u64;
                }
                result_size = Some(Est { min: 0, max });
                sz.factor(2.0 * STRING_TRAVERSAL)
            }
            ("join", true) => {
                let list = ts();
                let elem = target.and_then(|t| self.elem_size(t)).unwrap_or(Est { min: 0, max: self.checked.self_max });
                let mut sz = list.mul(elem);
                if !args.is_empty() {
                    let sep = a0();
                    sz = sz.add(sep.mul(Est { min: list.min.saturating_sub(1), max: list.max.saturating_sub(1) }));
                }
                result_size = Some(sz);
                sz.factor(STRING_TRAVERSAL)
            }
            ("find" | "findAll", true) if !args.is_empty() => {
                let sz = ts();
                result_size = Some(Est { min: 0, max: sz.max });
                sz.factor(STRING_TRAVERSAL).mul(a0().factor(REGEX_LENGTH))
            }
            ("validate", true) => {
                let regex = super::lib::named_format_regex_size("");
                a0().factor(STRING_TRAVERSAL).mul(Est::n(regex).factor(REGEX_LENGTH))
            }
            ("containsIP" | "containsCIDR", true) => match args.first().map(|a| self.ty(a)) {
                Some(Ty::Str) => a0().factor(STRING_TRAVERSAL),
                _ => Est::ONE,
            },
            ("sets.contains" | "sets.equivalent" | "sets.intersects", false) if args.len() == 2 => {
                self.size(&args[0]).mul(self.size(&args[1])).factor(1.0).add(Est::ONE)
            }
            // cel-go's own estimates.
            ("format", true) => ts().factor(STRING_TRAVERSAL),
            ("string", false) if matches!(args.first().map(|a| self.ty(a)), Some(Ty::Bytes)) => {
                let sz = a0();
                result_size = Some(Est { min: sz.min / 4, max: sz.max });
                sz.factor(STRING_TRAVERSAL)
            }
            ("bytes", false) if matches!(args.first().map(|a| self.ty(a)), Some(Ty::Str)) => {
                let sz = a0();
                result_size = Some(Est { min: sz.min, max: sz.max.saturating_mul(4) });
                sz.factor(STRING_TRAVERSAL)
            }
            ("strings.quote", false) => {
                let sz = a0();
                result_size = Some(Est { min: sz.min.saturating_add(2), max: sz.max.saturating_mul(2).saturating_add(2) });
                sz.factor(STRING_TRAVERSAL)
            }
            ("startsWith" | "endsWith", true) => a0().factor(STRING_TRAVERSAL),
            ("matches", true) => ts().add(Est::ONE).factor(STRING_TRAVERSAL).mul(a0().factor(REGEX_LENGTH)),
            ("contains", true) if matches!(target.map(|t| self.ty(t)), Some(Ty::Str)) => {
                ts().factor(STRING_TRAVERSAL).mul(a0().factor(STRING_TRAVERSAL))
            }
            _ => Est::ONE,
        };
        if let Some(sz) = result_size {
            self.computed.insert(e.id, sz);
        }
        target_cost.add(fn_cost.add(arg_sum))
    }

    fn comprehension(&mut self, e: &Expr, c: &Comp) -> Est {
        let range_cost = self.cost(&c.range);
        if matches!(c.name, "optMap" | "optFlatMap") {
            self.scopes.push((c.v1.clone(), None));
            let f = self.cost(&c.args[0]);
            self.scopes.pop();
            return range_cost.mul(Est::n(2)).add(Est::n(13)).add(f);
        }
        let range_count = self.size(&c.range);
        // Elements of a range the schema does not size (a function result)
        // get what the Kubernetes estimator returns for an unplaceable
        // path: the size of `self`.
        let fallback = Some(Est { min: 0, max: self.checked.self_max });
        let el = self.elem_size(&c.range).or(fallback);
        let key = self.checked.decls.get(&c.range.id).and_then(|d| d.key()).map(|d| Est { min: 0, max: d.max_elements }).or(fallback);
        let is_map = matches!(self.ty(&c.range), Ty::Map(..));
        match &c.v2 {
            Some(v2) => {
                self.scopes.push((c.v1.clone(), if is_map { key } else { None }));
                self.scopes.push((v2.clone(), el));
            }
            None => self.scopes.push((c.v1.clone(), if is_map { key } else { el })),
        }
        let arg_costs: Vec<Est> = c.args.iter().map(|a| self.cost(a)).collect();
        for _ in 0..if c.v2.is_some() { 2 } else { 1 } {
            self.scopes.pop();
        }
        let ident = Est::ONE;
        let last = *arg_costs.last().unwrap_or(&Est::ZERO);
        let filtered = |step: Est| -> Est {
            if arg_costs.len() == 2 { arg_costs[0].add(step.union(ident)) } else { step }
        };
        let (init, cond, step, result) = match c.name {
            "all" => (Est::ZERO, Est::n(2), Est { min: 1, max: 1u64.saturating_add(last.max) }, ident),
            "exists" => (Est::ZERO, Est::n(3), Est { min: 1, max: 1u64.saturating_add(last.max) }, ident),
            "exists_one" => (Est::ZERO, Est::ZERO, last.add(Est { min: 1, max: 2 }), Est::n(2)),
            "map" | "transformList" => (Est::n(10), Est::ZERO, filtered(Est::n(12).add(last)), ident),
            "filter" => (Est::n(10), Est::ZERO, last.add(Est { min: 1, max: 13 }), ident),
            "transformMap" => (Est::n(30), Est::ZERO, filtered(Est::n(3).add(last)), ident),
            _ => (Est::n(30), Est::ZERO, filtered(Est::n(2).add(last)), ident),
        };
        self.computed.insert(e.id, range_count);
        range_cost.add(init).add(result).add(range_count.mul(step.add(cond)))
    }
}

/// The estimated cost of evaluating `program` once.
pub fn estimate(program: &Program, checked: &Checked) -> Est {
    let mut c = Coster { checked, computed: HashMap::new(), scopes: Vec::new() };
    c.cost(&program.root)
}

/// The API server's "exceeds budget by factor of ..." message.
pub fn exceeds_message(cost_name: &str, cost: u64, limit: u64) -> String {
    let f = cost as f64 / limit as f64;
    let factor = if f > 100.0 {
        "more than 100x".to_string()
    } else if f < 1.5 {
        format!("{f:.6}x")
    } else {
        format!("{f:.1}x")
    };
    format!(
        "{cost_name} exceeds budget by factor of {factor} (try simplifying the rule, or adding maxItems, maxProperties, and maxLength where arrays, maps, and strings are declared)"
    )
}
