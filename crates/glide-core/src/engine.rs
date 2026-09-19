//! The Glide engine: orchestrates the VM lifecycle against a backend and
//! records every step in the lineage store.
//!
//! The ISO -> installed handoff lives here: create a VM (Created), install
//! from an ISO (Installing -> Installed, ISO detached), then run it
//! (Running <-> Stopped). Snapshots clone the disk and record provenance.

use crate::backend::{sha256_file, Backend, BackendError, Layout, Progress};
use crate::model::{Boot, Disk, IsoImage, Snapshot, Vm, VmState};
use crate::store::{new_vm, Store, StoreError};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("invalid state: {0}")]
    State(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// RFC 3339-ish UTC timestamp for lineage records.
fn now_rfc3339() -> String {
    let d = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        d.year(),
        d.month() as u8,
        d.day(),
        d.hour(),
        d.minute(),
        d.second()
    )
}

/// The engine. Owns the store and the layout; drives a backend.
pub struct Engine<B: Backend> {
    pub store: Store,
    pub layout: Layout,
    backend: B,
}

impl<B: Backend> Engine<B> {
    pub fn new(root: &Path, backend: B) -> Result<Self, EngineError> {
        let layout = Layout::new(root);
        std::fs::create_dir_all(layout.disks())?;
        std::fs::create_dir_all(layout.snapshots())?;
        let store = Store::open(&layout.store())?;
        Ok(Self {
            store,
            layout,
            backend,
        })
    }

    /// In-memory store + given layout root, for tests.
    pub fn in_memory_with(root: &Path, backend: B) -> Result<Self, EngineError> {
        std::fs::create_dir_all(root.join("disks"))?;
        std::fs::create_dir_all(root.join("snapshots"))?;
        Ok(Self {
            store: Store::in_memory()?,
            layout: Layout::new(root),
            backend,
        })
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Register an ISO image: checksum it and record it.
    pub fn register_iso(
        &mut self,
        path: &Path,
        label: Option<&str>,
    ) -> Result<IsoImage, EngineError> {
        let sha = sha256_file(path)?;
        let size = std::fs::metadata(path)?.len();
        let label = label
            .map(|s| s.to_string())
            .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "iso".into());
        let iso = IsoImage {
            sha256: sha,
            path: path.to_path_buf(),
            label,
            size_bytes: size,
        };
        self.store.record_iso(&iso)?;
        Ok(iso)
    }

    /// Create a new VM in `Created` state with an empty disk.
    pub fn create_vm(&mut self, name: &str, disk_gb: u64, mem_gb: u64) -> Result<Vm, EngineError> {
        if self.store.find_vm_by_name(name)?.is_some() {
            return Err(EngineError::State(format!(
                "vm named {name} already exists"
            )));
        }
        let mut vm = new_vm(name, disk_gb, mem_gb);
        let disk_path = self.layout.disks().join(format!("{}.disk", vm.id.as_str()));
        self.backend.create_disk(&disk_path, vm.config.disk_bytes)?;
        let disk = Disk {
            id: uuid::Uuid::new_v4().to_string(),
            path: disk_path,
            capacity_bytes: vm.config.disk_bytes,
            installed_from_iso: None,
        };
        self.store.record_disk(&disk, &vm, None)?;
        vm.disk = Some(disk);
        vm.state = VmState::Created;
        self.store.record_vm(&vm)?;
        Ok(vm)
    }

    /// Install from a registered ISO: the signature Glide transition.
    /// Created/Stopped -> Installing -> Installed, with the ISO detached on success.
    pub fn install_from_iso(
        &mut self,
        vm_name: &str,
        iso_sha: &str,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<Vm, EngineError> {
        let mut vm = self
            .store
            .find_vm_by_name(vm_name)?
            .ok_or_else(|| EngineError::NotFound(format!("vm {vm_name}")))?;
        if vm.state != VmState::Created && vm.state != VmState::Stopped {
            return Err(EngineError::State(format!(
                "cannot install while {}",
                vm.state.label()
            )));
        }
        let disk_path = vm
            .disk
            .as_ref()
            .ok_or_else(|| EngineError::State("vm has no disk".into()))?
            .path
            .clone();

        vm.state = VmState::Installing;
        vm.attached_iso = Some(iso_sha.to_string());
        self.store.record_vm(&vm)?;

        let disk_hash = vm.disk.as_ref().and_then(|d| self.store.disk_hash(&d.id));
        let boot = Boot {
            id: uuid::Uuid::new_v4().to_string(),
            vm: vm.id.clone(),
            disk_path: disk_path.clone(),
            iso: Some(iso_sha.to_string()),
            started_at: now_rfc3339(),
        };
        self.store.record_boot(&boot, disk_hash.as_deref())?;

        let iso_path = PathBuf::from(format!("iso:{iso_sha}"));
        match self
            .backend
            .boot(&vm, &disk_path, Some(&iso_path), on_progress)
        {
            Ok(()) => {
                vm.state = VmState::Installed;
                vm.attached_iso = None;
                vm.failure = None;
                if let Some(d) = vm.disk.as_mut() {
                    d.installed_from_iso = Some(iso_sha.to_string());
                }
                // Record after the mutable borrow of vm.disk has ended.
                let iso_hash = self.store.iso_hash(iso_sha);
                if let Some(d) = vm.disk.clone() {
                    self.store.record_disk(&d, &vm, iso_hash.as_deref())?;
                }
                self.store.record_vm(&vm)?;
                Ok(vm)
            }
            Err(e) => {
                vm.state = VmState::Failed;
                vm.attached_iso = None;
                vm.failure = Some(e.to_string());
                self.store.record_vm(&vm)?;
                Err(EngineError::Backend(e))
            }
        }
    }

    /// Boot an installed VM into Running.
    pub fn start(
        &mut self,
        vm_name: &str,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<Vm, EngineError> {
        let mut vm = self
            .store
            .find_vm_by_name(vm_name)?
            .ok_or_else(|| EngineError::NotFound(format!("vm {vm_name}")))?;
        if vm.state != VmState::Installed && vm.state != VmState::Stopped {
            return Err(EngineError::State(format!(
                "cannot start while {}",
                vm.state.label()
            )));
        }
        let disk_path = vm
            .disk
            .as_ref()
            .ok_or_else(|| EngineError::State("vm has no disk".into()))?
            .path
            .clone();
        let disk_hash = vm.disk.as_ref().and_then(|d| self.store.disk_hash(&d.id));
        let boot = Boot {
            id: uuid::Uuid::new_v4().to_string(),
            vm: vm.id.clone(),
            disk_path: disk_path.clone(),
            iso: None,
            started_at: now_rfc3339(),
        };
        self.store.record_boot(&boot, disk_hash.as_deref())?;
        self.backend.boot(&vm, &disk_path, None, on_progress)?;
        vm.state = VmState::Running;
        self.store.record_vm(&vm)?;
        Ok(vm)
    }

    /// Stop a running VM.
    pub fn stop(&mut self, vm_name: &str) -> Result<Vm, EngineError> {
        let mut vm = self
            .store
            .find_vm_by_name(vm_name)?
            .ok_or_else(|| EngineError::NotFound(format!("vm {vm_name}")))?;
        if vm.state != VmState::Running {
            return Err(EngineError::State(format!(
                "cannot stop while {}",
                vm.state.label()
            )));
        }
        self.backend.stop(&vm)?;
        vm.state = VmState::Stopped;
        self.store.record_vm(&vm)?;
        Ok(vm)
    }

    /// Snapshot a VM's current disk state. Records the lineage edge back to
    /// the disk (and through it, transitively, to the installing ISO).
    pub fn snapshot(&mut self, vm_name: &str, label: &str) -> Result<Snapshot, EngineError> {
        let vm = self
            .store
            .find_vm_by_name(vm_name)?
            .ok_or_else(|| EngineError::NotFound(format!("vm {vm_name}")))?;
        let disk = vm
            .disk
            .as_ref()
            .ok_or_else(|| EngineError::State("vm has no disk".into()))?;
        let snap_id = uuid::Uuid::new_v4().to_string();
        let snap_path = self.layout.snapshots().join(format!("{snap_id}.disk"));
        self.backend.clone_disk(&disk.path, &snap_path)?;

        let parent = self
            .store
            .snapshots_for(vm.id.as_str())?
            .last()
            .map(|s| s.id.clone());

        let snap = Snapshot {
            id: snap_id,
            vm: vm.id.clone(),
            label: label.to_string(),
            disk_path: snap_path,
            parent,
            created_at: now_rfc3339(),
        };
        // Lineage edge: branched from the current disk state.
        let disk_hash = self.store.disk_hash(&disk.id);
        self.store.record_snapshot(&snap, disk_hash.as_deref())?;
        Ok(snap)
    }

    /// Full lineage for a VM: vm -> disk -> installing ISO, then snapshots.
    pub fn trace(&mut self, vm_name: &str) -> Result<Vec<String>, EngineError> {
        let vm = self
            .store
            .find_vm_by_name(vm_name)?
            .ok_or_else(|| EngineError::NotFound(format!("vm {vm_name}")))?;
        let mut out = vec![format!("vm:{} ({})", vm.config.name, vm.id.as_str())];
        if let Some(d) = &vm.disk {
            out.push(format!("disk:{}", d.id));
            if let Some(iso) = &d.installed_from_iso {
                out.push(format!("installed-from-iso:{iso}"));
                if let Some(h) = self.store.iso_hash(iso) {
                    for line in self.store.trace_lineage(&h) {
                        out.push(format!("  {line}"));
                    }
                }
            }
        }
        for s in self.store.snapshots_for(vm.id.as_str())? {
            out.push(format!(
                "snapshot:{} \"{}\" parent={} at {}",
                s.id,
                s.label,
                s.parent.as_deref().unwrap_or("live-disk"),
                s.created_at
            ));
        }
        Ok(out)
    }

    pub fn list(&mut self) -> Result<Vec<Vm>, EngineError> {
        Ok(self.store.vms()?)
    }
}
