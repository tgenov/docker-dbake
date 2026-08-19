use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Characters kept verbatim in a log file name. Everything else is replaced,
/// so a bake target called `../../etc/passwd` cannot escape the log directory.
fn is_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Turn an arbitrary bake target name into a single safe path component.
///
/// Bake target names are arbitrary strings, so they cannot be used as a path
/// component as-is: `target "../../etc/cron.d/x"` is a legal HCL target.
pub fn safe_file_stem(target: &str) -> String {
    let mut mapped: String = target
        .chars()
        .map(|c| if is_safe(c) { c } else { '_' })
        .collect();

    // Dots survive the character rule but `..` is a traversal component and a
    // leading dot only makes a hidden file.
    while mapped.contains("..") {
        mapped = mapped.replace("..", "__");
    }
    if mapped.starts_with('.') {
        mapped.replace_range(0..1, "_");
    }

    if mapped.is_empty() {
        "_".to_string()
    } else {
        mapped
    }
}

/// Root directory for this run's logs.
///
/// Scoped by process id so concurrent runs cannot interleave their output into
/// the same file, and so a previous run's logs survive for comparison.
pub fn run_log_dir(run_id: u32) -> PathBuf {
    std::env::temp_dir()
        .join("dbake-logs")
        .join(run_id.to_string())
}

/// Path of the log file for one target inside a run directory.
pub fn log_path(run_dir: &Path, target: &str) -> PathBuf {
    run_dir.join(format!("{}.log", safe_file_stem(target)))
}

/// Create the run's log directory, restricted to the current user.
///
/// `/tmp` is world-writable on Linux, so both levels matter: an attacker who
/// pre-creates `/tmp/dbake-logs` — or the run directory itself — as a symlink
/// would otherwise receive every build log, and `set_permissions` follows
/// symlinks, so it would chmod their target rather than ours.
pub fn create_run_dir(run_dir: &Path) -> Result<()> {
    if let Some(parent) = run_dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
        ensure_private_dir(parent)?;
    }

    match std::fs::create_dir(run_dir) {
        Ok(()) => {}
        // Our own directory, from an earlier stage of the same run.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to create log directory {}", run_dir.display()))
        }
    }
    ensure_private_dir(run_dir)?;

    Ok(())
}

/// Verify a directory is a real directory we own, then restrict it to 0700.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    // symlink_metadata does not follow links, so a planted symlink is visible
    // rather than silently redirecting the chmod onto its target.
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("cannot stat log directory {}", dir.display()))?;

    if meta.file_type().is_symlink() {
        anyhow::bail!(
            "refusing to write logs through the symlink {} — remove it",
            dir.display()
        );
    }
    if !meta.is_dir() {
        anyhow::bail!("log path {} exists and is not a directory", dir.display());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        // SAFETY: geteuid takes no arguments and cannot fail.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            anyhow::bail!(
                "log directory {} is owned by uid {}, not {} — refusing to use it",
                dir.display(),
                meta.uid(),
                me
            );
        }

        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to restrict permissions on {}", dir.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_unchanged() {
        assert_eq!(safe_file_stem("web"), "web");
        assert_eq!(safe_file_stem("api-v2.1_x"), "api-v2.1_x");
    }

    #[test]
    fn path_separators_are_replaced() {
        assert_eq!(safe_file_stem("a/b"), "a_b");
        let stem = safe_file_stem("../../etc/passwd");
        assert!(!stem.contains('/'), "{stem}");
        assert!(!stem.contains(".."), "{stem}");
        assert!(stem.ends_with("etc_passwd"), "{stem}");
    }

    #[test]
    fn traversal_cannot_escape_the_run_directory() {
        let dir = Path::new("/tmp/dbake-logs/42");
        for hostile in [
            "../../../etc/cron.d/evil",
            "..",
            "./../x",
            "/absolute/path",
            "a\\b",
        ] {
            let p = log_path(dir, hostile);
            assert_eq!(p.parent().unwrap(), dir, "escaped with {hostile:?}");
            assert_eq!(p.components().count(), dir.components().count() + 1);
            assert!(!p.to_string_lossy().contains(".."), "{hostile:?}");
        }
    }

    #[test]
    fn dot_names_are_not_left_as_dots() {
        assert_eq!(safe_file_stem("."), "_");
        assert_eq!(safe_file_stem(".."), "__");
        assert_eq!(safe_file_stem(""), "_");
        // A legitimate dot inside a name is preserved.
        assert_eq!(safe_file_stem("api-v2.1"), "api-v2.1");
    }

    #[test]
    fn unicode_and_shell_metacharacters_are_replaced() {
        assert_eq!(safe_file_stem("café;rm -rf"), "caf__rm_-rf");
        assert!(!safe_file_stem("a\0b").contains('\0'));
    }

    #[test]
    fn runs_get_separate_directories() {
        assert_ne!(run_log_dir(1), run_log_dir(2));
        assert!(run_log_dir(7).ends_with("7"));
    }

    /// Removes its directory even if the test body panics.
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
            std::fs::remove_file(&self.0).ok();
        }
    }

    fn scratch(name: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("dbake-test-{}-{}", std::process::id(), name));
        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_file(&d).ok();
        TempDir(d)
    }

    #[test]
    fn create_run_dir_is_idempotent_and_private() {
        let root = scratch("private");
        let run_dir = root.0.join("77");
        create_run_dir(&run_dir).unwrap();
        create_run_dir(&run_dir).unwrap();
        assert!(run_dir.is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The shared parent matters as much as the leaf: it is the
            // component an attacker can pre-create in a world-writable /tmp.
            for dir in [&run_dir, &root.0] {
                let mode = std::fs::metadata(dir).unwrap().permissions().mode();
                assert_eq!(
                    mode & 0o777,
                    0o700,
                    "{} must not be group/world accessible",
                    dir.display()
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_write_through_a_planted_parent_symlink() {
        let root = scratch("symlink-parent");
        let elsewhere = scratch("symlink-target");
        std::fs::create_dir_all(&elsewhere.0).unwrap();
        std::os::unix::fs::symlink(&elsewhere.0, &root.0).unwrap();

        let err = create_run_dir(&root.0.join("77")).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_when_the_run_directory_itself_is_a_symlink() {
        let root = scratch("run-symlink");
        let elsewhere = scratch("run-symlink-target");
        std::fs::create_dir_all(&root.0).unwrap();
        std::fs::create_dir_all(&elsewhere.0).unwrap();
        let run_dir = root.0.join("77");
        std::os::unix::fs::symlink(&elsewhere.0, &run_dir).unwrap();

        let err = create_run_dir(&run_dir).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn refuses_when_the_log_path_is_a_file() {
        let root = scratch("notadir");
        std::fs::write(&root.0, b"not a directory").unwrap();
        let err = create_run_dir(&root.0.join("77")).unwrap_err().to_string();
        assert!(!err.is_empty());
    }
}
