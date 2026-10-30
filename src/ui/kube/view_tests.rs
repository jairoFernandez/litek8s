//! The Kubernetes view: keys against [`KubeUi`] inside an [`App`], frames
//! drawn on ratatui's TestBackend at 80x24.

use std::sync::Arc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

use super::state::*;
use super::table::{Health, Row};
use crate::ui::app::{App, Effect, Msg, Tab};

fn key(c: char) -> KeyEvent {
    KeyEvent::from(KeyCode::Char(c))
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn code(c: KeyCode) -> KeyEvent {
    KeyEvent::from(c)
}

fn typing(app: &mut App, s: &str) {
    for c in s.chars() {
        app.key(key(c));
    }
}

fn render(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| crate::ui::view::draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

const NOW: i64 = 1_767_225_600 + 3600;

pub fn pod(ns: &str, name: &str, ready: &str, status: &str, restarts: &str, app_label: &str) -> Row {
    let health = match status {
        "Running" if ready.split('/').next() == ready.split('/').nth(1) => Health::Ok,
        "Running" | "Pending" => Health::Warn,
        "Completed" => Health::Done,
        _ => Health::Bad,
    };
    let cells: Vec<String> = [name, ready, status, restarts, "", "10.0.0.1", "node-1"].iter().map(|s| s.to_string()).collect();
    let hay = format!("{} {}", name, cells.join(" ")).to_lowercase();
    Row {
        ns: ns.into(),
        name: name.into(),
        uid: format!("uid-{name}"),
        cells,
        created: 1_767_225_600 - name.len() as i64 * 60,
        deleting: false,
        labels: json!({"metadata": {"labels": {"app": app_label}}}),
        owners: vec![super::table::Owner { kind: "ReplicaSet".into(), name: format!("{app_label}-7d9"), uid: "rs".into(), controller: true }],
        health,
        haystack: hay,
    }
}

const POD_COLS: &[(&str, i64)] = &[("Name", 0), ("Ready", 0), ("Status", 0), ("Restarts", 0), ("Age", 0), ("IP", 1), ("Node", 1)];

fn pods_key(_ui: &KubeUi, ns: Option<&str>) -> WatchKey {
    WatchKey { res: "v1/pods".into(), ns: ns.map(str::to_string), fields: String::new() }
}

/// An app on the Kubernetes tab with pods in two namespaces.
fn app() -> App {
    let mut app = App::new("unix:///tmp/d.sock".into(), "https://k8s.example:6443".into(), "prod".into());
    app.now = NOW;
    app.tab = Tab::Kube;
    app.kube = KubeUi::new(None);
    app.kube.now = NOW;
    // A litek8s cluster: everything may be changed.
    app.kube.context = "prod".into();
    app.apply(Msg::Kube(KMsg::Identified { context: "prod".into(), litek8s: true }));
    let rows = vec![
        pod("default", "web-7d9-abc", "1/1", "Running", "0", "web"),
        pod("default", "web-7d9-def", "0/1", "CrashLoopBackOff", "7 (2m ago)", "web"),
        pod("default", "db-0", "1/1", "Running", "0", "db"),
        pod("kube-system", "coredns-5d78c", "1/1", "Running", "1", "dns"),
        pod("kube-system", "job-x-123", "0/1", "Completed", "0", "job"),
    ];
    let wk = pods_key(&app.kube, None);
    app.apply(Msg::Kube(KMsg::Data(wk, test_snapshot(POD_COLS, rows))));
    let ns_rows = ["default", "kube-system", "web"].iter().map(|n| Row { name: n.to_string(), cells: vec![n.to_string(), "Active".into(), String::new()], haystack: n.to_string(), ..Default::default() }).collect();
    app.apply(Msg::Kube(KMsg::Data(WatchKey { res: "v1/namespaces".into(), ns: None, fields: String::new() }, test_snapshot(&[("Name", 0), ("Status", 0), ("Age", 0)], ns_rows))));
    app.kube.take_effects();
    app
}

fn top_table(app: &App) -> &TableView {
    app.kube.top_table().expect("a table on top")
}

fn kube_effects(e: Vec<Effect>) -> Vec<KEffect> {
    e.into_iter()
        .filter_map(|e| match e {
            Effect::Kube(k) => Some(k),
            _ => None,
        })
        .collect()
}

#[test]
fn pods_table_fits_80x24_with_namespaces_and_health() {
    let app = app();
    let s = render(&app, 80, 24);
    assert!(s.contains("⎈ prod") && s.contains("ns all"), "{s}");
    assert!(s.contains("pods(all) [5]"), "{s}");
    assert!(s.contains("NAMESPACE") && s.contains("NAME") && s.contains("READY") && s.contains("STATUS") && s.contains("AGE"), "{s}");
    assert!(s.contains("default") && s.contains("web-7d9-abc") && s.contains("CrashLoopBackOff"), "{s}");
    // The wide-only columns wait for ctrl-w.
    assert!(!s.contains("10.0.0.1"), "{s}");
    for line in s.lines() {
        assert!(line.chars().count() <= 80);
    }
    let mut app = app;
    app.key(ctrl('w'));
    let s = render(&app, 120, 24);
    assert!(s.contains("10.0.0.1") && s.contains("node-1"), "{s}");
    // Ages come from creation times, not the server's text.
    assert!(s.contains("71m"), "{s}");
}

#[test]
fn command_mode_switches_resources_namespaces_and_contexts() {
    let mut app = app();
    app.key(key(':'));
    typing(&mut app, "dep");
    let s = render(&app, 80, 24);
    assert!(s.contains(":dep") && s.contains("deploy"), "completion shows: {s}");
    // Tab completes, enter runs; the informer for deployments is asked for.
    app.key(code(KeyCode::Tab));
    let Overlay::Prompt(p) = &app.kube.overlay else { panic!() };
    assert_eq!(p.text, "deploy");
    let effects = kube_effects(app.key(code(KeyCode::Enter)));
    assert!(effects.iter().any(|e| matches!(e, KEffect::SaveHistory(h) if h == &vec!["deploy".to_string()])));
    assert_eq!(top_table(&app).res.kind, "Deployment");
    let pending = app.kube.take_effects();
    assert!(pending.iter().any(|e| matches!(e, KEffect::Watch(w) if w.iter().any(|(r, _)| r.plural == "deployments"))), "{pending:?}");
    // `:pods kube-system` sets the namespace; ↑ brings back the last command.
    app.key(key(':'));
    typing(&mut app, "pods kube-system");
    app.key(code(KeyCode::Enter));
    assert_eq!(app.kube.namespace.as_deref(), Some("kube-system"));
    assert_eq!(top_table(&app).key().ns.as_deref(), Some("kube-system"));
    app.key(key(':'));
    app.key(code(KeyCode::Up));
    let Overlay::Prompt(p) = &app.kube.overlay else { panic!() };
    assert_eq!(p.text, "pods kube-system");
    app.key(code(KeyCode::Esc));
    // Unknown names say so.
    app.key(key(':'));
    typing(&mut app, "frobs");
    app.key(code(KeyCode::Enter));
    assert!(app.status.as_ref().is_some_and(|(t, e)| *e && t.contains("unknown resource")), "{:?}", app.status);
    // :ctx name switches the context (the runtime reconnects).
    app.key(key(':'));
    typing(&mut app, "ctx staging");
    assert_eq!(app.key(code(KeyCode::Enter)).first(), Some(&Effect::UseContext("staging".into())));
    // `0` toggles all namespaces.
    app.key(key('0'));
    assert_eq!(app.kube.namespace, None);
    app.key(key('0'));
    assert_eq!(app.kube.namespace.as_deref(), Some("kube-system"));
    // Digits 1-8 are still the app's tabs.
    app.key(key('1'));
    assert_eq!(app.tab, Tab::Home);
}

#[test]
fn any_resource_from_discovery_including_crds() {
    let mut app = app();
    let mut list = super::resources::builtins();
    list.push(super::resources::Resource { group: "cert-manager.io".into(), version: "v1".into(), plural: "certificates".into(), singular: "certificate".into(), kind: "Certificate".into(), namespaced: true, short: vec!["cert".into()], ..Default::default() });
    app.apply(Msg::Kube(KMsg::Discovery(Ok(list))));
    app.key(key(':'));
    typing(&mut app, "cert");
    app.key(code(KeyCode::Enter));
    assert_eq!(top_table(&app).res.name(), "certificates.cert-manager.io");
    let wk = top_table(&app).key();
    assert_eq!(wk.res, "cert-manager.io/v1/certificates");
    // A CRD's printer columns come as a Table like any other.
    let rows = vec![Row { ns: "default".into(), name: "site-tls".into(), cells: vec!["site-tls".into(), "True".into(), "site-tls".into(), "".into()], haystack: "site-tls true".into(), ..Default::default() }];
    app.apply(Msg::Kube(KMsg::Data(wk.clone(), test_snapshot(&[("Name", 0), ("Ready", 0), ("Secret", 0), ("Age", 0)], rows))));
    let s = render(&app, 80, 24);
    assert!(s.contains("certificates.cert-manager.io(all) [1]") && s.contains("site-tls") && s.contains("SECRET"), "{s}");
    // :aliases lists every type, and enter opens one.
    app.key(key(':'));
    typing(&mut app, "aliases");
    app.key(code(KeyCode::Enter));
    typing(&mut app, "certif");
    let s = render(&app, 80, 24);
    assert!(s.contains("certificates.cert-manager.io (cert)"), "{s}");
}

#[test]
fn filters_live_with_modes_selectors_inverse_and_memory() {
    let mut app = app();
    app.key(key('/'));
    typing(&mut app, "web");
    assert_eq!(top_table(&app).rows.len(), 2);
    let s = render(&app, 80, 24);
    assert!(s.contains("/web") && s.contains("2 of 5"), "{s}");
    // Tab: regex, then fuzzy.
    app.key(code(KeyCode::Tab));
    for _ in 0..3 {
        app.key(code(KeyCode::Backspace));
    }
    typing(&mut app, "^(db|core)");
    assert_eq!(top_table(&app).rows.len(), 2);
    app.key(code(KeyCode::Tab));
    for _ in 0.."^(db|core)".len() {
        app.key(code(KeyCode::Backspace));
    }
    typing(&mut app, "wdef");
    assert_eq!(top_table(&app).rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["web-7d9-def"]);
    app.key(code(KeyCode::Tab));
    app.key(code(KeyCode::Esc));
    assert_eq!(top_table(&app).rows.len(), 5);
    // Inverse and label selectors; enter keeps it, shown in the title.
    app.key(key('/'));
    typing(&mut app, "!running -l app!=job");
    let Overlay::Prompt(p) = &app.kube.overlay else { panic!() };
    assert_eq!((p.text.as_str(), p.mode), ("!running -l app!=job", super::filter::Mode::Substring));
    app.key(code(KeyCode::Enter));
    let names: Vec<&str> = top_table(&app).rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["web-7d9-def"]);
    assert!(render(&app, 80, 24).contains("</!running -l app!=job>"));
    // A field selector goes to the server: another informer.
    app.key(key('/'));
    for _ in 0..30 {
        app.key(code(KeyCode::Backspace));
    }
    typing(&mut app, "-f status.phase=Running");
    app.key(code(KeyCode::Enter));
    assert_eq!(top_table(&app).key().fields, "status.phase=Running");
    // The filter comes back with its resource.
    app.key(key(':'));
    typing(&mut app, "deploy");
    app.key(code(KeyCode::Enter));
    assert!(top_table(&app).filter.raw.is_empty());
    app.key(key(':'));
    typing(&mut app, "pods");
    app.key(code(KeyCode::Enter));
    assert_eq!(top_table(&app).filter.raw, "-f status.phase=Running");
    // esc clears it.
    app.key(code(KeyCode::Esc));
    assert!(top_table(&app).filter.raw.is_empty());
    // ctrl-z: faults only.
    app.key(ctrl('z'));
    assert_eq!(top_table(&app).rows.len(), 1);
}

#[test]
fn sorting_by_columns() {
    let mut app = app();
    app.key(key('N'));
    let names: Vec<&str> = top_table(&app).rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["coredns-5d78c", "db-0", "job-x-123", "web-7d9-abc", "web-7d9-def"]);
    app.key(key('O'));
    assert_eq!(top_table(&app).rows[0].name, "web-7d9-def");
    // Restarts by number, not text.
    app.key(key('>'));
    app.key(key('>'));
    app.key(key('>'));
    assert_eq!(top_table(&app).sort.as_ref().unwrap().0, "RESTARTS");
    let s = render(&app, 80, 24);
    assert!(s.contains("RESTARTS↓"), "{s}");
    assert_eq!(top_table(&app).rows[0].name, "web-7d9-def");
    app.key(key('A'));
    assert_eq!(top_table(&app).sort.as_ref().unwrap().0, "AGE");
}

#[test]
fn describe_yaml_search_and_edit() {
    let mut app = app();
    // web-7d9-abc is the second row (namespaces first).
    app.key(key('j'));
    let effects = kube_effects(app.key(key('d')));
    let [KEffect::Describe { id, obj }] = &effects[..] else { panic!("{effects:?}") };
    assert_eq!((obj.ns.as_str(), obj.name.as_str()), ("default", "web-7d9-def"));
    let lines: Vec<String> = ["Name:         web-7d9-def", "Namespace:    default", "Status:       Running", "Events:", "  Warning  BackOff  kubelet  Back-off restarting failed container"].iter().map(|s| s.to_string()).collect();
    app.apply(Msg::Kube(KMsg::Pager { id: *id, lines, append: false, done: true, error: None }));
    let s = render(&app, 80, 24);
    assert!(s.contains("describe po/default/web-7d9-def") && s.contains("Back-off restarting"), "{s}");
    app.key(key('/'));
    typing(&mut app, "backoff");
    app.key(code(KeyCode::Enter));
    let View::Pager(p) = app.kube.top() else { panic!() };
    assert_eq!(p.matches, vec![4]);
    // y: the YAML of the same object; e: edit it.
    let effects = kube_effects(app.key(key('y')));
    assert!(matches!(&effects[..], [KEffect::Yaml { managed: false, .. }]), "{effects:?}");
    let effects = kube_effects(app.key(key('e')));
    assert!(matches!(&effects[..], [KEffect::Edit { text: None, .. }]), "{effects:?}");
    // The server validated an edit: a diff to apply with y.
    let obj = ObjRef { res: app.kube.catalog.find("pods").unwrap().clone(), ns: "default".into(), name: "web-7d9-def".into() };
    app.apply(Msg::Kube(KMsg::Diff { obj: obj.clone(), lines: vec!["@@ -1,2 +1,2 @@".into(), "-  replicas: 1".into(), "+  replicas: 2".into()], body: "x: 1\n".into() }));
    let s = render(&app, 80, 24);
    assert!(s.contains("+  replicas: 2") && s.contains(" y  apply"), "{s}");
    assert_eq!(kube_effects(app.key(key('y'))), vec![KEffect::Apply { obj, body: "x: 1\n".into() }]);
    // esc goes back down the stack to the table.
    app.key(code(KeyCode::Esc));
    assert!(app.kube.top_table().is_some());
}

#[test]
fn deletes_ask_with_cascade_and_force_and_take_marks() {
    let mut app = app();
    app.key(key(' '));
    app.key(key(' '));
    let s = render(&app, 80, 24);
    assert!(s.contains("◆2"), "{s}");
    assert!(app.key(ctrl('d')).is_empty());
    let s = render(&app, 80, 24);
    assert!(s.contains("Delete 2 objects?") && s.contains("cascade: Background"), "{s}");
    app.key(key('p'));
    app.key(key('f'));
    let effects = kube_effects(app.key(key('y')));
    let [KEffect::Delete { objs, propagation, force }] = &effects[..] else { panic!("{effects:?}") };
    assert_eq!(objs.len(), 2);
    assert_eq!((*propagation, *force), (Propagation::Foreground, true));
    assert!(top_table(&app).marks.is_empty());
    // read-only refuses.
    app.kube.config.read_only = true;
    app.key(ctrl('d'));
    assert!(app.kube.overlay == Overlay::None);
    assert!(app.status.as_ref().is_some_and(|(t, _)| t.contains("read-only")));
}

#[test]
fn logs_pick_containers_stream_filter_and_pause() {
    let mut app = app();
    app.key(key('j'));
    app.key(key('j'));
    let effects = kube_effects(app.key(key('l')));
    let [KEffect::Containers { obj, purpose }] = &effects[..] else { panic!("{effects:?}") };
    let (obj, purpose) = (obj.clone(), *purpose);
    let list = vec![ContainerInfo { name: "app".into(), kind: String::new(), running: true }, ContainerInfo { name: "sidecar".into(), kind: String::new(), running: true }];
    app.apply(Msg::Kube(KMsg::Containers { obj, list, purpose }));
    let s = render(&app, 80, 24);
    assert!(s.contains("all containers") && s.contains("sidecar"), "{s}");
    app.key(code(KeyCode::Enter));
    let pending = app.kube.take_effects();
    let Some(KEffect::Logs { id, target, previous: false, tail: 100, .. }) = pending.iter().find(|e| matches!(e, KEffect::Logs { .. })) else { panic!("{pending:?}") };
    assert!(matches!(target, LogTarget::Pod { container: None, .. }));
    let id = *id;
    app.apply(Msg::Kube(KMsg::LogSources { id, sources: vec!["app".into(), "sidecar".into()] }));
    app.apply(Msg::Kube(KMsg::LogLines { id, src: 0, lines: vec!["2026-10-19T10:00:00.123456789Z GET / 200".into(), "2026-10-19T10:00:01.000000000Z GET /health 200".into()] }));
    app.apply(Msg::Kube(KMsg::LogLines { id, src: 1, lines: vec!["2026-10-19T10:00:02.000000000Z ERROR upstream gone".into()] }));
    let s = render(&app, 80, 24);
    assert!(s.contains("app") && s.contains("GET / 200") && s.contains("sidecar ERROR upstream gone") && s.contains("● follow"), "{s}");
    assert!(!s.contains("10:00:00.123"), "timestamps start hidden: {s}");
    app.key(key('t'));
    assert!(render(&app, 80, 24).contains("10:00:00.123"));
    app.key(key('/'));
    typing(&mut app, "error");
    app.key(code(KeyCode::Enter));
    let View::Logs(l) = app.kube.top() else { panic!() };
    assert_eq!(l.visible().len(), 1);
    app.key(key('f'));
    assert!(render(&app, 80, 24).contains("○ paused"));
    // Bounded: the oldest lines go.
    if let Some(View::Logs(l)) = app.kube.views.last_mut() {
        l.cap = 2;
    }
    app.apply(Msg::Kube(KMsg::LogLines { id, src: 0, lines: vec!["a".into(), "b".into()] }));
    let View::Logs(l) = app.kube.top() else { panic!() };
    assert_eq!((l.lines.len(), l.dropped), (2, 3));
    // esc leaves (and stops the streams).
    app.key(code(KeyCode::Esc));
    app.key(code(KeyCode::Esc));
    assert!(app.kube.take_effects().contains(&KEffect::StopLogs(id)));
}

#[test]
fn workloads_scale_restart_drill_and_xray() {
    let mut app = app();
    app.key(key(':'));
    typing(&mut app, "deploy -n default");
    app.key(code(KeyCode::Enter));
    let wk = top_table(&app).key();
    let rows = vec![Row { ns: "default".into(), name: "web".into(), uid: "d1".into(), cells: vec!["web".into(), "2/3".into(), "3".into(), "2".into(), "".into()], health: Health::Warn, haystack: "web".into(), ..Default::default() }];
    app.apply(Msg::Kube(KMsg::Data(wk, test_snapshot(&[("Name", 0), ("Ready", 0), ("Up-to-date", 0), ("Available", 0), ("Age", 0)], rows))));
    app.key(key_s());
    let Overlay::Input(i) = &app.kube.overlay else { panic!("{:?}", app.kube.overlay) };
    assert_eq!(i.value, "3");
    app.key(code(KeyCode::Backspace));
    app.key(KeyEvent::from(KeyCode::Char('x')));
    assert!(app.key(code(KeyCode::Enter)).is_empty());
    app.key(code(KeyCode::Backspace));
    app.key(KeyEvent::from(KeyCode::Char('5')));
    let effects = kube_effects(app.key(code(KeyCode::Enter)));
    assert!(matches!(&effects[..], [KEffect::Scale { replicas: 5, .. }]), "{effects:?}");
    app.key(KeyEvent::from(KeyCode::Char('r')));
    let effects = kube_effects(app.key(KeyEvent::from(KeyCode::Char('y'))));
    assert!(matches!(&effects[..], [KEffect::Restart(o)] if o.len() == 1), "{effects:?}");
    // Enter: its pods, by the selector the runtime finds.
    let effects = kube_effects(app.key(code(KeyCode::Enter)));
    assert!(matches!(&effects[..], [KEffect::DrillDown(o)] if o.name == "web"), "{effects:?}");
    app.apply(Msg::Kube(KMsg::Open { res: "v1/pods".into(), ns: Some("default".into()), labels: Some("app=web".into()), fields: None, title: "pods of deploy/default/web".into() }));
    let t = top_table(&app);
    assert_eq!(t.scope.title.as_deref(), Some("pods of deploy/default/web"));
    let s = render(&app, 80, 24);
    assert!(s.contains("‹app=web›") && s.contains("deployments › pods of deploy/default/web"), "{s}");
    app.key(code(KeyCode::Esc));
    // x: the owner tree.
    let effects = kube_effects(app.key(KeyEvent::from(KeyCode::Char('x'))));
    let [KEffect::Xray { id, .. }] = &effects[..] else { panic!("{effects:?}") };
    let pods = app.kube.catalog.find("pods").unwrap().clone();
    let nodes = vec![
        XNode { depth: 0, kind: "deploy".into(), name: "web".into(), status: "2/3 ready".into(), health: Health::Warn, obj: None, container_of: None },
        XNode { depth: 1, kind: "rs".into(), name: "web-7d9".into(), status: "2/3 ready, revision 4".into(), health: Health::Warn, obj: None, container_of: None },
        XNode { depth: 2, kind: "pod".into(), name: "web-7d9-abc".into(), status: "Running 1/1".into(), health: Health::Ok, obj: Some(ObjRef { res: pods, ns: "default".into(), name: "web-7d9-abc".into() }), container_of: None },
        XNode { depth: 3, kind: "container".into(), name: "nginx".into(), status: "running, ready".into(), health: Health::Ok, obj: None, container_of: Some(("default".into(), "web-7d9-abc".into())) },
    ];
    app.apply(Msg::Kube(KMsg::Xray { id: *id, nodes, error: None }));
    let s = render(&app, 80, 24);
    assert!(s.contains("▾ ◐ deploy web") && s.contains("    ▾ ● pod web-7d9-abc") && s.contains("● container nginx"), "{s}");
    // Fold the rs; act on a pod.
    app.key(KeyEvent::from(KeyCode::Char('j')));
    app.key(code(KeyCode::Enter));
    assert!(!render(&app, 80, 24).contains("pod web-7d9-abc"));
    app.key(code(KeyCode::Enter));
    app.key(KeyEvent::from(KeyCode::Char('j')));
    let effects = kube_effects(app.key(KeyEvent::from(KeyCode::Char('l'))));
    assert!(matches!(&effects[..], [KEffect::Containers { .. }]), "{effects:?}");
}

fn key_s() -> KeyEvent {
    KeyEvent::from(KeyCode::Char('s'))
}

#[test]
fn shell_port_forward_and_the_forwards_view() {
    let mut app = app();
    app.key(key('j'));
    let effects = kube_effects(app.key(key('s')));
    let [KEffect::Containers { obj, purpose: Purpose::Shell }] = &effects[..] else { panic!("{effects:?}") };
    // One running container: straight in.
    app.apply(Msg::Kube(KMsg::Containers { obj: obj.clone(), list: vec![ContainerInfo { name: "app".into(), kind: String::new(), running: true }, ContainerInfo { name: "init".into(), kind: "init".into(), running: false }], purpose: Purpose::Shell }));
    let pending = app.kube.take_effects();
    assert!(pending.contains(&KEffect::Shell { ns: "default".into(), pod: "web-7d9-def".into(), container: Some("app".into()) }), "{pending:?}");
    // shift-f: the ports, then local:remote.
    let effects = kube_effects(app.key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT)));
    let [KEffect::PortForwardPrepare(obj)] = &effects[..] else { panic!("{effects:?}") };
    app.apply(Msg::Kube(KMsg::Ports { obj: obj.clone(), ports: vec![(80, "http".into())] }));
    let s = render(&app, 80, 24);
    assert!(s.contains("8080:80") && s.contains("80 (http)"), "{s}");
    let effects = kube_effects(app.key(code(KeyCode::Enter)));
    assert!(matches!(&effects[..], [KEffect::PortForward { local: 8080, remote, .. }] if remote == "80"), "{effects:?}");
    app.apply(Msg::Kube(KMsg::Forwards(vec![FwdInfo { local: 8080, target: "po/web-7d9-def".into(), ns: "default".into(), pod: "web-7d9-def".into(), remote: 80, connections: 3, error: None, started: NOW - 120 }])));
    assert!(render(&app, 80, 24).contains("⇄1"));
    app.key(key('f'));
    let s = render(&app, 80, 24);
    assert!(s.contains("127.0.0.1:8080") && s.contains("default/po/web-7d9-def"), "{s}");
    assert_eq!(kube_effects(app.key(key('d'))), vec![KEffect::StopForward(8080)]);
}

#[test]
fn kubectl_hotkeys_plugins_and_help() {
    let mut app = app();
    app.key(key('!'));
    typing(&mut app, "get events -A");
    let effects = kube_effects(app.key(code(KeyCode::Enter)));
    let Some(KEffect::Kubectl { id, args }) = effects.first() else { panic!("{effects:?}") };
    assert_eq!(args, "get events -A");
    app.apply(Msg::Kube(KMsg::Pager { id: *id, lines: vec!["NAMESPACE   LAST SEEN".into()], append: true, done: false, error: None }));
    assert!(render(&app, 80, 24).contains("kubectl get events -A …"));
    app.key(code(KeyCode::Esc));
    assert!(app.kube.take_effects().iter().any(|e| matches!(e, KEffect::StopKubectl(_))));
    // ui.yaml: a hotkey runs a command line; a plugin gets the row's names.
    let cfg: super::uiconfig::UiConfig = serde_yaml::from_str("hotkeys: {shift-d: ':deploy -A'}\nplugins:\n- {name: dive, key: ctrl-i, scopes: [pods], command: dive, args: ['$NAMESPACE/$NAME', '$COL-STATUS']}\n").unwrap();
    app.apply(Msg::Kube(KMsg::Config(Ok(cfg))));
    let effects = kube_effects(app.key(ctrl('i')));
    let [KEffect::Plugin { plugin, vars }] = &effects[..] else { panic!("{effects:?}") };
    assert_eq!(plugin.name, "dive");
    assert!(vars.contains(&("NAME".to_string(), "web-7d9-abc".to_string())) && vars.contains(&("COL-STATUS".to_string(), "Running".to_string())), "{vars:?}");
    app.key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
    assert_eq!(top_table(&app).res.plural, "deployments");
    // ? : the keys of what is shown.
    app.key(key('?'));
    let s = render(&app, 80, 24);
    assert!(s.contains("help") && s.contains("scale") && s.contains("rollout restart"), "{s}");
}

#[test]
fn every_view_fits_80x24() {
    let mut app = app();
    let mut screens = vec![render(&app, 80, 24)];
    app.key(key(':'));
    typing(&mut app, "no");
    screens.push(render(&app, 80, 24));
    app.key(code(KeyCode::Esc));
    app.key(key('n'));
    screens.push(render(&app, 80, 24));
    app.key(code(KeyCode::Esc));
    app.key(key('m'));
    screens.push(render(&app, 80, 24));
    app.key(code(KeyCode::Esc));
    app.key(key('?'));
    screens.push(render(&app, 80, 24));
    for s in screens {
        assert_eq!(s.lines().count(), 24);
        for l in s.lines() {
            assert!(l.chars().count() == 80, "{l:?}");
        }
        assert!(s.lines().last().unwrap().contains('?') || s.contains("help"), "{s}");
    }
}

#[test]
fn errors_and_loading_say_so() {
    let mut app = app();
    app.key(key(':'));
    typing(&mut app, "svc");
    app.key(code(KeyCode::Enter));
    assert!(render(&app, 80, 24).contains("loading…"));
    let wk = top_table(&app).key();
    app.apply(Msg::Kube(KMsg::Data(wk, Arc::new(super::table::Snapshot { error: Some("services is forbidden: User cannot list".into()), ..Default::default() }))));
    assert!(render(&app, 80, 24).contains("services is forbidden"));
}

/// The cost of a keystroke in the filter with 5 000 pods: filter, sort and
/// size every row (the number docs/ui.md reports comes from a release build).
#[test]
fn filtering_thousands_of_rows_is_fast() {
    let mut app = app();
    let rows: Vec<Row> = (0..5000).map(|i| pod(&format!("ns-{}", i % 50), &format!("app-{i}-{:x}", i * 7919), "1/1", if i % 97 == 0 { "Error" } else { "Running" }, "0", &format!("app{}", i % 20))).collect();
    app.apply(Msg::Kube(KMsg::Data(pods_key(&app.kube, None), test_snapshot(POD_COLS, rows))));
    assert_eq!(top_table(&app).total, 5000);
    app.key(key('/'));
    let t = std::time::Instant::now();
    typing(&mut app, "app-4");
    let per_key = t.elapsed() / 5;
    assert_eq!(top_table(&app).rows.len(), 1111);
    let t = std::time::Instant::now();
    let s = render(&app, 120, 40);
    let frame = t.elapsed();
    assert!(s.contains("app-4"));
    eprintln!("5000 rows: {per_key:?} per filter keystroke, {frame:?} per frame");
    assert!(per_key < std::time::Duration::from_millis(if cfg!(debug_assertions) { 400 } else { 40 }), "{per_key:?}");
}

fn obj_row(ns: &str, name: &str, uid: &str, owner: Option<&str>, cells: &[&str], health: Health, labels: serde_json::Value) -> Row {
    let cells: Vec<String> = cells.iter().map(|s| s.to_string()).collect();
    Row {
        ns: ns.into(),
        name: name.into(),
        uid: uid.into(),
        haystack: format!("{name} {}", cells.join(" ")).to_lowercase(),
        cells,
        labels: json!({"metadata": {"labels": labels}}),
        owners: owner.map(|o| vec![super::table::Owner { kind: "X".into(), name: "x".into(), uid: o.into(), controller: true }]).unwrap_or_default(),
        health,
        ..Default::default()
    }
}

fn wk(res: &str, ns: Option<&str>) -> WatchKey {
    WatchKey { res: res.into(), ns: ns.map(str::to_string), fields: String::new() }
}

/// A namespace `shop`: deploy api → rs → 2 pods (one crashing), a bare
/// pod, a service picking the api pods, one node.
fn tree_data(app: &mut App, ns: Option<&str>) {
    let pod_cols: &[(&str, i64)] = &[("Name", 0), ("Ready", 0), ("Status", 0), ("Restarts", 0), ("Age", 0), ("IP", 1), ("Node", 1)];
    let pods = vec![
        obj_row("shop", "api-abc-1", "p1", Some("r1"), &["api-abc-1", "1/1", "Running", "0", "", "", "n1"], Health::Ok, json!({"app": "api"})),
        obj_row("shop", "api-abc-2", "p2", Some("r1"), &["api-abc-2", "0/1", "CrashLoopBackOff", "3 (1m ago)", "", "", "n1"], Health::Bad, json!({"app": "api"})),
        obj_row("shop", "debug", "p3", None, &["debug", "0/1", "Pending", "0", "", "", "<none>"], Health::Warn, json!({})),
    ];
    let msgs = vec![
        (wk("v1/nodes", None), test_snapshot(&[("Name", 0), ("Status", 0), ("Roles", 0), ("Age", 0), ("Version", 0)], vec![obj_row("", "n1", "n", None, &["n1", "Ready", "control-plane", "", "v1.33"], Health::Ok, json!({}))])),
        (wk("v1/pods", ns), test_snapshot(pod_cols, pods)),
        (wk("apps/v1/deployments", ns), test_snapshot(&[("Name", 0), ("Ready", 0), ("Up-to-date", 0), ("Available", 0), ("Age", 0)], vec![obj_row("shop", "api", "d1", None, &["api", "1/2", "2", "1", ""], Health::Warn, json!({}))])),
        (wk("apps/v1/replicasets", ns), test_snapshot(&[("Name", 0), ("Desired", 0), ("Current", 0), ("Ready", 0), ("Age", 0)], vec![obj_row("shop", "api-abc", "r1", Some("d1"), &["api-abc", "2", "2", "1", ""], Health::Warn, json!({})), obj_row("shop", "api-old", "r0", Some("d1"), &["api-old", "0", "0", "0", ""], Health::Ok, json!({}))])),
        (wk("v1/services", ns), test_snapshot(&[("Name", 0), ("Type", 0), ("Cluster-IP", 0), ("External-IP", 0), ("Port(s)", 0), ("Age", 0), ("Selector", 1)], vec![obj_row("shop", "api", "s1", None, &["api", "ClusterIP", "10.43.0.9", "<none>", "80/TCP", "", "app=api"], Health::Ok, json!({}))])),
    ];
    for (k, s) in msgs {
        app.apply(Msg::Kube(KMsg::Data(k, s)));
    }
}

#[test]
fn tree_shows_owners_services_and_nodes_live() {
    let mut app = app();
    app.key(key(':'));
    typing(&mut app, "tree -A");
    app.key(code(KeyCode::Enter));
    // It asks for the nine kinds' informers (no fetching of its own).
    let pending = app.kube.take_effects();
    let Some(KEffect::Watch(w)) = pending.iter().rev().find(|e| matches!(e, KEffect::Watch(_))) else { panic!("{pending:?}") };
    for res in ["v1/pods", "apps/v1/replicasets", "apps/v1/deployments", "batch/v1/cronjobs", "v1/services", "v1/nodes"] {
        assert!(w.iter().any(|(_, k)| k.res == res && k.ns.is_none()), "{res}");
    }
    tree_data(&mut app, None);
    let s = render(&app, 80, 24);
    for want in [
        "tree all namespaces",
        "├─ ● nodes",
        "│  └─ ● Node n1  Ready, 2 pods",
        "└─ ✕ namespace shop",
        "   ├─ ◐ Deployment api  1/2 ready",
        "   │  └─ ◐ ReplicaSet api-abc  1/2 ready",
        "   │     ├─ ● Pod api-abc-1  Running on n1",
        "   │     └─ ✕ Pod api-abc-2  CrashLoopBackOff on n1, 3 restarts",
        "   ├─ ◐ Pod debug  Pending",
        "   └─ ◐ Service api  ClusterIP 10.43.0.9 80 → 1/2 pods",
    ] {
        assert!(s.contains(want), "missing {want:?} in\n{s}");
    }
    assert!(!s.contains("api-old"), "{s}");
    for l in s.lines() {
        assert_eq!(l.chars().count(), 80);
    }
    // H: old ReplicaSets too.
    app.key(key('H'));
    assert!(render(&app, 80, 24).contains("ReplicaSet api-old  0/0 ready"));
    app.key(key('H'));
    // Live: a pod recovers, the tree follows.
    let pods = vec![
        obj_row("shop", "api-abc-1", "p1", Some("r1"), &["api-abc-1", "1/1", "Running", "0", "", "", "n1"], Health::Ok, json!({"app": "api"})),
        obj_row("shop", "api-abc-2", "p2", Some("r1"), &["api-abc-2", "1/1", "Running", "3", "", "", "n1"], Health::Ok, json!({"app": "api"})),
    ];
    app.apply(Msg::Kube(KMsg::Data(wk("v1/pods", None), test_snapshot(&[("Name", 0), ("Ready", 0), ("Status", 0), ("Restarts", 0), ("Age", 0), ("IP", 1), ("Node", 1)], pods))));
    let s = render(&app, 80, 24);
    assert!(s.contains("Pod api-abc-2  Running on n1, 3 restarts") && s.contains("→ 2/2 pods") && !s.contains("debug"), "{s}");
}

#[test]
fn tree_folds_filters_and_acts_on_objects() {
    let mut app = app();
    app.key(key(':'));
    typing(&mut app, "tree shop");
    app.key(code(KeyCode::Enter));
    tree_data(&mut app, Some("shop"));
    let s = render(&app, 80, 24);
    assert!(s.contains("tree namespace shop") && s.contains("Node n1  Ready, 2 pods in shop"), "{s}");
    // Down to the deployment; space folds what is under it.
    for _ in 0..3 {
        app.key(key('j'));
    }
    app.key(key(' '));
    let s = render(&app, 80, 24);
    assert!(s.contains("Deployment api  1/2 ready  ▸ 3 more") && !s.contains("api-abc-1"), "{s}");
    app.key(code(KeyCode::Right));
    assert!(render(&app, 80, 24).contains("api-abc-1"));
    // ← on a leaf goes to its parent.
    app.key(key('j'));
    app.key(key('j'));
    app.key(code(KeyCode::Left));
    let View::Tree(tv) = app.kube.top() else { panic!() };
    assert_eq!(tv.selected().unwrap().name, "api-abc");
    // enter: describe; l on a pod: its containers first.
    app.key(key('j'));
    let effects = kube_effects(app.key(code(KeyCode::Enter)));
    assert!(matches!(&effects[..], [KEffect::Describe { obj, .. }] if obj.name == "api-abc-1" && obj.res.plural == "pods"), "{effects:?}");
    app.key(code(KeyCode::Esc));
    let effects = kube_effects(app.key(key('l')));
    assert!(matches!(&effects[..], [KEffect::Containers { obj, .. }] if obj.name == "api-abc-1"), "{effects:?}");
    // /: matching lines and what they are under.
    app.key(key('/'));
    typing(&mut app, "crash");
    app.key(code(KeyCode::Enter));
    let s = render(&app, 80, 24);
    assert!(s.contains("Pod api-abc-2") && s.contains("ReplicaSet api-abc") && s.contains("namespace shop") && !s.contains("api-abc-1") && !s.contains("Service api"), "{s}");
    app.key(code(KeyCode::Esc));
    assert!(render(&app, 80, 24).contains("Service api"));
    // 0: all namespaces (a new set of informers).
    app.key(key('0'));
    let View::Tree(tv) = app.kube.top() else { panic!() };
    assert_eq!(tv.ns, None);
    // T from a table opens the tree of its namespace.
    app.key(code(KeyCode::Esc));
    app.key(key(':'));
    typing(&mut app, "deploy -n shop");
    app.key(code(KeyCode::Enter));
    app.key(key('T'));
    let View::Tree(tv) = app.kube.top() else { panic!() };
    assert_eq!(tv.ns.as_deref(), Some("shop"));
}

/// A tree of 5 000 pods is rebuilt on every change: it stays cheap.
#[test]
fn tree_of_thousands_of_pods_rebuilds_fast() {
    let mut app = app();
    app.key(key(':'));
    typing(&mut app, "tree -A");
    app.key(code(KeyCode::Enter));
    let cols: &[(&str, i64)] = &[("Name", 0), ("Ready", 0), ("Status", 0), ("Restarts", 0), ("Age", 0), ("IP", 1), ("Node", 1)];
    let rs: Vec<Row> = (0..50).map(|i| obj_row(&format!("ns-{}", i % 10), &format!("app{i}-rs"), &format!("r{i}"), None, &[&format!("app{i}-rs"), "100", "100", "100", ""], Health::Ok, json!({}))).collect();
    let pods: Vec<Row> = (0..5000).map(|i| obj_row(&format!("ns-{}", i % 50 % 10), &format!("app{}-{i}", i % 50), &format!("p{i}"), Some(&format!("r{}", i % 50)), &[&format!("app{}-{i}", i % 50), "1/1", "Running", "0", "", "", "n1"], Health::Ok, json!({}))).collect();
    app.apply(Msg::Kube(KMsg::Data(wk("apps/v1/replicasets", None), test_snapshot(&[("Name", 0), ("Desired", 0), ("Current", 0), ("Ready", 0), ("Age", 0)], rs))));
    let snap = test_snapshot(cols, pods);
    let t = std::time::Instant::now();
    app.apply(Msg::Kube(KMsg::Data(wk("v1/pods", None), snap)));
    let rebuild = t.elapsed();
    let View::Tree(tv) = app.kube.top() else { panic!() };
    assert_eq!(tv.rows.iter().filter(|r| r.kind == Some(crate::tree::Kind::Pod)).count(), 5000);
    let t = std::time::Instant::now();
    render(&app, 120, 40);
    eprintln!("tree of 5000 pods: rebuild {rebuild:?}, frame {:?}", t.elapsed());
    assert!(rebuild < std::time::Duration::from_millis(if cfg!(debug_assertions) { 1500 } else { 100 }), "{rebuild:?}");
}


/// An app on another cluster's context (not litek8s).
fn foreign(writable: &[&str]) -> App {
    let mut a = app();
    a.kube.context = "work-prod".into();
    a.apply(Msg::Kube(KMsg::Identified { context: "work-prod".into(), litek8s: false }));
    if !writable.is_empty() {
        let cfg: super::uiconfig::UiConfig = serde_yaml::from_str(&format!("writableContexts: [{}]", writable.join(", "))).unwrap();
        a.apply(Msg::Kube(KMsg::Config(Ok(cfg))));
    }
    a
}

#[test]
fn other_clusters_are_read_only_unless_listed() {
    let mut app = foreign(&[]);
    let s = render(&app, 80, 24);
    assert!(s.contains("read-only"), "{s}");
    // Delete: no dialog, a refusal saying how to allow it.
    assert!(app.key(ctrl('d')).is_empty());
    assert_eq!(app.kube.overlay, Overlay::None);
    let (msg, err) = app.status.clone().unwrap();
    assert!(err && msg.contains("work-prod is not a litek8s cluster") && msg.contains("writableContexts"), "{msg}");
    // Every other change too, also through paths without a key check.
    for k in [key('e'), key('s')] {
        assert!(kube_effects(app.key(k)).iter().all(|e| !e.mutates()));
    }
    let pod = ObjRef { res: app.kube.catalog.find("pods").unwrap().clone(), ns: "default".into(), name: "web-7d9-abc".into() };
    app.apply(Msg::Kube(KMsg::Containers { obj: pod.clone(), list: vec![ContainerInfo { name: "app".into(), kind: String::new(), running: true }], purpose: Purpose::Shell }));
    assert!(app.kube.take_effects().iter().all(|e| !matches!(e, KEffect::Shell { .. })));
    app.apply(Msg::Kube(KMsg::Diff { obj: pod.clone(), lines: vec!["+x".into()], body: "x: 1\n".into() }));
    assert!(kube_effects(app.key(key('y'))).is_empty());
    // kubectl: reading verbs run, changing ones do not.
    app.key(key('!'));
    typing(&mut app, "delete pod web-7d9-abc");
    assert!(kube_effects(app.key(code(KeyCode::Enter))).iter().all(|e| !matches!(e, KEffect::Kubectl { .. })));
    app.key(key('!'));
    typing(&mut app, "get pods -o wide");
    assert!(kube_effects(app.key(code(KeyCode::Enter))).iter().any(|e| matches!(e, KEffect::Kubectl { .. })));
    // Plugins run only when they say they only read.
    app.key(code(KeyCode::Esc));
    let cfg: super::uiconfig::UiConfig = serde_yaml::from_str("plugins:\n- {name: rm, key: ctrl-x, scopes: [pods], command: kubectl, args: [delete]}\n- {name: dive, key: ctrl-i, scopes: [pods], command: dive, mutates: false}\n").unwrap();
    app.apply(Msg::Kube(KMsg::Config(Ok(cfg))));
    assert!(kube_effects(app.key(ctrl('x'))).is_empty());
    assert!(matches!(&kube_effects(app.key(ctrl('i')))[..], [KEffect::Plugin { .. }]));
    // Reads, logs and port-forwards still work.
    app.key(code(KeyCode::Esc));
    assert!(matches!(&kube_effects(app.key(key('d')))[..], [KEffect::Describe { .. }]));
    app.key(code(KeyCode::Esc));
    assert!(matches!(&kube_effects(app.key(key('l')))[..], [KEffect::Containers { purpose: Purpose::Logs { .. }, .. }]));
    assert!(matches!(&kube_effects(app.key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT)))[..], [KEffect::PortForwardPrepare(_)]));
}

#[test]
fn writable_contexts_and_read_only_everywhere() {
    // Listed in writableContexts: changes are allowed.
    let mut app = foreign(&["work-prod"]);
    assert!(!render(&app, 80, 24).contains("read-only"));
    app.key(ctrl('d'));
    assert!(matches!(app.kube.overlay, Overlay::Confirm(_)));
    let effects = kube_effects(app.key(key('y')));
    assert!(matches!(&effects[..], [KEffect::Delete { .. }]), "{effects:?}");
    // A litek8s cluster needs no listing; readOnly: true wins over both.
    let mut app = self::app();
    app.key(ctrl('d'));
    assert!(matches!(app.kube.overlay, Overlay::Confirm(_)));
    app.key(code(KeyCode::Esc));
    let cfg: super::uiconfig::UiConfig = serde_yaml::from_str("readOnly: true\nwritableContexts: [prod]").unwrap();
    app.apply(Msg::Kube(KMsg::Config(Ok(cfg))));
    app.key(ctrl('d'));
    assert_eq!(app.kube.overlay, Overlay::None);
    assert!(app.status.as_ref().is_some_and(|(m, _)| m.contains("readOnly: true")));
    // Before it is known whether a new context is litek8s: read-only.
    let mut app = self::app();
    app.kube.reset(None, "new-one".into());
    assert!(app.kube.take_effects().contains(&KEffect::Identify("new-one".into())));
    assert_eq!(app.kube.access(), Access::Checking);
    app.apply(Msg::Kube(KMsg::Identified { context: "new-one".into(), litek8s: true }));
    assert_eq!(app.kube.access(), Access::Writable);
}

#[test]
fn kubectl_verbs_that_change_things() {
    for a in ["delete pod x", "-n web apply -f x.yaml", "rollout restart deploy/web", "exec -it web -- sh", "edit deploy web", "scale deploy web --replicas 2", "label pod x a=b", "drain n1", "auth reconcile -f x"] {
        assert!(kubectl_mutates(a), "{a}");
    }
    for a in ["get pods -A", "--context x -n web get events", "describe pod x", "logs -f web", "top nodes", "rollout status deploy/web", "auth can-i list pods", "config view", "port-forward svc/web 8080:80", ""] {
        assert!(!kubectl_mutates(a), "{a}");
    }
}

#[test]
fn t_opens_the_tree_on_the_selected_object_and_back() {
    let mut app = app();
    // Shown in the footer and in an empty list's tip.
    let s = render(&app, 80, 24);
    assert!(s.lines().last().unwrap().contains("t") && s.lines().last().unwrap().contains("tree"), "{s}");
    app.key(key(':'));
    typing(&mut app, "deploy -n shop");
    app.key(code(KeyCode::Enter));
    app.apply(Msg::Kube(KMsg::Data(wk("apps/v1/deployments", Some("shop")), test_snapshot(&[("Name", 0), ("Ready", 0)], Vec::new()))));
    assert!(render(&app, 80, 24).contains("t: the tree of the namespace"));
    let rows = vec![obj_row("shop", "api", "d1", None, &["api", "1/2", "2", "1", ""], Health::Warn, json!({}))];
    app.apply(Msg::Kube(KMsg::Data(wk("apps/v1/deployments", Some("shop")), test_snapshot(&[("Name", 0), ("Ready", 0), ("Up-to-date", 0), ("Available", 0), ("Age", 0)], rows))));
    app.key(key('t'));
    tree_data(&mut app, Some("shop"));
    let View::Tree(tv) = app.kube.top() else { panic!() };
    assert_eq!(tv.ns.as_deref(), Some("shop"));
    assert_eq!(tv.selected().unwrap().key(), "Deployment/shop/api");
    let s = render(&app, 80, 24);
    assert!(s.contains("Deployment api  1/2 ready") && s.contains("t/esc") && s.contains("back"), "{s}");
    // t again: back to the list.
    app.key(key('t'));
    assert_eq!(app.kube.top_table().unwrap().res.plural, "deployments");
    // On a CronJob t is the tree too; T runs it (asks first).
    app.key(key(':'));
    typing(&mut app, "cj -n shop");
    app.key(code(KeyCode::Enter));
    let rows = vec![obj_row("shop", "tick", "c1", None, &["tick", "*/5 * * * *", "False", "0", "", ""], Health::Ok, json!({}))];
    app.apply(Msg::Kube(KMsg::Data(wk("batch/v1/cronjobs", Some("shop")), test_snapshot(&[("Name", 0), ("Schedule", 0), ("Suspend", 0), ("Active", 0), ("Last Schedule", 0), ("Age", 0)], rows))));
    app.key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT));
    assert!(matches!(&app.kube.overlay, Overlay::Confirm(c) if c.title.contains("Trigger")), "{:?}", app.kube.overlay);
    app.key(code(KeyCode::Esc));
    app.key(key('t'));
    assert!(matches!(app.kube.top(), View::Tree(_)));
}

#[test]
fn services_without_a_selector_column_come_as_objects() {
    let mut app = app();
    app.key(key(':'));
    typing(&mut app, "tree shop");
    app.key(code(KeyCode::Enter));
    tree_data(&mut app, Some("shop"));
    app.kube.take_effects();
    // An older litek8s: its Service Table has no Selector column.
    let svc = vec![obj_row("shop", "api", "s1", None, &["api", "ClusterIP", "10.43.0.9", "<none>", "80/TCP", ""], Health::Ok, json!({}))];
    app.apply(Msg::Kube(KMsg::Data(wk("v1/services", Some("shop")), test_snapshot(&[("Name", 0), ("Type", 0), ("Cluster-IP", 0), ("External-IP", 0), ("Port(s)", 0), ("Age", 0)], svc))));
    let s = render(&app, 80, 24);
    assert!(s.contains("Service api  ClusterIP 10.43.0.9 80") && !s.contains("→"), "{s}");
    // The tree asks for the services as objects …
    let pending = app.kube.take_effects();
    let Some(KEffect::Watch(w)) = pending.iter().rev().find(|e| matches!(e, KEffect::Watch(_))) else { panic!("{pending:?}") };
    assert!(w.iter().any(|(_, k)| k.res == "v1/services#objects"), "{w:?}");
    // … and from their spec gets the selector, as `litek8s tree` does.
    let doc = json!({"kind": "ServiceList", "items": [{"metadata": {"name": "api", "namespace": "shop", "uid": "s1"}, "spec": {"type": "ClusterIP", "clusterIP": "10.43.0.9", "ports": [{"port": 80, "protocol": "TCP"}], "selector": {"app": "api"}}}]});
    let list: super::table::ListDoc = serde_json::from_value(doc).unwrap();
    let cols = super::table::plain_columns_for("Service");
    let rows = list.into_rows(&cols, "Service");
    let snap = std::sync::Arc::new(super::table::Snapshot { columns: std::sync::Arc::new(cols), rows: rows.into_iter().map(std::sync::Arc::new).collect(), synced: true, error: None });
    app.apply(Msg::Kube(KMsg::Data(wk("v1/services#objects", Some("shop")), snap)));
    assert!(render(&app, 80, 24).contains("Service api  ClusterIP 10.43.0.9 80 → 1/2 pods"));
}

#[test]
fn w_turns_on_write_mode_after_typing_the_context_name() {
    let mut app = foreign(&[]);
    assert!(render(&app, 80, 24).contains(" read-only "));
    // W asks for the name; a wrong one is refused.
    assert!(app.key(key('W')).is_empty());
    let s = render(&app, 80, 24);
    assert!(s.contains("Write to work-prod?") && s.contains("Type work-prod to turn it on") && s.contains("remember"), "{s}");
    for l in s.lines() {
        assert_eq!(l.chars().count(), 80);
    }
    typing(&mut app, "work-pro");
    app.key(code(KeyCode::Enter));
    assert!(render(&app, 80, 24).contains("type work-prod exactly"));
    typing(&mut app, "d");
    assert!(kube_effects(app.key(code(KeyCode::Enter))).is_empty());
    assert_eq!(app.kube.access(), Access::WriteMode);
    let s = render(&app, 80, 24);
    assert!(s.contains("WRITE · work-prod") && !s.contains(" read-only "), "{s}");
    // Changes go through now.
    app.key(ctrl('d'));
    assert!(matches!(app.kube.overlay, Overlay::Confirm(_)));
    assert!(matches!(&kube_effects(app.key(key('y')))[..], [KEffect::Delete { .. }]));
    // W again: read-only, no question.
    app.key(key('W'));
    assert_eq!(app.kube.overlay, Overlay::None);
    assert_eq!(app.kube.access(), Access::NotLitek8s);
    assert!(render(&app, 80, 24).contains(" read-only "));
    app.key(ctrl('d'));
    assert_eq!(app.kube.overlay, Overlay::None);
    assert!(app.status.as_ref().is_some_and(|(m, _)| m.contains("W turns on writing")));
}

#[test]
fn w_remembers_in_ui_yaml_and_explains_where_it_cannot() {
    let mut app = foreign(&[]);
    app.key(key('W'));
    app.key(code(KeyCode::Tab));
    assert!(render(&app, 80, 24).contains("[x] remember"));
    typing(&mut app, "work-prod");
    assert_eq!(kube_effects(app.key(code(KeyCode::Enter))), vec![KEffect::RememberWritable { context: "work-prod".into(), on: true }]);
    assert_eq!(app.kube.config.writable_contexts, vec!["work-prod"]);
    // Off takes it out of the file too.
    assert_eq!(kube_effects(app.key(key('W'))), vec![KEffect::RememberWritable { context: "work-prod".into(), on: false }]);
    assert!(app.kube.config.writable_contexts.is_empty());
    // readOnly: true wins, and says so.
    let cfg: super::uiconfig::UiConfig = serde_yaml::from_str("readOnly: true").unwrap();
    app.apply(Msg::Kube(KMsg::Config(Ok(cfg))));
    app.key(key('W'));
    assert_eq!(app.kube.overlay, Overlay::None);
    assert!(app.status.as_ref().is_some_and(|(m, _)| m.contains("readOnly: true") && m.contains("remove readOnly")), "{:?}", app.status);
    // A litek8s cluster is always writable.
    let mut app = self::app();
    app.key(key('W'));
    assert_eq!(app.kube.overlay, Overlay::None);
    assert!(app.status.as_ref().is_some_and(|(m, _)| m.contains("is a litek8s cluster")));
}

fn argo_app() -> App {
    let mut app = app();
    let mut list = super::resources::builtins();
    list.push(super::resources::Resource { group: "argoproj.io".into(), version: "v1alpha1".into(), plural: "applications".into(), singular: "application".into(), kind: "Application".into(), namespaced: true, short: vec!["app".into(), "apps".into()], ..Default::default() });
    app.apply(Msg::Kube(KMsg::Discovery(Ok(list))));
    app.key(key(':'));
    typing(&mut app, "apps -n argocd");
    app.key(code(KeyCode::Enter));
    let rows = vec![obj_row("argocd", "web", "a1", None, &["web", "OutOfSync", "Degraded", ""], Health::Bad, json!({}))];
    app.apply(Msg::Kube(KMsg::Data(wk("argoproj.io/v1alpha1/applications", Some("argocd")), test_snapshot(&[("Name", 0), ("Sync Status", 0), ("Health Status", 0), ("Age", 0)], rows))));
    app
}

#[test]
fn argo_applications_sync_refresh_terminate_and_resources() {
    let mut app = argo_app();
    let s = render(&app, 80, 24);
    assert!(s.contains("applications.argoproj.io(argocd) [1]") && s.contains("OutOfSync") && s.contains("Degraded"), "{s}");
    // s and p: a sync (and prune), asked first.
    app.key(key('s'));
    assert!(matches!(&app.kube.overlay, Overlay::Confirm(c) if c.title == "Sync app/argocd/web?"), "{:?}", app.kube.overlay);
    let effects = kube_effects(app.key(key('y')));
    assert!(matches!(&effects[..], [KEffect::AppSync { objs, prune: false, .. }] if objs.len() == 1 && objs[0].name == "web"), "{effects:?}");
    app.key(key('p'));
    let effects = kube_effects(app.key(key('y')));
    assert!(matches!(&effects[..], [KEffect::AppSync { prune: true, .. }]), "{effects:?}");
    // r and R: refreshes, at once.
    let effects = kube_effects(app.key(key('r')));
    assert!(matches!(&effects[..], [KEffect::AppRefresh { hard: false, .. }]), "{effects:?}");
    let effects = kube_effects(app.key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT)));
    assert!(matches!(&effects[..], [KEffect::AppRefresh { hard: true, .. }]), "{effects:?}");
    // T terminates the sync here, not the tree.
    app.key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT));
    assert!(matches!(&app.kube.overlay, Overlay::Confirm(c) if c.title.contains("Terminate")), "{:?}", app.kube.overlay);
    let effects = kube_effects(app.key(key('y')));
    assert!(matches!(&effects[..], [KEffect::AppTerminate(o)] if o.name == "web"), "{effects:?}");
    assert!(effects.iter().all(KEffect::mutates));
    // D: its diff with git, in a pager, also where nothing may change.
    let effects = kube_effects(app.key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT)));
    let [KEffect::AppDiff { id, obj }] = &effects[..] else { panic!("{effects:?}") };
    assert!(obj.name == "web" && !effects[0].mutates());
    app.apply(Msg::Kube(KMsg::Pager { id: *id, lines: vec!["=== deployment web/web: OutOfSync".into(), "-  replicas: 1".into(), "+  replicas: 3".into()], append: false, done: true, error: None }));
    let s = render(&app, 80, 24);
    assert!(s.contains("diff app/argocd/web") && s.contains("+  replicas: 3"), "{s}");
    app.key(code(KeyCode::Esc));
    // The m menu lists them.
    let labels: Vec<&str> = app.kube.actions().iter().map(|a| a.label).collect();
    assert!(labels.contains(&"sync and prune") && labels.contains(&"hard refresh"), "{labels:?}");
    // enter: its resources, from its status; s on its row syncs from there.
    let effects = kube_effects(app.key(code(KeyCode::Enter)));
    let [KEffect::Xray { id, res, .. }] = &effects[..] else { panic!("{effects:?}") };
    let status = json!({"metadata": {"namespace": "argocd", "name": "web"}, "status": {"sync": {"status": "OutOfSync"}, "health": {"status": "Degraded"},
        "resources": [{"group": "apps", "kind": "Deployment", "namespace": "web", "name": "web", "status": "OutOfSync", "health": {"status": "Degraded", "message": "exceeded its progress deadline"}},
                      {"kind": "Service", "namespace": "web", "name": "web", "status": "Synced", "health": {"status": "Healthy"}}]}});
    let nodes = super::gitops::tree(&[status], res, &|_, _| None, false);
    app.apply(Msg::Kube(KMsg::Xray { id: *id, nodes, error: None }));
    let s = render(&app, 80, 24);
    assert!(s.contains("✕ app web  OutOfSync · Degraded") && s.contains("deployment web/web  OutOfSync · Degraded: exceeded") && s.contains("● service web/web  Synced · Healthy"), "{s}");
    assert!(s.contains("sync") && s.contains("refresh"), "footer: {s}");
    app.key(key('s'));
    assert!(matches!(&app.kube.overlay, Overlay::Confirm(c) if c.title.starts_with("Sync")), "{:?}", app.kube.overlay);
}

#[test]
fn argo_applications_are_read_only_on_other_clusters() {
    let mut app = argo_app();
    app.apply(Msg::Kube(KMsg::Identified { context: "prod".into(), litek8s: false }));
    app.key(key('s'));
    assert!(matches!(app.kube.overlay, Overlay::None), "{:?}", app.kube.overlay);
    assert!(kube_effects(app.key(key('r'))).is_empty());
    let (msg, err) = app.status.clone().unwrap();
    assert!(err && msg.contains("not a litek8s cluster"), "{msg}");
}

#[test]
fn argo_applications_tree_reloads_when_the_apps_change() {
    let mut app = argo_app();
    let effects = kube_effects(app.key(code(KeyCode::Enter)));
    let [KEffect::Xray { id, res, .. }] = &effects[..] else { panic!("{effects:?}") };
    let (id, res) = (*id, res.clone());
    let key = wk("argoproj.io/v1alpha1/applications", Some("argocd"));
    let rows = |health: &str| vec![obj_row("argocd", "web", "a1", None, &["web", "Synced", health, ""], Health::Ok, json!({}))];
    let data = |app: &mut App, health: &str| app.apply(Msg::Kube(KMsg::Data(key.clone(), test_snapshot(&[("Name", 0), ("Sync Status", 0), ("Health Status", 0), ("Age", 0)], rows(health)))));
    let status = |health: &str| {
        json!({"metadata": {"namespace": "argocd", "name": "web"}, "status": {"sync": {"status": "Synced"}, "health": {"status": health},
            "resources": [{"group": "apps", "kind": "Deployment", "namespace": "web", "name": "web", "status": "Synced", "health": {"status": health}},
                          {"kind": "Service", "namespace": "web", "name": "web", "status": "Synced", "health": {"status": "Healthy"}}]}})
    };
    // The apps stay watched while the tree shows.
    let _ = app.kube.take_effects();
    assert!(app.kube.wanted().iter().any(|(_, k)| *k == key));
    // A change while the first load runs: one more load after it, not two at once.
    data(&mut app, "Progressing");
    assert!(app.kube.take_effects().iter().all(|e| !matches!(e, KEffect::Xray { .. })));
    app.apply(Msg::Kube(KMsg::Xray { id, nodes: super::gitops::tree(&[status("Progressing")], &res, &|_, _| None, false), error: None }));
    let again = app.kube.take_effects();
    assert!(matches!(&again[..], [KEffect::Xray { id: i, .. }] if *i == id), "{again:?}");
    // The cursor on the service and the app folded stay where they were.
    app.key(code(KeyCode::Down));
    app.key(code(KeyCode::Down));
    let Some(View::Xray(x)) = app.kube.views.last_mut() else { panic!() };
    assert_eq!(x.nodes[x.sel].kind, "service");
    x.folded.insert(0);
    app.apply(Msg::Kube(KMsg::Xray { id, nodes: super::gitops::tree(&[status("Healthy")], &res, &|_, _| None, false), error: None }));
    let Some(View::Xray(x)) = app.kube.views.last() else { panic!() };
    assert!(!x.loading && x.folded.contains(&0) && x.nodes[x.sel].kind == "service", "{:?} {:?}", x.folded, x.nodes[x.sel]);
    // Another change: loaded at once; leaving the tree stops watching.
    data(&mut app, "Healthy");
    assert!(app.kube.take_effects().iter().any(|e| matches!(e, KEffect::Xray { .. })));
    app.key(code(KeyCode::Esc));
    data(&mut app, "Degraded");
    assert!(app.kube.take_effects().iter().all(|e| !matches!(e, KEffect::Xray { .. })));
}
