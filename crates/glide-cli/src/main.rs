//! CLI over the same real VM service used by Forge UI.
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use glide_core::{CreateOptions, Service};
use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    name = "glide",
    version,
    about = "Boot Linux ISOs on macOS using QEMU/HVF"
)]
struct Cli {
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}
#[derive(Subcommand)]
enum Cmd {
    /// Check real QEMU, accelerator and display availability.
    Doctor,
    #[command(alias = "ls")]
    List,
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        iso: PathBuf,
        /// Explicit guest architecture; otherwise inspect ISO evidence.
        #[arg(long)]
        arch: Option<String>,
        #[arg(long, default_value_t = 4)]
        cpu: u32,
        #[arg(long, default_value = "8G")]
        memory: String,
        #[arg(long, default_value = "64G")]
        disk: String,
        #[arg(long)]
        boot: bool,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    /// Power off immediately: unsaved guest data can be lost.
    ForceStop {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    Restart {
        name: String,
    },
    Status {
        name: String,
    },
    /// Detach the installer explicitly; never infers installation completion.
    Eject {
        name: String,
    },
    Display {
        name: String,
    },
    /// Interactive serial socket. Guest must enable its serial console. Ctrl-] detaches.
    Console {
        name: String,
    },
    Logs {
        name: String,
    },
    /// Remove registration and KEEP the virtual disk.
    Remove {
        name: String,
    },
    /// Remove VM and its app-owned virtual disk; requires confirmation.
    Delete {
        name: String,
        #[arg(long)]
        yes: bool,
    },
}
fn size_mib(value: &str) -> Result<u64> {
    let s = value.trim().to_ascii_uppercase();
    let (number, mult) = if let Some(x) = s.strip_suffix("GIB").or_else(|| s.strip_suffix('G')) {
        (x, 1024)
    } else if let Some(x) = s.strip_suffix("MIB").or_else(|| s.strip_suffix('M')) {
        (x, 1)
    } else {
        (s.as_str(), 1)
    };
    number
        .parse::<u64>()
        .context("size must be an integer with M or G suffix")?
        .checked_mul(mult)
        .context("size overflow")
}
fn confirm(name: &str, action: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!("{action} requires explicit --yes in noninteractive use")
    }
    eprint!("{action}. Type the exact VM name or ID '{name}' to confirm: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if line.trim() != name {
        bail!("cancelled; no changes made")
    }
    Ok(())
}
struct RawTerminal(Option<libc::termios>);
impl RawTerminal {
    fn enter() -> Result<Self> {
        if !io::stdin().is_terminal() {
            return Ok(Self(None));
        }
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes the termios buffer for valid stdin fd.
        if unsafe { libc::tcgetattr(0, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(Some(original)))
    }
}
impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
    }
}
fn console(path: PathBuf) -> Result<()> {
    use std::os::{fd::AsRawFd, unix::net::UnixStream};
    let mut stream = UnixStream::connect(path).context("connect guest serial console")?;
    eprintln!("Connected to guest serial. Ctrl-] detaches. A blank console means the guest has not enabled serial output.");
    let _raw = RawTerminal::enter()?;
    let mut buf = [0; 4096];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if fds[0].revents & libc::POLLIN != 0 {
            let n = io::stdin().read(&mut buf)?;
            if n == 0 {
                break;
            }
            if let Some(i) = buf[..n].iter().position(|b| *b == 0x1d) {
                stream.write_all(&buf[..i])?;
                break;
            }
            stream.write_all(&buf[..n])?;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            io::stdout().write_all(&buf[..n])?;
            io::stdout().flush()?;
        }
        if fds
            .iter()
            .any(|p| p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0)
        {
            break;
        }
    }
    Ok(())
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    let service = Service::open(cli.root.unwrap_or_else(Service::default_root))?;
    match cli.command {
        Cmd::Doctor => {
            let b = service.discover();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&b)?)
            } else {
                println!("{} / {}\n{}", b.architecture, b.accelerator, b.detail)
            }
            if !b.available {
                bail!("backend unavailable")
            }
        }
        Cmd::List => {
            let list = service.list()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&list)?)
            } else {
                if list.is_empty() {
                    println!(
                        "No VMs. Use glide create --name ubuntu --iso /path/to/ubuntu.iso --boot"
                    )
                }
                for m in list {
                    println!(
                        "{}\t{}\t{}\t{} CPU / {} MiB / {} GiB",
                        m.config.name,
                        m.config.id,
                        m.state,
                        m.config.cpu,
                        m.config.memory_mb,
                        m.config.disk_gb
                    )
                }
            }
        }
        Cmd::Create {
            name,
            iso,
            arch,
            cpu,
            memory,
            disk,
            boot,
        } => {
            let disk_mib = size_mib(&disk)?;
            anyhow::ensure!(disk_mib % 1024 == 0, "disk must be whole GiB, e.g. 64G");
            let m = service.create(CreateOptions {
                name,
                iso,
                architecture: arch,
                cpu,
                memory_mb: size_mib(&memory)?,
                disk_gb: disk_mib / 1024,
            })?;
            if boot {
                service.start(&m.config.id).with_context(||format!("VM {} was created and its disk retained, but boot failed. Use glide logs '{}'",m.config.id,m.config.id))?;
            }
            let m = service.status(&m.config.id)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&m)?)
            } else {
                println!("{} [{}]: {}", m.config.name, m.config.id, m.state)
            }
        }
        Cmd::Status { name } => {
            let m = service.status(&name)?;
            println!("{}", serde_json::to_string_pretty(&m)?)
        }
        Cmd::Logs { name } => print!("{}", service.logs(&name)?),
        Cmd::Console { name } => console(service.console_path(&name)?)?,
        Cmd::Start { name } => {
            service.start(&name)?;
            println!("{}", serde_json::to_string_pretty(&service.status(&name)?)?)
        }
        Cmd::Stop { name } => {
            service.stop(&name)?;
            println!("Stopped {name}")
        }
        Cmd::ForceStop { name, yes } => {
            confirm(
                &name,
                "Force power off: unsaved guest data may be lost",
                yes,
            )?;
            service.force_stop(&name)?;
            println!("Powered off {name}")
        }
        Cmd::Restart { name } => {
            service.restart(&name)?;
            println!("Restarted {name}")
        }
        Cmd::Eject { name } => {
            service.eject(&name)?;
            println!("Installer detached from {name}; external ISO preserved")
        }
        Cmd::Display { name } => service.open_display(&name)?,
        Cmd::Remove { name } => {
            service.remove(&name, false)?;
            println!("Removed {name} from library; virtual disk retained")
        }
        Cmd::Delete { name, yes } => {
            confirm(&name, "Delete VM and its virtual disk permanently", yes)?;
            service.remove(&name, true)?;
            println!("Deleted {name} and its app-owned virtual disk")
        }
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("Glide: {e:#}");
        std::process::exit(1)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizes() {
        assert_eq!(size_mib("8G").unwrap(), 8192);
        assert_eq!(size_mib("512M").unwrap(), 512);
        assert!(size_mib("nope").is_err());
        assert!(size_mib("18446744073709551615G").is_err());
    }
    #[test]
    fn create_shape() {
        assert!(Cli::try_parse_from([
            "glide", "create", "--name", "ubuntu", "--iso", "/a.iso", "--boot"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["glide", "--backend", "mock", "list"]).is_err());
    }
}
