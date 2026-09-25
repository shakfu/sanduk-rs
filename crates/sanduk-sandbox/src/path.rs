use std::io;
use std::path::{Path, PathBuf};

/// Resolves `raw` against `root` and refuses it outside the root or on a protected component.
///
/// An existing path is resolved whole; a new one through its nearest existing parent. This
/// follows symlinks and removes `..`, so the check is about the filesystem location. `root` must
/// already be resolved, as [`crate::Policy::root`] is.
///
/// For writes a program makes in its own process, which the kernel policy does not reach. Unlike
/// that policy it has a window between this check and the open.
pub fn confine_path(root: &Path, raw: &str) -> io::Result<PathBuf> {
    let requested = Path::new(raw);
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    let mut existing = requested.clone();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Err(refused(format!("path {raw:?} has no existing parent")));
        };
        missing.push(name.to_os_string());
        existing.pop();
    }
    let mut resolved = std::fs::canonicalize(&existing)
        .map_err(|e| io::Error::new(e.kind(), format!("resolving {raw}: {e}")))?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    if !resolved.starts_with(root) {
        return Err(refused(format!(
            "path {raw:?} is outside root {}",
            root.display()
        )));
    }
    if let Some(name) = protected(root, &resolved) {
        return Err(refused(format!(
            "path {raw:?} is protected: {name} is not writable"
        )));
    }
    Ok(resolved)
}

fn refused(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

/// The protected component of a resolved path, if it has one.
///
/// `.git` because a damaged object store loses history nothing can rebuild, and `.env` because a
/// replaced secret is not in the repository to restore. `.git` matches whole, leaving `.github`
/// and `.gitignore` alone. `.env` matches as a prefix, so `.env.production` is covered, minus the
/// names that hold no secret by convention: a template and `.envrc` are committed files. Only
/// components below the root count.
///
/// The kernel policy ignores this on both platforms. Landlock grants access by union over the
/// rules met walking a path, so the rule granting the root cannot have a hole cut in it, and a
/// boundary that held only on macOS would be worse than none.
fn protected(root: &Path, resolved: &Path) -> Option<String> {
    const COMMITTED: [&str; 3] = ["example", "sample", "template"];
    let relative = resolved.strip_prefix(root).ok()?;
    relative.components().find_map(|component| {
        let name = component.as_os_str().to_str()?;
        let secret = name.starts_with(".env")
            && name != ".envrc"
            && !COMMITTED.iter().any(|kind| name.contains(kind));
        (name == ".git" || secret).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("sanduk-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(std::fs::canonicalize(dir).unwrap())
        }

        fn file(&self, name: &str) -> String {
            self.0.join(name).display().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_path_outside_root_is_refused() {
        let root = Scratch::new("root-boundary");
        let outside = Scratch::new("outside-boundary");
        let path = outside.file("secret.txt");
        std::fs::write(&path, "secret").unwrap();

        let err = confine_path(&root.0, &path).unwrap_err();
        assert!(err.to_string().contains("outside root"), "{err}");
    }

    #[test]
    fn a_protected_path_under_the_root_is_refused_and_a_lookalike_is_not() {
        let dir = Scratch::new("protected");
        std::fs::create_dir_all(dir.0.join(".git")).unwrap();

        for path in [".git", ".git/config", ".env", ".env.local", "sub/.env"] {
            let err = confine_path(&dir.0, path).unwrap_err();
            assert!(err.to_string().contains("is protected"), "{path}: {err}");
        }
        for path in [
            ".github/ci.yml",
            ".gitignore",
            "env.sample",
            "src/environment.rs",
            ".env.example",
            ".env.local.template",
            ".envrc",
        ] {
            confine_path(&dir.0, path).unwrap_or_else(|e| panic!("{path}: {e}"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_outside_root_is_refused() {
        let root = Scratch::new("root-symlink");
        let outside = Scratch::new("outside-symlink");
        let target = outside.file("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        let link = root.file("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = confine_path(&root.0, &link).unwrap_err();
        assert!(err.to_string().contains("outside root"), "{err}");
    }
}
