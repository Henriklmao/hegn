//! SSH transport for guests over the control TAP.
//!
//! Every VM owns a host-side SSH identity in `<vms_dir>/<vm>/ssh/`. hegn
//! never falls back to a host shell: an SSH target points at the VM's control
//! TAP address and is only usable while the VM is running.
//!
//! - [`identity`] — per-VM keypair and `known_hosts`
//! - [`client`] — the OpenSSH client wrapper, readiness probe and error
//!   classification
//! - [`title`] — RAII terminal-title guard for interactive sessions

mod client;
mod identity;
mod title;

pub use client::{ClassifiedError, GuestEnv, SshTarget, classify_stderr};
pub use identity::SshIdentity;
pub use title::{TitleGuard, reset_sequence, title_sequence};

use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

/// Failures while preparing an identity or driving the OpenSSH client.
#[derive(Debug, Error)]
pub enum SshError {
    #[error("OpenSSH key generator 'ssh-keygen' not found. Install openssh.")]
    MissingKeygen,

    #[error("ssh-keygen failed: {detail}")]
    KeygenFailed { detail: String },

    #[error(
        "SSH identity for VM '{vm}' is missing at {path}. The VM was probably started before SSH \
         support. Run 'hegn vm apply {vm}' and restart the VM."
    )]
    MissingIdentity { vm: String, path: PathBuf },

    #[error(
        "SSH identity for VM '{vm}' is corrupt at {path} ({detail}). The VM was probably started \
         before SSH support. Run 'hegn vm apply {vm}' and restart the VM."
    )]
    CorruptIdentity {
        vm: String,
        path: PathBuf,
        detail: String,
    },

    #[error("SSH I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("invalid VM name {vm:?}: expected a single path component")]
    InvalidVmName { vm: String },

    #[error("invalid SSH user {user:?}: expected [a-z0-9_-] without a leading '-' or whitespace")]
    InvalidUser { user: String },

    #[error("OpenSSH client 'ssh' not found. Install openssh.")]
    MissingClient,

    #[error(
        "VM '{vm}': ssh to {user}@{host} was refused. The VM may have been started before SSH \
         support, or sshd is not ready. Try 'hegn attach {vm} --serial', or 'hegn vm apply {vm}' \
         and restart the VM."
    )]
    ConnectionRefused {
        vm: String,
        user: String,
        host: Ipv4Addr,
    },

    #[error(
        "VM '{vm}': permission denied for {user}@{host}. The guest key may be stale. Run \
         'hegn vm apply {vm}' and restart the VM."
    )]
    PermissionDenied {
        vm: String,
        user: String,
        host: Ipv4Addr,
    },

    #[error(
        "VM '{vm}': host key verification failed for {host}. Delete {known_hosts} or run \
         'hegn vm apply {vm}' and restart the VM."
    )]
    HostKeyVerification {
        vm: String,
        host: Ipv4Addr,
        known_hosts: PathBuf,
    },

    #[error("VM '{vm}': {user}@{host} is unreachable (connection timed out or no route to host).")]
    Unreachable {
        vm: String,
        user: String,
        host: Ipv4Addr,
    },

    #[error("ssh transport failed: {stderr}")]
    Transport { stderr: String },

    #[error("VM '{vm}' did not accept ssh as {user}@{host} within {timeout:?}: {last}")]
    Timeout {
        vm: String,
        user: String,
        host: Ipv4Addr,
        timeout: Duration,
        last: String,
    },

    #[error(
        "VM '{vm}': ssh to {user}@{host} failed after connecting. Try 'hegn attach {vm} --serial', \
         or run 'hegn vm apply {vm}' and restart the VM."
    )]
    TransportAfterConnect {
        vm: String,
        user: String,
        host: Ipv4Addr,
    },
}
