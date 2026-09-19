//! Opt-in real-emulator integration tests. No fake ISO and no mock backend.
//! GLIDE_TEST_ISO=/path/to/real.iso GLIDE_ACCEL=tcg GLIDE_DISPLAY=none \
//! cargo test -p glide-core --test real_qemu -- --ignored --test-threads=1
use glide_core::{CreateOptions, Service};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

struct Cleanup {
    service: Service,
    id: String,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.service.force_stop(&self.id);
    }
}

fn options(name: &str) -> CreateOptions {
    CreateOptions {
        name: name.into(),
        iso: PathBuf::from(
            std::env::var_os("GLIDE_TEST_ISO").expect("GLIDE_TEST_ISO must name a real installer"),
        ),
        architecture: Some("x86_64".into()),
        cpu: 1,
        memory_mb: 512,
        disk_gb: 1,
    }
}
fn qmp(socket: &std::path::Path, command: &str) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    for (id, cmd) in [(1, "qmp_capabilities"), (2, command)] {
        writeln!(stream, "{}", json!({"execute":cmd,"id":id})).unwrap();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("event").is_some() {
                continue;
            }
            assert_eq!(v["id"], id);
            assert!(v.get("error").is_none(), "{v}");
            if id == 2 {
                return v["return"].clone();
            }
            break;
        }
    }
    unreachable!()
}

#[test]
fn api_is_send_sync_and_invalid_requests_fail_without_backend() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Service>();
    let root = tempfile::tempdir().unwrap();
    let service = Service::open(root.path().join("glide")).unwrap();
    assert!(service.list().unwrap().is_empty());
    assert!(service
        .status("missing")
        .unwrap_err()
        .to_string()
        .contains("not found"));
    assert!(service.start("missing").is_err());
    assert!(service.remove("missing", true).is_err());
}

#[test]
#[ignore = "requires real QEMU and GLIDE_TEST_ISO; no backend or ISO is simulated"]
fn real_qemu_lifecycle_persistence_eject_and_safe_removal() {
    let root = tempfile::tempdir().unwrap();
    // Deliberately exceed sockaddr_un length and include comma/space in disk path.
    let home = root
        .path()
        .join("long-root-with-spaces-and,comma-".repeat(5));
    let service = Service::open(home.clone()).unwrap();
    let found = service.discover();
    assert!(found.available, "{}", found.detail);
    let mut create = options("real-lifecycle");
    create.architecture = None; // Exercise real EFI/ISO architecture inspection, not a forced result.
    let machine = service.create(create).unwrap();
    assert_eq!(machine.state, "stopped");
    let id = machine.config.id.clone();
    let _cleanup = Cleanup {
        service: service.clone(),
        id: id.clone(),
    };
    let disk = machine.config.disk.clone();
    let original_iso = machine.config.iso.clone().unwrap();
    assert!(
        disk.metadata().unwrap().len() < 1_073_741_824,
        "qcow2 is sparse"
    );
    assert!(service.create(options("real-lifecycle")).is_err());
    service.start(&id).unwrap();
    // A separately opened service sees real daemon state, not in-memory bookkeeping.
    let independent = Service::open(home.clone()).unwrap();
    assert_eq!(independent.status(&id).unwrap().state, "running");
    assert!(service.start(&id).is_err());
    assert!(service.remove(&id, true).is_err());
    let serial = service.console_path(&id).unwrap();
    assert!(serial.as_os_str().len() < 100);
    assert!(serial.exists());
    let socket = serial.with_file_name("qmp.sock");
    service.force_stop(&id).unwrap();
    assert!(
        service.status(&id).unwrap().config.iso.is_some(),
        "stop never silently detaches installer"
    );
    service.start(&id).unwrap();
    assert!(
        service.status(&id).unwrap().config.iso.is_some(),
        "start never silently detaches installer"
    );
    qmp(&socket, "stop");
    assert_eq!(service.status(&id).unwrap().state, "paused");
    qmp(&socket, "cont");
    assert_eq!(service.status(&id).unwrap().state, "running");
    let before = qmp(&socket, "query-block");
    assert!(before
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["device"] == "installer" && d.get("inserted").is_some()));
    service.eject(&id).unwrap();
    assert!(independent.status(&id).unwrap().config.iso.is_none());
    let after = qmp(&socket, "query-block");
    assert!(after
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["device"] == "installer" && d.get("inserted").is_none()));
    assert!(original_iso.exists());
    service.force_stop(&id).unwrap();
    assert_eq!(service.status(&id).unwrap().state, "stopped");
    // With no installer or guest OS, SeaBIOS cannot honor ACPI shutdown. Must not lie or restart.
    service.start(&id).unwrap();
    let error = service.stop(&id).unwrap_err();
    assert!(format!("{error:#}").contains("timed out"));
    assert_eq!(service.status(&id).unwrap().state, "running");
    service.force_stop(&id).unwrap();
    service.restart(&id).unwrap();
    assert_eq!(service.status(&id).unwrap().state, "running");
    service.force_stop(&id).unwrap();
    assert!(service.logs(&id).unwrap().contains("Launching real QEMU"));
    service.remove(&id, false).unwrap();
    assert!(disk.exists());
    assert!(service.list().unwrap().is_empty());
    let tombstone: Value =
        serde_json::from_slice(&fs::read(home.join("removed").join(format!("{id}.json"))).unwrap())
            .unwrap();
    assert_eq!(tombstone["disk_retained"], true);
    let second = service.create(options("delete-confirmed")).unwrap();
    service.remove(&second.config.id, true).unwrap();
    assert!(!second.config.disk.exists());
    assert!(original_iso.exists());
    let audit = fs::read_to_string(home.join("audit.jsonl")).unwrap();
    assert!(audit.lines().count() >= 10);
    if std::env::var("GLIDE_AUDIT").as_deref() == Ok("nedb") {
        let db = nedb_engine::Db::open(&home.join("audit.nedb"), None).unwrap();
        assert!(
            db.list("events").len() >= 10,
            "real NEDB audit persists across reopen"
        );
    }
}

#[test]
#[ignore = "requires real QEMU and GLIDE_TEST_ISO"]
fn offline_eject_and_external_disk_deletion_are_safe() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("glide");
    let s = Service::open(home.clone()).unwrap();
    let m = s.create(options("disk-safety")).unwrap();
    let iso = m.config.iso.clone().unwrap();
    s.eject(&m.config.id).unwrap();
    assert!(iso.exists());
    assert!(s.status(&m.config.id).unwrap().config.iso.is_none());
    let old = m.config.disk.with_extension("preserved.qcow2");
    fs::rename(&m.config.disk, &old).unwrap();
    symlink(&old, &m.config.disk).unwrap();
    assert!(s.start(&m.config.id).is_err());
    assert!(s.remove(&m.config.id, true).is_err());
    assert!(old.exists());
    s.remove(&m.config.id, false).unwrap();
    assert!(old.exists());
}
