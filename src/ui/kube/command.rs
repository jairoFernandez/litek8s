//! Command mode (`:`): what a line means, aliases, history and completion.
//!
//! `:pods`, `:po -n kube-system`, `:deploy -A -l app=web`, `:pods kube-system`
//! (a bare word is a namespace, as in k9s), `:svc -f metadata.name=web`,
//! `:ctx [name]`, `:ns [name]`, `:xray deploy [ns]`, `:pf`, `:kubectl <args>`
//! (also `:!` and `:k`), `:aliases`, `:help`, `:q`.

use std::collections::BTreeMap;

use super::filter::words;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ns {
    All,
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// A resource list: its name as typed, a namespace, and a filter (`-l`
    /// / `-f` / a pattern after `/`).
    Resource { name: String, ns: Option<Ns>, filter: String },
    Context(Option<String>),
    Namespace(Option<String>),
    Kubectl(String),
    Xray { name: String, ns: Option<Ns> },
    Forwards,
    Aliases,
    Help,
    Quit,
}

/// Parse a command line (without the `:`), after expanding the user's
/// aliases (`pp: pods -n kube-system`).
pub fn parse(line: &str, aliases: &BTreeMap<String, String>) -> Result<Cmd, String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('!') {
        return Ok(Cmd::Kubectl(rest.trim().to_string()));
    }
    let expanded;
    let line = match line.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((line, "")) {
        (head, rest) if aliases.contains_key(head) => {
            expanded = format!("{} {rest}", aliases[head]);
            expanded.trim()
        }
        _ => line,
    };
    let (head, rest) = line.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((line, ""));
    if head.is_empty() {
        return Err("type a resource (pods, deploy, svc, …) or a command (ctx, ns, xray, kubectl, help)".into());
    }
    let arg = || words(rest).into_iter().next();
    Ok(match head.to_lowercase().as_str() {
        "q" | "q!" | "quit" | "exit" => Cmd::Quit,
        "help" | "h" | "?" => Cmd::Help,
        "ctx" | "context" | "contexts" => Cmd::Context(arg()),
        "ns" | "namespace" if !rest.is_empty() && !rest.starts_with('-') => Cmd::Namespace(arg()),
        "kubectl" | "k" => Cmd::Kubectl(rest.to_string()),
        "pf" | "portforward" | "portforwards" | "port-forward" | "port-forwards" => Cmd::Forwards,
        "aliases" | "alias" | "a" | "api-resources" => Cmd::Aliases,
        "xray" | "x" => {
            let (name, rest) = rest.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((rest, ""));
            if name.is_empty() || name.starts_with('-') {
                return Err("xray what? e.g. :xray deploy (or sts, ds, job, cronjob, rs, svc, node)".into());
            }
            let (bare, ns, _) = resource_args(rest)?;
            let ns = ns.or(bare.map(Ns::Named));
            Cmd::Xray { name: name.to_string(), ns }
        }
        _ => {
            let (extra, ns, filter) = resource_args(rest)?;
            // `:pods kube-system`: a bare word names the namespace.
            let ns = match (ns, extra) {
                (Some(n), _) => Some(n),
                (None, Some(w)) => Some(Ns::Named(w)),
                (None, None) => None,
            };
            Cmd::Resource { name: head.to_string(), ns, filter }
        }
    })
}

/// `-n ns`, `-A`, `-l sel`, `-f sel`, `/pattern` and one bare word.
fn resource_args(rest: &str) -> Result<(Option<String>, Option<Ns>, String), String> {
    let toks = words(rest);
    let mut ns = None;
    let mut bare = None;
    let mut filter = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].as_str();
        match t {
            "-A" | "--all-namespaces" | "--all" => ns = Some(Ns::All),
            "-n" | "--namespace" => {
                i += 1;
                ns = Some(Ns::Named(toks.get(i).cloned().ok_or("-n needs a namespace")?));
            }
            _ if t.starts_with("--namespace=") => ns = Some(Ns::Named(t[12..].to_string())),
            _ if t.starts_with("-n") && t.len() > 2 => ns = Some(Ns::Named(t[2..].to_string())),
            "-l" | "--selector" | "-f" | "--field-selector" => {
                i += 1;
                let v = toks.get(i).ok_or(format!("{t} needs a selector"))?;
                filter.push(format!("{t} {}", quote(v)));
                // `tier in (a, b)` spans words.
                while let Some(next) = toks.get(i + 1) {
                    let last = filter.last().unwrap();
                    if last.matches('(').count() > last.matches(')').count() || matches!(next.as_str(), "in" | "notin") || next.starts_with('(') {
                        let l = filter.pop().unwrap();
                        filter.push(format!("{l} {}", quote(next)));
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            _ if t.starts_with('/') => filter.push(toks[i..].join(" ")[1..].to_string()),
            _ if t.starts_with('-') => return Err(format!("unknown flag {t} (-n, -A, -l, -f)")),
            _ if bare.is_none() => bare = Some(t.to_string()),
            _ => return Err(format!("unexpected {t:?}")),
        }
        if t.starts_with('/') {
            break;
        }
        i += 1;
    }
    Ok((bare, ns, filter.join(" ")))
}

fn quote(s: &str) -> String {
    if s.contains(char::is_whitespace) { format!("'{s}'") } else { s.to_string() }
}

/// The words a command can start with besides resource names.
pub const COMMANDS: &[&str] = &["ctx", "ns", "xray", "pf", "kubectl", "aliases", "help", "quit"];

/// What the word being typed could become: candidates for the last word
/// of `line` (with `ns` / `ctx` / `-n` taking names).
pub fn complete(line: &str, resources: &[String], namespaces: &[String], contexts: &[String], aliases: &BTreeMap<String, String>) -> Vec<String> {
    let ends_in_space = line.ends_with(' ');
    let toks: Vec<&str> = line.split_whitespace().collect();
    let (prev, cur) = match (toks.len(), ends_in_space) {
        (0, _) => ("", ""),
        (n, true) => (toks[n - 1], ""),
        (1, false) => ("", toks[0]),
        (n, false) => (toks[n - 2], toks[n - 1]),
    };
    let first = toks.first().copied().unwrap_or("");
    let cmds: Vec<String> = COMMANDS.iter().map(|s| s.to_string()).collect();
    let pool: Vec<&String> = if toks.is_empty() || (toks.len() == 1 && !ends_in_space) {
        cmds.iter().chain(aliases.keys()).chain(resources.iter()).collect()
    } else if matches!(first, "ctx" | "context") {
        contexts.iter().collect()
    } else if matches!(first, "ns" | "namespace") || prev == "-n" || prev == "--namespace" || (toks.len() <= 2 && !first.starts_with('!') && !matches!(first, "kubectl" | "k" | "xray" | "x") && !cur.starts_with('-')) {
        namespaces.iter().collect()
    } else if matches!(first, "xray" | "x") && toks.len() <= 2 {
        resources.iter().collect()
    } else {
        Vec::new()
    };
    let lc = cur.to_lowercase();
    let mut out: Vec<String> = pool.into_iter().filter(|c| c.to_lowercase().starts_with(&lc) && c.as_str() != cur).cloned().collect();
    out.dedup();
    out.truncate(50);
    out
}

/// Command history, most recent last, without repeats.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub entries: Vec<String>,
    /// While browsing with ↑↓: the position.
    pub pos: Option<usize>,
}

pub const HISTORY_MAX: usize = 100;

impl History {
    pub fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        self.entries.retain(|e| e != line);
        self.entries.push(line.to_string());
        if self.entries.len() > HISTORY_MAX {
            self.entries.remove(0);
        }
        self.pos = None;
    }

    /// ↑: an older entry.
    pub fn older(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let p = match self.pos {
            None => self.entries.len() - 1,
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.pos = Some(p);
        self.entries.get(p).map(String::as_str)
    }

    /// ↓: a newer one; `None` past the newest (an empty line).
    pub fn newer(&mut self) -> Option<&str> {
        match self.pos {
            Some(p) if p + 1 < self.entries.len() => {
                self.pos = Some(p + 1);
                self.entries.get(p + 1).map(String::as_str)
            }
            _ => {
                self.pos = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn parses_resources_with_namespaces_and_selectors() {
        assert_eq!(parse("pods", &no()), Ok(Cmd::Resource { name: "pods".into(), ns: None, filter: String::new() }));
        assert_eq!(parse("po kube-system", &no()), Ok(Cmd::Resource { name: "po".into(), ns: Some(Ns::Named("kube-system".into())), filter: String::new() }));
        assert_eq!(parse("deploy -A -l app=web", &no()), Ok(Cmd::Resource { name: "deploy".into(), ns: Some(Ns::All), filter: "-l app=web".into() }));
        assert_eq!(parse("pods -n web -f status.phase=Running", &no()), Ok(Cmd::Resource { name: "pods".into(), ns: Some(Ns::Named("web".into())), filter: "-f status.phase=Running".into() }));
        assert_eq!(parse("pods -l tier in (a, b) /nginx x", &no()), Ok(Cmd::Resource { name: "pods".into(), ns: None, filter: "-l tier in (a, b) nginx x".into() }));
        assert_eq!(parse("pods -nfoo", &no()), Ok(Cmd::Resource { name: "pods".into(), ns: Some(Ns::Named("foo".into())), filter: String::new() }));
        assert!(parse("pods -z", &no()).is_err());
        assert!(parse("pods a b", &no()).is_err());
        assert!(parse("", &no()).is_err());
    }

    #[test]
    fn parses_commands_and_aliases() {
        assert_eq!(parse("ctx", &no()), Ok(Cmd::Context(None)));
        assert_eq!(parse("ctx prod", &no()), Ok(Cmd::Context(Some("prod".into()))));
        assert_eq!(parse("ns kube-system", &no()), Ok(Cmd::Namespace(Some("kube-system".into()))));
        assert_eq!(parse("ns", &no()), Ok(Cmd::Resource { name: "ns".into(), ns: None, filter: String::new() }));
        assert_eq!(parse("!get pods -o wide", &no()), Ok(Cmd::Kubectl("get pods -o wide".into())));
        assert_eq!(parse("kubectl top nodes", &no()), Ok(Cmd::Kubectl("top nodes".into())));
        assert_eq!(parse("xray deploy kube-system", &no()), Ok(Cmd::Xray { name: "deploy".into(), ns: Some(Ns::Named("kube-system".into())) }));
        assert!(parse("xray", &no()).is_err());
        assert_eq!(parse("q", &no()), Ok(Cmd::Quit));
        assert_eq!(parse("pf", &no()), Ok(Cmd::Forwards));
        let aliases: BTreeMap<String, String> = [("cp".to_string(), "pods -n kube-system -l tier=control-plane".to_string())].into();
        assert_eq!(parse("cp", &aliases), Ok(Cmd::Resource { name: "pods".into(), ns: Some(Ns::Named("kube-system".into())), filter: "-l tier=control-plane".into() }));
    }

    #[test]
    fn completes_resources_namespaces_and_contexts() {
        let res: Vec<String> = ["po", "pods", "deploy", "deployments", "daemonsets"].iter().map(|s| s.to_string()).collect();
        let ns: Vec<String> = ["default", "kube-system", "web"].iter().map(|s| s.to_string()).collect();
        let ctx: Vec<String> = ["litek8s-dev", "prod"].iter().map(|s| s.to_string()).collect();
        assert_eq!(complete("dep", &res, &ns, &ctx, &no()), vec!["deploy", "deployments"]);
        assert_eq!(complete("pods -n ku", &res, &ns, &ctx, &no()), vec!["kube-system"]);
        assert_eq!(complete("pods k", &res, &ns, &ctx, &no()), vec!["kube-system"]);
        assert_eq!(complete("ctx p", &res, &ns, &ctx, &no()), vec!["prod"]);
        assert_eq!(complete("ns ", &res, &ns, &ctx, &no()).len(), 3);
        assert!(complete("kubectl get po", &res, &ns, &ctx, &no()).is_empty());
        assert!(complete("he", &res, &ns, &ctx, &no()).contains(&"help".to_string()));
    }

    #[test]
    fn history_browses_and_dedupes() {
        let mut h = History::default();
        h.push("pods");
        h.push("deploy");
        h.push("pods");
        assert_eq!(h.entries, vec!["deploy", "pods"]);
        assert_eq!(h.older(), Some("pods"));
        assert_eq!(h.older(), Some("deploy"));
        assert_eq!(h.older(), Some("deploy"));
        assert_eq!(h.newer(), Some("pods"));
        assert_eq!(h.newer(), None);
    }
}
