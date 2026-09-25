//! Confine a child process's writes to one directory.
//!
//! A [`Policy`] names a root. [`Policy::command`] returns a `std::process::Command` whose process,
//! and every descendant, may write only under the root, the temp directory, `/dev/null`, the
//! toolchain caches (see [`caches`]) and any directory added with [`Policy::writable`]. Reads are
//! never restricted, and neither is the network.
//!
//! | Platform | Mechanism | Requires |
//! |-|-|-|
//! | Linux | Landlock | kernel 6.2 (ABI 3) |
//! | macOS | Seatbelt, via `/usr/bin/sandbox-exec` | -- |
//!
//! Both are enforced by the kernel at `open`, inherited across `fork` and `exec`, and cannot be
//! lifted by the process they apply to. The policy is the intersection of what both enforce, so a
//! command behaves the same on either platform. macOS also denies preference writes, signals to
//! processes outside the sandbox, and `open`, three routes the file rules cannot see.
//!
//! [`confine_path`] is the userspace counterpart, for writes a program makes in its own process,
//! which the kernel policy does not reach.

pub mod caches;
mod path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use path::confine_path;

#[cfg(target_os = "macos")]
pub use macos::SANDBOX_EXEC;

/// The directories a confined command may write, beyond the fixed set.
#[derive(Debug, Clone)]
pub struct Policy {
    root: PathBuf,
    writable: Vec<PathBuf>,
}

impl Policy {
    /// A policy bounding writes to `root`. Resolved here, because both backends match the
    /// resolved path: on macOS `/tmp` is a symlink to `/private/tmp`.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            root: std::fs::canonicalize(root)?,
            writable: Vec::new(),
        })
    }

    /// Adds a directory outside the root. One that does not resolve is an error rather than a
    /// silent skip, since Landlock would drop it.
    pub fn writable(mut self, dir: impl AsRef<Path>) -> io::Result<Self> {
        self.writable.push(std::fs::canonicalize(dir)?);
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `program` under the policy. Arguments, environment and working directory are the
    /// caller's to add.
    ///
    /// `RUSTC_WRAPPER` and `RUSTC_WORKSPACE_WRAPPER` are set empty: a wrapper such as sccache
    /// hands the compile to a server with its own bounds, unconfined if started outside, or
    /// pinned to this root after the caller exits if started here. Empty rather than removed,
    /// because empty also overrides `build.rustc-wrapper` in cargo's config.
    pub fn command(&self, program: impl AsRef<OsStr>) -> io::Result<Command> {
        let mut command = self.backend(program.as_ref())?;
        command
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "");
        Ok(command)
    }

    #[cfg(target_os = "linux")]
    fn backend(&self, program: &OsStr) -> io::Result<Command> {
        linux::command(self, program)
    }

    #[cfg(target_os = "macos")]
    fn backend(&self, program: &OsStr) -> io::Result<Command> {
        Ok(macos::command(self, program))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn backend(&self, _program: &OsStr) -> io::Result<Command> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform has no filesystem sandbox",
        ))
    }

    /// Creates the cache entries the policy grants, then runs one confined command. A kernel
    /// without Landlock ABI 3, or a macOS without `sandbox-exec`, fails here rather than on the
    /// first real command.
    pub fn preflight(&self) -> io::Result<()> {
        caches::create();
        let status = self
            .command("/bin/sh")?
            .args(["-c", "exit 0"])
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "a confined `exit 0` ended {status}"
            )))
        }
    }

    /// Every directory the kernel policy lets a command write, root first.
    fn write_set(&self) -> impl Iterator<Item = PathBuf> + '_ {
        std::iter::once(self.root.clone())
            .chain(caches::writable())
            .chain(self.writable.iter().cloned())
    }
}

/// Why a confined command's stderr suggests the policy stopped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// A write outside the policy. Seatbelt returns `EPERM` and Landlock `EACCES`, and the
    /// program prints its own message, which never names the policy.
    Write,
    /// Seatbelt refuses a profile inside a sandbox. SwiftPM compiles `Package.swift` under its
    /// own `sandbox-exec`, so this is how `swift build` fails.
    Nested,
    /// LaunchServices' own message when the profile denies `open`; it never says why.
    Open,
}

const NESTED_REFUSED: &str = "sandbox_apply: Operation not permitted";
const OPEN_REFUSED: &str = "failed with error -54";

/// The denials `stderr` looks like, for a caller to explain. A heuristic: an ordinary permission
/// error matches too, and a translated system matches nothing. `Nested` replaces `Write`, which
/// its message also matches and which a wider policy cannot fix.
pub fn denials(stderr: &str) -> Vec<Denial> {
    let mut found = Vec::new();
    if stderr.contains(NESTED_REFUSED) {
        found.push(Denial::Nested);
    } else if stderr.contains("Operation not permitted") || stderr.contains("Permission denied") {
        found.push(Denial::Write);
    }
    if stderr.contains(OPEN_REFUSED) {
        found.push(Denial::Open);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denials_are_read_from_stderr() {
        assert_eq!(denials("touch: x: Permission denied"), [Denial::Write]);
        assert_eq!(denials("rm: x: Operation not permitted"), [Denial::Write]);
        assert_eq!(denials(NESTED_REFUSED), [Denial::Nested]);
        assert_eq!(
            denials("LSOpenURLsWithRole() failed with error -54"),
            [Denial::Open]
        );
        assert!(denials("error: no such file").is_empty());
    }

    #[test]
    fn a_writable_directory_that_does_not_resolve_is_refused() {
        let policy = Policy::new(std::env::temp_dir()).unwrap();
        assert!(policy.writable("/no/such/directory").is_err());
    }
}
