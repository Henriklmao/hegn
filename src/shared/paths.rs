//! Path expansion helpers.

use std::path::{Path, PathBuf};

/// Expand a leading `~` or `~/` to the user's home directory.
///
/// A `~user` prefix and tildes in the middle of a path stay untouched.
pub fn expand_home(path: &Path) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    let raw = path.to_string_lossy();
    if raw == "~" {
        home
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        path.to_path_buf()
    }
}

/// Alias used by the project import flow.
pub use self::expand_home as expand_tilde;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_only_touches_a_leading_tilde() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(expand_home(Path::new("~")), home);
        assert_eq!(expand_home(Path::new("~/")), home);
        assert_eq!(
            expand_home(Path::new("~/.config/x")),
            home.join(".config/x")
        );
        for untouched in ["~user/x", "a/~/b", "/tmp/~", "~~"] {
            assert_eq!(expand_home(Path::new(untouched)), PathBuf::from(untouched));
        }
    }
}
