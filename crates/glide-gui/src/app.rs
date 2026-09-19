//! Real Service operations run on one worker; all presentation state belongs to the UI.
use forge_ui::{Action, ActionKind, Application, Color, Node, Theme};
use glide_core::{BackendInfo, CreateOptions, Machine, Service};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Duration;

#[derive(Debug)]
enum Command {
    Refresh,
    Browse,
    Create(CreateOptions),
    Start(String),
    Stop(String),
    Restart(String),
    Display(String),
    Eject(String),
    Console(String),
    Logs(String),
    Remove { id: String, delete_disk: bool },
}
struct Request {
    sequence: u64,
    command: Command,
}
#[derive(Default)]
struct Outcome {
    message: String,
    selected: Option<String>,
    iso: Option<PathBuf>,
    logs: Option<(String, String)>,
    error: Option<String>,
}
enum Message {
    Backend(BackendInfo),
    Machines(Result<Vec<Machine>, String>),
    Complete {
        sequence: u64,
        result: Result<Outcome, String>,
    },
    Fatal(String),
}
fn worker(root: PathBuf, requests: Receiver<Request>, replies: Sender<Message>) {
    let service = match Service::open(root.clone()) {
        Ok(service) => service,
        Err(error) => {
            let _ = replies.send(Message::Fatal(format!("Open VM library: {error:#}")));
            return;
        }
    };
    let _ = replies.send(Message::Backend(service.discover()));
    let publish = || {
        replies
            .send(Message::Machines(
                service
                    .list()
                    .map_err(|e| format!("Refresh VM states: {e:#}")),
            ))
            .is_ok()
    };
    if !publish() {
        return;
    }
    loop {
        match requests.recv_timeout(Duration::from_secs(1)) {
            Ok(request) => {
                if matches!(&request.command, Command::Refresh) {
                    let _ = replies.send(Message::Backend(service.discover()));
                }
                let result = execute_command(&service, &root, request.command);
                // Publish actual state BEFORE releasing pending controls. Never optimistic state.
                if !publish() {
                    break;
                }
                if replies
                    .send(Message::Complete {
                        sequence: request.sequence,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !publish() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
fn execute_command(service: &Service, root: &Path, command: Command) -> Result<Outcome, String> {
    let result: anyhow::Result<Outcome> = (|| {
        let mut out = Outcome::default();
        out.message = match command {
            Command::Refresh => "Library refreshed from the backend.".into(),
            Command::Browse => {
                out.iso = choose_iso()?;
                if out.iso.is_some() {
                    "ISO selected."
                } else {
                    "ISO selection cancelled."
                }
                .into()
            }
            Command::Create(options) => {
                let machine = service.create(options)?;
                out.selected = Some(machine.config.id.clone());
                match service.start(&machine.config.id) {
                    Ok(()) => format!(
                        "Boot requested for {}. State is reported by the backend.",
                        machine.config.name
                    ),
                    Err(error) => {
                        out.error = Some(format!("Created {} [{}], but boot failed: {error:#}. The VM remains in the library for retry.", machine.config.name, machine.config.id));
                        "VM created; boot failed. See the backend error below.".into()
                    }
                }
            }
            Command::Start(id) => {
                service.start(&id)?;
                "Start request completed; see current backend state.".into()
            }
            Command::Stop(id) => {
                service.stop(&id)?;
                "Shutdown requested. The guest may take time to stop.".into()
            }
            Command::Restart(id) => {
                service.restart(&id)?;
                "Restart request completed; see current backend state.".into()
            }
            Command::Display(id) => {
                service.open_display(&id)?;
                "Display viewer launch requested.".into()
            }
            Command::Eject(id) => {
                service.eject(&id)?;
                "Installer ISO ejected.".into()
            }
            Command::Console(id) => {
                let _ = service.console_path(&id)?;
                open_console(root, &id)?;
                "Terminal console launch requested. Serial output depends on the guest configuration.".into()
            }
            Command::Logs(id) => {
                let logs = service.logs(&id)?;
                out.logs = Some((id, logs));
                "Backend logs loaded.".into()
            }
            Command::Remove { id, delete_disk } => {
                let disk = service.status(&id)?.config.disk;
                service.remove(&id, delete_disk)?;
                if delete_disk {
                    format!(
                        "VM {id} removed and its managed disk deleted: {}",
                        disk.display()
                    )
                } else {
                    format!(
                        "VM {id} removed from the library. Disk retained: {}",
                        disk.display()
                    )
                }
            }
        };
        Ok(out)
    })();
    result.map_err(|e| format!("{e:#}"))
}
#[cfg(target_os = "macos")]
fn choose_iso() -> anyhow::Result<Option<PathBuf>> {
    // Constant script, no shell and no interpolation of user-controlled paths.
    let output = std::process::Command::new("/usr/bin/osascript").args(["-e", "try\nset chosen to choose file with prompt \"Choose a bootable installer ISO\"\nreturn POSIX path of chosen\non error number -128\nreturn \"\"\nend try"]).output()?;
    anyhow::ensure!(
        output.status.success(),
        "ISO picker: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8(output.stdout)?
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}
#[cfg(not(target_os = "macos"))]
fn choose_iso() -> anyhow::Result<Option<PathBuf>> {
    anyhow::bail!("Native ISO browsing requires macOS. Enter an absolute ISO path.")
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn console_command(cli: &Path, root: &Path, id: &str) -> anyhow::Result<String> {
    let cli = cli
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("CLI path is not valid UTF-8"))?;
    let root = root
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Library path is not valid UTF-8"))?;
    Ok(format!(
        "{} --root {} console {}",
        shell_quote(cli),
        shell_quote(root),
        shell_quote(id)
    ))
}
fn open_console(root: &Path, id: &str) -> anyhow::Result<()> {
    let cli = std::env::current_exe()?.with_file_name("glide");
    anyhow::ensure!(
        cli.is_file(),
        "CLI not found at {}. Install glide next to glide-gui to open a console.",
        cli.display()
    );
    let command = console_command(&cli, root, id)?;
    #[cfg(target_os = "macos")]
    {
        // Shell command travels as argv, not AppleScript source; every shell argument is quoted.
        let output = std::process::Command::new("/usr/bin/osascript").args(["-e", "on run argv\ntell application \"Terminal\"\nactivate\ndo script (item 1 of argv)\nend tell\nend run", "--", &command]).output()?;
        anyhow::ensure!(
            output.status.success(),
            "Open Terminal: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("Automatic Terminal launch requires macOS. Run in a terminal: {command}")
    }
}
#[derive(Clone)]
struct Removal {
    id: String,
    name: String,
    disk: PathBuf,
    delete_disk: bool,
}
struct Form {
    name: String,
    iso: String,
    architecture: String,
    cpu: String,
    memory: String,
    disk: String,
}
impl Default for Form {
    fn default() -> Self {
        Self {
            name: String::new(),
            iso: String::new(),
            architecture: std::env::consts::ARCH.into(),
            cpu: "4".into(),
            memory: "8192".into(),
            disk: "64".into(),
        }
    }
}
impl Form {
    fn options(&self) -> Result<CreateOptions, String> {
        if self.name.trim().is_empty() {
            return Err("Enter a VM name.".into());
        }
        if self.iso.trim().is_empty() {
            return Err("Choose or enter a bootable ISO path.".into());
        }
        let architecture = match self.architecture.trim() {
            "x86_64" | "amd64" => "x86_64",
            "aarch64" | "arm64" => "aarch64",
            _ => {
                return Err(
                    "Architecture must be x86_64 or aarch64. Match the installer ISO.".into(),
                )
            }
        };
        let cpu = self
            .cpu
            .parse::<u32>()
            .map_err(|_| "CPU must be a whole number.")?;
        let memory_mb = self
            .memory
            .parse::<u64>()
            .map_err(|_| "Memory must be a whole number of MiB.")?;
        let disk_gb = self
            .disk
            .parse::<u64>()
            .map_err(|_| "Disk must be a whole number of GiB.")?;
        if cpu == 0 || memory_mb == 0 || disk_gb == 0 {
            return Err("CPU, memory and disk must be greater than zero.".into());
        }
        Ok(CreateOptions {
            name: self.name.trim().into(),
            iso: PathBuf::from(&self.iso),
            architecture: Some(architecture.into()),
            cpu,
            memory_mb,
            disk_gb,
        })
    }
}
pub struct GlideApp {
    root: PathBuf,
    machines: Vec<Machine>,
    backend: Option<BackendInfo>,
    selected: Option<String>,
    form: Form,
    creating: bool,
    removal: Option<Removal>,
    pending: Option<(u64, String)>,
    next_sequence: u64,
    connected: bool,
    fresh: bool,
    status: String,
    error: Option<String>,
    logs: Option<(String, String)>,
    to_worker: Sender<Request>,
    from_worker: Receiver<Message>,
    cached_view: Node,
}
impl GlideApp {
    pub fn spawn(root: PathBuf) -> Self {
        let (tx, requests) = channel();
        let (replies, rx) = channel();
        let worker_root = root.clone();
        std::thread::spawn(move || worker(worker_root, requests, replies));
        Self::new(root, tx, rx)
    }
    fn new(root: PathBuf, to_worker: Sender<Request>, from_worker: Receiver<Message>) -> Self {
        let mut app = Self {
            root,
            machines: Vec::new(),
            backend: None,
            selected: None,
            form: Form::default(),
            creating: false,
            removal: None,
            pending: None,
            next_sequence: 1,
            connected: true,
            fresh: false,
            status: "Discovering backend and reading VM library…".into(),
            error: None,
            logs: None,
            to_worker,
            from_worker,
            cached_view: Node::column("initial", vec![]),
        };
        app.rebuild();
        app
    }
    fn machine(&self) -> Option<&Machine> {
        self.selected
            .as_ref()
            .and_then(|id| self.machines.iter().find(|m| &m.config.id == id))
    }
    fn ready(&self) -> bool {
        self.connected && self.pending.is_none()
    }
    fn available(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.available)
    }
    fn dispatch(&mut self, command: Command, label: &str) {
        if !self.ready() {
            return;
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        // Mark pending synchronously. A fast worker cannot outrun this state change.
        self.pending = Some((sequence, label.into()));
        self.error = None;
        self.status = format!("{label}…");
        if self.to_worker.send(Request { sequence, command }).is_err() {
            self.pending = None;
            self.connected = false;
            self.error = Some("Backend worker disconnected. Close and reopen Glide.".into());
        }
    }
    fn receive(&mut self, message: Message) {
        match message {
            Message::Backend(info) => {
                self.status = if info.available {
                    "Backend ready. Select a VM or create one from an ISO.".into()
                } else {
                    "Backend unavailable. No simulated backend will be used.".into()
                };
                self.backend = Some(info);
            }
            Message::Machines(result) => match result {
                Ok(machines) => {
                    self.machines = machines;
                    self.fresh = true;
                    if self
                        .selected
                        .as_ref()
                        .is_some_and(|id| !self.machines.iter().any(|m| &m.config.id == id))
                    {
                        self.selected = None;
                    }
                    if self.selected.is_none() && !self.creating {
                        self.selected = self.machines.first().map(|m| m.config.id.clone());
                    }
                }
                Err(error) => {
                    self.fresh = false;
                    self.error = Some(error);
                }
            },
            Message::Complete { sequence, result } => {
                if self.pending.as_ref().map(|p| p.0) != Some(sequence) {
                    return;
                }
                self.pending = None;
                match result {
                    Ok(out) => {
                        if let Some(id) = out.selected {
                            self.selected = Some(id);
                            self.creating = false;
                        }
                        if let Some(iso) = out.iso {
                            self.form.iso = iso.to_string_lossy().into_owned();
                        }
                        if let Some(logs) = out.logs {
                            self.logs = Some(logs);
                        }
                        if let Some(error) = out.error {
                            self.error = Some(error);
                        }
                        self.status = if self.fresh {
                            out.message
                        } else {
                            format!(
                                "{} Library refresh failed; current VM states are unknown.",
                                out.message
                            )
                        };
                    }
                    Err(error) => {
                        self.status = "Operation failed. See the backend error below.".into();
                        self.error = Some(error);
                    }
                }
            }
            Message::Fatal(error) => {
                self.error = Some(error);
                self.connected = false;
                self.pending = None;
                self.fresh = false;
            }
        }
    }
    fn rebuild(&mut self) {
        self.cached_view = self.build_view();
    }
    fn build_view(&self) -> Node {
        let ready = self.ready();
        let mut library = vec![
            Node::label("library-title", "Virtual machines")
                .bold()
                .font_size(17.0),
            button("new", "New from ISO", 238.0, ready),
            button("refresh", "Refresh library", 238.0, ready),
        ];
        if self.machines.is_empty() {
            library.extend(text_lines("empty-library", "No virtual machines yet.", 29));
        }
        for machine in &self.machines {
            let selected = self.selected.as_ref() == Some(&machine.config.id) && !self.creating;
            library.push(
                button(
                    &format!("select:{}", machine.config.id),
                    &shorten(&machine.config.name, 26),
                    238.0,
                    ready,
                )
                .background(Color(if selected { 0xdce8f5 } else { 0xf0f2f5 }))
                .color(Color(0x202a35)),
            );
            library.push(
                Node::label(
                    format!("state:{}", machine.config.id),
                    format!("{} · {}", machine.state, machine.config.architecture),
                )
                .font_size(12.0)
                .color(Color(0x5a6673)),
            );
        }
        let body = if let Some(removal) = &self.removal {
            self.removal_view(removal)
        } else if self.creating {
            self.create_view()
        } else {
            self.detail_view()
        };
        let mut backend = vec![Node::label(
            "host",
            format!(
                "Host: {} / {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        )
        .font_size(12.0)];
        if let Some(info) = &self.backend {
            backend.extend(text_lines(
                "backend-detail",
                &format!(
                    "{} · {} · {} · {}",
                    if info.available {
                        "Available"
                    } else {
                        "Unavailable"
                    },
                    info.architecture,
                    info.accelerator,
                    info.detail
                ),
                136,
            ));
        } else {
            backend.push(Node::label(
                "discovery",
                "Discovering the real virtualization backend…",
            ));
        }
        let mut activity = text_lines("status-line", &self.status, 137);
        if let Some(error) = &self.error {
            activity.extend(
                text_lines("error-line", error, 130)
                    .into_iter()
                    .map(|n| n.color(Color(0xa62929))),
            );
        }
        if let Some((id, logs)) = &self.logs {
            activity.push(Node::label("logs-title", format!("Backend logs · {id}")).bold());
            activity.extend(text_lines(
                "logs-line",
                if logs.is_empty() {
                    "The backend log is empty."
                } else {
                    logs
                },
                130,
            ));
        }
        Node::column(
            "root",
            vec![
                Node::row(
                    "header",
                    vec![
                        Node::label("title", "Glide")
                            .font_size(28.0)
                            .bold()
                            .width(120.0),
                        Node::label("subtitle", "Virtual machine manager").font_size(17.0),
                    ],
                )
                .height(38.0)
                .gap(12.0),
                Node::scroll("backend", backend).gap(3.0).height(64.0),
                Node::row(
                    "workspace",
                    vec![
                        Node::scroll("library", library)
                            .width(270.0)
                            .fill_height()
                            .padding(16.0)
                            .gap(10.0)
                            .background(Color(0xffffff))
                            .border(),
                        Node::scroll("detail", body)
                            .fill_height()
                            .padding(20.0)
                            .gap(12.0)
                            .background(Color(0xffffff))
                            .border(),
                    ],
                )
                .gap(16.0)
                .fill_height(),
                Node::label("activity-heading", "Activity & backend errors").bold(),
                Node::scroll("activity", activity)
                    .height(128.0)
                    .padding(12.0)
                    .gap(4.0)
                    .background(Color(0xffffff))
                    .border(),
                Node::label(
                    "root-path",
                    format!(
                        "Library: {}  ·  Backend state checked every second",
                        self.root.display()
                    ),
                )
                .font_size(11.0)
                .color(Color(0x5a6673)),
            ],
        )
        .padding(24.0)
        .gap(12.0)
        .fill_height()
    }
    fn create_view(&self) -> Vec<Node> {
        let editable = self.ready();
        vec![
            Node::label("create-title", "Create a virtual machine")
                .font_size(22.0)
                .bold(),
            Node::label(
                "create-hint",
                "Boot an installer ISO on a new persistent disk.",
            )
            .color(Color(0x5a6673)),
            Node::row(
                "identity-fields",
                vec![
                    field(
                        "name",
                        "Name",
                        &self.form.name,
                        "e.g. Debian development",
                        editable,
                    ),
                    field(
                        "architecture",
                        "Guest architecture",
                        &self.form.architecture,
                        "x86_64 or aarch64",
                        editable,
                    )
                    .width(210.0),
                ],
            )
            .gap(16.0),
            Node::row(
                "iso-row",
                vec![
                    field(
                        "iso",
                        "ISO path",
                        &self.form.iso,
                        "/path/to/installer.iso",
                        editable,
                    ),
                    button(
                        "browse",
                        "Browse ISO",
                        126.0,
                        editable && cfg!(target_os = "macos"),
                    ),
                ],
            )
            .gap(12.0),
            Node::label(
                "arch-help",
                "Defaults to host architecture, not ISO detection. Match your installer.",
            )
            .font_size(12.0)
            .color(Color(0x5a6673)),
            Node::row(
                "resources",
                vec![
                    field("cpu", "CPUs", &self.form.cpu, "4", editable).width(125.0),
                    field(
                        "memory",
                        "Memory (MiB)",
                        &self.form.memory,
                        "8192",
                        editable,
                    )
                    .width(195.0),
                    field("disk", "Disk (GiB)", &self.form.disk, "64", editable).width(180.0),
                ],
            )
            .gap(16.0),
            Node::row(
                "create-buttons",
                vec![
                    button(
                        "create",
                        "Create and Boot",
                        172.0,
                        editable && self.available(),
                    ),
                    button("cancel-create", "Cancel", 100.0, editable),
                ],
            )
            .gap(12.0),
        ]
    }
    fn detail_view(&self) -> Vec<Node> {
        let Some(machine) = self.machine() else {
            return vec![
                Node::label("welcome", "Your VM library")
                    .font_size(22.0)
                    .bold(),
                Node::label(
                    "welcome-hint",
                    "Choose New from ISO to create and boot a virtual machine.",
                ),
                Node::label(
                    "welcome-real",
                    "Existing VMs use the same library as the Glide CLI.",
                )
                .color(Color(0x5a6673)),
            ];
        };
        let c = &machine.config;
        let ready = self.ready() && self.fresh;
        let running = machine.state == "running";
        let stopped = machine.state == "stopped";
        let mut nodes = vec![
            Node::label("vm-name", shorten(&c.name, 58))
                .font_size(22.0)
                .bold(),
            Node::label(
                "vm-state",
                format!(
                    "State: {}{}",
                    machine.state,
                    if self.fresh {
                        ""
                    } else {
                        " (last known; refresh failed)"
                    }
                ),
            )
            .bold(),
            Node::label("vm-id", format!("ID: {}", c.id)).font_size(12.0),
            Node::label(
                "vm-resources",
                format!(
                    "{}  ·  {} CPUs  ·  {} MiB memory  ·  {} GiB virtual disk",
                    c.architecture, c.cpu, c.memory_mb, c.disk_gb
                ),
            ),
        ];
        nodes.extend(text_lines(
            "disk-path",
            &format!("Disk: {}", c.disk.display()),
            85,
        ));
        nodes.extend(text_lines(
            "iso-path",
            &format!(
                "Installer: {}",
                c.iso
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "None — boot from disk".into())
            ),
            85,
        ));
        nodes.push(
            Node::row(
                "power",
                vec![
                    button("start", "Start", 95.0, ready && stopped && self.available()),
                    button("stop", "Stop", 95.0, ready && running),
                    button(
                        "restart",
                        "Restart",
                        100.0,
                        ready && running && self.available(),
                    ),
                    button("display", "Open Display", 142.0, ready && running),
                ],
            )
            .gap(12.0),
        );
        nodes.push(
            Node::row(
                "tools",
                vec![
                    button(
                        "eject",
                        "Eject ISO",
                        115.0,
                        ready && c.iso.is_some() && (running || stopped),
                    ),
                    button("console", "Open Console", 142.0, ready && running),
                    button("logs", "View Logs", 115.0, ready),
                    button("refresh-detail", "Refresh", 100.0, self.ready()),
                ],
            )
            .gap(12.0),
        );
        nodes.push(
            Node::label(
                "stop-note",
                "Stop asks the guest to shut down; watch its actual state before removing.",
            )
            .font_size(12.0)
            .color(Color(0x5a6673)),
        );
        nodes.push(
            Node::row(
                "remove-buttons",
                vec![
                    button("remove", "Remove (keep disk)", 194.0, ready && stopped),
                    button("delete", "Delete disk…", 142.0, ready && stopped)
                        .color(Color(0xa62929)),
                ],
            )
            .gap(12.0),
        );
        nodes
    }
    fn removal_view(&self, removal: &Removal) -> Vec<Node> {
        let mut nodes = vec![Node::label(
            "remove-heading",
            if removal.delete_disk {
                "Permanently delete this VM and disk?"
            } else {
                "Remove this VM from the library?"
            },
        )
        .font_size(22.0)
        .bold()];
        nodes.extend(text_lines(
            "remove-name",
            &format!("VM: {}", removal.name),
            82,
        ));
        nodes.push(Node::label("remove-id", format!("Exact ID: {}", removal.id)).font_size(12.0));
        nodes.extend(text_lines(
            "remove-disk",
            &format!("Disk: {}", removal.disk.display()),
            82,
        ));
        nodes.extend(text_lines("remove-warning", if removal.delete_disk { "This deletes the managed disk and its contents. This cannot be undone. The installer ISO is not deleted." } else { "Only the VM registration is removed. The disk stays at the path above; keep this path if you need its contents." }, 82));
        let stopped = self.fresh
            && self
                .machines
                .iter()
                .any(|m| m.config.id == removal.id && m.state == "stopped");
        nodes.push(
            Node::row(
                "remove-confirmation",
                vec![
                    button("cancel-remove", "Cancel — keep VM", 195.0, self.ready()),
                    button(
                        "confirm-remove",
                        if removal.delete_disk {
                            "Delete VM and disk"
                        } else {
                            "Remove, keep disk"
                        },
                        205.0,
                        self.ready() && stopped,
                    )
                    .color(Color(0xa62929)),
                ],
            )
            .gap(16.0),
        );
        nodes
    }
}
fn button(id: &str, label: &str, width: f32, enabled: bool) -> Node {
    let n = Node::button(id, label).width(width).height(40.0);
    if enabled {
        n
    } else {
        n.disabled()
    }
}
fn field(id: &str, label: &str, value: &str, hint: &str, enabled: bool) -> Node {
    let input = Node::text_input(id, label, value, hint);
    Node::column(
        format!("field-{id}"),
        vec![
            Node::label(format!("label-{id}"), label)
                .font_size(12.0)
                .bold(),
            if enabled { input } else { input.disabled() },
        ],
    )
    .gap(5.0)
}
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.into()
    } else {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    }
}
// Forge labels are single-line; explicit Unicode-safe wrapping keeps paths/errors readable.
fn text_lines(prefix: &str, text: &str, columns: usize) -> Vec<Node> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            lines.push(String::new());
        }
        for chunk in chars.chunks(columns) {
            lines.push(chunk.iter().collect::<String>());
        }
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| Node::label(format!("{prefix}-{i}"), line).font_size(12.0))
        .collect()
}
impl Application for GlideApp {
    fn theme(&self) -> Theme {
        Theme {
            background: Color(0xf3f5f7),
            panel: Color(0xffffff),
            elevated: Color(0xe9edf2),
            accent: Color(0x2b5f91),
            on_accent: Color(0xffffff),
            ..Theme::LIGHT
        }
    }
    fn view(&self) -> Node {
        self.cached_view.clone()
    }
    fn tick(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.from_worker.try_recv() {
                Ok(message) => {
                    self.receive(message);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.connected {
                        self.connected = false;
                        self.pending = None;
                        self.fresh = false;
                        if self.error.is_none() {
                            self.error =
                                Some("Backend worker stopped. Close and reopen Glide.".into());
                        }
                        changed = true;
                    }
                    break;
                }
            }
        }
        if changed {
            self.rebuild();
        }
        // Forge 0.2 has no external wake handle. Keep its timer alive to receive data
        // without input; no IO, locks or tree construction on idle ticks.
        self.connected
    }
    fn update(&mut self, action: Action) {
        if !self.ready() {
            return;
        }
        if let ActionKind::ChangeText(value) = action.kind {
            if self.creating {
                match action.id.as_str() {
                    "name" => self.form.name = value,
                    "iso" => self.form.iso = value,
                    "architecture" => self.form.architecture = value,
                    "cpu" => self.form.cpu = value,
                    "memory" => self.form.memory = value,
                    "disk" => self.form.disk = value,
                    _ => {}
                }
            }
            self.rebuild();
            return;
        }
        if action.kind != ActionKind::Activate {
            return;
        }
        if let Some(id) = action.id.strip_prefix("select:") {
            if self.machines.iter().any(|m| m.config.id == id) {
                self.selected = Some(id.into());
                self.creating = false;
                self.removal = None;
                self.logs = None;
            }
        } else {
            match action.id.as_str() {
                "new" => {
                    self.creating = true;
                    self.removal = None;
                }
                "cancel-create" => self.creating = false,
                "browse" if self.creating => self.dispatch(Command::Browse, "Choosing ISO"),
                "create" if self.creating && self.available() => match self.form.options() {
                    Ok(options) => {
                        self.dispatch(Command::Create(options), "Creating VM and requesting boot")
                    }
                    Err(error) => self.error = Some(error),
                },
                "refresh" | "refresh-detail" => {
                    self.dispatch(Command::Refresh, "Refreshing library")
                }
                "cancel-remove" => self.removal = None,
                "confirm-remove" => {
                    if let Some(removal) = self.removal.clone() {
                        if self.fresh
                            && self
                                .machines
                                .iter()
                                .any(|m| m.config.id == removal.id && m.state == "stopped")
                        {
                            self.removal = None;
                            self.dispatch(
                                Command::Remove {
                                    id: removal.id,
                                    delete_disk: removal.delete_disk,
                                },
                                if removal.delete_disk {
                                    "Deleting VM and disk"
                                } else {
                                    "Removing VM; retaining disk"
                                },
                            );
                        } else {
                            self.error = Some("Removal cancelled: this VM is no longer confirmed stopped. Refresh and review again.".into());
                            self.removal = None;
                        }
                    }
                }
                id => {
                    if let Some(machine) = self.machine().cloned() {
                        let target = machine.config.id.clone();
                        let running = self.fresh && machine.state == "running";
                        let stopped = self.fresh && machine.state == "stopped";
                        match id {
                            "start" if stopped && self.available() => {
                                self.dispatch(Command::Start(target), "Starting VM")
                            }
                            "stop" if running => {
                                self.dispatch(Command::Stop(target), "Requesting shutdown")
                            }
                            "restart" if running && self.available() => {
                                self.dispatch(Command::Restart(target), "Restarting VM")
                            }
                            "display" if running => {
                                self.dispatch(Command::Display(target), "Opening display")
                            }
                            "console" if running => {
                                self.dispatch(Command::Console(target), "Opening serial console")
                            }
                            "eject" if (running || stopped) && machine.config.iso.is_some() => {
                                self.dispatch(Command::Eject(target), "Ejecting installer ISO")
                            }
                            "logs" if self.fresh => {
                                self.dispatch(Command::Logs(target), "Reading backend logs")
                            }
                            "remove" | "delete" if stopped => {
                                self.removal = Some(Removal {
                                    id: target,
                                    name: machine.config.name,
                                    disk: machine.config.disk,
                                    delete_disk: id == "delete",
                                })
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        self.rebuild();
    }
}
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
