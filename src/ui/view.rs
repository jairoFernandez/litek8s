//! Drawing the UI. Everything fits 80x24: a header with the tabs and the
//! cluster's badge (one line on short terminals, a box on tall ones), a
//! footer of key chips, the rest for the current view; wider and taller
//! terminals get more columns, sparklines and side-by-side boxes. The look
//! (colors, glyphs, meters) is in `theme`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, Clear, HighlightSpacing, Padding, Paragraph, Row, Table, TableState, Wrap};

use super::app::{App, Bounds, CRow, DevStatus, Field, InputKind, NewCluster, Overlay, RECLAIMS, SETTINGS, Setting, TABS, Tab};
use super::model::{age, bytes, ibytes};
use super::theme::{Health, Theme};

/// Terminals at least this tall get the boxed header.
const TALL: u16 = 30;

pub fn draw(f: &mut Frame, app: &App) {
    let t = *app.theme();
    let area = f.area();
    let head_h = if area.height >= TALL { 3 } else { 1 };
    let [top, body, bottom] = Layout::vertical([Constraint::Length(head_h), Constraint::Min(1), Constraint::Length(1)]).areas(area);
    app.header_h.set(head_h);
    header(f, app, &t, top);
    app.list_top.set(0);
    app.list_offset.set(0);
    if let Some(logs) = &app.logs {
        log_view(f, &t, logs, body);
    } else {
        match app.tab {
            Tab::Home => home(f, app, &t, body),
            Tab::Clusters => clusters(f, app, &t, body),
            Tab::Containers => containers(f, app, &t, body),
            Tab::Images => images(f, app, &t, body),
            Tab::Volumes => volumes(f, app, &t, body),
            Tab::Space => space(f, app, &t, body),
            Tab::Kube => super::kube::draw::draw(f, app, body),
            Tab::Settings => settings(f, app, &t, body),
            Tab::Observe => observe(f, app, &t, body),
        }
    }
    footer(f, app, &t, bottom);
    long_error(f, app, &t, area);
    match &app.overlay {
        Overlay::None => {}
        Overlay::Help => help(f, &t, area),
        Overlay::Confirm { title, lines, .. } => confirm(f, &t, title, lines, area),
        Overlay::Input { prompt, value, error, kind } => input(f, &t, prompt, value, error.as_deref(), kind, area),
        Overlay::NewCluster(form) => new_cluster(f, app, &t, form, area),
    }
    if t.ascii {
        super::theme::asciify(f.buffer_mut());
    }
}

/// A tab's name, shorter the narrower the terminal (tier 0 is the full one).
fn tab_name(tab: Tab, tier: usize) -> &'static str {
    match (tab, tier) {
        (t, 0) => t.title(),
        (Tab::Space, _) => "Space",
        (Tab::Kube, _) => "Kube",
        (Tab::Clusters, 2..) => "Clust",
        (Tab::Containers, 2..) => "Ctrs",
        (Tab::Images, 2..) => "Imgs",
        (Tab::Volumes, 2..) => "Vols",
        (Tab::Settings, 2..) => "Set",
        (Tab::Observe, 1..) => "Obs",
        (t, _) => t.title(),
    }
}

/// The tabs in `width`: each ` n name ` (the current one a pill), the
/// longest names that fit; then short names one space apart (only the
/// current one padded); on very narrow terminals only the current one has
/// a name. Returns the spans and each tab's columns (for clicks).
fn tab_spans(app: &App, t: &Theme, width: usize) -> (Vec<Span<'static>>, Vec<(u16, u16)>) {
    // Each tab keeps its number (its key) when others are hidden.
    let tabs: Vec<(usize, Tab)> = app.visible_tabs().into_iter().map(|tab| (TABS.iter().position(|x| *x == tab).unwrap_or(0), tab)).collect();
    for tier in 0..5 {
        let tight = tier >= 3;
        let label = |i: usize, tab: Tab| if tier == 4 && tab != app.tab { format!("{}", i + 1) } else { format!("{} {}", i + 1, tab_name(tab, tier.min(2))) };
        // A tight slot is its label and a space; the current one is padded.
        let slot = |i: usize, tab: Tab| label(i, tab).chars().count() + if !tight || tab == app.tab { 2 } else { 1 };
        let need: usize = tabs.iter().map(|(i, tab)| slot(*i, *tab)).sum();
        if need > width && tier < 4 {
            continue;
        }
        let mut spans = Vec::new();
        let mut hit = Vec::new();
        let mut x = 0u16;
        for (i, tab) in &tabs {
            let l = label(*i, *tab);
            let w = slot(*i, *tab) as u16;
            hit.push((x, x + w));
            x += w;
            if *tab == app.tab {
                spans.push(Span::styled(format!(" {l} "), t.pill));
            } else {
                let (num, name) = l.split_once(' ').unwrap_or((&l, ""));
                let lead = if tight { "" } else { " " };
                spans.push(Span::styled(format!("{lead}{num}"), t.key.remove_modifier(Modifier::BOLD)));
                spans.push(Span::raw(if name.is_empty() { " ".to_string() } else { format!(" {name} ") }));
            }
        }
        return (spans, hit);
    }
    unreachable!()
}

/// `● dev · macOS VM · up 2h 1m`: the shown cluster and its state, as
/// short as `room` needs.
fn badge(app: &App, t: &Theme, room: usize) -> Vec<Span<'static>> {
    let h = &app.home;
    let (health, style) = match h.status {
        DevStatus::Up => (Health::Ok, t.ok),
        DevStatus::Starting | DevStatus::Stopping | DevStatus::Unknown => (Health::Warn, t.warn),
        DevStatus::Down => (Health::Done, t.dim),
        DevStatus::Remote => (Health::Ok, t.info),
    };
    // A remote litek8s is named by its context, not by this machine's
    // dev cluster the other views may still be about.
    let name = match (&app.context_override, h.status) {
        (Some(c), _) => c.clone(),
        (None, DevStatus::Remote) => app.kube_context.clone(),
        (None, _) => app.cluster.clone(),
    };
    let mut parts: Vec<String> = Vec::new();
    match h.status {
        DevStatus::Up | DevStatus::Starting => {
            if h.status == DevStatus::Starting {
                parts.push("starting".into());
            }
            parts.push(
                match h.mode.as_str() {
                    "vm" => "macOS VM",
                    "rootless" => "rootless sandbox",
                    "root" => "sandbox (root)",
                    m => m,
                }
                .to_string(),
            );
            if h.uptime_s > 0 && h.status == DevStatus::Up {
                parts.push(format!("up {}", duration(h.uptime_s)));
            }
        }
        s => parts.push(s.label().to_string()),
    }
    parts.retain(|p| !p.is_empty());
    // Drop the details from the right until it fits.
    while !parts.is_empty() && 2 + name.chars().count() + parts.iter().map(|p| p.chars().count() + 3).sum::<usize>() > room {
        parts.pop();
    }
    if 2 + name.chars().count() > room {
        return Vec::new();
    }
    let mut spans = vec![Span::styled(format!("{} ", health.glyph()), style), Span::styled(name, t.bold)];
    for p in parts {
        spans.push(Span::styled(" · ", t.dim));
        spans.push(Span::styled(p, t.dim));
    }
    spans
}

fn header(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    if area.height >= 3 {
        // ╭─ litek8s ─────────────── ● dev · macOS VM · up 2h ─╮
        // │  1 Home   2 Clusters  …                             │
        let inner_w = area.width.saturating_sub(2) as usize;
        let build = build_span(app, t, inner_w / 3);
        let room = inner_w.saturating_sub(14 + build.width());
        let mut title = vec![Span::styled("litek8s", t.accent)];
        if build.width() > 0 {
            title.extend([Span::raw(" "), build]);
        }
        let b = t.block_with(Line::from(title), badge(app, t, room));
        let inner = b.inner(area);
        f.render_widget(b, area);
        let (mut spans, hit) = tab_spans(app, t, inner_w.saturating_sub(1));
        spans.insert(0, Span::raw(" "));
        let x0 = inner.x + 1;
        *app.tab_spans.borrow_mut() = hit.into_iter().map(|(a, b)| (a + x0, b + x0)).collect();
        app.tab_row.set(inner.y);
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { height: 1, ..inner });
        return;
    }
    //  litek8s  1 Home  2 Clusters  …           ● dev · up 2h
    let mut logo = vec![Span::styled(" litek8s", t.accent), Span::raw(" ")];
    let build = build_span(app, t, area.width as usize / 4);
    if build.width() > 0 {
        logo.extend([build, Span::raw(" ")]);
    }
    let lw = logo.iter().map(|s| s.width()).sum::<usize>() as u16;
    let (tabs, hit) = tab_spans(app, t, area.width.saturating_sub(lw) as usize);
    let tw: usize = tabs.iter().map(|s| s.width()).sum();
    let room = (area.width as usize).saturating_sub(lw as usize + tw + 1);
    let mut right = badge(app, t, room);
    if !right.is_empty() && right.iter().map(|s| s.width()).sum::<usize>() < room {
        right.push(Span::raw(" "));
    }
    let rw = right.iter().map(|s| s.width()).sum::<usize>() as u16;
    let mut spans = logo;
    spans.extend(tabs);
    *app.tab_spans.borrow_mut() = hit.into_iter().map(|(a, b)| (a + area.x + lw, b + area.x + lw)).collect();
    app.tab_row.set(area.y);
    let [l, r] = Layout::horizontal([Constraint::Min(1), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(spans)), l);
    f.render_widget(Paragraph::new(Line::from(right)).right_aligned(), r);
}

/// `DEV <commit> · <LITEK8S_HOME>` in at most `w` columns (nothing for the
/// plain release).
fn build_span(app: &App, t: &Theme, w: usize) -> Span<'static> {
    if app.build.is_empty() || w < 4 {
        return Span::raw("");
    }
    Span::styled(trunc(&app.build, w), t.warn.add_modifier(Modifier::BOLD))
}

/// The keys of the view on screen, as chips, or the status line.
fn footer(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let boxed = is_long_error(app, area.width);
    let help = vec![Span::styled(" ? ", t.chip), Span::styled(" help ", t.dim)];
    let hw = help.iter().map(|s| s.width()).sum::<usize>() as u16;
    let [left, right] = Layout::horizontal([Constraint::Min(1), Constraint::Length(hw)]).areas(area);
    let line = if let Some((text, error)) = app.status.as_ref().filter(|_| !boxed) {
        let (glyph, style) = if *error { ("✕", t.bad) } else { ("✓", t.ok) };
        Line::from(vec![Span::styled(format!(" {glyph} "), style.add_modifier(Modifier::BOLD)), Span::styled(text.clone(), style)])
    } else {
        let kube_keys;
        let keys: &[(&str, &str)] = if app.logs.is_some() {
            &[("esc", "back"), ("f", "follow"), ("w", "wrap"), ("↑↓", "scroll"), ("g/G", "top/end")]
        } else {
            match app.tab {
                // A remote litek8s is not started or stopped from here.
                Tab::Home if app.home.status == DevStatus::Remote => &[("n", "cluster/node"), ("b", "breakdown"), ("c", "copy API"), ("7", "kube"), ("2", "clusters")],
                Tab::Home => &[("u", "up"), ("d", "down"), ("R", "delete"), ("b", "breakdown"), ("c", "copy API"), ("2", "clusters")],
                Tab::Clusters => &[("n", "new"), ("u", "up"), ("d", "down"), ("D", "delete"), ("t", "tree"), ("enter", "use")],
                Tab::Containers => &[("l", "logs"), ("s", "shell"), ("u", "start"), ("x", "stop"), ("r", "restart"), ("d", "remove"), ("/", "filter")],
                Tab::Images | Tab::Volumes => &[("d", "remove"), ("/", "filter"), ("6", "free space")],
                Tab::Space => &[("space", "select"), ("p", "free selected")],
                Tab::Kube => {
                    kube_keys = super::kube::draw::hints(app);
                    &kube_keys
                }
                Tab::Settings => &[("enter", "edit"), ("del", "default"), ("space", "toggle")],
                Tab::Observe => &[("l", "logs"), ("L", "search logs"), ("s", "sort"), ("a/A", "ack"), ("n", "namespace")],
            }
        };
        Line::from(key_chips(t, keys, left.width as usize))
    };
    f.render_widget(Paragraph::new(line), left);
    f.render_widget(Paragraph::new(Line::from(help)).right_aligned(), right);
}

/// `[ l ] logs  [ s ] shell …` in `width`: padded chips if they all fit,
/// else tight ones, else the first that fit whole.
pub fn key_chips(t: &Theme, keys: &[(&str, &str)], width: usize) -> Vec<Span<'static>> {
    let sized = |pad: bool| -> Vec<(Span<'static>, Span<'static>)> {
        keys.iter()
            .map(|(k, v)| {
                let chip = if pad { format!(" {k} ") } else { k.to_string() };
                (Span::styled(chip, t.chip), Span::styled(format!(" {v}  "), t.dim))
            })
            .collect()
    };
    let width_of = |v: &[(Span, Span)]| 1 + v.iter().map(|(a, b)| a.width() + b.width()).sum::<usize>();
    let mut chips = sized(true);
    if width_of(&chips) > width {
        chips = sized(false);
    }
    let mut out = vec![Span::raw(" ")];
    let mut used = 1;
    for (a, b) in chips {
        let w = a.width() + b.width();
        // The last label's trailing spaces may go.
        if used + w > width && used + w - 2 > width {
            break;
        }
        used += w;
        out.push(a);
        out.push(b);
    }
    out
}

/// An error too long for the footer, wrapped in a box just above it
/// (it goes with the status line: after 8 s or a key).
fn is_long_error(app: &App, width: u16) -> bool {
    matches!(&app.status, Some((text, true)) if text.chars().count() + 3 > width.saturating_sub(10) as usize)
}

fn long_error(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let Some((text, true)) = &app.status else { return };
    let inner = area.width.saturating_sub(2).max(1) as usize;
    if !is_long_error(app, area.width) {
        return;
    }
    let rows = (text.chars().count().div_ceil(inner) as u16).clamp(1, 5);
    let r = Rect { x: area.x, y: area.y + area.height.saturating_sub(rows + 3), width: area.width, height: rows + 2 };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(Span::styled(text.clone(), t.bad)).wrap(Wrap { trim: false }).block(t.dialog(Line::from(vec![Span::styled("✕ ", t.bad), Span::styled("error", t.bold)]), t.bad)), r);
}

fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A string shortened from the left: `…/litek8s/docker.sock`.
fn trunc_left(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - w.saturating_sub(1)).collect();
    format!("…{tail}")
}

fn home_dir_short(s: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.contains(&h) => s.replace(&h, "~"),
        _ => s.to_string(),
    }
}

fn duration(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        3600..=86399 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}

fn status_health(s: DevStatus) -> Health {
    match s {
        DevStatus::Up | DevStatus::Remote => Health::Ok,
        DevStatus::Starting | DevStatus::Stopping | DevStatus::Unknown => Health::Warn,
        DevStatus::Down => Health::Done,
    }
}

/// The base style of a row by its health: plain while fine, dim when done,
/// yellow or red otherwise (the glyph carries the green).
fn row_style(t: &Theme, h: Health) -> Style {
    match h {
        Health::Ok => Style::new(),
        Health::Done => t.dim,
        h => t.health(h),
    }
}

/// Home's headline figures: (ratio, text) for CPU, memory and disk, and
/// how memory stacks up when its breakdown is known.
struct Figures {
    cpu: (Option<f64>, String),
    mem: (Option<f64>, String),
    disk: (Option<f64>, String),
    stack: Option<Stack>,
}

/// The memory meter's parts: (share of the meter, style, glyph), and the
/// color its percentage takes (from how short of memory it is, not from
/// how full the meter looks).
struct Stack {
    parts: Vec<(f64, Style, &'static str)>,
    level: f64,
}

fn gib(b: u64) -> String {
    format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
}

/// Memory against its limit: what its owners hold (in use) out of the VM's
/// `--memory`, the sandbox's limit or the host's RAM, as stacked parts
/// (litek8s, pods, other clusters, the base, then cache and freed pages,
/// which are not in use); colored by the guest's own pressure. macOS's
/// footprint of the VM also counts pages the guest freed long ago, so it
/// can exceed the VM's size while the guest has plenty available. The
/// Docker API's containers are left out: they are not the cluster's (the
/// docker box shows them).
fn memory_stack(t: &Theme, b: &crate::dev::memory::Breakdown) -> Option<((Option<f64>, String), Stack)> {
    use crate::dev::memory::Seg;
    let limit = b.limit.filter(|l| *l > 0)?;
    let in_use = b.cluster_in_use();
    let all: u64 = b.lines.iter().map(|l| l.bytes).sum();
    let scale = all.max(limit) as f64;
    let parts = b
        .lines
        .iter()
        .filter(|l| l.seg != Seg::Docker)
        .map(|l| {
            let (style, glyph) = match l.seg {
                Seg::Litek8s => (t.accent.remove_modifier(Modifier::BOLD), "█"),
                Seg::Pods => (t.info, "█"),
                Seg::Docker => (t.warn, "█"),
                Seg::Others => (t.marked.remove_modifier(Modifier::BOLD), "█"),
                Seg::Base => (t.done, "█"),
                Seg::Cache => (t.dim, "▒"),
                Seg::Freed => (t.track, "▒"),
            };
            (l.bytes as f64 / scale, style, glyph)
        })
        .collect();
    // Short of memory from 0.5 on (half of it unavailable is normal):
    // yellow at 0.75, red past 0.9.
    let level = ((b.pressure.unwrap_or(in_use as f64 / limit as f64) - 0.5) / 0.5).clamp(0.0, 1.0);
    // Under a GiB in MB, so a sliver never reads "0.0 GiB".
    let used = if in_use < 1 << 30 { bytes(in_use) } else { gib(in_use) };
    let mut text = format!("{used} in use · {} {}", b.limit_what, gib(limit));
    if level >= 0.8 {
        text.push_str(" · short");
    }
    Some(((Some(in_use as f64 / limit as f64), text), Stack { parts, level }))
}

fn figures(app: &App) -> Figures {
    let h = &app.home;
    let cpus = app.home_cpus();
    let mem_total = app.home_memory();
    let remote_cpu = app.cpu.as_ref().filter(|_| h.status == DevStatus::Remote && h.cpu_pct.is_none());
    let cpu = match (app.cluster_cpu_pct(), cpus) {
        _ if remote_cpu.is_some() => {
            let c = remote_cpu.unwrap();
            (Some(c.total_pct / 100.0 / c.cpus.max(1.0)), format!("of {:.0} CPUs · {} of one", c.cpus, pct(c.total_pct, 0)))
        }
        (Some(p), Some(c)) => (Some(p / 100.0 / c.max(1.0)), format!("of {c:.0} CPUs · {} of one", pct(p, 0))),
        (Some(p), None) => (Some(p / 100.0), format!("{p:.0}%")),
        (None, Some(c)) => (None, format!("{c:.0} CPUs")),
        (None, None) => (None, "-".into()),
    };
    let stacked = h.memory.as_ref().and_then(|b| memory_stack(app.theme(), b));
    let mem = match (h.mem_used, mem_total) {
        _ if stacked.is_some() => stacked.as_ref().unwrap().0.clone(),
        (Some(u), Some(t)) => (Some(u as f64 / t.max(1) as f64), format!("{} of {}", bytes(u), bytes(t))),
        (Some(u), None) => (None, bytes(u)),
        (None, Some(t)) => (None, format!("- / {}", bytes(t))),
        _ => (None, "-".into()),
    };
    let disk = disk_figure(h);
    Figures { cpu, mem, disk, stack: stacked.map(|(_, s)| s) }
}

/// The disk meter. The macOS VM's: what its data disk holds against its
/// size, or against what the Mac has left when that is less (a sparse
/// image grows only as far as the Mac's free space), and where to grow
/// it when it is nearly full. Elsewhere: used against used and free.
fn disk_figure(h: &super::app::HomeSample) -> (Option<f64>, String) {
    if let (Some((alloc, size)), Some(free)) = (h.vm_disk.filter(|(_, s)| *s > 0), h.disk_free) {
        // What the guest's filesystem holds (the image may hold blocks the
        // guest has freed and not trimmed yet).
        let used = h.disk_node.as_ref().map(|(n, _, _)| n.fs_used).filter(|u| *u > 0).unwrap_or(alloc);
        let room = size.min(used + free).max(1);
        let ratio = used as f64 / room as f64;
        let g = |b: u64| format!("{:.1}", b as f64 / (1u64 << 30) as f64);
        let mut text = format!("{}/{} GiB · Mac {} free", g(used), g(size), bytes(free));
        if ratio >= 0.8 {
            // Out of room on the Mac: growing the VM disk does not help.
            text.push_str(if used + free < size { " · Mac short" } else { " · grow: 8" });
        }
        return (Some(ratio), text);
    }
    let text = match (h.disk_used, h.disk_free) {
        (Some(u), Some(free)) => format!("{} used · {} free", bytes(u), bytes(free)),
        (Some(u), None) => format!("{} used", bytes(u)),
        (None, Some(free)) => format!("{} free", bytes(free)),
        _ => "-".into(),
    };
    (h.disk_used.zip(h.disk_free).map(|(u, free)| u as f64 / (u + free).max(1) as f64), text)
}

/// `cpu    ▁▂▃▅  ███▍░░░░░  3% of 4 CPUs`: a label, an optional sparkline,
/// a meter `mw` wide and the figure.
#[allow(clippy::too_many_arguments)]
fn meter_line(t: &Theme, label: &str, spark: Option<&[f64]>, spark_max: f64, ratio: Option<f64>, text: &str, stack: Option<&Stack>, mw: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(format!(" {label:<7}"), t.dim)];
    if let Some(values) = spark {
        spans.extend(t.sparkline(values, spark_max, 14));
        spans.push(Span::raw(" "));
    }
    match ratio {
        Some(r) => spans.extend(meter_spans(t, r, text, stack, mw)),
        None => spans.push(Span::styled(text.to_string(), t.dim)),
    }
    Line::from(spans)
}

/// `███▍░░░  12% text`: a plain meter, or the stacked one with its
/// percentage colored by pressure.
fn meter_spans(t: &Theme, r: f64, text: &str, stack: Option<&Stack>, mw: usize) -> Vec<Span<'static>> {
    let mut spans = match stack {
        Some(s) => t.stacked(&s.parts, mw),
        None => t.meter(r, mw),
    };
    let level = stack.map(|s| s.level).unwrap_or(r);
    spans.push(Span::styled(format!(" {:>4} ", pct(r * 100.0, 0)), t.level(level).add_modifier(Modifier::BOLD)));
    match text.strip_suffix(" · short") {
        Some(head) => {
            spans.push(Span::raw(head.to_string()));
            spans.push(Span::styled(" · short", t.bad.add_modifier(Modifier::BOLD)));
        }
        None => spans.push(Span::raw(text.to_string())),
    }
    spans
}

/// Chips after a label, wrapped under it when they do not fit in `width`.
fn chip_lines(t: &Theme, label: &str, chips: Vec<Vec<Span<'static>>>, width: usize) -> Vec<Line<'static>> {
    let indent = label.chars().count();
    let mut lines = Vec::new();
    let mut cur = vec![Span::styled(label.to_string(), t.dim)];
    let mut used = indent;
    for chip in chips {
        let w: usize = chip.iter().map(|s| s.width()).sum();
        if used > indent && used + w > width {
            lines.push(Line::from(std::mem::take(&mut cur)));
            cur.push(Span::raw(" ".repeat(indent)));
            used = indent;
        }
        used += w;
        cur.extend(chip);
    }
    lines.push(Line::from(cur));
    lines
}

/// One state's chip: `● 12 running`.
fn chip(t: &Theme, h: Health, count: String, word: &str) -> Vec<Span<'static>> {
    vec![Span::styled(format!("{} ", h.glyph()), t.health(h)), Span::styled(count, t.bold), Span::raw(format!(" {word}   "))]
}

/// Pods by state: `● 12 running  ◐ 1 pending  ✕ 1 crashloop  ○ 3 completed`.
fn pod_chips(app: &App, t: &Theme) -> Vec<Vec<Span<'static>>> {
    let mut n = [0usize; 4];
    let mut crash = false;
    for p in &app.pods {
        let h = Health::of_pod(&p.status, &p.ready);
        crash |= p.status.contains("CrashLoop");
        n[h as usize] += 1;
    }
    let words = ["running", "pending", if crash { "crashloop" } else { "failed" }, "completed"];
    let mut out = Vec::new();
    for (i, h) in [Health::Ok, Health::Warn, Health::Bad, Health::Done].into_iter().enumerate() {
        if n[i] == 0 && !(i == 0 && n.iter().all(|c| *c == 0)) {
            continue;
        }
        out.push(chip(t, h, n[i].to_string(), words[i]));
    }
    out
}

/// Containers by state: `● 2/4 running  ○ 1 exited  ✕ 1 failed`.
fn container_chips(app: &App, t: &Theme) -> Vec<Vec<Span<'static>>> {
    let mut n = [0usize; 4];
    for c in &app.containers {
        n[Health::of_container(&c.state, &c.status) as usize] += 1;
    }
    let mut out = vec![chip(t, Health::Ok, format!("{}/{}", n[0], app.containers.len()), "running")];
    for (i, h, word) in [(1, Health::Warn, "starting"), (2, Health::Bad, "failed"), (3, Health::Done, "exited")] {
        if n[i] > 0 {
            out.push(chip(t, h, n[i].to_string(), word));
        }
    }
    out
}

fn home(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    if area.width >= 100 && area.height >= 26 {
        return home_wide(f, app, t, area);
    }
    let h = &app.home;
    let fig = figures(app);
    let down = matches!(h.status, DevStatus::Down);
    let engine_down = down || app.docker_error.is_some();
    let kube_down = down || app.kube_error.is_some();
    // One column: the resources, where the tools point, then the
    // breakdowns (`b` picks which comes first) while there is room.
    let out_rows = if app.dev_output.is_empty() { 1 } else { output_rows(app, area.width) };
    let remote = h.status == DevStatus::Remote;
    // A remote litek8s: its nodes in the box; else this machine's Docker
    // in a box of its own under the cluster's.
    let res_h = if remote { 7u16 } else { 6 }.min(area.height);
    let docker_h = if remote { 0 } else { 3u16.min(area.height.saturating_sub(res_h)) };
    // The breakdown `b` picked first, then the connections if they fit,
    // then the other breakdowns while there is room.
    let mut avail = area.height.saturating_sub(res_h + docker_h + out_rows);
    let all = explain_boxes(app);
    let n = all.len();
    let mut boxes: Vec<(&Explain, u16)> = Vec::new();
    let start = all.iter().position(|b| b.kind >= app.explain).unwrap_or(0);
    for i in 0..n {
        let b = &all[(start + i) % n];
        let want = b.lines.len() as u16 + 2;
        let room = if i == 0 { avail } else { avail.saturating_sub(5) };
        if room < 4 || (i > 0 && want > room) {
            break;
        }
        let r = want.min(room);
        avail -= r;
        boxes.push((b, r));
    }
    let conn_h = if avail >= 5 { 5 } else { 0 };
    let rows: u16 = boxes.iter().map(|(_, r)| r).sum();
    let [res, docker, explained, conn, out, _] = Layout::vertical([
        Constraint::Length(res_h),
        Constraint::Length(docker_h),
        Constraint::Length(rows),
        Constraint::Length(conn_h),
        Constraint::Length(out_rows),
        Constraint::Fill(1),
    ])
    .areas(area);

    let w = res.width.saturating_sub(2) as usize;
    let history = (w >= 88 && !app.history.is_empty()).then_some(());
    let cpu_hist: Vec<f64> = app.history.iter().map(|(c, _)| *c as f64).collect();
    let mem_hist: Vec<f64> = app.history.iter().map(|(_, m)| *m as f64).collect();
    let cpu_max = cpu_hist.iter().cloned().fold(0.1, f64::max);
    // One meter width for the three, from the longest figure.
    let tw = [&fig.cpu.1, &fig.mem.1, &fig.disk.1].iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let mw = w.saturating_sub(8 + if history.is_some() { 15 } else { 0 } + 7 + tw).clamp(4, 36);
    let mut lines = vec![
        meter_line(t, "cpu", history.map(|_| cpu_hist.as_slice()), cpu_max, fig.cpu.0, &fig.cpu.1, None, mw),
        meter_line(t, "memory", history.map(|_| mem_hist.as_slice()), 1.0, fig.mem.0, &fig.mem.1, fig.stack.as_ref(), mw),
        meter_line(t, "disk", history.map(|_| &[][..]), 1.0, fig.disk.0, &fig.disk.1, None, mw),
    ];
    let flat = |chips: Vec<Vec<Span<'static>>>| chips.into_iter().flatten().collect::<Vec<_>>();
    let mut pods = vec![Span::styled(" pods   ", t.dim)];
    if kube_down {
        pods.push(Span::styled("–", t.dim));
    } else {
        pods.extend(flat(pod_chips(app, t)));
    }
    lines.push(Line::from(pods));
    if remote {
        let mut nodes = vec![Span::styled(" nodes  ", t.dim)];
        let names = app.node_reports.names();
        nodes.push(Span::styled(names.len().to_string(), t.bold));
        nodes.push(Span::raw(format!(" · showing {}", app.scope.as_deref().map(|n| format!("node {n}")).unwrap_or_else(|| "the cluster".into()))));
        if names.len() > 1 {
            nodes.push(Span::styled("  n: next", t.dim));
        }
        lines.push(Line::from(nodes));
    }
    let state = badge(app, t, (res.width as usize).saturating_sub(18));
    f.render_widget(Paragraph::new(lines).block(t.block_with(Line::from(state), vec![Span::styled("cluster", t.dim)])), res);
    if docker_h > 0 {
        let mut line = vec![Span::raw(" ")];
        if engine_down {
            line.push(Span::styled(if down { "– (engine not running)" } else { "–" }, t.dim));
        } else {
            line.extend(flat(container_chips(app, t)));
            line.extend(docker_store_spans(app, t, false));
        }
        f.render_widget(Paragraph::new(Line::from(line)).block(docker_block(app, t, docker.width)), docker);
    }

    if conn_h > 0 {
        connections(f, app, t, conn, false);
    }
    let mut y = explained.y;
    for (b, r) in &boxes {
        explain_box(f, t, b, Rect { y, height: *r, ..explained }, boxes.len() < n);
        y += r;
    }
    if !app.dev_output.is_empty() {
        dev_output(f, app, t, out);
    } else {
        tip(f, app, t, out);
    }
}

/// What the Docker API's containers take, apart from the cluster's
/// figures: on the cluster's own socket, their share of the VM or sandbox
/// (from the breakdowns); on another engine (`--docker-host`), only what
/// that engine says it has.
fn docker_figures(app: &App) -> String {
    if app.docker_error.is_some() {
        return String::new();
    }
    if !app.docker_is_cluster {
        return match (app.vm_cpus, app.vm_memory) {
            (Some(c), Some(m)) => format!("its engine: {c} CPUs · {}", bytes(m)),
            (Some(c), None) => format!("its engine: {c} CPUs"),
            _ => String::new(),
        };
    }
    let cpu = app.cpu.as_ref().map(|c| format!("cpu {} of one", pct(c.docker_pct, 1)));
    let mem = app.home.memory.as_ref().map(|m| format!("mem {}", bytes(m.docker())));
    [cpu, mem].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

/// The docker box's border: whose socket, and what its containers take.
fn docker_block(app: &App, t: &Theme, width: u16) -> ratatui::widgets::Block<'static> {
    let figures = docker_figures(app);
    let socket = app.docker_endpoint.strip_prefix("unix://").unwrap_or(&app.docker_endpoint);
    let room = (width as usize).saturating_sub(figures.chars().count() + 20);
    let title = Line::from(vec![Span::styled("docker", t.title), Span::styled(format!(" · {}", trunc_left(&home_dir_short(socket), room)), t.dim)]);
    if figures.is_empty() { t.block(title) } else { t.block_with(title, vec![Span::styled(figures, t.dim)]) }
}

/// `images 3 · 235 MB   volumes 2 · 52 MB` (volumes when `always` or
/// there are some).
fn docker_store_spans(app: &App, t: &Theme, always: bool) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("images ", t.dim), Span::raw(format!("{} · {}", app.images.len(), bytes(app.images.iter().map(|i| i.size).sum())))];
    if always || !app.volumes.is_empty() {
        spans.push(Span::styled("   vols ", t.dim));
        spans.push(Span::raw(format!("{} · {}", app.volumes.len(), bytes(app.volumes.iter().filter_map(|v| v.size).sum()))));
    }
    spans
}

fn tip(f: &mut Frame, app: &App, t: &Theme, out: Rect) {
    let tip = match app.home.status {
        DevStatus::Down => format!("{} is down: press u to start it (2: all clusters).", app.cluster),
        DevStatus::Remote => "Connected to a litek8s that is not this machine's dev cluster.".into(),
        _ => "Switch docker/kubectl to litek8s in Settings (8). Help: ?".into(),
    };
    f.render_widget(Paragraph::new(Span::styled(format!(" {tip}"), t.dim)), out);
}

/// The API server, Docker socket and engine, and where `docker` and
/// `kubectl` point: two columns.
fn connections(f: &mut Frame, app: &App, t: &Theme, area: Rect, wide: bool) {
    let h = &app.home;
    let down = matches!(h.status, DevStatus::Down);
    let b = t.block("connections · your tools point at");
    let inner = b.inner(area);
    f.render_widget(b, area);
    let [l, r] = Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)]).areas(inner);
    let w = (l.width as usize).saturating_sub(10);
    let kv = |k: &str, v: String, style: Style| Line::from(vec![Span::styled(format!(" {k:<8}"), t.dim), Span::styled(v, style)]);
    let mut left = Vec::new();
    let api_style = if app.kube_error.is_some() && !down { t.bad } else { Style::new() };
    left.push(kv("API", trunc(&app.kube_endpoint, w), api_style));
    // Looking at a remote litek8s, Docker is this machine's dev cluster's:
    // down is expected there, not a fault of the cluster on screen.
    let docker_fault = app.docker_error.is_some() && !down && !app.docker_is_local_dev;
    let docker_style = if docker_fault { t.bad } else { Style::new() };
    let socket = app.docker_endpoint.strip_prefix("unix://").unwrap_or(&app.docker_endpoint);
    left.push(kv("Docker", trunc_left(&home_dir_short(socket), w), docker_style));
    let engine = match &app.docker_error {
        Some(_) if down => "not running".into(),
        Some(_) if app.docker_is_local_dev => "not running (this machine's dev cluster)".into(),
        Some(e) => trunc(e, w),
        None if app.docker_version.is_empty() => "-".into(),
        None => trunc(&format!("{} ({})", app.docker_version, app.docker_os), w),
    };
    left.push(kv("Engine", engine, if docker_fault { t.bad } else { t.dim }));
    if let Some(ip) = h.vm_ip.as_ref().filter(|_| wide) {
        left.push(kv("VM IP", ip.clone(), t.dim));
    }
    f.render_widget(Paragraph::new(left), l);

    let c = &h.contexts;
    let mark = |ok: bool| if ok { Span::styled("✓ ", t.ok) } else { Span::styled("✗ ", t.dim) };
    let cw = (r.width as usize).saturating_sub(19);
    let mut right = vec![Line::from(vec![mark(c.docker_context_is_litek8s), Span::styled("docker context   ", t.dim), Span::raw(trunc(&c.docker_context, cw))])];
    match &c.docker_host_env {
        Some(v) => right.push(Line::from(vec![
            mark(c.docker_host_is_litek8s),
            Span::styled("DOCKER_HOST      ", t.dim),
            Span::styled(trunc(&home_dir_short(v), cw), if c.docker_host_is_litek8s { Style::new() } else { t.warn }),
        ])),
        None => right.push(Line::from(vec![Span::raw("  "), Span::styled("DOCKER_HOST      ", t.dim), Span::styled("unset", t.dim)])),
    }
    let kname = if c.kube_context.is_empty() { "-".to_string() } else { c.kube_context.clone() };
    right.push(Line::from(vec![mark(c.kube_is_litek8s), Span::styled("kubectl context  ", t.dim), Span::raw(trunc(&kname, cw))]));
    if wide {
        right.push(Line::from(vec![Span::raw("  "), Span::styled("kubeconfig       ", t.dim), Span::styled(trunc_left(&home_dir_short(&c.kubeconfig), cw), t.dim)]));
    }
    f.render_widget(Paragraph::new(right), r);
}

/// Wide and tall: CPU and memory side by side with their history and
/// what they are made of, disk and the workloads under them, then the
/// connections.
fn home_wide(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let h = &app.home;
    let fig = figures(app);
    let down = matches!(h.status, DevStatus::Down);
    let engine_down = down || app.docker_error.is_some();
    let kube_down = down || app.kube_error.is_some();
    let out_rows = if app.dev_output.is_empty() { 1 } else { output_rows(app, area.width) };
    let conn_h = 6u16;
    let all = explain_boxes(app);
    let find = |k: usize| all.iter().find(|b| b.kind == k);
    let (mem_b, cpu_b, disk_b) = (find(0), find(1), find(2));
    let lines_of = |b: Option<&Explain>| b.map(|b| b.lines.len() as u16).unwrap_or(0);
    let need_a = 4 + lines_of(cpu_b).max(lines_of(mem_b));
    // The pods' box and the docker box (containers, images) beside disk.
    let need_b = (3 + lines_of(disk_b)).max(if h.status == DevStatus::Remote { 6 } else { 7 });
    let room = area.height.saturating_sub(conn_h + out_rows);
    // Room to spare goes to the history graphs (up to 8 rows).
    let graph_h = 1 + room.saturating_sub(need_a + need_b).min(7);
    let need_a = need_a + graph_h - 1;
    let (ha, hb) = if need_a + need_b <= room {
        (need_a, need_b)
    } else {
        let ha = need_a.min(room.saturating_sub(6).max(4));
        (ha, room.saturating_sub(ha).min(need_b))
    };
    let [row_a, row_b, conn, out, _] =
        Layout::vertical([Constraint::Length(ha), Constraint::Length(hb), Constraint::Length(conn_h), Constraint::Length(out_rows), Constraint::Fill(1)]).areas(area);
    let [cpu_r, mem_r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(row_a);
    let [disk_r, work_r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(row_b);

    let cpu_hist: Vec<f64> = app.history.iter().map(|(c, _)| *c as f64).collect();
    let mem_hist: Vec<f64> = app.history.iter().map(|(_, m)| *m as f64).collect();
    let cpu_max = cpu_hist.iter().cloned().fold(0.1, f64::max);
    resource_box(f, t, cpu_r, "cpu", cpu_b, Some((&cpu_hist, cpu_max)), graph_h, fig.cpu.0, fig.cpu.1.clone(), None);
    resource_box(f, t, mem_r, "memory", mem_b, Some((&mem_hist, 1.0)), graph_h, fig.mem.0, fig.mem.1.clone(), fig.stack.as_ref());
    resource_box(f, t, disk_r, "disk", disk_b, None, 0, fig.disk.0, fig.disk.1.clone(), None);

    let mut lines = Vec::new();
    let ww = work_r.width.saturating_sub(2) as usize;
    if kube_down {
        lines.push(Line::from(vec![Span::styled(" pods        ", t.dim), Span::styled("–", t.dim)]));
    } else {
        lines.extend(chip_lines(t, " pods        ", pod_chips(app, t), ww));
    }
    let remote = h.status == DevStatus::Remote;
    if remote {
        // A remote litek8s: its nodes (this machine's Docker is not its).
        lines.extend(node_lines(app, t, ww));
        f.render_widget(Paragraph::new(lines).block(t.block("workloads · nodes")), work_r);
    } else {
        // The cluster's pods, then the Docker API's containers in a box of
        // their own: they run on the node but are not the cluster's.
        let pods_h = (lines.len() as u16 + 2).min(work_r.height.saturating_sub(3));
        let [pods_r, docker_r] = Layout::vertical([Constraint::Length(pods_h), Constraint::Fill(1)]).areas(work_r);
        f.render_widget(Paragraph::new(lines).block(t.block("pods")), pods_r);
        let mut dl = Vec::new();
        if engine_down {
            dl.push(Line::from(vec![Span::styled(" containers  ", t.dim), Span::styled(if down { "– (engine not running)" } else { "–" }, t.dim)]));
        } else {
            dl.extend(chip_lines(t, " containers  ", container_chips(app, t), ww));
            let mut store = vec![Span::raw(" ")];
            store.extend(docker_store_spans(app, t, true));
            dl.push(Line::from(store));
        }
        f.render_widget(Paragraph::new(dl).block(docker_block(app, t, docker_r.width)), docker_r);
    }
    connections(f, app, t, conn, true);
    if !app.dev_output.is_empty() {
        dev_output(f, app, t, out);
    } else {
        tip(f, app, t, out);
    }
}

/// A remote litek8s's nodes: the whole cluster, then each node, with its
/// CPU, memory and disk; the one Home shows (`n`) marked.
fn node_lines(app: &App, t: &Theme, width: usize) -> Vec<Line<'static>> {
    use crate::dev::{disk, memory};
    let r = &app.node_reports;
    let names = r.names();
    if names.is_empty() {
        return vec![Line::from(vec![Span::styled(" nodes       ", t.dim), Span::styled("–", t.dim)])];
    }
    let name_w = names.iter().map(|n| n.chars().count()).max().unwrap_or(7).clamp(11, 24);
    let row = |label: String, key: &str, mems: Vec<memory::NodeMemory>, disks: Vec<disk::NodeDisk>, error: Option<&String>, on: bool| {
        let mark = if on { "▸ " } else { "  " };
        let mut spans = vec![Span::styled(format!(" {mark}{:<name_w$} ", trunc(&label, name_w)), if on { t.bold } else { Style::new() })];
        if let Some(e) = error {
            spans.push(Span::styled(trunc(e, width.saturating_sub(name_w + 6)), t.bad));
            return Line::from(spans);
        }
        let cpu = app.scoped_cpu.get(key).map(|c| pct(c.total_pct / c.cpus.max(1.0), 0)).unwrap_or_else(|| "–".into());
        let mem = (!mems.is_empty()).then(|| memory::scoped_breakdown(&memory::sum(&mems, ""), "", "")).and_then(|b| b.limit.map(|l| format!("{}/{}", gib(b.in_use()), gib(l)))).unwrap_or_else(|| "–".into());
        let d = if disks.is_empty() { "–".into() } else { bytes(disk::sum(&disks, "").total()) };
        spans.push(Span::styled("cpu ", t.dim));
        spans.push(Span::raw(format!("{cpu:>5}  ")));
        spans.push(Span::styled("mem ", t.dim));
        spans.push(Span::raw(format!("{mem:<18}")));
        if width > name_w + 48 {
            spans.push(Span::styled("disk ", t.dim));
            spans.push(Span::raw(d));
        }
        Line::from(spans)
    };
    let mut lines = Vec::new();
    if names.len() > 1 {
        lines.push(row(format!("cluster ({})", names.len()), "", r.memory.clone(), r.disk.clone(), None, app.scope.is_none()));
    }
    for n in &names {
        let mems = r.memory.iter().filter(|m| m.node == *n).cloned().collect();
        let disks = r.disk.iter().filter(|d| d.node == *n).cloned().collect();
        let error = r.errors.iter().find(|(x, _)| x == n).map(|(_, e)| e);
        lines.push(row(n.clone(), n, mems, disks, error, names.len() == 1 || app.scope.as_deref() == Some(n.as_str())));
    }
    lines
}

/// A resource in a box: its history, a meter, then its breakdown as
/// inline bars, as many lines as fit (`+N` says how many did not).
#[allow(clippy::too_many_arguments)]
fn resource_box(f: &mut Frame, t: &Theme, area: Rect, name: &str, b: Option<&Explain>, hist: Option<(&[f64], f64)>, graph_h: u16, ratio: Option<f64>, text: String, stack: Option<&Stack>) {
    if area.height < 3 {
        return;
    }
    let inner_h = area.height.saturating_sub(2) as usize;
    let w = area.width.saturating_sub(2) as usize;
    let mut title = vec![Span::styled(name.to_string(), t.title)];
    if let Some(b) = b {
        // What the figure is, cut to leave room for the bar's scale.
        let room = w.saturating_sub(name.chars().count() + b.scale.chars().count() + 12);
        if room > 6 {
            title.push(Span::styled(format!(" · {}", trunc(&b.what, room)), t.dim));
        }
    }
    let mut lines = Vec::new();
    if let Some((values, max)) = hist {
        let gh = (graph_h as usize).clamp(1, inner_h.saturating_sub(1).max(1));
        if values.is_empty() {
            lines.push(Line::styled(" collecting…", t.dim));
            lines.extend((1..gh).map(|_| Line::raw("")));
        } else {
            lines.extend(t.graph(values, max, w.saturating_sub(2), gh).into_iter().map(|l| {
                let mut spans = vec![Span::raw(" ")];
                spans.extend(l.spans);
                Line::from(spans)
            }));
        }
    }
    let tw = text.chars().count();
    match ratio {
        Some(r) => {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(meter_spans(t, r, &text, stack, w.saturating_sub(tw + 8).clamp(4, 40)));
            lines.push(Line::from(spans));
        }
        None => lines.push(Line::styled(format!(" {text}"), t.dim)),
    }
    let mut more = 0;
    if let Some(b) = b {
        let room = inner_h.saturating_sub(lines.len());
        let n = b.lines.len();
        let shown = if n > room { room.saturating_sub(1) } else { n };
        more = n - shown;
        let lw = b.label_width();
        for l in b.lines.iter().take(shown) {
            lines.push(explain_line(t, l, w, lw));
        }
    }
    let mut block = match b {
        Some(b) => t.block_with(Line::from(title), vec![Span::styled(b.scale.clone(), t.dim)]),
        None => t.block(Line::from(title)),
    };
    if more > 0 {
        block = block.title_bottom(Line::from(Span::styled(format!(" +{more} more (narrower: b) ─"), t.dim)).right_aligned());
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// One of Home's breakdowns: a figure and what it is made of.
struct Explain {
    /// 0 memory, 1 CPU, 2 disk (the order `b` cycles).
    kind: usize,
    title: String,
    /// What the headline is (`what macOS charges the VM`).
    what: String,
    /// What a full bar is (`bar: 1 CPU`): every row's bar is its value
    /// against it, so bar and number always agree.
    scale: String,
    lines: Vec<ExplainLine>,
}

/// A percentage with `digits` decimals that never reads as zero when it is
/// not: `<0.1%` (or `<1%`).
fn pct(v: f64, digits: usize) -> String {
    let unit = 10f64.powi(-(digits as i32));
    if v > 0.0 && v < unit / 2.0 {
        return format!("<{unit:.digits$}%");
    }
    format!("{v:.digits$}%")
}

impl Explain {
    fn label_width(&self) -> usize {
        self.lines.iter().map(|l| l.label.chars().count() + if l.indent { 2 } else { 0 }).max().unwrap_or(0)
    }
}

struct ExplainLine {
    label: String,
    value: String,
    note: String,
    indent: bool,
    /// Its value against the box's scale (`Explain::scale`), for the bar.
    share: f64,
}

fn explain_boxes(app: &App) -> Vec<Explain> {
    let mut out = Vec::new();
    if let Some(m) = &app.home.memory {
        // Against the VM's or sandbox's limit (the meter's), else the total.
        let (scale, name) = match m.limit.filter(|l| *l > 0) {
            Some(l) => (l, format!("bar: {} {}", m.limit_what, gib(l))),
            None => (m.total.max(1), format!("bar: {}", bytes(m.total))),
        };
        out.push(Explain {
            kind: 0,
            title: format!("Memory · {} ({})", bytes(m.total), m.what),
            what: m.what.to_string(),
            scale: name,
            lines: m.lines.iter().map(|l| ExplainLine { label: l.label.clone(), value: bytes(l.bytes), note: l.note.to_string(), indent: false, share: l.bytes as f64 / scale as f64 }).collect(),
        });
    }
    if let Some(c) = &app.cpu {
        // Rows are in % of one CPU, and so are the bars: a full bar is a
        // whole CPU (a row using more fills it; its number says how much).
        let share = if c.cpus > 0.0 { format!(" = {} of {:.0} CPUs", pct(c.total_pct / c.cpus, 0), c.cpus) } else { String::new() };
        out.push(Explain {
            kind: 1,
            title: format!("CPU · {} of one CPU{share} ({})", pct(c.total_pct, 1), c.what),
            what: c.what.to_string(),
            scale: "bar: 1 CPU".into(),
            lines: c.lines.iter().map(|l| ExplainLine { label: l.label.clone(), value: pct(l.pct, 1), note: l.note.clone(), indent: l.indent, share: l.pct / 100.0 }).collect(),
        });
    }
    if let Some(d) = &app.disk_breakdown() {
        out.push(Explain {
            kind: 2,
            title: format!("Disk · {} ({})", bytes(d.total), d.what),
            what: d.what.to_string(),
            scale: format!("bar: {}", bytes(d.total)),
            lines: d
                .lines
                .iter()
                .filter(|l| l.bytes > 0 || !l.indent)
                .map(|l| ExplainLine { label: l.label.clone(), value: bytes(l.bytes), note: l.note.clone(), indent: l.indent, share: l.bytes as f64 / d.total.max(1) as f64 })
                .collect(),
        });
    }
    out
}

/// `litek8s (dev)      ▏          2.7 MB  API server, controllers…`;
/// `label_w`: the box's longest label (so the bars line up).
fn explain_line(t: &Theme, l: &ExplainLine, width: usize, label_w: usize) -> Line<'static> {
    let label_w = label_w.clamp(8, 24).min(width / 3);
    let bar_w = (width.saturating_sub(label_w + 12) / 4).clamp(4, 14);
    let label = if l.indent { format!("  {}", l.label) } else { l.label.clone() };
    let mut spans = vec![
        Span::styled(format!(" {:<label_w$} ", trunc(&label, label_w)), if l.indent { t.dim } else { Style::new() }),
        t.bar(l.share, bar_w, if l.indent { t.dim } else { t.accent.remove_modifier(Modifier::BOLD) }),
        Span::styled(format!("{:>9}", l.value), if l.indent { Style::new() } else { t.bold }),
    ];
    let room = width.saturating_sub(label_w + bar_w + 14);
    if room > 4 && !l.note.is_empty() {
        spans.push(Span::styled(format!("  {}", trunc(&l.note, room)), t.dim));
    }
    Line::from(spans)
}

/// A breakdown box; `more`: some did not fit (`b` shows the next).
fn explain_box(f: &mut Frame, t: &Theme, b: &Explain, area: Rect, more: bool) {
    let w = area.width.saturating_sub(2) as usize;
    let lw = b.label_width();
    let lines: Vec<Line> = b.lines.iter().map(|l| explain_line(t, l, w, lw)).collect();
    // The bars' scale at the top right, or the bottom right when the
    // title needs the room.
    let right = b.scale.chars().count() + 4;
    let room = (area.width as usize).saturating_sub(if more { 18 } else { 6 });
    let top = b.title.chars().count() + right <= room;
    let mut title = vec![Span::styled(trunc(&b.title, room), t.title)];
    if more {
        title.push(Span::styled("  b: next", t.dim));
    }
    let scale = vec![Span::styled(b.scale.clone(), t.dim)];
    let block = if top {
        t.block_with(Line::from(title), scale)
    } else {
        t.block(Line::from(title)).title_bottom(Line::from(vec![Span::raw(" "), scale[0].clone(), Span::raw(" "), Span::styled("─", t.border)]).right_aligned())
    };
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Rows the output of a running `litek8s dev` needs (wrapped) in `width`.
fn output_rows(app: &App, width: u16) -> u16 {
    let inner_w = width.saturating_sub(2).max(1) as usize;
    let wrapped: usize = app.dev_output.iter().map(|l| l.chars().count().max(1).div_ceil(inner_w)).sum();
    if app.dev_output.is_empty() { 0 } else { (wrapped as u16 + 2).clamp(3, 12) }
}

/// The last lines of a `litek8s dev` run that fit, wrapped.
fn dev_output(f: &mut Frame, app: &App, t: &Theme, out: Rect) {
    let inner_w = out.width.saturating_sub(2).max(1) as usize;
    let title = match &app.dev_busy {
        Some(op) => Line::from(vec![Span::styled("◐ ", t.warn), Span::styled(format!("litek8s {} …", op.args().join(" ")), t.title)]),
        None => Line::from("output"),
    };
    let mut rows = out.height.saturating_sub(2) as usize;
    let mut first = app.dev_output.len();
    while first > 0 {
        let need = app.dev_output[first - 1].chars().count().max(1).div_ceil(inner_w);
        if need > rows {
            break;
        }
        rows -= need;
        first -= 1;
    }
    let lines: Vec<Line> = app.dev_output.iter().skip(first).map(|l| if l.starts_with("$ ") { Line::styled(l.clone(), t.accent) } else { Line::raw(l.clone()) }).collect();
    f.render_widget(Paragraph::new(lines).block(t.block(title)).wrap(Wrap { trim: false }), out);
}

/// A list in a box: bold headers, zebra rows when the terminal's
/// background is known, the selection highlighted.
#[allow(clippy::too_many_arguments)]
fn render_table(f: &mut Frame, app: &App, t: &Theme, area: Rect, title: &str, right: String, header: Vec<&str>, rows: Vec<Row<'static>>, widths: Vec<Constraint>, empty: &str) {
    let mut tl = vec![Span::styled(title.to_string(), t.title)];
    if !app.filter().is_empty() {
        tl.push(Span::styled(format!("  /{}", app.filter()), t.accent));
    }
    let b = t.block_with(Line::from(tl), if right.is_empty() { Vec::new() } else { vec![Span::styled(right, t.dim)] });
    let inner = b.inner(area);
    // Header row, then the list.
    app.list_top.set(inner.y + 1);
    if rows.is_empty() {
        f.render_widget(b, area);
        f.render_widget(Paragraph::new(Span::styled(format!(" {empty}"), t.dim)).wrap(Wrap { trim: false }), Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner });
        f.render_widget(Paragraph::new(Line::from(header.iter().map(|h| Span::styled(format!(" {h:<10}"), t.header)).collect::<Vec<_>>())), Rect { height: 1, ..inner });
        return;
    }
    let rows: Vec<Row> = match t.zebra {
        Some(z) => rows.into_iter().enumerate().map(|(i, r)| if i % 2 == 1 { r.style(z) } else { r }).collect(),
        None => rows,
    };
    let mut state = TableState::default().with_selected(Some(app.sel()));
    let table = Table::new(rows, widths)
        .header(Row::new(header.into_iter().map(|h| Cell::from(Span::styled(h, t.header)))))
        .block(b)
        .column_spacing(1)
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_symbol(Span::styled("▌", t.accent))
        .row_highlight_style(t.selected);
    f.render_stateful_widget(table, area, &mut state);
    app.list_offset.set(state.offset());
}

fn clusters(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let c = &app.clusters;
    let out_h = output_rows(app, area.width);
    let others_h = if c.others.is_empty() { 0 } else { (c.others.len() as u16 + 2).min(8) };
    let local_h = (c.local.len().max(1) as u16 + 3).min(area.height.saturating_sub(others_h + out_h).max(4));
    let [local, others, out, rest] = Layout::vertical([Constraint::Length(local_h), Constraint::Length(others_h), Constraint::Length(out_h), Constraint::Fill(1)]).areas(area);

    let what = if cfg!(target_os = "macos") { "VM" } else { "sandbox" };
    let shared = match &c.env_mode {
        Some(_) => {
            let mut parts = Vec::new();
            if let Some(n) = c.env_cpus {
                parts.push(format!("{n} CPUs"));
            }
            if let Some(m) = c.env_memory_mib {
                parts.push(format!("{} max", bytes(m << 20)));
            }
            if let Some(u) = c.env_memory {
                parts.push(format!("{} used", bytes(u)));
            }
            format!("shared {what}: {}", if parts.is_empty() { "running".into() } else { parts.join(", ") })
        }
        None => format!("{what} not running"),
    };
    let sel = app.sel();
    let rows: Vec<Row> = c
        .local
        .iter()
        .map(|r| {
            let current = r.name == app.cluster && app.context_override.is_none();
            let nodes = match r.ready_nodes {
                Some(n) if (n as u32) < r.nodes => format!("{n}/{}", r.nodes),
                _ => r.nodes.to_string(),
            };
            let h = status_health(r.status);
            Row::new(vec![
                Cell::from(Line::from(vec![Span::styled(if current { "› " } else { "  " }, t.accent), Span::styled(r.name.clone(), if current { t.accent } else { Style::new() })])),
                Cell::from(t.status(h, r.status.label())),
                Cell::from(r.port.map(|p| format!("127.0.0.1:{p}")).unwrap_or_else(|| "-".into())),
                Cell::from(nodes),
                Cell::from(r.memory.map(bytes).unwrap_or_else(|| "-".into())),
                Cell::from(Span::styled(r.age.clone().unwrap_or_else(|| "-".into()), t.dim)),
                Cell::from(Span::styled(r.context.clone(), t.dim)),
            ])
        })
        .collect();
    let b = t.block_with("Local clusters", vec![Span::styled(trunc(&shared, local.width.saturating_sub(22) as usize), t.dim)]);
    let inner = b.inner(local);
    app.list_top.set(inner.y + 1);
    let header = ["NAME", "STATE", "API", "NODES", "MEMORY", "AGE", "CONTEXT"];
    let widths = [Constraint::Length(14), Constraint::Length(11), Constraint::Length(16), Constraint::Length(5), Constraint::Length(9), Constraint::Length(4), Constraint::Fill(1)];
    if rows.is_empty() {
        f.render_widget(b, local);
        f.render_widget(Paragraph::new(Span::styled(" No local clusters: n creates one (or `litek8s dev up`).", t.dim)), Rect { y: inner.y + 1, height: 1, ..inner });
    } else {
        let mut state = TableState::default().with_selected((sel < c.local.len()).then_some(sel));
        let table = Table::new(rows, widths)
            .header(Row::new(header.into_iter().map(|h| Cell::from(Span::styled(h, t.header)))))
            .block(b)
            .column_spacing(1)
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol(Span::styled("▌", t.accent))
            .row_highlight_style(t.selected);
        f.render_stateful_widget(table, local, &mut state);
        app.list_offset.set(state.offset());
    }

    if !c.others.is_empty() {
        let rows: Vec<Row> = c
            .others
            .iter()
            .map(|(name, server)| {
                let current = app.context_override.as_deref() == Some(name);
                let kind = match c.others_litek8s.get(name) {
                    Some(true) => Span::styled("litek8s", t.ok),
                    Some(false) => Span::styled("read-only", t.dim),
                    None => Span::styled("-", t.dim),
                };
                Row::new(vec![
                    Cell::from(Line::from(vec![Span::styled(if current { "› " } else { "  " }, t.accent), Span::styled(name.clone(), if current { t.accent } else { Style::new() })])),
                    Cell::from(kind),
                    Cell::from(Span::styled(server.clone().unwrap_or_else(|| "-".into()), t.dim)),
                ])
            })
            .collect();
        let mut state = TableState::default().with_selected(sel.checked_sub(c.local.len()));
        let table = Table::new(rows, [Constraint::Percentage(35), Constraint::Length(10), Constraint::Fill(1)])
            .block(t.block_with("Other kubeconfig contexts", vec![Span::styled("non-litek8s read-only · enter: Kubernetes view", t.dim)]))
            .column_spacing(1)
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol(Span::styled("▌", t.accent))
            .row_highlight_style(t.selected);
        f.render_stateful_widget(table, others, &mut state);
    }
    if out_h > 0 {
        dev_output(f, app, t, out);
    } else if rest.height > 0 {
        let tip = if cfg!(target_os = "macos") {
            "All clusters share one VM (its CPUs and memory: Settings)."
        } else {
            "All clusters share one sandbox (its limits: Settings)."
        };
        f.render_widget(Paragraph::new(Span::styled(format!(" {tip}"), t.dim)).wrap(Wrap { trim: false }), rest);
    }
}

fn new_cluster(f: &mut Frame, app: &App, t: &Theme, form: &NewCluster, area: Rect) {
    let field = |label: &str, value: &str, active: bool| {
        let mut spans = vec![Span::styled(if active { "› " } else { "  " }, t.accent), Span::styled(format!("{label:<7}"), if active { t.accent } else { t.dim }), Span::styled(value.to_string(), if active { t.bold } else { Style::new() })];
        if active {
            spans.push(Span::styled("█", t.accent.remove_modifier(Modifier::BOLD)));
        }
        Line::from(spans)
    };
    let name = if form.name.is_empty() { "<name>".to_string() } else { form.name.clone() };
    let cluster = crate::dev::cluster::Cluster { name: name.clone() };
    let size = if cfg!(target_os = "macos") {
        let cpus = app.clusters.env_cpus.or(app.config.cpus).unwrap_or_else(crate::dev::config::default_cpus);
        let mem = app.clusters.env_memory_mib.or(app.config.memory).unwrap_or(crate::dev::config::DEFAULT_MEMORY_MIB);
        format!("Runs in the shared VM: {cpus} CPUs, {mem} MiB for all clusters.")
    } else {
        "Runs in the shared rootless sandbox (limits for all: Settings).".to_string()
    };
    let mut lines = vec![
        field("Name", &form.name, form.field == 0),
        field("Nodes", &form.nodes, form.field == 1),
        Line::raw(""),
        Line::styled(size, t.dim),
        Line::styled(format!("Context {}, socket {}", cluster.context(), home_dir_short(&cluster.docker_socket().display().to_string())), t.dim),
        Line::styled("Nodes after the first are workers joined like real nodes.", t.dim),
    ];
    if let Some(e) = &form.error {
        lines.push(Line::from(vec![Span::styled("✕ ", t.bad), Span::styled(e.clone(), t.bad)]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(key_chips(t, &[("tab", "next field"), ("enter", "create"), ("esc", "cancel")], 64)));
    let width = 68u16;
    let inner = width.min(area.width.saturating_sub(2)).saturating_sub(4).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner)).sum();
    let r = centered(area, width, rows as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(t.dialog("New cluster", t.accent).padding(Padding::horizontal(1))), r);
}

fn containers(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    if let Some(e) = &app.docker_error {
        return docker_unavailable(f, app, t, area, "Containers", e, "Start the cluster (1, then u; 2 for the others) or pass --docker-host.");
    }
    let wide = area.width >= 100;
    let rows: Vec<Row> = app
        .container_rows()
        .into_iter()
        .map(|r| match r {
            CRow::Group { name, count, running, collapsed } => {
                let h = if running == count { Health::Ok } else if running == 0 { Health::Done } else { Health::Warn };
                let (budget, cpu) = project_cells(t, app.projects.get(&name));
                let cells = vec![
                    Cell::from(Span::styled(format!("{} {name}", if collapsed { "▸" } else { "▾" }), t.accent)),
                    Cell::from(Span::styled(format!("{} {running}/{count} up", h.glyph()), t.health(h))),
                    Cell::from(budget),
                    Cell::from(cpu),
                ];
                Row::new(cells)
            }
            CRow::Item(i) => {
                let c = &app.containers[i];
                // The OOM killer stopped it: say so instead of `exited`.
                let oom = !c.running() && app.oom_killed.contains(&c.id);
                let h = if oom { Health::Bad } else { Health::of_container(&c.state, &c.status) };
                let name = if c.project.is_some() { format!("  {}", c.service.as_deref().unwrap_or(&c.name)) } else { c.name.clone() };
                let base = row_style(t, h);
                let state = if oom { Span::styled(format!("{} OOMKilled", h.glyph()), t.health(h).add_modifier(Modifier::BOLD)) } else { t.status(h, &c.state) };
                let mut cells = vec![Cell::from(Span::styled(name, base)), Cell::from(state), Cell::from(Span::styled(c.image.clone(), if h == Health::Ok { Style::new() } else { t.dim })), Cell::from(c.ports.clone())];
                if wide {
                    cells.insert(3, Cell::from(Span::styled(c.status.clone(), t.dim)));
                }
                Row::new(cells)
            }
        })
        .collect();
    let (header, widths) = if wide {
        (vec!["NAME", "STATE", "IMAGE", "STATUS", "PORTS"], vec![Constraint::Percentage(25), Constraint::Length(12), Constraint::Percentage(25), Constraint::Length(22), Constraint::Fill(1)])
    } else {
        (vec!["NAME", "STATE", "IMAGE", "PORTS"], vec![Constraint::Percentage(30), Constraint::Length(11), Constraint::Percentage(30), Constraint::Fill(1)])
    };
    let running = app.containers.iter().filter(|c| c.running()).count();
    render_table(f, app, t, area, "Containers", format!("{running}/{} running", app.containers.len()), header, rows, widths, "No containers. `docker run` against the litek8s socket and they show up here.");
}

/// A compose project's row: its memory against the budget (a meter like
/// Home's) and its CPUs, with the OOM kills it has seen.
fn project_cells(t: &Theme, p: Option<&super::model::ProjectUse>) -> (Line<'static>, Line<'static>) {
    let Some(p) = p else { return (Line::from(Span::styled("compose project", t.dim)), Line::default()) };
    let mem = match (p.memory, p.memory_max) {
        (Some(m), Some(max)) => Line::from(meter_spans(t, m as f64 / max.max(1) as f64, &format!("{}/{}", ibytes(m), ibytes(max)), None, 6)),
        (Some(m), None) => Line::from(vec![Span::styled("mem ", t.dim), Span::raw(ibytes(m)), Span::styled(" · no budget", t.dim)]),
        _ => Line::from(Span::styled("compose project", t.dim)),
    };
    let mut cpu = Vec::new();
    match (p.cpu_rate, p.cpus) {
        (Some(r), Some(c)) => {
            cpu.push(Span::styled("cpu ", t.dim));
            cpu.push(Span::styled(format!("{r:.1}/{c}"), t.level(r / c.max(0.01))));
        }
        (None, Some(c)) => cpu.push(Span::styled(format!("cpu max {c}"), t.dim)),
        (Some(r), None) => cpu.push(Span::styled(format!("cpu {r:.1}"), t.dim)),
        (None, None) => {}
    }
    if p.oom_kills > 0 {
        if !cpu.is_empty() {
            cpu.push(Span::raw(" "));
        }
        cpu.push(Span::styled(format!("✗ {} OOM", p.oom_kills), t.bad.add_modifier(Modifier::BOLD)));
    }
    (mem, Line::from(cpu))
}

/// `web-1`, `web-1 +2`: who uses an image or volume, in little space.
fn users_text(t: &Theme, list: &[String]) -> Span<'static> {
    match list {
        [] => Span::styled("unused", t.dim),
        [one] => Span::raw(one.clone()),
        [first, rest @ ..] => Span::raw(format!("{first} +{}", rest.len())),
    }
}

fn images(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    if let Some(e) = &app.docker_error {
        return docker_unavailable(f, app, t, area, "Images", e, "");
    }
    let wide = area.width >= 100;
    let rows: Vec<Row> = app
        .visible_images()
        .into_iter()
        .map(|i| {
            let img = &app.images[i];
            let mut users: Vec<String> = img.containers.clone();
            users.extend(img.pods.iter().map(|p| format!("pod {}", p.split_once('/').map(|(_, n)| n).unwrap_or(p))));
            let name = if img.dangling() { Span::styled("<none>", t.dim) } else { Span::raw(img.tags.join(", ")) };
            let mut cells = vec![Cell::from(name), Cell::from(Line::from(bytes(img.size)).right_aligned()), Cell::from(Span::styled(age(app.now, img.created), t.dim)), Cell::from(users_text(t, &users))];
            if wide {
                cells.insert(1, Cell::from(Span::styled(img.short_id().to_string(), t.dim)));
            }
            Row::new(cells)
        })
        .collect();
    let total: u64 = app.images.iter().map(|i| i.size).sum();
    let (header, widths) = if wide {
        (vec!["IMAGE", "ID", "SIZE", "AGE", "USED BY"], vec![Constraint::Fill(3), Constraint::Length(12), Constraint::Length(8), Constraint::Length(5), Constraint::Fill(2)])
    } else {
        (vec!["IMAGE", "SIZE", "AGE", "USED BY"], vec![Constraint::Fill(1), Constraint::Length(8), Constraint::Length(5), Constraint::Fill(1)])
    };
    render_table(f, app, t, area, "Images", format!("{} · {}", app.images.len(), bytes(total)), header, rows, widths, "No images.");
}

fn volumes(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    if let Some(e) = &app.docker_error {
        return docker_unavailable(f, app, t, area, "Volumes", e, "");
    }
    let rows: Vec<Row> = app
        .visible_volumes()
        .into_iter()
        .map(|i| {
            let v = &app.volumes[i];
            let name = if v.anonymous { Line::from(vec![Span::raw(format!("{}…", &v.name[..12])), Span::styled(" (anonymous)", t.dim)]) } else { Line::raw(v.name.clone()) };
            Row::new(vec![
                Cell::from(name),
                Cell::from(Line::from(v.size.map(bytes).unwrap_or_else(|| "-".into())).right_aligned()),
                Cell::from(users_text(t, &v.users)),
                Cell::from(Span::styled(v.project.clone().unwrap_or_default(), t.dim)),
            ])
        })
        .collect();
    let total: u64 = app.volumes.iter().filter_map(|v| v.size).sum();
    render_table(f, app, t, area, "Volumes", format!("{} · {}", app.volumes.len(), bytes(total)), vec!["VOLUME", "SIZE", "USED BY", "PROJECT"], rows, vec![Constraint::Percentage(40), Constraint::Length(9), Constraint::Fill(1), Constraint::Length(14)], "No volumes.");
}

fn space(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    if let Some(e) = &app.docker_error {
        return docker_unavailable(f, app, t, area, "Free space", e, "");
    }
    let [list, info] = Layout::vertical([Constraint::Length(RECLAIMS.len() as u16 + 4), Constraint::Min(1)]).areas(area);
    let mut total_sel = 0;
    let all: u64 = RECLAIMS.iter().map(|r| app.reclaimable(*r).0).sum();
    let rows: Vec<Row> = RECLAIMS
        .iter()
        .map(|r| {
            let (b, n) = app.reclaimable(*r);
            let on = app.space_checked.contains(r);
            if on {
                total_sel += b;
            }
            let style = if n == 0 { t.dim } else { Style::new() };
            Row::new(vec![
                Cell::from(Span::styled(if on { "[✓]" } else { "[ ]" }, if on { t.accent } else { t.dim })),
                Cell::from(Span::styled(r.title(), style)),
                Cell::from(Line::from(Span::styled(n.to_string(), style)).right_aligned()),
                Cell::from(Line::from(Span::styled(bytes(b), if on { t.bold } else { style })).right_aligned()),
                Cell::from(t.bar(b as f64 / all.max(1) as f64, 12, if on { t.accent.remove_modifier(Modifier::BOLD) } else { t.dim })),
            ])
        })
        .collect();
    render_table(f, app, t, list, "Reclaimable space", format!("{} in all", bytes(all)), vec!["", "WHAT", "ITEMS", "SIZE", ""], rows, vec![Constraint::Length(3), Constraint::Fill(1), Constraint::Length(6), Constraint::Length(9), Constraint::Length(12)], "");
    let h = &app.home;
    let mut lines = vec![
        Line::from(vec![Span::styled("Selected  ", t.dim), Span::styled(bytes(total_sel), t.accent), Span::raw("   "), Span::styled(" p ", t.chip), Span::styled(" frees it (asks first)", t.dim)]),
        Line::raw(""),
    ];
    if let Some(u) = h.disk_used {
        let room = (info.width as usize).saturating_sub(30);
        lines.push(Line::from(vec![Span::styled("litek8s data  ", t.dim), Span::raw(format!("{} in {}", bytes(u), trunc_left(&home_dir_short(&h.data_dir), room)))]));
    }
    if let Some(free) = h.disk_free {
        let style = if free < 40_000_000_000 { t.warn } else { Style::new() };
        lines.push(Line::from(vec![Span::styled("disk free    ", t.dim), Span::styled(bytes(free), style)]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Docker and Kubernetes share one image store: images no container or pod", t.dim));
    lines.push(Line::styled("uses are unused (pods pull them again when needed). Volumes are never", t.dim));
    lines.push(Line::styled("selected by default: they hold data.", t.dim));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 }));
}

fn cpu_text(cores: f64) -> String {
    match cores * 1000.0 {
        m if m >= 1000.0 => format!("{cores:.2}"),
        m if m >= 10.0 || m == 0.0 => format!("{m:.0}m"),
        m => format!("{m:.1}m"),
    }
}

/// Alerts at the top (state glyphs: `✕` critical and firing, `◐` firing,
/// `○` pending or acknowledged), then pods by CPU or memory with their
/// last half hour as gradient sparklines.
fn observe(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let o = &app.obs;
    if let Some(e) = &o.error {
        if o.pods.is_empty() && o.alerts.is_empty() {
            return unavailable(f, t, area, "Observe", e, "Metrics, logs and alerts come from the API server (litek8s apiserver, on by default).");
        }
    }
    let shown = o.alerts.len().min(5);
    let alert_rows = if o.alerts.is_empty() { 3 } else { shown as u16 + 2 };
    let [top, rest] = Layout::vertical([Constraint::Length(alert_rows), Constraint::Min(3)]).areas(area);
    let firing = o.alerts.iter().filter(|a| a.state == "firing").count();
    let mut right = Vec::new();
    if !o.alerts.is_empty() {
        right.push(Span::styled(format!("{firing} firing"), if firing > 0 { t.bad } else { t.dim }));
        right.push(Span::styled(format!(" · {} pending", o.alerts.len() - firing), t.dim));
        if o.alerts.len() > shown {
            right.push(Span::styled(format!(" · {} more: litek8s alerts", o.alerts.len() - shown), t.dim));
        }
    }
    let w = top.width.saturating_sub(2) as usize;
    let lines: Vec<Line> = if o.alerts.is_empty() {
        vec![Line::from(vec![Span::styled(" ● ", t.ok), Span::styled("No alerts. Built-in rules watch crash loops, OOM kills, node health, full disks and volumes.", t.dim)])]
    } else {
        o.alerts
            .iter()
            .take(shown)
            .map(|a| {
                let (h, word) = match (a.state.as_str(), a.silenced) {
                    (_, true) => (Health::Done, "acked  "),
                    ("firing", _) if a.severity == "critical" => (Health::Bad, "firing "),
                    ("firing", _) => (Health::Warn, "firing "),
                    _ => (Health::Done, "pending"),
                };
                let since = age(app.now, a.since);
                let head = format!(" {} {word} ", h.glyph());
                let name = format!("{:<24} ", trunc(&a.name, 24));
                let room = w.saturating_sub(head.chars().count() + name.chars().count() + 7);
                let detail = if a.summary.is_empty() { a.target.clone() } else { a.summary.clone() };
                let quiet = a.silenced || a.state != "firing";
                Line::from(vec![
                    Span::styled(head, t.health(h)),
                    Span::styled(name, if quiet { t.dim } else { t.bold }),
                    Span::styled(format!("{since:<6} "), t.dim),
                    Span::styled(trunc(&detail, room), if quiet { t.dim } else { Style::new() }),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines).block(t.block_with("Alerts", right)), top);

    let idx = app.visible_obs_pods();
    // Each figure against what caps it: `used/limit` and the share of it
    // (`r` and the request's share when there is no limit, `—` when there
    // is neither). The sparklines get what the pod column leaves: both on
    // wide terminals, the sorted resource's alone on narrow ones.
    const NS: u16 = 10;
    const CPU: u16 = 9;
    const MEM: u16 = 13;
    const PCT: u16 = 5;
    let inner = rest.width.saturating_sub(3);
    let fixed = NS + CPU + PCT + MEM + PCT + 6;
    let both = inner >= fixed + 1 + 2 * 17 + 24;
    let spark_w = if both { 17 } else { inner.saturating_sub(fixed + 18).min(17) as usize };
    let spark_w = if spark_w < 6 { 0 } else { spark_w };
    let cpu_max = idx.iter().flat_map(|i| o.pods[*i].cpu.iter().flatten()).fold(0.0f64, |a, b| a.max(*b));
    let mem_max = idx.iter().flat_map(|i| o.pods[*i].mem.iter().flatten()).fold(0.0f64, |a, b| a.max(*b));
    let rows: Vec<Row> = idx
        .iter()
        .map(|i| {
            let p = &o.pods[*i];
            // A pod gone during the window: dim, no current figures.
            let live = p.live();
            let figure = |on: bool| if !live { t.dim } else if on { t.bold } else { Style::new() };
            let spark = |v: &[Option<f64>], max: f64| if live { Line::from(t.sparkline_gaps(v, max, spark_w)) } else { Line::styled(t.sparkline_gaps(v, max, spark_w).iter().map(|s| s.content.to_string()).collect::<String>(), t.dim) };
            // `used/limit` (or `/request`, or `/—`) and the share.
            let against = |used: f64, b: &Bounds, text: &dyn Fn(f64) -> String, w: u16, on: bool| -> (Cell<'static>, Cell<'static>) {
                if !live {
                    return (Cell::from(Span::styled(format!("{:>w$}", if on { "gone" } else { "-" }, w = w as usize), t.dim)), Cell::from(""));
                }
                let cap = b.of();
                let of = cap.map(|(c, _)| text(c)).unwrap_or_else(|| "—".into());
                let head = text(used);
                let pad = (w as usize).saturating_sub(head.chars().count() + 1 + of.chars().count());
                let cap_style = if cap.is_some_and(|(_, req)| !req) { Style::new() } else { t.dim };
                let fig = Cell::from(Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(head, figure(on)), Span::styled("/", t.dim), Span::styled(of, cap_style)]));
                let pct = match cap {
                    Some((c, req)) => {
                        let r = used / c;
                        let n = format!("{}%", (r * 100.0).round().min(999.0) as u64);
                        if req { Span::styled(format!("r{n:>4}"), t.dim) } else { Span::styled(format!("{n:>5}"), if r >= 0.9 { t.level(r).add_modifier(Modifier::BOLD) } else { t.level(r) }) }
                    }
                    None => Span::styled(format!("{:>5}", "—"), t.dim),
                };
                (fig, Cell::from(pct))
            };
            let (cpu, cpu_pct) = against(p.cpu_now(), &p.cpu_bounds, &cpu_text, CPU, !app.obs_by_mem);
            let (mem, mem_pct) = against(p.mem_now(), &p.mem_bounds, &|b| ibytes(b as u64), MEM, app.obs_by_mem);
            let mut cells = vec![Cell::from(Span::styled(p.namespace.clone(), t.dim)), Cell::from(Span::styled(p.name.clone(), if live { Style::new() } else { t.dim })), cpu, cpu_pct];
            if both {
                cells.push(Cell::from(spark(&p.cpu, cpu_max)));
            }
            cells.extend([mem, mem_pct]);
            if both {
                cells.push(Cell::from(spark(&p.mem, mem_max)));
            } else if spark_w > 0 {
                cells.push(Cell::from(if app.obs_by_mem { spark(&p.mem, mem_max) } else { spark(&p.cpu, cpu_max) }));
            }
            Row::new(cells)
        })
        .collect();
    let ns = app.obs_namespace.as_deref().map(|n| format!(" · {n}")).unwrap_or_default();
    let by = if app.obs_by_mem { "memory" } else { "CPU" };
    let mut header = vec!["NAMESPACE", "POD", "CPU/LIMIT", "    %"];
    let mut widths = vec![Constraint::Length(NS), Constraint::Fill(1), Constraint::Length(CPU), Constraint::Length(PCT)];
    if both {
        header.push("");
        widths.push(Constraint::Length(17));
    }
    header.extend(["    MEM/LIMIT", "    %"]);
    widths.extend([Constraint::Length(MEM), Constraint::Length(PCT)]);
    if both {
        header.push("");
        widths.push(Constraint::Length(17));
    } else if spark_w > 0 {
        header.push(if app.obs_by_mem { "MEMORY" } else { "CPU" });
        widths.push(Constraint::Length(spark_w as u16));
    }
    render_table(f, app, t, rest, &format!("Pods by {by}"), format!("% of limit, r: of request · 30 min{ns}"), header, rows, widths, "No pod metrics yet (the first samples arrive within 10 s).");
}

fn settings(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let restart = app.restart_needed();
    let notes = 4 + if restart.is_empty() { 0 } else { 1 } + if app.config_error.is_some() { 1 } else { 0 };
    let [list, info] = Layout::vertical([Constraint::Length(SETTINGS.len() as u16 + 3), Constraint::Min(notes)]).areas(area);
    let c = &app.home.contexts;
    let mac = cfg!(target_os = "macos");
    let default = |v: String, d: String| if v.is_empty() { Span::styled(format!("{d} (default)"), t.dim) } else { Span::raw(v) };
    let rows: Vec<Row> = SETTINGS
        .iter()
        .map(|s| {
            let (name, value) = match s {
                Setting::Field(Field::Cpus) if mac => ("VM CPUs", default(app.setting_value(Field::Cpus), crate::dev::config::default_cpus().to_string())),
                Setting::Field(Field::Cpus) => ("CPU limit", default(app.setting_value(Field::Cpus), "none".into())),
                Setting::Field(Field::Memory) if mac => ("VM memory (MiB)", default(app.setting_value(Field::Memory), crate::dev::config::DEFAULT_MEMORY_MIB.to_string())),
                Setting::Field(Field::Memory) => ("Memory limit (MiB)", default(app.setting_value(Field::Memory), "none".into())),
                Setting::Field(Field::Disk) if mac => {
                    let size = app.home.vm_disk.map(|(_, size)| format!("  (now {} GiB, grows only)", size >> 30)).unwrap_or_default();
                    let v = default(app.setting_value(Field::Disk), crate::dev::config::DEFAULT_DISK_GIB.to_string());
                    ("VM disk (GiB)", Span::styled(format!("{}{size}", v.content), v.style))
                }
                Setting::Field(Field::Disk) => ("VM disk (GiB)", Span::styled("macOS VM only (the sandbox uses the host's disk)", t.dim)),
                Setting::Field(Field::Shares) if mac => ("Shared directories", default(app.setting_value(Field::Shares), "$HOME".into())),
                Setting::Field(Field::Shares) => ("Shared directories", Span::styled("macOS VM only (Linux uses the host's files)", t.dim)),
                Setting::Field(Field::Port) => ("API port (dev)", default(app.setting_value(Field::Port), crate::dev::DEFAULT_PORT.to_string())),
                Setting::Field(Field::Publish) => (
                    "Published ports on",
                    Span::raw(match app.config.publish {
                        crate::dev::config::Publish::Localhost => "localhost only",
                        crate::dev::config::Publish::All => "all interfaces (LAN)",
                    }),
                ),
                Setting::DockerContext => (
                    "docker context",
                    if c.docker_context_is_litek8s { Span::styled(format!("{} ✓  (enter: switch back)", c.docker_context), t.ok) } else { Span::raw(format!("{}  (enter: use litek8s)", c.docker_context)) },
                ),
                Setting::KubeContext => (
                    "kubectl context",
                    if c.kube_is_litek8s { Span::styled(format!("{} ✓  (enter: switch back)", c.kube_context), t.ok) } else { Span::raw(format!("{}  (enter: use litek8s-dev)", if c.kube_context.is_empty() { "-" } else { &c.kube_context })) },
                ),
                Setting::EditFile => ("Config file", Span::raw(format!("{}  (enter: $EDITOR)", trunc_left(&home_dir_short(&app.config_path), list.width.saturating_sub(43) as usize)))),
            };
            Row::new(vec![Cell::from(Span::styled(name, t.dim)), Cell::from(value)])
        })
        .collect();
    render_table(f, app, t, list, "Settings", String::new(), vec!["SETTING", "VALUE"], rows, vec![Constraint::Length(20), Constraint::Fill(1)], "");
    let mut lines = Vec::new();
    if let Some(e) = &app.config_error {
        let e = e.rsplit_once(" is not valid: ").map(|(_, why)| why).unwrap_or(e);
        lines.push(Line::styled(format!(" ✕ config file: {e}"), t.bad));
    }
    if !restart.is_empty() {
        lines.push(Line::styled(format!(" ⟳ Restart needed for {}: Home (1), d then u.", restart.join(", ")), t.warn));
    }
    let platform = if mac {
        "CPUs, memory, disk and shares size the macOS VM; a bigger disk grows its image in place (data kept)."
    } else {
        "CPU and memory limits apply to everything the dev cluster runs."
    };
    lines.push(Line::styled(format!(" {platform}"), t.dim));
    lines.push(Line::styled(" Settings apply on the next `litek8s dev up` (its flags win).", t.dim));
    lines.push(Line::from(vec![Span::styled(" Look: ", t.dim), Span::raw(look(t)), Span::styled(" (ui.yaml)", t.dim)]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info);
}

/// `default theme, 24-bit color, dark background`: what the UI found.
fn look(t: &Theme) -> String {
    use super::theme::{Depth, Preset};
    let preset = match t.preset {
        Preset::Default => "default",
        Preset::HighContrast => "high-contrast",
        Preset::Mono => "mono",
    };
    let depth = match t.depth {
        Depth::True => "24-bit color",
        Depth::X256 => "256 colors",
        Depth::X16 => "16 colors",
        Depth::None => "no color",
    };
    let bg = match super::theme::term().bg {
        Some(b) if b.light() => "light background",
        Some(_) => "dark background",
        None => "background unknown",
    };
    format!("{preset}, {depth}, {bg}{}", if t.ascii { ", ASCII" } else { "" })
}

/// A Docker view without its Docker API. Looking at a remote litek8s, that
/// is this machine's dev cluster's API, down: say so, not as a failure.
fn docker_unavailable(f: &mut Frame, app: &App, t: &Theme, area: Rect, title: &str, error: &str, hint: &str) {
    if !app.docker_is_local_dev {
        return unavailable(f, t, area, title, error, hint);
    }
    let lines = vec![
        Line::styled(format!(" The Docker views show a Docker API: here, this machine's dev cluster's ({}), which is not running.", home_dir_short(app.docker_endpoint.strip_prefix("unix://").unwrap_or(&app.docker_endpoint))), t.dim),
        Line::raw(""),
        Line::styled(format!(" {} is a remote litek8s: its workloads are in Kube (7), its metrics in Obs (9).", app.kube_context), t.dim),
        Line::styled(" Another Docker API: litek8s ui --docker-host tcp://HOST:PORT (or unix://PATH).", t.dim),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(t.block(title)), area);
}

fn unavailable(f: &mut Frame, t: &Theme, area: Rect, title: &str, error: &str, hint: &str) {
    let lines = vec![Line::from(vec![Span::styled(" ✕ ", t.bad), Span::styled(error.to_string(), t.bad)]), Line::raw(""), Line::styled(format!(" {hint}"), t.dim)];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(t.block(title)), area);
}

fn log_view(f: &mut Frame, t: &Theme, v: &super::app::LogView, area: Rect) {
    let mut right = Vec::new();
    right.push(if v.follow { Span::styled("● follow", t.ok) } else { Span::styled("○ paused", t.dim) });
    if let Some(e) = &v.ended {
        right.push(Span::styled(format!("  [{e}]"), t.warn));
    }
    let b = t.block_with(Line::from(Span::styled(v.title.clone(), t.title)), right).padding(Padding::left(1));
    let inner = b.inner(area);
    let height = inner.height as usize;
    let len = v.lines.len();
    // Scrolled back past the top: show the first screenful.
    let end = len.saturating_sub(v.offset).max(height.min(len));
    let start = end.saturating_sub(height);
    let text: Vec<Line> = v.lines.range(start..end).map(|l| Line::raw(l.clone())).collect();
    let mut p = Paragraph::new(Text::from(text)).block(b);
    if v.wrap {
        p = p.wrap(Wrap { trim: false });
    }
    f.render_widget(p, area);
}

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn confirm(f: &mut Frame, t: &Theme, title: &str, lines: &[String], area: Rect) {
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(20).max(title.len() + 4).clamp(30, 74) as u16 + 6;
    let inner = (width.min(area.width.saturating_sub(2)).saturating_sub(4)).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.chars().count().max(1).div_ceil(inner)).sum();
    let r = centered(area, width, rows as u16 + 4);
    f.render_widget(Clear, r);
    let mut text: Vec<Line> = lines.iter().map(|l| Line::raw(l.clone())).collect();
    text.push(Line::raw(""));
    text.push(Line::from(vec![Span::styled(" y ", t.chip), Span::raw(" yes   "), Span::styled(" n/esc ", t.chip), Span::raw(" no")]));
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(t.dialog(Line::from(vec![Span::styled("⚠ ", t.warn), Span::styled(title.to_string(), t.bold)]), t.warn).padding(Padding::horizontal(1))), r);
}

fn input(f: &mut Frame, t: &Theme, prompt: &str, value: &str, error: Option<&str>, kind: &InputKind, area: Rect) {
    if *kind == InputKind::Filter {
        // The filter edits in the footer.
        let r = Rect { y: area.y + area.height - 1, height: 1, ..area };
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(Line::from(vec![Span::styled(" / ", t.chip), Span::raw(" "), Span::raw(value.to_string()), Span::styled("█", t.accent.remove_modifier(Modifier::BOLD))])), r);
        return;
    }
    let r = centered(area, 72, if error.is_some() { 7 } else { 6 });
    f.render_widget(Clear, r);
    let mut lines = vec![Line::from(vec![Span::styled("› ", t.accent), Span::styled(value.to_string(), t.bold), Span::styled("█", t.accent.remove_modifier(Modifier::BOLD))])];
    if let Some(e) = error {
        lines.push(Line::from(vec![Span::styled("✕ ", t.bad), Span::styled(e.to_string(), t.bad)]));
    }
    lines.push(Line::raw(""));
    let enter = if matches!(kind, InputKind::Setting(_)) { "save" } else { "ok" };
    lines.push(Line::from(key_chips(t, &[("enter", enter), ("esc", "cancel"), ("ctrl-u", "clear")], 66)));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(t.dialog(prompt.to_string(), t.accent).padding(Padding::horizontal(1))), r);
}

/// The help screen: per view, its keys (each line fits 76 columns).
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    ("Everywhere", &[("1-9 tab", "views"), ("↑↓ jk", "move"), ("/", "filter"), ("ctrl-r", "refresh"), ("q", "quit")]),
    ("Home", &[("u d R", "start stop delete"), ("n", "nodes"), ("b", "memory/CPU/disk"), ("c", "copy API")]),
    ("Clusters", &[("n", "new"), ("u d D", "start stop delete"), ("enter", "use it"), ("c", "copy context")]),
    ("Containers", &[("l", "logs"), ("s", "shell"), ("u x r", "start stop restart"), ("d", "remove"), ("space", "fold")]),
    ("Images", &[("d", "remove (asks)")]),
    ("Volumes", &[("d", "remove (asks)")]),
    ("Free space", &[("space", "select a category"), ("p", "free the selected (asks)")]),
    ("Kubernetes", &[(":", "resources, ctx, ns"), ("/", "filter"), ("d y e", "describe yaml edit"), ("?", "all")]),
    ("Settings", &[("enter", "edit"), ("del", "default"), ("space", "toggle / switch context")]),
    ("Observe", &[("l", "logs"), ("L", "search logs"), ("s", "sort"), ("a A", "ack pod/all")]),
    ("Logs", &[("f", "follow"), ("w", "wrap"), ("PgUp PgDn", "scroll"), ("g G", "top/end"), ("esc", "back")]),
];

fn help(f: &mut Frame, t: &Theme, area: Rect) {
    let r = centered(area, 78, HELP.len() as u16 + 9);
    f.render_widget(Clear, r);
    let mut lines = Vec::new();
    for (section, keys) in HELP {
        let mut row: Vec<Span> = vec![Span::styled(format!(" {section:<11}"), t.accent)];
        for (k, v) in keys.iter() {
            row.push(Span::styled(*k, t.bold));
            row.push(Span::raw(format!(" {v}  ")));
        }
        lines.push(Line::from(row));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(" Home's breakdowns (b): litek8s = its own processes; pods & containers;", t.dim));
    lines.push(Line::styled(" kernel/VM = fixed cost; cache, reclaimable = can go (6 Free space for disk)", t.dim));
    lines.push(Line::from(vec![Span::styled(" Kubernetes (7): any cluster or context (", t.dim), Span::styled(":ctx", t.key), Span::styled("); its ? lists every key.", t.dim)]));
    lines.push(Line::styled(" Destructive actions always ask. Mouse: litek8s ui --mouse. Docs: docs/ui.md", t.dim));
    lines.push(Line::styled(" Copy: c (and in Kubernetes c / C). Select with the mouse: option-drag (macOS)", t.dim));
    lines.push(Line::styled(" Any key closes this.", t.dim));
    f.render_widget(Paragraph::new(lines).block(t.dialog("Keys", t.accent)), r);
}
