//! YAML for people: an object as kubectl prints it (managedFields left out
//! unless asked), syntax colors for a line, and a line diff (Myers) for
//! edits.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use serde_json::Value;

use super::uiconfig::Theme;

/// The object as YAML; without `metadata.managedFields` unless `managed`.
pub fn to_yaml(obj: &Value, managed: bool) -> String {
    let mut o = obj.clone();
    if !managed {
        if let Some(m) = o.get_mut("metadata").and_then(Value::as_object_mut) {
            m.remove("managedFields");
        }
    }
    serde_yaml::to_string(&o).unwrap_or_else(|e| format!("# cannot print it as YAML: {e}\n"))
}

/// What colors a YAML line gets: keys, scalars by type, comments.
pub fn highlight(line: &str, t: &Theme) -> Vec<Span<'static>> {
    let key_style = t.accent.remove_modifier(Modifier::BOLD);
    let mut out = Vec::new();
    let indent = line.len() - line.trim_start().len();
    let mut rest = &line[indent..];
    out.push(Span::raw(line[..indent].to_string()));
    if rest.starts_with('#') {
        out.push(Span::styled(rest.to_string(), t.dim));
        return out;
    }
    if let Some(r) = rest.strip_prefix("- ") {
        out.push(Span::styled("- ", t.dim));
        rest = r;
    } else if rest == "-" {
        out.push(Span::styled("-", t.dim));
        return out;
    }
    // `key: value` (not inside a quoted scalar).
    let split = if rest.starts_with('"') || rest.starts_with('\'') { None } else { rest.find(": ").or_else(|| rest.strip_suffix(':').map(|r| r.len())) };
    match split {
        Some(i) => {
            out.push(Span::styled(rest[..i].to_string(), key_style));
            out.push(Span::styled(":", t.dim));
            let value = rest.get(i + 1..).unwrap_or("");
            if !value.is_empty() {
                out.push(Span::raw(" "));
                out.push(scalar(value.trim_start(), t));
            }
        }
        None => out.push(scalar(rest, t)),
    }
    out
}

fn scalar(v: &str, t: &Theme) -> Span<'static> {
    let style = match v {
        "true" | "false" | "null" | "~" => t.info,
        _ if v.parse::<f64>().is_ok() => t.info,
        _ if v.starts_with('"') || v.starts_with('\'') => t.ok,
        "|" | "|-" | ">" | ">-" | "{}" | "[]" => t.dim,
        _ => Style::new(),
    };
    Span::styled(v.to_string(), style)
}

/// A line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Same(String),
    Del(String),
    Add(String),
}

/// The shortest edit script from `a` to `b` (Myers' O(ND) algorithm).
pub fn diff(a: &[&str], b: &[&str]) -> Vec<Edit> {
    // Common prefix and suffix first: edits are usually small.
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out: Vec<Edit> = a[..pre].iter().map(|s| Edit::Same(s.to_string())).collect();
    out.extend(myers(am, bm));
    out.extend(a[a.len() - suf..].iter().map(|s| Edit::Same(s.to_string())));
    out
}

fn myers(a: &[&str], b: &[&str]) -> Vec<Edit> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    if max == 0 {
        return Vec::new();
    }
    let off = max as isize;
    let mut v = vec![0isize; 2 * max + 2];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    'outer: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let i = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) { v[i + 1] } else { v[i - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            if x >= n && y >= m {
                break 'outer;
            }
            k += 2;
        }
    }
    // Walk back through the saved frontiers.
    let mut edits = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let i = (k + off) as usize;
        let prev_k = if k == -d || (k != d && v[i - 1] < v[i + 1]) { k + 1 } else { k - 1 };
        let prev_x = v[(prev_k + off) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            edits.push(Edit::Same(a[(x - 1) as usize].to_string()));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                edits.push(Edit::Add(b[(y - 1) as usize].to_string()));
            } else {
                edits.push(Edit::Del(a[(x - 1) as usize].to_string()));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    edits.reverse();
    edits
}

/// A unified diff with `context` lines around changes; empty when equal.
pub fn unified(a: &str, b: &str, context: usize) -> Vec<String> {
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let edits = diff(&al, &bl);
    let changed: Vec<usize> = edits.iter().enumerate().filter(|(_, e)| !matches!(e, Edit::Same(_))).map(|(i, _)| i).collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut shown_to = 0usize;
    let mut i = 0;
    while i < changed.len() {
        // A hunk: changes closer than 2*context lines join.
        let start = changed[i].saturating_sub(context).max(shown_to);
        let mut end = changed[i];
        while i + 1 < changed.len() && changed[i + 1] <= end + 2 * context + 1 {
            i += 1;
            end = changed[i];
        }
        let end = (end + context + 1).min(edits.len());
        let (mut la, mut lb) = (1, 1);
        for e in &edits[..start] {
            match e {
                Edit::Same(_) => {
                    la += 1;
                    lb += 1;
                }
                Edit::Del(_) => la += 1,
                Edit::Add(_) => lb += 1,
            }
        }
        let na = edits[start..end].iter().filter(|e| !matches!(e, Edit::Add(_))).count();
        let nb = edits[start..end].iter().filter(|e| !matches!(e, Edit::Del(_))).count();
        out.push(format!("@@ -{la},{na} +{lb},{nb} @@"));
        for e in &edits[start..end] {
            out.push(match e {
                Edit::Same(s) => format!(" {s}"),
                Edit::Del(s) => format!("-{s}"),
                Edit::Add(s) => format!("+{s}"),
            });
        }
        shown_to = end;
        i += 1;
    }
    out
}

pub fn diff_style(line: &str, t: &Theme) -> Style {
    if line.starts_with('+') {
        t.ok
    } else if line.starts_with('-') {
        t.bad
    } else if line.starts_with("@@") {
        t.accent
    } else {
        Style::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn yaml_without_managed_fields() {
        let o = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "a", "managedFields": [{"manager": "kubectl"}]}});
        let y = to_yaml(&o, false);
        assert!(y.contains("name: a") && !y.contains("managedFields"), "{y}");
        assert!(to_yaml(&o, true).contains("managedFields"));
    }

    #[test]
    fn highlights_keys_and_scalars() {
        let t = Theme::default();
        let spans = highlight("  - name: web", &t);
        let text: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, vec!["  ", "- ", "name", ":", " ", "web"]);
        let spans = highlight("replicas: 3", &t);
        assert_eq!(spans.last().unwrap().style, t.info);
        assert_eq!(highlight("# c", &t)[1].style, t.dim);
    }

    #[test]
    fn diffs_are_minimal_and_unified() {
        let a = ["a", "b", "c", "d"];
        let b = ["a", "x", "c", "d", "e"];
        let d = diff(&a, &b);
        assert_eq!(d, vec![Edit::Same("a".into()), Edit::Del("b".into()), Edit::Add("x".into()), Edit::Same("c".into()), Edit::Same("d".into()), Edit::Add("e".into())]);
        let u = unified("a\nb\nc\nd\n", "a\nx\nc\nd\ne\n", 1);
        assert_eq!(u, vec!["@@ -1,4 +1,5 @@", " a", "-b", "+x", " c", " d", "+e"]);
        assert!(unified("same\n", "same\n", 3).is_empty());
        // Far-apart changes make separate hunks.
        let a: Vec<String> = (0..30).map(|i| i.to_string()).collect();
        let mut b = a.clone();
        b[2] = "two".into();
        b[25] = "x".into();
        let u = unified(&a.join("\n"), &b.join("\n"), 2);
        assert_eq!(u.iter().filter(|l| l.starts_with("@@")).count(), 2, "{u:?}");
        assert_eq!(diff(&[], &["n"]), vec![Edit::Add("n".into())]);
        assert_eq!(diff(&["o"], &[]), vec![Edit::Del("o".into())]);
    }
}
