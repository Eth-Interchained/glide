# Glide

**Codename: Glide** — a macOS VM manager with a *provable* lineage.

Glide boots an installer ISO and restarts elegantly into the installed system
(the ISO → installed handoff every VM tool fumbles), and records the whole
life of every VM in NEDB so you can *prove* where a machine came from, not
just hope a folder-naming convention held up.

## Why not UTM / Multipass / Lima / Tart

They're good. What none of them have is **lineage you can prove**:

```text
iso_images   → checksum + where it came from
disks        → caused_by the ISO that installed them
snapshots    → caused_by their parent snapshot / disk state
boots        → caused_by the disk state they booted from
```

`glide trace <vm>` walks the causal chain and answers *"which ISO originally
installed this, what branched from what, when did it last boot"* — as a
query, not a guess. Snapshot trees are a DAG; Glide models them as one.

## Architecture

Pure Rust, no Objective-C shim. On macOS the backend is Apple
Virtualization.framework via `objc2-virtualization`: `VZEFIBootLoader` boots
an ISO directly, then boots the installed system afterward (macOS 13+). On
Intel it runs x86-64 guests.

| crate | role |
|---|---|
| `glide-core` | portable engine: domain model, lifecycle state machine, NEDB lineage store |
| `glide-vz`   | the real backend (macOS only); stubs out elsewhere so CI stays green |
| `glide-cli`  | the `glide` command line |
| `glide-gui`  | the desktop app, built on [Forge UI](https://crates.io/crates/forge-ui) |

The engine is testable anywhere against a `MockBackend`; the macOS backend is
compiled and verified on the Mac host it targets.

## CLI

```bash
glide register-iso ~/Downloads/ubuntu-24.04.iso --label ubuntu-24.04
glide create dev --disk-gb 64 --mem-gb 8
glide install dev --iso d4c807dd     # boots installer, detaches ISO, -> installed
glide start dev                      # boots the installed system
glide snapshot dev pre-docker
glide trace dev                      # full provenance
glide ls
```

## GUI

```bash
glide-gui
```

Live boot/install progress runs on a worker thread and animates via Forge
UI's `Application::tick`.

## Notes

- **Entitlement (macOS):** Virtualization.framework requires the binary be
  code-signed with `com.apple.security.virtualization` or it refuses to run.
  Distribution needs an Apple Developer ID (the Certum cert is Windows-only).
- **Networking:** NAT by default. Bridged needs the `com.apple.vm.networking`
  entitlement, granted by Apple on request.
- **Snapshots:** copy-on-write via APFS `clonefile(2)` — a 40 GB snapshot is
  ~zero bytes and ~zero seconds.

## License

BUSL-1.1 → GPL-3.0-only. See `LICENSE`.
