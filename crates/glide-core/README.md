# glide-core: real QEMU service

The public API is `Config`, `CreateOptions`, `Machine`, `BackendInfo`, and `Service` (re-exported from `lib.rs`). Every Service operation takes `&self`; Service is Send + Sync. No mock, Apple VZ stub or historical engine module is compiled.

## Runtime

- macOS: standalone QEMU with HVF and Cocoa, native host architecture only. Install `brew install qemu`; Intel Homebrew `/usr/local/bin`, ARM Homebrew `/opt/homebrew/bin`, and PATH are searched. `GLIDE_QEMU_DIR` explicitly selects a directory containing both `qemu-system-ARCH` and `qemu-img`; UTM application helpers are not assumed runnable.
- Real Linux/headless testing only: explicitly set **both** `GLIDE_ACCEL=tcg` and `GLIDE_DISPLAY=none`. This is a real emulator, not simulation. No automatic software-emulation fallback.
- x86 uses Q35 + SeaBIOS, virtio qcow2 disk, VGA, USB tablet and user networking. ARM requires installed UEFI firmware, optionally selected with `GLIDE_AARCH64_FIRMWARE`; ARM runtime has not been tested here.
- `-daemonize` keeps QEMU alive after CLI/GUI exit. UUID-authenticated QMP returns live status. A paused guest reports `paused`; timeout/protocol/permission errors report `unknown`, not `stopped`.
- Graceful stop sends `system_powerdown` and waits up to 30 seconds. An uncooperative guest produces an explicit error and is not killed. Force-stop sends QMP `quit` and waits for actual exit. No pid-based termination is used by the service.
- Cocoa opens at start. `open_display` asks macOS System Events to foreground its QEMU process; Automation/Accessibility permission may be required. It never pretends a headless VM has a display.
- Serial is an actual UNIX socket and file log. A guest must configure ttyS0 to expose a login console; an Ubuntu GUI installer may not produce serial output by default.

## Persistence and safety

`GLIDE_HOME`, or `~/Library/Application Support/Glide` on macOS, contains `machines/UUID/config.json` and `disk.qcow2`. The JSON is authoritative and replaced with flushed/synced atomic writes. A cross-process fs2 lock serializes CLI/GUI mutations; internal lifecycle helpers avoid nested public-method deadlocks. Root/runtime directories must be private (0700) and user-owned. QMP/serial paths live in a short private `/tmp/glide-UID/root-hash/UUID` directory so long Application Support paths are safe.

ISO9660 descriptors are scanned starting at sector 16, including boot descriptors preceding the primary descriptor. Actual EFI PE machine fields are inspected when accessible. Filename/volume-label architecture hints are explicitly logged as **inferred**, not detected; otherwise require a user-selected architecture. UDF-only media is rejected with a useful error. No ISO is synthesized, modified or deleted. The installer remains attached and precedes the disk on boot until explicit eject. Live eject identifies the actual QMP block device and persists detachment only after QMP confirms an empty drive; offline eject updates only config.

Removal requires a confirmed stopped process. `remove(false)` retains disk data in its UUID directory and writes a tombstone to `removed/UUID.json` outside active listing. `remove(true)` deletes only the validated app-owned UUID directory, rejecting symlink/hardlinked/external disks and symlinks within the deletion tree. External qcow2 backing/data files and physical disks are not allowed. UI/CLI must ask for deletion confirmation before passing true.

## Audit

Actual successful operations append flushed `audit.jsonl` events; no guest installation-success event is invented. `GLIDE_AUDIT=nedb` additionally writes real causal records to `audit.nedb`. NEDB 2.8.6 prints unconditional startup banners to stdout, so its integration is opt-in to avoid corrupting CLI JSON output. Audit failure is reported as a warning and does not lie about or undo an already-completed VM action. Config/QMP are authoritative.

## Verification

```sh
cargo test -p glide-core
GLIDE_QEMU_DIR=/path/to/qemu/bin \
GLIDE_ACCEL=tcg GLIDE_DISPLAY=none \
GLIDE_TEST_ISO=/path/to/real-amd64-installer.iso \
GLIDE_AUDIT=nedb \
cargo test -p glide-core -- --ignored --test-threads=1
cargo clippy -p glide-core --tests -- -D warnings
```

The ignored suites exercise a real QEMU daemon/qemu-img, a real ISO, live QMP pause/resume, attachment persistence, actual live eject, genuine graceful-shutdown timeout, QMP quit, restart, removal safety, long/comma-containing paths, persisted NEDB events, and timeout classification by suspending only the test's UUID-verified QEMU process. Default tests never substitute a mock backend. macOS HVF/Cocoa runtime and a completed Ubuntu installation require verification on an actual Mac; Linux TCG tests do not prove those work.
