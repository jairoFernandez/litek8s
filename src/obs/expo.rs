//! The Prometheus text exposition format: parsing (litek8s's own
//! `/metrics`, ingested like any other source) and writing (`/federate`).

use std::fmt::Write as _;

use super::tsdb::Labels;

/// A parsed exposition: samples and `# TYPE`/`# HELP` per metric name.
#[derive(Default, Debug)]
pub struct Parsed {
    pub samples: Vec<(Labels, f64)>,
    /// (name, type, help)
    pub metadata: Vec<(String, String, String)>,
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_value(s: &str) -> Option<f64> {
    match s {
        "+Inf" | "Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => s.parse().ok(),
    }
}

/// Parse one sample line: `name{a="b",c="d"} value [timestamp]`.
fn parse_line(line: &str) -> Option<(Labels, f64)> {
    let (name, rest) = match line.find(['{', ' ']) {
        Some(i) => (&line[..i], &line[i..]),
        None => return None,
    };
    let mut labels: Labels = vec![("__name__".into(), name.to_string())];
    let rest = if let Some(body) = rest.strip_prefix('{') {
        let mut i = 0;
        let b = body.as_bytes();
        loop {
            while i < b.len() && (b[i] == b',' || b[i] == b' ') {
                i += 1;
            }
            if i >= b.len() {
                return None;
            }
            if b[i] == b'}' {
                i += 1;
                break;
            }
            let eq = body[i..].find('=')? + i;
            let key = body[i..eq].trim().to_string();
            if b.get(eq + 1) != Some(&b'"') {
                return None;
            }
            let mut j = eq + 2;
            while j < b.len() && b[j] != b'"' {
                if b[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            if j >= b.len() {
                return None;
            }
            labels.push((key, unescape(&body[eq + 2..j])));
            i = j + 1;
        }
        &body[i..]
    } else {
        rest
    };
    let value = parse_value(rest.split_whitespace().next()?)?;
    Some((labels, value))
}

pub fn parse(text: &str) -> Parsed {
    let mut p = Parsed::default();
    let mut help: std::collections::HashMap<String, String> = Default::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# HELP ") {
            if let Some((name, h)) = rest.split_once(' ') {
                help.insert(name.to_string(), h.to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            if let Some((name, t)) = rest.split_once(' ') {
                p.metadata.push((name.to_string(), t.trim().to_string(), String::new()));
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(s) = parse_line(line) {
            p.samples.push(s);
        }
    }
    for m in &mut p.metadata {
        if let Some(h) = help.get(&m.0) {
            m.2 = h.clone();
        }
    }
    p
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// One sample in the text format (labels sorted, `__name__` as the name).
pub fn write_sample(out: &mut String, labels: &Labels, v: f64, t_ms: Option<i64>) {
    let name = labels.iter().find(|(n, _)| n == "__name__").map(|(_, v)| v.as_str()).unwrap_or("");
    out.push_str(name);
    let rest: Vec<String> = labels.iter().filter(|(n, _)| n != "__name__").map(|(n, v)| format!("{n}=\"{}\"", escape(v))).collect();
    if !rest.is_empty() {
        out.push('{');
        out.push_str(&rest.join(","));
        out.push('}');
    }
    let _ = write!(out, " {}", super::promql::format_value(v));
    if let Some(t) = t_ms {
        let _ = write!(out, " {t}");
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_writes() {
        let text = "# HELP up Whether up.\n# TYPE up gauge\nup 1\nreq_total{code=\"200\",path=\"/a \\\"b\\\"\"} 3 1700000000000\nlat_bucket{le=\"+Inf\"} +Inf\nbad{x=1} 2\n";
        let p = parse(text);
        assert_eq!(p.samples.len(), 3);
        assert_eq!(p.metadata, vec![("up".into(), "gauge".into(), "Whether up.".into())]);
        assert_eq!(p.samples[1].0[2], ("path".into(), "/a \"b\"".into()));
        assert_eq!(p.samples[2].1, f64::INFINITY);
        let mut out = String::new();
        let mut labels = p.samples[1].0.clone();
        labels.sort();
        write_sample(&mut out, &labels, 3.0, Some(5));
        assert_eq!(out, "req_total{code=\"200\",path=\"/a \\\"b\\\"\"} 3 5\n");
    }
}
