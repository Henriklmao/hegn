//! Per-VM SSH identities.
//!
//! Every VM owns a private directory `<vms_dir>/<vm>/ssh/` with an Ed25519
//! keypair and an empty `known_hosts`. The private key is `0600`, the public
//! key `0644`, the directory and `known_hosts` are `0700`/`0600`.
//!
//! [`SshIdentity::ensure`] is used when a VM starts: it creates a missing or
//! repairs a corrupt keypair. [`SshIdentity::load`] is used when attaching: it
//! never regenerates a key, because a new key is unknown to the running guest.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::shared::fs::create_private_dir;

use super::SshError;

/// Private key file name inside the identity directory.
pub const KEY_FILE: &str = "id_ed25519";
/// Public key file name inside the identity directory.
pub const PUBLIC_KEY_FILE: &str = "id_ed25519.pub";
/// Host-key cache file name inside the identity directory.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// A per-VM SSH identity rooted below the configured VM directory.
#[derive(Debug, Clone)]
pub struct SshIdentity {
    vm: String,
    dir: PathBuf,
    key: PathBuf,
    public_key: PathBuf,
    known_hosts: PathBuf,
}

impl SshIdentity {
    /// Describe the identity of `vm` below `vms_dir`.
    ///
    /// The name is validated before it is used as a path component. For an
    /// invalid name the returned paths are harmless placeholders and every
    /// fallible method reports [`SshError::InvalidVmName`]; use one of those
    /// methods (never the raw getters) for untrusted input.
    pub fn new(vms_dir: &Path, vm: &str) -> Self {
        let component = if validate_vm_name(vm).is_ok() {
            vm
        } else {
            ".invalid-vm-name"
        };
        let dir = vms_dir.join(component).join("ssh");
        Self {
            vm: vm.to_string(),
            key: dir.join(KEY_FILE),
            public_key: dir.join(PUBLIC_KEY_FILE),
            known_hosts: dir.join(KNOWN_HOSTS_FILE),
            dir,
        }
    }

    /// Create or repair the identity for a starting VM.
    ///
    /// A missing, unreadable or non-Ed25519 keypair is regenerated. Missing
    /// `known_hosts` is created empty. File and directory modes are always
    /// enforced, including on files that already existed.
    pub fn ensure(&self) -> Result<(), SshError> {
        self.validate()?;
        self.create_dir()?;
        self.ensure_known_hosts()?;

        if !self.keypair_usable()? {
            self.remove_keypair()?;
            self.run_keygen()?;
        }

        self.enforce_modes()?;
        if !self.keypair_usable()? {
            return Err(SshError::KeygenFailed {
                detail: "ssh-keygen did not leave behind a readable Ed25519 keypair".to_string(),
            });
        }
        Ok(())
    }

    /// Load an identity for an attach, without ever regenerating it.
    ///
    /// A key that the running guest does not know cannot be repaired on the
    /// host: the caller has to re-provision the guest or restart it.
    pub fn load(&self) -> Result<(), SshError> {
        self.validate()?;
        if !self.dir_is_valid()? {
            return Err(self.missing());
        }
        if !is_regular_file(&self.key)? || !is_regular_file(&self.public_key)? {
            return Err(self.missing());
        }
        if !self.key_is_valid()? {
            return Err(corrupt(
                &self.vm,
                &self.key,
                "ssh-keygen cannot read the private key",
            ));
        }
        self.read_public_key()?;
        self.enforce_modes()?;
        self.ensure_known_hosts()?;
        Ok(())
    }

    /// Read and validate the public key.
    pub fn public_key(&self) -> Result<String, SshError> {
        self.validate()?;
        if !is_regular_file(&self.public_key)? {
            return Err(self.missing());
        }
        self.read_public_key()
    }

    /// Path of the `known_hosts` file.
    pub fn known_hosts(&self) -> &Path {
        &self.known_hosts
    }

    /// Path of the private key.
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// Path of the identity directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Remove the host-key cache. Missing files are not an error.
    pub fn remove_known_hosts(&self) -> Result<(), SshError> {
        match fs::remove_file(&self.known_hosts) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(&self.known_hosts, error)),
        }
    }

    /// Remove the whole identity directory. Missing paths are not an error.
    ///
    /// A planted symlink is removed as a link, never followed.
    pub fn remove_all(&self) -> Result<(), SshError> {
        match fs::symlink_metadata(&self.dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(&self.dir, error)),
            Ok(metadata) if metadata.is_dir() => {
                fs::remove_dir_all(&self.dir).map_err(|error| io_error(&self.dir, error))
            }
            Ok(_) => fs::remove_file(&self.dir).map_err(|error| io_error(&self.dir, error)),
        }
    }

    fn validate(&self) -> Result<(), SshError> {
        validate_vm_name(&self.vm)
    }

    fn missing(&self) -> SshError {
        SshError::MissingIdentity {
            vm: self.vm.clone(),
            path: self.key.clone(),
        }
    }

    fn create_dir(&self) -> Result<(), SshError> {
        if let Some(vm_dir) = self.dir.parent() {
            refuse_symlink(vm_dir)?;
        }
        create_private_dir(&self.dir).map_err(|error| io_error(&self.dir, error))
    }

    fn dir_is_valid(&self) -> Result<bool, SshError> {
        match fs::symlink_metadata(&self.dir) {
            Ok(metadata) => Ok(metadata.is_dir() && !metadata.file_type().is_symlink()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(&self.dir, error)),
        }
    }

    fn ensure_known_hosts(&self) -> Result<(), SshError> {
        // `O_NOFOLLOW` refuses a planted symlink instead of writing through it.
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.known_hosts)
            .map_err(|error| io_error(&self.known_hosts, error))?;
        set_file_mode(&self.known_hosts, 0o600)
    }

    fn enforce_modes(&self) -> Result<(), SshError> {
        set_file_mode(&self.key, 0o600)?;
        set_file_mode(&self.public_key, 0o644)
    }

    /// Whether the keypair exists and both halves are valid Ed25519 material.
    fn keypair_usable(&self) -> Result<bool, SshError> {
        if !is_regular_file(&self.key)? || !is_regular_file(&self.public_key)? {
            return Ok(false);
        }
        if !self.key_is_valid()? {
            return Ok(false);
        }
        match self.read_public_key() {
            Ok(_) => Ok(true),
            Err(SshError::CorruptIdentity { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn key_is_valid(&self) -> Result<bool, SshError> {
        let output = Command::new("ssh-keygen")
            .arg("-y")
            .arg("-f")
            .arg(&self.key)
            .stdin(Stdio::null())
            .output();
        match output {
            Ok(output) => Ok(output.status.success()
                && String::from_utf8_lossy(&output.stdout).starts_with("ssh-ed25519 ")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(SshError::MissingKeygen)
            }
            Err(error) => Err(io_error(Path::new("ssh-keygen"), error)),
        }
    }

    fn read_public_key(&self) -> Result<String, SshError> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.public_key)
            .map_err(|error| io_error(&self.public_key, error))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|error| io_error(&self.public_key, error))?;
        validate_public_key(&self.vm, &self.public_key, &contents)
    }

    fn remove_keypair(&self) -> Result<(), SshError> {
        for path in [&self.key, &self.public_key] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(path, error)),
            }
        }
        Ok(())
    }

    fn run_keygen(&self) -> Result<(), SshError> {
        let comment = format!("hegn:{}", self.vm);
        let output = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C"])
            .arg(&comment)
            .arg("-f")
            .arg(&self.key)
            .stdin(Stdio::null())
            .output();
        match output {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(SshError::KeygenFailed {
                detail: stderr_detail(&output.stderr),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(SshError::MissingKeygen)
            }
            Err(error) => Err(io_error(Path::new("ssh-keygen"), error)),
        }
    }
}

fn validate_vm_name(vm: &str) -> Result<(), SshError> {
    let invalid = vm.is_empty()
        || vm == "."
        || vm == ".."
        || vm.starts_with('.')
        || vm.contains('/')
        || vm.contains('\\')
        || vm.contains('\0')
        || vm.trim() != vm;
    if invalid {
        return Err(SshError::InvalidVmName { vm: vm.to_string() });
    }
    Ok(())
}

fn validate_public_key(vm: &str, path: &Path, contents: &str) -> Result<String, SshError> {
    let line = contents.trim();
    if line.is_empty() {
        return Err(corrupt(vm, path, "the public key file is empty"));
    }
    if line.lines().count() != 1 {
        return Err(corrupt(
            vm,
            path,
            "the public key file must contain exactly one line",
        ));
    }
    let Some(rest) = line.strip_prefix("ssh-ed25519 ") else {
        return Err(corrupt(
            vm,
            path,
            "the public key is not an ssh-ed25519 key",
        ));
    };
    if rest.trim().is_empty() {
        return Err(corrupt(vm, path, "the public key has no key material"));
    }
    Ok(line.to_string())
}

fn corrupt(vm: &str, path: &Path, detail: &str) -> SshError {
    SshError::CorruptIdentity {
        vm: vm.to_string(),
        path: path.to_path_buf(),
        detail: detail.to_string(),
    }
}

fn io_error(path: &Path, source: std::io::Error) -> SshError {
    SshError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn is_regular_file(path: &Path) -> Result<bool, SshError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(path, error)),
    }
}

fn refuse_symlink(path: &Path) -> Result<(), SshError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing symlinked SSH path",
            ),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
    }
}

fn set_file_mode(path: &Path, mode: u32) -> Result<(), SshError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing non-regular SSH file",
            ),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| io_error(path, error))
}

fn stderr_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        "unknown error".to_string()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn keygen_available() -> bool {
        Command::new("ssh-keygen")
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn identity(dir: &Path) -> SshIdentity {
        SshIdentity::new(dir, "rust")
    }

    #[test]
    fn ensure_creates_private_identity_files() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());

        identity.ensure().unwrap();

        assert!(identity.key().is_file());
        assert!(identity.dir().join(PUBLIC_KEY_FILE).is_file());
        assert!(identity.known_hosts().is_file());
        assert_eq!(mode(identity.dir()), 0o700);
        assert_eq!(mode(identity.key()), 0o600);
        assert_eq!(mode(&identity.dir().join(PUBLIC_KEY_FILE)), 0o644);
        assert_eq!(mode(identity.known_hosts()), 0o600);
        assert!(
            fs::read_to_string(identity.known_hosts())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn public_key_is_a_single_ed25519_line() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());
        identity.ensure().unwrap();

        let public = identity.public_key().unwrap();
        assert!(public.starts_with("ssh-ed25519 "));
        assert_eq!(public.lines().count(), 1);
    }

    #[test]
    fn ensure_regenerates_a_corrupt_key_but_load_reports_it() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());
        identity.ensure().unwrap();
        let original = fs::read(identity.key()).unwrap();
        fs::write(identity.key(), b"not an ed25519 key").unwrap();

        assert!(matches!(
            identity.load(),
            Err(SshError::CorruptIdentity { .. })
        ));

        identity.ensure().unwrap();
        assert_ne!(fs::read(identity.key()).unwrap(), original);
        identity.load().unwrap();
    }

    #[test]
    fn load_reports_missing_identity_and_ensure_creates_it() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());
        identity.ensure().unwrap();
        fs::remove_file(identity.key()).unwrap();

        assert!(matches!(
            identity.load(),
            Err(SshError::MissingIdentity { .. })
        ));

        identity.ensure().unwrap();
        identity.load().unwrap();
    }

    #[test]
    fn ensure_tightens_modes_on_existing_files() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());
        identity.ensure().unwrap();
        let public = identity.dir().join(PUBLIC_KEY_FILE);
        fs::set_permissions(identity.key(), fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o666)).unwrap();
        fs::set_permissions(identity.known_hosts(), fs::Permissions::from_mode(0o666)).unwrap();

        identity.ensure().unwrap();

        assert_eq!(mode(identity.key()), 0o600);
        assert_eq!(mode(&public), 0o644);
        assert_eq!(mode(identity.known_hosts()), 0o600);
    }

    #[test]
    fn removal_helpers_are_idempotent() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());
        identity.ensure().unwrap();

        identity.remove_known_hosts().unwrap();
        identity.remove_known_hosts().unwrap();
        assert!(!identity.known_hosts().exists());

        identity.remove_all().unwrap();
        identity.remove_all().unwrap();
        assert!(!identity.dir().exists());
    }

    #[test]
    fn invalid_vm_names_are_rejected_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["", ".", "..", "../escape", "a/b", "a\\b", " two"] {
            let identity = SshIdentity::new(dir.path(), name);
            assert!(
                matches!(identity.ensure(), Err(SshError::InvalidVmName { .. })),
                "ensure accepted {name:?}"
            );
            assert!(
                matches!(identity.load(), Err(SshError::InvalidVmName { .. })),
                "load accepted {name:?}"
            );
        }
        assert!(!dir.path().join("escape").exists());
    }

    #[test]
    fn symlinked_known_hosts_is_refused() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());
        identity.ensure().unwrap();
        identity.remove_known_hosts().unwrap();

        let target = dir.path().join("planted");
        fs::write(&target, b"attacker-controlled").unwrap();
        std::os::unix::fs::symlink(&target, identity.known_hosts()).unwrap();

        assert!(matches!(identity.ensure(), Err(SshError::Io { .. })));
        assert_eq!(fs::read(&target).unwrap(), b"attacker-controlled");
    }
}
