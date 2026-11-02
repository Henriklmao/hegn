//! Internal relay arguments and recognition of a running relay process.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::INTERNAL_ARG;
use super::server::run;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayArgs {
    /// Cloud Hypervisor serial socket (the relay is its only client).
    pub backend: PathBuf,
    /// Socket that `hegn attach` connects to.
    pub frontend: PathBuf,
    /// Append-only session log.
    pub log: PathBuf,
}

/// Is this process the internal relay?
pub fn requested() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == INTERNAL_ARG)
}

pub fn run_from_current_args() -> Result<()> {
    let args =
        parse_args(std::env::args_os().skip(2)).context("invalid console relay arguments")?;
    run(args)
}

fn parse_args(args: impl Iterator<Item = std::ffi::OsString>) -> Result<RelayArgs> {
    let mut backend = None;
    let mut frontend = None;
    let mut log = None;
    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        let target = match flag.to_str() {
            Some("--backend") => &mut backend,
            Some("--frontend") => &mut frontend,
            Some("--log") => &mut log,
            _ => bail!("unknown relay argument {flag:?}"),
        };
        *target = Some(PathBuf::from(
            args.next().context("missing value for relay argument")?,
        ));
    }
    let args = RelayArgs {
        backend: backend.context("missing --backend")?,
        frontend: frontend.context("missing --frontend")?,
        log: log.context("missing --log")?,
    };
    if args.backend.as_os_str().is_empty()
        || args.frontend.as_os_str().is_empty()
        || args.log.as_os_str().is_empty()
    {
        bail!("relay paths must not be empty");
    }
    Ok(args)
}

/// Does `pid` still run the internal console relay?
pub fn process_matches_relay(pid: u32, binary: &Path) -> bool {
    if !crate::vm::instance::process_alive(pid) {
        return false;
    }
    let Some(expected) = binary.file_name() else {
        return false;
    };
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
    let argv0 = args.first().copied().unwrap_or(&[]);
    let argv0 = String::from_utf8_lossy(argv0);
    let is_binary = Path::new(argv0.as_ref())
        .file_name()
        .is_some_and(|actual| actual == expected);
    is_binary && args.iter().any(|arg| *arg == INTERNAL_ARG.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_from(values: &[&str]) -> RelayArgs {
        parse_args(values.iter().map(std::ffi::OsString::from)).unwrap()
    }

    #[test]
    fn parses_internal_arguments() {
        let args = args_from(&[
            "--backend",
            "/run/serial.sock",
            "--frontend",
            "/run/console.sock",
            "--log",
            "/logs/session.log",
        ]);
        assert_eq!(args.backend, PathBuf::from("/run/serial.sock"));
        assert_eq!(args.frontend, PathBuf::from("/run/console.sock"));
        assert_eq!(args.log, PathBuf::from("/logs/session.log"));
        assert!(parse_args(["--backend".into()].into_iter()).is_err());
        assert!(parse_args(["--unknown".into(), "x".into()].into_iter()).is_err());
    }

    #[test]
    fn process_match_requires_the_internal_argument() {
        let exe = std::env::current_exe().unwrap();
        // The test binary is not the relay, even though the pid is alive.
        assert!(!process_matches_relay(std::process::id(), &exe));
        assert!(!process_matches_relay(4_000_000_000, &exe));
    }
}
