//! Presentation fixtures below are data only: no fake hypervisor or Service implementation.
use super::*;
use forge_ui::Ui;
use glide_core::Config;
use std::collections::HashSet;
use std::time::Instant;

fn harness() -> (GlideApp, Receiver<Request>, Sender<Message>) {
    let (tx, requests) = channel();
    let (replies, rx) = channel();
    (
        GlideApp::new(
            PathBuf::from("/Users/test/Library/Application Support/Glide"),
            tx,
            rx,
        ),
        requests,
        replies,
    )
}
fn activate(app: &mut GlideApp, id: &str) {
    app.update(Action {
        id: id.into(),
        kind: ActionKind::Activate,
    });
}
fn change(app: &mut GlideApp, id: &str, text: &str) {
    app.update(Action {
        id: id.into(),
        kind: ActionKind::ChangeText(text.into()),
    });
}
fn fixture(id: &str, state: &str) -> Machine {
    Machine {
        config: Config {
            id: id.into(),
            name: format!("Presentation fixture {id}"),
            architecture: "x86_64".into(),
            cpu: 4,
            memory_mb: 8192,
            disk_gb: 64,
            iso: Some(PathBuf::from("/Users/test/Downloads/debian-12-amd64.iso")),
            disk: PathBuf::from(format!(
                "/Users/test/Library/Application Support/Glide/vms/{id}/disk.qcow2"
            )),
        },
        state: state.into(),
    }
}
fn ready(app: &mut GlideApp) {
    app.receive(Message::Backend(BackendInfo {
        available: true,
        architecture: "x86_64".into(),
        accelerator: "presentation fixture".into(),
        detail: "Synthetic Config for UI tests only; not a boot claim.".into(),
    }));
    app.receive(Message::Machines(Ok(vec![
        fixture("vm-a", "stopped"),
        fixture("vm-b", "running"),
    ])));
    app.rebuild();
}
fn rendered(app: &GlideApp) -> Ui {
    let mut ui = Ui::new(app.view()).expect("valid unique widget IDs");
    ui.theme = app.theme();
    ui.resize(1120, 820, 1.0);
    ui.paint();
    let mut ids = HashSet::new();
    for item in &ui.items {
        assert!(ids.insert(&item.node.id), "duplicate {}", item.node.id);
    }
    assert!(ui
        .painter
        .pixels
        .iter()
        .any(|p| *p != app.theme().background.0));
    ui
}
fn enabled(app: &GlideApp, id: &str) -> bool {
    rendered(app).item(id).expect(id).node.enabled
}

#[test]
fn form_actions_validate_and_dispatch_exact_create_options() {
    let (mut app, requests, _replies) = harness();
    ready(&mut app);
    activate(&mut app, "new");
    activate(&mut app, "create");
    assert!(app.error.as_ref().unwrap().contains("name"));
    assert!(requests.try_recv().is_err());
    change(&mut app, "name", "Debian lab");
    change(&mut app, "iso", "/tmp/install.iso");
    change(&mut app, "architecture", "amd64");
    activate(&mut app, "create");
    let request = requests.try_recv().unwrap();
    let Command::Create(options) = request.command else {
        panic!("wrong command")
    };
    assert_eq!(options.name, "Debian lab");
    assert_eq!(options.iso, PathBuf::from("/tmp/install.iso"));
    assert_eq!(options.architecture.as_deref(), Some("x86_64"));
    assert_eq!(
        (options.cpu, options.memory_mb, options.disk_gb),
        (4, 8192, 64)
    );
    assert!(app.pending.is_some());
    assert!(!enabled(&app, "create"));
    activate(&mut app, "create");
    assert!(requests.try_recv().is_err());
}
#[test]
fn pending_is_immediate_and_only_matching_completion_releases_it() {
    let (mut app, requests, replies) = harness();
    ready(&mut app);
    activate(&mut app, "start");
    let request = requests.try_recv().unwrap();
    assert!(matches!(request.command, Command::Start(ref id) if id == "vm-a"));
    assert_eq!(
        app.machine().unwrap().state,
        "stopped",
        "no optimistic success"
    );
    activate(&mut app, "start");
    assert!(requests.try_recv().is_err());
    replies
        .send(Message::Complete {
            sequence: request.sequence + 10,
            result: Ok(Outcome::default()),
        })
        .unwrap();
    assert!(app.tick());
    assert!(app.pending.is_some());
    replies
        .send(Message::Complete {
            sequence: request.sequence,
            result: Err("actual QEMU startup failure".into()),
        })
        .unwrap();
    assert!(app.tick());
    assert!(app.pending.is_none());
    assert!(app.error.as_ref().unwrap().contains("QEMU"));
}
#[test]
fn power_controls_follow_actual_state_and_poll_errors_are_visible() {
    let (mut app, requests, replies) = harness();
    ready(&mut app);
    assert!(enabled(&app, "start"));
    assert!(!enabled(&app, "stop"));
    activate(&mut app, "select:vm-b");
    assert!(!enabled(&app, "start"));
    assert!(enabled(&app, "stop"));
    assert!(!enabled(&app, "delete"));
    activate(&mut app, "delete");
    assert!(app.removal.is_none());
    replies
        .send(Message::Machines(Err(
            "Cannot read QMP socket: permission denied".into(),
        )))
        .unwrap();
    app.tick();
    assert!(!enabled(&app, "stop"));
    assert!(app.error.as_ref().unwrap().contains("permission denied"));
    activate(&mut app, "stop");
    assert!(requests.try_recv().is_err());
}
#[test]
fn deletion_needs_second_confirmation_and_cancel_is_safe() {
    let (mut app, requests, _replies) = harness();
    ready(&mut app);
    activate(&mut app, "delete");
    assert!(app.removal.as_ref().unwrap().delete_disk);
    assert!(requests.try_recv().is_err());
    rendered(&app);
    activate(&mut app, "cancel-remove");
    activate(&mut app, "confirm-remove");
    assert!(requests.try_recv().is_err());
    assert!(app.removal.is_none());
    activate(&mut app, "delete");
    // A background selection change cannot redirect the stored confirmation target.
    app.selected = Some("vm-b".into());
    activate(&mut app, "confirm-remove");
    assert!(
        matches!(requests.try_recv().unwrap().command, Command::Remove { id, delete_disk: true } if id == "vm-a")
    );
}
#[test]
fn keep_disk_is_distinct_and_state_change_invalidates_confirmation() {
    let (mut app, requests, _replies) = harness();
    ready(&mut app);
    activate(&mut app, "remove");
    activate(&mut app, "confirm-remove");
    assert!(
        matches!(requests.try_recv().unwrap().command, Command::Remove { id, delete_disk: false } if id == "vm-a")
    );
    app.pending = None;
    activate(&mut app, "delete");
    app.receive(Message::Machines(Ok(vec![fixture("vm-a", "running")])));
    activate(&mut app, "confirm-remove");
    assert!(requests.try_recv().is_err());
    assert!(app.removal.is_none());
    assert!(app
        .error
        .as_ref()
        .unwrap()
        .contains("no longer confirmed stopped"));
}
#[test]
fn console_shell_arguments_are_quoted_without_injection() {
    let command = console_command(
        Path::new("/Applications/Glide's tools/glide"),
        Path::new("/tmp/a;$(touch NO)"),
        "id' ; echo bad",
    )
    .unwrap();
    assert_eq!(command, "'/Applications/Glide'\\''s tools/glide' --root '/tmp/a;$(touch NO)' console 'id'\\'' ; echo bad'");
}
#[test]
fn worker_disconnect_releases_pending_with_an_error() {
    let (mut app, requests, replies) = harness();
    ready(&mut app);
    activate(&mut app, "refresh");
    let _ = requests.try_recv().unwrap();
    drop(replies);
    assert!(!app.tick());
    assert!(app.pending.is_none());
    assert!(app.error.is_some());
    assert!(!enabled(&app, "new"));
}
#[test]
fn real_service_worker_reads_and_refreshes_an_empty_library() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = GlideApp::spawn(dir.path().join("library"));
    let until = Instant::now() + Duration::from_secs(10);
    while app.backend.is_none() && app.connected && Instant::now() < until {
        app.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        app.backend.is_some(),
        "real Service did not report backend discovery: {:?}",
        app.error
    );
    while !app.fresh && app.connected && Instant::now() < until {
        app.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.fresh, "real library list failed: {:?}", app.error);
    assert!(app.machines.is_empty());
    activate(&mut app, "refresh");
    while app.pending.is_some() && Instant::now() < until {
        app.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.pending.is_none());
    assert!(app.error.is_none(), "{:?}", app.error);
    let ui = rendered(&app);
    if let Some(dir) = std::env::var_os("GLIDE_GUI_RENDER_DIR") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("real-backend.ppm");
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        ui.painter.save_ppm(&path).unwrap();
    }
}
#[test]
fn all_panels_render_unique_ids_and_fit_the_window() {
    let (mut app, _requests, _replies) = harness();
    for panel in ["empty", "detail", "create", "delete"] {
        match panel {
            "detail" => ready(&mut app),
            "create" => activate(&mut app, "new"),
            "delete" => {
                activate(&mut app, "cancel-create");
                activate(&mut app, "delete");
            }
            _ => {}
        }
        let ui = rendered(&app);
        for id in ["library", "detail", "activity"] {
            let r = ui.item(id).unwrap().rect;
            assert!(
                r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= 1120.1 && r.y + r.h <= 820.1,
                "{panel}: {id}: {r:?}"
            );
        }
        if panel == "create" {
            let r = ui.item("create").unwrap().rect;
            let clip = ui.item("detail").unwrap().rect;
            assert!(
                r.y + r.h <= clip.y + clip.h - 20.0,
                "create button needs scrolling: {r:?} in {clip:?}"
            );
        }
        if let Some(dir) = std::env::var_os("GLIDE_GUI_RENDER_DIR") {
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{panel}.ppm"));
            if path.exists() {
                std::fs::remove_file(&path).unwrap();
            }
            ui.painter.save_ppm(&path).unwrap();
        }
    }
}
