use crate::{iso, qmp};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub id: String,
    pub name: String,
    pub architecture: String,
    pub cpu: u32,
    pub memory_mb: u64,
    pub disk_gb: u64,
    pub iso: Option<PathBuf>,
    pub disk: PathBuf,
}
#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub name: String,
    pub iso: PathBuf,
    pub architecture: Option<String>,
    pub cpu: u32,
    pub memory_mb: u64,
    pub disk_gb: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct Machine {
    pub config: Config,
    pub state: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct BackendInfo {
    pub available: bool,
    pub architecture: String,
    pub accelerator: String,
    pub detail: String,
}
#[derive(Clone, Debug)]
pub struct Service {
    root: PathBuf,
}
struct Backend {
    system: PathBuf,
    image: PathBuf,
    accelerator: String,
    display: String,
    firmware: Option<PathBuf>,
}

impl Service {
    pub fn default_root() -> PathBuf {
        if let Some(root) = env::var_os("GLIDE_HOME") {
            return PathBuf::from(root);
        }
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Glide")
        } else {
            env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("glide")
        }
    }
    pub fn open(root: PathBuf) -> Result<Self> {
        private_dir(&root)?;
        let root = root.canonicalize().context("canonicalize Glide root")?;
        private_dir(&root.join("machines"))?;
        private_dir(&root.join("removed"))?;
        Ok(Self { root })
    }
    fn lock(&self) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.root.join("service.lock"))?;
        ensure!(
            file.metadata()?.uid() == uid(),
            "service lock not owned by current user"
        );
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(file),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(40))
                }
                Err(e) => {
                    return Err(e).context("Glide busy in another CLI/GUI operation (lock timeout)")
                }
            }
        }
    }
    pub fn discover(&self) -> BackendInfo {
        let architecture = env::consts::ARCH.to_owned();
        let accelerator = env::var("GLIDE_ACCEL").unwrap_or_else(|_| "hvf".into());
        match backend(&architecture) {
            Ok(b) => BackendInfo { available: true, architecture, accelerator: b.accelerator,
                detail: format!("QEMU: {}; qemu-img: {}; display: {}. Actual HVF entitlement/guest boot errors are reported at launch.", b.system.display(), b.image.display(), b.display) },
            Err(e) => BackendInfo { available: false, architecture, accelerator, detail: format!("{e:#}") },
        }
    }
    pub fn create(&self, opts: CreateOptions) -> Result<Machine> {
        let _lock = self.lock()?;
        validate_name(&opts.name)?;
        validate_resources(opts.cpu, opts.memory_mb, opts.disk_gb)?;
        ensure!(
            !self.configs()?.iter().any(|c| c.name == opts.name),
            "machine name already exists: {}",
            opts.name
        );
        let installer = regular_path(&opts.iso, "installer ISO")?;
        let inspection = iso::inspect(&installer)?;
        let explicit = opts
            .architecture
            .as_deref()
            .map(normalize_arch)
            .transpose()?;
        if let (Some(selected), Some(actual)) = (&explicit, &inspection.detected) {
            ensure!(selected == actual, "explicit architecture {selected} conflicts with actual EFI PE architecture {actual}");
        }
        let architecture = explicit.or_else(|| inspection.detected.clone()).or_else(|| inspection.hint.clone())
            .context("ISO architecture not detectable from EFI PE and no unambiguous filename/volume hint; specify --arch x86_64 or --arch aarch64 explicitly")?;
        let evidence = if let Some(actual) = inspection.detected {
            format!("EFI PE machine detected: {actual}")
        } else if opts.architecture.is_some() {
            "architecture explicitly selected by user; not detected".into()
        } else {
            format!(
                "architecture inferred from filename/volume label (not detected): {architecture}"
            )
        };
        let b = backend(&architecture)?;
        let id = Uuid::new_v4().to_string();
        let dir = self.root.join("machines").join(&id);
        private_dir(&dir)?;
        let config = Config {
            id,
            name: opts.name,
            architecture,
            cpu: opts.cpu,
            memory_mb: opts.memory_mb,
            disk_gb: opts.disk_gb,
            iso: Some(installer),
            disk: dir.join("disk.qcow2"),
        };
        atomic_json(
            &dir.join("owner.json"),
            &json!({"id":config.id,"root":self.root}),
        )?;
        checked_output(
            Command::new(&b.image)
                .args(["create", "-f", "qcow2"])
                .arg(&config.disk)
                .arg(format!("{}G", config.disk_gb)),
            Duration::from_secs(30),
        )
        .context("qemu-img create failed; unregistered directory retained for inspection")?;
        self.validate_disk(&config)?;
        atomic_json(&dir.join("config.json"), &config)?;
        self.append_log(&config, &format!("Created sparse qcow2. ISO volume: {}. {evidence}. ISO remains attached until explicit eject.", inspection.label))?;
        self.audit("create", &config, json!({"architecture_evidence":evidence}));
        Ok(Machine {
            config,
            state: "stopped".into(),
        })
    }
    pub fn list(&self) -> Result<Vec<Machine>> {
        let _lock = self.lock()?;
        let mut machines = Vec::new();
        for config in self.configs()? {
            machines.push(self.machine(config)?);
        }
        machines.sort_by(|a, b| a.config.name.cmp(&b.config.name));
        Ok(machines)
    }
    pub fn status(&self, id_or_name: &str) -> Result<Machine> {
        let _lock = self.lock()?;
        self.machine(self.resolve(id_or_name)?)
    }
    fn machine(&self, config: Config) -> Result<Machine> {
        let state = match self.live(&config) {
            Ok(None) => "stopped".to_owned(),
            Ok(Some(mut q)) => match q.execute("query-status", json!({})) {
                Ok(v) => v["status"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_ascii_lowercase(),
                Err(e) => {
                    let _ = self.append_log(&config, &format!("QMP query-status error: {e:#}"));
                    "unknown".into()
                }
            },
            Err(e) => {
                let _ = self.append_log(&config, &format!("QMP connection error: {e:#}"));
                "unknown".into()
            }
        };
        Ok(Machine { config, state })
    }
    pub fn start(&self, id_or_name: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.start_locked(&self.resolve(id_or_name)?)
    }
    fn start_locked(&self, c: &Config) -> Result<()> {
        ensure!(
            self.live(c)?.is_none(),
            "machine already active; inspect live status"
        );
        self.validate_disk(c)?;
        let b = backend(&c.architecture)?;
        if let Some(path) = &c.iso {
            regular_path(path, "installer ISO")?;
            iso::inspect(path)?;
        }
        let runtime = self.runtime(c)?;
        for name in ["qmp.sock", "serial.sock"] {
            let p = runtime.join(name);
            if let Ok(m) = fs::symlink_metadata(&p) {
                use std::os::unix::fs::FileTypeExt;
                ensure!(
                    m.file_type().is_socket() && m.uid() == uid(),
                    "refusing to remove unexpected runtime entry {}",
                    p.display()
                );
                fs::remove_file(p)?;
            }
        }
        // Reject writable output symlinks before QEMU itself opens them.
        for name in ["qemu.pid", "launch.json"] {
            reject_existing_symlink(&runtime.join(name))?;
        }
        let qlog = self.dir(c).join("qemu.log");
        let log = safe_append(&qlog)?;
        safe_append(&self.dir(c).join("serial.log"))?;
        let mut cmd = Command::new(&b.system);
        cmd.arg("-name")
            .arg(format!("guest={}", c.name))
            .arg("-uuid")
            .arg(&c.id)
            .arg("-machine")
            .arg(if c.architecture == "x86_64" {
                "q35"
            } else {
                "virt"
            })
            .arg("-accel")
            .arg(&b.accelerator)
            .arg("-cpu")
            .arg(if b.accelerator == "hvf" {
                "host"
            } else {
                "max"
            })
            .arg("-smp")
            .arg(c.cpu.to_string())
            .arg("-m")
            .arg(c.memory_mb.to_string())
            .arg("-display")
            .arg(&b.display)
            .arg("-daemonize")
            .arg("-D")
            .arg(&qlog)
            .args(["-d", "guest_errors"])
            .arg("-pidfile")
            .arg(runtime.join("qemu.pid"))
            .arg("-qmp")
            .arg(format!(
                "unix:{},server=on,wait=off",
                qemu_path(&runtime.join("qmp.sock"))?
            ))
            .arg("-chardev")
            .arg(format!(
                "socket,id=serial0,path={},server=on,wait=off,logfile={},logappend=on",
                qemu_path(&runtime.join("serial.sock"))?,
                qemu_path(&self.dir(c).join("serial.log"))?
            ))
            .args([
                "-serial",
                "chardev:serial0",
                "-monitor",
                "none",
                "-device",
                "qemu-xhci",
                "-device",
                "usb-tablet",
            ])
            .args([
                "-netdev",
                "user,id=net0",
                "-device",
                "virtio-net-pci,netdev=net0",
            ])
            .arg("-drive")
            .arg(format!(
                "if=none,id=osdisk,file={},format=qcow2",
                qemu_path(&c.disk)?
            ))
            .args(["-device", "virtio-blk-pci,drive=osdisk,bootindex=2"]);
        if c.architecture == "x86_64" {
            cmd.args(["-vga", "std"]);
        } else {
            cmd.args(["-device", "virtio-gpu-pci"])
                .arg("-bios")
                .arg(b.firmware.as_ref().context("ARM firmware unavailable")?);
        }
        // Installer precedes disk on every boot until explicit eject. We never claim installation completed.
        if let Some(path) = &c.iso {
            cmd.arg("-drive").arg(format!(
                "if=none,id=installer,file={},format=raw,media=cdrom,readonly=on",
                qemu_path(path)?
            ));
            if c.architecture == "x86_64" {
                cmd.args([
                    "-device",
                    "ide-cd,drive=installer,id=cdrom,bus=ide.0,bootindex=1",
                ]);
            } else {
                cmd.args([
                    "-device",
                    "virtio-scsi-pci,id=scsi0",
                    "-device",
                    "scsi-cd,drive=installer,id=cdrom,bus=scsi0.0,bootindex=1",
                ]);
            }
        }
        self.append_log(
            c,
            &format!(
                "Launching real QEMU ({}, {}, display {}).",
                b.system.display(),
                b.accelerator,
                b.display
            ),
        )?;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let mut child = cmd.spawn().context("spawn QEMU")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(
                    status.success(),
                    "QEMU launch failed ({status}); see {}",
                    qlog.display()
                );
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("QEMU launcher timed out; daemon may have started. Inspect status/logs before retrying; no PID-based VM kill attempted");
            }
            thread::sleep(Duration::from_millis(50));
        }
        atomic_json(
            &runtime.join("launch.json"),
            &json!({"display":b.display,"binary":b.system}),
        )?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.live(c) {
                Ok(Some(mut q)) => {
                    let state = q.execute("query-status", json!({}))?;
                    ensure!(
                        state["status"] == "running",
                        "QEMU started but is not running: {state}; see logs"
                    );
                    self.audit("start", c, state);
                    return Ok(());
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e)
                            .context("QEMU launched but QMP unavailable; it may still be running");
                    }
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        bail!(
                            "QEMU exited or QMP socket did not appear; see {}",
                            qlog.display()
                        );
                    }
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    pub fn stop(&self, id_or_name: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.stop_locked(&self.resolve(id_or_name)?, false)
    }
    pub fn force_stop(&self, id_or_name: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.stop_locked(&self.resolve(id_or_name)?, true)
    }
    fn stop_locked(&self, c: &Config, force: bool) -> Result<()> {
        let Some(mut q) = self.live(c)? else {
            return Ok(());
        };
        // QEMU may close a connection during quit before the reply is read. An error
        // is never success: retain it and verify actual socket absence/refusal below.
        let mut last_error = q
            .execute(if force { "quit" } else { "system_powerdown" }, json!({}))
            .err()
            .map(|e| format!("{e:#}"));
        drop(q);
        let deadline = Instant::now() + Duration::from_secs(if force { 10 } else { 30 });
        loop {
            match self.live(c) {
                Ok(None) => {
                    self.audit(if force { "force-stop" } else { "stop" }, c, json!({}));
                    return Ok(());
                }
                Ok(Some(mut q)) if !force => match q.execute("query-status", json!({})) {
                    Ok(v) if v["status"] == "shutdown" => {
                        if let Err(e) = q.execute("quit", json!({})) {
                            last_error = Some(format!("{e:#}"));
                        }
                    }
                    Err(e) => last_error = Some(format!("{e:#}")),
                    _ => (),
                },
                Ok(Some(_)) => (),
                Err(e) => last_error = Some(format!("{e:#}")),
            }
            if Instant::now() >= deadline {
                bail!("{} timed out; guest may still be running. Inspect status; use force-stop only if data loss is acceptable. Last QMP error: {}", if force { "QMP quit" } else { "Graceful shutdown" }, last_error.as_deref().unwrap_or("none; QEMU remains active"));
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
    pub fn restart(&self, id_or_name: &str) -> Result<()> {
        let _lock = self.lock()?;
        let c = self.resolve(id_or_name)?;
        self.stop_locked(&c, false)?;
        self.start_locked(&c)
    }
    pub fn eject(&self, id_or_name: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut c = self.resolve(id_or_name)?;
        if c.iso.is_none() {
            return Ok(());
        }
        if let Some(mut q) = self.live(&c)? {
            let blocks = q.execute("query-block", json!({}))?;
            let blocks = blocks.as_array().context("invalid QMP query-block array")?;
            let cd = blocks
                .iter()
                .find(|b| {
                    b["device"] == "installer"
                        || b["qdev"].as_str().is_some_and(|p| p.ends_with("/cdrom"))
                })
                .context(
                    "installer CD drive not found in actual QMP block list; config unchanged",
                )?;
            ensure!(
                cd["removable"] == true,
                "identified installer is not removable; refusing eject"
            );
            let device = cd["device"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("CD device has no backend identifier")?;
            if cd.get("inserted").is_some() {
                q.execute("eject", json!({"device":device,"force":true}))
                    .context("QMP eject failed; config unchanged")?;
                let after = q.execute("query-block", json!({}))?;
                ensure!(
                    after
                        .as_array()
                        .context("invalid query-block after eject")?
                        .iter()
                        .any(|b| b["device"] == device && b.get("inserted").is_none()),
                    "QMP did not confirm empty CD; config unchanged"
                );
            }
        }
        c.iso = None;
        atomic_json(&self.dir(&c).join("config.json"), &c)?;
        self.audit("eject", &c, json!({"external_iso_preserved":true}));
        Ok(())
    }
    pub fn remove(&self, id_or_name: &str, delete_disk: bool) -> Result<()> {
        let _lock = self.lock()?;
        let c = self.resolve(id_or_name)?;
        ensure!(self.live(&c)?.is_none(), "stop the VM before removing it");
        self.validate_owned_dir(&c)?;
        if delete_disk {
            self.validate_disk(&c)?;
            reject_symlinks(&self.dir(&c))?;
        }
        atomic_json(
            &self.root.join("removed").join(format!("{}.json", c.id)),
            &json!({"config":c,"removed_at":unix_time(),"delete_disk_requested":delete_disk,"disk_retained":!delete_disk}),
        )?;
        self.audit("remove", &c, json!({"delete_disk":delete_disk}));
        if delete_disk {
            fs::remove_dir_all(self.dir(&c)).context("delete app-owned machine directory")?;
        } else {
            fs::remove_file(self.dir(&c).join("config.json"))?;
            File::open(self.dir(&c))?.sync_all()?;
        }
        File::open(self.root.join("machines"))?.sync_all()?;
        let runtime = self.runtime(&c)?;
        reject_symlinks(&runtime)?;
        fs::remove_dir_all(runtime)?;
        Ok(())
    }
    pub fn logs(&self, id_or_name: &str) -> Result<String> {
        let _lock = self.lock()?;
        let c = self.resolve(id_or_name)?;
        let mut text = String::new();
        for name in ["service.log", "qemu.log", "serial.log"] {
            match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(self.dir(&c).join(name))
            {
                Ok(mut f) => {
                    ensure!(f.metadata()?.is_file(), "log is not a regular file");
                    let size = f.metadata()?.len();
                    f.seek(SeekFrom::Start(size.saturating_sub(256 * 1024)))?;
                    let mut data = Vec::new();
                    f.take(256 * 1024).read_to_end(&mut data)?;
                    text.push_str(&format!(
                        "=== {name} (last 256 KiB) ===\n{}\n",
                        String::from_utf8_lossy(&data)
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(text)
    }
    pub fn console_path(&self, id_or_name: &str) -> Result<PathBuf> {
        let _lock = self.lock()?;
        let c = self.resolve(id_or_name)?;
        ensure!(
            self.live(&c)?.is_some(),
            "machine stopped; start before connecting serial console"
        );
        Ok(self.runtime(&c)?.join("serial.sock"))
    }
    pub fn open_display(&self, id_or_name: &str) -> Result<()> {
        let _lock = self.lock()?;
        let c = self.resolve(id_or_name)?;
        ensure!(
            self.live(&c)?.is_some(),
            "machine stopped; start to open QEMU's display"
        );
        let runtime = self.runtime(&c)?;
        let launch: Value = read_json(&runtime.join("launch.json"))?;
        ensure!(
            cfg!(target_os = "macos") && launch["display"] == "cocoa",
            "VM has no Cocoa display; stop and restart on macOS without GLIDE_DISPLAY=none"
        );
        let pid: u32 = fs::read_to_string(runtime.join("qemu.pid"))?
            .trim()
            .parse()
            .context("invalid QEMU pidfile")?;
        let script = "on run argv\ntell application \"System Events\"\nset frontmost of first application process whose unix id is (item 1 of argv as integer) to true\nend tell\nend run";
        checked_output(Command::new("/usr/bin/osascript").arg("-e").arg(script).arg(pid.to_string()), Duration::from_secs(5))
            .context("Cocoa window is created at VM start; foregrounding failed (macOS Automation/Accessibility permission may be required)")?;
        Ok(())
    }
    fn dir(&self, c: &Config) -> PathBuf {
        self.root.join("machines").join(&c.id)
    }
    fn configs(&self) -> Result<Vec<Config>> {
        let mut configs = Vec::new();
        for item in fs::read_dir(self.root.join("machines"))? {
            let item = item?;
            let id = item.file_name().to_string_lossy().to_string();
            ensure!(
                Uuid::parse_str(&id).is_ok(),
                "unexpected entry in machines: {id}"
            );
            ensure!(
                item.file_type()?.is_dir(),
                "machine directory not a real directory: {id}"
            );
            let path = item.path().join("config.json");
            if !path.try_exists()? {
                continue;
            }
            let c: Config = read_json(&path).with_context(|| format!("read {}", path.display()))?;
            ensure!(
                c.id == id && Uuid::parse_str(&c.id)?.to_string() == c.id,
                "config UUID does not match canonical directory UUID"
            );
            validate_name(&c.name)?;
            validate_resources(c.cpu, c.memory_mb, c.disk_gb)?;
            ensure!(
                normalize_arch(&c.architecture)? == c.architecture,
                "noncanonical architecture in config"
            );
            self.validate_owned_dir(&c)?;
            configs.push(c);
        }
        Ok(configs)
    }
    fn resolve(&self, query: &str) -> Result<Config> {
        let configs = self.configs()?;
        if let Some(c) = configs.iter().find(|c| c.id == query) {
            return Ok(c.clone());
        }
        let matches: Vec<_> = configs.into_iter().filter(|c| c.name == query).collect();
        ensure!(matches.len() <= 1, "ambiguous machine name; use full UUID");
        matches
            .into_iter()
            .next()
            .with_context(|| format!("machine not found: {query}"))
    }
    fn validate_owned_dir(&self, c: &Config) -> Result<()> {
        let dir = self.dir(c);
        let m = fs::symlink_metadata(&dir)?;
        ensure!(
            m.is_dir() && !m.file_type().is_symlink() && m.uid() == uid(),
            "machine directory not owned by this user or is a symlink"
        );
        ensure!(
            dir.canonicalize()? == dir,
            "machine directory escapes Glide root"
        );
        let owner: Value = read_json(&dir.join("owner.json"))?;
        ensure!(
            owner["id"] == c.id && owner["root"] == json!(self.root),
            "ownership marker mismatch; refusing disk operation"
        );
        Ok(())
    }
    fn validate_disk(&self, c: &Config) -> Result<()> {
        self.validate_owned_dir(c)?;
        let expected = self.dir(c).join("disk.qcow2");
        ensure!(
            c.disk == expected,
            "only app-owned disk.qcow2 permitted; external/physical disks never opened"
        );
        let meta = fs::symlink_metadata(&expected)?;
        ensure!(
            meta.is_file()
                && !meta.file_type().is_symlink()
                && meta.uid() == uid()
                && meta.nlink() == 1,
            "disk must be an owned regular file, not symlink, hardlink or device"
        );
        ensure!(
            expected.canonicalize()? == expected,
            "disk escapes machine directory"
        );
        let mut f = File::open(expected)?;
        let mut hdr = [0; 24];
        f.read_exact(&mut hdr)?;
        ensure!(&hdr[..4] == b"QFI\xfb", "disk is not qcow2");
        ensure!(
            u64::from_be_bytes(hdr[8..16].try_into()?) == 0,
            "external qcow2 backing files are not permitted"
        );
        let version = u32::from_be_bytes(hdr[4..8].try_into()?);
        ensure!(version == 2 || version == 3, "unsupported qcow2 version");
        if version == 3 {
            f.seek(SeekFrom::Start(72))?;
            let mut features = [0; 8];
            f.read_exact(&mut features)?;
            ensure!(
                u64::from_be_bytes(features) & (1 << 2) == 0,
                "qcow2 external data files are not permitted"
            );
        }
        Ok(())
    }
    fn runtime(&self, c: &Config) -> Result<PathBuf> {
        // Short deterministic path for macOS's 104-byte sockaddr_un, independent of GLIDE_HOME.
        let base = PathBuf::from(format!("/tmp/glide-{}", uid()));
        private_dir(&base)?;
        let hash = hex::encode(Sha256::digest(self.root.as_os_str().as_encoded_bytes()));
        let root = base.join(&hash[..12]);
        private_dir(&root)?;
        let dir = root.join(&c.id);
        private_dir(&dir)?;
        Ok(dir)
    }
    fn live(&self, c: &Config) -> Result<Option<qmp::Qmp>> {
        let path = self.runtime(c)?.join("qmp.sock");
        if let Ok(m) = fs::symlink_metadata(&path) {
            use std::os::unix::fs::FileTypeExt;
            ensure!(
                m.uid() == uid() && m.file_type().is_socket(),
                "QMP path not an owned socket"
            );
        }
        match qmp::Qmp::connect(&path, &c.id) {
            Ok(q) => Ok(Some(q)),
            Err(e) => {
                // Only absence/refusal is stopped; no timeout, protocol or permission error lies.
                let absent = e.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    )
                });
                if absent {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }
    fn append_log(&self, c: &Config, message: &str) -> Result<()> {
        let mut log = safe_append(&self.dir(c).join("service.log"))?;
        writeln!(log, "{} {message}", unix_time())?;
        log.flush()?;
        Ok(())
    }
    fn audit(&self, action: &str, c: &Config, detail: Value) {
        // Never invent installation state. JSON/QMP remain authoritative. The always-on
        // local audit is JSONL. NEDB is opt-in because 2.8.6 unconditionally prints startup
        // banners to stdout, which would otherwise corrupt CLI JSON output.
        let result = (|| -> Result<()> {
            let event = json!({"action":action,"machine":c,"at":unix_time(),"detail":detail});
            let mut log = safe_append(&self.root.join("audit.jsonl"))?;
            serde_json::to_writer(&mut log, &event)?;
            log.write_all(b"\n")?;
            log.flush()?;
            log.sync_all()?;
            if env::var("GLIDE_AUDIT").as_deref() != Ok("nedb") {
                return Ok(());
            }
            let db = nedb_engine::Db::open(&self.root.join("audit.nedb"), None)?;
            let previous = db
                .get("machines", &c.id)
                .map(|n| n.hash)
                .into_iter()
                .collect();
            let node = db.put(
                "events",
                &Uuid::new_v4().to_string(),
                event,
                previous,
                None,
                None,
            )?;
            db.put(
                "machines",
                &c.id,
                serde_json::to_value(c)?,
                vec![node.hash],
                None,
                None,
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("Glide audit warning: {e:#}");
            let _ = self.append_log(c, &format!("audit warning: {e:#}"));
        }
    }
}
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn normalize_arch(s: &str) -> Result<String> {
    match s.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok("x86_64".into()),
        "aarch64" | "arm64" => Ok("aarch64".into()),
        _ => bail!("unsupported architecture {s}; use x86_64 or aarch64"),
    }
}
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name.len() <= 80
            && !name
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\' || c == ','),
        "name must be 1–80 bytes, no controls/slash/backslash/comma"
    );
    Ok(())
}
fn validate_resources(cpu: u32, memory: u64, disk: u64) -> Result<()> {
    ensure!((1..=256).contains(&cpu), "CPU count must be 1–256");
    ensure!(
        (128..=1_048_576).contains(&memory),
        "memory must be 128–1048576 MiB"
    );
    ensure!(
        (1..=16_384).contains(&disk),
        "disk size must be 1–16384 GiB"
    );
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !path.try_exists()? {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink() && m.uid() == uid(),
        "{} must be a real directory owned by current user",
        path.display()
    );
    ensure!(
        m.permissions().mode() & 0o077 == 0,
        "{} must be private (chmod 700)",
        path.display()
    );
    Ok(())
}
fn safe_append(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    ensure!(
        f.metadata()?.is_file() && f.metadata()?.uid() == uid() && f.metadata()?.nlink() == 1,
        "unsafe log file: {}",
        path.display()
    );
    Ok(f)
}
fn regular_path(path: &Path, label: &str) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{label} missing: {}", path.display()))?;
    ensure!(
        path.metadata()?.is_file(),
        "{label} must be regular file, not physical disk/device"
    );
    qemu_path(&path)?;
    Ok(path)
}
fn qemu_path(path: &Path) -> Result<String> {
    let s = path.to_str().context("QEMU paths must be valid UTF-8")?;
    ensure!(
        !s.chars().any(|c| c.is_control()),
        "QEMU path contains control characters"
    );
    Ok(s.replace(',', ",,")) // QEMU keyval escaping, not shell quoting.
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    ensure!(
        f.metadata()?.is_file() && f.metadata()?.len() <= 1024 * 1024,
        "invalid/oversized JSON {}",
        path.display()
    );
    serde_json::from_reader(f).context("parse JSON")
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("JSON path has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn reject_existing_symlink(path: &Path) -> Result<()> {
    if let Ok(m) = fs::symlink_metadata(path) {
        ensure!(
            m.is_file() && !m.file_type().is_symlink() && m.uid() == uid() && m.nlink() == 1,
            "unsafe output path {}",
            path.display()
        );
    }
    Ok(())
}
fn reject_symlinks(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let typ = entry.file_type()?;
        ensure!(
            !typ.is_symlink(),
            "refusing destructive cleanup: symlink {}",
            entry.path().display()
        );
        if typ.is_dir() {
            reject_symlinks(&entry.path())?;
        }
    }
    Ok(())
}
fn executable(name: &str) -> Result<PathBuf> {
    if let Some(dir) = env::var_os("GLIDE_QEMU_DIR") {
        let path = PathBuf::from(dir).join(name);
        ensure!(
            is_executable(&path),
            "GLIDE_QEMU_DIR selected but {} not executable",
            path.display()
        );
        return Ok(path.canonicalize()?);
    }
    let mut dirs = vec![
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ];
    if let Some(path) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path));
    }
    for dir in dirs {
        let p = dir.join(name);
        if is_executable(&p) {
            return Ok(p.canonicalize()?);
        }
    }
    bail!("{name} not found. Install standalone QEMU (macOS: brew install qemu), or set GLIDE_QEMU_DIR. UTM helpers are not assumed runnable")
}
fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
fn backend(arch: &str) -> Result<Backend> {
    let arch = normalize_arch(arch)?;
    let accelerator = env::var("GLIDE_ACCEL").unwrap_or_else(|_| "hvf".into());
    ensure!(
        accelerator == "hvf" || accelerator == "tcg",
        "GLIDE_ACCEL must be hvf or tcg"
    );
    if accelerator == "hvf" {
        ensure!(cfg!(target_os = "macos"), "HVF requires macOS. For real Linux tests explicitly set GLIDE_ACCEL=tcg and GLIDE_DISPLAY=none");
        ensure!(
            arch == env::consts::ARCH,
            "HVF cannot cross architectures: host {}, VM {arch}",
            env::consts::ARCH
        );
    }
    let display = env::var("GLIDE_DISPLAY").unwrap_or_else(|_| "cocoa".into());
    ensure!(display == "none" || (display == "cocoa" && cfg!(target_os = "macos")), "graphical display requires macOS Cocoa; headless tests must explicitly set GLIDE_DISPLAY=none");
    let system = executable(&format!("qemu-system-{arch}"))?;
    let image = executable("qemu-img")?;
    let version = checked_output(
        Command::new(&system).arg("--version"),
        Duration::from_secs(5),
    )?;
    ensure!(
        String::from_utf8_lossy(&version).contains("QEMU"),
        "emulator does not report QEMU version"
    );
    checked_output(
        Command::new(&image).arg("--version"),
        Duration::from_secs(5),
    )?;
    let accelerators = checked_output(
        Command::new(&system).args(["-accel", "help"]),
        Duration::from_secs(5),
    )?;
    ensure!(
        String::from_utf8_lossy(&accelerators)
            .lines()
            .any(|l| l.trim() == accelerator),
        "QEMU build lacks {accelerator}"
    );
    let displays = checked_output(
        Command::new(&system).args(["-display", "help"]),
        Duration::from_secs(5),
    )?;
    ensure!(
        String::from_utf8_lossy(&displays)
            .lines()
            .any(|l| l.trim() == display),
        "QEMU build lacks display {display}"
    );
    let firmware = if arch == "aarch64" {
        let mut paths = Vec::new();
        if let Some(path) = env::var_os("GLIDE_AARCH64_FIRMWARE") {
            paths.push(PathBuf::from(path));
        } else {
            if let Some(prefix) = system.parent().and_then(Path::parent) {
                paths.push(prefix.join("share/qemu/edk2-aarch64-code.fd"));
            }
            for p in [
                "/usr/local/share/qemu/edk2-aarch64-code.fd",
                "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
                "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
                "/usr/share/AAVMF/AAVMF_CODE.fd",
            ] {
                paths.push(PathBuf::from(p));
            }
        }
        let firmware = paths.into_iter().find(|p| p.is_file()).context("ARM requires real UEFI firmware; install edk2-aarch64-code.fd or set GLIDE_AARCH64_FIRMWARE")?;
        Some(regular_path(&firmware, "ARM UEFI firmware")?)
    } else {
        None
    };
    Ok(Backend {
        system,
        image,
        accelerator,
        display,
        firmware,
    })
}
fn checked_output(cmd: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    // Regular temp files avoid deadlocks and descendants holding stdout pipes open.
    let mut out = tempfile::tempfile()?;
    let mut err = tempfile::tempfile()?;
    cmd.stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(err.try_clone()?);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("execute {:?}", cmd.get_program()))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("command {:?} timed out", cmd.get_program());
        }
        thread::sleep(Duration::from_millis(20));
    };
    out.seek(SeekFrom::Start(0))?;
    err.seek(SeekFrom::Start(0))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    out.take(1024 * 1024).read_to_end(&mut stdout)?;
    err.take(1024 * 1024).read_to_end(&mut stderr)?;
    ensure!(
        status.success(),
        "command {:?} failed ({status}): {}",
        cmd.get_program(),
        String::from_utf8_lossy(&stderr)
    );
    Ok(stdout)
}
