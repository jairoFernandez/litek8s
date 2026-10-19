//! CustomResourceDefinition schemas, validated the way the API server
//! does it.
//!
//! - [`validate_crd`]: checks on the CRD itself at create/update: list and
//!   map types, patterns that must compile, and `x-kubernetes-validations`
//!   rules, which must type-check against the schema (with `self` and
//!   `oldSelf`), return a bool, only use `oldSelf` where the old value can
//!   be correlated, and fit the CEL cost budget.
//! - [`validate`]: checks on a custom resource: OpenAPI value validations
//!   (types, enum, bounds, `pattern`, `format`, `allOf`/`anyOf`/`oneOf`/`not`),
//!   list-set and list-map uniqueness, then the CEL rules, with transition
//!   rules on updates and validation ratcheting (unchanged values that no
//!   longer validate are let through).
//! - [`prune_nulls`]: nulls of non-nullable fields are dropped (or replaced
//!   by the default), like the API server does before validating.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use super::cel::{self, Val, check, cost};

const PER_RULE_COST_LIMIT: u64 = 10_000_000;
const PER_CRD_COST_LIMIT: u64 = 100_000_000;
const REASONS: &[&str] = &["FieldValueDuplicate", "FieldValueForbidden", "FieldValueInvalid", "FieldValueRequired"];

fn flag(s: &Value, k: &str) -> bool {
    s.get(k).and_then(Value::as_bool) == Some(true)
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn rules_of(s: &Value) -> &[Value] {
    s.get("x-kubernetes-validations").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()
}

/// Go's `%q`.
fn go_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A rule as the API server prints it in errors (`%#v` of the internal
/// ValidationRule).
fn rule_repr(r: &Value) -> String {
    let reason = if r.get("reason").is_some_and(|v| !v.is_null()) { "(*apiextensions.FieldValueErrorReason)(0xc000a1b2c0)" } else { "(*apiextensions.FieldValueErrorReason)(nil)" };
    let oos = if r.get("optionalOldSelf").is_some_and(|v| !v.is_null()) { "(*bool)(0xc000a1b2c8)" } else { "(*bool)(nil)" };
    format!(
        "apiextensions.ValidationRule{{Rule:{}, Message:{}, MessageExpression:{}, Reason:{reason}, FieldPath:{}, OptionalOldSelf:{oos}}}",
        go_quote(str_of(r, "rule")),
        go_quote(str_of(r, "message")),
        go_quote(str_of(r, "messageExpression")),
        go_quote(str_of(r, "fieldPath")),
    )
}

fn parse_cached(src: &str) -> Result<Arc<cel::Program>, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<cel::Program>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(p) = cache.lock().unwrap().get(src) {
        return Ok(p.clone());
    }
    let p = Arc::new(cel::parse::parse(src).map_err(|e| e.render(src))?);
    let mut c = cache.lock().unwrap();
    if c.len() > 4096 {
        c.clear();
    }
    c.insert(src.to_string(), p.clone());
    Ok(p)
}

/// Whether an expression refers to `oldSelf` (outside a comprehension
/// variable of that name).
fn uses_old_self(e: &cel::Expr) -> bool {
    use cel::Kind;
    match &e.kind {
        Kind::Ident(n) => n == "oldSelf",
        Kind::Lit(_) => false,
        Kind::Select { operand, .. } => uses_old_self(operand),
        Kind::Index { operand, index, .. } => uses_old_self(operand) || uses_old_self(index),
        Kind::Call { target, args, .. } => target.as_deref().is_some_and(uses_old_self) || args.iter().any(uses_old_self),
        Kind::List(items) => items.iter().any(uses_old_self),
        Kind::Map(entries) => entries.iter().any(|(k, v)| uses_old_self(k) || uses_old_self(v)),
        Kind::Unary(_, a) => uses_old_self(a),
        Kind::Binary(_, a, b) => uses_old_self(a) || uses_old_self(b),
        Kind::Cond(a, b, c) => uses_old_self(a) || uses_old_self(b) || uses_old_self(c),
        Kind::Comp(c) => {
            uses_old_self(&c.range)
                || (c.v1 != "oldSelf" && c.v2.as_deref() != Some("oldSelf") && c.args.iter().any(uses_old_self))
        }
    }
}

// ---------------------------------------------------------------------------
// The CRD itself

/// Where each version's schema is reported: `spec.validation` when all
/// versions share one schema (that is how the API server stores it
/// internally), else per version.
fn schema_roots(crd: &Value) -> Vec<(String, Value)> {
    let versions = crd.pointer("/spec/versions").and_then(Value::as_array).cloned().unwrap_or_default();
    let schemas: Vec<Option<&Value>> = versions.iter().map(|v| v.pointer("/schema/openAPIV3Schema").filter(|s| s.is_object())).collect();
    if !schemas.is_empty() && schemas.iter().all(|s| s.is_some() && *s == schemas[0]) {
        return vec![("spec.validation.openAPIV3Schema".into(), schemas[0].unwrap().clone())];
    }
    schemas
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|s| (format!("spec.versions[{i}].schema.openAPIV3Schema"), s.clone())))
        .collect()
}

#[derive(Default)]
struct CrdCheck {
    schema_errs: Vec<String>,
    cel_errs: Vec<String>,
    /// CEL rules found while walking: node path, the rule index, the node
    /// schema, cardinality, whether the node is correlatable, and the
    /// uncorrelatable ancestor if any.
    rules: Vec<RuleSite>,
}

struct RuleSite {
    path: String,
    schema: Value,
    resource_root: bool,
    cardinality: Option<u64>,
    uncorrelatable: Option<String>,
}

/// A fieldPath (`.a.b`, `.m['k']`) that names an existing schema field.
fn valid_field_path(fp: &str, schema: &Value) -> bool {
    let mut s = schema;
    let mut rest = fp;
    if rest.is_empty() {
        return true;
    }
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            let name = &r[..end];
            if name.is_empty() {
                return false;
            }
            match s.pointer(&format!("/properties/{}", name.replace('~', "~0").replace('/', "~1"))) {
                Some(p) => s = p,
                None => return false,
            }
            rest = &r[end..];
        } else if let Some(r) = rest.strip_prefix("['") {
            let Some(end) = r.find("']") else { return false };
            let name = &r[..end];
            s = match s.pointer(&format!("/properties/{}", name.replace('~', "~0").replace('/', "~1"))) {
                Some(p) => p,
                None => match s.get("additionalProperties").filter(|a| a.is_object()) {
                    Some(a) => a,
                    None => return false,
                },
            };
            rest = &r[end + 2..];
        } else {
            return false;
        }
    }
    true
}

impl CrdCheck {
    fn walk(&mut self, s: &Value, path: &str, cardinality: Option<u64>, uncorrelatable: Option<&str>, resource_root: bool) {
        if !s.is_object() {
            return;
        }
        let ty = str_of(s, "type");
        if let Some(p) = s.get("pattern").and_then(Value::as_str) {
            if let Err(e) = cel::eval::valid_regex(p) {
                let detail = e.trim_start_matches("error parsing regexp: ");
                self.schema_errs.push(format!(
                    "{path}.pattern: Invalid value: {}: must be a valid regular expression, but isn't: error parsing regexp: {detail}: `{}`",
                    go_quote(p),
                    p
                ));
            }
        }
        let list_type = s.get("x-kubernetes-list-type").and_then(Value::as_str);
        if let Some(lt) = list_type {
            if ty != "array" {
                self.schema_errs.push(format!("{path}.type: Invalid value: {}: must be array if x-kubernetes-list-type is specified", go_quote(ty)));
            } else if !matches!(lt, "atomic" | "set" | "map") {
                self.schema_errs.push(format!("{path}.x-kubernetes-list-type: Unsupported value: {}: supported values: \"atomic\", \"map\", \"set\"", go_quote(lt)));
            }
            let items = s.get("items").cloned().unwrap_or(Value::Null);
            if lt == "map" {
                let keys: Vec<&str> = s.get("x-kubernetes-list-map-keys").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                if keys.is_empty() {
                    self.schema_errs.push(format!("{path}.x-kubernetes-list-map-keys: Required value: must not be empty if x-kubernetes-list-type is map"));
                }
                if str_of(&items, "type") != "object" {
                    self.schema_errs.push(format!("{path}.items.type: Invalid value: {}: must be object if parent array's x-kubernetes-list-type is map", go_quote(str_of(&items, "type"))));
                } else {
                    let required: Vec<&str> = items.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    for k in &keys {
                        match items.pointer(&format!("/properties/{k}")) {
                            None => self.schema_errs.push(format!("{path}.x-kubernetes-list-map-keys: Invalid value: {}: entries must all be names of item properties", go_quote(k))),
                            Some(p) => {
                                if !matches!(str_of(p, "type"), "string" | "integer" | "number" | "boolean") && !flag(p, "x-kubernetes-int-or-string") {
                                    self.schema_errs.push(format!("{path}.items.properties[{k}].type: Invalid value: {}: must be a scalar type if parent array's x-kubernetes-list-type is map", go_quote(str_of(p, "type"))));
                                }
                                if !required.contains(k) && p.get("default").is_none() {
                                    self.schema_errs.push(format!("{path}.items.properties[{k}].default: Required value: this property is in x-kubernetes-list-map-keys, so it must have a default or be a required property"));
                                }
                            }
                        }
                    }
                }
            }
            if lt == "set" && str_of(&items, "type") == "object" && str_of(&items, "x-kubernetes-map-type") != "atomic" {
                let shown = items.get("x-kubernetes-map-type").and_then(Value::as_str).map(go_quote).unwrap_or_else(|| "\"null\"".into());
                self.schema_errs.push(format!("{path}.items.x-kubernetes-map-type: Invalid value: {shown}: must be atomic as item of a list with x-kubernetes-list-type=set"));
            }
            if lt == "set" && str_of(&items, "type") == "array" && str_of(&items, "x-kubernetes-list-type") != "atomic" {
                self.schema_errs.push(format!("{path}.items.x-kubernetes-list-type: Invalid value: {}: must be atomic as item of a list with x-kubernetes-list-type=set", items.get("x-kubernetes-list-type").and_then(Value::as_str).map(go_quote).unwrap_or_else(|| "\"null\"".into())));
            }
        }
        if let Some(mt) = s.get("x-kubernetes-map-type").and_then(Value::as_str) {
            if ty != "object" {
                self.schema_errs.push(format!("{path}.type: Invalid value: {}: must be object if x-kubernetes-map-type is specified", go_quote(ty)));
            } else if !matches!(mt, "atomic" | "granular") {
                self.schema_errs.push(format!("{path}.x-kubernetes-map-type: Unsupported value: {}: supported values: \"atomic\", \"granular\"", go_quote(mt)));
            }
        }
        let rules = rules_of(s);
        for (i, r) in rules.iter().enumerate() {
            let at = format!("{path}.x-kubernetes-validations[{i}]");
            if str_of(r, "rule").trim().is_empty() {
                self.schema_errs.push(format!("{at}.rule: Required value: rule is not specified"));
            }
            if let Some(m) = r.get("message").and_then(Value::as_str) {
                if m.trim().is_empty() {
                    self.schema_errs.push(format!("{at}.message: Invalid value: {}: message must be non-empty if specified", go_quote(m)));
                } else if m.contains('\n') {
                    self.schema_errs.push(format!("{at}.message: Invalid value: {}: message must not contain line breaks", go_quote(m)));
                } else if m.len() > 2048 {
                    self.schema_errs.push(format!("{at}.message: Too long: may not be more than 2048 bytes"));
                }
            }
            if let Some(m) = r.get("messageExpression").and_then(Value::as_str) {
                if m.trim().is_empty() {
                    self.schema_errs.push(format!("{at}.messageExpression: Required value: messageExpression must be non-empty if specified"));
                }
            }
            if let Some(reason) = r.get("reason").and_then(Value::as_str) {
                if !REASONS.contains(&reason) {
                    self.schema_errs.push(format!(
                        "{at}.reason: Unsupported value: {}: supported values: \"FieldValueDuplicate\", \"FieldValueForbidden\", \"FieldValueInvalid\", \"FieldValueRequired\"",
                        go_quote(reason)
                    ));
                }
            }
            if let Some(fp) = r.get("fieldPath").and_then(Value::as_str) {
                if !fp.is_empty() && !valid_field_path(fp, s) {
                    self.schema_errs.push(format!("{at}.fieldPath: Invalid value: {}: fieldPath must be a valid path", go_quote(fp)));
                }
            }
        }
        if !rules.is_empty() {
            self.rules.push(RuleSite {
                path: path.to_string(),
                schema: s.clone(),
                resource_root,
                cardinality,
                uncorrelatable: uncorrelatable.map(str::to_string),
            });
        }
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            for (k, p) in props {
                self.walk(p, &format!("{path}.properties[{k}]"), cardinality, uncorrelatable, flag(p, "x-kubernetes-embedded-resource"));
            }
        }
        if let Some(a) = s.get("additionalProperties").filter(|a| a.is_object()) {
            let card = cardinality.zip(s.get("maxProperties").and_then(Value::as_u64)).map(|(c, m)| c.saturating_mul(m));
            self.walk(a, &format!("{path}.additionalProperties"), card, uncorrelatable, flag(a, "x-kubernetes-embedded-resource"));
        }
        if let Some(items) = s.get("items").filter(|i| i.is_object()) {
            let card = cardinality.zip(s.get("maxItems").and_then(Value::as_u64)).map(|(c, m)| c.saturating_mul(m));
            let correlatable = list_type == Some("map");
            let unc = if uncorrelatable.is_none() && !correlatable { Some(path) } else { uncorrelatable };
            self.walk(items, &format!("{path}.items"), card, unc, flag(items, "x-kubernetes-embedded-resource"));
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            for (i, sub) in s.get(key).and_then(Value::as_array).into_iter().flatten().enumerate() {
                // Value validations only; rules there are not structural.
                let mut nested = CrdCheck::default();
                nested.walk_patterns(sub, &format!("{path}.{key}[{i}]"));
                self.schema_errs.extend(nested.schema_errs);
            }
        }
    }

    fn walk_patterns(&mut self, s: &Value, path: &str) {
        if let Some(p) = s.get("pattern").and_then(Value::as_str) {
            if let Err(e) = cel::eval::valid_regex(p) {
                self.schema_errs.push(format!(
                    "{path}.pattern: Invalid value: {}: must be a valid regular expression, but isn't: error parsing regexp: {}: `{p}`",
                    go_quote(p),
                    e.trim_start_matches("error parsing regexp: ")
                ));
            }
        }
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            for (k, p) in props {
                self.walk_patterns(p, &format!("{path}.properties[{k}]"));
            }
        }
    }
}

fn type_suffix(path: &str) -> u32 {
    // The API server names rule types `selfType<random>`; a stable hash
    // keeps messages deterministic here.
    path.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619)) % 1_000_000_000
}

/// The variables of a rule at a schema node: `self`, and `oldSelf`
/// (optional for `optionalOldSelf`).
fn rule_vars(decl: &Rc<check::Decl>, optional_old: bool) -> BTreeMap<String, check::Var> {
    let mut vars = BTreeMap::new();
    vars.insert("self".to_string(), check::Var { ty: decl.ty(), decl: Some(decl.clone()) });
    let old_ty = if optional_old { check::Ty::Opt(Box::new(decl.ty())) } else { decl.ty() };
    vars.insert("oldSelf".to_string(), check::Var { ty: old_ty, decl: Some(decl.clone()) });
    vars
}

/// Errors for a CRD's schemas, in the API server's field-error form.
/// Rules that the old CRD already had are not re-checked for cost.
pub fn validate_crd(crd: &Value, old: Option<&Value>) -> Vec<String> {
    let mut existing: Vec<String> = Vec::new();
    if let Some(old) = old {
        for (_, s) in schema_roots(old) {
            collect_rules(&s, &mut existing);
        }
    }
    let mut errs = Vec::new();
    for (root_path, schema) in schema_roots(crd) {
        let mut c = CrdCheck::default();
        c.walk(&schema, &root_path, Some(1), None, true);
        if !c.schema_errs.is_empty() {
            errs.extend(c.schema_errs);
            continue;
        }
        let mut total: u64 = 0;
        let mut expensive: Vec<(String, u64)> = Vec::new();
        let mut observe = |path: String, cost: u64, total: &mut u64| {
            *total = total.saturating_add(cost);
            if cost < PER_CRD_COST_LIMIT / 100 {
                return;
            }
            let mut item = (path, cost);
            for slot in expensive.iter_mut() {
                if slot.1 < item.1 {
                    std::mem::swap(slot, &mut item);
                }
            }
            if expensive.len() < 4 {
                expensive.push(item);
            }
        };
        for site in &c.rules {
            let name = format!("selfType{}", type_suffix(&site.path));
            let Some(decl) = check::Decl::from_schema(&site.schema, site.resource_root, &name) else { continue };
            let max_card = site.cardinality.unwrap_or_else(|| (check::MAX_REQUEST_SIZE_BYTES as u64) / (decl.min_size as u64 + 1));
            for (i, r) in rules_of(&site.schema).iter().enumerate() {
                let at = format!("{}.x-kubernetes-validations[{i}]", site.path);
                let src = str_of(r, "rule");
                let preexisting = existing.iter().any(|e| e == src);
                let optional_old = flag(r, "optionalOldSelf");
                let vars = rule_vars(&decl, optional_old);
                let program = match cel::parse::parse(src) {
                    Ok(p) => p,
                    Err(e) => {
                        c.cel_errs.push(format!("{at}.rule: Invalid value: {}: compilation failed: {}", rule_repr(r), e.render(src)));
                        continue;
                    }
                };
                let checked = match check::check(&program, &vars) {
                    Ok(ch) => ch,
                    Err(e) => {
                        c.cel_errs.push(format!("{at}.rule: Invalid value: {}: compilation failed: {e}", rule_repr(r)));
                        continue;
                    }
                };
                let rule_cost = cost::estimate(&program, &checked).max.saturating_mul(max_card);
                if rule_cost > PER_RULE_COST_LIMIT && !preexisting {
                    c.cel_errs.push(format!("{at}.rule: Forbidden: {}", cost::exceeds_message("estimated rule cost", rule_cost, PER_RULE_COST_LIMIT)));
                }
                if !preexisting {
                    observe(format!("{at}.rule"), rule_cost, &mut total);
                }
                if !matches!(checked.result, check::Ty::Bool | check::Ty::Dyn) {
                    c.cel_errs.push(format!("{at}.rule: Invalid value: {}: cel expression must evaluate to a bool", rule_repr(r)));
                }
                let transition = uses_old_self(&program.root);
                if transition {
                    if let Some(within) = &site.uncorrelatable {
                        c.cel_errs.push(format!(
                            "{at}.rule: Invalid value: {}: oldSelf cannot be used on the uncorrelatable portion of the schema within {within}",
                            go_quote(src)
                        ));
                    }
                } else if optional_old {
                    c.cel_errs.push(format!("{at}.optionalOldSelf: Invalid value: true: may not be set if oldSelf is not used in rule"));
                }
                if let Some(me) = r.get("messageExpression").and_then(Value::as_str).filter(|m| !m.trim().is_empty()) {
                    let parsed = cel::parse::parse(me).map_err(|e| e.render(me)).and_then(|p| check::check(&p, &vars).map(|ch| (p, ch)));
                    match parsed {
                        Err(e) => c.cel_errs.push(format!("{at}.messageExpression: Invalid value: {}: messageExpression compilation failed: {e}", rule_repr(r))),
                        Ok((p, ch)) => {
                            if !matches!(ch.result, check::Ty::Str | check::Ty::Dyn) {
                                c.cel_errs.push(format!("{at}.messageExpression: Invalid value: {}: messageExpression must evaluate to a string", rule_repr(r)));
                            }
                            let me_cost = cost::estimate(&p, &ch).max.saturating_mul(max_card);
                            if me_cost > PER_RULE_COST_LIMIT && !preexisting {
                                c.cel_errs.push(format!(
                                    "{at}.messageExpression: Forbidden: {}",
                                    cost::exceeds_message("estimated messageExpression cost", me_cost, PER_RULE_COST_LIMIT)
                                ));
                            }
                            if !preexisting {
                                observe(format!("{at}.messageExpression"), me_cost, &mut total);
                            }
                        }
                    }
                }
            }
        }
        if total > PER_CRD_COST_LIMIT {
            for (path, _) in &expensive {
                c.cel_errs.push(format!("{path}: Forbidden: contributed to estimated rule cost total exceeding cost limit for entire OpenAPIv3 schema"));
            }
            c.cel_errs.push(format!(
                "{root_path}: Forbidden: {}",
                cost::exceeds_message("x-kubernetes-validations estimated rule cost total for entire OpenAPIv3 schema", total, PER_CRD_COST_LIMIT)
            ));
        }
        errs.extend(c.cel_errs);
    }
    errs
}

fn collect_rules(s: &Value, out: &mut Vec<String>) {
    match s {
        Value::Object(m) => {
            for r in rules_of(s) {
                out.push(str_of(r, "rule").to_string());
            }
            for (k, v) in m {
                if k != "x-kubernetes-validations" && k != "default" && k != "enum" && k != "example" {
                    collect_rules(v, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|v| collect_rules(v, out)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Custom resources

/// Drop nulls the schema does not allow (or replace them with the default).
pub fn prune_nulls(v: &mut Value, s: &Value) {
    match v {
        Value::Object(map) => {
            let props = s.get("properties").and_then(Value::as_object);
            let add = s.get("additionalProperties").filter(|a| a.is_object());
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let Some(ps) = props.and_then(|p| p.get(&k)).or(add) else { continue };
                if map[&k].is_null() && !flag(ps, "nullable") {
                    match ps.get("default") {
                        Some(d) => {
                            map.insert(k.clone(), d.clone());
                        }
                        None => {
                            map.remove(&k);
                            continue;
                        }
                    }
                }
                if let Some(child) = map.get_mut(&k) {
                    prune_nulls(child, ps);
                }
            }
        }
        Value::Array(items) => {
            if let Some(is) = s.get("items") {
                for item in items {
                    prune_nulls(item, is);
                }
            }
        }
        _ => {}
    }
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_string() } else { format!("{path}.{key}") }
}

fn shown_path(p: &str) -> String {
    if p.is_empty() { "<root>".to_string() } else { p.to_string() }
}

fn err_path(p: &str) -> String {
    if p.is_empty() { "<nil>".to_string() } else { p.to_string() }
}

/// A field error, and whether it stops CEL rules from running (type,
/// format and enum errors do).
struct FieldErr {
    msg: String,
    blocking: bool,
}

fn number_shown(n: &serde_json::Number) -> String {
    n.to_string()
}

fn go_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e21 { format!("{}", f as i64) } else { format!("{f}") }
}

/// The key fields of a list-map item, as the API server prints them.
fn map_key_repr(item: &Value, keys: &[&str]) -> String {
    let parts: Vec<String> = keys
        .iter()
        .filter_map(|k| item.get(*k).map(|v| format!("{}:{}", go_quote(k), if let Value::String(s) = v { go_quote(s) } else { v.to_string() })))
        .collect();
    format!("map[string]interface {{}}{{{}}}", parts.join(", "))
}

/// OpenAPI value validation of `v` against `s`, with ratcheting against the
/// correlated old value.
fn check_values(v: &Value, old: Option<&Value>, s: &Value, path: &str, root: bool, out: &mut Vec<FieldErr>) {
    if old == Some(v) {
        return;
    }
    let p = shown_path(path);
    let push = |out: &mut Vec<FieldErr>, msg: String, blocking: bool| out.push(FieldErr { msg, blocking });
    if flag(s, "x-kubernetes-int-or-string") {
        if !matches!(v, Value::String(_) | Value::Number(_) | Value::Null) {
            push(out, format!("{p}: Invalid value: \"{}\": {p} in body must be of type int-or-string: \"{}\"", type_of(v), type_of(v)), true);
            return;
        }
    }
    if v.is_null() {
        if !flag(s, "nullable") && s.get("type").is_some() && !root {
            let t = str_of(s, "type");
            push(out, format!("{p}: Invalid value: \"null\": {p} in body must be of type {t}: \"null\""), true);
        }
        return;
    }
    if let Some(t) = s.get("type").and_then(Value::as_str) {
        let got = type_of(v);
        if !(got == t || (t == "number" && got == "integer")) {
            push(out, format!("{p}: Invalid value: \"{got}\": {p} in body must be of type {t}: \"{got}\""), true);
            return;
        }
    }
    if let Some(values) = s.get("enum").and_then(Value::as_array) {
        if !values.contains(v) {
            let shown = |x: &Value| match x {
                Value::String(s) => go_quote(s),
                other => other.to_string(),
            };
            let supported: Vec<String> = values.iter().map(shown).collect();
            push(out, format!("{p}: Unsupported value: {}: supported values: {}", shown(v), supported.join(", ")), true);
        }
    }
    match v {
        Value::String(st) => {
            let len = st.chars().count() as u64;
            if let Some(max) = s.get("maxLength").and_then(Value::as_u64).filter(|m| len > *m) {
                push(out, format!("{p}: Too long: may not be more than {max} bytes"), false);
            }
            if let Some(min) = s.get("minLength").and_then(Value::as_u64).filter(|m| len < *m) {
                push(out, format!("{p}: Invalid value: {}: {p} in body should be at least {min} chars long", go_quote(st)), false);
            }
            if let Some(pat) = s.get("pattern").and_then(Value::as_str) {
                if let Ok(re) = regex::Regex::new(pat) {
                    if !re.is_match(st) {
                        push(out, format!("{p}: Invalid value: {}: {p} in body should match '{pat}'", go_quote(st)), false);
                    }
                }
            }
            if let Some(fmt) = s.get("format").and_then(Value::as_str) {
                if cel::lib::check_format(fmt, st) == Some(false) {
                    push(out, format!("{p}: Invalid value: {}: {p} in body must be of type {fmt}: {}", go_quote(st), go_quote(st)), true);
                }
            }
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            if let Some(max) = s.get("maximum").and_then(Value::as_f64) {
                if flag(s, "exclusiveMaximum") {
                    if x >= max {
                        push(out, format!("{p}: Invalid value: {}: {p} in body should be less than {}", number_shown(n), go_float(max)), false);
                    }
                } else if x > max {
                    push(out, format!("{p}: Invalid value: {}: {p} in body should be less than or equal to {}", number_shown(n), go_float(max)), false);
                }
            }
            if let Some(min) = s.get("minimum").and_then(Value::as_f64) {
                if flag(s, "exclusiveMinimum") {
                    if x <= min {
                        push(out, format!("{p}: Invalid value: {}: {p} in body should be greater than {}", number_shown(n), go_float(min)), false);
                    }
                } else if x < min {
                    push(out, format!("{p}: Invalid value: {}: {p} in body should be greater than or equal to {}", number_shown(n), go_float(min)), false);
                }
            }
            if let Some(m) = s.get("multipleOf").and_then(Value::as_f64).filter(|m| *m > 0.0) {
                let q = x / m;
                if (q - q.round()).abs() > 1e-9 {
                    push(out, format!("{p}: Invalid value: {}: {p} in body should be a multiple of {}", number_shown(n), go_float(m)), false);
                }
            }
        }
        Value::Object(map) => {
            for r in s.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                if !map.contains_key(r) {
                    push(out, format!("{}: Required value", join(path, r)), false);
                }
            }
            let props = s.get("properties").and_then(Value::as_object);
            let add = s.get("additionalProperties").filter(|a| a.is_object());
            for (k, child) in map {
                if root && (k == "apiVersion" || k == "kind" || k == "metadata") {
                    continue;
                }
                let child_schema = props.and_then(|ps| ps.get(k)).or(add);
                if let Some(cs) = child_schema {
                    let child_old = old.and_then(|o| o.get(k));
                    check_values(child, child_old, cs, &join(path, k), flag(cs, "x-kubernetes-embedded-resource"), out);
                }
            }
            if let Some(max) = s.get("maxProperties").and_then(Value::as_u64).filter(|m| map.len() as u64 > *m) {
                push(out, format!("{p}: Too many: {}: must have at most {max} items", map.len()), false);
            }
            if let Some(min) = s.get("minProperties").and_then(Value::as_u64).filter(|m| (map.len() as u64) < *m) {
                push(out, format!("{p}: Invalid value: {}: {p} in body should have at least {min} properties", map.len()), false);
            }
        }
        Value::Array(items) => {
            if let Some(max) = s.get("maxItems").and_then(Value::as_u64).filter(|m| items.len() as u64 > *m) {
                push(out, format!("{p}: Too many: {}: must have at most {max} items", items.len()), false);
            }
            if let Some(min) = s.get("minItems").and_then(Value::as_u64).filter(|m| (items.len() as u64) < *m) {
                push(out, format!("{p}: Invalid value: {}: {p} in body should have at least {min} items", items.len()), false);
            }
            if let Some(is) = s.get("items") {
                let olds = correlate_items(items, old, s);
                for (i, item) in items.iter().enumerate() {
                    check_values(item, olds[i], is, &format!("{path}[{i}]"), flag(is, "x-kubernetes-embedded-resource"), out);
                }
            }
            match s.get("x-kubernetes-list-type").and_then(Value::as_str) {
                Some("set") => {
                    for (i, item) in items.iter().enumerate() {
                        if items[..i].contains(item) {
                            let shown = match item {
                                Value::String(st) => go_quote(st),
                                other => other.to_string(),
                            };
                            push(out, format!("{path}[{i}]: Duplicate value: {shown}"), false);
                        }
                    }
                }
                Some("map") => {
                    let keys: Vec<&str> = s.get("x-kubernetes-list-map-keys").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    let key_of = |item: &Value| keys.iter().map(|k| item.get(*k).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>();
                    for (i, item) in items.iter().enumerate() {
                        if items[..i].iter().any(|prev| key_of(prev) == key_of(item)) {
                            push(out, format!("{path}[{i}]: Duplicate value: {}", map_key_repr(item, &keys)), false);
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    // Value-only combinators.
    let quoted = go_quote(&p);
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        let mut sub = Vec::new();
        for a in all {
            check_values(v, None, a, path, root, &mut sub);
        }
        if !sub.is_empty() {
            out.extend(sub);
            push(out, format!("<nil>: Invalid value: \"\": {quoted} must validate all the schemas (allOf)"), false);
        }
    }
    if let Some(any) = s.get("anyOf").and_then(Value::as_array) {
        let results: Vec<Vec<FieldErr>> = any.iter().map(|a| {
            let mut sub = Vec::new();
            check_values(v, None, a, path, root, &mut sub);
            sub
        }).collect();
        if !any.is_empty() && results.iter().all(|r| !r.is_empty()) {
            push(out, format!("<nil>: Invalid value: \"\": {quoted} must validate at least one schema (anyOf)"), false);
            out.extend(results.into_iter().next().unwrap());
        }
    }
    if let Some(one) = s.get("oneOf").and_then(Value::as_array) {
        let results: Vec<Vec<FieldErr>> = one.iter().map(|a| {
            let mut sub = Vec::new();
            check_values(v, None, a, path, root, &mut sub);
            sub
        }).collect();
        let valid = results.iter().filter(|r| r.is_empty()).count();
        if valid == 0 && !one.is_empty() {
            push(out, format!("<nil>: Invalid value: \"\": {quoted} must validate one and only one schema (oneOf). Found none valid"), false);
            out.extend(results.into_iter().next().unwrap());
        } else if valid > 1 {
            push(out, format!("<nil>: Invalid value: \"\": {quoted} must validate one and only one schema (oneOf). Found {valid} valid alternatives"), false);
        }
    }
    if let Some(not) = s.get("not").filter(|n| n.is_object()) {
        let mut sub = Vec::new();
        check_values(v, None, not, path, root, &mut sub);
        if sub.is_empty() {
            push(out, format!("<nil>: Invalid value: \"\": {quoted} must not validate the schema (not)"), false);
        }
    }
}

/// Old values of list items: by key for list-type map, none otherwise.
fn correlate_items<'a>(items: &[Value], old: Option<&'a Value>, s: &Value) -> Vec<Option<&'a Value>> {
    let olds = old.and_then(Value::as_array);
    if s.get("x-kubernetes-list-type").and_then(Value::as_str) != Some("map") {
        return vec![None; items.len()];
    }
    let keys: Vec<&str> = s.get("x-kubernetes-list-map-keys").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let key_of = |item: &Value| keys.iter().map(|k| item.get(*k).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>();
    items
        .iter()
        .map(|item| olds.and_then(|o| o.iter().find(|x| key_of(x) == key_of(item))))
        .collect()
}

/// The typed CEL value of `v` under schema `s` (number fields are doubles,
/// `byte`/`date`/`date-time`/`duration` strings are bytes, timestamps and
/// durations, object properties use their escaped names).
pub fn typed(v: &Value, s: &Value, resource_root: bool) -> Val {
    if flag(s, "x-kubernetes-int-or-string") {
        return Val::from_json(v);
    }
    match (v, str_of(s, "type")) {
        (Value::Null, _) => Val::Null,
        (Value::Number(n), "number") => Val::Double(n.as_f64().unwrap_or(0.0)),
        (Value::String(st), "string") => match str_of(s, "format") {
            "byte" => {
                use base64::Engine;
                match base64::engine::general_purpose::STANDARD.decode(st) {
                    Ok(b) => Val::Bytes(b),
                    Err(_) => Val::Str(st.clone()),
                }
            }
            "date-time" => cel::lib::parse_datetime(st).map(Val::Timestamp).unwrap_or_else(|| Val::Str(st.clone())),
            "date" => chrono::NaiveDate::parse_from_str(st, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| Val::Timestamp(d.and_utc()))
                .unwrap_or_else(|| Val::Str(st.clone())),
            "duration" => cel::eval::parse_go_duration(st).map(Val::Duration).unwrap_or_else(|_| Val::Str(st.clone())),
            _ => Val::Str(st.clone()),
        },
        (Value::Array(items), "array") => {
            let is = s.get("items").cloned().unwrap_or(Value::Null);
            let emb = flag(&is, "x-kubernetes-embedded-resource");
            Val::List(items.iter().map(|i| typed(i, &is, emb)).collect())
        }
        (Value::Object(map), "object") => {
            if let Some(add) = s.get("additionalProperties").filter(|a| a.is_object()) {
                let emb = flag(add, "x-kubernetes-embedded-resource");
                return Val::Map(map.iter().map(|(k, x)| (k.clone(), typed(x, add, emb))).collect());
            }
            let props = s.get("properties").and_then(Value::as_object);
            let mut out = BTreeMap::new();
            for (k, x) in map {
                if resource_root && (k == "apiVersion" || k == "kind") {
                    out.insert(k.clone(), Val::from_json(x));
                    continue;
                }
                if resource_root && k == "metadata" {
                    let mut meta = BTreeMap::new();
                    for f in ["name", "generateName"] {
                        if let Some(Value::String(n)) = x.get(f) {
                            meta.insert(f.to_string(), Val::Str(n.clone()));
                        }
                    }
                    out.insert("metadata".into(), Val::Map(meta));
                    continue;
                }
                let Some(ps) = props.and_then(|p| p.get(k)) else { continue };
                let Some(name) = check::escape(k) else { continue };
                out.insert(name, typed(x, ps, flag(ps, "x-kubernetes-embedded-resource")));
            }
            Val::Map(out)
        }
        _ => Val::from_json(v),
    }
}

/// The path a rule's `fieldPath` names, relative to the rule's node.
fn with_field_path(path: &str, fp: &str) -> String {
    let mut out = path.to_string();
    let mut rest = fp;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            out = join(&out, &r[..end]);
            rest = &r[end..];
        } else if let Some(r) = rest.strip_prefix("['") {
            let end = r.find("']").unwrap_or(r.len());
            out = format!("{out}[{}]", &r[..end]);
            rest = r.get(end + 2..).unwrap_or("");
        } else {
            break;
        }
    }
    out
}

/// The runtime CEL budget: per rule evaluation, and per request.
const PER_CALL_RUNTIME_LIMIT: u64 = 1_000_000;
const RUNTIME_BUDGET: u64 = 10_000_000;

/// Rule evaluation state across one object: the remaining budget, and
/// whether evaluation stopped.
struct Budget {
    remaining: u64,
    stopped: bool,
}

fn run_rules(v: &Value, old: Option<&Value>, s: &Value, path: &str, resource_root: bool, budget: &mut Budget, out: &mut Vec<String>) {
    let rules = rules_of(s);
    if rules.is_empty() || v.is_null() || budget.stopped {
        return;
    }
    let ty = if flag(s, "x-kubernetes-int-or-string") { "" } else { str_of(s, "type") };
    let self_val = typed(v, s, resource_root);
    let old_val = old.map(|o| typed(o, s, resource_root));
    for r in rules {
        let src = str_of(r, "rule");
        let Ok(program) = parse_cached(src) else { continue };
        let transition = uses_old_self(&program.root);
        let optional_old = flag(r, "optionalOldSelf");
        if transition && old_val.is_none() && !optional_old {
            continue;
        }
        if !transition && old == Some(v) {
            // Ratcheting: an unchanged value is not re-validated.
            continue;
        }
        let mut env = cel::Env { cost_limit: PER_CALL_RUNTIME_LIMIT.min(budget.remaining), ..Default::default() };
        env.vars.insert("self".into(), self_val.clone());
        let old_binding = match (&old_val, optional_old) {
            (Some(o), true) => Val::Optional(Some(Box::new(o.clone()))),
            (None, true) => Val::Optional(None),
            (Some(o), false) => o.clone(),
            (None, false) => Val::Null,
        };
        env.vars.insert("oldSelf".into(), old_binding);
        let message = str_of(r, "message").trim();
        let rule_str = if message.is_empty() { src.trim() } else { message };
        let at = err_path(path);
        let result = program.eval(&env);
        let spent = env.cost.get();
        if result.as_ref().err().map(String::as_str) == Some(cel::eval::COST_EXCEEDED) {
            budget.stopped = true;
            if budget.remaining <= PER_CALL_RUNTIME_LIMIT {
                out.push(format!("{at}: Invalid value: {}: validation failed due to running out of cost budget, no further validation rules will be run", go_quote(ty)));
            } else {
                out.push(format!(
                    "{at}: Invalid value: {}: '{}': no further validation rules will be run due to call cost exceeds limit for rule: {rule_str}",
                    go_quote(ty),
                    cel::eval::COST_EXCEEDED
                ));
            }
            return;
        }
        budget.remaining = budget.remaining.saturating_sub(spent);
        match result {
            Ok(Val::Bool(true)) => {}
            Ok(Val::Bool(false)) => {
                let mut msg = if message.is_empty() { format!("failed rule: {}", src.trim()) } else { message.to_string() };
                if let Some(me) = r.get("messageExpression").and_then(Value::as_str).filter(|m| !m.trim().is_empty()) {
                    if let Ok(Val::Str(m)) = parse_cached(me).and_then(|p| p.eval(&env)) {
                        if !m.trim().is_empty() && !m.contains('\n') {
                            msg = m.trim().to_string();
                        }
                    }
                }
                let fp = str_of(r, "fieldPath");
                let target = if fp.is_empty() { at.clone() } else { err_path(&with_field_path(path, fp)) };
                out.push(match str_of(r, "reason") {
                    "FieldValueForbidden" => format!("{target}: Forbidden: {msg}"),
                    "FieldValueRequired" => format!("{target}: Required value: {msg}"),
                    "FieldValueDuplicate" => format!("{target}: Duplicate value: {}", go_quote(ty)),
                    _ => format!("{target}: Invalid value: {}: {msg}", go_quote(ty)),
                });
            }
            Ok(other) => out.push(format!("{at}: Invalid value: {}: rule did not evaluate to a bool (got {}) evaluating rule: {rule_str}", go_quote(ty), other.type_name())),
            Err(e) if e.starts_with("no such overload") => out.push(format!(
                "{at}: Invalid value: {}: '{e}': call arguments did not match a supported operator, function or macro signature for rule: {rule_str}",
                go_quote(ty)
            )),
            Err(e) => out.push(format!("{at}: Invalid value: {}: {e} evaluating rule: {rule_str}", go_quote(ty))),
        }
    }
}

fn walk_rules(v: &Value, old: Option<&Value>, s: &Value, path: &str, resource_root: bool, budget: &mut Budget, out: &mut Vec<String>) {
    run_rules(v, old, s, path, resource_root, budget, out);
    match v {
        Value::Object(map) => {
            let props = s.get("properties").and_then(Value::as_object);
            let add = s.get("additionalProperties").filter(|a| a.is_object());
            for (k, child) in map {
                if resource_root && (k == "apiVersion" || k == "kind" || k == "metadata") {
                    continue;
                }
                let Some(cs) = props.and_then(|ps| ps.get(k)).or(add) else { continue };
                let child_old = old.and_then(|o| o.get(k));
                let child_path = if props.and_then(|ps| ps.get(k)).is_some() { join(path, k) } else { format!("{path}[{k}]") };
                walk_rules(child, child_old, cs, &child_path, flag(cs, "x-kubernetes-embedded-resource"), budget, out);
            }
        }
        Value::Array(items) => {
            if let Some(is) = s.get("items") {
                let olds = correlate_items(items, old, s);
                for (i, item) in items.iter().enumerate() {
                    walk_rules(item, olds[i], is, &format!("{path}[{i}]"), flag(is, "x-kubernetes-embedded-resource"), budget, out);
                }
            }
        }
        _ => {}
    }
}

fn has_rules(s: &Value) -> bool {
    match s {
        Value::Object(m) => {
            !rules_of(s).is_empty()
                || ["properties", "additionalProperties", "items"].iter().any(|k| match m.get(*k) {
                    Some(Value::Object(props)) if *k == "properties" => props.values().any(has_rules),
                    Some(other) if *k != "properties" => has_rules(other),
                    _ => false,
                })
        }
        _ => false,
    }
}

/// Field errors for a custom resource (`old` on updates).
pub fn validate(obj: &Value, old: Option<&Value>, schema: &Value) -> Vec<String> {
    let mut field_errs = Vec::new();
    check_values(obj, old, schema, "", true, &mut field_errs);
    let blocking = field_errs.iter().any(|e| e.blocking);
    let mut errs: Vec<String> = field_errs.into_iter().map(|e| e.msg).collect();
    if has_rules(schema) {
        if blocking {
            errs.push("<nil>: Invalid value: \"null\": some validation rules were not checked because the object was invalid; correct the existing errors to complete validation".into());
        } else {
            let mut budget = Budget { remaining: RUNTIME_BUDGET, stopped: false };
            walk_rules(obj, old, schema, "", true, &mut budget, &mut errs);
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn crd(schema: Value) -> Value {
        json!({"spec": {"versions": [{"name": "v1", "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": schema}}}}]}})
    }

    #[test]
    fn crd_rule_checks() {
        let errs = validate_crd(
            &crd(json!({"type": "object", "x-kubernetes-validations": [
                {"rule": "self.foo > 0"}, {"rule": "self.replicas"}, {"rule": "self.replicas > 0", "messageExpression": "1"},
                {"rule": "self.replicas > 0", "optionalOldSelf": true},
            ], "properties": {"replicas": {"type": "integer"},
                "items": {"type": "array", "items": {"type": "string"}, "x-kubernetes-validations": [{"rule": "self.all(x, oldSelf.all(y, x == y))"}]}}})),
            None,
        );
        let all = errs.join("\n");
        assert!(all.contains("x-kubernetes-validations[0].rule: Invalid value: apiextensions.ValidationRule{Rule:\"self.foo > 0\", Message:\"\", MessageExpression:\"\", Reason:(*apiextensions.FieldValueErrorReason)(nil), FieldPath:\"\", OptionalOldSelf:(*bool)(nil)}: compilation failed: ERROR: <input>:1:5: undefined field 'foo'"), "{all}");
        assert!(all.contains("x-kubernetes-validations[1].rule: Invalid value: apiextensions.ValidationRule{Rule:\"self.replicas\""), "{all}");
        assert!(all.contains("cel expression must evaluate to a bool"), "{all}");
        assert!(all.contains("messageExpression must evaluate to a string"), "{all}");
        assert!(all.contains("x-kubernetes-validations[3].optionalOldSelf: Invalid value: true: may not be set if oldSelf is not used in rule"), "{all}");
        assert!(all.contains("properties[items].x-kubernetes-validations[0].rule: Forbidden: estimated rule cost exceeds budget by factor of more than 100x"), "{all}");
        assert!(all.contains("spec.validation.openAPIV3Schema: Forbidden: x-kubernetes-validations estimated rule cost total for entire OpenAPIv3 schema exceeds budget"), "{all}");
        assert!(!all.contains("uncorrelatable"), "{all}");
        let errs = validate_crd(&crd(json!({"type": "object", "properties": {"l": {"type": "array", "maxItems": 3, "x-kubernetes-list-type": "set",
            "items": {"type": "string", "maxLength": 3, "x-kubernetes-validations": [{"rule": "self == oldSelf"}]}}}})), None);
        assert_eq!(errs, vec!["spec.validation.openAPIV3Schema.properties[spec].properties[l].items.x-kubernetes-validations[0].rule: Invalid value: \"self == oldSelf\": oldSelf cannot be used on the uncorrelatable portion of the schema within spec.validation.openAPIV3Schema.properties[spec].properties[l]"]);

        let errs = validate_crd(&crd(json!({"type": "object", "properties": {"name": {"type": "string", "pattern": "(?=x)"}}})), None);
        assert!(errs[0].starts_with("spec.validation.openAPIV3Schema.properties[spec].properties[name].pattern: Invalid value: \"(?=x)\": must be a valid regular expression"), "{errs:?}");
    }

    #[test]
    fn object_validation() {
        let schema = json!({"type": "object", "x-kubernetes-validations": [{"rule": "self.metadata.name.startsWith('p')", "message": "name must start with p"}],
            "properties": {"spec": {"type": "object", "x-kubernetes-validations": [
                {"rule": "self.min <= self.max", "message": "min must be <= max"},
                {"rule": "!has(self.mode) || self.mode != 'bad'", "reason": "FieldValueForbidden", "fieldPath": ".mode", "message": "mode bad forbidden"},
                {"rule": "self.immutable == oldSelf.immutable", "message": "immutable is immutable"},
                {"rule": "self.mode != 'dup'", "reason": "FieldValueDuplicate"},
            ], "properties": {
                "min": {"type": "integer"}, "max": {"type": "integer"}, "mode": {"type": "string"}, "immutable": {"type": "string"},
                "code": {"type": "string", "pattern": "^[A-Z]{3}$"}, "host": {"type": "string", "format": "hostname"},
                "ttl": {"type": "string", "format": "duration", "x-kubernetes-validations": [{"rule": "self < duration('1h')"}]},
            }}}});
        let obj = json!({"metadata": {"name": "x"}, "spec": {"min": 5, "max": 1, "mode": "bad", "immutable": "a", "ttl": "2h"}});
        let errs = validate(&obj, None, &schema);
        assert_eq!(errs, vec![
            "<nil>: Invalid value: \"object\": name must start with p",
            "spec: Invalid value: \"object\": min must be <= max",
            "spec.mode: Forbidden: mode bad forbidden",
            "spec.ttl: Invalid value: \"string\": failed rule: self < duration('1h')",
        ]);
        let errs = validate(&json!({"metadata": {"name": "p"}, "spec": {"min": 1, "max": 1, "mode": "dup"}}), None, &schema);
        assert_eq!(errs, vec!["spec: Duplicate value: \"object\""]);
        let errs = validate(&json!({"metadata": {"name": "p"}, "spec": {"min": 1, "max": 2, "code": "abc", "host": "-bad-"}}), None, &schema);
        assert_eq!(errs, vec![
            "spec.code: Invalid value: \"abc\": spec.code in body should match '^[A-Z]{3}$'",
            "spec.host: Invalid value: \"-bad-\": spec.host in body must be of type hostname: \"-bad-\"",
            "<nil>: Invalid value: \"null\": some validation rules were not checked because the object was invalid; correct the existing errors to complete validation",
        ]);
        // Transition rules run on update; unchanged invalid values ratchet.
        let old = json!({"metadata": {"name": "p"}, "spec": {"min": 1, "max": 2, "mode": "m", "immutable": "a", "code": "abc"}});
        let new = json!({"metadata": {"name": "p"}, "spec": {"min": 1, "max": 3, "mode": "m", "immutable": "b", "code": "abc"}});
        assert_eq!(validate(&new, Some(&old), &schema), vec!["spec: Invalid value: \"object\": immutable is immutable"]);
        let new = json!({"metadata": {"name": "p"}, "spec": {"min": 1, "max": 3, "mode": "m", "immutable": "a", "code": "abc"}});
        assert!(validate(&new, Some(&old), &schema).is_empty());
    }
}

/// Every CRD in `$CRD_CORPUS` (JSON list or multi-document YAML files,
/// comma-separated) was accepted by a real API server, so must be here:
/// `CRD_CORPUS=a.json,b.yaml cargo test real_crds -- --ignored`.
#[cfg(test)]
#[test]
#[ignore]
fn real_crds() {
    use serde::Deserialize;
    let mut crds: Vec<Value> = Vec::new();
    for path in std::env::var("CRD_CORPUS").unwrap().split(',') {
        let text = std::fs::read_to_string(path).unwrap();
        if path.ends_with(".json") {
            let v: Value = serde_json::from_str(&text).unwrap();
            crds.extend(v["items"].as_array().cloned().unwrap_or_else(|| vec![v.clone()]));
        } else {
            for doc in serde_yaml::Deserializer::from_str(&text) {
                let v: Value = Value::deserialize(doc).unwrap();
                if v.get("kind").and_then(Value::as_str) == Some("CustomResourceDefinition") {
                    crds.push(v);
                }
            }
        }
    }
    let mut bad = 0;
    let started = std::time::Instant::now();
    for c in &crds {
        let errs = validate_crd(c, None);
        if !errs.is_empty() {
            bad += 1;
            eprintln!("{}:\n  {}", c["metadata"]["name"], errs.join("\n  "));
        }
    }
    eprintln!("{} CRDs, {bad} rejected, {:?}", crds.len(), started.elapsed());
    assert_eq!(bad, 0);
}

/// Cost calibration: `COST_PROBE=file.json` holds `[{"schema": .., "rule": ..}]`;
/// prints each rule's estimated cost at cardinality 1.
#[cfg(test)]
#[test]
#[ignore]
fn cost_probe() {
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(std::env::var("COST_PROBE").unwrap()).unwrap()).unwrap();
    let mut out = Vec::new();
    for c in cases {
        let decl = check::Decl::from_schema(&c["schema"], false, "selfType1").unwrap();
        let src = c["rule"].as_str().unwrap();
        let p = cel::parse::parse(src).unwrap();
        let cost = match check::check(&p, &rule_vars(&decl, false)) {
            Ok(ch) => cost::estimate(&p, &ch).max as i128,
            Err(e) => {
                eprintln!("{src}: {e}");
                -1
            }
        };
        out.push(cost);
    }
    std::fs::write(std::env::var("COST_OUT").unwrap(), serde_json::to_string(&out).unwrap()).unwrap();
}

#[cfg(test)]
mod runtime_cost_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_cost_limit() {
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object", "x-kubernetes-validations": [
            {"rule": "self.l.all(x, self.l.all(y, y == x))"}, {"rule": "self.n > 0"}],
            "properties": {"n": {"type": "integer"}, "l": {"type": "array", "maxItems": 5000, "items": {"type": "string", "maxLength": 20}}}}}});
        let l: Vec<String> = vec!["a".repeat(20); 725];
        let errs = validate(&json!({"spec": {"n": 0, "l": l}}), None, &schema);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("no further validation rules will be run due to call cost exceeds limit for rule: self.l.all"), "{errs:?}");
    }
}
