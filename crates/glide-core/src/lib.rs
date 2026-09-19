//! Real QEMU/HVF VM service. JSON is authoritative; live status comes from QMP.
//! macOS defaults to native HVF + Cocoa. Linux emulator tests require explicit
//! GLIDE_ACCEL=tcg and GLIDE_DISPLAY=none. No mock/VZ backend is compiled.
mod iso;
mod qmp;
mod service;
pub use service::{BackendInfo, Config, CreateOptions, Machine, Service};
