use std::path::{Component, Path, PathBuf, Prefix};

/// Accept Explorer paste syntax, but require a directory below a local drive.
/// Reject network/device namespaces before any filesystem access.
pub fn normalize_source_root(raw: &str) -> Option<PathBuf> {
    let cleaned = raw.trim().trim_matches('"').trim().replace('/', "\\");
    let path = PathBuf::from(cleaned);
    is_local_data_path(&path).then_some(path)
}

pub(crate) fn is_local_data_path(path: &Path) -> bool {
    let mut components = path.components();
    if !matches!(
        components.next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    ) || !matches!(components.next(), Some(Component::RootDir))
    {
        return false;
    }
    let remaining = components.collect::<Vec<_>>();
    !remaining.is_empty()
        && remaining
            .iter()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Resolve aliases so overlapping roots use the same source identity. Validate
/// again after resolution, since a junction can point at a volume or share root.
pub fn resolve_source_root(path: &Path) -> Option<PathBuf> {
    if !is_local_data_path(path) {
        return None;
    }
    let resolved = path.canonicalize().ok()?;
    if !is_local_data_path(&resolved) || !resolved.is_dir() {
        return None;
    }
    let mut components = resolved.components();
    let Component::Prefix(prefix) = components.next()? else {
        return None;
    };
    let drive = match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
        _ => return None,
    };
    // Keep ordinary drive paths for compatibility with existing source IDs.
    let mut normalized = PathBuf::from(format!("{}:\\", char::from(drive)));
    for component in components {
        if let Component::Normal(name) = component {
            normalized.push(name);
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_volume_network_device_and_parent_paths_before_io() {
        for raw in [
            "",
            "relative",
            r"C:\",
            r"C:\\",
            r"C:\.",
            r"\\?\C:\",
            r"\\?\C:\.",
            r"C:\WXWork\..",
            r"\\server\share\WXWork",
            r"\\?\UNC\server\share\WXWork",
            r"\\.\C:\WXWork",
        ] {
            assert!(normalize_source_root(raw).is_none());
        }
        assert_eq!(
            normalize_source_root(" \"D:/WXWork/\" "),
            Some(PathBuf::from(r"D:\WXWork"))
        );
    }

    #[test]
    fn resolves_extended_drive_alias_to_the_same_root() {
        let directory = tempfile::tempdir().unwrap();
        let ordinary = resolve_source_root(directory.path()).unwrap();
        let extended = directory.path().canonicalize().unwrap();
        assert_eq!(resolve_source_root(&extended), Some(ordinary));
    }
}
