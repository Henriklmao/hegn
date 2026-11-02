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
    if !crate::shared::process::executable_matches(pid, binary) {
        return false;
    }
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    cmdline
        .split(|byte| *byte == 0)
        .any(|arg| arg == INTERNAL_ARG.as_bytes())
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

    #[test]
    fn duplicate_flags_take_the_last_value() {
        let args = args_from(&[
            "--backend",
            "/first.sock",
            "--frontend",
            "/console.sock",
            "--log",
            "/log",
            "--backend",
            "/second.sock",
        ]);
        assert_eq!(args.backend, PathBuf::from("/second.sock"));
    }

    #[test]
    fn every_flag_is_required() {
        for values in [
            vec!["--frontend", "/c", "--log", "/l"],
            vec!["--backend", "/b", "--log", "/l"],
            vec!["--backend", "/b", "--frontend", "/c"],
        ] {
            assert!(parse_args(values.into_iter().map(Into::into)).is_err());
        }
    }

    #[test]
    fn empty_paths_are_rejected() {
        let err = parse_args(
            ["--backend", "", "--frontend", "/c", "--log", "/l"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn a_flag_used_as_a_value_is_not_recognised_as_a_flag() {
        // "--frontend" becomes the backend value; the next token is then an
        // unknown flag rather than a silently swapped path.
        let err = parse_args(
            ["--backend", "--frontend", "/c"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown relay argument"), "{err}");
    }

    #[test]
    fn non_utf8_flags_are_refused() {
        use std::os::unix::ffi::OsStringExt;
        let flag = std::ffi::OsString::from_vec(vec![0xff, 0xfe]);
        let err = parse_args([flag].into_iter()).unwrap_err().to_string();
        assert!(err.contains("unknown relay argument"), "{err}");
    }

    #[test]
    fn process_match_requires_a_binary_with_a_file_name() {
        // "/" has no file name, so it can never match a process argv0.
        assert!(!process_matches_relay(std::process::id(), Path::new("/")));
    }
}
