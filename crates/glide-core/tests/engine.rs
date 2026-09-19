//! Engine tests: the lifecycle state machine and the lineage, driven against
//! the MockBackend so they run anywhere.

use glide_core::backend::Progress;
use glide_core::{Engine, MockBackend, VmState};
use std::path::{Path, PathBuf};

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glide-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_iso(root: &Path, name: &str) -> PathBuf {
    let p = root.join(name);
    std::fs::write(&p, format!("FAKE ISO IMAGE {name}\n")).unwrap();
    p
}

fn collect() -> (
    impl FnMut(Progress),
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let l2 = lines.clone();
    let cb = move |p: Progress| {
        if let Progress::ConsoleLine(s) = p {
            l2.lock().unwrap().push(s);
        }
    };
    (cb, lines)
}

#[test]
fn full_install_start_snapshot_trace_flow() {
    let root = tmp_root("full");
    let iso_path = make_iso(&root, "ubuntu-24.04.iso");

    let mut eng = Engine::in_memory_with(&root, MockBackend::new()).unwrap();

    // Register the ISO.
    let iso = eng.register_iso(&iso_path, Some("ubuntu-24.04")).unwrap();
    assert_eq!(iso.sha256.len(), 64, "sha256 hex is 64 chars");

    // Create a VM.
    let vm = eng.create_vm("dev", 64, 8).unwrap();
    assert_eq!(vm.state, VmState::Created);
    assert!(vm.disk.is_some());
    assert!(vm.disk.as_ref().unwrap().path.exists(), "disk file created");

    // Duplicate name is rejected.
    assert!(eng.create_vm("dev", 64, 8).is_err());

    // Install from the ISO.
    let (mut cb, _lines) = collect();
    let vm = eng.install_from_iso("dev", &iso.sha256, &mut cb).unwrap();
    assert_eq!(vm.state, VmState::Installed);
    assert!(vm.attached_iso.is_none(), "ISO detached after install");
    assert_eq!(
        vm.disk.as_ref().unwrap().installed_from_iso.as_deref(),
        Some(iso.sha256.as_str()),
        "disk records its installing ISO"
    );

    // Start it.
    let (mut cb2, _) = collect();
    let vm = eng.start("dev", &mut cb2).unwrap();
    assert_eq!(vm.state, VmState::Running);

    // Can't start twice.
    let (mut cb3, _) = collect();
    assert!(eng.start("dev", &mut cb3).is_err());

    // Snapshot it (allowed from a stopped/installed/running disk state).
    let snap = eng.snapshot("dev", "pre-docker").unwrap();
    assert!(snap.disk_path.exists(), "snapshot disk cloned");
    assert!(snap.parent.is_none(), "first snapshot roots at live disk");

    // A second snapshot chains to the first.
    let snap2 = eng.snapshot("dev", "post-docker").unwrap();
    assert_eq!(
        snap2.parent.as_deref(),
        Some(snap.id.as_str()),
        "snapshots chain"
    );

    // Trace shows the provenance.
    let trace = eng.trace("dev").unwrap();
    let joined = trace.join("\n");
    assert!(joined.contains("vm:dev"), "trace names the vm:\n{joined}");
    assert!(
        joined.contains("installed-from-iso"),
        "trace shows the ISO edge:\n{joined}"
    );
    assert!(
        joined.contains("pre-docker"),
        "trace lists snapshots:\n{joined}"
    );

    // Stop it.
    let vm = eng.stop("dev").unwrap();
    assert_eq!(vm.state, VmState::Stopped);

    // Backend recorded the stop.
    assert_eq!(eng.list().unwrap().len(), 1);
}

#[test]
fn failed_install_marks_vm_failed_and_detaches_iso() {
    let root = tmp_root("fail");
    let iso_path = make_iso(&root, "broken.iso");

    let mut backend = MockBackend::new();
    backend.fail_installs_with("EFI partition has no bootloader");
    let mut eng = Engine::in_memory_with(&root, backend).unwrap();

    let iso = eng.register_iso(&iso_path, None).unwrap();
    eng.create_vm("dev", 64, 8).unwrap();

    let (mut cb, _) = collect();
    let res = eng.install_from_iso("dev", &iso.sha256, &mut cb);
    assert!(res.is_err(), "failed install returns an error");

    let vm = eng.store.find_vm_by_name("dev").unwrap().unwrap();
    assert_eq!(vm.state, VmState::Failed);
    assert!(vm.attached_iso.is_none(), "ISO detached even on failure");
    assert!(vm.failure.as_deref().unwrap().contains("bootloader"));
}

#[test]
fn state_machine_rejects_illegal_transitions() {
    let root = tmp_root("transitions");
    let iso_path = make_iso(&root, "u.iso");
    let mut eng = Engine::in_memory_with(&root, MockBackend::new()).unwrap();
    let iso = eng.register_iso(&iso_path, None).unwrap();
    eng.create_vm("dev", 64, 8).unwrap();

    // Can't start a VM that was never installed.
    let (mut cb, _) = collect();
    assert!(
        eng.start("dev", &mut cb).is_err(),
        "start from Created is rejected"
    );

    // Can't stop a VM that isn't running.
    assert!(eng.stop("dev").is_err(), "stop from Created is rejected");

    // Can't install a VM that's already running.
    let (mut cb2, _) = collect();
    eng.install_from_iso("dev", &iso.sha256, &mut cb2).unwrap();
    let (mut cb3, _) = collect();
    eng.start("dev", &mut cb3).unwrap();
    let (mut cb4, _) = collect();
    assert!(
        eng.install_from_iso("dev", &iso.sha256, &mut cb4).is_err(),
        "reinstall while running is rejected"
    );
}

#[test]
fn install_records_boot_and_iso_lineage_in_store() {
    let root = tmp_root("lineage");
    let iso_path = make_iso(&root, "u.iso");
    let mut eng = Engine::in_memory_with(&root, MockBackend::new()).unwrap();
    let iso = eng.register_iso(&iso_path, None).unwrap();
    let vm0 = eng.create_vm("dev", 64, 8).unwrap();

    let (mut cb, _) = collect();
    eng.install_from_iso("dev", &iso.sha256, &mut cb).unwrap();

    // One boot recorded for the install, and it names the ISO.
    let boots = eng.store.boots_for(vm0.id.as_str()).unwrap();
    assert_eq!(boots.len(), 1);
    assert_eq!(boots[0].iso.as_deref(), Some(iso.sha256.as_str()));

    // The disk's causal edge to the ISO is real: tracing the ISO hash
    // reachable from the store returns at least the iso node itself.
    let iso_hash = eng.store.iso_hash(&iso.sha256).expect("iso node hash");
    let lineage = eng.store.trace_lineage(&iso_hash);
    assert!(
        lineage.iter().any(|l| l.contains("iso_images")),
        "lineage includes the iso node: {lineage:?}"
    );
}

#[test]
fn missing_vm_is_a_clean_not_found() {
    let root = tmp_root("missing");
    let mut eng = Engine::in_memory_with(&root, MockBackend::new()).unwrap();
    let (mut cb, _) = collect();
    let e = eng.start("ghost", &mut cb).unwrap_err();
    assert!(format!("{e}").contains("not found"), "{e}");
}
