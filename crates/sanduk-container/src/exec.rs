//! Every subprocess an engine runs goes through [`Exec`], so a test can answer for the engine.

use std::path::Path;
use std::process::Command;

/// What a finished command left. `code` is `None` when it was killed by a signal or never
/// started; a program missing from `PATH` reads as a failed command, not a panic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs an engine's CLI. `argv[0]` is the program.
pub trait Exec: Send + Sync {
    /// With `capture`, stdout and stderr are returned; without, they go to the terminal, for a
    /// build or a service start the user should watch.
    fn run(&self, argv: &[String], capture: bool) -> Captured;

    /// Whether `program` is an executable on `PATH`.
    fn which(&self, program: &str) -> bool;
}

/// The real thing: `std::process::Command`, never a shell.
#[derive(Debug, Clone, Copy, Default)]
pub struct System;

impl Exec for System {
    fn run(&self, argv: &[String], capture: bool) -> Captured {
        let Some((program, args)) = argv.split_first() else {
            return Captured::default();
        };
        let mut command = Command::new(program);
        command.args(args);
        let started = if capture {
            command.output().map(|out| Captured {
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        } else {
            command.status().map(|status| Captured {
                code: status.code(),
                ..Captured::default()
            })
        };
        started.unwrap_or_else(|e| Captured {
            code: None,
            stdout: String::new(),
            stderr: format!("{program}: {e}"),
        })
    }

    fn which(&self, program: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| executable(&dir.join(program)))
        })
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}
