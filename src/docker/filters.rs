//! The `filters` query parameter: JSON, either `{"label": {"a=b": true}}`
//! or the older `{"label": ["a=b"]}`.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct Filters(pub HashMap<String, Vec<String>>);

impl Filters {
    pub fn parse(raw: Option<&str>) -> Self {
        let mut out = HashMap::new();
        let Some(raw) = raw.filter(|r| !r.is_empty()) else { return Self(out) };
        let Ok(v) = serde_json::from_str::<Value>(raw) else { return Self(out) };
        if let Some(map) = v.as_object() {
            for (k, vals) in map {
                let list: Vec<String> = match vals {
                    Value::Object(m) => m.iter().filter(|(_, on)| on.as_bool().unwrap_or(true)).map(|(s, _)| s.clone()).collect(),
                    Value::Array(a) => a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect(),
                    Value::String(s) => vec![s.clone()],
                    _ => Vec::new(),
                };
                out.insert(k.clone(), list);
            }
        }
        Self(out)
    }

    pub fn get(&self, key: &str) -> &[String] {
        self.0.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.get(key).is_some_and(|v| !v.is_empty())
    }

    /// Every `label` filter (`key`, `key=value`, `key!=value`) matches.
    pub fn labels_match(&self, key: &str, labels: &BTreeMap<String, String>) -> bool {
        self.get(key).iter().all(|f| {
            if let Some((k, v)) = f.split_once("!=") {
                labels.get(k).is_none_or(|have| have != v)
            } else if let Some((k, v)) = f.split_once('=') {
                labels.get(k).is_some_and(|have| have == v)
            } else {
                labels.contains_key(f.as_str())
            }
        })
    }

    /// Some value of `key` satisfies `pred` (or there is no such filter).
    pub fn any(&self, key: &str, pred: impl Fn(&str) -> bool) -> bool {
        let v = self.get(key);
        v.is_empty() || v.iter().any(|f| pred(f))
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key).first().map(|v| matches!(v.as_str(), "true" | "1"))
    }
}

/// `filters` for labels given as a JSON object of strings.
pub fn labels_of(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_forms_and_matches_labels() {
        let f = Filters::parse(Some(r#"{"label":{"com.docker.compose.project=demo":true,"x":true},"status":["running"]}"#));
        let mut labels = BTreeMap::new();
        labels.insert("com.docker.compose.project".to_string(), "demo".to_string());
        assert!(!f.labels_match("label", &labels));
        labels.insert("x".into(), "1".into());
        assert!(f.labels_match("label", &labels));
        assert_eq!(f.get("status"), ["running"]);
        assert!(f.any("name", |_| false));
        let f = Filters::parse(Some(r#"{"label":["a!=b"]}"#));
        assert!(f.labels_match("label", &BTreeMap::new()));
    }
}
