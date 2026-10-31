//! Label and field selectors, as accepted by `?labelSelector=` and
//! `?fieldSelector=` on list and watch requests.

use anyhow::{Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LabelRequirement {
    Equals(String, String),
    NotEquals(String, String),
    In(String, Vec<String>),
    NotIn(String, Vec<String>),
    Exists(String),
    DoesNotExist(String),
}

/// A parsed label selector. The empty selector matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelSelector(Vec<LabelRequirement>);

impl LabelSelector {
    pub fn parse(raw: &str) -> Result<Self> {
        let mut requirements = Vec::new();
        for term in split_top_level(raw) {
            let term = term.trim();
            if term.is_empty() {
                continue;
            }
            requirements.push(parse_label_term(term)?);
        }
        Ok(Self(requirements))
    }

    /// A structured `metav1.LabelSelector` (`matchLabels` plus
    /// `matchExpressions`). `None` (or `null`) selects nothing, the empty
    /// object everything, as in Kubernetes.
    pub fn from_object(selector: Option<&Value>) -> Option<Self> {
        let selector = selector.filter(|s| !s.is_null())?;
        let mut requirements = Vec::new();
        if let Some(labels) = selector.get("matchLabels").and_then(Value::as_object) {
            for (k, v) in labels {
                requirements.push(LabelRequirement::Equals(k.clone(), v.as_str().unwrap_or_default().to_string()));
            }
        }
        for expr in selector.get("matchExpressions").and_then(Value::as_array).into_iter().flatten() {
            let key = expr.get("key").and_then(Value::as_str).unwrap_or_default().to_string();
            let values: Vec<String> = expr
                .get("values")
                .and_then(Value::as_array)
                .map(|vs| vs.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            requirements.push(match expr.get("operator").and_then(Value::as_str).unwrap_or_default() {
                "In" => LabelRequirement::In(key, values),
                "NotIn" => LabelRequirement::NotIn(key, values),
                "Exists" => LabelRequirement::Exists(key),
                "DoesNotExist" => LabelRequirement::DoesNotExist(key),
                _ => return None,
            });
        }
        Some(Self(requirements))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn matches(&self, object: &Value) -> bool {
        self.matches_labels(object.pointer("/metadata/labels"))
    }

    /// [`LabelSelector::matches`] given only the object's `metadata.labels`.
    pub fn matches_labels(&self, labels: Option<&Value>) -> bool {
        let get = |key: &str| labels.and_then(|l| l.get(key)).and_then(Value::as_str);
        self.0.iter().all(|req| match req {
            LabelRequirement::Equals(k, v) => get(k) == Some(v.as_str()),
            LabelRequirement::NotEquals(k, v) => get(k) != Some(v.as_str()),
            LabelRequirement::In(k, vs) => get(k).is_some_and(|got| vs.iter().any(|v| v == got)),
            LabelRequirement::NotIn(k, vs) => !get(k).is_some_and(|got| vs.iter().any(|v| v == got)),
            LabelRequirement::Exists(k) => get(k).is_some(),
            LabelRequirement::DoesNotExist(k) => get(k).is_none(),
        })
    }
}

/// Split on commas that are not inside a `(...)` value set.
fn split_top_level(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in raw.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&raw[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&raw[start..]);
    parts
}

fn parse_label_term(term: &str) -> Result<LabelRequirement> {
    if let Some(key) = term.strip_prefix('!') {
        return Ok(LabelRequirement::DoesNotExist(key.trim().to_string()));
    }
    for (op, notin) in [(" notin ", true), (" in ", false)] {
        if let Some(idx) = term.find(op) {
            let key = term[..idx].trim().to_string();
            let set = term[idx + op.len()..].trim();
            let Some(inner) = set.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
                bail!("invalid label selector term `{term}`: expected a (value, ...) set");
            };
            let values = inner
                .split(',')
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect();
            return Ok(if notin {
                LabelRequirement::NotIn(key, values)
            } else {
                LabelRequirement::In(key, values)
            });
        }
    }
    if let Some((k, v)) = term.split_once("!=") {
        return Ok(LabelRequirement::NotEquals(k.trim().into(), v.trim().into()));
    }
    if let Some((k, v)) = term.split_once("==") {
        return Ok(LabelRequirement::Equals(k.trim().into(), v.trim().into()));
    }
    if let Some((k, v)) = term.split_once('=') {
        return Ok(LabelRequirement::Equals(k.trim().into(), v.trim().into()));
    }
    Ok(LabelRequirement::Exists(term.to_string()))
}

/// A parsed field selector: `path=value` / `path==value` / `path!=value` terms
/// over dotted JSON paths such as `metadata.name` or `status.phase`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldSelector(Vec<(Vec<String>, String, bool)>);

impl FieldSelector {
    pub fn parse(raw: &str) -> Result<Self> {
        let mut terms = Vec::new();
        for term in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let (path, value, negate) = if let Some((p, v)) = term.split_once("!=") {
                (p, v, true)
            } else if let Some((p, v)) = term.split_once("==") {
                (p, v, false)
            } else if let Some((p, v)) = term.split_once('=') {
                (p, v, false)
            } else {
                bail!("invalid field selector term `{term}`");
            };
            let path = path.trim().split('.').map(str::to_string).collect();
            terms.push((path, value.trim().to_string(), negate));
        }
        Ok(Self(terms))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The `metadata.name=` value, if the selector pins one; lets callers
    /// turn a list into a single lookup.
    pub fn exact_name(&self) -> Option<&str> {
        self.0
            .iter()
            .find(|(path, _, negate)| !negate && path == &["metadata", "name"])
            .map(|(_, v, _)| v.as_str())
    }

    pub fn matches(&self, object: &Value) -> bool {
        self.0.iter().all(|(path, want, negate)| {
            let mut cur = Some(object);
            for seg in path {
                cur = cur.and_then(|v| v.get(seg));
            }
            // core/v1 Events select on `source` as the source component.
            if let Some(Value::Object(m)) = cur {
                if path.len() == 1 && path[0] == "source" {
                    cur = m.get("component");
                }
            }
            let got = match cur {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Bool(b)) => b.to_string(),
                Some(Value::Number(n)) => n.to_string(),
                // Boolean fields the API server formats with strconv.FormatBool,
                // so an absent value selects as "false".
                _ if matches!(path_str(path).as_str(), "spec.unschedulable" | "spec.hostNetwork") => {
                    "false".into()
                }
                _ => String::new(),
            };
            (got == *want) != *negate
        })
    }
}

fn path_str(path: &[String]) -> String {
    path.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(labels: Value) -> Value {
        json!({"metadata": {"name": "a", "labels": labels}, "status": {"phase": "Running"}})
    }

    #[test]
    fn label_selector_operators() {
        let o = obj(json!({"app": "web", "tier": "front"}));
        let m = |s: &str| LabelSelector::parse(s).unwrap().matches(&o);
        assert!(m(""));
        assert!(m("app=web"));
        assert!(m("app==web,tier"));
        assert!(!m("app!=web"));
        assert!(m("tier in (front, back)"));
        assert!(!m("tier notin (front)"));
        assert!(m("!missing"));
        assert!(!m("missing"));
        assert!(m("app=web,tier in (x,front)"));
    }

    #[test]
    fn field_selector_paths() {
        let o = obj(json!({}));
        let m = |s: &str| FieldSelector::parse(s).unwrap().matches(&o);
        assert!(m("metadata.name=a"));
        assert!(!m("metadata.name!=a"));
        assert!(m("status.phase==Running"));
        assert!(!m("status.phase=Pending"));
        assert!(m("spec.unschedulable=false"));
        assert_eq!(
            FieldSelector::parse("metadata.name=x").unwrap().exact_name(),
            Some("x")
        );
    }
}
