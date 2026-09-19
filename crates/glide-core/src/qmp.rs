//! Minimal synchronous QMP client: newline-delimited JSON, IDs, event skipping and deadlines.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) struct Qmp {
    stream: BufReader<UnixStream>,
    next_id: u64,
}
impl Qmp {
    pub fn connect(path: &Path, expected_uuid: &str) -> Result<Self> {
        // Nonblocking connect with poll bounds even a full UNIX socket accept queue.
        let stream = connect_bounded(path, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut q = Self {
            stream: BufReader::new(stream),
            next_id: 0,
        };
        let greeting = q.read(Instant::now() + Duration::from_secs(3))?;
        ensure!(greeting.get("QMP").is_some(), "invalid QMP greeting");
        q.execute("qmp_capabilities", json!({}))?;
        let identity = q.execute("query-uuid", json!({}))?;
        ensure!(
            identity["UUID"].as_str() == Some(expected_uuid),
            "QMP UUID mismatch; refusing to control this process"
        );
        Ok(q)
    }
    fn read(&mut self, deadline: Instant) -> Result<Value> {
        let left = deadline
            .checked_duration_since(Instant::now())
            .context("QMP response deadline expired")?;
        self.stream.get_ref().set_read_timeout(Some(left))?;
        // Read incrementally with a frame limit, not an unbounded read_line allocation.
        let mut bytes = Vec::new();
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .context("QMP response deadline expired")?;
            self.stream.get_ref().set_read_timeout(Some(left))?;
            let chunk = self
                .stream
                .fill_buf()
                .context("QMP read timed out or failed")?;
            ensure!(!chunk.is_empty(), "QMP closed connection before reply");
            let n = chunk
                .iter()
                .position(|b| *b == b'\n')
                .map(|p| p + 1)
                .unwrap_or(chunk.len());
            ensure!(
                bytes.len() + n <= 4 * 1024 * 1024,
                "QMP response exceeds 4 MiB"
            );
            let done = chunk[n - 1] == b'\n';
            bytes.extend_from_slice(&chunk[..n]);
            self.stream.consume(n);
            if done {
                return serde_json::from_slice(&bytes).context("invalid QMP JSON");
            }
        }
    }
    pub fn execute(&mut self, command: &str, args: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = serde_json::to_vec(&json!({"execute":command,"arguments":args,"id":id}))?;
        request.push(b'\n');
        self.stream
            .get_mut()
            .write_all(&request)
            .context("write QMP request")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let reply = self.read(deadline)?;
            if reply.get("event").is_some() {
                continue;
            }
            ensure!(
                reply["id"].as_u64() == Some(id),
                "unexpected QMP response ID"
            );
            if let Some(error) = reply.get("error") {
                bail!("QMP {command}: {error}");
            }
            return reply
                .get("return")
                .cloned()
                .context("QMP reply has neither return nor error");
        }
    }
}

fn connect_bounded(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::{io, mem};
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "QMP socket path too long",
        ));
    }
    addr.sun_family = libc::AF_UNIX as _;
    #[cfg(target_os = "macos")]
    {
        addr.sun_len = mem::size_of_val(&addr) as u8;
    }
    for (to, from) in addr.sun_path.iter_mut().zip(bytes) {
        *to = *from as _;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    stream.set_nonblocking(true)?;
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            mem::size_of_val(&addr) as _,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        // EAGAIN on Linux AF_UNIX means the backlog is full, not connected.
        if err.raw_os_error() == Some(libc::EAGAIN) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "QMP accept queue full",
            ));
        }
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(err);
        }
        let mut pfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as _) };
        if rc == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "QMP connect timed out",
            ));
        }
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if let Some(err) = stream.take_error()? {
            return Err(err);
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}
