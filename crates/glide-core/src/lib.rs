//! Glide — a macOS VM manager with a provable lineage.
//!
//! `glide-core` is the portable engine: the domain model, the lifecycle
//! state machine, the backend seam, and the NEDB-backed lineage store.
//! The real hypervisor backend (`glide-vz`, Virtualization.framework) is
//! macOS-only; everything here builds and tests on any platform against the
//! `MockBackend`.

pub mod backend;
pub mod engine;
pub mod model;
pub mod store;

pub use backend::{Backend, BackendError, Layout, MockBackend, Progress};
pub use engine::{Engine, EngineError};
pub use model::{Boot, Disk, IsoImage, Snapshot, Vm, VmConfig, VmId, VmState};
pub use store::{Store, StoreError};
