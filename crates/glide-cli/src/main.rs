//! `glide` — the Glide VM manager CLI.
//!
//! Backend selection: on macOS the default is Virtualization.framework;
//! `--backend mock` drives the in-memory backend (used for tests and for
//! exercising the flow without a hypervisor). Off macOS the mock backend is
//! the only one available and is selected automatically with a note.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use glide_core::backend::{Backend, MockBackend, Progress};
use glide_core::Engine;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "glide",
    version,
    about = "Glide — a macOS VM manager with provable lineage"
)]
struct Cli {
    /// Backend to use: "vz" (macOS Virtualization.framework) or "mock".
    #[arg(long, global = true)]
    backend: Option<String>,

    /// Root directory for Glide's state (disks, snapshots, lineage db).
    #[arg(long, global = true, default_value = "~/.glide")]
    root: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Register an ISO installer image (checksums it into the lineage store).
    RegisterIso {
        /// Path to the .iso file.
        path: PathBuf,
        /// Optional human label.
        #[arg(long)]
        label: Option<String>,
    },
    /// Create a new VM (empty disk, state=created).
    Create {
        name: String,
        #[arg(long, default_value = "64")]
        disk_gb: u64,
        #[arg(long, default_value = "8")]
        mem_gb: u64,
    },
    /// Install a VM from a registered ISO (the ISO -> installed handoff).
    Install {
        name: String,
        /// sha256 of a registered ISO (or a unique prefix).
        #[arg(long)]
        iso: String,
    },
    /// Boot an installed VM.
    Start { name: String },
    /// Stop a running VM.
    Stop { name: String },
    /// Snapshot a VM's current disk state.
    Snapshot { name: String, label: String },
    /// Show a VM's full provenance: disk -> installing ISO -> snapshots.
    Trace { name: String },
    /// List VMs.
    Ls,
    /// List registered ISOs.
    Isos,
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

fn print_progress(p: Progress) {
    match p {
        Progress::Working(phase) => println!("  .. {phase}"),
        Progress::ConsoleLine(line) => println!("  | {line}"),
        Progress::Done => println!("  .. done"),
    }
}

fn run_with<B: Backend>(eng: &mut Engine<B>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::RegisterIso { path, label } => {
            let iso = eng
                .register_iso(path, label.as_deref())
                .with_context(|| format!("register iso {}", path.display()))?;
            println!(
                "registered {} ({:.1} GiB)",
                iso.label,
                iso.size_bytes as f64 / 1e9
            );
            println!("  sha256 {}", iso.sha256);
        }
        Cmd::Create {
            name,
            disk_gb,
            mem_gb,
        } => {
            let vm = eng.create_vm(name, *disk_gb, *mem_gb)?;
            println!(
                "created {} [{}] — state {}",
                vm.config.name,
                vm.id,
                vm.state.label()
            );
        }
        Cmd::Install { name, iso } => {
            // Resolve a unique sha256 prefix.
            let isos = eng.store.isos()?;
            let matches: Vec<_> = isos
                .iter()
                .filter(|i| i.sha256.starts_with(iso.as_str()))
                .collect();
            let sha = match matches.len() {
                1 => matches[0].sha256.clone(),
                0 => anyhow::bail!("no registered iso matches {iso}"),
                _ => anyhow::bail!("ambiguous iso prefix {iso} ({} matches)", matches.len()),
            };
            println!("installing {name} from iso {sha:.12}…");
            let vm = eng.install_from_iso(name, &sha, &mut print_progress)?;
            println!("installed {} — state {}", vm.config.name, vm.state.label());
        }
        Cmd::Start { name } => {
            let vm = eng.start(name, &mut print_progress)?;
            println!("started {} — state {}", vm.config.name, vm.state.label());
        }
        Cmd::Stop { name } => {
            let vm = eng.stop(name)?;
            println!("stopped {} — state {}", vm.config.name, vm.state.label());
        }
        Cmd::Snapshot { name, label } => {
            let s = eng.snapshot(name, label)?;
            println!(
                "snapshot {} \"{}\" at {}",
                &s.id[..8],
                s.label,
                s.created_at
            );
        }
        Cmd::Trace { name } => {
            for line in eng.trace(name)? {
                println!("{line}");
            }
        }
        Cmd::Ls => {
            let vms = eng.list()?;
            if vms.is_empty() {
                println!("no VMs");
            }
            for v in vms {
                let disk = v
                    .disk
                    .as_ref()
                    .map(|d| format!("{} GiB", d.capacity_bytes / (1024 * 1024 * 1024)))
                    .unwrap_or_else(|| "no disk".into());
                println!(
                    "{:<20} {:<11} {:<8} {} cpu  {}",
                    v.config.name,
                    v.state.label(),
                    disk,
                    v.config.cpu_count,
                    v.failure.as_deref().unwrap_or("")
                );
            }
        }
        Cmd::Isos => {
            let isos = eng.store.isos()?;
            if isos.is_empty() {
                println!("no ISOs registered");
            }
            for i in isos {
                println!(
                    "{:<32} {:.1} GiB  sha256 {}…",
                    i.label,
                    i.size_bytes as f64 / 1e9,
                    &i.sha256[..12]
                );
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = expand_tilde(&cli.root);

    let backend = cli
        .backend
        .as_deref()
        .unwrap_or(if cfg!(target_os = "macos") {
            "vz"
        } else {
            "mock"
        });

    match backend {
        "mock" => {
            let mut eng = Engine::new(&root, MockBackend::new())?;
            eprintln!("[backend: mock]");
            run_with(&mut eng, &cli.cmd)
        }
        "vz" => {
            #[cfg(target_os = "macos")]
            {
                let be = glide_vz::VzBackend::new()?;
                let mut eng = Engine::new(&root, be)?;
                eprintln!("[backend: {}]", eng.backend_name());
                run_with(&mut eng, &cli.cmd)
            }
            #[cfg(not(target_os = "macos"))]
            {
                anyhow::bail!("the vz backend requires macOS; use --backend mock here")
            }
        }
        other => anyhow::bail!("unknown backend {other:?} (expected vz or mock)"),
    }
}
