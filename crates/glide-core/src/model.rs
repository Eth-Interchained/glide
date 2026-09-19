//! Core domain model for Glide.
//!
//! Everything is a typed record. Provenance is first-class: a disk was
//! installed *from* an ISO, a snapshot was taken *from* a disk state, a boot
//! ran *against* a disk. Those edges are what NEDB stores and what
//! `TRACE caused_by` walks.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Stable identity for a VM.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VmId(pub String);

impl VmId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for VmId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for VmId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A registered ISO installer image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsoImage {
    /// Content checksum (sha256, hex) — the real identity of the image.
    pub sha256: String,
    /// Where the file lived when it was registered.
    pub path: PathBuf,
    /// Human label, e.g. "ubuntu-24.04-desktop-amd64".
    pub label: String,
    /// Size in bytes at registration time.
    pub size_bytes: u64,
}

/// A virtual disk owned by a VM. Copy-on-write clones are how snapshots work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disk {
    pub id: String,
    /// Path to the disk image file.
    pub path: PathBuf,
    /// Capacity in bytes.
    pub capacity_bytes: u64,
    /// Provenance: the ISO that installed this disk, if it was installed from one.
    pub installed_from_iso: Option<String>,
}

/// A point-in-time snapshot of a VM's disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub vm: VmId,
    pub label: String,
    /// Path to the cloned disk image capturing this state.
    pub disk_path: PathBuf,
    /// Provenance: the snapshot this was branched from (None = branched from live disk).
    pub parent: Option<String>,
    /// Wall-clock creation time, RFC 3339.
    pub created_at: String,
}

/// A single boot of a VM, recorded for lineage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boot {
    pub id: String,
    pub vm: VmId,
    /// The disk state the boot ran against.
    pub disk_path: PathBuf,
    /// The ISO attached for this boot, if any (installer boots).
    pub iso: Option<String>,
    pub started_at: String,
}

/// Desired hardware configuration for a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub name: String,
    pub cpu_count: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}

impl VmConfig {
    pub fn sane(name: impl Into<String>, disk_bytes: u64, memory_bytes: u64) -> Self {
        Self {
            name: name.into(),
            cpu_count: 4,
            memory_bytes,
            disk_bytes,
        }
    }
}

/// The lifecycle state of a VM. This *is* the product: Glide's whole job is
/// the smooth ISO -> installed handoff, and that handoff is a state
/// transition you can observe and prove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    /// Defined but no disk yet.
    Created,
    /// ISO attached, installer running.
    Installing,
    /// Install complete; ISO detached; installed system on disk.
    Installed,
    /// Installed system is running.
    Running,
    /// Cleanly stopped.
    Stopped,
    /// Something failed; holds a human-readable reason.
    Failed,
}

impl VmState {
    pub fn label(self) -> &'static str {
        match self {
            VmState::Created => "created",
            VmState::Installing => "installing",
            VmState::Installed => "installed",
            VmState::Running => "running",
            VmState::Stopped => "stopped",
            VmState::Failed => "failed",
        }
    }
}

/// A managed virtual machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vm {
    pub id: VmId,
    pub config: VmConfig,
    pub state: VmState,
    /// The VM's primary disk.
    pub disk: Option<Disk>,
    /// ISO currently attached (set while installing).
    pub attached_iso: Option<String>,
    /// If Failed, why.
    pub failure: Option<String>,
}

impl Vm {
    pub fn new(config: VmConfig) -> Self {
        Self {
            id: VmId::new(),
            config,
            state: VmState::Created,
            disk: None,
            attached_iso: None,
            failure: None,
        }
    }
}
