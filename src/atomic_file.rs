/// Atomically write `content` to `path` by writing to a temporary file
/// first and then renaming it into place.
pub fn save_atomic(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    let mut tmp_path = path.to_path_buf();
    tmp_path.set_extension("tmp");
    std::fs::write(&tmp_path, content)?;
    set_secure_permissions(&tmp_path)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn set_secure_permissions(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_atomic_write_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save_atomic(&path, "key = \"value\"").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "key = \"value\"");
    }

    #[test]
    fn save_atomic_failure_no_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-dir").join("file");
        std::fs::write(dir.path().join("not-a-dir"), "content").unwrap();
        assert!(save_atomic(&path, "content").is_err());
    }

    #[test]
    fn save_atomic_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old").unwrap();
        save_atomic(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }
}
