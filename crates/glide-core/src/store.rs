//! NEDB-backed lineage store.
//!
//! Provenance is the differentiator, so it lives in NEDB and is queryable.
//! Collections:
//!
//! ```text
//! iso_images  -> checksum, path, label, size
//! vms         -> config + current state
//! disks       -> id, path, capacity   (caused_by the iso node that installed it)
//! snapshots   -> id, vm, label, path  (caused_by the parent snapshot / disk node)
//! boots       -> id, vm, disk, iso    (caused_by the disk node booted)
//! ```
//!
//! `put` takes a `caused_by` of node hashes and writes the causal graph
//! edges itself; `Db::trace(hash, reverse, limit)` walks them. So a
//! snapshot's lineage back to the installing ISO is a real NEDB query.

use crate::model::{Boot, Disk, IsoImage, Snapshot, Vm};
use nedb_engine::Db;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("nedb: {0}")]
    Nedb(#[from] anyhow::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
}

/// The lineage + registry store.
pub struct Store {
    db: Db,
}

impl Store {
    /// Open (or create) a store rooted at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            db: Db::open(path, None)?,
        })
    }

    /// An in-memory store for tests.
    pub fn in_memory() -> Result<Self, StoreError> {
        Ok(Self {
            db: Db::in_memory(),
        })
    }

    /// Insert or replace a typed record in a collection, returning the node hash.
    /// The hash is what `caused_by` edges reference.
    fn put<T: serde::Serialize>(
        &mut self,
        coll: &str,
        id: &str,
        value: &T,
        caused_by: Vec<String>,
    ) -> Result<String, StoreError> {
        let data = serde_json::to_value(value)?;
        let node = self.db.put(coll, id, data, caused_by, None, None)?;
        Ok(node.hash)
    }

    fn get_typed<T: serde::de::DeserializeOwned>(
        &self,
        coll: &str,
        id: &str,
    ) -> Result<Option<T>, StoreError> {
        match self.db.get(coll, id) {
            Some(node) => Ok(Some(serde_json::from_value(node.data)?)),
            None => Ok(None),
        }
    }

    fn list_typed<T: serde::de::DeserializeOwned>(&self, coll: &str) -> Result<Vec<T>, StoreError> {
        self.db
            .list(coll)
            .into_iter()
            .map(|n| serde_json::from_value(n.data).map_err(StoreError::from))
            .collect()
    }

    /// The node hash for an id in a collection — the handle `caused_by` wants.
    fn hash_of(&self, coll: &str, id: &str) -> Option<String> {
        self.db.get(coll, id).map(|n| n.hash)
    }

    // ---- ISO images ----------------------------------------------------

    pub fn record_iso(&mut self, iso: &IsoImage) -> Result<String, StoreError> {
        self.put("iso_images", &iso.sha256, iso, vec![])
    }

    pub fn isos(&self) -> Result<Vec<IsoImage>, StoreError> {
        self.list_typed("iso_images")
    }

    // ---- VMs -----------------------------------------------------------

    pub fn record_vm(&mut self, vm: &Vm) -> Result<String, StoreError> {
        self.put("vms", vm.id.as_str(), vm, vec![])
    }

    pub fn get_vm(&self, id: &str) -> Result<Vm, StoreError> {
        self.get_typed("vms", id)?
            .ok_or_else(|| StoreError::NotFound(format!("vm {id}")))
    }

    pub fn find_vm_by_name(&self, name: &str) -> Result<Option<Vm>, StoreError> {
        Ok(self
            .list_typed::<Vm>("vms")?
            .into_iter()
            .find(|v| v.config.name == name))
    }

    pub fn vms(&self) -> Result<Vec<Vm>, StoreError> {
        self.list_typed("vms")
    }

    pub fn set_vm_state(
        &mut self,
        id: &str,
        state: crate::model::VmState,
    ) -> Result<(), StoreError> {
        let mut vm = self.get_vm(id)?;
        vm.state = state;
        self.record_vm(&vm)?;
        Ok(())
    }

    // ---- Disks ----------------------------------------------------------

    /// Record a disk; `caused_by_iso` is the *iso node hash* that installed it.
    pub fn record_disk(
        &mut self,
        disk: &Disk,
        vm: &Vm,
        caused_by_iso: Option<&str>,
    ) -> Result<String, StoreError> {
        #[derive(serde::Serialize)]
        struct DiskRec<'a> {
            #[serde(flatten)]
            disk: &'a Disk,
            vm: &'a str,
        }
        let rec = DiskRec {
            disk,
            vm: vm.id.as_str(),
        };
        let causes = caused_by_iso
            .map(|h| vec![h.to_string()])
            .unwrap_or_default();
        self.put("disks", &disk.id, &rec, causes)
    }

    pub fn disks(&self) -> Result<Vec<Disk>, StoreError> {
        self.list_typed("disks")
    }

    pub fn disk_hash(&self, id: &str) -> Option<String> {
        self.hash_of("disks", id)
    }

    pub fn iso_hash(&self, sha: &str) -> Option<String> {
        self.hash_of("iso_images", sha)
    }

    // ---- Snapshots -------------------------------------------------------

    /// Record a snapshot; `caused_by_disk` is the node hash of the disk/snapshot
    /// state it branched from.
    pub fn record_snapshot(
        &mut self,
        snap: &Snapshot,
        caused_by_disk: Option<&str>,
    ) -> Result<String, StoreError> {
        let causes = caused_by_disk
            .map(|h| vec![h.to_string()])
            .unwrap_or_default();
        self.put("snapshots", &snap.id, snap, causes)
    }

    pub fn snapshots(&self) -> Result<Vec<Snapshot>, StoreError> {
        self.list_typed("snapshots")
    }

    pub fn snapshots_for(&self, vm_id: &str) -> Result<Vec<Snapshot>, StoreError> {
        Ok(self
            .snapshots()?
            .into_iter()
            .filter(|s| s.vm.as_str() == vm_id)
            .collect())
    }

    // ---- Boots -----------------------------------------------------------

    pub fn record_boot(
        &mut self,
        boot: &Boot,
        caused_by_disk: Option<&str>,
    ) -> Result<String, StoreError> {
        let causes = caused_by_disk
            .map(|h| vec![h.to_string()])
            .unwrap_or_default();
        self.put("boots", &boot.id, boot, causes)
    }

    pub fn boots(&self) -> Result<Vec<Boot>, StoreError> {
        self.list_typed("boots")
    }

    pub fn boots_for(&self, vm_id: &str) -> Result<Vec<Boot>, StoreError> {
        Ok(self
            .boots()?
            .into_iter()
            .filter(|b| b.vm.as_str() == vm_id)
            .collect())
    }

    // ---- Lineage -----------------------------------------------------------

    /// Walk the causal graph from a node hash back toward its origins.
    /// Returns one human line per node on the path (newest first).
    pub fn trace_lineage(&self, hash: &str) -> Vec<String> {
        self.db
            .trace(hash, false, 64)
            .into_iter()
            .map(|n| {
                let label = n
                    .data
                    .get("label")
                    .or_else(|| n.data.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                format!("{}:{} {}", n.coll, n.id, label)
            })
            .collect()
    }
}

/// Convenience: build a `VmConfig` and a fresh `Vm`.
pub fn new_vm(name: impl Into<String>, disk_gb: u64, mem_gb: u64) -> Vm {
    use crate::model::VmConfig;
    Vm::new(VmConfig::sane(
        name,
        disk_gb * 1024 * 1024 * 1024,
        mem_gb * 1024 * 1024 * 1024,
    ))
}
