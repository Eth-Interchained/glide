//! The Glide GUI: a live view over the engine, built on Forge UI.
//!
//! Architecture: the engine runs on a background worker thread; the UI sends
//! commands over a channel and the worker reports progress back over another.
//! `Application::tick` drains the progress channel each frame and returns
//! true while work is in flight, so boot/install progress animates live
//! instead of freezing until the user moves the mouse.

use forge_ui::{Action, ActionKind, Application, Color, Node, Source, Theme};
use glide_core::backend::{Backend, Progress};
use glide_core::{Engine, Vm, VmState};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// A command from the UI to the engine worker.
#[derive(Debug)]
enum Cmd {
    Refresh,
    Create {
        name: String,
        disk_gb: u64,
        mem_gb: u64,
    },
    Install {
        name: String,
        iso_prefix: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    Snapshot {
        name: String,
        label: String,
    },
    Trace {
        name: String,
    },
}

/// A message from the engine worker back to the UI.
#[derive(Debug, Clone)]
enum Msg {
    /// Full refresh of the VM list.
    Vms(Vec<Vm>),
    /// A progress phase/console line for the in-flight op.
    Progress(String),
    /// An op finished; carries a status line.
    OpDone(String),
    /// An op failed.
    OpFailed(String),
    /// Trace output lines.
    Trace(Vec<String>),
}

/// Shared, mutable app state the view reads.
#[derive(Default)]
struct Shared {
    vms: Vec<Vm>,
    /// Recent progress lines for the in-flight / last op.
    log: Vec<String>,
    /// True while a long op is running.
    busy: bool,
    /// Last status line.
    status: String,
    /// Trace output for the selected VM.
    trace: Vec<String>,
    /// Create form fields.
    form_name: String,
    form_disk: String,
    form_mem: String,
    form_iso: String,
}

pub struct GlideApp {
    shared: Arc<Mutex<Shared>>,
    to_worker: Sender<Cmd>,
    from_worker: Receiver<Msg>,
    /// Set true on the first tick after a Msg that should trigger a repaint.
    dirty: bool,
}

impl GlideApp {
    /// Build the app around an engine running on its worker thread.
    pub fn spawn<B>(mut engine: Engine<B>) -> Self
    where
        B: Backend + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<Msg>();
        let shared = Arc::new(Mutex::new(Shared {
            form_disk: "64".into(),
            form_mem: "8".into(),
            status: "idle".into(),
            ..Default::default()
        }));

        // Engine worker thread: owns the engine, executes commands, reports.
        let shared_worker = shared.clone();
        std::thread::spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                let report = {
                    let msg_tx = msg_tx.clone();
                    move |p: Progress| {
                        let line = match p {
                            Progress::Working(s) => format!(".. {s}"),
                            Progress::ConsoleLine(s) => format!("| {s}"),
                            Progress::Done => ".. done".to_string(),
                        };
                        let _ = msg_tx.send(Msg::Progress(line));
                    }
                };
                let mut report = report;

                match cmd {
                    Cmd::Refresh => {
                        if let Ok(vms) = engine.list() {
                            let _ = msg_tx.send(Msg::Vms(vms));
                        }
                    }
                    Cmd::Create {
                        name,
                        disk_gb,
                        mem_gb,
                    } => {
                        match engine.create_vm(&name, disk_gb, mem_gb) {
                            Ok(vm) => {
                                let _ = msg_tx.send(Msg::OpDone(format!(
                                    "created {} [{}]",
                                    vm.config.name, vm.id
                                )));
                            }
                            Err(e) => {
                                let _ = msg_tx.send(Msg::OpFailed(format!("create: {e}")));
                            }
                        }
                        if let Ok(vms) = engine.list() {
                            let _ = msg_tx.send(Msg::Vms(vms));
                        }
                    }
                    Cmd::Install { name, iso_prefix } => {
                        let sha = match engine.store.isos() {
                            Ok(isos) => {
                                let m: Vec<_> = isos
                                    .iter()
                                    .filter(|i| i.sha256.starts_with(iso_prefix.as_str()))
                                    .collect();
                                if m.len() == 1 {
                                    Some(m[0].sha256.clone())
                                } else {
                                    None
                                }
                            }
                            Err(_) => None,
                        };
                        match sha {
                            Some(sha) => {
                                let r = engine.install_from_iso(&name, &sha, &mut report);
                                match r {
                                    Ok(vm) => {
                                        let _ = msg_tx.send(Msg::OpDone(format!(
                                            "installed {} — {}",
                                            vm.config.name,
                                            vm.state.label()
                                        )));
                                    }
                                    Err(e) => {
                                        let _ = msg_tx.send(Msg::OpFailed(format!("install: {e}")));
                                    }
                                }
                            }
                            None => {
                                let _ = msg_tx.send(Msg::OpFailed(format!(
                                    "no unique iso matching {iso_prefix}"
                                )));
                            }
                        }
                        if let Ok(vms) = engine.list() {
                            let _ = msg_tx.send(Msg::Vms(vms));
                        }
                    }
                    Cmd::Start { name } => {
                        match engine.start(&name, &mut report) {
                            Ok(vm) => {
                                let _ = msg_tx.send(Msg::OpDone(format!(
                                    "started {} — {}",
                                    vm.config.name,
                                    vm.state.label()
                                )));
                            }
                            Err(e) => {
                                let _ = msg_tx.send(Msg::OpFailed(format!("start: {e}")));
                            }
                        }
                        if let Ok(vms) = engine.list() {
                            let _ = msg_tx.send(Msg::Vms(vms));
                        }
                    }
                    Cmd::Stop { name } => {
                        match engine.stop(&name) {
                            Ok(vm) => {
                                let _ = msg_tx.send(Msg::OpDone(format!(
                                    "stopped {} — {}",
                                    vm.config.name,
                                    vm.state.label()
                                )));
                            }
                            Err(e) => {
                                let _ = msg_tx.send(Msg::OpFailed(format!("stop: {e}")));
                            }
                        }
                        if let Ok(vms) = engine.list() {
                            let _ = msg_tx.send(Msg::Vms(vms));
                        }
                    }
                    Cmd::Snapshot { name, label } => match engine.snapshot(&name, &label) {
                        Ok(s) => {
                            let _ = msg_tx.send(Msg::OpDone(format!(
                                "snapshot {} \"{}\"",
                                &s.id[..8.min(s.id.len())],
                                s.label
                            )));
                        }
                        Err(e) => {
                            let _ = msg_tx.send(Msg::OpFailed(format!("snapshot: {e}")));
                        }
                    },
                    Cmd::Trace { name } => match engine.trace(&name) {
                        Ok(lines) => {
                            let _ = msg_tx.send(Msg::Trace(lines));
                        }
                        Err(e) => {
                            let _ = msg_tx.send(Msg::OpFailed(format!("trace: {e}")));
                        }
                    },
                }

                // Flip busy off in shared state at the end of every command.
                if let Ok(mut s) = shared_worker.lock() {
                    s.busy = false;
                }
            }
        });

        // Kick an initial refresh.
        let _ = cmd_tx.send(Cmd::Refresh);

        Self {
            shared,
            to_worker: cmd_tx,
            from_worker: msg_rx,
            dirty: true,
        }
    }

    fn send(&self, cmd: Cmd) {
        if let Ok(mut s) = self.shared.lock() {
            s.busy = true;
        }
        let _ = self.to_worker.send(cmd);
    }

    fn state(&self) -> Shared {
        self.shared
            .lock()
            .map(|s| Shared {
                vms: s.vms.clone(),
                log: s.log.clone(),
                busy: s.busy,
                status: s.status.clone(),
                trace: s.trace.clone(),
                form_name: s.form_name.clone(),
                form_disk: s.form_disk.clone(),
                form_mem: s.form_mem.clone(),
                form_iso: s.form_iso.clone(),
            })
            .unwrap_or_default()
    }
}

impl Application for GlideApp {
    fn view(&self) -> Node {
        let s = self.state();
        let t = self.theme();

        // VM list rows.
        let mut rows: Vec<Node> = Vec::new();
        if s.vms.is_empty() {
            rows.push(
                Node::label("empty", "no VMs yet — create one below")
                    .color(t.muted)
                    .padding(8.0),
            );
        }
        for v in &s.vms {
            let disk = v
                .disk
                .as_ref()
                .map(|d| format!("{} GiB", d.capacity_bytes / (1024 * 1024 * 1024)))
                .unwrap_or_else(|| "no disk".into());
            rows.push(
                Node::row(
                    format!("row-{}", v.id),
                    vec![
                        Node::label(format!("name-{}", v.id), v.config.name.clone())
                            .bold()
                            .width(200.0),
                        Node::label(format!("state-{}", v.id), v.state.label())
                            .color(state_color(&t, v.state))
                            .width(110.0),
                        Node::label(format!("disk-{}", v.id), disk).width(80.0),
                        Node::button(format!("install-{}", v.config.name), "Install"),
                        Node::button(format!("start-{}", v.config.name), "Start"),
                        Node::button(format!("stop-{}", v.config.name), "Stop"),
                        Node::button(format!("snap-{}", v.config.name), "Snapshot"),
                        Node::button(format!("trace-{}", v.config.name), "Trace"),
                    ],
                )
                .gap(8.0)
                .padding(6.0)
                .border(),
            );
        }

        // Progress / status panel.
        let log_text = if s.log.is_empty() {
            s.status.clone()
        } else {
            s.log.join("\n")
        };
        let progress_panel = Node::column(
            "progress",
            vec![
                Node::label("progress-title", "Activity").bold(),
                Node::scroll(
                    "progress-log",
                    vec![Node::label("progress-log-text", log_text).font_size(13.0)],
                )
                .height(160.0)
                .border()
                .padding(8.0),
            ],
        )
        .gap(6.0)
        .padding(8.0);

        // Create form (also carries the ISO prefix used by Install).
        let form = Node::row(
            "create-form",
            vec![
                Node::text_input("in-name", "vm name", &s.form_name, "ubuntu-dev").width(160.0),
                Node::text_input("in-disk", "disk GB", &s.form_disk, "64").width(70.0),
                Node::text_input("in-mem", "mem GB", &s.form_mem, "8").width(70.0),
                Node::text_input("in-iso", "iso sha prefix", &s.form_iso, "").width(160.0),
                Node::button("btn-create", "Create VM"),
            ],
        )
        .gap(8.0)
        .padding(8.0)
        .border();

        // Trace panel.
        let trace_panel = Node::column(
            "trace",
            vec![
                Node::label("trace-title", "Lineage").bold(),
                Node::scroll(
                    "trace-body",
                    vec![Node::label(
                        "trace-text",
                        if s.trace.is_empty() {
                            "select Trace on a VM".to_string()
                        } else {
                            s.trace.join("\n")
                        },
                    )
                    .font_size(12.0)],
                )
                .height(140.0)
                .border()
                .padding(8.0),
            ],
        )
        .gap(6.0)
        .padding(8.0);

        Node::column(
            "root",
            vec![
                Node::label("title", "Glide")
                    .font_size(28.0)
                    .bold()
                    .padding(8.0),
                Node::label("subtitle", "a macOS VM manager with provable lineage")
                    .color(t.muted)
                    .padding(8.0),
                Node::scroll("vm-list", rows)
                    .fill_height()
                    .gap(4.0)
                    .padding(8.0),
                progress_panel,
                form,
                trace_panel,
            ],
        )
        .gap(4.0)
        .background(t.background)
    }

    fn update(&mut self, action: Action) {
        self.handle(action);
    }

    fn update_from(&mut self, action: Action, _source: Source) {
        self.handle(action);
    }

    fn theme(&self) -> Theme {
        Theme::DARK
    }

    fn tick(&mut self) -> bool {
        // Drain all pending worker messages.
        let mut got = false;
        while let Ok(msg) = self.from_worker.try_recv() {
            got = true;
            if let Ok(mut s) = self.shared.lock() {
                match msg {
                    Msg::Vms(vms) => s.vms = vms,
                    Msg::Progress(line) => {
                        s.log.push(line);
                        if s.log.len() > 500 {
                            let drop = s.log.len() - 500;
                            s.log.drain(0..drop);
                        }
                    }
                    Msg::OpDone(line) => {
                        s.log.push(format!("ok: {line}"));
                        s.status = line;
                        s.busy = false;
                    }
                    Msg::OpFailed(line) => {
                        s.log.push(format!("error: {line}"));
                        s.status = line;
                        s.busy = false;
                    }
                    Msg::Trace(lines) => s.trace = lines,
                }
            }
        }
        let busy = self.shared.lock().map(|s| s.busy).unwrap_or(false);
        // Keep animating while busy; also repaint once when we just got data.
        busy || got || self.dirty
    }
}

impl GlideApp {
    fn handle(&mut self, action: Action) {
        let id = action.id.as_str();
        match &action.kind {
            ActionKind::ChangeText(text) | ActionKind::Submit(text) => {
                if let Ok(mut s) = self.shared.lock() {
                    match id {
                        "in-name" => s.form_name = text.clone(),
                        "in-disk" => s.form_disk = text.clone(),
                        "in-mem" => s.form_mem = text.clone(),
                        "in-iso" => s.form_iso = text.clone(),
                        _ => {}
                    }
                }
            }
            ActionKind::Activate => {
                if id == "btn-create" {
                    let s = self.state();
                    let name = s.form_name.trim().to_string();
                    if name.is_empty() {
                        if let Ok(mut sh) = self.shared.lock() {
                            sh.status = "name the VM first".into();
                        }
                        return;
                    }
                    let disk_gb = s.form_disk.trim().parse::<u64>().unwrap_or(64);
                    let mem_gb = s.form_mem.trim().parse::<u64>().unwrap_or(8);
                    self.send(Cmd::Create {
                        name,
                        disk_gb,
                        mem_gb,
                    });
                } else if let Some(name) = id.strip_prefix("install-") {
                    let iso = self.state().form_iso.trim().to_string();
                    if iso.is_empty() {
                        if let Ok(mut sh) = self.shared.lock() {
                            sh.status = "enter an iso sha prefix in the form".into();
                        }
                        return;
                    }
                    self.send(Cmd::Install {
                        name: name.to_string(),
                        iso_prefix: iso,
                    });
                } else if let Some(name) = id.strip_prefix("start-") {
                    self.send(Cmd::Start {
                        name: name.to_string(),
                    });
                } else if let Some(name) = id.strip_prefix("stop-") {
                    self.send(Cmd::Stop {
                        name: name.to_string(),
                    });
                } else if let Some(name) = id.strip_prefix("snap-") {
                    self.send(Cmd::Snapshot {
                        name: name.to_string(),
                        label: "snapshot".into(),
                    });
                } else if let Some(name) = id.strip_prefix("trace-") {
                    self.send(Cmd::Trace {
                        name: name.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
}

fn state_color(t: &Theme, s: VmState) -> Color {
    match s {
        VmState::Running => t.success,
        VmState::Failed => Color::rgb(0xd0604a),
        VmState::Installing => Color::rgb(0xd0a24a),
        _ => t.muted,
    }
}
