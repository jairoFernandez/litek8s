//! Drawing the UI. Everything fits 80x24: one line of tabs, one of hints,
//! the rest for the current view; wider terminals get more columns.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap};

use super::app::{App, CRow, DevStatus, Field, InputKind, KubeView, Overlay, RECLAIMS, SETTINGS, Setting, TABS, Tab};
use super::model::{age, bytes};

const ACCENT: Color = Color::Cyan;
const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const OK: Style = Style::new().fg(Color::Green);
const WARN: Style = Style::new().fg(Color::Yellow);
const BAD: Style = Style::new().fg(Color::Red);
const SELECTED: Style = Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD);

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let [top, body, bottom] = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(area);
    tabs(f, app, top);
    app.list_top.set(0);
    app.list_offset.set(0);
    if let Some(logs) = &app.logs {
        log_view(f, logs, body);
    } else {
        match app.tab {
            Tab::Home => home(f, app, body),
            Tab::Containers => containers(f, app, body),
            Tab::Images => images(f, app, body),
            Tab::Volumes => volumes(f, app, body),
            Tab::Space => space(f, app, body),
            Tab::Kube => kube(f, app, body),
            Tab::Settings => settings(f, app, body),
        }
    }
    hints(f, app, bottom);
    match &app.overlay {
        Overlay::None => {}
        Overlay::Help => help(f, app, area),
        Overlay::Confirm { title, lines, .. } => confirm(f, title, lines, area),
        Overlay::Input { prompt, value, error, kind } => input(f, prompt, value, error.as_deref(), kind, area),
    }
}

fn tabs(f: &mut Frame, app: &App, area: Rect) {
    let narrow = area.width < 100;
    let mut spans = vec![Span::styled(" litek8s ", Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw(" ")];
    let mut x = 10u16;
    let mut hit = Vec::new();
    for (i, t) in TABS.iter().enumerate() {
        let title = match (*t, narrow) {
            (Tab::Space, true) => "Space",
            (Tab::Kube, true) => "Kube",
            (t, _) => t.title(),
        };
        let label = format!("{} {title}", i + 1);
        let w = label.chars().count() as u16;
        hit.push((x, x + w));
        x += w + 2;
        let style = if *t == app.tab { Style::new().fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { Style::new() };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("  "));
    }
    *app.tab_spans.borrow_mut() = hit;
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn hints(f: &mut Frame, app: &App, area: Rect) {
    let line = if let Some((text, error)) = &app.status {
        Line::from(Span::styled(format!(" {text}"), if *error { BAD } else { OK }))
    } else {
        let keys: &[(&str, &str)] = if app.logs.is_some() {
            &[("esc", "back"), ("f", "follow"), ("w", "wrap"), ("↑↓", "scroll"), ("g/G", "top/end")]
        } else {
            match app.tab {
                Tab::Home => &[("u", "up"), ("d", "down"), ("R", "reset"), ("7", "settings")],
                Tab::Containers => &[("l", "logs"), ("s", "shell"), ("u", "start"), ("x", "stop"), ("r", "restart"), ("d", "remove"), ("/", "filter")],
                Tab::Images | Tab::Volumes => &[("d", "remove"), ("/", "filter"), ("5", "free space")],
                Tab::Space => &[("space", "select"), ("p", "free selected")],
                Tab::Kube => match app.kube_view {
                    KubeView::Pods => &[("l", "logs"), ("f", "forward"), ("d", "delete"), ("n", "namespace"), ("v", "deployments")],
                    KubeView::Deployments => &[("s", "scale"), ("r", "restart"), ("l", "logs"), ("n", "namespace"), ("v", "pods")],
                },
                Tab::Settings => &[("enter", "edit"), ("del", "default"), ("space", "toggle")],
            }
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, v) in keys {
            spans.push(Span::styled(*k, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)));
            spans.push(Span::raw(format!(" {v}  ")));
        }
        Line::from(spans)
    };
    let [left, right] = Layout::horizontal([Constraint::Min(1), Constraint::Length(9)]).areas(area);
    f.render_widget(Paragraph::new(line), left);
    f.render_widget(Paragraph::new(Line::from(vec![Span::styled("?", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw(" help ")])).right_aligned(), right);
}

fn block(title: &str) -> Block<'static> {
    Block::bordered().border_style(DIM).title(Span::styled(format!(" {title} "), BOLD))
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

fn status_style(s: DevStatus) -> Style {
    match s {
        DevStatus::Up => OK,
        DevStatus::Starting | DevStatus::Stopping | DevStatus::Unknown => WARN,
        DevStatus::Down => BAD,
        DevStatus::Remote => Style::new().fg(ACCENT),
    }
}

fn home(f: &mut Frame, app: &App, area: Rect) {
    let h = &app.home;
    // Wrapped lines take more than one row each.
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let wrapped: usize = app.dev_output.iter().map(|l| l.chars().count().max(1).div_ceil(inner_w)).sum();
    let output_rows = if app.dev_output.is_empty() { 1 } else { (wrapped as u16 + 2).clamp(3, 12) };
    let [upper, ctx, out, _] = Layout::vertical([Constraint::Length(7), Constraint::Length(5), Constraint::Length(output_rows), Constraint::Fill(1)]).areas(area);
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(upper);

    let w = left.width.saturating_sub(12) as usize;
    let mut state = vec![Span::styled(format!("● {}", h.status.label()), status_style(h.status).add_modifier(Modifier::BOLD))];
    if h.status == DevStatus::Up || h.status == DevStatus::Starting {
        let mode = match h.mode.as_str() {
            "vm" => "macOS VM",
            "rootless" => "rootless sandbox",
            "root" => "sandbox (root)",
            m => m,
        };
        state.push(Span::raw(format!("  {mode}")));
        if h.uptime_s > 0 {
            state.push(Span::styled(format!(", up {}", duration(h.uptime_s)), DIM));
        }
    }
    let kv = |k: &str, v: String, style: Style| Line::from(vec![Span::styled(format!("{k:<9}"), DIM), Span::styled(v, style)]);
    let mut lines = vec![Line::from(state)];
    let down = matches!(h.status, DevStatus::Down);
    let api_style = if app.kube_error.is_some() && !down { BAD } else { Style::new() };
    lines.push(kv("API", trunc(&app.kube_endpoint, w), api_style));
    let docker_style = if app.docker_error.is_some() && !down { BAD } else { Style::new() };
    let socket = app.docker_endpoint.strip_prefix("unix://").unwrap_or(&app.docker_endpoint);
    lines.push(kv("Docker", trunc_left(&home_dir_short(socket), w), docker_style));
    let engine = match &app.docker_error {
        Some(_) if down => "not running".into(),
        Some(e) => trunc(e, w),
        None if app.docker_version.is_empty() => "-".into(),
        None => trunc(&format!("{} ({})", app.docker_version, app.docker_os), w),
    };
    lines.push(kv("Engine", engine, if app.docker_error.is_some() && !down { BAD } else { DIM }));
    if let Some(ip) = &h.vm_ip {
        lines.push(kv("VM IP", ip.clone(), DIM));
    }
    f.render_widget(Paragraph::new(lines).block(block("Dev cluster")), left);

    // Resources.
    let inner = block("Resources").inner(right);
    f.render_widget(block("Resources"), right);
    let rows = Layout::vertical([Constraint::Length(1); 5]).split(inner);
    let label_w = 7u16;
    let gauge = |f: &mut Frame, row: Rect, label: &str, ratio: Option<f64>, text: String| {
        let [l, g] = Layout::horizontal([Constraint::Length(label_w), Constraint::Min(1)]).areas(row);
        f.render_widget(Paragraph::new(Span::styled(label.to_string(), DIM)), l);
        match ratio {
            Some(r) => {
                let color = if r > 0.9 { Color::Red } else if r > 0.7 { Color::Yellow } else { Color::Green };
                f.render_widget(Gauge::default().ratio(r.clamp(0.0, 1.0)).label(text).gauge_style(Style::new().fg(color).bg(Color::Black)), g);
            }
            None => f.render_widget(Paragraph::new(Span::styled(text, DIM)), g),
        }
    };
    // A limit on the Linux sandbox is its size; else what the engine sees.
    let limits = h.running.as_ref().filter(|s| s.mode != "vm");
    let cpus = limits.and_then(|s| s.cpus).map(|c| c as u64).or(app.vm_cpus).map(|c| c as f64);
    let mem_total = limits.and_then(|s| s.memory_mib).map(|m| m << 20).or(app.vm_memory);
    let cpu_text = match (h.cpu_pct, cpus) {
        (Some(p), Some(c)) => format!("{p:.0}% of {c:.0} CPUs"),
        (Some(p), None) => format!("{p:.0}%"),
        (None, Some(c)) => format!("{c:.0} CPUs"),
        (None, None) => "-".into(),
    };
    let cpu_ratio = h.cpu_pct.map(|p| p / 100.0 / cpus.unwrap_or(1.0));
    gauge(f, rows[0], "CPU", cpu_ratio, cpu_text);
    let mem_ratio = h.mem_used.zip(mem_total).map(|(u, t)| u as f64 / t.max(1) as f64);
    let mem_text = match (h.mem_used, mem_total) {
        (Some(u), Some(t)) => format!("{} / {}", bytes(u), bytes(t)),
        (Some(u), None) => bytes(u),
        (None, Some(t)) => format!("- / {}", bytes(t)),
        _ => "-".into(),
    };
    gauge(f, rows[1], "Memory", mem_ratio, mem_text);
    let disk_text = match (h.disk_used, h.disk_free) {
        (Some(u), Some(free)) => format!("{} used, {} free", bytes(u), bytes(free)),
        (Some(u), None) => format!("{} used", bytes(u)),
        (None, Some(free)) => format!("{} free", bytes(free)),
        _ => "-".into(),
    };
    let disk_ratio = h.disk_used.zip(h.disk_free).map(|(u, free)| u as f64 / (u + free).max(1) as f64);
    gauge(f, rows[2], "Disk", disk_ratio, disk_text);
    let running = app.containers.iter().filter(|c| c.running()).count();
    let pods_running = app.pods.iter().filter(|p| p.status == "Running").count();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{:<label_w$}", "Ctrs", label_w = label_w as usize), DIM),
            Span::raw(format!("{running}/{} running   ", app.containers.len())),
            Span::styled("Pods ", DIM),
            Span::raw(format!("{pods_running}/{}", app.pods.len())),
        ])),
        rows[3],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(format!("{:<label_w$}", "Images", label_w = label_w as usize), DIM), Span::raw(format!("{}  ({})", app.images.len(), bytes(app.images.iter().map(|i| i.size).sum())))])),
        rows[4],
    );

    // Contexts.
    let c = &h.contexts;
    let mark = |ok: bool| if ok { Span::styled("✓ ", OK) } else { Span::styled("✗ ", DIM) };
    let cw = ctx.width.saturating_sub(22) as usize;
    let mut lines = vec![Line::from(vec![mark(c.docker_context_is_litek8s), Span::styled("docker context   ", DIM), Span::raw(trunc(&c.docker_context, cw))])];
    match &c.docker_host_env {
        Some(v) => lines.push(Line::from(vec![
            mark(c.docker_host_is_litek8s),
            Span::styled("DOCKER_HOST      ", DIM),
            Span::styled(trunc(&home_dir_short(v), cw), if c.docker_host_is_litek8s { Style::new() } else { WARN }),
            Span::styled(if c.docker_host_is_litek8s { "" } else { " (wins over the context)" }, DIM),
        ])),
        None => lines.push(Line::from(vec![Span::raw("  "), Span::styled("DOCKER_HOST      ", DIM), Span::styled("unset", DIM)])),
    }
    let kname = if c.kube_context.is_empty() { "-".to_string() } else { c.kube_context.clone() };
    let kpath = trunc_left(&home_dir_short(&c.kubeconfig), cw.saturating_sub(kname.chars().count() + 4));
    lines.push(Line::from(vec![mark(c.kube_is_litek8s), Span::styled("kubectl context  ", DIM), Span::raw(trunc(&kname, cw)), Span::styled(format!("  ({kpath})"), DIM)]));
    f.render_widget(Paragraph::new(lines).block(block("Your tools point at")), ctx);

    if !app.dev_output.is_empty() {
        let title = match app.dev_busy {
            Some(op) => format!("litek8s dev {} …", format!("{op:?}").to_lowercase()),
            None => "Output".into(),
        };
        // The last lines that fit, wrapped.
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
        let lines: Vec<Line> = app.dev_output.iter().skip(first).map(|l| Line::raw(l.clone())).collect();
        f.render_widget(Paragraph::new(lines).block(block(&title)).wrap(Wrap { trim: false }), out);
    } else {
        let tip = match h.status {
            DevStatus::Down => "The dev cluster is down: press u to start it.",
            DevStatus::Remote => "Connected to a litek8s that is not this machine's dev cluster.",
            _ => "Switch docker/kubectl to litek8s in Settings (7). Help: ?",
        };
        f.render_widget(Paragraph::new(Span::styled(format!(" {tip}"), DIM)), out);
    }
}

fn table_area(app: &App, area: Rect, title: &str) -> (Block<'static>, Rect) {
    let mut t = title.to_string();
    if !app.filter().is_empty() {
        t = format!("{t}  /{}", app.filter());
    }
    let b = block(&t);
    let inner = b.inner(area);
    // Header row, then the list.
    app.list_top.set(inner.y + 1);
    (b, inner)
}

fn render_table(f: &mut Frame, app: &App, area: Rect, title: &str, header: Vec<&str>, rows: Vec<Row<'static>>, widths: Vec<Constraint>, empty: &str) {
    let (b, inner) = table_area(app, area, title);
    if rows.is_empty() {
        f.render_widget(b, area);
        f.render_widget(Paragraph::new(Span::styled(format!(" {empty}"), DIM)).wrap(Wrap { trim: false }), Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner });
        f.render_widget(Paragraph::new(Line::from(header.iter().map(|h| Span::styled(format!(" {h:<10}"), DIM)).collect::<Vec<_>>())), Rect { height: 1, ..inner });
        return;
    }
    let mut state = TableState::default().with_selected(Some(app.sel()));
    let table = Table::new(rows, widths)
        .header(Row::new(header.into_iter().map(|h| Cell::from(Span::styled(h, DIM)))))
        .block(b)
        .column_spacing(1)
        .row_highlight_style(SELECTED);
    f.render_stateful_widget(table, area, &mut state);
    app.list_offset.set(state.offset());
}

fn state_style(state: &str) -> Style {
    match state {
        "running" => OK,
        "restarting" | "paused" | "created" => WARN,
        "dead" => BAD,
        _ => DIM,
    }
}

fn containers(f: &mut Frame, app: &App, area: Rect) {
    if let Some(e) = &app.docker_error {
        return unavailable(f, area, "Containers", e, "Start the dev cluster (1, then u) or pass --docker-host.");
    }
    let wide = area.width >= 100;
    let rows: Vec<Row> = app
        .container_rows()
        .into_iter()
        .map(|r| match r {
            CRow::Group { name, count, running, collapsed } => Row::new(vec![
                Cell::from(Span::styled(format!("{} {name}", if collapsed { "▸" } else { "▾" }), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))),
                Cell::from(Span::styled(format!("{running}/{count} up"), if running == count { OK } else if running == 0 { DIM } else { WARN })),
                Cell::from(Span::styled("compose project", DIM)),
                Cell::from(""),
            ]),
            CRow::Item(i) => {
                let c = &app.containers[i];
                let name = if c.project.is_some() { format!("  {}", c.service.as_deref().unwrap_or(&c.name)) } else { c.name.clone() };
                let mut cells = vec![Cell::from(name), Cell::from(Span::styled(c.state.clone(), state_style(&c.state))), Cell::from(c.image.clone()), Cell::from(c.ports.clone())];
                if wide {
                    cells.insert(3, Cell::from(Span::styled(c.status.clone(), DIM)));
                }
                Row::new(cells)
            }
        })
        .collect();
    let (header, widths) = if wide {
        (vec!["NAME", "STATE", "IMAGE", "STATUS", "PORTS"], vec![Constraint::Percentage(25), Constraint::Length(10), Constraint::Percentage(25), Constraint::Length(22), Constraint::Fill(1)])
    } else {
        (vec!["NAME", "STATE", "IMAGE", "PORTS"], vec![Constraint::Percentage(32), Constraint::Length(9), Constraint::Percentage(30), Constraint::Fill(1)])
    };
    let running = app.containers.iter().filter(|c| c.running()).count();
    let title = format!("Containers ({running}/{} running)", app.containers.len());
    render_table(f, app, area, &title, header, rows, widths, "No containers. `docker run` against the litek8s socket and they show up here.");
}

/// `web-1`, `web-1 +2`: who uses an image or volume, in little space.
fn users_text(list: &[String]) -> Span<'static> {
    match list {
        [] => Span::styled("unused", DIM),
        [one] => Span::raw(one.clone()),
        [first, rest @ ..] => Span::raw(format!("{first} +{}", rest.len())),
    }
}

fn images(f: &mut Frame, app: &App, area: Rect) {
    if let Some(e) = &app.docker_error {
        return unavailable(f, area, "Images", e, "");
    }
    let wide = area.width >= 100;
    let rows: Vec<Row> = app
        .visible_images()
        .into_iter()
        .map(|i| {
            let img = &app.images[i];
            let mut users: Vec<String> = img.containers.clone();
            users.extend(img.pods.iter().map(|p| format!("pod {}", p.split_once('/').map(|(_, n)| n).unwrap_or(p))));
            let name = if img.dangling() { Span::styled("<none>", DIM) } else { Span::raw(img.tags.join(", ")) };
            let mut cells = vec![Cell::from(name), Cell::from(bytes(img.size)), Cell::from(age(app.now, img.created)), Cell::from(users_text(&users))];
            if wide {
                cells.insert(1, Cell::from(Span::styled(img.short_id().to_string(), DIM)));
            }
            Row::new(cells)
        })
        .collect();
    let total: u64 = app.images.iter().map(|i| i.size).sum();
    let title = format!("Images ({}, {})", app.images.len(), bytes(total));
    let (header, widths) = if wide {
        (vec!["IMAGE", "ID", "SIZE", "AGE", "USED BY"], vec![Constraint::Fill(3), Constraint::Length(12), Constraint::Length(8), Constraint::Length(5), Constraint::Fill(2)])
    } else {
        (vec!["IMAGE", "SIZE", "AGE", "USED BY"], vec![Constraint::Fill(1), Constraint::Length(8), Constraint::Length(5), Constraint::Fill(1)])
    };
    render_table(f, app, area, &title, header, rows, widths, "No images.");
}

fn volumes(f: &mut Frame, app: &App, area: Rect) {
    if let Some(e) = &app.docker_error {
        return unavailable(f, area, "Volumes", e, "");
    }
    let rows: Vec<Row> = app
        .visible_volumes()
        .into_iter()
        .map(|i| {
            let v = &app.volumes[i];
            let name = if v.anonymous { format!("{}… (anonymous)", &v.name[..12]) } else { v.name.clone() };
            Row::new(vec![Cell::from(name), Cell::from(v.size.map(bytes).unwrap_or_else(|| "-".into())), Cell::from(users_text(&v.users)), Cell::from(Span::styled(v.project.clone().unwrap_or_default(), DIM))])
        })
        .collect();
    let total: u64 = app.volumes.iter().filter_map(|v| v.size).sum();
    let title = format!("Volumes ({}, {})", app.volumes.len(), bytes(total));
    render_table(f, app, area, &title, vec!["VOLUME", "SIZE", "USED BY", "PROJECT"], rows, vec![Constraint::Percentage(40), Constraint::Length(9), Constraint::Fill(1), Constraint::Length(14)], "No volumes.");
}

fn space(f: &mut Frame, app: &App, area: Rect) {
    if let Some(e) = &app.docker_error {
        return unavailable(f, area, "Free space", e, "");
    }
    let [list, info] = Layout::vertical([Constraint::Length(RECLAIMS.len() as u16 + 4), Constraint::Min(1)]).areas(area);
    let mut total_sel = 0;
    let rows: Vec<Row> = RECLAIMS
        .iter()
        .map(|r| {
            let (b, n) = app.reclaimable(*r);
            let on = app.space_checked.contains(r);
            if on {
                total_sel += b;
            }
            let style = if n == 0 { DIM } else { Style::new() };
            Row::new(vec![
                Cell::from(Span::styled(if on { "[x]" } else { "[ ]" }, if on { Style::new().fg(ACCENT) } else { DIM })),
                Cell::from(Span::styled(r.title(), style)),
                Cell::from(Span::styled(n.to_string(), style)),
                Cell::from(Span::styled(bytes(b), style)),
            ])
        })
        .collect();
    render_table(f, app, list, "Reclaimable space", vec!["", "WHAT", "ITEMS", "SIZE"], rows, vec![Constraint::Length(3), Constraint::Fill(1), Constraint::Length(6), Constraint::Length(9)], "");
    let h = &app.home;
    let mut lines = vec![
        Line::from(vec![Span::styled("Selected: ", DIM), Span::styled(bytes(total_sel), BOLD), Span::styled("   p frees it (asks first)", DIM)]),
        Line::raw(""),
    ];
    if let Some(u) = h.disk_used {
        let room = (info.width as usize).saturating_sub(30);
        lines.push(Line::from(vec![Span::styled("litek8s data  ", DIM), Span::raw(format!("{} in {}", bytes(u), trunc_left(&home_dir_short(&h.data_dir), room)))]));
    }
    if let Some(free) = h.disk_free {
        let style = if free < 40_000_000_000 { WARN } else { Style::new() };
        lines.push(Line::from(vec![Span::styled("disk free    ", DIM), Span::styled(bytes(free), style)]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Docker and Kubernetes share one image store: images no container or pod", DIM));
    lines.push(Line::styled("uses are unused (pods pull them again when needed). Volumes are never", DIM));
    lines.push(Line::styled("selected by default: they hold data.", DIM));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 }));
}

fn kube(f: &mut Frame, app: &App, area: Rect) {
    if let Some(e) = &app.kube_error {
        return unavailable(f, area, "Kubernetes", e, "Start the dev cluster, or pass --context for another cluster.");
    }
    let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let ns = app.namespace.clone().unwrap_or_else(|| "all".into());
    let tab = |name: &str, on: bool| if on { Span::styled(format!("[{name}]"), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)) } else { Span::styled(format!(" {name} "), DIM) };
    let mut spans = vec![
        Span::styled(" namespace ", DIM),
        Span::styled(ns.clone(), BOLD),
        Span::raw("   "),
        tab("Pods", app.kube_view == KubeView::Pods),
        tab("Deployments", app.kube_view == KubeView::Deployments),
    ];
    for (i, fwd) in app.forwards.iter().enumerate() {
        let gone = !app.pods.iter().any(|p| p.namespace == fwd.namespace && p.name == fwd.pod);
        let text = format!("{}{}→{}:{}{}", if i == 0 { "   ⇄ " } else { ", " }, fwd.local, fwd.pod, fwd.remote, if gone { " (pod gone)" } else { "" });
        spans.push(Span::styled(text, if gone { WARN } else { OK }));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), head);
    let all = app.namespace.is_none();
    let ctx = if app.kube_context.is_empty() { String::new() } else { format!(" — {}", app.kube_context) };
    match app.kube_view {
        KubeView::Pods => {
            let rows: Vec<Row> = app
                .visible_pods()
                .into_iter()
                .map(|i| {
                    let p = &app.pods[i];
                    let style = match p.status.as_str() {
                        "Running" | "Completed" | "Succeeded" => OK,
                        "Pending" | "ContainerCreating" | "Terminating" | "PodInitializing" => WARN,
                        _ => BAD,
                    };
                    let name = if all { format!("{}/{}", p.namespace, p.name) } else { p.name.clone() };
                    Row::new(vec![Cell::from(name), Cell::from(p.ready.clone()), Cell::from(Span::styled(p.status.clone(), style)), Cell::from(p.restarts.to_string()), Cell::from(age(app.now, p.created))])
                })
                .collect();
            render_table(
                f,
                app,
                body,
                &format!("Pods{ctx}"),
                vec!["NAME", "READY", "STATUS", "RESTARTS", "AGE"],
                rows,
                vec![Constraint::Fill(1), Constraint::Length(6), Constraint::Length(18), Constraint::Length(8), Constraint::Length(5)],
                "No pods here. k9s gives the full Kubernetes picture (see ?).",
            );
        }
        KubeView::Deployments => {
            let rows: Vec<Row> = app
                .visible_deployments()
                .into_iter()
                .map(|i| {
                    let d = &app.deployments[i];
                    let style = if d.ready >= d.desired { OK } else { WARN };
                    let name = if all { format!("{}/{}", d.namespace, d.name) } else { d.name.clone() };
                    Row::new(vec![Cell::from(name), Cell::from(Span::styled(format!("{}/{}", d.ready, d.desired), style)), Cell::from(d.updated.to_string()), Cell::from(d.available.to_string()), Cell::from(age(app.now, d.created))])
                })
                .collect();
            render_table(
                f,
                app,
                body,
                &format!("Deployments{ctx}"),
                vec!["NAME", "READY", "UP-TO-DATE", "AVAILABLE", "AGE"],
                rows,
                vec![Constraint::Fill(1), Constraint::Length(7), Constraint::Length(10), Constraint::Length(9), Constraint::Length(5)],
                "No deployments here.",
            );
        }
    }
}

fn settings(f: &mut Frame, app: &App, area: Rect) {
    let restart = app.restart_needed();
    let notes = 3 + if restart.is_empty() { 0 } else { 1 } + if app.config_error.is_some() { 1 } else { 0 };
    let [list, info] = Layout::vertical([Constraint::Length(SETTINGS.len() as u16 + 3), Constraint::Min(notes)]).areas(area);
    let c = &app.home.contexts;
    let mac = cfg!(target_os = "macos");
    let default = |v: String, d: String| if v.is_empty() { Span::styled(format!("{d} (default)"), DIM) } else { Span::raw(v) };
    let rows: Vec<Row> = SETTINGS
        .iter()
        .map(|s| {
            let (name, value) = match s {
                Setting::Field(Field::Cpus) if mac => ("VM CPUs", default(app.setting_value(Field::Cpus), crate::dev::config::default_cpus().to_string())),
                Setting::Field(Field::Cpus) => ("CPU limit", default(app.setting_value(Field::Cpus), "none".into())),
                Setting::Field(Field::Memory) if mac => ("VM memory (MiB)", default(app.setting_value(Field::Memory), crate::dev::config::DEFAULT_MEMORY_MIB.to_string())),
                Setting::Field(Field::Memory) => ("Memory limit (MiB)", default(app.setting_value(Field::Memory), "none".into())),
                Setting::Field(Field::Shares) if mac => ("Shared directories", default(app.setting_value(Field::Shares), "$HOME".into())),
                Setting::Field(Field::Shares) => ("Shared directories", Span::styled("macOS VM only (Linux uses the host's files)", DIM)),
                Setting::Field(Field::Port) => ("API server port", default(app.setting_value(Field::Port), crate::dev::DEFAULT_PORT.to_string())),
                Setting::Field(Field::Publish) => (
                    "Published ports on",
                    Span::raw(match app.config.publish {
                        crate::dev::config::Publish::Localhost => "localhost only",
                        crate::dev::config::Publish::All => "all interfaces (LAN)",
                    }),
                ),
                Setting::DockerContext => (
                    "docker context",
                    if c.docker_context_is_litek8s { Span::styled(format!("{} ✓  (enter: switch back)", c.docker_context), OK) } else { Span::raw(format!("{}  (enter: use litek8s)", c.docker_context)) },
                ),
                Setting::KubeContext => (
                    "kubectl context",
                    if c.kube_is_litek8s { Span::styled(format!("{} ✓  (enter: switch back)", c.kube_context), OK) } else { Span::raw(format!("{}  (enter: use litek8s-dev)", if c.kube_context.is_empty() { "-" } else { &c.kube_context })) },
                ),
                Setting::EditFile => ("Config file", Span::raw(format!("{}  (enter: $EDITOR)", trunc_left(&home_dir_short(&app.config_path), list.width.saturating_sub(43) as usize)))),
            };
            Row::new(vec![Cell::from(Span::styled(name, DIM)), Cell::from(value)])
        })
        .collect();
    render_table(f, app, list, "Settings", vec!["SETTING", "VALUE"], rows, vec![Constraint::Length(20), Constraint::Fill(1)], "");
    let mut lines = Vec::new();
    if let Some(e) = &app.config_error {
        let e = e.rsplit_once(" is not valid: ").map(|(_, why)| why).unwrap_or(e);
        lines.push(Line::styled(format!(" ✗ config file: {e}"), BAD));
    }
    if !restart.is_empty() {
        lines.push(Line::styled(format!(" ⟳ Restart needed for {}: Home (1), d then u.", restart.join(", ")), WARN));
    }
    let platform = if mac { "CPUs, memory and shares size the macOS VM." } else { "CPU and memory limits apply to everything the dev cluster runs." };
    lines.push(Line::styled(format!(" {platform}"), DIM));
    lines.push(Line::styled(" Settings apply on the next `litek8s dev up` (its flags win).", DIM));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info);
}

fn unavailable(f: &mut Frame, area: Rect, title: &str, error: &str, hint: &str) {
    let lines = vec![Line::styled(format!(" {error}"), BAD), Line::raw(""), Line::styled(format!(" {hint}"), DIM)];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block(title)), area);
}

fn log_view(f: &mut Frame, v: &super::app::LogView, area: Rect) {
    let mut title = v.title.clone();
    if v.follow {
        title.push_str("  [follow]");
    }
    if let Some(e) = &v.ended {
        title.push_str(&format!("  [{e}]"));
    }
    let b = block(&title);
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

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn confirm(f: &mut Frame, title: &str, lines: &[String], area: Rect) {
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(20).max(title.len()).clamp(30, 74) as u16 + 4;
    let r = centered(area, width, lines.len() as u16 + 4);
    f.render_widget(Clear, r);
    let mut text: Vec<Line> = lines.iter().map(|l| Line::raw(l.clone())).collect();
    text.push(Line::raw(""));
    text.push(Line::from(vec![Span::styled("y", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw(" yes   "), Span::styled("n/esc", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw(" no")]));
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().border_style(WARN).title(Span::styled(format!(" {title} "), BOLD))), r);
}

fn input(f: &mut Frame, prompt: &str, value: &str, error: Option<&str>, kind: &InputKind, area: Rect) {
    if *kind == InputKind::Filter {
        // The filter edits in the hint line.
        let r = Rect { y: area.y + area.height - 1, height: 1, ..area };
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(Line::from(vec![Span::styled(" /", Style::new().fg(ACCENT)), Span::raw(value.to_string()), Span::styled("█", DIM)])), r);
        return;
    }
    let r = centered(area, 70, if error.is_some() { 6 } else { 5 });
    f.render_widget(Clear, r);
    let mut lines = vec![Line::from(vec![Span::raw(value.to_string()), Span::styled("█", Style::new().fg(ACCENT))])];
    if let Some(e) = error {
        lines.push(Line::styled(e.to_string(), BAD));
    }
    let enter = if matches!(kind, InputKind::Setting(_)) { "enter save" } else { "enter ok" };
    lines.push(Line::styled(format!("{enter}   esc cancel   ctrl-u clear"), DIM));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered().border_style(Style::new().fg(ACCENT)).title(Span::styled(format!(" {prompt} "), BOLD))), r);
}

/// The help screen: per view, its keys (each line fits 76 columns).
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    ("Everywhere", &[("1-7 tab", "views"), ("↑↓ jk", "move"), ("/", "filter"), ("ctrl-r", "refresh"), ("q", "quit")]),
    ("Home", &[("u", "start"), ("d", "stop (asks)"), ("R", "reset: delete all data (asks)")]),
    ("Containers", &[("l", "logs"), ("s", "shell"), ("u x r", "start stop restart"), ("d", "remove"), ("space", "fold")]),
    ("Images", &[("d", "remove (asks)")]),
    ("Volumes", &[("d", "remove (asks)")]),
    ("Free space", &[("space", "select a category"), ("p", "free the selected (asks)")]),
    ("Kubernetes", &[("v", "pods/deployments"), ("n", "namespace"), ("a", "all"), ("l", "logs"), ("f", "port-forward")]),
    ("", &[("X", "stop forwards"), ("s", "scale"), ("r", "restart"), ("d", "delete pod")]),
    ("Settings", &[("enter", "edit"), ("del", "default"), ("space", "toggle / switch context")]),
    ("Logs", &[("f", "follow"), ("w", "wrap"), ("PgUp PgDn", "scroll"), ("g G", "top/end"), ("esc", "back")]),
];

fn help(f: &mut Frame, app: &App, area: Rect) {
    let r = centered(area, 78, HELP.len() as u16 + 7);
    f.render_widget(Clear, r);
    let mut lines = Vec::new();
    for (section, keys) in HELP {
        let mut row: Vec<Span> = vec![Span::styled(format!("{section:<11}"), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))];
        for (k, v) in keys.iter() {
            row.push(Span::styled(*k, BOLD));
            row.push(Span::raw(format!(" {v}  ")));
        }
        lines.push(Line::from(row));
    }
    let ctx = if app.kube_context.is_empty() { "litek8s-dev" } else { &app.kube_context };
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled("More Kubernetes: ", DIM), Span::styled(format!("k9s --context {ctx}"), BOLD), Span::styled(" (https://k9scli.io)", DIM)]));
    lines.push(Line::styled("Destructive actions always ask. Mouse: litek8s ui --mouse. Docs: docs/ui.md", DIM));
    lines.push(Line::styled("Any key closes this.", DIM));
    f.render_widget(Paragraph::new(lines).block(Block::bordered().border_style(Style::new().fg(ACCENT)).title(Span::styled(" Keys ", BOLD))), r);
}
