//! State and rendering tests: keys against [`App`], frames drawn on
//! ratatui's TestBackend at 80x24.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::*;
use super::model::*;
use super::view;

fn key(c: char) -> KeyEvent {
    KeyEvent::from(KeyCode::Char(c))
}

fn code(c: KeyCode) -> KeyEvent {
    KeyEvent::from(c)
}

fn ctr(name: &str, project: Option<&str>, state: &str) -> Container {
    Container {
        id: format!("{name:0<64}"),
        name: name.into(),
        image: "nginx:1.27".into(),
        image_id: "sha256:aa".into(),
        state: state.into(),
        status: if state == "running" { "Up 2 minutes".into() } else { "Exited (0) 1 hour ago".into() },
        project: project.map(str::to_string),
        service: project.map(|_| name.rsplit('-').nth(1).unwrap_or(name).to_string()),
        ports: if state == "running" { "8080->80/tcp".into() } else { String::new() },
        ..Default::default()
    }
}

fn sample_app() -> App {
    let mut app = App::new("unix:///Users/me/.litek8s/docker.sock".into(), "http://127.0.0.1:6443".into(), "litek8s-dev".into());
    app.now = 1_767_225_600 + 3600;
    app.apply(Msg::Containers(vec![ctr("scratch", None, "exited"), ctr("shop-web-1", Some("shop"), "running"), ctr("shop-db-1", Some("shop"), "running"), ctr("shop-worker-1", Some("shop"), "exited")]));
    app.apply(Msg::Images(vec![
        Image { id: "sha256:aa".into(), tags: vec!["nginx:1.27".into()], size: 190_000_000, created: 1_767_225_600, ..Default::default() },
        Image { id: "sha256:bb".into(), tags: vec!["redis:7".into()], size: 40_000_000, created: 1_767_225_600, ..Default::default() },
        Image { id: "sha256:cc".into(), tags: vec![], size: 5_000_000, created: 1_767_225_600, ..Default::default() },
    ]));
    app.apply(Msg::Volumes(vec![
        Volume { name: "shop_data".into(), driver: "local".into(), size: Some(52_000_000), project: Some("shop".into()), ..Default::default() },
        Volume { name: "old".into(), driver: "local".into(), size: Some(1_000), ..Default::default() },
    ]));
    let mut df = DiskUsage { build_cache: 300_000_000, build_cache_records: 12, loaded: true, ..Default::default() };
    df.container_sizes.insert(format!("{:0<64}", "scratch"), 2_000);
    app.apply(Msg::Disk(df));
    app.apply(Msg::Pods(vec![Pod {
        namespace: "default".into(),
        name: "web-7d9-abc".into(),
        status: "Running".into(),
        ready: "1/1".into(),
        created: 1_767_225_600,
        containers: vec!["nginx".into()],
        images: vec!["redis:7".into()],
        ports: vec![80],
        owner: Some("web-7d9".into()),
        ..Default::default()
    }]));
    app.apply(Msg::DockerInfo { version: "litek8s 0.1.0".into(), cpus: Some(4), memory: Some(4_294_967_296), os: "litek8s dev aarch64".into() });
    app.apply(Msg::Home(Box::new(HomeSample {
        status: DevStatus::Up,
        mode: "vm".into(),
        uptime_s: 7260,
        api: "http://127.0.0.1:6443".into(),
        cpu_pct: Some(12.0),
        mem_used: Some(1_200_000_000),
        disk_used: Some(2_300_000_000),
        disk_free: Some(150_000_000_000),
        data_dir: "/Users/me/Library/Application Support/dev.litek8s/dev".into(),
        contexts: Contexts { docker_context: "litek8s".into(), docker_context_is_litek8s: true, kube_context: "litek8s-dev".into(), kube_is_litek8s: true, kubeconfig: "/Users/me/.kube/config".into(), ..Default::default() },
        ..Default::default()
    })));
    app
}

fn render(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| view::draw(f, app)).unwrap();
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

#[test]
fn every_view_fits_80x24() {
    let mut app = sample_app();
    for (i, tab) in TABS.iter().enumerate() {
        app.key(key(char::from(b'1' + i as u8)));
        assert_eq!(app.tab, *tab);
        let screen = render(&app, 80, 24);
        assert_eq!(screen.lines().count(), 24);
        assert!(screen.lines().all(|l| l.chars().count() == 80));
        assert!(screen.lines().next().unwrap().contains("8 Set"), "tab bar cut off:\n{screen}");
        assert!(screen.lines().last().unwrap().contains("?  help"), "{screen}");
    }
}

#[test]
fn home_shows_state_resources_and_contexts() {
    let app = sample_app();
    let s = render(&app, 80, 24);
    assert!(s.contains("● dev · macOS VM · up 2h 1m"), "{s}");
    assert!(s.contains("3% of 4 CPUs · 12% of one"), "{s}");
    assert!(s.contains("28% 1.2 GB of 4.3 GB"), "{s}");
    assert!(s.contains("150 GB free"), "{s}");
    assert!(s.contains("✓ docker context"), "{s}");
    assert!(s.contains("✓ kubectl context"), "{s}");
    assert!(s.contains("2/4 running"), "{s}");
}

#[test]
fn containers_group_by_compose_project() {
    let mut app = sample_app();
    app.key(key('3'));
    let rows = app.container_rows();
    assert_eq!(rows[0], CRow::Item(0));
    assert!(matches!(&rows[1], CRow::Group { name, count: 3, running: 2, collapsed: false } if name == "shop"));
    assert_eq!(rows.len(), 5);
    let s = render(&app, 80, 24);
    assert!(s.contains("▾ shop"), "{s}");
    assert!(s.contains("2/3 up"), "{s}");
    assert!(s.contains("8080->80/tcp"), "{s}");
    // Fold the project.
    app.key(key('j'));
    app.key(key(' '));
    assert_eq!(app.container_rows().len(), 2);
    assert!(render(&app, 80, 24).contains("▸ shop"));
}

#[test]
fn project_budgets_and_oom_kills_show() {
    let mut app = sample_app();
    app.key(key('3'));
    let t0 = std::time::Instant::now();
    let worker = format!("{:0<64}", "shop-worker-1");
    let shop = |cpu_usec: u64| ProjectUse { name: "shop".into(), memory: Some(192 << 20), memory_max: Some(256 << 20), cpu_usec, cpus: Some(2.0), oom_kills: 1, ..Default::default() };
    app.apply(Msg::Projects { at: t0, projects: vec![shop(1_000_000)], oom_killed: vec![worker.clone()] });
    // Half a CPU over the next 2 s.
    app.apply(Msg::Projects { at: t0 + std::time::Duration::from_secs(2), projects: vec![shop(2_000_000)], oom_killed: vec![worker] });
    assert_eq!(app.projects["shop"].cpu_rate, Some(0.5));
    let s = render(&app, 120, 24);
    assert!(s.contains("192Mi/256Mi"), "{s}");
    assert!(s.contains("75%"), "{s}");
    assert!(s.contains("cpu 0.5/2"), "{s}");
    assert!(s.contains("cpu 0.5/2 ✗ 1 OOM"), "{s}");
    assert!(s.contains("OOMKilled"), "{s}");
    // At 80 columns the meter and the state still fit.
    let s = render(&app, 80, 24);
    assert!(s.contains("192Mi/256Mi") && s.contains("OOMKilled") && s.contains("✗ 1 OOM"), "{s}");
    // The engine's oom event: a clear status line.
    app.apply(Msg::Oom { name: "shop-worker-1".into(), project: Some("shop".into()), scope: "project".into(), memory: "256Mi".into() });
    assert_eq!(app.status, Some(("shop-worker-1 was OOM-killed: compose project shop hit its 256Mi budget".into(), true)));
}

#[test]
fn destructive_actions_ask_first() {
    let mut app = sample_app();
    app.key(key('3'));
    // On the project row: stopping asks, and covers all three.
    app.key(key('j'));
    assert!(app.key(key('x')).is_empty());
    match &app.overlay {
        Overlay::Confirm { title, effect: Effect::Containers(ContainerOp::Stop, ids), .. } => {
            assert_eq!(title, "Stop project shop?");
            assert_eq!(ids.len(), 3);
        }
        o => panic!("no confirmation: {o:?}"),
    }
    assert!(render(&app, 80, 24).contains(" y  yes"));
    // n cancels, y runs.
    assert!(app.key(key('n')).is_empty());
    assert_eq!(app.overlay, Overlay::None);
    app.key(key('d'));
    let effects = app.key(key('y'));
    assert!(matches!(&effects[..], [Effect::Containers(ContainerOp::Remove, ids)] if ids.len() == 3));
    // Starting needs no confirmation.
    let effects = app.key(key('u'));
    assert!(matches!(&effects[..], [Effect::Containers(ContainerOp::Start, _)]));
    // Reset on Home asks, and says what it deletes.
    app.key(key('1'));
    app.key(key('R'));
    let s = render(&app, 80, 24);
    assert!(s.contains("Delete cluster dev?") && s.contains("cannot be undone") && s.contains("litek8s-dev is removed"), "{s}");
    assert_eq!(app.key(code(KeyCode::Esc)), Vec::new());
    app.key(key('R'));
    assert_eq!(app.key(code(KeyCode::Enter)), vec![Effect::Dev(DevOp::Delete("dev".into()))]);
    // Until it finishes, Home says it is stopping, and refuses another run.
    assert!(render(&app, 80, 24).contains("◐ dev · stopping"));
    assert!(app.key(key('u')).is_empty());
    app.apply(Msg::DevFinished(Ok("litek8s dev delete --name dev: done".into())));
    assert_eq!(app.dev_busy, None);
}

#[test]
fn logs_follow_and_scroll() {
    let mut app = sample_app();
    app.key(key('3'));
    let effects = app.key(key('l'));
    assert!(matches!(&effects[..], [Effect::Logs(LogSource::Container { name, .. })] if name == "scratch"));
    let id = app.log_id().unwrap();
    app.apply(Msg::LogLines(id, (0..100).map(|i| format!("line {i}")).collect()));
    let s = render(&app, 80, 24);
    assert!(s.contains("line 99") && !s.contains("line 50"), "{s}");
    assert!(s.contains("● follow"));
    app.key(code(KeyCode::PageUp));
    let s = render(&app, 80, 24);
    assert!(!s.contains("line 99") && s.contains("line 79"), "{s}");
    // New lines while scrolled back keep the view where it is.
    app.apply(Msg::LogLines(id, vec!["line 100".into()]));
    assert!(render(&app, 80, 24).contains("line 79"));
    app.key(key('G'));
    assert!(render(&app, 80, 24).contains("line 100"));
    // Lines of an older stream are ignored.
    app.apply(Msg::LogLines(id + 7, vec!["stale".into()]));
    assert!(!render(&app, 80, 24).contains("stale"));
    // Scrolling back past the top of a short log shows its first lines.
    app.key(key('c'));
    app.apply(Msg::LogLines(id, vec!["only".into(), "two".into()]));
    app.key(code(KeyCode::PageUp));
    assert!(render(&app, 80, 24).contains("only"));
    assert_eq!(app.key(code(KeyCode::Esc)), vec![Effect::StopLogs]);
    assert!(app.logs.is_none());
}

#[test]
fn shell_only_into_running_containers() {
    let mut app = sample_app();
    app.key(key('3'));
    assert!(app.key(key('s')).is_empty());
    assert!(app.status.as_ref().is_some_and(|(m, err)| *err && m.contains("not running")));
    app.key(key('j'));
    app.key(key('j'));
    assert!(matches!(&app.key(key('s'))[..], [Effect::Exec(id)] if id.starts_with("shop-web-1")));
}

#[test]
fn free_space_counts_and_prunes_what_is_selected() {
    let mut app = sample_app();
    app.key(key('6'));
    assert_eq!(app.reclaimable(Reclaim::StoppedContainers), (2_000, 2));
    assert_eq!(app.reclaimable(Reclaim::DanglingImages), (5_000_000, 1));
    // nginx is used by containers, redis by a pod: neither is unused.
    assert_eq!(app.reclaimable(Reclaim::UnusedImages), (0, 0));
    assert_eq!(app.reclaimable(Reclaim::UnusedVolumes), (52_001_000, 2));
    let s = render(&app, 80, 24);
    assert!(s.contains("[✓] Stopped containers"), "{s}");
    assert!(s.contains("[ ] Unused volumes"), "{s}");
    assert!(s.contains("305 MB"), "selected total:\n{s}");
    app.key(key('p'));
    let Overlay::Confirm { effect: Effect::Batch(batch), lines, .. } = app.overlay.clone() else { panic!("no confirmation") };
    assert_eq!(batch, vec![Effect::PruneContainers, Effect::RemoveImages(vec!["sha256:cc".into()]), Effect::PruneBuildCache]);
    assert!(lines.iter().any(|l| l.contains("cannot be undone")));
    app.key(key('n'));
    // Selecting volumes adds them.
    app.key(key('G'));
    app.key(key(' '));
    app.key(key('p'));
    let Overlay::Confirm { effect: Effect::Batch(batch), .. } = app.overlay.clone() else { panic!() };
    assert!(batch.contains(&Effect::RemoveVolumes(vec!["shop_data".into(), "old".into()])));
}

#[test]
fn image_removal_says_who_uses_it() {
    let mut app = sample_app();
    app.key(key('4'));
    let s = render(&app, 80, 24);
    assert!(s.contains("nginx:1.27"), "{s}");
    assert!(s.contains("pod web-7d9-abc"), "{s}");
    assert!(s.contains("scratch +3"), "{s}");
    app.key(code(KeyCode::Delete));
    let Overlay::Confirm { lines, .. } = &app.overlay else { panic!() };
    assert!(lines.iter().any(|l| l.contains("Used by containers")), "{lines:?}");
}

#[test]
fn filter_narrows_lists() {
    let mut app = sample_app();
    app.key(key('3'));
    app.key(key('/'));
    for c in "work".chars() {
        app.key(key(c));
    }
    assert_eq!(app.container_rows().len(), 2);
    app.key(code(KeyCode::Enter));
    assert_eq!(app.filter(), "work");
    assert!(render(&app, 80, 24).contains("/work"));
    app.key(code(KeyCode::Esc));
    assert_eq!(app.filter(), "");
}

#[test]
fn settings_validate_and_flag_restarts() {
    let mut app = sample_app();
    app.key(key('8'));
    let s = render(&app, 80, 24);
    assert!(s.contains(if cfg!(target_os = "macos") { "VM CPUs" } else { "CPU limit" }) && s.contains("(default)"), "{s}");
    // CPUs: 0 is refused with the reason, in the prompt.
    app.key(code(KeyCode::Enter));
    app.key(key('0'));
    assert!(app.key(code(KeyCode::Enter)).is_empty());
    let Overlay::Input { error: Some(e), .. } = &app.overlay else { panic!("{:?}", app.overlay) };
    assert!(e.contains("cpus must be between 1"), "{e}");
    app.key(code(KeyCode::Backspace));
    app.key(key('1'));
    let effects = app.key(code(KeyCode::Enter));
    let [Effect::SaveConfig(c)] = &effects[..] else { panic!("{effects:?}") };
    assert_eq!(c.cpus, Some(1));
    // A saved change the running VM did not start with needs a restart.
    app.apply(Msg::Config(Ok(c.clone())));
    let mut home = app.home.clone();
    home.running = Some(crate::dev::DevState {
        pid: 1,
        port: 6443,
        mode: "vm".into(),
        started_unix_ms: 0,
        cgroup: None,
        vm_ip: None,
        helper_pid: None,
        cpus: Some(4),
        memory_mib: Some(4096),
        shares: Vec::new(),
        publish: Some("localhost".into()),
        name: Some("dev".into()),
        nodes: Some(1),
        docker_socket: None,
    });
    app.apply(Msg::Home(Box::new(home)));
    assert_eq!(app.restart_needed(), vec!["CPUs"]);
    assert!(render(&app, 80, 24).contains("Restart needed for CPUs"));
    // Published ports toggle with space.
    app.key(key('G'));
    app.key(key('k'));
    app.key(key('k'));
    app.key(key('k'));
    let effects = app.key(key(' '));
    assert!(matches!(&effects[..], [Effect::SaveConfig(c)] if c.publish == crate::dev::config::Publish::All), "{effects:?}");
    // Context rows switch to litek8s or back.
    app.key(key('j'));
    assert_eq!(app.key(code(KeyCode::Enter)), vec![Effect::DockerContext(false)]);
}

#[test]
fn help_overlay_lists_keys() {
    let mut app = sample_app();
    app.key(key('?'));
    let s = render(&app, 80, 24);
    assert!(s.contains("Keys") && s.contains(":ctx") && s.contains("describe yaml edit") && s.contains("Any key closes"), "{s}");
    // Every line fits inside the box at 80 columns: nothing wraps or is cut.
    for (section, keys) in view::HELP {
        let width = 11 + keys.iter().map(|(k, v)| k.chars().count() + v.chars().count() + 3).sum::<usize>();
        assert!(width <= 76, "{section}: {width} columns");
    }
    // Any key closes it without acting.
    assert!(app.key(key('q')).is_empty());
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.key(key('q')), vec![Effect::Quit]);
    assert_eq!(app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), vec![Effect::Quit]);
}

#[test]
fn docker_down_explains_instead_of_empty_lists() {
    let mut app = sample_app();
    app.apply(Msg::DockerDown("unix:///x.sock: cannot connect to /x.sock".into()));
    app.key(key('3'));
    let s = render(&app, 80, 24);
    assert!(s.contains("cannot connect") && s.contains("Start the cluster"), "{s}");
    app.apply(Msg::Home(Box::new(HomeSample { status: DevStatus::Down, ..Default::default() })));
    app.key(key('1'));
    assert!(render(&app, 80, 24).contains("press u to start"));
    assert_eq!(app.key(key('u')), vec![Effect::Dev(DevOp::Up { cluster: "dev".into(), switch: true })]);
    // While it starts, a second `u` does nothing.
    assert!(app.key(key('u')).is_empty());
    assert_eq!(app.home.status, DevStatus::Starting);
}

#[test]
fn wide_terminals_get_more_columns() {
    let mut app = sample_app();
    app.key(key('3'));
    let s = render(&app, 120, 30);
    assert!(s.contains("STATUS") && s.contains("Up 2 minutes"), "{s}");
    // A tall one gets the header box, its tabs on the second line.
    assert!(render(&app, 120, 30).lines().nth(1).unwrap().contains("6 Free space"));
    assert!(render(&app, 120, 29).lines().next().unwrap().contains("6 Free space"));
}

#[test]
fn mouse_selects_tabs_and_rows_also_when_scrolled() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let click = |column, row| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE };
    let mut app = sample_app();
    render(&app, 80, 24);
    // "4 Imgs" in the tab bar.
    app.mouse(click(34, 0));
    assert_eq!(app.tab, Tab::Images);
    app.key(key('3'));
    let many: Vec<Container> = (0..40).map(|i| ctr(&format!("c{i:02}"), None, "running")).collect();
    app.apply(Msg::Containers(many));
    app.key(key('G'));
    render(&app, 80, 24);
    assert!(app.list_offset.get() > 0);
    // The first row on screen is row `offset`, not 0.
    let top = app.list_top.get();
    app.mouse(click(5, top));
    assert_eq!(app.sel(), app.list_offset.get());
    // The wheel moves the selection.
    let before = app.sel();
    app.mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 5, row: 5, modifiers: KeyModifiers::NONE });
    assert_eq!(app.sel(), before + 3);
}

#[test]
fn dev_output_shows_its_last_lines_wrapped() {
    let mut app = sample_app();
    app.apply(Msg::DevOutput("$ litek8s dev up".into()));
    app.apply(Msg::DevOutput(format!("litek8s-dev ready in 0.10s (rootless, http://127.0.0.1:6443, kubectl context {})", "x".repeat(40))));
    let s = render(&app, 80, 24);
    assert!(s.contains("$ litek8s dev up") && s.contains("xxxxx"), "{s}");
    for i in 0..30 {
        app.apply(Msg::DevOutput(format!("line {i}")));
    }
    let s = render(&app, 80, 24);
    assert!(s.contains("line 29") && !s.contains("line 5 "), "{s}");
}

#[test]
fn long_errors_wrap_above_the_hints() {
    let mut app = sample_app();
    let msg = "1 of 1 image(s) not removed: conflict: unable to remove repository reference \"busybox\" (must force) - container shop-worker-1 is using its referenced image";
    app.apply(Msg::Done(Err(msg.into())));
    let s = render(&app, 80, 24);
    assert!(s.contains(" error ") && s.contains("is using its referenced image"), "{s}");
    // Short errors stay in the hint line, without a box.
    app.apply(Msg::Done(Err("nope".into())));
    assert!(!render(&app, 80, 24).contains(" error "));
}

fn clusters_sample() -> ClustersSample {
    let row = |name: &str, status, port: Option<u16>, nodes, ready, memory| ClusterRow {
        name: name.into(),
        context: format!("litek8s-{name}"),
        status,
        port,
        nodes,
        ready_nodes: ready,
        memory,
        age: Some("2h".into()),
    };
    ClustersSample {
        local: vec![
            row("dev", DevStatus::Up, Some(6443), 1, Some(1), Some(38_000_000)),
            row("ci", DevStatus::Starting, Some(50213), 3, Some(2), Some(96_000_000)),
            row("old", DevStatus::Down, None, 1, None, None),
        ],
        others: vec![("home".into(), Some("https://192.168.1.10:6443".into())), ("orbstack".into(), Some("https://127.0.0.1:26443".into()))],
        env_mode: Some("vm".into()),
        env_memory: Some(412_000_000),
        env_cpus: Some(4),
        env_memory_mib: Some(4096),
    }
}

#[test]
fn clusters_view_lists_local_clusters_and_other_contexts() {
    let mut app = sample_app();
    app.apply(Msg::Clusters(Box::new(clusters_sample())));
    app.key(key('2'));
    assert_eq!(app.tab, Tab::Clusters);
    let s = render(&app, 80, 24);
    assert!(s.contains("Local clusters") && s.contains("4 CPUs") && s.contains("412 MB used"), "{s}");
    assert!(s.contains("› dev") && s.contains("● up") && s.contains("127.0.0.1:50213") && s.contains("2/3") && s.contains("96.0 MB"), "{s}");
    assert!(s.contains("Other kubeconfig contexts") && s.contains("https://192.168.1.10:6443"), "{s}");
    assert!(s.lines().last().unwrap().contains("n  new"), "{s}");
    // Start a stopped one: no question; stop and delete ask.
    app.key(key('G'));
    app.key(key('k'));
    app.key(key('k'));
    assert_eq!(app.selected_cluster().unwrap().unwrap().name, "old");
    assert_eq!(app.key(key('u')), vec![Effect::Dev(DevOp::Up { cluster: "old".into(), switch: false })]);
    assert_eq!(DevOp::Up { cluster: "old".into(), switch: false }.args(), ["dev", "up", "--name", "old", "--no-switch"]);
    app.apply(Msg::DevFinished(Ok("done".into())));
    app.key(key('k'));
    app.key(key('D'));
    let s = render(&app, 80, 24);
    assert!(s.contains("Delete cluster ci?") && s.contains("litek8s-ci is removed") && s.contains("cannot be undone"), "{s}");
    assert_eq!(app.key(key('y')), vec![Effect::Dev(DevOp::Delete("ci".into()))]);
    assert!(render(&app, 80, 24).contains("stopping"));
    app.apply(Msg::DevFinished(Ok("done".into())));
    // enter on a local cluster shows it everywhere and offers to switch the tools.
    let effects = app.key(code(KeyCode::Enter));
    assert_eq!(effects, vec![Effect::UseCluster("ci".into())]);
    assert_eq!(app.cluster, "ci");
    assert!(render(&app, 80, 24).contains("Point kubectl and docker at ci?"));
    assert_eq!(app.key(key('y')), vec![Effect::SwitchTools("ci".into())]);
    // Another context: read-only, the Kubernetes view only.
    app.key(key('G'));
    assert!(app.key(key('d')).is_empty());
    assert_eq!(app.key(code(KeyCode::Enter)), vec![Effect::UseContext("orbstack".into())]);
}

#[test]
fn new_cluster_form_validates_and_creates() {
    let mut app = sample_app();
    app.apply(Msg::Clusters(Box::new(clusters_sample())));
    app.key(key('2'));
    app.key(key('n'));
    let s = render(&app, 80, 24);
    assert!(s.contains("New cluster") && s.contains("dev2") && s.contains("shared VM") || s.contains("shared rootless sandbox"), "{s}");
    // A name that exists, then a bad node count.
    for _ in 0..4 {
        app.key(code(KeyCode::Backspace));
    }
    for c in "ci".chars() {
        app.key(key(c));
    }
    assert!(app.key(code(KeyCode::Enter)).is_empty());
    assert!(render(&app, 80, 24).contains("ci exists already"));
    app.key(key('2'));
    app.key(code(KeyCode::Tab));
    app.key(code(KeyCode::Backspace));
    app.key(key('0'));
    assert!(app.key(code(KeyCode::Enter)).is_empty());
    assert!(render(&app, 80, 24).contains("nodes: 1 to 10"));
    app.key(code(KeyCode::Backspace));
    app.key(key('3'));
    assert_eq!(app.key(code(KeyCode::Enter)), vec![Effect::Dev(DevOp::Create { cluster: "ci2".into(), nodes: 3 })]);
    assert_eq!(DevOp::Create { cluster: "ci2".into(), nodes: 3 }.args(), ["dev", "up", "--name", "ci2", "--nodes", "3", "--no-switch"]);
    assert_eq!(app.overlay, Overlay::None);
}

#[test]
fn home_says_dash_while_the_engine_is_down() {
    let mut app = sample_app();
    app.apply(Msg::DockerDown("cannot connect".into()));
    app.apply(Msg::KubeDown("down".into()));
    app.apply(Msg::Home(Box::new(HomeSample { status: DevStatus::Down, ..Default::default() })));
    let s = render(&app, 80, 24);
    assert!(!s.contains("0/0") && !s.contains("0 (0 B)") && !s.contains("0  (0 B)"), "{s}");
    assert!(s.contains("engine not running"), "{s}");
}

#[test]
fn home_explains_the_memory_figure() {
    let mut app = sample_app();
    let mem = crate::dev::memory::parse_meminfo("MemTotal: 3934000 kB\nMemFree: 3700000 kB\nCached: 125000 kB\nShmem: 51200 kB\nSReclaimable: 10000 kB\nSUnreclaim: 35000 kB\n");
    let shown = crate::dev::memory::Usage { litek8s: 12 << 20, containers: 30 << 20, cache: 0 };
    let mut home = app.home.clone();
    home.memory = Some(crate::dev::memory::vm_breakdown(421 << 20, 4096 << 20, &mem, 0, None, "dev", shown, &[]));
    app.apply(Msg::Home(Box::new(home)));
    let s = render(&app, 80, 24);
    assert!(s.contains("Memory · 441 MB (what macOS charges the VM)"), "{s}");
    assert!(s.contains("litek8s (dev)") && s.contains("12.6 MB") && s.contains("pods & containers"), "{s}");
    assert!(s.contains("Linux kernel + base") && s.contains("fixed cost: kernel") && s.contains("reclaimable"), "{s}");
    assert!(s.contains("✓ docker context"), "the contexts still fit:\n{s}");
    app.key(key('?'));
    let s = render(&app, 80, 24);
    assert!(s.contains("fixed cost") && s.contains("Any key closes") && s.contains("memory/CPU/disk"), "{s}");
    app.key(key('x'));
    // Under a ceiling the title says so, and still fits with the hint.
    let mut small = mem;
    small.free -= 3 << 30;
    let mut home = app.home.clone();
    home.memory = Some(crate::dev::memory::vm_breakdown(421 << 20, 4096 << 20, &small, 3 << 30, Some(1 << 30), "dev", shown, &[]));
    home.cpu_sample = None;
    app.apply(Msg::Home(Box::new(home)));
    let s = render(&app, 80, 24);
    assert!(s.contains("Memory · 441 MB (what macOS charges; VM ceiling 1.0/4.0 GiB)"), "{s}");
}

#[test]
fn home_explains_cpu_and_disk_with_b() {
    use crate::dev::cpu::{Kind, Sample, Split};
    let mut app = sample_app();
    let mut pods = app.pods.clone();
    pods[0].uid = "0f1e-22".into();
    app.apply(Msg::Pods(pods));
    let t0 = std::time::Instant::now();
    let split = |l: u64, p: u64| Split { litek8s_usec: l, cgroups: vec![("pod0f1e_22".into(), p)] };
    let sample = |at, total, l, p, busy| Sample { at: Some(at), total_usec: total, shown: split(l, p), guest_busy_usec: Some(busy), cpus: 4.0, kind: Kind::Vm, ..Default::default() };
    for (at, total, l, p, busy) in [(t0, 0, 0, 0, 0), (t0 + std::time::Duration::from_secs(2), 600_000, 100_000, 400_000, 560_000)] {
        let mut home = app.home.clone();
        home.cpu_sample = Some(sample(at, total, l, p, busy));
        home.disk_node = Some((
            crate::dev::disk::NodeDisk { images: 300_000_000, container_layers: 2_000_000, volumes: 50_000_000, logs: 1_000_000, objects: 3_000_000, other: 1_000_000, fs_used: 500_000_000, ..Default::default() },
            90_000_000,
            Some(900_000_000),
        ));
        app.apply(Msg::Home(Box::new(home)));
    }
    let c = app.cpu.as_ref().unwrap();
    assert!((c.total_pct - 30.0).abs() < 1e-6, "{c:?}");
    // `b`: CPU first, then disk.
    app.key(key('b'));
    let s = render(&app, 80, 24);
    assert!(s.contains("CPU · 30.0% of one CPU = 8% of 4 CPUs") && s.contains("bar: 1 CPU"), "{s}");
    assert!(s.contains("pod default/web-7d9") && s.contains("20.0%") && s.contains("VM overhead"), "{s}");
    app.key(key('b'));
    let s = render(&app, 80, 24);
    assert!(s.contains("Disk · 990 MB (litek8s and its VM disk)"), "{s}");
    assert!(s.contains("litek8s itself") && s.contains("images") && s.contains("deleted in the VM"), "{s}");
    // Unused images show as reclaimable, pointing at Free space.
    assert!(s.contains("reclaimable (6 Free space)"), "{s}");
    // A wide and tall terminal shows all three side by side.
    let s = render(&app, 120, 40);
    assert!(s.contains("─ cpu · the VM process") && s.contains("─ disk · litek8s and its VM disk") && s.contains("deleted in the VM"), "{s}");
}

fn render_buf(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| view::draw(f, app)).unwrap();
    t.backend().buffer().clone()
}

fn theme(preset: super::theme::Preset, depth: super::theme::Depth, ascii: bool, bg: Option<super::theme::Bg>) -> super::theme::Theme {
    super::theme::Theme::build(preset, super::theme::Term { depth, ascii, bg }, super::theme::Background::Auto)
}

/// Every view, with and without an overlay, at sizes from small to large:
/// nothing panics, every line is the full width, the tabs and help fit.
#[test]
fn every_view_renders_at_many_sizes() {
    let mut app = sample_app();
    for (w, h) in [(60, 18), (80, 24), (100, 30), (120, 40), (200, 60)] {
        for (i, _) in TABS.iter().enumerate() {
            app.key(key(char::from(b'1' + i as u8)));
            for overlay in [false, true] {
                if overlay {
                    app.key(key('?'));
                }
                let s = render(&app, w, h);
                assert!(s.lines().all(|l| l.chars().count() == w as usize), "{w}x{h}:\n{s}");
                let tabs = if h >= 30 { 1 } else { 0 };
                assert!(s.lines().nth(tabs).unwrap().contains(&format!("{} ", i + 1)), "{w}x{h}:\n{s}");
                assert!(s.lines().last().unwrap().contains("help"), "{w}x{h}:\n{s}");
                if overlay {
                    app.key(key('x'));
                }
            }
        }
    }
}

/// Home: one column on small terminals, CPU and memory side by side with
/// their history on wide and tall ones; sparklines go before anything else.
#[test]
fn home_collapses_on_small_terminals() {
    let mut app = sample_app();
    for i in 0..30 {
        let mut home = app.home.clone();
        home.cpu_pct = Some(10.0 + i as f64);
        app.apply(Msg::Home(Box::new(home)));
    }
    assert_eq!(app.history.len(), 31);
    let wide = render(&app, 120, 40);
    assert!(wide.contains("─ cpu ") && wide.contains("─ memory ") && wide.contains("─ workloads "), "{wide}");
    // The graph: block columns rising to the right.
    assert!(wide.contains("▁") || wide.contains("▂"), "{wide}");
    let narrow = render(&app, 80, 24);
    assert!(narrow.contains("─ resources ─") && !narrow.contains("─ workloads "), "{narrow}");
    assert!(!narrow.contains('▁') && !narrow.contains('▂'), "no sparkline at 80 columns:\n{narrow}");
    // A bit wider, the sparklines come back in the single column.
    let mid = render(&app, 96, 30);
    assert!(mid.contains("─ resources ─") && (mid.contains('▆') || mid.contains('█')), "{mid}");
}

/// The history is kept only while Home is on screen (no gaps of a
/// minute between 2 s samples), bounded, and dropped with the cluster.
#[test]
fn history_only_while_home_shows() {
    let mut app = sample_app();
    assert_eq!(app.history.len(), 1);
    app.key(key('3'));
    app.apply(Msg::Home(Box::new(app.home.clone())));
    assert_eq!(app.history.len(), 1);
    app.key(key('1'));
    for _ in 0..HISTORY + 10 {
        app.apply(Msg::Home(Box::new(app.home.clone())));
    }
    assert_eq!(app.history.len(), HISTORY);
    let (cpu, mem) = *app.history.back().unwrap();
    assert!((cpu - 0.03).abs() < 1e-6 && (mem - 0.279).abs() < 1e-3, "{cpu} {mem}");
    let mut down = app.home.clone();
    down.status = DevStatus::Down;
    app.apply(Msg::Home(Box::new(down)));
    assert!(app.history.is_empty());
}

/// 24-bit, 256 and 16 colors, none (NO_COLOR or `mono`): each frame uses
/// only the colors its terminal can show.
#[test]
fn color_depths_use_only_what_the_terminal_has() {
    use super::theme::{Bg, Depth, Preset};
    use ratatui::style::Color;
    let mut app = sample_app();
    for (preset, depth, bg) in [
        (Preset::Default, Depth::True, Some(Bg::Rgb(0x1e, 0x1e, 0x2e))),
        (Preset::Default, Depth::True, Some(Bg::Rgb(0xff, 0xff, 0xff))),
        (Preset::Default, Depth::True, None),
        (Preset::Default, Depth::X256, Some(Bg::Dark)),
        (Preset::Default, Depth::X16, None),
        (Preset::HighContrast, Depth::True, None),
        (Preset::Mono, Depth::True, None),
        (Preset::Default, Depth::None, None),
    ] {
        app.kube.theme = theme(preset, depth, false, bg);
        for tab in ['1', '3', '6', '7'] {
            app.key(key(tab));
            let buf = render_buf(&app, 120, 40);
            let colors: Vec<Color> = buf.content.iter().flat_map(|c| [c.fg, c.bg]).filter(|c| *c != Color::Reset).collect();
            let ok = |c: &Color| match (preset, depth) {
                (Preset::Mono, _) | (_, Depth::None) => false,
                (Preset::HighContrast, _) | (_, Depth::X16) => !matches!(c, Color::Rgb(..) | Color::Indexed(_)),
                (_, Depth::X256) => !matches!(c, Color::Rgb(..)),
                _ => true,
            };
            assert!(colors.iter().all(ok), "{preset:?} {depth:?} tab {tab}: {:?}", colors.iter().find(|c| !ok(c)));
            if depth == Depth::True && preset == Preset::Default {
                assert!(colors.iter().any(|c| matches!(c, Color::Rgb(..))));
            }
        }
    }
}

/// `ascii: true`: nothing but ASCII on screen, the layout unchanged.
#[test]
fn ascii_mode_draws_only_ascii() {
    let mut app = sample_app();
    let plain = render(&app, 120, 40);
    app.kube.theme = theme(super::theme::Preset::Default, super::theme::Depth::X256, true, None);
    for (i, _) in TABS.iter().enumerate() {
        app.key(key(char::from(b'1' + i as u8)));
        for size in [(80, 24), (120, 40)] {
            let s = render(&app, size.0, size.1);
            assert!(s.is_ascii(), "{}", s.chars().filter(|c| !c.is_ascii()).collect::<String>());
        }
    }
    app.key(key('1'));
    let ascii = render(&app, 120, 40);
    assert_eq!(ascii.lines().count(), plain.lines().count());
    assert!(ascii.starts_with("+- litek8s -"), "{ascii}");
}

/// Status glyphs and their colors are the same everywhere: Home's chips,
/// the containers' states, the Kubernetes rows.
#[test]
fn statuses_look_the_same_everywhere() {
    use super::theme::{Bg, Depth, Preset};
    let mut app = sample_app();
    app.kube.theme = theme(Preset::Default, Depth::True, false, Some(Bg::Dark));
    let t = *app.theme();
    let find = |buf: &ratatui::buffer::Buffer, glyph: &str| buf.content.iter().find(|c| c.symbol() == glyph).map(|c| c.fg);
    let home = render_buf(&app, 80, 24);
    assert_eq!(find(&home, "●"), Some(t.ok.fg.unwrap()));
    assert_eq!(find(&home, "○"), Some(t.done.fg.unwrap()));
    app.key(key('3'));
    let ctrs = render_buf(&app, 80, 24);
    assert_eq!(find(&ctrs, "●"), Some(t.ok.fg.unwrap()));
    assert_eq!(find(&ctrs, "○"), Some(t.done.fg.unwrap()));
}

/// The memory meter is what the guest holds against the VM's `--memory`,
/// colored by how short the guest is: macOS's footprint (4.3 GB here, more
/// than the 4 GiB VM because it keeps pages the guest freed) neither fills
/// it nor turns it red; a guest with little available does.
#[test]
fn memory_meter_is_in_use_against_the_limit_not_the_footprint() {
    use super::theme::{Bg, Depth, Preset};
    use ratatui::style::Color;
    let mut app = sample_app();
    app.kube.theme = theme(Preset::Default, Depth::True, false, Some(Bg::Dark));
    let t = *app.theme();
    let shown = crate::dev::memory::Usage { litek8s: 40 << 20, containers: 600 << 20, cache: 0 };
    let roomy = crate::dev::memory::parse_meminfo("MemTotal: 4000000 kB\nMemFree: 2600000 kB\nMemAvailable: 3500000 kB\nCached: 1000000 kB\nSUnreclaim: 40000 kB\n");
    let mut home = app.home.clone();
    home.mem_used = Some(4_300_000_000);
    home.memory = Some(crate::dev::memory::vm_breakdown(4_300_000_000, 4096 << 20, &roomy, 0, None, "dev", shown, &[]));
    app.apply(Msg::Home(Box::new(home.clone())));
    let pct_color = |buf: &ratatui::buffer::Buffer| {
        let row = (0..buf.area.height).find(|y| (0..buf.area.width).map(|x| buf[(x, *y)].symbol()).collect::<String>().contains("in use · VM limit")).unwrap();
        // The memory meter's percentage: the last one before the text (the
        // CPU box shares the row on wide terminals).
        let text: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, row)].symbol()).collect();
        let at = (0..text.len()).find(|i| text[*i..].concat().starts_with("in use · VM limit")).unwrap();
        let x = (0..at as u16).rev().find(|x| buf[(*x, row)].symbol() == "%").unwrap();
        buf[(x, row)].fg
    };
    for (w, h) in [(80, 24), (120, 40)] {
        let s = render(&app, w, h);
        assert!(s.contains("in use · VM limit 4.0 GiB") && !s.contains("short"), "{s}");
        assert!(!s.contains("4.3 GB of"), "the footprint is not the meter: {s}");
        // In use: litek8s, pods and the kernel, well under half.
        let line = s.lines().find(|l| l.contains("in use · VM limit")).unwrap();
        assert!(line.contains('▒') && line.contains('█'), "stacked parts, cache and freed apart: {line}");
        assert_eq!(pct_color(&render_buf(&app, w, h)), t.level(0.0).fg.unwrap(), "green while the guest has memory available");
    }
    // The history follows what is in use, not the footprint.
    let (_, mem) = *app.history.back().unwrap();
    assert!(mem < 0.5, "{mem}");
    // A guest with 100 MB available is short: red, and it says so.
    let tight = crate::dev::memory::parse_meminfo("MemTotal: 4000000 kB\nMemFree: 50000 kB\nMemAvailable: 100000 kB\nCached: 80000 kB\nSUnreclaim: 40000 kB\n");
    home.memory = Some(crate::dev::memory::vm_breakdown(4_300_000_000, 4096 << 20, &tight, 0, None, "dev", crate::dev::memory::Usage { litek8s: 40 << 20, containers: 3500 << 20, cache: 0 }, &[]));
    app.apply(Msg::Home(Box::new(home)));
    let s = render(&app, 80, 24);
    assert!(s.contains("· short"), "{s}");
    let red = pct_color(&render_buf(&app, 80, 24));
    assert!(matches!(red, Color::Rgb(r, g, _) if r > 0xe0 && g < 0x90), "{red:?}");
}

/// Home's CPU breakdown, `b` once (memory first) at 80x24, with these rows.
fn cpu_screen(lines: &[(&str, f64)]) -> String {
    let mut app = sample_app();
    app.cpu = Some(crate::dev::cpu::Breakdown {
        total_pct: lines.iter().map(|(_, p)| p).sum(),
        cpus: 16.0,
        what: "the sandbox's cgroup",
        lines: lines.iter().map(|(l, p)| crate::dev::cpu::Line { label: l.to_string(), pct: *p, note: String::new(), indent: false }).collect(),
    });
    app.explain = 1;
    render(&app, 80, 24)
}

/// (number, bar) of a breakdown row: the bar is the block characters
/// between the label and the number.
fn row<'a>(s: &'a str, label: &str) -> (String, String) {
    let line = s.lines().find(|l| l.contains(&format!(" {label} "))).unwrap_or_else(|| panic!("no row {label}:\n{s}"));
    let rest = line.split_once(label).unwrap().1;
    let bar: String = rest.chars().filter(|c| "█▉▊▋▌▍▎▏".contains(*c)).collect();
    let number = rest.split_whitespace().find(|w| w.ends_with('%')).unwrap_or("").to_string();
    (number, bar)
}

/// The bars say what the numbers say: a full bar is one CPU (the unit the
/// rows are in, named in the box's title), not the largest row; a value
/// that rounds to nothing prints `<0.1%` with at most a hairline, and zero
/// draws no bar.
#[test]
fn cpu_bars_agree_with_their_numbers() {
    // Tiny values (the reported screen: an idle sandbox on 16 CPUs).
    let s = cpu_screen(&[("litek8s (dev)", 0.1), ("pods & containers", 0.0), ("sandbox", 0.03)]);
    assert!(s.contains("bar: 1 CPU"), "the scale is named:\n{s}");
    assert_eq!(row(&s, "litek8s (dev)"), ("0.1%".into(), "▏".into()), "{s}");
    assert_eq!(row(&s, "pods & containers"), ("0.0%".into(), "".into()), "{s}");
    assert_eq!(row(&s, "sandbox"), ("<0.1%".into(), "▏".into()), "{s}");
    assert!(s.contains("CPU · 0.1% of one CPU = <1% of 16 CPUs"), "{s}");
    // All zero: no bars at all.
    let s = cpu_screen(&[("litek8s (dev)", 0.0), ("pods & containers", 0.0), ("sandbox", 0.0)]);
    for l in ["litek8s (dev)", "pods & containers", "sandbox"] {
        assert_eq!(row(&s, l), ("0.0%".into(), "".into()), "{s}");
    }
    // One dominant row: its bar is its share of a CPU, the others stay
    // slivers (not scaled up to the largest), past one CPU it is full.
    let s = cpu_screen(&[("litek8s (dev)", 0.5), ("pods & containers", 50.0), ("sandbox", 1.2)]);
    let (n, bar) = row(&s, "pods & containers");
    assert_eq!(n, "50.0%");
    let width = row(&cpu_screen(&[("litek8s (dev)", 0.0), ("pods & containers", 100.0), ("sandbox", 0.0)]), "pods & containers").1.chars().count();
    assert_eq!(bar.chars().filter(|c| *c == '█').count(), width / 2, "half of a full bar ({width}): {s}");
    assert_eq!(row(&s, "litek8s (dev)").1, "▏", "{s}");
    let s = cpu_screen(&[("litek8s (dev)", 0.5), ("pods & containers", 250.0), ("sandbox", 1.2)]);
    assert_eq!(row(&s, "pods & containers"), ("250.0%".into(), "█".repeat(width)), "{s}");
    assert_eq!(row(&s, "sandbox").1.chars().count(), 1, "{s}");
}

/// Memory and disk bars are against their box's stated scale too: the VM
/// limit, the disk total.
#[test]
fn memory_and_disk_bars_name_their_scale() {
    let mut app = sample_app();
    let mem = crate::dev::memory::parse_meminfo("MemTotal: 4000000 kB\nMemFree: 3000000 kB\nMemAvailable: 3500000 kB\nCached: 100000 kB\n");
    let mut home = app.home.clone();
    home.memory = Some(crate::dev::memory::vm_breakdown(600 << 20, 4096 << 20, &mem, 0, None, "dev", crate::dev::memory::Usage { litek8s: 12 << 20, containers: 2048 << 20, cache: 0 }, &[]));
    app.apply(Msg::Home(Box::new(home)));
    let s = render(&app, 80, 24);
    assert!(s.contains("bar: VM limit 4.0 GiB"), "{s}");
    // 2 GiB of pods is half of the 4 GiB limit: half a bar.
    let full = s.lines().find(|l| l.contains(" pods & containers ")).unwrap();
    let bar: String = full.chars().filter(|c| "█▉▊▋▌▍▎▏".contains(*c)).collect();
    let litek8s: String = s.lines().find(|l| l.contains(" litek8s (dev) ")).unwrap().chars().filter(|c| "█▉▊▋▌▍▎▏".contains(*c)).collect();
    assert!(bar.chars().count() >= 5 && bar.chars().count() <= 7, "{s}");
    assert_eq!(litek8s, "▏", "12 MiB of 4 GiB is a sliver: {s}");
    assert!(s.contains("GiB in use · VM limit 4.0 GiB"), "{s}");
    // Under a GiB the meter says MB: a sliver never reads "0.0 GiB".
    let mut home = app.home.clone();
    let idle = crate::dev::memory::parse_meminfo("MemTotal: 4000000 kB\nMemFree: 3900000 kB\nMemAvailable: 3950000 kB\nCached: 20000 kB\n");
    home.memory = Some(crate::dev::memory::vm_breakdown(600 << 20, 4096 << 20, &idle, 0, None, "dev", crate::dev::memory::Usage { litek8s: 12 << 20, containers: 20 << 20, cache: 0 }, &[]));
    app.apply(Msg::Home(Box::new(home)));
    let s = render(&app, 80, 24);
    assert!(s.contains(" MB in use · VM limit 4.0 GiB") && !s.contains("0.0 GiB in use"), "{s}");
}

/// `cargo test --lib ui::tests::print_screens -- --ignored --nocapture`:
/// every view at 80x24 and 120x40, to look at.
#[test]
#[ignore]
fn print_screens() {
    let mut app = sample_app();
    for (w, h) in [(80, 24), (120, 40)] {
        for (i, _) in TABS.iter().enumerate() {
            app.key(key(char::from(b'1' + i as u8)));
            println!("{w}x{h} tab {}\n{}", i + 1, render(&app, w, h));
        }
    }
}

#[test]
fn observe_shows_alerts_top_pods_and_searches_logs() {
    let mut app = sample_app();
    let series = |base: f64| (0..17).map(|i| if i == 3 { None } else { Some(base * (1.0 + i as f64 / 16.0)) }).collect::<Vec<_>>();
    app.apply(Msg::Obs(Box::new(ObsSample {
        pods: vec![
            PodUsage { namespace: "default".into(), name: "web-7d9-abc".into(), cpu: series(0.05), mem: series(80e6) },
            PodUsage { namespace: "shop".into(), name: "db-0".into(), cpu: series(0.4), mem: series(30e6) },
        ],
        alerts: vec![AlertRow {
            state: "firing".into(),
            name: "KubePodCrashLooping".into(),
            target: "default/web-7d9-abc/nginx".into(),
            severity: "warning".into(),
            since: app.now - 300,
            summary: "default/web-7d9-abc (nginx) is crash looping".into(),
            fingerprint: "abc".into(),
            pod: Some(("default".into(), "web-7d9-abc".into())),
            ..Default::default()
        }],
        error: None,
        at: app.now,
    })));
    app.key(key('9'));
    assert_eq!(app.tab, Tab::Observe);
    let s = render(&app, 80, 24);
    assert!(s.contains("9 Obs") && s.contains("8 Set"), "{s}");
    assert!(s.contains("1 firing") && s.contains("KubePodCrashLooping") && s.contains("web-7d9-abc (nginx)"), "{s}");
    assert!(s.contains("Pods by CPU") && s.contains("db-0") && s.contains("800m") && s.contains("▁"), "{s}");
    // Top consumer first; s sorts by memory.
    assert_eq!(app.selected_obs_pod().unwrap().name, "db-0");
    // A pod deleted 20 minutes ago ranks last, marked gone.
    let mut gone = PodUsage { namespace: "default".into(), name: "old-job".into(), cpu: series(5.0), mem: series(1e9) };
    let n = gone.cpu.len();
    for i in n - 10..n {
        gone.cpu[i] = None;
        gone.mem[i] = None;
    }
    let mut with_gone = app.obs.clone();
    with_gone.pods.push(gone);
    app.apply(Msg::Obs(Box::new(with_gone)));
    assert_eq!(app.obs.pods[*app.visible_obs_pods().last().unwrap()].name, "old-job");
    assert!(render(&app, 80, 24).contains("gone"));
    app.key(key('s'));
    assert_eq!(app.selected_obs_pod().unwrap().name, "web-7d9-abc");
    assert!(render(&app, 80, 24).contains("Pods by memory"));
    // l: every container of the pod, gone ones too, through the log search.
    assert_eq!(app.key(key('l')), vec![Effect::Logs(LogSource::Search { namespace: Some("default".into()), pod: Some("web-7d9-abc".into()), grep: String::new() })]);
    app.key(code(KeyCode::Esc));
    // a: acknowledge the pod's alerts (asks first).
    assert!(app.key(key('a')).is_empty());
    assert!(matches!(&app.overlay, Overlay::Confirm { effect: Effect::AckAlerts(a), .. } if a.len() == 1));
    app.key(key('n'));
    // L: a regex for every pod's logs.
    app.key(key('L'));
    for c in "ERR(".chars() {
        app.key(key(c));
    }
    app.key(code(KeyCode::Enter));
    assert!(matches!(&app.overlay, Overlay::Input { error: Some(_), .. }), "a bad regex is refused");
    app.key(code(KeyCode::Backspace));
    assert_eq!(app.key(code(KeyCode::Enter)), vec![Effect::Logs(LogSource::Search { namespace: None, pod: None, grep: "ERR".into() })]);
    // Without the API server's observability: an explanation, not an empty view.
    app.key(code(KeyCode::Esc));
    app.apply(Msg::Obs(Box::new(ObsSample { error: Some("observability is off on this server".into()), ..Default::default() })));
    assert!(render(&app, 80, 24).contains("observability is off"));
}

#[test]
fn header_shows_a_development_build() {
    let mut app = sample_app();
    let plain = render(&app, 80, 24);
    assert!(!plain.contains("DEV"), "{plain}");
    app.build = "DEV 1a2b3c4d5e6f · ~/.litek8s-dev".into();
    // The boxed header (tall terminals) and the one-line one.
    let tall = render(&app, 120, 30);
    assert!(tall.lines().next().unwrap().contains("litek8s DEV 1a2b3c4d5e6f · ~/.litek8s-dev"), "{tall}");
    let short = render(&app, 80, 10);
    assert!(short.lines().next().unwrap().contains("litek8s DEV"), "{short}");
}

#[test]
fn other_contexts_can_be_hidden() {
    let mut app = sample_app();
    app.apply(Msg::Clusters(Box::new(clusters_sample())));
    app.key(key('2'));
    assert!(render(&app, 80, 24).contains("Other kubeconfig contexts"));
    // showOtherContexts: false in ui.yaml: gone now and from later samples.
    let cfg: super::kube::uiconfig::UiConfig = serde_yaml::from_str("showOtherContexts: false").unwrap();
    app.apply(Msg::Kube(super::kube::KMsg::Config(Ok(cfg))));
    let s = render(&app, 80, 24);
    assert!(!s.contains("Other kubeconfig contexts") && !s.contains("192.168.1.10") && s.contains("Local clusters"), "{s}");
    app.apply(Msg::Clusters(Box::new(clusters_sample())));
    assert!(!render(&app, 80, 24).contains("Other kubeconfig contexts"));
    assert_eq!(app.list_len(), app.clusters.local.len());
}

#[test]
fn clusters_t_opens_a_cluster_in_tree_mode() {
    let mut app = sample_app();
    app.apply(Msg::Clusters(Box::new(clusters_sample())));
    app.key(key('2'));
    assert!(render(&app, 80, 24).lines().last().unwrap().contains("tree"));
    // A local cluster: its tree, no question about kubectl or docker.
    app.key(key('j'));
    assert_eq!(app.key(key('t')), vec![Effect::UseCluster("ci".into())]);
    assert_eq!(app.tab, Tab::Kube);
    assert!(app.overlay == Overlay::None);
    assert!(matches!(app.kube.top(), super::kube::state::View::Tree(_)));
    // The switch resets the view: the tree stays.
    app.kube.reset(None, "litek8s-ci".into());
    assert!(matches!(app.kube.top(), super::kube::state::View::Tree(_)));
    // Another context: the same.
    app.key(key('2'));
    app.key(key('G'));
    assert_eq!(app.key(key('t')), vec![Effect::UseContext("orbstack".into())]);
    assert_eq!(app.tab, Tab::Kube);
    assert!(matches!(app.kube.top(), super::kube::state::View::Tree(_)));
}
