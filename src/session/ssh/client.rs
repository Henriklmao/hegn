//! OpenSSH client wrapper for the guest control TAP.
//!
//! The argument list is fixed: the host's own `ssh_config` is ignored
//! (`-F /dev/null`), only the per-VM identity is offered, agent and forwarding
//! are disabled, and host keys are accepted on first use. The guest receives
//! the session context through the four `HEGN_*` environment variables.
//!
//! Errors are classified from SSH's stderr so callers can tell a VM that has
//! not booted sshd yet from a stale guest key or a changed host key.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::net::Ipv4Addr;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::SshError;

/// Guest-side session context forwarded through `SendEnv`.
#[derive(Debug, Clone, Default)]
pub struct GuestEnv {
    pub vm: String,
    pub project: String,
    pub harness: String,
    pub workspace: String,
}

/// One resolved SSH destination for a running VM.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub vm: String,
    pub host: Ipv4Addr,
    pub user: String,
    pub key: PathBuf,
    pub known_hosts: PathBuf,
    pub port: u16,
    pub env: GuestEnv,
}

/// A stderr signature that maps to a user-facing SSH failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifiedError {
    ConnectionRefused,
    PermissionDenied,
    HostKeyVerification,
    Unreachable,
}

const DEFAULT_PORT: u16 = 22;
const PROBE_INTERVAL: Duration = Duration::from_millis(250);

impl SshTarget {
    /// Build a target for `user@host`, validating the user name.
    ///
    /// The port starts at [`DEFAULT_PORT`]; a `0` assigned later is treated as
    /// [`DEFAULT_PORT`] as well.
    pub fn new(
        vm: impl Into<String>,
        host: Ipv4Addr,
        user: impl Into<String>,
        key: PathBuf,
        known_hosts: PathBuf,
    ) -> Result<Self, SshError> {
        let user = user.into();
        validate_user(&user)?;
        Ok(Self {
            vm: vm.into(),
            host,
            user,
            key,
            known_hosts,
            port: DEFAULT_PORT,
            env: GuestEnv::default(),
        })
    }

    /// Attach the guest session context forwarded to the remote command.
    pub fn with_env(mut self, env: GuestEnv) -> Self {
        self.env = env;
        self
    }

    /// Run a one-shot `true` on the guest without allocating a TTY.
    pub fn probe(&self) -> Result<(), SshError> {
        self.probe_with_program(Path::new("ssh"))
    }

    /// [`Self::probe`] with an explicit client binary, for tests.
    pub fn probe_with_program(&self, program: &Path) -> Result<(), SshError> {
        let mut command = Command::new(program);
        command.args(self.base_args(false, Some("true")));
        command.stdin(Stdio::null());
        self.apply_env(&mut command);

        let output = command
            .output()
            .map_err(|error| client_error(program, error))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(self
            .classified_error(classify_stderr(&stderr))
            .unwrap_or(SshError::Transport { stderr }))
    }

    /// Probe until the guest accepts the key or `timeout` expires.
    ///
    /// The first probe runs immediately; retries are spaced by 250 ms. A zero
    /// timeout returns immediately without starting a client.
    pub fn wait_ready(&self, timeout: Duration) -> Result<(), SshError> {
        if timeout.is_zero() {
            return Err(self.timeout_error(timeout, "no probe was attempted"));
        }

        let started = Instant::now();
        let mut last = None;
        while started.elapsed() < timeout {
            match self.probe() {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                break;
            }
            std::thread::sleep((timeout - elapsed).min(PROBE_INTERVAL));
        }
        let last = last
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no probe was attempted".to_string());
        Err(self.timeout_error(timeout, &last))
    }

    /// Open an interactive session, optionally running `command`.
    ///
    /// Allocates a TTY only when stdin is a terminal. The returned value is the
    /// remote exit code (or `128 + signal` when the client was signalled).
    pub fn interactive(&self, command: Option<&str>) -> Result<i32, SshError> {
        self.interactive_with_program(Path::new("ssh"), command)
    }

    /// [`Self::interactive`] with an explicit client binary, for tests.
    pub fn interactive_with_program(
        &self,
        program: &Path,
        command: Option<&str>,
    ) -> Result<i32, SshError> {
        // A readiness probe turns auth/transport failures into targeted errors
        // before the terminal is handed over to the client.
        self.probe_with_program(program)?;

        let tty = std::io::stdin().is_terminal();
        let mut client = Command::new(program);
        client.args(self.base_args(tty, command));
        client
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        self.apply_env(&mut client);

        let status = client
            .status()
            .map_err(|error| client_error(program, error))?;
        self.exit_code(status)
    }

    /// Translate the client's exit status into the remote session result.
    fn exit_code(&self, status: ExitStatus) -> Result<i32, SshError> {
        if let Some(code) = status.code() {
            if code == 255 {
                return Err(SshError::TransportAfterConnect {
                    vm: self.vm.clone(),
                    user: self.user.clone(),
                    host: self.host,
                });
            }
            return Ok(code);
        }
        if let Some(signal) = status.signal() {
            return Ok(128 + signal);
        }
        Ok(1)
    }

    /// The exact argument list passed to `ssh`.
    ///
    /// `tty` selects `-t` (allocate a remote TTY) versus `-T`; `command` is
    /// passed as a single argument after the destination.
    pub fn base_args(&self, tty: bool, command: Option<&str>) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::with_capacity(40);
        args.push("-F".into());
        args.push("/dev/null".into());
        args.push("-i".into());
        args.push(self.key.clone().into_os_string());

        let mut known_hosts = OsString::from("UserKnownHostsFile=");
        known_hosts.push(self.known_hosts.as_os_str());
        let options: [OsString; 16] = [
            "IdentitiesOnly=yes".into(),
            known_hosts,
            "GlobalKnownHostsFile=/dev/null".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "PasswordAuthentication=no".into(),
            "KbdInteractiveAuthentication=no".into(),
            "PreferredAuthentications=publickey".into(),
            "BatchMode=yes".into(),
            "ConnectTimeout=5".into(),
            "ServerAliveInterval=30".into(),
            "ServerAliveCountMax=3".into(),
            "ForwardAgent=no".into(),
            "ClearAllForwardings=yes".into(),
            "LogLevel=ERROR".into(),
            "SendEnv=HEGN_VM".into(),
            "SendEnv=HEGN_PROJECT".into(),
        ];
        // The two remaining SendEnv options keep the array readable above.
        for option in options.into_iter().chain([
            "SendEnv=HEGN_HARNESS".into(),
            "SendEnv=HEGN_WORKSPACE".into(),
        ]) {
            args.push("-o".into());
            args.push(option);
        }

        let port = self.effective_port();
        if port != DEFAULT_PORT {
            args.push("-p".into());
            args.push(port.to_string().into());
        }

        args.push(if tty { "-t" } else { "-T" }.into());
        args.push("--".into());
        args.push(format!("{}@{}", self.user, self.host).into());
        if let Some(command) = command {
            args.push(command.into());
        }
        args
    }

    fn effective_port(&self) -> u16 {
        if self.port == 0 {
            DEFAULT_PORT
        } else {
            self.port
        }
    }

    /// Forward only non-empty context so the guest never sees empty variables.
    fn apply_env(&self, command: &mut Command) {
        for (name, value) in [
            ("HEGN_VM", &self.env.vm),
            ("HEGN_PROJECT", &self.env.project),
            ("HEGN_HARNESS", &self.env.harness),
            ("HEGN_WORKSPACE", &self.env.workspace),
        ] {
            if !value.is_empty() {
                command.env(name, value);
            }
        }
    }

    fn classified_error(&self, classified: Option<ClassifiedError>) -> Option<SshError> {
        let classified = classified?;
        Some(match classified {
            ClassifiedError::ConnectionRefused => SshError::ConnectionRefused {
                vm: self.vm.clone(),
                user: self.user.clone(),
                host: self.host,
            },
            ClassifiedError::PermissionDenied => SshError::PermissionDenied {
                vm: self.vm.clone(),
                user: self.user.clone(),
                host: self.host,
            },
            ClassifiedError::HostKeyVerification => SshError::HostKeyVerification {
                vm: self.vm.clone(),
                host: self.host,
                known_hosts: self.known_hosts.clone(),
            },
            ClassifiedError::Unreachable => SshError::Unreachable {
                vm: self.vm.clone(),
                user: self.user.clone(),
                host: self.host,
            },
        })
    }

    fn timeout_error(&self, timeout: Duration, last: &str) -> SshError {
        SshError::Timeout {
            vm: self.vm.clone(),
            user: self.user.clone(),
            host: self.host,
            timeout,
            last: last.to_string(),
        }
    }
}

/// Classify an OpenSSH stderr message. Matching is case-insensitive.
pub fn classify_stderr(stderr: &str) -> Option<ClassifiedError> {
    let stderr = stderr.to_ascii_lowercase();
    if stderr.contains("connection refused") {
        Some(ClassifiedError::ConnectionRefused)
    } else if stderr.contains("permission denied") {
        Some(ClassifiedError::PermissionDenied)
    } else if stderr.contains("host key verification failed") {
        Some(ClassifiedError::HostKeyVerification)
    } else if stderr.contains("connection timed out") || stderr.contains("no route to host") {
        Some(ClassifiedError::Unreachable)
    } else {
        None
    }
}

fn validate_user(user: &str) -> Result<(), SshError> {
    let invalid = user.is_empty()
        || user.starts_with('-')
        || !user
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if invalid {
        return Err(SshError::InvalidUser {
            user: user.to_string(),
        });
    }
    Ok(())
}

fn client_error(program: &Path, error: std::io::Error) -> SshError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return SshError::MissingClient;
    }
    SshError::Io {
        path: program.to_path_buf(),
        source: error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(user: &str) -> SshTarget {
        SshTarget::new(
            "rust",
            Ipv4Addr::new(10, 0, 0, 2),
            user,
            PathBuf::from("/tmp/hegn/key"),
            PathBuf::from("/tmp/hegn/known_hosts"),
        )
        .unwrap()
    }

    fn args(target: &SshTarget, tty: bool, command: Option<&str>) -> Vec<String> {
        target
            .base_args(tty, command)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn base_args_are_exact_and_ordered() {
        let target = target("hegn");
        assert_eq!(
            args(&target, false, None),
            vec![
                "-F",
                "/dev/null",
                "-i",
                "/tmp/hegn/key",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "UserKnownHostsFile=/tmp/hegn/known_hosts",
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-o",
                "PreferredAuthentications=publickey",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "LogLevel=ERROR",
                "-o",
                "SendEnv=HEGN_VM",
                "-o",
                "SendEnv=HEGN_PROJECT",
                "-o",
                "SendEnv=HEGN_HARNESS",
                "-o",
                "SendEnv=HEGN_WORKSPACE",
                "-T",
                "--",
                "hegn@10.0.0.2",
            ]
        );
    }

    #[test]
    fn tty_flag_selects_t_and_the_command_is_one_trailing_argument() {
        let target = target("hegn");
        let with_tty = args(&target, true, Some("cargo test --all"));
        assert!(with_tty.contains(&"-t".to_string()));
        assert!(!with_tty.contains(&"-T".to_string()));
        assert_eq!(with_tty.last().unwrap(), "cargo test --all");
        assert_eq!(with_tty[with_tty.len() - 2], "hegn@10.0.0.2");
        assert_eq!(with_tty[with_tty.len() - 3], "--");
        assert_eq!(with_tty[with_tty.len() - 4], "-t");

        let without_tty = args(&target, false, None);
        assert!(without_tty.contains(&"-T".to_string()));
        assert!(!without_tty.contains(&"-t".to_string()));
    }

    #[test]
    fn ports_default_to_22_and_omit_the_flag() {
        let mut target = target("hegn");
        assert!(!args(&target, false, None).contains(&"-p".to_string()));

        target.port = 0;
        assert!(!args(&target, false, None).contains(&"-p".to_string()));

        target.port = 2222;
        let with_port = args(&target, false, None);
        let index = with_port.iter().position(|arg| arg == "-p").unwrap();
        assert_eq!(with_port[index + 1], "2222");
        let dash_index = with_port.iter().position(|arg| arg == "--").unwrap();
        assert!(index < dash_index);
    }

    #[test]
    fn users_are_validated() {
        for user in ["root", "hegn", "ubuntu", "agent_1", "user-2"] {
            assert!(
                SshTarget::new(
                    "rust",
                    Ipv4Addr::LOCALHOST,
                    user,
                    PathBuf::from("/k"),
                    PathBuf::from("/h")
                )
                .is_ok()
            );
        }
        for user in [
            "",
            "-oProxyCommand=evil",
            "root user",
            "../x",
            "ROOT",
            "a@b",
        ] {
            assert!(
                matches!(
                    SshTarget::new(
                        "rust",
                        Ipv4Addr::LOCALHOST,
                        user,
                        PathBuf::from("/k"),
                        PathBuf::from("/h")
                    ),
                    Err(SshError::InvalidUser { .. })
                ),
                "accepted {user:?}"
            );
        }
    }

    #[test]
    fn stderr_classification_covers_every_known_failure() {
        assert_eq!(
            classify_stderr("ssh: connect to host 10.0.0.2 port 22: Connection refused"),
            Some(ClassifiedError::ConnectionRefused)
        );
        assert_eq!(
            classify_stderr("hegn@10.0.0.2: Permission denied (publickey)."),
            Some(ClassifiedError::PermissionDenied)
        );
        assert_eq!(
            classify_stderr(
                "@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\nHost key verification failed."
            ),
            Some(ClassifiedError::HostKeyVerification)
        );
        assert_eq!(
            classify_stderr("ssh: connect to host 10.0.0.2 port 22: Connection timed out"),
            Some(ClassifiedError::Unreachable)
        );
        assert_eq!(
            classify_stderr("ssh: connect to host 10.0.0.2 port 22: No route to host"),
            Some(ClassifiedError::Unreachable)
        );
        assert_eq!(classify_stderr("ssh: something unexpected"), None);
        assert_eq!(classify_stderr(""), None);
    }

    #[test]
    fn missing_client_binary_is_reported() {
        let target = target("hegn");
        assert!(matches!(
            target.probe_with_program(Path::new("/nonexistent/ssh")),
            Err(SshError::MissingClient)
        ));
        assert!(matches!(
            target.interactive_with_program(Path::new("/nonexistent/ssh"), None),
            Err(SshError::MissingClient)
        ));
    }

    #[test]
    fn a_stand_in_program_can_succeed_without_a_guest() {
        let target = target("hegn");
        // `/bin/true` ignores the ssh arguments and exits 0; this exercises the
        // spawn path without opening a network connection.
        if Path::new("/bin/true").exists() {
            target.probe_with_program(Path::new("/bin/true")).unwrap();
        }
    }

    #[test]
    fn zero_timeout_fails_without_spawning_a_client() {
        let target = target("hegn");
        let error = target.wait_ready(Duration::ZERO).unwrap_err();
        match error {
            SshError::Timeout { vm, user, host, .. } => {
                assert_eq!(vm, "rust");
                assert_eq!(user, "hegn");
                assert_eq!(host, Ipv4Addr::new(10, 0, 0, 2));
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    #[test]
    fn exit_status_maps_codes_and_signals() {
        let target = target("hegn");
        assert_eq!(target.exit_code(ExitStatus::from_raw(0)).unwrap(), 0);
        assert_eq!(target.exit_code(ExitStatus::from_raw(1 << 8)).unwrap(), 1);
        assert_eq!(
            target.exit_code(ExitStatus::from_raw(254 << 8)).unwrap(),
            254
        );
        assert_eq!(
            target
                .exit_code(ExitStatus::from_raw(libc::SIGTERM))
                .unwrap(),
            128 + libc::SIGTERM
        );
        assert!(matches!(
            target.exit_code(ExitStatus::from_raw(255 << 8)),
            Err(SshError::TransportAfterConnect { .. })
        ));
    }
}
