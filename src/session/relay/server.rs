//! The relay loop, its sockets and replay of the session log.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::INTERNAL_ARG;
use super::args::RelayArgs;

/// Maximum number of historical bytes replayed to a newly attached client.
const MAX_REPLAY_BYTES: u64 = 4 * 1024 * 1024;
/// Upper bound for unread terminal input held in the relay.
const MAX_PENDING_INPUT: usize = 64 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn the detached relay and return its pid.
///
/// The relay's stderr is redirected to `stderr_log` instead of `/dev/null`:
/// message texts are never shown directly to the user after the detach, but a
/// failed start needs to explain *why* the relay died.
pub fn spawn(args: &RelayArgs, stderr_log: &Path) -> Result<u32> {
    let stderr = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(stderr_log)
        .with_context(|| format!("failed to open relay log {}", stderr_log.display()))?;
    let exe = std::env::current_exe().context("failed to locate the hegn executable")?;
    let mut child = std::process::Command::new(exe)
        .arg(INTERNAL_ARG)
        .arg("--backend")
        .arg(&args.backend)
        .arg("--frontend")
        .arg(&args.frontend)
        .arg("--log")
        .arg(&args.log)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr))
        .spawn()
        .context("failed to spawn the console relay")?;
    let pid = child.id();
    // The relay outlives this function; a tiny reaper thread prevents a
    // zombie while the TUI process is still running.
    let _ = std::thread::Builder::new()
        .name("hegn-relay-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(pid)
}

/// Run the relay loop until the backend disappears or the process is killed.
pub fn run(args: RelayArgs) -> Result<()> {
    let mut backend = connect_backend(&args.backend)?;
    backend
        .set_nonblocking(true)
        .context("failed to configure the serial backend")?;
    // Open the log before publishing the frontend socket: a failed log open
    // must not leave a socket behind that looks attachable.
    let mut log = open_log(&args.log)?;
    let listener = bind_frontend(&args.frontend)?;
    listener
        .set_nonblocking(true)
        .context("failed to configure the relay socket")?;

    // Safety: the frontend socket is owned by this process; remove it on a
    // clean exit. A SIGKILL leaves it behind and the next start cleans it up
    // after verifying that it is a socket.
    let _guard = SocketGuard(args.frontend.clone());
    let mut client: Option<UnixStream> = None;
    let mut pending_input: Vec<u8> = Vec::new();

    loop {
        let mut pollfds = Vec::with_capacity(3);
        pollfds.push(libc::pollfd {
            fd: backend.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        pollfds.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        if let Some(stream) = &client {
            pollfds.push(libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // Safety: `pollfds` is a correctly sized, initialized array of pollfd
        // values; poll only reads the descriptors and writes the revents
        // fields for the duration of this call.
        let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, 200) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("console relay poll failed");
        }

        // Backend first: guest output is never dropped in favour of input.
        if pollfds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match read_backend(&mut backend, &mut log, client.as_mut()) {
                BackendRead::Data | BackendRead::Empty => {}
                BackendRead::Closed => return Ok(()),
                BackendRead::ClientGone => client = None,
            }
        }

        // Accept a new client; a newer attach replaces the previous one, which
        // mirrors Cloud Hypervisor's own single-client serial socket.
        if pollfds[1].revents & libc::POLLIN != 0 {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Some(old) = client.take() {
                        let _ = old.shutdown(std::net::Shutdown::Both);
                    }
                    match attach_client(stream, &mut log) {
                        Ok(stream) => client = Some(stream),
                        Err(err) => {
                            let _ = write_log(
                                &mut log,
                                format!("relay: attach failed: {err}\n").as_bytes(),
                            );
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => {
                    let _ = write_log(
                        &mut log,
                        format!("relay: accept failed: {err}\n").as_bytes(),
                    );
                }
            }
        }

        // Terminal input.
        if let Some(stream) = &client
            && pollfds
                .get(2)
                .is_some_and(|fd| fd.revents & (libc::POLLIN | libc::POLLHUP) != 0)
        {
            let mut buf = [0u8; 4096];
            match stream_read(stream, &mut buf) {
                Ok(0) => client = None,
                Ok(n) => {
                    let room = MAX_PENDING_INPUT.saturating_sub(pending_input.len());
                    pending_input.extend_from_slice(&buf[..n.min(room)]);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => client = None,
            }
        }

        if !pending_input.is_empty() {
            match backend.write(&pending_input) {
                Ok(0) => {}
                Ok(n) => {
                    pending_input.drain(..n);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(err).context("failed to write to the guest serial port"),
            }
        }
    }
}

enum BackendRead {
    Data,
    Empty,
    Closed,
    ClientGone,
}

fn read_backend(
    backend: &mut UnixStream,
    log: &mut File,
    mut client: Option<&mut UnixStream>,
) -> BackendRead {
    let mut buf = [0u8; READ_CHUNK];
    match backend.read(&mut buf) {
        Ok(0) => BackendRead::Closed,
        Ok(n) => {
            let data = &buf[..n];
            if write_log(log, data).is_err() {
                // Logging is the reason the relay exists. A broken log is a
                // hard error for the relay; the VM itself keeps running.
                return BackendRead::Closed;
            }
            if let Some(stream) = client.as_mut()
                && write_client(stream, data).is_err()
            {
                return BackendRead::ClientGone;
            }
            BackendRead::Data
        }
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => BackendRead::Empty,
        Err(_) => BackendRead::Closed,
    }
}

fn stream_read(stream: &UnixStream, buf: &mut [u8]) -> std::io::Result<usize> {
    (&*stream).read(buf)
}

fn write_client(stream: &mut UnixStream, data: &[u8]) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(data)?;
    stream.set_nonblocking(true)?;
    Ok(())
}

fn write_log(log: &mut File, data: &[u8]) -> std::io::Result<()> {
    log.write_all(data)?;
    log.flush()
}

fn attach_client(mut stream: UnixStream, log: &mut File) -> Result<UnixStream> {
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.set_nonblocking(false)?;
    replay_log(log, &mut stream)?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}

/// Copy the tail of the session log to a fresh client.
fn replay_log(log: &mut File, stream: &mut UnixStream) -> Result<()> {
    let len = log.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    let start = len.saturating_sub(MAX_REPLAY_BYTES);
    log.seek(SeekFrom::Start(start))?;
    let mut remaining = len - start;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let read = log.read(&mut buf[..want])?;
        if read == 0 {
            break;
        }
        stream.write_all(&buf[..read])?;
        remaining -= read as u64;
    }
    Ok(())
}

fn connect_backend(path: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() < deadline => {
                let retryable = matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::WouldBlock
                );
                if !retryable {
                    return Err(err).with_context(|| {
                        format!("failed to connect to serial socket {}", path.display())
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("timed out waiting for serial socket {}", path.display())
                });
            }
        }
    }
}

/// RAII guard that temporarily narrows the process umask.
///
/// The previous mask is restored on drop, so an early return or panic while
/// binding can not leave the process with a permissive umask.
struct UmaskGuard(libc::mode_t);

impl UmaskGuard {
    fn new(mask: libc::mode_t) -> Self {
        Self(unsafe { libc::umask(mask) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.0);
        }
    }
}

fn bind_frontend(path: &Path) -> Result<UnixListener> {
    remove_stale_socket(path)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    // Create the socket without group/other access from the very first instant.
    // The guard restores the previous mask on drop (including early returns and
    // panics); the chmod below stays as belt-and-braces.
    let _umask = UmaskGuard::new(0o077);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("failed to bind relay socket {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", path.display()))?;
    Ok(listener)
}

fn open_log(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to open serial log {}", path.display()))
}

/// Remove a stale relay socket, but never a regular file or symlink.
fn remove_stale_socket(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)
            .with_context(|| format!("failed to remove stale socket {}", path.display())),
        Ok(_) => bail!(
            "refusing to replace {}: it exists and is not a socket",
            path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = remove_stale_socket(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener as StdListener;

    #[test]
    fn frontend_refuses_to_replace_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        fs::write(&path, b"not a socket").unwrap();
        let err = bind_frontend(&path).unwrap_err().to_string();
        assert!(err.contains("not a socket"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), b"not a socket");
    }

    #[test]
    fn frontend_replaces_a_stale_socket_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let _old = StdListener::bind(&path).unwrap();
        drop(_old);
        let _listener = bind_frontend(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn socket_is_private_even_with_a_permissive_umask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");

        // A permissive ambient umask must not leak a wider socket mode in the
        // window before the explicit chmod runs.
        let previous = unsafe { libc::umask(0o000) };
        let listener = bind_frontend(&path);
        let restored = unsafe { libc::umask(previous) };
        assert_eq!(restored, 0o000, "bind must restore the previous umask");

        let _listener = listener.unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn log_is_opened_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"secret").unwrap();
        let link = dir.path().join("session.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_log(&link).is_err());

        let path = dir.path().join("real.log");
        let mut log = open_log(&path).unwrap();
        write_log(&mut log, b"boot\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"boot\n");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn replays_the_log_tail_to_a_new_client() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("session.log");
        fs::write(&log_path, b"boot complete\n").unwrap();
        let mut log = open_log(&log_path).unwrap();
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        replay_log(&mut log, &mut writer).unwrap();
        drop(writer);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"boot complete\n");
    }
}
