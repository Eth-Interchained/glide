//! `glide-vz`: the real Glide backend on Apple Virtualization.framework.
//!
//! On macOS this is the production backend. Everywhere else the crate
//! compiles to a stub that reports the backend unavailable, so the workspace
//! builds and tests on Linux CI. The real implementation lives in
//! `vz_macos.rs` and is verified by building on the Mac host it targets.

#[cfg(target_os = "macos")]
#[path = "vz_macos.rs"]
mod vz_impl;

use glide_core::backend::{Backend, BackendError, Progress};
use glide_core::model::Vm;
use std::path::Path;

/// The Virtualization.framework backend.
pub struct VzBackend {
    _private: (),
}

impl VzBackend {
    /// Construct, or fail plainly if the host can't virtualize.
    #[cfg(target_os = "macos")]
    pub fn new() -> Result<Self, BackendError> {
        Ok(Self { _private: () })
    }

    /// Off macOS there is no backend.
    #[cfg(not(target_os = "macos"))]
    pub fn new() -> Result<Self, BackendError> {
        Err(BackendError::Unavailable(
            "glide-vz requires macOS (Virtualization.framework)".into(),
        ))
    }
}

#[cfg(target_os = "macos")]
impl Backend for VzBackend {
    fn name(&self) -> &'static str {
        "virtualization-framework"
    }
    fn create_disk(&self, path: &Path, bytes: u64) -> Result<(), BackendError> {
        vz_impl::create_disk_image(path, bytes)
    }
    fn clone_disk(&self, from: &Path, to: &Path) -> Result<(), BackendError> {
        vz_impl::clone_disk_cow(from, to)
    }
    fn boot(
        &mut self,
        vm: &Vm,
        disk: &Path,
        iso: Option<&Path>,
        on_progress: &mut dyn FnMut(Progress),
    ) -> Result<(), BackendError> {
        vz_impl::run_vm(vm, disk, iso, on_progress)
    }
    fn stop(&mut self, vm: &Vm) -> Result<(), BackendError> {
        vz_impl::stop_vm(vm)
    }
}

#[cfg(not(target_os = "macos"))]
impl Backend for VzBackend {
    fn name(&self) -> &'static str {
        "virtualization-framework-unavailable"
    }
    fn create_disk(&self, _p: &Path, _b: u64) -> Result<(), BackendError> {
        Err(BackendError::Unavailable("macOS only".into()))
    }
    fn clone_disk(&self, _f: &Path, _t: &Path) -> Result<(), BackendError> {
        Err(BackendError::Unavailable("macOS only".into()))
    }
    fn boot(
        &mut self,
        _vm: &Vm,
        _d: &Path,
        _i: Option<&Path>,
        _cb: &mut dyn FnMut(Progress),
    ) -> Result<(), BackendError> {
        Err(BackendError::Unavailable("macOS only".into()))
    }
    fn stop(&mut self, _vm: &Vm) -> Result<(), BackendError> {
        Err(BackendError::Unavailable("macOS only".into()))
    }
}
