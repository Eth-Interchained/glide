//! A real QEMU process is suspended to exercise actual socket read deadlines.
use glide_core::{CreateOptions, Service};
use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
struct Resume(i32);
impl Drop for Resume {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0, libc::SIGCONT);
        }
    }
}
struct Cleanup(Service, String);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.force_stop(&self.1);
    }
}
#[test]
#[ignore = "requires real QEMU and a real GLIDE_TEST_ISO; never uses a simulated QMP server"]
fn unresponsive_real_qemu_is_unknown_not_stopped() {
    let root = tempfile::tempdir().unwrap();
    let s = Service::open(root.path().join("glide")).unwrap();
    let m = s
        .create(CreateOptions {
            name: "real-timeout".into(),
            iso: PathBuf::from(std::env::var_os("GLIDE_TEST_ISO").unwrap()),
            architecture: Some("x86_64".into()),
            cpu: 1,
            memory_mb: 512,
            disk_gb: 1,
        })
        .unwrap();
    let id = m.config.id;
    let _cleanup = Cleanup(s.clone(), id.clone());
    s.start(&id).unwrap();
    // console_path verified QMP's actual UUID before this private daemon pidfile is used.
    let serial = s.console_path(&id).unwrap();
    let pid: i32 = fs::read_to_string(serial.with_file_name("qemu.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid > 1);
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let resume = Resume(pid);
    let now = Instant::now();
    assert_eq!(s.status(&id).unwrap().state, "unknown");
    assert!(
        now.elapsed().as_secs() < 8,
        "QMP must have a bounded timeout"
    );
    assert!(s.start(&id).is_err(), "unknown is not safe to start");
    assert!(
        s.eject(&id).is_err(),
        "don't detach config when live QMP is unresponsive"
    );
    assert!(
        s.remove(&id, true).is_err(),
        "don't delete a possibly live disk"
    );
    drop(resume);

    // SIGCONT schedules QEMU to resume, but QMP recovery is asynchronous on
    // loaded Linux runners. Poll with a hard bound rather than assuming that
    // the first status request after SIGCONT must already succeed.
    let resumed = Instant::now();
    loop {
        let state = s.status(&id).unwrap().state;
        if state == "running" {
            break;
        }
        assert_eq!(state, "unknown", "resuming QEMU entered unexpected state");
        assert!(
            resumed.elapsed() < Duration::from_secs(10),
            "QEMU did not recover its QMP control channel after SIGCONT"
        );
        thread::sleep(Duration::from_millis(100));
    }

    assert!(s.status(&id).unwrap().config.iso.is_some());
    s.force_stop(&id).unwrap();
    assert_eq!(s.status(&id).unwrap().state, "stopped");
    s.remove(&id, true).unwrap();
}
