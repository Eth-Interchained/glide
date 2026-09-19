# Glide

Rust CLI + Forge UI desktop VM manager for **Intel macOS (x86_64)**. Select a real Linux ISO, create a qcow2 disk, boot it with QEMU/HVF, interact with the guest's native graphical window, eject the installer explicitly, and boot from disk again.

## What this build is

This revision replaces the earlier mock/VZ scaffold. There is **no mock backend or simulated installation success**. The GUI and CLI call the same `glide-core::Service`; state comes from the real QEMU QMP socket. QEMU continues running when the CLI or manager exits.

**Backend:** standalone QEMU controlled directly by Glide, not UTM automation. No UTM configuration or manual UTM launch is involved. The graphical guest window uses QEMU Cocoa, not an embedded SPICE viewer. Do not assume an installed UTM.app supplies the runnable standalone QEMU binaries Glide needs. Native reboot/physical disk passthrough, snapshots and cloning are not in this build.

## Intel Mac quick start

Requires macOS 13+, Rust stable for source builds, and standalone QEMU with HVF + Cocoa:

```sh
brew install qemu
git clone https://github.com/Eth-Interchained/glide.git
cd glide
bash scripts/package-macos.sh x86_64-apple-darwin
open dist/x86_64-apple-darwin/Glide.app
```

Or download the `Glide-Intel-macOS` artifact from a successful GitHub Actions run and extract its app zip. The development app is **ad-hoc signed, not Developer ID signed or notarized**; macOS may require Open Anyway in Privacy & Security after you verify its source. QEMU is a separately installed dependency, not bundled in the zip.

1. **New from ISO → Browse ISO.** Choose your Ubuntu **amd64** ISO.
2. Name it; retain **x86_64 / 4 CPUs / 8192 MiB / 64 GiB**.
3. **Create and Boot.** QEMU's graphical guest window opens; install Ubuntu onto the virtual disk.
4. When Ubuntu asks you to remove installation media, use **Eject ISO** in Glide, then continue the guest restart. If already shut down, eject offline and press Start.
5. Subsequent starts use the existing disk. Glide never assumes installer shutdown means installation succeeded.

Buttons follow actual backend state. Stop requests ACPI shutdown and waits; if the guest does not respond, the error tells you it may still be running. Nothing is killed silently. GUI/CLI errors retain disk data and expose the backend log.

## CLI

The app includes `Glide.app/Contents/MacOS/glide`; the source build places it in `target/release/glide` (or the target-specific release directory).

```sh
glide doctor
glide create --name ubuntu-dev --iso "$HOME/Downloads/ubuntu.iso" --arch x86_64 --cpu 4 --memory 8G --disk 64G --boot
glide list
glide status ubuntu-dev
glide display ubuntu-dev
glide eject ubuntu-dev
glide restart ubuntu-dev
glide stop ubuntu-dev
glide logs ubuntu-dev
glide console ubuntu-dev             # requires guest serial-console configuration; Ctrl-] detaches
glide remove ubuntu-dev              # removes registration, RETAINS disk
# Destructive operations require typed confirmation or explicit --yes:
glide force-stop ubuntu-dev          # potential guest data loss
glide delete ubuntu-dev              # deletes only this VM's app-owned virtual disk
```

`--root PATH` is shared by GUI and CLI. Default macOS state lives at `~/Library/Application Support/Glide/machines/<uuid>/`. No ISO copy, host disk writes or physical disk passthrough occur. UNIX control sockets are in a private short runtime directory. Config writes are atomic; a cross-process lock coordinates GUI and CLI.

ISO architecture is read from accessible EFI PE headers where possible. Filename/volume hints are labelled **inferred**, not detected. The GUI's architecture field defaults to the host, not a claim about the ISO. HVF never silently falls back to cross-architecture emulation. ARM64 support checks for real firmware but is not runtime-verified in this change.

## Verification and boundaries

- Ordinary tests exercise parsing, real filesystem errors, UI Actions, pending-operation handling, deletion confirmation and Forge rendering.
- Opt-in integration tests launch **real QEMU** with a checksum-verified Alpine Linux ISO, real qcow2/QMP and real persistence. They exercise status, pause/resume, ejection, restart, shutdown timeout, safe removal and an unresponsive real QEMU.
- Linux TCG is real emulation, but **not evidence that Intel HVF or macOS Cocoa works**. Intel CI compiles/tests the whole workspace and packages the real app/CLI. The full Ubuntu install → eject → disk boot acceptance test remains a Mac runtime test.
- Guest serial login needs ttyS0 configuration; blank serial output is not a failed graphical boot.
- Structured audit always writes JSONL; `GLIDE_AUDIT=nedb` additionally uses actual NEDB causal writes. It is opt-in because the pinned engine prints startup banners on stdout. Audit errors cannot turn a failed boot into success.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
GLIDE_TEST_ISO=/path/to/real-x86_64.iso GLIDE_ACCEL=tcg GLIDE_DISPLAY=none \
  cargo test -p glide-core -- --ignored --test-threads=1
```

`GLIDE_QEMU_DIR` explicitly selects a directory containing the emulator and qemu-img. Otherwise Glide searches Intel/ARM Homebrew locations and PATH. `glide doctor` checks real executable, accelerator and display availability before creation.

## Code

- `crates/glide-core/src/service.rs`: lifecycle, storage, discovery, safe deletion, audit.
- `crates/glide-core/src/qmp.rs`: bounded socket protocol with request IDs and VM UUID verification.
- `crates/glide-core/src/iso.rs`: real installation-media inspection.
- `crates/glide-cli/src/main.rs`: CLI and serial console.
- `crates/glide-gui/src/app.rs`: native-style Forge UI over the same service.

License: BUSL-1.1, converting to GPL-3.0-only on the date specified in LICENSE. QEMU remains a separate GPL-licensed subprocess and is not linked or bundled.
