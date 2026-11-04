//! `run --clone`: the agent works on a clone of the `-w` repository, and its commits come back as
//! a branch. With the repository bind-mounted, the agent can write its `.git/config`, and a
//! `core.fsmonitor` there runs on the host's next `git status`. The clone's commits come back
//! through a git bundle made in a container, so host git never reads the clone's config or hooks.
//! See docs/dev/tsk-tsk.md.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;

use crate::agent::REPORT_NAME;
use crate::error::{Error, Result};
use crate::util::open_unfollowed;

/// Where the return script leaves the bundle, relative to the clone.
const BUNDLE: &str = ".git/sanduk-return.bundle";

/// The return script's exit status when the agent changed nothing.
pub const UNCHANGED: i32 = 3;

static SHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[0-9a-f]{40}([0-9a-f]{24})?$").unwrap());

pub struct Clone {
    pub origin: PathBuf,
    pub copy: PathBuf,
    pub base: String,
    pub branch: String,
}

/// Host git on the user's repository. Variables a calling hook may have set would point it at
/// another repository.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|e| Error::about("clone_into", format!("needs git on this host: {e}")))?;
    if !out.status.success() {
        return Err(Error::new(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Clone {
    /// Checks that `origin` is a repository's top level with no uncommitted change to a tracked
    /// file, which the clone would not carry, and that nothing is at `copy`. Nothing is created yet.
    pub fn plan(origin: &Path, run: &str, copy: PathBuf) -> Result<Self> {
        let refuse = |why: &str| {
            Error::about(
                "clone_into",
                format!("refuses {}: it {why}", origin.display()),
            )
        };
        let top = git(origin, &["rev-parse", "--show-toplevel"])
            .map_err(|_| refuse("is not a git repository"))?;
        if std::fs::canonicalize(&top).ok().as_deref() != Some(origin) {
            return Err(refuse(&format!("is inside {top}; give the top level")));
        }
        let base = git(origin, &["rev-parse", "HEAD"]).map_err(|_| refuse("has no commit"))?;
        if !SHA.is_match(&base) {
            return Err(refuse(&format!("names HEAD as {base:?}")));
        }
        if !git(origin, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
            return Err(refuse(
                "has uncommitted changes, which the clone would not carry. Commit or stash them",
            ));
        }
        if copy.symlink_metadata().is_ok() {
            return Err(Error::about(
                "clone_into",
                format!(
                    "{} already exists; the clone needs a path of its own",
                    copy.display()
                ),
            ));
        }
        Ok(Clone {
            origin: origin.to_path_buf(),
            copy,
            base,
            branch: format!("sanduk/{run}"),
        })
    }

    /// Clones `origin` into `copy`. `--no-local` copies objects rather than hard-linking them,
    /// so nothing in the clone is a file of the repository's.
    pub fn create(&self) -> Result<()> {
        std::fs::create_dir_all(self.copy.parent().unwrap_or(&self.copy))?;
        let copy = self.copy.to_string_lossy();
        git(
            &self.origin,
            &[
                "clone",
                "--quiet",
                "--no-local",
                "--no-hardlinks",
                ".",
                &copy,
            ],
        )?;
        Ok(())
    }

    /// Run with `sh -c` in the clone, in a container: commits what the agent left uncommitted,
    /// except its report, and bundles everything since `base`. Hooks and fsmonitor are off: an
    /// fsmonitor the agent set could hide a change from `add`.
    pub fn script(&self) -> String {
        let base = &self.base;
        format!(
            "set -e; g() {{ git -c safe.directory='*' -c core.hooksPath=/dev/null -c core.fsmonitor=false \"$@\"; }}; \
             rm -f {BUNDLE}; \
             g add -A -- . ':(exclude){REPORT_NAME}'; \
             g diff --cached --quiet || g -c user.name=sanduk -c user.email=sanduk@localhost \
             commit --quiet --no-verify -m 'sanduk: changes the agent left uncommitted'; \
             [ \"$(g rev-parse HEAD)\" = {base} ] && exit {UNCHANGED}; \
             g bundle create {BUNDLE} {base}..HEAD"
        )
    }

    /// Fetches the bundle into `branch`, from a copy this process made, so a symlink the agent
    /// left cannot point the fetch elsewhere. Objects are fsck'd as they arrive.
    pub fn fetch(&self) -> Result<String> {
        let found = open_unfollowed(&self.copy.join(BUNDLE)).map_err(|refused| {
            Error::new(refused.unwrap_or_else(|| "the return step wrote no bundle".into()))
        })?;
        let held = self.copy.with_extension("bundle");
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&held)?;
        std::io::copy(&mut &found, &mut out)?;
        drop(out);
        let bundle = held.to_string_lossy();
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        let fetched = git(
            &self.origin,
            &[
                "-c",
                "transfer.fsckObjects=true",
                "fetch",
                "--quiet",
                &bundle,
                &refspec,
            ],
        );
        let _ = std::fs::remove_file(&held);
        fetched?;
        git(
            &self.origin,
            &[
                "rev-list",
                "--count",
                &format!("{}..{}", self.base, self.branch),
            ],
        )
    }

    /// Deletes the clone. Only once its work is fetched, or there was none.
    pub fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.copy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) -> std::process::Output {
        Command::new("sh")
            .args(["-c", script])
            .current_dir(dir)
            .output()
            .unwrap()
    }

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sanduk-clone-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let out = sh(
            &dir,
            "git init -q && echo a > a.txt && git add a.txt && \
             git -c user.name=t -c user.email=t@t commit -qm base",
        );
        assert!(out.status.success(), "{out:?}");
        dir
    }

    #[test]
    fn a_dirty_tree_a_subdirectory_and_a_plain_directory_are_refused() {
        let dir = repo("refuse");
        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        let e = Clone::plan(&dir, "x", dir.with_extension("x"))
            .err()
            .unwrap();
        assert!(e.message.contains("uncommitted"), "{}", e.message);
        sh(&dir, "git checkout -q a.txt && mkdir sub");
        // Untracked files are not a refusal: the clone leaves them where they are.
        std::fs::write(dir.join("new.txt"), "x").unwrap();
        assert!(Clone::plan(&dir, "x", dir.with_extension("x")).is_ok());
        let e = Clone::plan(&dir.join("sub"), "x", dir.with_extension("x"))
            .err()
            .unwrap();
        assert!(e.message.contains("top level"), "{}", e.message);
        let plain = dir.with_extension("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let e = Clone::plan(&plain, "x", dir.with_extension("x"))
            .err()
            .unwrap();
        assert!(e.message.contains("not a git repository"), "{}", e.message);
        let _ = std::fs::remove_dir_all(&plain);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The agent commits once, leaves an edit, writes its report and poisons the clone's config.
    /// The branch carries both changes and not the report; host git ran nothing the agent wrote.
    #[test]
    fn the_agents_commits_come_back_as_a_branch_and_its_config_does_not() {
        let dir = repo("roundtrip");
        let state = dir.with_extension("state");
        {
            let c = Clone::plan(&dir, "abc123", state.join("abc123")).unwrap();
            c.create().unwrap();
            let marker = dir.with_extension("pwned");
            let agent = format!(
                "echo b > b.txt && git add b.txt && \
                 git -c user.name=a -c user.email=a@a commit -qm agent && \
                 echo edited > a.txt && echo report > {REPORT_NAME} && \
                 git config core.fsmonitor 'touch {}'",
                marker.display()
            );
            assert!(sh(&c.copy, &agent).status.success());
            let _ = std::fs::remove_file(&marker);
            let out = sh(&c.copy, &c.script());
            assert!(out.status.success(), "{out:?}");

            assert_eq!(c.fetch().unwrap(), "2");
            let show = |what: &str| git(&dir, &["show", &format!("{}:{what}", c.branch)]);
            assert_eq!(show("b.txt").unwrap(), "b");
            assert_eq!(show("a.txt").unwrap(), "edited");
            assert!(show(REPORT_NAME).is_err(), "the report was committed");
            git(&dir, &["status"]).unwrap();
            assert!(!marker.exists(), "host git ran the clone's fsmonitor");
            assert_eq!(git(&dir, &["config", "--get", "core.fsmonitor"]).ok(), None);
            c.remove();
            assert!(!c.copy.exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn an_agent_that_changed_nothing_returns_unchanged() {
        let dir = repo("unchanged");
        let state = dir.with_extension("state");
        {
            let c = Clone::plan(&dir, "u", state.join("u")).unwrap();
            c.create().unwrap();
            std::fs::write(c.copy.join(REPORT_NAME), "only a report").unwrap();
            let out = sh(&c.copy, &c.script());
            assert_eq!(out.status.code(), Some(UNCHANGED), "{out:?}");
            c.remove();
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }
}
