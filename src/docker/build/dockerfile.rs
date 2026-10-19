//! Dockerfile parsing: the instruction set of Docker's classic builder plus
//! the BuildKit syntax real projects use (`RUN --mount`, `COPY --chmod`,
//! `--link`, heredocs), so a Dockerfile written for `docker buildx build`
//! builds here unchanged. Started from litecontainer's (`lc`) parser.

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    /// Upper-case keyword.
    pub keyword: String,
    /// `--name=value` flags (and `--name` alone, value empty).
    pub flags: Vec<(String, String)>,
    /// The arguments after the flags, as written (continuations joined).
    pub args: String,
    /// Heredocs following the line: (name, content).
    pub heredocs: Vec<(String, String)>,
    /// The original text, for build output and cache keys.
    pub original: String,
    pub line: usize,
}

impl Instruction {
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn flags_named(&self, name: &str) -> Vec<&str> {
        self.flags.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    /// Exec form (`["a", "b"]`) or shell form.
    pub fn command(&self) -> Command {
        match parse_json_array(&self.args) {
            Some(v) => Command::Exec(v),
            None => Command::Shell(self.args.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Shell(String),
    Exec(Vec<String>),
}

pub fn parse_json_array(s: &str) -> Option<Vec<String>> {
    let t = s.trim();
    if !t.starts_with('[') {
        return None;
    }
    serde_json::from_str::<Vec<String>>(t).ok()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    /// `FROM` image (unexpanded) and optional `--platform`.
    pub from: String,
    pub platform: Option<String>,
    pub name: Option<String>,
    pub index: usize,
    pub instructions: Vec<Instruction>,
    pub from_line: Instruction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dockerfile {
    /// `ARG`s before the first `FROM`.
    pub global_args: Vec<Instruction>,
    pub stages: Vec<Stage>,
    pub escape: char,
}

impl Dockerfile {
    pub fn stage_index(&self, name: &str) -> Option<usize> {
        let lower = name.to_lowercase();
        self.stages
            .iter()
            .position(|s| s.name.as_deref().is_some_and(|n| n.to_lowercase() == lower))
            .or_else(|| name.parse::<usize>().ok().filter(|i| *i < self.stages.len()))
    }
}

/// Parse a Dockerfile.
pub fn parse(text: &str) -> Result<Dockerfile> {
    let mut escape = '\\';
    let lines: Vec<&str> = text.lines().collect();
    // Parser directives: leading `# key=value` comments.
    for l in &lines {
        let t = l.trim();
        let Some(d) = t.strip_prefix('#') else { break };
        let Some((k, v)) = d.split_once('=') else { break };
        match k.trim().to_lowercase().as_str() {
            "escape" => {
                if let Some(c) = v.trim().chars().next() {
                    escape = c;
                }
            }
            "syntax" | "check" => {}
            _ => break,
        }
    }
    let mut instructions = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = i;
        let first = lines[i];
        let t = first.trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        // Join continuation lines (comment lines inside are dropped).
        let mut logical = String::new();
        let mut original = String::new();
        loop {
            let raw = lines[i];
            i += 1;
            original.push_str(raw);
            original.push('\n');
            let trimmed_end = raw.trim_end();
            if let Some(body) = trimmed_end.strip_suffix(escape) {
                logical.push_str(body);
                // Skip comment and empty lines inside a continuation.
                while i < lines.len() && (lines[i].trim_start().starts_with('#') || lines[i].trim().is_empty()) {
                    i += 1;
                }
                if i >= lines.len() {
                    break;
                }
                continue;
            }
            logical.push_str(raw);
            break;
        }
        let (keyword, rest) = match logical.trim().split_once(char::is_whitespace) {
            Some((k, r)) => (k.to_uppercase(), r.trim().to_string()),
            None => (logical.trim().to_uppercase(), String::new()),
        };
        let (flags, args) = split_flags(&rest);
        // Heredocs (`<<EOF`, `<<-EOF`, quoted) on RUN, COPY and ADD.
        let mut heredocs = Vec::new();
        if matches!(keyword.as_str(), "RUN" | "COPY" | "ADD") {
            for (name, strip_tabs) in heredoc_markers(&args) {
                let mut content = String::new();
                let mut found = false;
                while i < lines.len() {
                    let l = lines[i];
                    i += 1;
                    original.push_str(l);
                    original.push('\n');
                    let cmp = if strip_tabs { l.trim_start_matches('\t') } else { l };
                    if cmp == name {
                        found = true;
                        break;
                    }
                    content.push_str(if strip_tabs { l.trim_start_matches('\t') } else { l });
                    content.push('\n');
                }
                if !found {
                    bail!("Dockerfile line {}: unterminated heredoc {name}", start + 1);
                }
                heredocs.push((name, content));
            }
        }
        instructions.push(Instruction { keyword, flags, args, heredocs, original: original.trim_end().to_string(), line: start + 1 });
    }
    let mut global_args = Vec::new();
    let mut stages: Vec<Stage> = Vec::new();
    for ins in instructions {
        match ins.keyword.as_str() {
            "FROM" => {
                let words: Vec<&str> = ins.args.split_whitespace().collect();
                let (from, name) = match words.as_slice() {
                    [image] => (image.to_string(), None),
                    [image, as_kw, name] if as_kw.eq_ignore_ascii_case("as") => (image.to_string(), Some(name.to_string())),
                    _ => bail!("Dockerfile line {}: FROM requires either one or three arguments", ins.line),
                };
                let platform = ins.flag("platform").map(str::to_string);
                stages.push(Stage { from, platform, name, index: stages.len(), instructions: Vec::new(), from_line: ins });
            }
            "ARG" if stages.is_empty() => global_args.push(ins),
            k => match stages.last_mut() {
                Some(s) => {
                    if !KNOWN.contains(&k) {
                        bail!("Dockerfile line {}: unknown instruction: {k}", ins.line);
                    }
                    s.instructions.push(ins)
                }
                None => bail!("Dockerfile line {}: no build stage in current context ({k} before FROM)", ins.line),
            },
        }
    }
    if stages.is_empty() {
        bail!("the Dockerfile has no FROM instruction");
    }
    Ok(Dockerfile { global_args, stages, escape })
}

const KNOWN: &[&str] = &[
    "ADD", "ARG", "CMD", "COPY", "ENTRYPOINT", "ENV", "EXPOSE", "HEALTHCHECK", "LABEL", "MAINTAINER", "ONBUILD", "RUN", "SHELL", "STOPSIGNAL", "USER", "VOLUME",
    "WORKDIR",
];

/// Leading `--flag[=value]` words.
fn split_flags(rest: &str) -> (Vec<(String, String)>, String) {
    let mut flags = Vec::new();
    let mut s = rest.trim_start();
    while let Some(after) = s.strip_prefix("--") {
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        let word = &after[..end];
        match word.split_once('=') {
            Some((k, v)) => flags.push((k.to_lowercase(), unquote(v))),
            None => flags.push((word.to_lowercase(), String::new())),
        }
        s = after[end..].trim_start();
    }
    (flags, s.to_string())
}

/// Heredoc markers in an instruction's arguments: (word, strip tabs).
fn heredoc_markers(args: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = args;
    while let Some(pos) = rest.find("<<") {
        let after = &rest[pos + 2..];
        let (strip, after) = match after.strip_prefix('-') {
            Some(a) => (true, a),
            None => (false, after),
        };
        let word: String = after.trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !word.is_empty() && (after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '"' || c == '\'')) {
            out.push((word, strip));
        }
        rest = &rest[pos + 2..];
    }
    out
}

pub fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Expand `$VAR`, `${VAR}`, `${VAR:-default}`, `${VAR:+alt}` (and the
/// non-colon forms) with `lookup`; `escape` before `$` keeps it literal.
pub fn expand(s: &str, escape: char, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut in_single = false;
    while i < chars.len() {
        let c = chars[i];
        if c == escape && i + 1 < chars.len() && chars[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }
        if c == '\'' {
            in_single = !in_single;
            out.push(c);
            i += 1;
            continue;
        }
        if c != '$' || in_single || i + 1 >= chars.len() {
            out.push(c);
            i += 1;
            continue;
        }
        if chars[i + 1] == '{' {
            let Some(close) = chars[i + 2..].iter().position(|c| *c == '}') else {
                out.push(c);
                i += 1;
                continue;
            };
            let inner: String = chars[i + 2..i + 2 + close].iter().collect();
            i += 3 + close;
            let (name, op, word) = match inner.find([':', '-', '+']) {
                Some(p) => {
                    let (n, rest) = inner.split_at(p);
                    let (op, word) = if let Some(w) = rest.strip_prefix(":-") {
                        (":-", w)
                    } else if let Some(w) = rest.strip_prefix(":+") {
                        (":+", w)
                    } else if let Some(w) = rest.strip_prefix('-') {
                        ("-", w)
                    } else if let Some(w) = rest.strip_prefix('+') {
                        ("+", w)
                    } else {
                        ("", "")
                    };
                    (n.to_string(), op, word.to_string())
                }
                None => (inner.clone(), "", String::new()),
            };
            let value = lookup(&name);
            let word = expand(&word, escape, lookup);
            out.push_str(&match op {
                ":-" => value.filter(|v| !v.is_empty()).unwrap_or(word),
                "-" => value.unwrap_or(word),
                ":+" => {
                    if value.is_some_and(|v| !v.is_empty()) {
                        word
                    } else {
                        String::new()
                    }
                }
                "+" => {
                    if value.is_some() {
                        word
                    } else {
                        String::new()
                    }
                }
                _ => value.unwrap_or_default(),
            });
            continue;
        }
        let name: String = chars[i + 1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == '_').collect();
        if name.is_empty() {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1 + name.chars().count();
        out.push_str(&lookup(&name).unwrap_or_default());
    }
    out
}

/// `KEY=VALUE KEY2="v 2"` pairs (ENV, LABEL, ARG); the legacy `KEY VALUE`
/// form gives one pair.
pub fn key_values(args: &str) -> Result<Vec<(String, String)>> {
    let words = shell_words(args);
    if words.is_empty() {
        bail!("missing arguments");
    }
    if !words[0].0.contains('=') {
        // `ENV KEY value with spaces`
        let key = words[0].0.clone();
        let value = args.trim_start()[words[0].1..].trim().to_string();
        return Ok(vec![(key, unquote(&value))]);
    }
    let mut out = Vec::new();
    for (w, _) in words {
        let (k, v) = w.split_once('=').with_context(|| format!("expected KEY=VALUE, got {w}"))?;
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

/// Words with shell-like quoting (quotes removed, backslash escapes);
/// each with the byte offset where it ends in the (left-trimmed) input.
pub fn shell_words(s: &str) -> Vec<(String, usize)> {
    let s = s.trim_start();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = s.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some((_, n)) = chars.next() {
                    if !matches!(n, '"' | '\\' | '$') {
                        cur.push('\\');
                    }
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some((_, n)) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push((std::mem::take(&mut cur), idx));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push((cur, s.len()));
    }
    out
}

/// A `RUN --mount=...` spec.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunMount {
    /// `bind` (default), `cache`, `tmpfs`, `secret`, `ssh`.
    pub kind: String,
    pub target: String,
    pub source: Option<String>,
    pub from: Option<String>,
    pub id: Option<String>,
    pub read_only: bool,
    pub sharing: Option<String>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

pub fn parse_mount(spec: &str) -> Result<RunMount> {
    let mut m = RunMount { kind: "bind".into(), ..Default::default() };
    let mut rw_set = false;
    for part in spec.split(',') {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        match k.trim() {
            "type" => m.kind = v.to_string(),
            "target" | "dst" | "destination" => m.target = v.to_string(),
            "source" | "src" => m.source = Some(v.to_string()),
            "from" => m.from = Some(v.to_string()),
            "id" => m.id = Some(v.to_string()),
            "ro" | "readonly" => {
                m.read_only = v.is_empty() || v == "true";
                rw_set = true;
            }
            "rw" | "readwrite" => {
                m.read_only = !(v.is_empty() || v == "true");
                rw_set = true;
            }
            "sharing" => m.sharing = Some(v.to_string()),
            "mode" => m.mode = u32::from_str_radix(v, 8).ok(),
            "uid" => m.uid = v.parse().ok(),
            "gid" => m.gid = v.parse().ok(),
            "required" | "size" | "env" => {}
            other => bail!("unexpected key '{other}' in '{spec}'"),
        }
    }
    if m.kind == "bind" && !rw_set {
        m.read_only = true;
    }
    if m.target.is_empty() && !matches!(m.kind.as_str(), "secret" | "ssh") {
        bail!("mount {spec}: target is required");
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multistage_with_flags_and_heredoc() {
        let df = r#"# syntax=docker/dockerfile:1
ARG BASE=alpine:3.20
FROM ${BASE} AS build
WORKDIR /src
# comment
RUN --mount=type=cache,target=/root/.cache \
    # inside
    echo one && \
    echo two
COPY --chown=1000:1000 --chmod=755 a.sh b.sh /app/
RUN <<EOF
echo hello
echo world
EOF

FROM scratch
COPY --from=build /src/out /out
CMD ["/out"]
"#;
        let d = parse(df).unwrap();
        assert_eq!(d.global_args.len(), 1);
        assert_eq!(d.stages.len(), 2);
        let s0 = &d.stages[0];
        assert_eq!((s0.from.as_str(), s0.name.as_deref()), ("${BASE}", Some("build")));
        let run = &s0.instructions[1];
        assert_eq!(run.flag("mount"), Some("type=cache,target=/root/.cache"));
        assert_eq!(run.args, "echo one &&     echo two");
        let copy = &s0.instructions[2];
        assert_eq!(copy.flag("chown"), Some("1000:1000"));
        assert_eq!(copy.args, "a.sh b.sh /app/");
        let heredoc = &s0.instructions[3];
        assert_eq!(heredoc.heredocs, vec![("EOF".to_string(), "echo hello\necho world\n".to_string())]);
        assert_eq!(d.stage_index("BUILD"), Some(0));
        assert_eq!(d.stages[1].instructions[1].command(), Command::Exec(vec!["/out".into()]));
        assert!(parse("RUN x").is_err());
    }

    #[test]
    fn expands_variables() {
        let look = |n: &str| match n {
            "A" => Some("1".to_string()),
            "E" => Some(String::new()),
            _ => None,
        };
        assert_eq!(expand("$A-${A}-${B:-d}-${E:-x}-${E-y}-${A:+set}-\\$A", '\\', &look), "1-1-d-x--set-$A");
        assert_eq!(expand("'$A' $A", '\\', &look), "'$A' 1");
    }

    #[test]
    fn key_value_forms() {
        assert_eq!(key_values("A=1 B=\"two words\" C=x\\ y").unwrap(), vec![("A".into(), "1".into()), ("B".into(), "two words".into()), ("C".into(), "x y".into())]);
        assert_eq!(key_values("KEY some value").unwrap(), vec![("KEY".into(), "some value".into())]);
        let m = parse_mount("type=cache,target=/root/.npm,id=npm").unwrap();
        assert_eq!((m.kind.as_str(), m.target.as_str(), m.id.as_deref()), ("cache", "/root/.npm", Some("npm")));
        assert!(parse_mount("target=/x").unwrap().read_only);
    }
}
