//! `/` filters: a pattern over a row's name and cells (substring, regex or
//! fuzzy), `!` in front to invert it, a label selector (`-l app=web,tier!=db`,
//! matched here against the rows' labels) and a field selector
//! (`-f status.phase=Running`, sent to the API server).

use regex::Regex;

use super::table::Row;
use crate::kube::selector::LabelSelector;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Substring,
    Regex,
    Fuzzy,
}

impl Mode {
    pub fn next(self) -> Mode {
        match self {
            Mode::Substring => Mode::Regex,
            Mode::Regex => Mode::Fuzzy,
            Mode::Fuzzy => Mode::Substring,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Substring => "substr",
            Mode::Regex => "regex",
            Mode::Fuzzy => "fuzzy",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// What was typed.
    pub raw: String,
    pub mode: Mode,
    pub pattern: String,
    pub inverse: bool,
    pub labels: Option<(String, LabelSelector)>,
    pub fields: Option<String>,
    lower: String,
    regex: Option<Regex>,
    /// A selector or regex that does not parse (the rest still applies).
    pub error: Option<String>,
}

impl PartialEq for Filter {
    fn eq(&self, o: &Filter) -> bool {
        self.raw == o.raw && self.mode == o.mode
    }
}

/// Split on whitespace, keeping quoted parts together.
pub fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl Filter {
    pub fn parse(raw: &str, mode: Mode) -> Filter {
        let mut f = Filter { raw: raw.to_string(), mode, ..Default::default() };
        let mut rest = Vec::new();
        let toks = words(raw);
        let mut i = 0;
        while i < toks.len() {
            let t = toks[i].as_str();
            let (flag, inline) = match t {
                "-l" | "--selector" => ("l", None),
                "-f" | "--field-selector" => ("f", None),
                _ if t.starts_with("-l") && t.len() > 2 => ("l", Some(&t[2..])),
                _ if t.starts_with("--selector=") => ("l", Some(&t[11..])),
                _ if t.starts_with("--field-selector=") => ("f", Some(&t[17..])),
                _ => {
                    rest.push(t.to_string());
                    i += 1;
                    continue;
                }
            };
            let mut value = match inline {
                Some(v) => v.to_string(),
                None => {
                    i += 1;
                    toks.get(i).cloned().unwrap_or_default()
                }
            };
            i += 1;
            // `tier in (a, b)` spans words.
            while let Some(next) = toks.get(i) {
                let open = value.matches('(').count() > value.matches(')').count();
                if open || matches!(next.as_str(), "in" | "notin") || next.starts_with('(') {
                    value.push(' ');
                    value.push_str(next);
                    i += 1;
                } else {
                    break;
                }
            }
            if flag == "l" {
                match LabelSelector::parse(&value) {
                    Ok(s) if !value.is_empty() => f.labels = Some((value, s)),
                    Ok(_) => {}
                    Err(e) => f.error = Some(format!("label selector: {e}")),
                }
            } else if !value.is_empty() {
                f.fields = Some(value);
            }
        }
        let mut pattern = rest.join(" ");
        if let Some(p) = pattern.strip_prefix('!') {
            f.inverse = true;
            pattern = p.to_string();
        }
        f.lower = pattern.to_lowercase();
        if mode == Mode::Regex && !pattern.is_empty() {
            match Regex::new(&format!("(?i){pattern}")) {
                Ok(r) => f.regex = Some(r),
                Err(_) => f.error = Some("not a valid regex: matching it as text".into()),
            }
        }
        f.pattern = pattern;
        f
    }

    pub fn is_empty(&self) -> bool {
        self.pattern.is_empty() && self.labels.is_none() && self.fields.is_none()
    }

    /// Whether a row passes; with a score for fuzzy matches (higher is
    /// better), else 0.
    pub fn matches(&self, row: &Row) -> Option<i64> {
        if let Some((_, sel)) = &self.labels {
            if !sel.matches(&row.labels) {
                return None;
            }
        }
        if self.pattern.is_empty() {
            return Some(0);
        }
        let hit = match (self.mode, &self.regex) {
            (Mode::Fuzzy, _) => fuzzy(&self.lower, &row.name.to_lowercase()),
            (Mode::Regex, Some(r)) => r.is_match(&row.haystack).then_some(0),
            _ => row.haystack.contains(&self.lower).then_some(0),
        };
        match (hit, self.inverse) {
            (Some(s), false) => Some(s),
            (None, true) => Some(0),
            _ => None,
        }
    }

    /// Whether a line of text matches the pattern (logs, pagers).
    pub fn matches_text(&self, text: &str) -> bool {
        if self.pattern.is_empty() {
            return true;
        }
        let hit = match (&self.regex, self.mode) {
            (Some(r), _) => r.is_match(text),
            (None, Mode::Fuzzy) => fuzzy(&self.lower, &text.to_lowercase()).is_some(),
            _ => text.to_lowercase().contains(&self.lower),
        };
        hit != self.inverse
    }

    /// Byte ranges of the matches in `text` (to highlight them).
    pub fn spans(&self, text: &str) -> Vec<(usize, usize)> {
        if self.pattern.is_empty() || self.inverse {
            return Vec::new();
        }
        match (&self.regex, self.mode) {
            (Some(r), _) => r.find_iter(text).filter(|m| m.end() > m.start()).map(|m| (m.start(), m.end())).collect(),
            (None, Mode::Fuzzy) => Vec::new(),
            _ => {
                // Case-insensitive, on ASCII-lowercased bytes (same offsets).
                let hay = text.to_ascii_lowercase();
                let needle = self.lower.as_str();
                if needle.is_empty() || needle.len() != self.pattern.len() {
                    return Vec::new();
                }
                hay.match_indices(needle).map(|(i, m)| (i, i + m.len())).collect()
            }
        }
    }
}

/// A fuzzy (subsequence) match: every character of `needle` in order;
/// consecutive runs and matches at word starts score higher.
pub fn fuzzy(needle: &str, hay: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    let h: Vec<char> = hay.chars().collect();
    let mut score = 0i64;
    let mut hi = 0;
    let mut prev: Option<usize> = None;
    for n in needle.chars() {
        let pos = (hi..h.len()).find(|&i| h[i] == n)?;
        score += 1;
        if prev.is_some_and(|p| p + 1 == pos) {
            score += 5;
        }
        if pos == 0 || matches!(h[pos - 1], '-' | '_' | '.' | '/' | ' ') {
            score += 3;
        }
        prev = Some(pos);
        hi = pos + 1;
    }
    // Shorter names with the same letters first.
    Some(score * 100 - h.len() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, cells: &[&str], labels: serde_json::Value) -> Row {
        let mut hay = name.to_lowercase();
        for c in cells {
            hay.push(' ');
            hay.push_str(&c.to_lowercase());
        }
        Row { name: name.into(), cells: cells.iter().map(|s| s.to_string()).collect(), labels: json!({"metadata": {"labels": labels}}), haystack: hay, ..Default::default() }
    }

    #[test]
    fn patterns_selectors_and_inverse() {
        let web = row("web-7d9", &["Running"], json!({"app": "web", "tier": "front"}));
        let db = row("db-0", &["CrashLoopBackOff"], json!({"app": "db", "tier": "db"}));
        let f = Filter::parse("crash", Mode::Substring);
        assert!(f.matches(&web).is_none() && f.matches(&db).is_some());
        let f = Filter::parse("!crash", Mode::Substring);
        assert!(f.matches(&web).is_some() && f.matches(&db).is_none());
        let f = Filter::parse("^w.b", Mode::Regex);
        assert!(f.matches(&web).is_some() && f.matches(&db).is_none());
        let f = Filter::parse("-l app=web,tier!=db", Mode::Substring);
        assert!(f.matches(&web).is_some() && f.matches(&db).is_none());
        assert_eq!(f.labels.as_ref().unwrap().0, "app=web,tier!=db");
        let f = Filter::parse("run -l tier in (front,db) -f status.phase=Running", Mode::Substring);
        assert_eq!(f.pattern, "run");
        assert_eq!(f.fields.as_deref(), Some("status.phase=Running"));
        assert!(f.matches(&web).is_some() && f.matches(&db).is_none());
        let f = Filter::parse("-l 'tier in x'", Mode::Substring);
        assert!(f.error.is_some());
        let f = Filter::parse("([", Mode::Regex);
        assert!(f.error.is_some() && f.matches(&row("x([y", &[], json!({}))).is_some());
    }

    #[test]
    fn fuzzy_ranks_tighter_matches_first() {
        let f = Filter::parse("web", Mode::Fuzzy);
        let a = f.matches(&row("web-1", &[], json!({}))).unwrap();
        let b = f.matches(&row("wide-error-box", &[], json!({}))).unwrap();
        assert!(f.matches(&row("db", &[], json!({}))).is_none());
        assert!(a > b, "{a} {b}");
        assert!(fuzzy("ngx", "nginx").is_some());
    }

    #[test]
    fn highlights_matches() {
        let f = Filter::parse("err", Mode::Substring);
        assert_eq!(f.spans("an ERRor and err"), vec![(3, 6), (13, 16)]);
        assert!(f.matches_text("ERROR"));
        let f = Filter::parse("e.r", Mode::Regex);
        assert_eq!(f.spans("ear"), vec![(0, 3)]);
        assert_eq!(words(r#"get pods -l "app in (a, b)" x"#), vec!["get", "pods", "-l", "app in (a, b)", "x"]);
    }
}
