//! Deprecated `config.toml` keys.
//!
//! The v2 schema renames three global keys. They keep serde aliases so an
//! upgrade cannot brick startup, but [`super::Config::load`] warns once per key
//! that is still present in the file. Detection works on the raw TOML because
//! the deserialized [`super::Config`] no longer remembers which alias was used
//! and must not gain a serialized field for it.

/// Keys that are still accepted but deprecated, paired with their replacement.
const DEPRECATED_KEYS: &[(&str, &str)] = &[
    ("instances_dir", "vms_dir"),
    ("sandboxes_dir", "vms_dir"),
    ("shared_tag", "share_tag_prefix"),
    ("guest_mount", "guest_projects_base"),
];

/// Deprecated top-level keys present in `raw`.
///
/// The result follows [`DEPRECATED_KEYS`] order and is empty when the document
/// is not valid TOML; the real parser reports that error separately.
pub fn deprecated_keys(raw: &str) -> Vec<&'static str> {
    let Ok(value) = toml::from_str::<toml::Value>(raw) else {
        return Vec::new();
    };
    let Some(table) = value.as_table() else {
        return Vec::new();
    };
    DEPRECATED_KEYS
        .iter()
        .filter(|(key, _)| table.contains_key(*key))
        .map(|(key, _)| *key)
        .collect()
}

/// The replacement key for a deprecated key returned by [`deprecated_keys`].
pub(super) fn replacement(key: &str) -> &'static str {
    DEPRECATED_KEYS
        .iter()
        .find(|(deprecated, _)| *deprecated == key)
        .map(|(_, replacement)| *replacement)
        .unwrap_or("the documented key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_deprecated_key_once() {
        let raw = r#"
instances_dir = "/data/instances"
sandboxes_dir = "/data/sandboxes"
shared_tag = "workspace"
guest_mount = "/workspace"
vcpus = 4
"#;
        assert_eq!(
            deprecated_keys(raw),
            vec![
                "instances_dir",
                "sandboxes_dir",
                "shared_tag",
                "guest_mount"
            ]
        );
    }

    #[test]
    fn ignores_new_keys_and_invalid_toml() {
        let raw = r#"
vms_dir = "/data/vms"
share_tag_prefix = "hegn-prj"
guest_projects_base = "/projects"
"#;
        assert!(deprecated_keys(raw).is_empty());
        assert!(deprecated_keys("not = [valid").is_empty());
    }

    #[test]
    fn reports_the_replacement_name() {
        assert_eq!(replacement("instances_dir"), "vms_dir");
        assert_eq!(replacement("shared_tag"), "share_tag_prefix");
        assert_eq!(replacement("guest_mount"), "guest_projects_base");
    }
}
