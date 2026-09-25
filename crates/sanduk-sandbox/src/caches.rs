//! Paths outside the root that stay writable. A shell needs the temp directory and `/dev/null`;
//! a build needs the ecosystem caches. Measured 2026-09-19: an offline `cargo build` opens
//! `$CARGO_HOME/.package-cache` with `O_RDWR|O_CREAT` on every run, so a policy without the
//! caches denies the build, not just the dependency fetch. A lost cache costs a re-download
//! rather than work, which is why they sit on the permissive side of the line.
//!
//! `bin/` under `$CARGO_HOME` and `$GOPATH` is not granted: it is on `PATH`, so a file written
//! there runs unconfined in the next shell.

use std::path::{Path, PathBuf};

/// The fixed write set outside the root, resolved. What does not resolve is dropped, except the
/// cargo and go entries, which Seatbelt can still grant the creation of.
pub fn writable() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    [
        // Reads TMPDIR, which on macOS is a per-user path under /var/folders rather than /tmp.
        Some(std::env::temp_dir()),
        Some(PathBuf::from("/dev/null")),
        named("XDG_CACHE_HOME", ".cache"),
        home.as_ref().map(|h| h.join(".npm")),
    ]
    .into_iter()
    .flatten()
    .chain(platform_caches())
    // Canonical, because Seatbelt matches a profile against the resolved path: on macOS `/tmp` is
    // a symlink to `/private/tmp`, and `$TMPDIR` carries a trailing slash that `subpath` will not
    // match. Dropping what does not resolve also drops what does not exist.
    .filter_map(|path| std::fs::canonicalize(path).ok())
    .chain(cargo_home().map_or_else(Vec::new, |h| children(&h, CARGO_CACHES)))
    .chain(go_caches(gopath()))
    .chain(library_caches(home.as_deref()))
    .collect()
}

/// Creates the cache entries the policy grants, under a `$CARGO_HOME` or `$GOPATH` that exists.
/// Once confined, cargo cannot create `registry/` in a directory it may not write, nor go
/// `pkg/mod`, so a fresh install would fail its first fetch; Landlock also drops a path that does
/// not exist yet. `.global-cache` is left to cargo, which creates it as a database. Best effort:
/// what cannot be created is denied later.
///
/// `$XDG_CACHE_HOME` too, and only when its parent exists. Measured 2026-09-22 on a CI runner with
/// no `~/.cache`: go fetched the module, then failed `mkdir ~/.cache` for its build cache.
pub fn create() {
    if let Some(cache) = named("XDG_CACHE_HOME", ".cache") {
        let _ = std::fs::create_dir(cache);
    }
    create_under(
        cargo_home().as_deref(),
        &["registry", "git"],
        &[".package-cache", ".package-cache-mutate"],
    );
    create_under(gopath().as_deref(), &["pkg/mod", "pkg/sumdb"], &[]);
}

/// `$var`, or `under_home` below `$HOME` when it is unset.
fn named(var: &str, under_home: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(under_home)))
}

fn cargo_home() -> Option<PathBuf> {
    named("CARGO_HOME", ".cargo")
}

/// The first entry: Go keeps `pkg/` there when `$GOPATH` is a list.
fn gopath() -> Option<PathBuf> {
    named("GOPATH", "go").and_then(|p| std::env::split_paths(&p).next())
}

/// Nothing when `base` is missing: no `~/.cargo` is created for someone without Rust.
/// A file is opened for append, so one that exists is never truncated.
fn create_under(base: Option<&Path>, dirs: &[&str], files: &[&str]) {
    let Some(base) = base.filter(|b| b.is_dir()) else {
        return;
    };
    for dir in dirs {
        let _ = std::fs::create_dir_all(base.join(dir));
    }
    for file in files {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(base.join(file));
    }
}

/// Resolved paths under `base`, kept when they do not exist yet: Seatbelt can still grant their
/// creation, and Landlock drops them.
fn children(base: &Path, names: &[&str]) -> Vec<PathBuf> {
    let Ok(base) = std::fs::canonicalize(base) else {
        return Vec::new();
    };
    names
        .iter()
        .map(|name| base.join(name))
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .collect()
}

/// What cargo writes under `$CARGO_HOME` when it builds or fetches, and nothing else: `cargo
/// install` is a global change. Measured 2026-09-22 on macOS: without the lock files cargo warns
/// and runs unlocked, and without the journal it cannot record last use for its garbage
/// collector. The journal exists only during a write; Landlock drops a path that does not exist,
/// so on Linux that record is lost and the build still succeeds.
const CARGO_CACHES: &[&str] = &[
    "registry",
    "git",
    ".package-cache",
    ".package-cache-mutate",
    ".global-cache",
    ".global-cache-journal",
];

/// The module cache and the checksum database's state under the first `$GOPATH` entry. Measured
/// 2026-09-22 on macOS: without `pkg/sumdb` every new fetch fails verifying the module.
/// `$GOMODCACHE` moves the module cache.
fn go_caches(gopath: Option<PathBuf>) -> Vec<PathBuf> {
    let mut paths = gopath.map_or_else(Vec::new, |g| children(&g, &["pkg/mod", "pkg/sumdb"]));
    paths.extend(
        std::env::var_os("GOMODCACHE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .map(|c| std::fs::canonicalize(&c).unwrap_or(c)),
    );
    paths
}

/// The per-user cache directory sits beside `$TMPDIR` under `/var/folders`, not inside it.
/// Measured 2026-09-21: `swiftc` fails without it, unable to write its clang module cache there.
#[cfg(target_os = "macos")]
fn platform_caches() -> Vec<PathBuf> {
    user_cache_dir().into_iter().collect()
}

#[cfg(not(target_os = "macos"))]
fn platform_caches() -> Vec<PathBuf> {
    Vec::new()
}

/// The toolchain entries of `~/Library/Caches`, the macOS half of `$XDG_CACHE_HOME`, and not the
/// directory: every app on the machine keeps its cache there. Measured 2026-09-22 with the rest
/// denied: `ccache` and `deno` fail without theirs; go, pip, python and swiftpm run uncached.
/// A relocated cache (`$GOCACHE`, `$PIP_CACHE_DIR`, ...) needs [`crate::Policy::writable`].
#[cfg(target_os = "macos")]
fn library_caches(home: Option<&Path>) -> Vec<PathBuf> {
    home.map_or_else(Vec::new, |h| {
        children(
            &h.join("Library/Caches"),
            &[
                "go-build",
                "pip",
                "com.apple.python",
                "org.swift.swiftpm",
                "ccache",
                "deno",
            ],
        )
    })
}

#[cfg(not(target_os = "macos"))]
fn library_caches(_home: Option<&Path>) -> Vec<PathBuf> {
    Vec::new()
}

/// `confstr(_CS_DARWIN_USER_CACHE_DIR)`, the directory under `/var/folders` beside `$TMPDIR`.
#[cfg(target_os = "macos")]
pub fn user_cache_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: confstr writes at most `buf.len()` bytes, NUL included, into a buffer we own.
    let len = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_CACHE_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    // 0 is failure; a length past the buffer means the value was cut.
    if len == 0 || len > buf.len() {
        return None;
    }
    let path = std::ffi::OsStr::from_bytes(&buf[..len - 1]);
    Some(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_entries_are_created_without_truncating_or_creating_the_base() {
        let base = std::env::temp_dir().join(format!("sanduk-caches-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join(".lock"), "held").unwrap();
        create_under(Some(&base), &["a/b"], &[".lock", ".new"]);
        let missing = base.join("missing");
        create_under(Some(&missing), &["a"], &[".new"]);

        let kept = std::fs::read_to_string(base.join(".lock")).unwrap();
        let (dir, new) = (base.join("a/b").is_dir(), base.join(".new").is_file());
        let base_created = missing.exists();
        let _ = std::fs::remove_dir_all(&base);
        assert!(dir && new, "an entry was not created");
        assert_eq!(kept, "held", "an existing file was truncated");
        assert!(!base_created, "a missing base was created");
    }
}
