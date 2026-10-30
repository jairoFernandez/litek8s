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
        env_cpus: Some(4.0),
        env_memory: Some(4_294_967_296),
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
        others_litek8s: [("home".to_string(), true), ("orbstack".to_string(), false)].into_iter().collect(),
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
    // A remote litek8s cluster says so; another cluster is read-only.
    let home = s.lines().find(|l| l.contains("home") && l.contains("192.168.1.10")).unwrap();
    assert!(home.contains("litek8s"), "{home}");
    let orb = s.lines().find(|l| l.contains("orbstack")).unwrap();
    assert!(orb.contains("read-only"), "{orb}");
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
    let shown = crate::dev::memory::Usage { litek8s: 12 << 20, pods: 30 << 20, docker: 0, cache: 0 };
    let mut home = app.home.clone();
    home.memory = Some(crate::dev::memory::vm_breakdown(421 << 20, 4096 << 20, &mem, 0, None, "dev", shown, &[]));
    app.apply(Msg::Home(Box::new(home)));
    let s = render(&app, 80, 24);
    assert!(s.contains("Memory · 441 MB (what macOS charges the VM)"), "{s}");
    assert!(s.contains("litek8s (dev)") && s.contains("12.6 MB") && s.contains(" pods ") && s.contains("Docker containers"), "{s}");
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
    // Rows and bars in shares of the VM's 4 CPUs, as the meter above.
    assert!(s.contains("CPU · 7.5% of 4 CPUs = 30% of one") && s.contains("bar: 4 CPUs"), "{s}");
    assert!(s.contains("pod default/web-7d9") && s.contains("5.0%") && s.contains("VM overhead"), "{s}");
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
    assert!(wide.contains("─ cpu ") && wide.contains("─ memory ") && wide.contains("─ pods ") && wide.contains("─ docker "), "{wide}");
    // The graph: block columns rising to the right.
    assert!(wide.contains("▁") || wide.contains("▂"), "{wide}");
    let narrow = render(&app, 80, 24);
    assert!(narrow.contains("─ cluster ─") && narrow.contains("─ docker ") && !narrow.contains("─ pods "), "{narrow}");
    assert!(!narrow.contains('▁') && !narrow.contains('▂'), "no sparkline at 80 columns:\n{narrow}");
    // A bit wider, the sparklines come back in the single column.
    let mid = render(&app, 96, 30);
    assert!(mid.contains("─ cluster ─") && (mid.contains('▆') || mid.contains('█')), "{mid}");
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
    let shown = crate::dev::memory::Usage { litek8s: 40 << 20, pods: 600 << 20, docker: 0, cache: 0 };
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
    home.memory = Some(crate::dev::memory::vm_breakdown(4_300_000_000, 4096 << 20, &tight, 0, None, "dev", crate::dev::memory::Usage { litek8s: 40 << 20, pods: 3500 << 20, docker: 0, cache: 0 }, &[]));
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
        what: "the sandbox's cgroup".into(),
        lines: lines.iter().map(|(l, p)| crate::dev::cpu::Line { label: l.to_string(), pct: *p, note: String::new(), indent: false }).collect(),
        docker_pct: 0.0,
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

/// The bars say what the numbers say: a full bar is every CPU of what Home
/// shows (16 here), the unit the rows are in and the meter's; a value that
/// rounds to nothing prints `<0.1%` with at most a hairline, and zero draws
/// no bar. (Rows come in % of one CPU: 1600% is all 16.)
#[test]
fn cpu_bars_agree_with_their_numbers() {
    // Tiny values (an idle sandbox on 16 CPUs).
    let s = cpu_screen(&[("litek8s (dev)", 1.6), ("pods & containers", 0.0), ("sandbox", 0.03)]);
    assert!(s.contains("bar: 16 CPUs"), "the scale is named:\n{s}");
    assert_eq!(row(&s, "litek8s (dev)"), ("0.1%".into(), "▏".into()), "{s}");
    assert_eq!(row(&s, "pods & containers"), ("0.0%".into(), "".into()), "{s}");
    assert_eq!(row(&s, "sandbox"), ("<0.1%".into(), "▏".into()), "{s}");
    assert!(s.contains("CPU · 0.1% of 16 CPUs = 2% of one"), "{s}");
    // All zero: no bars at all.
    let s = cpu_screen(&[("litek8s (dev)", 0.0), ("pods & containers", 0.0), ("sandbox", 0.0)]);
    for l in ["litek8s (dev)", "pods & containers", "sandbox"] {
        assert_eq!(row(&s, l), ("0.0%".into(), "".into()), "{s}");
    }
    // One dominant row: its bar is its share of the 16 CPUs, the others
    // stay slivers (not scaled up to the largest); all 16 is a full bar.
    let s = cpu_screen(&[("litek8s (dev)", 8.0), ("pods & containers", 800.0), ("sandbox", 19.2)]);
    let (n, bar) = row(&s, "pods & containers");
    assert_eq!(n, "50.0%");
    let width = row(&cpu_screen(&[("litek8s (dev)", 0.0), ("pods & containers", 1600.0), ("sandbox", 0.0)]), "pods & containers").1.chars().count();
    assert_eq!(bar.chars().filter(|c| *c == '█').count(), width / 2, "half of a full bar ({width}): {s}");
    assert_eq!(row(&s, "litek8s (dev)").1, "▏", "{s}");
    assert_eq!(row(&s, "sandbox").1.chars().count(), 1, "{s}");
}

/// Memory and disk bars are against their box's stated scale too: the VM
/// limit, the disk total.
#[test]
fn memory_and_disk_bars_name_their_scale() {
    let mut app = sample_app();
    let mem = crate::dev::memory::parse_meminfo("MemTotal: 4000000 kB\nMemFree: 3000000 kB\nMemAvailable: 3500000 kB\nCached: 100000 kB\n");
    let mut home = app.home.clone();
    home.memory = Some(crate::dev::memory::vm_breakdown(600 << 20, 4096 << 20, &mem, 0, None, "dev", crate::dev::memory::Usage { litek8s: 12 << 20, pods: 2048 << 20, docker: 0, cache: 0 }, &[]));
    app.apply(Msg::Home(Box::new(home)));
    let s = render(&app, 80, 24);
    assert!(s.contains("bar: VM limit 4.0 GiB"), "{s}");
    // 2 GiB of pods is half of the 4 GiB limit: half a bar.
    let full = s.lines().find(|l| l.contains(" pods ") && l.contains("GB")).unwrap();
    let bar: String = full.chars().filter(|c| "█▉▊▋▌▍▎▏".contains(*c)).collect();
    let litek8s: String = s.lines().find(|l| l.contains(" litek8s (dev) ")).unwrap().chars().filter(|c| "█▉▊▋▌▍▎▏".contains(*c)).collect();
    assert!(bar.chars().count() >= 5 && bar.chars().count() <= 7, "{s}");
    assert_eq!(litek8s, "▏", "12 MiB of 4 GiB is a sliver: {s}");
    assert!(s.contains("GiB in use · VM limit 4.0 GiB"), "{s}");
    // Under a GiB the meter says MB: a sliver never reads "0.0 GiB".
    let mut home = app.home.clone();
    let idle = crate::dev::memory::parse_meminfo("MemTotal: 4000000 kB\nMemFree: 3900000 kB\nMemAvailable: 3950000 kB\nCached: 20000 kB\n");
    home.memory = Some(crate::dev::memory::vm_breakdown(600 << 20, 4096 << 20, &idle, 0, None, "dev", crate::dev::memory::Usage { litek8s: 12 << 20, pods: 20 << 20, docker: 0, cache: 0 }, &[]));
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
            PodUsage { namespace: "default".into(), name: "web-7d9-abc".into(), cpu: series(0.05), mem: series(80e6), ..Default::default() },
            PodUsage { namespace: "shop".into(), name: "db-0".into(), cpu: series(0.4), mem: series(30e6), ..Default::default() },
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
    assert!(s.contains("Pods by CPU") && s.contains("db-0") && s.contains("800m/—") && s.contains("▆"), "{s}");
    // Top consumer first; s sorts by memory.
    assert_eq!(app.selected_obs_pod().unwrap().name, "db-0");
    // A pod deleted 20 minutes ago ranks last, marked gone.
    let mut gone = PodUsage { namespace: "default".into(), name: "old-job".into(), cpu: series(5.0), mem: series(1e9), ..Default::default() };
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
fn observe_shows_use_against_limits_and_requests() {
    let mut app = sample_app();
    let flat = |v: f64| vec![Some(v); 17];
    let mi = (1u64 << 20) as f64;
    let pod = |name: &str, cpu: f64, mem: f64, cpu_bounds: Bounds, mem_bounds: Bounds| PodUsage { namespace: "shop".into(), name: name.into(), cpu: flat(cpu), mem: flat(mem), cpu_bounds, mem_bounds };
    let limit = |l: f64| Bounds { limit: Some(l), request: Some(l / 2.0) };
    app.apply(Msg::Obs(Box::new(ObsSample {
        pods: vec![
            // Near both ceilings.
            pod("api-0", 0.475, 243.0 * mi, limit(0.5), limit(256.0 * mi)),
            // Requests only: measured against them.
            pod("worker-1", 0.3, 100.0 * mi, Bounds { limit: None, request: Some(0.1) }, Bounds { limit: None, request: Some(200.0 * mi) }),
            // Neither: said so, not blank.
            pod("cache-2", 0.02, 50.0 * mi, Bounds::default(), Bounds::default()),
        ],
        at: app.now,
        ..Default::default()
    })));
    app.key(key('9'));
    let s = render(&app, 80, 24);
    assert!(s.contains("CPU/LIMIT") && s.contains("MEM/LIMIT") && s.contains("% of limit, r: of request"), "{s}");
    let line = |name: &str| s.lines().find(|l| l.contains(name)).unwrap_or_else(|| panic!("{name} missing:\n{s}")).to_string();
    let api = line("api-0");
    assert!(api.contains("475m/500m") && api.contains("95%") && api.contains("243Mi/256Mi"), "{api}");
    let worker = line("worker-1");
    assert!(worker.contains("300m/100m") && worker.contains("r300%") && worker.contains("100Mi/200Mi") && worker.contains("r 50%"), "{worker}");
    let cache = line("cache-2");
    assert!(cache.contains("20m/—") && cache.contains("50Mi/—"), "{cache}");
    // The sorted resource's sparkline still fits at 80 columns.
    assert!(api.contains('▁') || api.contains('█'), "{api}");
    // Wide: both sparklines.
    let wide = render(&app, 140, 24);
    assert_eq!(wide.lines().find(|l| l.contains("api-0")).unwrap().matches('█').count(), 34, "{wide}");
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

/// `litek8s ui --context <a remote litek8s>`: Home samples its API server,
/// the Docker views read this machine's (down) dev cluster.
fn remote_app() -> App {
    let mut app = App::new("unix:///Users/me/.litek8s/docker.sock".into(), "https://100.112.25.4:6443".into(), "lab2".into());
    app.docker_is_local_dev = true;
    app.apply(Msg::DockerDown("cannot connect to /Users/me/.litek8s/docker.sock: No such file or directory (os error 2)".into()));
    app
}

#[test]
fn a_remote_litek8s_graphs_and_names_its_node() {
    use crate::dev::cpu::{Kind, Sample, Split};
    let mut app = remote_app();
    let t0 = std::time::Instant::now();
    let mem = crate::dev::memory::MemInfo { total: 4 << 30, free: 3 << 30, available: 3600 << 20, cached: 580 << 20, buffers: 30 << 20, shmem: 5 << 20, slab_reclaimable: 40 << 20, ..Default::default() };
    let node = crate::dev::memory::NodeMemory { node: "lab2-control-1".into(), usage: crate::dev::memory::Usage { litek8s: 8 << 20, pods: 0, docker: 0, cache: 0 }, meminfo: Some(mem) };
    for i in 0..3u64 {
        let home = HomeSample {
            status: DevStatus::Remote,
            memory: Some(crate::dev::memory::node_breakdown(&node)),
            cpu_sample: Some(Sample { at: Some(t0 + std::time::Duration::from_secs(2 * i)), total_usec: 200_000 * i, shown: Split { litek8s_usec: 20_000 * i, ..Default::default() }, cpus: 2.0, kind: Kind::Remote, ..Default::default() }),
            ..Default::default()
        };
        app.apply(Msg::Home(Box::new(home)));
    }
    // The graph fills (it stayed at "collecting…" before), CPU in shares of the node's 2 CPUs.
    assert_eq!(app.history.len(), 2, "two intervals, two points");
    assert!((app.history.back().unwrap().0 - 0.05).abs() < 1e-6, "{:?}", app.history);
    let s = render(&app, 120, 40);
    assert!(!s.contains("litek8s (dev)") && s.contains("operating system"), "{s}");
    assert!(!s.contains("collecting"), "{s}");
}

#[test]
fn a_remote_litek8s_is_not_started_or_stopped_from_home() {
    let mut app = remote_app();
    app.apply(Msg::Home(Box::new(HomeSample { status: DevStatus::Remote, ..Default::default() })));
    let s = render(&app, 120, 40);
    assert!(s.contains("lab2 · remote") && !s.contains("dev · remote"), "the header names the remote context:\n{s}");
    let footer = s.lines().rfind(|l| !l.trim().is_empty()).unwrap();
    assert!(!footer.contains("up") && footer.contains("copy API"), "{footer}");
    for k in ['u', 'd', 'R'] {
        assert!(app.key(key(k)).is_empty(), "{k} must not act on this machine's dev cluster");
        assert!(app.status.as_ref().is_some_and(|(m, err)| *err && m.contains("remote litek8s")), "{:?}", app.status);
    }
    assert_eq!(app.key(key('c')), vec![Effect::Kube(crate::ui::kube::state::KEffect::Copy("https://100.112.25.4:6443".into()))]);
}

#[test]
fn docker_views_of_a_remote_litek8s_explain_instead_of_failing() {
    let mut app = remote_app();
    app.apply(Msg::Home(Box::new(HomeSample { status: DevStatus::Remote, ..Default::default() })));
    // This machine's Docker tabs are not in the bar of a remote litek8s;
    // their keys say where they went, and the others keep their numbers.
    let s = render(&app, 120, 30);
    assert!(s.contains("2 Clusters  7 Kubernetes") && !s.contains("Containers"), "{s}");
    app.key(key('3'));
    assert_eq!(app.tab, Tab::Home);
    assert!(app.status.as_ref().is_some_and(|(m, err)| !*err && m.contains("this machine's Docker")), "{:?}", app.status);
    app.key(key('7'));
    assert_eq!(app.tab, Tab::Kube);
    app.key(KeyEvent::from(KeyCode::BackTab));
    assert_eq!(app.tab, Tab::Clusters, "Tab skips the hidden ones");
    app.key(key('1'));
    let s = render(&app, 120, 40);
    assert!(s.contains("not running (this machine's dev cluster)"), "{s}");
}

#[test]
fn clusters_starts_on_the_remote_context() {
    let mut app = remote_app();
    app.focus_context = Some("lab2".into());
    // The first sample may come before the kubeconfig lists it.
    app.apply(Msg::Clusters(Box::default()));
    assert_eq!(app.focus_context.as_deref(), Some("lab2"));
    let mut sample = ClustersSample::default();
    sample.local.push(ClusterRow { name: "dev".into(), context: "lkdev-dev".into(), ..Default::default() });
    sample.others = vec![("orbstack".into(), None), ("lab2".into(), Some("https://100.112.25.4:6443".into()))];
    app.apply(Msg::Clusters(Box::new(sample)));
    app.key(key('2'));
    assert!(matches!(app.selected_cluster(), Some(Err((c, _))) if c == "lab2"), "{:?}", app.selected_cluster());
    assert!(app.focus_context.is_none(), "only once: moving away stays");
}

#[test]
fn a_remote_cluster_adds_its_nodes_up_and_shows_each_one() {
    use crate::dev::{cpu, disk, memory};
    let mut app = remote_app();
    let gib = 1u64 << 30;
    let mem = |node: &str, used: u64| memory::NodeMemory {
        node: node.into(),
        usage: memory::Usage { litek8s: 70 << 20, pods: used, docker: 0, cache: 0 },
        meminfo: Some(memory::MemInfo { total: 2 * gib, free: gib, available: gib, ..Default::default() }),
    };
    let cpu = |node: &str, busy: u64| cpu::NodeCpu { node: node.into(), split: cpu::Split::default(), proc_stat: Some(cpu::ProcStat { busy_usec: busy, total_usec: busy * 10, cpus: 1 }) };
    let dsk = |node: &str, images: u64| disk::NodeDisk { node: node.into(), images, fs_used: 10 * gib, fs_size: 40 * gib, ..Default::default() };
    let sample = |busy: u64| {
        let nodes = NodeReports {
            memory: vec![mem("server-1", 100 << 20), mem("worker-1", 900 << 20)],
            cpu: vec![cpu("server-1", busy), cpu("worker-1", busy * 2)],
            disk: vec![dsk("server-1", 1 << 30), dsk("worker-1", 3 << 30)],
            errors: vec![],
        };
        HomeSample { status: DevStatus::Remote, nodes: Some(nodes), ..Default::default() }
    };
    app.apply(Msg::Home(Box::new(sample(1_000_000))));
    std::thread::sleep(std::time::Duration::from_millis(20));
    app.apply(Msg::Home(Box::new(sample(1_010_000))));
    // The whole cluster: RAM, CPUs and disks added up.
    let m = app.home.memory.as_ref().unwrap();
    assert_eq!((m.what.as_str(), m.limit, m.limit_what), ("cluster · 2 nodes", Some(4 * gib), "cluster RAM"));
    assert_eq!(app.cpu.as_ref().map(|c| c.cpus), Some(2.0));
    assert_eq!(app.home.disk_used, Some(4 << 30));
    assert_eq!(app.home.disk_free, Some(60 * gib), "the nodes' filesystems, not this machine's");
    let s = render(&app, 140, 44);
    assert!(s.contains("cluster · 2 nodes") && s.contains("cluster (2)") && s.contains("worker-1") && s.contains("cluster/node"), "{s}");
    // n: each node in turn, then the cluster again.
    app.key(key('n'));
    assert_eq!(app.scope.as_deref(), Some("server-1"));
    let m = app.home.memory.as_ref().unwrap();
    assert_eq!((m.what.as_str(), m.limit, m.limit_what), ("node server-1", Some(2 * gib), "node RAM"));
    assert_eq!(app.home.disk_used, Some(1 << 30));
    assert_eq!(app.cpu.as_ref().map(|c| c.cpus), Some(1.0));
    app.key(key('n'));
    assert_eq!(app.scope.as_deref(), Some("worker-1"));
    app.key(key('n'));
    assert_eq!(app.scope, None);
}

/// The Docker API's containers run on the cluster's node but are not the
/// cluster's: Home's cluster figures leave them out and its docker box
/// shows them; another engine's capacity (`--docker-host`) never becomes
/// the cluster's.
#[test]
fn local_docker_stays_out_of_the_cluster_figures() {
    use crate::dev::cpu::{Kind, Sample, Split};
    let mut app = sample_app();
    // Another engine with more CPUs and memory than the VM.
    app.docker_is_cluster = false;
    app.apply(Msg::DockerInfo { version: "28.0".into(), cpus: Some(16), memory: Some(32 << 30), os: "Docker Desktop".into() });
    let s = render(&app, 80, 24);
    assert!(s.contains("3% of 4 CPUs · 12% of one") && s.contains("1.2 GB of 4.3 GB"), "the VM's size, not the engine's:\n{s}");
    assert!(s.contains("─ docker ") && s.contains("its engine: 16 CPUs · 34.4 GB"), "{s}");
    assert!(!s.contains("of 16 CPUs"), "{s}");

    // The cluster's own Docker API: 1 GiB and 6% of a CPU are Docker's.
    app.docker_is_cluster = true;
    let mem = crate::dev::memory::parse_meminfo("MemTotal: 4000000 kB\nMemFree: 2000000 kB\nMemAvailable: 3000000 kB\nCached: 100000 kB\n");
    let t0 = std::time::Instant::now();
    let split = |pod: u64, docker: u64| Split { litek8s_usec: 0, cgroups: vec![("pod_x".into(), pod), ("docker/abc".into(), docker)] };
    for (at, total, pod, docker) in [(t0, 0, 0, 0), (t0 + std::time::Duration::from_secs(1), 120_000, 40_000, 60_000)] {
        let mut home = app.home.clone();
        home.cpu_pct = Some(12.0);
        home.memory = Some(crate::dev::memory::vm_breakdown(2 << 30, 4096 << 20, &mem, 0, None, "dev", crate::dev::memory::Usage { litek8s: 50 << 20, pods: 200 << 20, docker: 1 << 30, cache: 0 }, &[]));
        home.cpu_sample = Some(Sample { at: Some(at), total_usec: total, shown: split(pod, docker), guest_busy_usec: Some(total), cpus: 4.0, kind: Kind::Vm, ..Default::default() });
        app.apply(Msg::Home(Box::new(home)));
    }
    let s = render(&app, 80, 24);
    assert!(s.contains("─ docker ") && s.contains("cpu 6.0% of one · mem 1.1 GB"), "{s}");
    // 12% of one CPU for the VM, 6% of it Docker's: the cluster is 6%.
    assert!(s.contains("2% of 4 CPUs · 6% of one"), "{s}");
    let b = app.home.memory.as_ref().unwrap();
    assert_eq!(b.cluster_in_use(), b.in_use() - (1 << 30));
    // Without Docker's GiB the cluster holds under one.
    assert!(b.cluster_in_use() < 1 << 30 && s.contains("1.1 GB in use · VM limit") && !s.contains("GiB in use"), "{s}");
    // The sparkline's memory is the cluster's share too.
    let (_, m) = *app.history.back().unwrap();
    assert!((m as f64 - b.cluster_in_use() as f64 / (4096u64 << 20) as f64).abs() < 1e-3);
}

/// The macOS VM's data disk: what it holds against its size (or the
/// Mac's free space, when less), where to grow it when it is nearly
/// full, and its setting with the image's size now.
#[test]
fn the_vm_disk_shows_its_size_and_how_to_grow_it() {
    let mut app = sample_app();
    let node = |used: u64| crate::dev::disk::NodeDisk { fs_used: used, ..Default::default() };
    let mut home = app.home.clone();
    home.vm_disk = Some((3 << 30, 64 << 30));
    home.disk_node = Some((node(2_000_000_000), 0, Some(3 << 30)));
    home.disk_free = Some(150_000_000_000);
    app.apply(Msg::Home(Box::new(home.clone())));
    let s = render(&app, 80, 24);
    assert!(s.contains("1.9/64.0 GiB · Mac 150 GB free"), "{s}");
    assert!(!s.contains("grow:"), "{s}");
    // Nearly full: grow it in Settings.
    home.disk_node = Some((node(60 << 30), 0, Some(61 << 30)));
    app.apply(Msg::Home(Box::new(home.clone())));
    let s = render(&app, 80, 24);
    assert!(s.contains("60.0/64.0 GiB · Mac 150 GB free · grow: 8"), "{s}");
    // The Mac nearly full instead: a bigger VM disk would not help.
    home.disk_free = Some(1 << 30);
    app.apply(Msg::Home(Box::new(home.clone())));
    assert!(render(&app, 120, 40).contains("· Mac short"));
    home.disk_free = Some(150_000_000_000);
    app.apply(Msg::Home(Box::new(home.clone())));

    // Settings: the row, then a bigger size needs the VM to restart.
    app.key(key('8'));
    if cfg!(target_os = "macos") {
        let s = render(&app, 80, 24);
        assert!(s.contains("VM disk (GiB)") && s.contains("64 (default)  (now 64 GiB, grows only)"), "{s}");
    }
    app.key(key('j'));
    app.key(key('j'));
    app.key(code(KeyCode::Enter));
    app.key(key('8'));
    assert!(app.key(code(KeyCode::Enter)).is_empty(), "too small");
    let Overlay::Input { error: Some(e), .. } = &app.overlay else { panic!("{:?}", app.overlay) };
    assert!(e.contains("disk must be between 16 and 4096 GiB"), "{e}");
    app.key(code(KeyCode::Backspace));
    for c in "128".chars() {
        app.key(key(c));
    }
    let effects = app.key(code(KeyCode::Enter));
    let [Effect::SaveConfig(c)] = &effects[..] else { panic!("{effects:?}") };
    assert_eq!(c.disk, Some(128));
    app.apply(Msg::Config(Ok(c.clone())));
    home.running = Some(crate::dev::DevState {
        pid: 1,
        port: 6443,
        mode: "vm".into(),
        started_unix_ms: 0,
        cgroup: None,
        vm_ip: None,
        helper_pid: None,
        cpus: Some(crate::dev::config::default_cpus()),
        memory_mib: Some(4096),
        shares: Vec::new(),
        publish: Some("localhost".into()),
        name: Some("dev".into()),
        nodes: Some(1),
        docker_socket: None,
    });
    app.apply(Msg::Home(Box::new(home)));
    assert_eq!(app.restart_needed(), vec!["disk"]);
    assert!(render(&app, 80, 24).contains("Restart needed for disk"));
}

#[test]
fn home_and_nodes_show_what_the_pods_reserve() {
    use super::model::{Alloc, NodeAlloc};
    let mut app = remote_app();
    let gib = 1073741824.0;
    let meminfo = crate::dev::memory::MemInfo { total: 8 << 30, free: 6 << 30, available: 7 << 30, ..Default::default() };
    let node = |n: &str| crate::dev::memory::NodeMemory { node: n.into(), usage: crate::dev::memory::Usage { litek8s: 8 << 20, pods: 1 << 30, docker: 0, cache: 0 }, meminfo: Some(meminfo.clone()) };
    let mut r = super::app::NodeReports::default();
    r.memory = vec![node("lab2-a"), node("lab2-b")];
    app.apply(Msg::Home(Box::new(HomeSample { status: DevStatus::Remote, nodes: Some(r), ..Default::default() })));
    let pod = |name: &str, node: &str, req: f64, lim: Option<f64>| Pod {
        namespace: "default".into(),
        name: name.into(),
        status: "Running".into(),
        node: node.into(),
        alloc: Alloc { cpu_req: 0.5, cpu_lim: Some(1.0), mem_req: req * gib, mem_lim: lim.map(|l| l * gib) },
        ..Default::default()
    };
    app.apply(Msg::Pods(vec![pod("web", "lab2-a", 1.0, Some(2.0)), pod("db", "lab2-a", 2.0, None), pod("api", "lab2-b", 1.0, Some(3.0))]));
    app.apply(Msg::Nodes(vec![NodeAlloc { name: "lab2-a".into(), cpu: 4.0, memory: 8.0 * gib }, NodeAlloc { name: "lab2-b".into(), cpu: 4.0, memory: 8.0 * gib }]));
    // The cluster: 4 GiB of 16 reserved, 5 GiB limited, one pod unlimited.
    let s = render(&app, 140, 40);
    assert!(s.contains("reserved 4.0 GiB 25%") && s.contains("limits 5.0 GiB 31%"), "{s}");
    assert!(s.contains("reserved 1.50 CPUs 19%") && s.contains("1 pod without a lim"), "{s}");
    // The meters carry the marks: requests │ and limits ┃ on the bar.
    let mem_line = s.lines().find(|l| l.contains("GiB in use")).unwrap_or_else(|| panic!("{s}"));
    assert!(mem_line.contains("█│┃"), "marks on the bar: {mem_line}");
    // One node (n): only its pods, against its own allocatable.
    app.key(key('n'));
    let s = render(&app, 140, 40);
    assert!(s.contains("reserved 3.0 GiB 38%"), "{s}");
    // Narrow Home too.
    let s = render(&app, 80, 30);
    assert!(s.contains("reserved"), "{s}");
    // The nodes table's extra columns, a request before its limit.
    let ext = &app.kube.ext;
    assert_eq!(ext.get(&("Node".into(), "MEM REQ".into())).and_then(|m| m.get("lab2-a")).map(String::as_str), Some("3072Mi 38%"));
    assert_eq!(ext.get(&("Node".into(), "CPU LIM".into())).and_then(|m| m.get("lab2-b")).map(String::as_str), Some("1000m 25%"));
}

