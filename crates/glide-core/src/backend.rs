//! The backend seam.
//!
//! `Backend` is the boundary between the engine (pure, portable, testable)
//! and a hypervisor. The real implementation is `glide-vz` on macOS
//! (Virtualization.framework). A `MockBackend` here drives the full
//! ISO -> installed -> snapshot -> boot flow in-memory so the state machine
//! and the lineage recording are testable on Linux CI without a hypervisor.
//!
//! The trait exists because there are two real backends in the product's
//! future (VZ now, QEMU possibly later) — not as speculative abstraction.
//! It is deliberately narrow: only what the engine actually calls.

use crate::model::Vm;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("hypervisor unavailable: {0}")]
    Unavailable(String),
    #[error("vm operation failed: {0}")]
    Op(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Progress signal emitted while a long operation (install, boot) runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Indeterminate activity with a human-readable phase label.
    Working(String),
    /// The VM's guest produced a line of console output.
    ConsoleLine(String),
    /// The operation finished.
    Done,
}

/// A hypervisor backend.
pub trait Backend {
    /// Human name, e.g. "virtualization-framework" or "mock".
    fn name(&self) -> &'static str;

    /// Create an empty disk image of `bytes` capacity at `path`.
    fn create_disk(&self, path: &Path, bytes: u64) -> Result<(), BackendError>;

    /// Clone `from` to `to` copy-on-write (APFS clonefile on macOS; a full
    /// copy is an acceptable fallback). Must be cheap on the real backend.
    fn clone_disk(&self, from: &Path, to: &Path) -> Result<(), BackendError>;

    /// Boot `vm` with `disk`, optionally with an installer `iso` attached.
    /// Calls `on_progress` as work proceeds. Blocks until the boot/install
    /// reaches a terminal point (installer exits, or VM is shut down).
    fn boot(
        &mut self,
        vm: &Vm,
        disk: &Path,
        iso: Option<&Path>,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<(), BackendError>;

    /// Request a running VM to stop.
    fn stop(&mut self, vm: &Vm) -> Result<(), BackendError>;
}

/// In-memory backend that simulates the whole flow. Deterministic and fast.
///
/// It writes real (tiny) files for disks so paths, clones, and existence
/// checks behave like the real thing, but no hypervisor is involved.
#[derive(Debug, Default)]
pub struct MockBackend {
    /// If set, the next `boot` with an ISO attached returns this error.
    pub fail_next_install: Option<String>,
    /// Console lines to emit during a boot.
    pub console: Vec<String>,
    /// Recorded stop calls, for assertions.
    pub stopped: Vec<String>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate a failing installer on the next install boot.
    pub fn fail_installs_with(&mut self, reason: impl Into<String>) {
        self.fail_next_install = Some(reason.into());
    }
}

impl Backend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn create_disk(&self, path: &Path, bytes: u64) -> Result<(), BackendError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Sparse-ish: just record the requested capacity as the file content.
        std::fs::write(path, format!("MOCKDISK capacity={bytes}\n"))?;
        Ok(())
    }

    fn clone_disk(&self, from: &Path, to: &Path) -> Result<(), BackendError> {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to)?;
        Ok(())
    }

    fn boot(
        &mut self,
        vm: &Vm,
        _disk: &Path,
        iso: Option<&Path>,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<(), BackendError> {
        if iso.is_some() {
            // This is an installer boot.
            if let Some(reason) = self.fail_next_install.take() {
                on_progress(Progress::Working("installer starting".into()));
                return Err(BackendError::Op(reason));
            }
            on_progress(Progress::Working("installer running".into()));
            for line in self.console.drain(..) {
                on_progress(Progress::ConsoleLine(line));
            }
            on_progress(Progress::Working("install complete, detaching ISO".into()));
            on_progress(Progress::Done);
            return Ok(());
        }
        // Normal boot of the installed system.
        on_progress(Progress::Working(format!("booting {}", vm.config.name)));
        on_progress(Progress::Done);
        Ok(())
    }

    fn stop(&mut self, vm: &Vm) -> Result<(), BackendError> {
        self.stopped.push(vm.id.as_str().to_string());
        Ok(())
    }
}

/// Compute the sha256 (hex) of a file — the real identity of an ISO.
pub fn sha256_file(path: &Path) -> Result<String, BackendError> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Default layout under a root dir: where Glide keeps its artifacts.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn disks(&self) -> PathBuf {
        self.root.join("disks")
    }
    pub fn snapshots(&self) -> PathBuf {
        self.root.join("snapshots")
    }
    pub fn store(&self) -> PathBuf {
        self.root.join("glide.nedb")
    }
}
