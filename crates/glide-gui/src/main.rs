//! `glide-gui` — the Glide VM manager desktop app.

mod app;

use app::GlideApp;
use forge_ui::{run, WindowOptions};
use glide_core::Service;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut root = Service::default_root();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--root" {
            root = PathBuf::from(args.next().ok_or("--root requires a directory path")?);
        } else if arg == "--help" || arg == "-h" {
            println!("Usage: glide-gui [--root PATH]\nUses the same VM library and real backend as glide.");
            return Ok(());
        } else {
            return Err(format!(
                "Unknown argument: {}. Usage: glide-gui [--root PATH]",
                arg.to_string_lossy()
            )
            .into());
        }
    }
    // Terminal may start in a different directory; preserve --root identity there.
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    let app = GlideApp::spawn(root);

    run(
        app,
        WindowOptions {
            title: "Glide".into(),
            width: 1120.0,
            height: 820.0,
            ..Default::default()
        },
    )
}
