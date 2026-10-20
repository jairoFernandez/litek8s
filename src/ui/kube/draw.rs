//! Drawing the Kubernetes view. Only what is on screen is built (tables of
//! thousands of rows draw the visible ones); columns fit 80 columns by
//! dropping the wide-only ones, then narrowing the widest.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::state::*;
use super::table::Health;
use super::uiconfig::Theme;
use super::yaml;
use crate::ui::app::App;

fn block(title: String, t: &Theme) -> Block<'static> {
    Block::bordered().border_style(t.dim).title(Span::styled(format!(" {title} "), t.bold))
}

fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(w - 1).collect();
    out.push('…');
    out
}

fn pad(s: &str, w: usize, right: bool) -> String {
    let s = trunc(s, w);
    let n = s.chars().count();
    if right { format!("{}{s}", " ".repeat(w - n)) } else { format!("{s}{}", " ".repeat(w - n)) }
}

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let ui = &app.kube;
    let t = &ui.theme;
    let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    header(f, app, head);
    match ui.top() {
        View::Table(tv) => table(f, app, tv, body),
        View::Pager(p) => pager(f, p, t, body),
        View::Logs(l) => logs(f, l, t, body),
        View::Xray(x) => xray(f, x, t, body),
        View::Forwards { sel } => forwards(f, ui, *sel, body),
    }
    match &ui.overlay {
        Overlay::None | Overlay::Prompt(_) => {}
        Overlay::Confirm(c) => confirm(f, c, t, area),
        Overlay::Input(i) => input(f, i, t, area),
        Overlay::Picker(p) => picker(f, p, t, area),
    }
}

/// The line above the view: context, namespace, the stack; or the prompt.
fn header(f: &mut Frame, app: &App, area: Rect) {
    let ui = &app.kube;
    let t = &ui.theme;
    if let Overlay::Prompt(p) = &ui.overlay {
        return prompt(f, ui, p, area);
    }
    let mut spans = vec![Span::styled(" ⎈ ", t.accent), Span::styled(if app.kube_context.is_empty() { "-".to_string() } else { app.kube_context.clone() }, t.bold), Span::styled("  ns ", t.dim), Span::styled(ui.namespace.clone().unwrap_or_else(|| "all".into()), t.bold)];
    // The stack of views, as k9s's crumbs.
    let crumbs: Vec<String> = ui
        .views
        .iter()
        .map(|v| match v {
            View::Table(tv) => tv.scope.title.clone().unwrap_or_else(|| tv.res.name()),
            View::Pager(p) => match p.kind {
                PagerKind::Describe => "describe".into(),
                PagerKind::Yaml => "yaml".into(),
                PagerKind::Diff => "diff".into(),
                PagerKind::Kubectl => "kubectl".into(),
                PagerKind::Help => "help".into(),
                PagerKind::Text => "history".into(),
            },
            View::Logs(_) => "logs".into(),
            View::Xray(x) => format!("xray {}", x.res.plural),
            View::Forwards { .. } => "port-forwards".into(),
        })
        .collect();
    spans.push(Span::styled("  │ ", t.dim));
    let room = (area.width as usize).saturating_sub(spans.iter().map(|s| s.width()).sum::<usize>() + 16);
    let mut c = crumbs.join(" › ");
    if c.chars().count() > room {
        c = format!("…{}", c.chars().rev().take(room.saturating_sub(1)).collect::<Vec<_>>().into_iter().rev().collect::<String>());
    }
    spans.push(Span::raw(c));
    let mut right = Vec::new();
    if !ui.forwards.is_empty() {
        right.push(Span::styled(format!("⇄{} ", ui.forwards.len()), t.ok.add_modifier(Modifier::BOLD)));
    }
    if !ui.discovered {
        right.push(Span::styled(if ui.discovery_error.is_some() { "api? " } else { "… " }, t.dim));
    }
    let rw = right.iter().map(|s| s.width()).sum::<usize>() as u16;
    let [l, r] = Layout::horizontal([Constraint::Min(1), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(spans)), l);
    f.render_widget(Paragraph::new(Line::from(right)), r);
}

fn prompt(f: &mut Frame, ui: &KubeUi, p: &Prompt, area: Rect) {
    let t = &ui.theme;
    let sigil = match p.kind {
        PromptKind::Command => ":",
        PromptKind::Kubectl => "!kubectl ",
        _ => "/",
    };
    let mut spans = vec![Span::styled(format!(" {sigil}"), t.accent), Span::raw(p.text.clone())];
    match p.kind {
        PromptKind::Command => {
            // The first suggestion as ghost text, the others after it.
            let cur = p.text.rsplit(' ').next().unwrap_or("");
            if let Some(first) = p.suggestions.first().filter(|_| p.tab.is_none()) {
                if first.starts_with(cur) {
                    spans.push(Span::styled("█", t.accent));
                    spans.push(Span::styled(first[cur.len()..].to_string(), t.dim));
                } else {
                    spans.push(Span::styled("█", t.accent));
                }
            } else {
                spans.push(Span::styled("█", t.accent));
            }
            let others: Vec<&String> = p.suggestions.iter().take(12).collect();
            if !others.is_empty() {
                spans.push(Span::styled(format!("   {}", others.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")), t.dim));
            }
        }
        PromptKind::Kubectl => spans.push(Span::styled("█", t.accent)),
        _ => {
            spans.push(Span::styled("█", t.accent));
            spans.push(Span::styled(format!("   [{}] tab: mode", p.mode.label()), t.dim));
            if p.kind == PromptKind::Filter {
                if let Some(tv) = ui.top_table() {
                    spans.push(Span::styled(format!("  {} of {}", tv.rows.len(), tv.total), t.dim));
                    if let Some(e) = &tv.filter.error {
                        spans.push(Span::styled(format!("  {e}"), t.warn));
                    }
                }
            }
        }
    }
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Which columns fit, and how wide: wide-only ones go first, then the
/// widest columns narrow (names last).
pub fn fit(cols: &[DCol], width: usize, wide: bool) -> Vec<(usize, usize)> {
    let mut keep: Vec<usize> = (0..cols.len()).filter(|&i| wide || cols[i].priority == 0).collect();
    let need = |keep: &[usize]| keep.iter().map(|&i| cols[i].width.max(cols[i].name.len()) + 1).sum::<usize>();
    // Still too wide: drop the rightmost non-name columns while they are many.
    while need(&keep) > width && keep.len() > 3 {
        let mins: usize = keep.iter().map(|&i| cols[i].name.len().min(6) + 1).sum();
        if mins <= width {
            break;
        }
        keep.pop();
    }
    let mut w: Vec<usize> = keep.iter().map(|&i| cols[i].width.max(cols[i].name.len())).collect();
    let mut total: usize = w.iter().map(|x| x + 1).sum();
    while total > width {
        // Narrow the widest column (at least to its header's first letters).
        let (j, _) = w.iter().enumerate().max_by_key(|(_, x)| **x).unwrap();
        let min = cols[keep[j]].name.len().min(6).max(3);
        if w[j] <= min {
            break;
        }
        let cut = (total - width).min(w[j] - min).max(1);
        w[j] -= cut;
        total -= cut;
    }
    keep.into_iter().zip(w).collect()
}

fn health_style(h: Health, t: &Theme) -> Style {
    match h {
        Health::Ok => t.ok,
        Health::Warn => t.warn,
        Health::Bad => t.bad,
        Health::Done => t.done,
    }
}

fn table(f: &mut Frame, app: &App, tv: &TableView, area: Rect) {
    let ui = &app.kube;
    let t = &ui.theme;
    let mut title = format!("{} [{}", tv.title(), tv.rows.len());
    if tv.rows.len() != tv.total {
        title.push_str(&format!("/{}", tv.total));
    }
    title.push(']');
    if !tv.filter.raw.is_empty() {
        title.push_str(&format!(" </{}>", tv.filter.raw));
    }
    if let Some((l, _)) = &tv.scope.labels {
        title.push_str(&format!(" ‹{l}›"));
    }
    if let Some(fs) = &tv.scope.fields.as_ref().filter(|_| tv.res.plural != CONTAINERS) {
        title.push_str(&format!(" ‹{fs}›"));
    }
    if tv.faults {
        title.push_str(" ⚠faults");
    }
    if !tv.marks.is_empty() {
        title.push_str(&format!(" ◆{}", tv.marks.len()));
    }
    let b = block(trunc(&title, area.width.saturating_sub(4) as usize), t);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.height == 0 {
        return;
    }
    app.list_top.set(inner.y + 1);
    let height = inner.height.saturating_sub(1) as usize;
    tv.height.set(height.max(1));
    if tv.cols.is_empty() || (!tv.synced && tv.rows.is_empty()) {
        let msg = match (&tv.error, &app.kube_error) {
            (Some(e), _) => Line::styled(format!(" {e}"), t.bad),
            (None, Some(e)) if tv.seen_none() => Line::styled(format!(" {e}"), t.bad),
            _ => Line::styled(" loading…", t.dim),
        };
        f.render_widget(Paragraph::new(vec![msg]).wrap(Wrap { trim: false }), inner);
        return;
    }
    let fitted = fit(&tv.cols, inner.width.saturating_sub(1) as usize, tv.wide);
    // Header, with the sort column marked.
    let mut hs = vec![Span::raw(" ")];
    for (i, w) in &fitted {
        let c = &tv.cols[*i];
        let mut name = c.name.clone();
        if let Some((s, desc)) = &tv.sort {
            if *s == c.name {
                name.push(if *desc { '↓' } else { '↑' });
            }
        }
        hs.push(Span::styled(pad(&name, *w, c.numeric), t.dim.add_modifier(Modifier::BOLD)));
        hs.push(Span::raw(" "));
    }
    let mut lines = vec![Line::from(hs)];
    // The visible window keeps the selection on screen.
    let mut off = tv.offset.get().min(tv.rows.len().saturating_sub(1));
    if tv.sel < off {
        off = tv.sel;
    }
    if tv.sel >= off + height {
        off = tv.sel + 1 - height;
    }
    tv.offset.set(off);
    for (i, r) in tv.rows.iter().enumerate().skip(off).take(height) {
        let marked = tv.marks.contains(&r.key());
        let mut style = health_style(r.health, t);
        if marked {
            style = style.patch(t.marked);
        }
        if i == tv.sel {
            style = style.patch(t.selected);
        }
        let mut spans = vec![Span::styled(if marked { "◆" } else { " " }, style)];
        for (ci, w) in &fitted {
            let c = &tv.cols[*ci];
            let text = cell_value(tv, c, r, ui.now);
            spans.push(Span::styled(pad(&text, *w, c.numeric), style));
            spans.push(Span::styled(" ", style));
        }
        // The selection bar spans the width.
        let used: usize = spans.iter().map(|s| s.width()).sum();
        if i == tv.sel && used < inner.width as usize {
            spans.push(Span::styled(" ".repeat(inner.width as usize - used), style));
        }
        lines.push(Line::from(spans));
    }
    if tv.rows.is_empty() {
        let why = if tv.total > 0 { "nothing matches (esc clears the filter)".to_string() } else { format!("no {} here", tv.res.plural) };
        lines.push(Line::styled(format!(" {why}"), t.dim));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

impl TableView {
    fn seen_none(&self) -> bool {
        self.total == 0 && !self.synced
    }
}

/// `text` with the parts `spans` covers highlighted.
fn highlighted(text: &str, spans: &[(usize, usize)], base: Style, hl: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut at = 0;
    for &(a, b) in spans {
        if a < at || b > text.len() || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        if a > at {
            out.push(Span::styled(text[at..a].to_string(), base));
        }
        out.push(Span::styled(text[a..b].to_string(), hl));
        at = b;
    }
    if at < text.len() {
        out.push(Span::styled(text[at..].to_string(), base));
    }
    out
}

fn skip_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

fn pager(f: &mut Frame, p: &Pager, t: &Theme, area: Rect) {
    let mut title = p.title.clone();
    if p.loading {
        title.push_str(" …");
    }
    if let Some(s) = &p.search {
        title.push_str(&format!("  /{} ({} lines)", s.raw, p.matches.len()));
    }
    if p.pending.is_some() {
        title.push_str("  y apply · e edit · esc drop");
    }
    let b = block(trunc(&title, area.width.saturating_sub(4) as usize), t);
    let inner = b.inner(area);
    f.render_widget(b, area);
    let height = inner.height as usize;
    p.height.set(height.max(1));
    let hl = Style::new().add_modifier(Modifier::REVERSED);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(e) = &p.error {
        lines.push(Line::styled(e.clone(), t.bad));
    }
    let top = p.top.min(p.lines.len().saturating_sub(1));
    for l in p.lines.iter().skip(top).take(height) {
        let shown = if p.wrap { l.as_str() } else { skip_chars(l, p.left) };
        if let Some(s) = p.search.as_ref().filter(|s| s.matches_text(l)) {
            lines.push(Line::from(highlighted(shown, &s.spans(shown), Style::new(), hl)));
            continue;
        }
        lines.push(match p.kind {
            PagerKind::Yaml => Line::from(yaml::highlight(shown, t)),
            PagerKind::Diff => Line::styled(shown.to_string(), yaml::diff_style(l, t)),
            PagerKind::Describe => describe_line(shown, t),
            PagerKind::Help => help_line(shown, t),
            _ => Line::raw(shown.to_string()),
        });
    }
    if p.lines.is_empty() && !p.loading && p.error.is_none() {
        lines.push(Line::styled("(no output)", t.dim));
    }
    let mut para = Paragraph::new(lines);
    if p.wrap {
        para = para.wrap(Wrap { trim: false });
    }
    f.render_widget(para, inner);
}

/// Keys of `Key:  value` lines colored.
fn describe_line(l: &str, t: &Theme) -> Line<'static> {
    let indent = l.len() - l.trim_start().len();
    let rest = &l[indent..];
    match rest.find(':') {
        Some(i) if i > 0 && !rest[..i].contains("  ") && (rest.len() == i + 1 || rest.as_bytes()[i + 1] == b' ') => {
            Line::from(vec![Span::raw(l[..indent].to_string()), Span::styled(rest[..=i].to_string(), t.accent.remove_modifier(Modifier::BOLD)), Span::raw(rest[i + 1..].to_string())])
        }
        _ => Line::raw(l.to_string()),
    }
}

fn help_line(l: &str, t: &Theme) -> Line<'static> {
    if let Some(h) = l.strip_prefix("## ") {
        return Line::styled(h.to_string(), t.accent);
    }
    if l.starts_with("  ") && l.len() > 17 {
        let (k, v) = l.split_at(17.min(l.len()));
        return Line::from(vec![Span::styled(k.to_string(), t.key), Span::raw(v.to_string())]);
    }
    Line::raw(l.to_string())
}

/// Colors of log sources' prefixes.
const PALETTE: [Color; 6] = [Color::Cyan, Color::Green, Color::Magenta, Color::Yellow, Color::Blue, Color::LightRed];

fn logs(f: &mut Frame, l: &LogsView, t: &Theme, area: Rect) {
    let ended = l.ended.iter().filter(|e| e.is_some()).count();
    let mut title = format!("logs {}", l.title);
    if l.previous {
        title.push_str(" (previous)");
    }
    title.push_str(if l.follow { "  [follow]" } else { "  [paused]" });
    if l.sources.len() > 1 {
        title.push_str(&format!("  {} streams", l.sources.len()));
    }
    if ended > 0 {
        let why = l.ended.iter().flatten().find(|e| *e != "ended").cloned().unwrap_or_else(|| "ended".into());
        title.push_str(&format!("  {ended} {}", if l.sources.len() == 1 { why } else { format!("ended ({why})") }));
    }
    if let Some(fl) = &l.filter {
        title.push_str(&format!("  /{}", fl.raw));
    }
    let b = block(trunc(&title, area.width.saturating_sub(4) as usize), t);
    let inner = b.inner(area);
    f.render_widget(b, area);
    let height = inner.height as usize;
    l.height.set(height.max(1));
    let shown = l.visible();
    let end = shown.len().saturating_sub(l.offset).max(height.min(shown.len()));
    let start = end.saturating_sub(height);
    let hl = Style::new().add_modifier(Modifier::REVERSED);
    let multi = l.sources.len() > 1;
    let w = l.sources.iter().map(|s| s.chars().count()).max().unwrap_or(0).min(30);
    let lines: Vec<Line> = shown[start..end]
        .iter()
        .map(|&i| {
            let line = &l.lines[i];
            let mut spans = Vec::new();
            if l.timestamps && line.ts_len > 0 {
                let ts = line.ts();
                // 2026-10-19T10:00:00.123456789Z → 10:00:00.123
                let short = ts.get(11..23).unwrap_or(ts);
                spans.push(Span::styled(format!("{short} "), t.dim));
            }
            if multi {
                let name = l.sources.get(line.src as usize).map(String::as_str).unwrap_or("?");
                spans.push(Span::styled(format!("{} ", pad(name, w, false)), Style::new().fg(PALETTE[line.src as usize % PALETTE.len()])));
            }
            let body = line.body();
            match &l.filter {
                Some(fl) => spans.extend(highlighted(body, &fl.spans(body), Style::new(), hl)),
                None => spans.push(Span::raw(body.to_string())),
            }
            Line::from(spans)
        })
        .collect();
    let empty = lines.is_empty();
    let mut para = Paragraph::new(lines);
    if l.wrap {
        para = para.wrap(Wrap { trim: false });
    }
    f.render_widget(para, inner);
    if empty {
        let msg = if l.sources.is_empty() { " connecting…".to_string() } else if l.filter.is_some() { " no line matches".to_string() } else { " no log lines yet".to_string() };
        f.render_widget(Paragraph::new(Span::styled(msg, t.dim)), inner);
    }
}

fn xray(f: &mut Frame, x: &XrayView, t: &Theme, area: Rect) {
    let title = format!("xray {}({}){}", x.res.plural, x.ns.as_deref().unwrap_or("all"), if x.loading { " …" } else { "" });
    let b = block(title, t);
    let inner = b.inner(area);
    f.render_widget(b, area);
    let height = inner.height as usize;
    x.height.set(height.max(1));
    if let Some(e) = &x.error {
        f.render_widget(Paragraph::new(Span::styled(format!(" {e}"), t.bad)).wrap(Wrap { trim: false }), inner);
        return;
    }
    let shown = x.shown();
    let pos = shown.iter().position(|&i| i == x.sel).unwrap_or(0);
    let mut off = x.offset.get();
    if pos < off {
        off = pos;
    }
    if pos >= off + height {
        off = pos + 1 - height;
    }
    x.offset.set(off);
    let lines: Vec<Line> = shown
        .iter()
        .skip(off)
        .take(height)
        .map(|&i| {
            let n = &x.nodes[i];
            let has_kids = x.nodes.get(i + 1).is_some_and(|c| c.depth > n.depth);
            let fold = if !has_kids { "  " } else if x.folded.contains(&i) { "▸ " } else { "▾ " };
            let mut style = health_style(n.health, t);
            if i == x.sel {
                style = style.patch(t.selected);
            }
            let text = format!("{}{fold}{} {}", "  ".repeat(n.depth as usize), n.kind, n.name);
            let mut spans = vec![Span::styled(text.clone(), style)];
            if !n.status.is_empty() {
                spans.push(Span::styled(format!("  {}", n.status), if i == x.sel { style } else { t.dim.patch(health_style(n.health, t)) }));
            }
            if i == x.sel {
                let used: usize = spans.iter().map(|s| s.width()).sum();
                if used < inner.width as usize {
                    spans.push(Span::styled(" ".repeat(inner.width as usize - used), style));
                }
            }
            Line::from(spans)
        })
        .collect();
    if lines.is_empty() && !x.loading {
        f.render_widget(Paragraph::new(Span::styled(format!(" no {} here", x.res.plural), t.dim)), inner);
        return;
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn forwards(f: &mut Frame, ui: &KubeUi, sel: usize, area: Rect) {
    let t = &ui.theme;
    let b = block(format!("port-forwards [{}]", ui.forwards.len()), t);
    let inner = b.inner(area);
    f.render_widget(b, area);
    let mut lines = vec![Line::styled(format!(" {:<22}{:<34}{:<7}{:<6}{}", "LOCAL", "TARGET", "REMOTE", "CONNS", "AGE"), t.dim.add_modifier(Modifier::BOLD))];
    for (i, fw) in ui.forwards.iter().enumerate() {
        let mut style = if fw.error.is_some() { t.warn } else { t.ok };
        if i == sel {
            style = style.patch(t.selected);
        }
        let target = format!("{}/{} → {}", fw.ns, fw.target, fw.pod);
        let mut text = format!(" {:<22}{:<34}{:<7}{:<6}{}", format!("127.0.0.1:{}", fw.local), trunc(&target, 33), fw.remote, fw.connections, super::describe::human((ui.now - fw.started).max(0)));
        if let Some(e) = &fw.error {
            text.push_str(&format!("  {e}"));
        }
        let w = inner.width as usize;
        lines.push(Line::styled(pad(&text, w, false), style));
    }
    if ui.forwards.is_empty() {
        lines.push(Line::styled(" none: shift-f on a pod, service or deployment starts one", t.dim));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn confirm(f: &mut Frame, c: &Confirm, t: &Theme, area: Rect) {
    let mut text: Vec<Line> = c.lines.iter().map(|l| Line::raw(l.clone())).collect();
    if let ConfirmAction::Delete { propagation, force, .. } = &c.action {
        text.push(Line::raw(""));
        text.push(Line::from(vec![Span::styled("p", t.key), Span::raw(format!(" cascade: {}   ", propagation.as_str())), Span::styled("f", t.key), Span::raw(format!(" force (no grace period): {}", if *force { "yes" } else { "no" }))]));
    }
    text.push(Line::raw(""));
    text.push(Line::from(vec![Span::styled("y", t.key), Span::raw(" yes   "), Span::styled("n/esc", t.key), Span::raw(" no")]));
    let width = text.iter().map(|l| l.width()).max().unwrap_or(20).max(c.title.len() + 4).clamp(30, 76) as u16 + 4;
    let inner_w = width.min(area.width.saturating_sub(2)).saturating_sub(2).max(1) as usize;
    let rows: usize = text.iter().map(|l| l.width().max(1).div_ceil(inner_w)).sum();
    let r = centered(area, width, rows as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().border_style(t.warn).title(Span::styled(format!(" {} ", c.title), t.bold))), r);
}

fn input(f: &mut Frame, i: &Input, t: &Theme, area: Rect) {
    let mut lines = vec![Line::from(vec![Span::raw(i.value.clone()), Span::styled("█", t.accent)])];
    if !i.hint.is_empty() {
        lines.push(Line::styled(i.hint.clone(), t.dim));
    }
    if let Some(e) = &i.error {
        lines.push(Line::styled(e.clone(), t.bad));
    }
    lines.push(Line::styled("enter ok   esc cancel   ctrl-u clear", t.dim));
    let r = centered(area, 70, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered().border_style(t.accent).title(Span::styled(format!(" {} ", i.prompt), t.bold))), r);
}

fn picker(f: &mut Frame, p: &Picker, t: &Theme, area: Rect) {
    let shown = p.shown();
    let height = (shown.len() as u16 + 3).clamp(5, area.height.saturating_sub(2).max(5));
    let width = (p.items.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(20) as u16 + 6).clamp(36, 78);
    let r = centered(area, width, height);
    f.render_widget(Clear, r);
    let b = Block::bordered().border_style(t.accent).title(Span::styled(format!(" {} ", p.title), t.bold));
    let inner = b.inner(r);
    f.render_widget(b, r);
    let rows = inner.height.saturating_sub(1) as usize;
    let off = p.sel.saturating_sub(rows.saturating_sub(1));
    let mut lines = vec![Line::from(vec![Span::styled(" ›", t.accent), Span::raw(p.filter.clone()), Span::styled("█", t.accent), Span::styled(if p.filter.is_empty() { "  type to filter" } else { "" }, t.dim)])];
    for (n, &i) in shown.iter().enumerate().skip(off).take(rows) {
        let style = if n == p.sel { t.selected } else { Style::new() };
        lines.push(Line::styled(pad(&format!(" {}", p.items[i].0), inner.width as usize, false), style));
    }
    if shown.is_empty() {
        lines.push(Line::styled(if p.items.is_empty() { " (loading…)" } else { " nothing matches" }, t.dim));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The hint line's keys for the view on top.
pub fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let ui = &app.kube;
    match &ui.overlay {
        Overlay::Prompt(p) => {
            return match p.kind {
                PromptKind::Command => vec![("enter", "run"), ("tab", "complete"), ("↑↓", "history"), ("esc", "cancel")],
                PromptKind::Kubectl => vec![("enter", "run"), ("esc", "cancel")],
                _ => vec![("enter", "keep"), ("tab", "mode"), ("esc", "clear")],
            };
        }
        Overlay::Picker(_) => return vec![("enter", "choose"), ("↑↓", "move"), ("type", "filter"), ("esc", "cancel")],
        Overlay::Confirm(_) | Overlay::Input(_) => return Vec::new(),
        Overlay::None => {}
    }
    match ui.top() {
        View::Table(tv) => {
            let mut v: Vec<(&str, &str)> = vec![(":", "cmd"), ("/", "filter")];
            match tv.res.kind.as_str() {
                "Pod" => v.extend([("l", "logs"), ("s", "shell"), ("d", "describe"), ("y", "yaml"), ("F", "fwd")]),
                "Container" => v.extend([("l", "logs"), ("s", "shell"), ("d", "describe")]),
                "Deployment" | "StatefulSet" => v.extend([("enter", "pods"), ("l", "logs"), ("s", "scale"), ("r", "restart")]),
                "Node" => v.extend([("enter", "pods"), ("c", "cordon"), ("r", "drain"), ("d", "describe")]),
                "Namespace" => v.extend([("enter", "pods"), ("u", "use"), ("d", "describe")]),
                _ => v.extend([("enter", "open"), ("d", "describe"), ("y", "yaml"), ("e", "edit")]),
            }
            v.extend([("m", "menu"), ("n", "ns")]);
            v
        }
        View::Pager(p) if p.pending.is_some() => vec![("y", "apply"), ("e", "edit again"), ("esc", "drop")],
        View::Pager(p) => {
            let mut v = vec![("esc", "back"), ("/", "search"), ("n/N", "next"), ("w", "wrap"), ("c", "copy")];
            if p.obj.is_some() {
                v.insert(1, ("e", "edit"));
            }
            v
        }
        View::Logs(_) => vec![("esc", "back"), ("f", "follow"), ("/", "filter"), ("t", "time"), ("w", "wrap"), ("p", "previous"), ("^s", "save")],
        View::Xray(_) => vec![("esc", "back"), ("enter", "fold"), ("d", "describe"), ("l", "logs"), ("s", "shell")],
        View::Forwards { .. } => vec![("esc", "back"), ("d", "stop")],
    }
}
