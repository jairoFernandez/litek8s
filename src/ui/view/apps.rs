//! The Apps view (0): Argo CD's Applications as Argo CD's own UI lists
//! them (sync, health, revision, where from, last sync, what the operation
//! says), the ApplicationSets (`A`), one app's resources, sync history and
//! last result (enter), and the sync dialog with its prune and force
//! options.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Clear, Padding, Paragraph, Row, Wrap};
use serde_json::Value;

use super::super::app::{App, ArgoPane};
use super::super::argo;
use super::super::kube::state::ObjRef;
use super::super::model::age;
use super::super::theme::{Health, Theme};
use super::{centered, key_chips, render_table, row_style, trunc, unavailable};

/// The keys the footer shows.
pub fn hints(app: &App) -> &'static [(&'static str, &'static str)] {
    if app.argo.detail.is_some() {
        &[("esc", "back"), ("h", "history/result"), ("enter", "describe"), ("s", "sync"), ("r/R", "refresh"), ("T", "terminate"), ("D", "diff")]
    } else if app.argo.sets {
        &[("A", "applications"), ("/", "filter")]
    } else {
        &[("enter", "open"), ("s", "sync"), ("r/R", "refresh"), ("T", "terminate"), ("D", "diff"), ("A", "appsets"), ("/", "filter")]
    }
}

pub fn draw(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let s = &app.argo.sample;
    if let Some(e) = s.error.as_ref().filter(|_| s.apps.is_empty()) {
        return unavailable(f, t, area, "Apps", e, "Apps lists the cluster's applications.argoproj.io (Argo CD's or litecd's) through the Kubernetes API.");
    }
    if app.argo.detail.is_some() {
        return detail(f, app, t, area);
    }
    if app.argo.sets {
        return sets(f, app, t, area);
    }
    // The selected app's details under the list when there is room.
    let boxed = area.height >= 16;
    let [list, sel] = Layout::vertical([Constraint::Min(5), Constraint::Length(if boxed { 5 } else { 0 })]).areas(area);
    let idx = app.visible_argo_apps();
    let namespaces = {
        let mut n: Vec<&str> = s.apps.iter().map(argo::namespace).collect();
        n.dedup();
        n.len() > 1
    };
    let wide = area.width >= 140;
    // Names as wide as the longest, up to a third of the line: the message
    // gets the rest.
    let longest = idx.iter().map(|i| argo::name(&s.apps[*i]).chars().count() + if namespaces { argo::namespace(&s.apps[*i]).chars().count() + 1 } else { 0 }).max().unwrap_or(4) + 2;
    let name_w = longest.clamp(6, (list.width as usize / 3).max(16)) as u16;
    let rows: Vec<Row> = idx
        .iter()
        .map(|i| {
            let a = &s.apps[*i];
            let h = argo::glyph(a);
            let (note, nh) = argo::note(a);
            let name = if namespaces { format!("{}/{}", argo::namespace(a), argo::name(a)) } else { argo::name(a).to_string() };
            let sync = argo::sync_status(a);
            let health = argo::health_status(a);
            let mut cells = vec![
                Cell::from(Line::from(vec![Span::styled(format!("{} ", h.glyph()), t.health(h)), Span::raw(mid(&name, name_w as usize - 2))])),
                Cell::from(Span::styled(sync.to_string(), if sync == "Synced" { Style::new() } else { t.warn })),
                Cell::from(Span::styled(health.to_string(), t.health(super::super::kube::gitops::health_of(health, "Synced")))),
                Cell::from(Span::styled(argo::revision(a), t.dim)),
                Cell::from(Span::styled(argo::last_sync(a).map(|at| age(app.now, at)).unwrap_or_else(|| "-".into()), t.dim)),
            ];
            if wide {
                let project = a.pointer("/spec/project").and_then(Value::as_str).unwrap_or("default");
                cells.push(Cell::from(Span::styled(project.to_string(), t.dim)));
                cells.push(Cell::from(Span::styled(argo::sources(a).first().map(|x| argo::short_repo(x)).unwrap_or_default(), t.dim)));
            }
            cells.push(Cell::from(Span::styled(note, if nh == Health::Ok { t.dim } else { t.health(nh) })));
            Row::new(cells).style(row_style(t, h))
        })
        .collect();
    let mut header = vec!["NAME", "SYNC", "HEALTH", "REV", "SYNCED"];
    let mut widths = vec![Constraint::Length(name_w), Constraint::Length(9), Constraint::Length(11), Constraint::Length(7), Constraint::Length(6)];
    if wide {
        let src_w = idx.iter().filter_map(|i| argo::sources(&s.apps[*i]).first().map(|x| argo::short_repo(x).chars().count())).max().unwrap_or(6).clamp(6, 36) as u16;
        header.extend(["PROJECT", "SOURCE"]);
        widths.extend([Constraint::Length(10), Constraint::Length(src_w)]);
    }
    header.push("MESSAGE");
    widths.push(Constraint::Fill(1));
    let title = format!("Applications · {}", s.controller.as_ref().map(argo::Controller::label).unwrap_or_else(|| "controller unknown".into()));
    let empty = if s.at == 0 { "Reading the Applications…" } else { "No Applications (applications.argoproj.io) on this cluster." };
    // The counts that fit next to the title, most important first.
    let room = (list.width as usize).saturating_sub(title.chars().count() + 9);
    let mut right = String::new();
    for p in counts(&s.apps) {
        let next = if right.is_empty() { p } else { format!("{right} · {p}") };
        if next.chars().count() > room {
            break;
        }
        right = next;
    }
    render_table(f, app, t, list, &title, right, header, rows, widths, empty);
    if boxed {
        selected(f, app, t, sel);
    }
}

/// `38 apps`, `1 degraded`, `2 OutOfSync`, `3 progressing`, `1 syncing`.
fn counts(apps: &[Value]) -> Vec<String> {
    let n = |f: &dyn Fn(&Value) -> bool| apps.iter().filter(|a| f(a)).count();
    let mut parts = vec![format!("{} apps", apps.len())];
    for (count, word) in [
        (n(&|a| argo::health_status(a) == "Degraded"), "degraded"),
        (n(&|a| argo::sync_status(a) == "OutOfSync"), "OutOfSync"),
        (n(&|a| matches!(argo::health_status(a), "Progressing" | "Missing")), "progressing"),
        (n(&|a| argo::running(a)), "syncing"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {word}"));
        }
    }
    parts
}

/// Where the selected app comes from and goes, and what it is doing.
fn selected(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Some(a) = app.argo_selected() else {
        f.render_widget(Paragraph::new("").block(t.block("")), area);
        return;
    };
    let w = area.width.saturating_sub(12) as usize;
    let label = |l: &str| Span::styled(format!(" {l:<8} "), t.dim);
    let src = argo::sources(a);
    let src_text = match src.len() {
        0 => "-".to_string(),
        1 => src[0].clone(),
        n => format!("{} (+{} more sources)", src[0], n - 1),
    };
    let mut lines = vec![
        Line::from(vec![label("source"), Span::raw(trunc(&src_text, w))]),
        Line::from(vec![label("dest"), Span::raw(trunc(&format!("{} · {}", argo::destination(a), argo::policy(a)), w))]),
    ];
    let (note, h) = argo::note(a);
    lines.push(if note.is_empty() {
        let compared = a.pointer("/status/reconciledAt").and_then(Value::as_str).and_then(argo::parse_time).map(|at| format!("compared {} ago", age(app.now, at))).unwrap_or_default();
        Line::from(vec![label("status"), Span::styled(format!("{} · {}  {compared}", argo::sync_status(a), argo::health_status(a)), t.dim)])
    } else {
        Line::from(vec![label("status"), Span::styled(trunc(&note, w), if h == Health::Ok { t.dim } else { t.health(h) })])
    });
    let title = format!("{}/{}", argo::namespace(a), argo::name(a));
    f.render_widget(Paragraph::new(lines).block(t.block_with(Line::styled(title, t.title), vec![Span::styled("enter: resources & history", t.dim)])), area);
}

fn sets(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let s = &app.argo.sample;
    let list = s.appsets.as_deref().unwrap_or_default();
    let rows: Vec<Row> = app
        .visible_argo_sets()
        .iter()
        .map(|i| {
            let set = &list[*i];
            let (gens, owned, msg, h) = argo::appset_line(set, &s.apps);
            Row::new(vec![
                Cell::from(Line::from(vec![Span::styled(format!("{} ", h.glyph()), t.health(h)), Span::raw(format!("{}/{}", argo::namespace(set), argo::name(set)))])),
                Cell::from(Span::styled(gens, t.dim)),
                Cell::from(format!("{owned:>4}")),
                Cell::from(Span::styled(msg, if h == Health::Ok { t.dim } else { t.health(h) })),
            ])
        })
        .collect();
    render_table(f, app, t, area, "ApplicationSets", format!("{} sets", list.len()), vec!["NAME", "GENERATORS", "APPS", "STATUS"], rows, vec![Constraint::Fill(2), Constraint::Length(18), Constraint::Length(4), Constraint::Fill(3)], "No ApplicationSets.");
}

fn detail(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Some(a) = app.argo_detail_app() else {
        let (ns, name) = app.argo.detail.clone().unwrap_or_default();
        return unavailable(f, t, area, "Apps", &format!("{ns}/{name} is gone"), "esc: back to the list");
    };
    let w = area.width.saturating_sub(13) as usize;
    let label = |l: &str| Span::styled(format!(" {l:<9} "), t.dim);
    let mut lines = Vec::new();
    for (i, src) in argo::sources(a).iter().enumerate() {
        lines.push(Line::from(vec![label(if i == 0 { "source" } else { "" }), Span::raw(trunc(src, w))]));
    }
    let project = a.pointer("/spec/project").and_then(Value::as_str).unwrap_or("default");
    lines.push(Line::from(vec![label("dest"), Span::raw(trunc(&format!("{} · project {project} · {}", argo::destination(a), argo::policy(a)), w))]));
    let rev = argo::revision(a);
    let synced = argo::last_sync(a).map(|at| format!("synced {} ago", age(app.now, at))).unwrap_or_else(|| "never synced".into());
    let compared = a.pointer("/status/reconciledAt").and_then(Value::as_str).and_then(argo::parse_time).map(|at| format!(" · compared {} ago", age(app.now, at))).unwrap_or_default();
    lines.push(Line::from(vec![label("revision"), Span::raw(if rev.is_empty() { "-".to_string() } else { rev }), Span::styled(format!(" · {synced}{compared}"), t.dim)]));
    if let Some((phase, msg)) = argo::operation(a) {
        let h = match phase.as_str() {
            "Running" | "Terminating" | "Requested" => Health::Warn,
            "Failed" | "Error" => Health::Bad,
            _ => Health::Done,
        };
        let by = a.pointer("/status/operationState/operation/initiatedBy").map(|b| if b["automated"] == true { " (auto)".to_string() } else { b["username"].as_str().map(|u| format!(" ({u})")).unwrap_or_default() }).unwrap_or_default();
        lines.push(Line::from(vec![label("operation"), Span::styled(format!("{} {phase}{by}", h.glyph()), t.health(h)), Span::styled(trunc(&format!(": {msg}"), w.saturating_sub(phase.len() + by.len() + 2)), if h == Health::Done { t.dim } else { t.health(h) })]));
    }
    let health_msg = a.pointer("/status/health/message").and_then(Value::as_str).unwrap_or_default();
    if !health_msg.is_empty() {
        lines.push(Line::from(vec![label("health"), Span::styled(trunc(health_msg, w), t.warn)]));
    }
    for (ty, msg, at) in argo::conditions(a).into_iter().take(3) {
        let h = if ty.ends_with("Error") { Health::Bad } else { Health::Warn };
        let when = at.map(|at| format!(" ({})", age(app.now, at))).unwrap_or_default();
        lines.push(Line::from(vec![label("condition"), Span::styled(trunc(&format!("{ty}{when}: {msg}"), w), t.health(h))]));
    }
    // The summary takes what it needs, the list at least 5 rows.
    let top_h = (lines.len() as u16 + 2).min(area.height.saturating_sub(5));
    let [top, rest] = Layout::vertical([Constraint::Length(top_h), Constraint::Min(3)]).areas(area);
    let h = argo::glyph(a);
    let title = Line::from(vec![Span::styled(format!("{} ", h.glyph()), t.health(h)), Span::styled(format!("app {}/{}", argo::namespace(a), argo::name(a)), t.title)]);
    let right = vec![Span::styled(format!("{} · {}", argo::sync_status(a), argo::health_status(a)), t.health(h))];
    f.render_widget(Paragraph::new(lines).block(t.block_with(title, right)), top);

    let pane = app.argo.pane;
    let others = [ArgoPane::Resources, ArgoPane::History, ArgoPane::Result].into_iter().filter(|p| *p != pane).map(ArgoPane::title).collect::<Vec<_>>().join(", ");
    match pane {
        ArgoPane::Resources => {
            let res = argo::resources(a);
            let synced = res.iter().filter(|r| r.sync == "Synced").count();
            let kind_w = res.iter().map(|r| r.kind.chars().count() + 2).max().unwrap_or(6).clamp(6, 24) as u16;
            let name_w = res.iter().map(|r| r.name.chars().count() + r.namespace.chars().count() + 1).max().unwrap_or(6).clamp(6, (rest.width as usize / 3).max(12)) as u16;
            let rows: Vec<Row> = res
                .iter()
                .map(|r| {
                    let h = r.glyph();
                    let mut note = r.message.clone();
                    for (on, word) in [(r.prune, "requires pruning"), (r.hook, "hook")] {
                        if on {
                            note = if note.is_empty() { word.to_string() } else { format!("{note} · {word}") };
                        }
                    }
                    let name = if r.namespace.is_empty() { r.name.clone() } else { format!("{}/{}", r.namespace, r.name) };
                    Row::new(vec![
                        Cell::from(Line::from(vec![Span::styled(format!("{} ", h.glyph()), t.health(h)), Span::raw(r.kind.clone())])),
                        Cell::from(mid(&name, name_w as usize)),
                        Cell::from(Span::styled(r.sync.clone(), if r.sync == "Synced" { t.dim } else { t.warn })),
                        Cell::from(Span::styled(if r.health.is_empty() { "-".to_string() } else { r.health.clone() }, t.health(super::super::kube::gitops::health_of(&r.health, "Synced")))),
                        Cell::from(Span::styled(note, if h == Health::Bad { t.bad } else { t.dim })),
                    ])
                })
                .collect();
            let live = if res.iter().any(|r| r.live) { " · health read from the live objects" } else { "" };
            render_table(f, app, t, rest, &format!("resources · h: {others}"), format!("{synced}/{} synced{live}", res.len()), vec!["KIND", "NAME", "SYNC", "HEALTH", "MESSAGE"], rows, vec![Constraint::Length(kind_w), Constraint::Length(name_w), Constraint::Length(9), Constraint::Length(11), Constraint::Fill(1)], "No resources in its status yet.");
        }
        ArgoPane::History => {
            let hist = argo::history(a);
            let rows: Vec<Row> = hist
                .iter()
                .map(|d| {
                    Row::new(vec![
                        Cell::from(format!("{:>3}", d.id)),
                        Cell::from(d.revision.clone()),
                        Cell::from(Span::styled(d.at.map(|at| age(app.now, at)).unwrap_or_else(|| "-".into()), t.dim)),
                        Cell::from(d.by.clone()),
                        Cell::from(Span::styled(d.source.clone(), t.dim)),
                    ])
                })
                .collect();
            render_table(f, app, t, rest, &format!("sync history · h: {others}"), format!("{} deploys, newest first", hist.len()), vec![" ID", "REVISION", "AGO", "BY", "SOURCE"], rows, vec![Constraint::Length(4), Constraint::Length(16), Constraint::Length(5), Constraint::Length(10), Constraint::Fill(1)], "No sync history yet.");
        }
        ArgoPane::Result => {
            let res = argo::sync_result(a);
            let rows: Vec<Row> = res
                .iter()
                .map(|(what, status, msg)| {
                    let h = match status.as_str() {
                        "Synced" => Health::Ok,
                        "SyncFailed" => Health::Bad,
                        "Pruned" | "PruneSkipped" => Health::Done,
                        _ => Health::Warn,
                    };
                    Row::new(vec![Cell::from(Line::from(vec![Span::styled(format!("{} ", h.glyph()), t.health(h)), Span::raw(what.clone())])), Cell::from(Span::styled(status.clone(), t.health(h))), Cell::from(Span::styled(msg.clone(), t.dim))])
                })
                .collect();
            let phase = argo::operation(a).map(|(p, _)| p).unwrap_or_else(|| "no operation".into());
            render_table(f, app, t, rest, &format!("last sync result · h: {others}"), phase, vec!["RESOURCE", "RESULT", "MESSAGE"], rows, vec![Constraint::Fill(2), Constraint::Length(12), Constraint::Fill(3)], "No sync has run yet.");
        }
    }
}

/// The sync dialog: what it does, prune and force (each says what it
/// costs), the keys.
pub fn sync_dialog(f: &mut Frame, t: &Theme, objs: &[ObjRef], prune: bool, force: bool, area: Rect) {
    let what = if objs.len() == 1 { objs[0].label() } else { format!("{} apps", objs.len()) };
    let check = |on: bool| if on { "[x]" } else { "[ ]" };
    let mut lines = vec![Line::raw("Applies what git has: writes an operation on the app, as argocd app sync,"), Line::raw("for whichever controller reconciles it (Argo CD or litecd)."), Line::raw("")];
    lines.push(Line::from(vec![Span::styled(format!("{} ", check(prune)), if prune { t.bad } else { t.dim }), Span::styled("p prune ", t.bold), Span::styled("deletes what git no longer has", if prune { t.bad } else { t.dim })]));
    lines.push(Line::from(vec![Span::styled(format!("{} ", check(force)), if force { t.bad } else { t.dim }), Span::styled("f force ", t.bold), Span::styled("deletes and recreates what cannot be patched", if force { t.bad } else { t.dim })]));
    lines.push(Line::raw(""));
    lines.push(Line::from(key_chips(t, &[("y", "sync"), ("p", "prune"), ("f", "force"), ("n/esc", "cancel")], 66)));
    let r = centered(area, 80, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(t.dialog(Line::from(vec![Span::styled("⚠ ", t.warn), Span::styled(format!("Sync {what}?"), t.bold)]), t.warn).padding(Padding::horizontal(1))), r);
}

/// `development-env…-groveback`: names that differ at the end stay told
/// apart when cut.
fn mid(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w || w < 5 {
        return s.to_string();
    }
    let head = (w - 1) / 2;
    let tail = w - 1 - head;
    format!("{}…{}", s.chars().take(head).collect::<String>(), s.chars().skip(n - tail).collect::<String>())
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_are_cut_in_the_middle() {
        assert_eq!(super::mid("development-environment-groveback", 16), "develop…roveback");
        assert_eq!(super::mid("web", 16), "web");
    }
}
