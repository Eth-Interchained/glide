//! `glide-gui` — the Glide VM manager desktop app.

mod app;

use app::GlideApp;
use forge_ui::{run, WindowOptions};
use glide_core::backend::MockBackend;
use glide_core::Engine;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".glide")
        });

    // Backend: Virtualization.framework on macOS, mock elsewhere.
    #[cfg(target_os = "macos")]
    let app = {
        let be = glide_vz::VzBackend::new()
            .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;
        let engine = Engine::new(&root, be)?;
        GlideApp::spawn(engine)
    };
    #[cfg(not(target_os = "macos"))]
    let app = {
        let engine = Engine::new(&root, MockBackend::new())?;
        GlideApp::spawn(engine)
    };

    run(
        app,
        WindowOptions {
            title: "Glide".into(),
            width: 1040.0,
            height: 760.0,
            ..Default::default()
        },
    )
}
