//! Real commands under the kernel policy, judged on disk rather than by exit status.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use sanduk_sandbox::{Denial, Policy, denials};

fn sandboxed() -> Policy {
    Policy::new(std::env::current_dir().unwrap()).unwrap()
}

fn run_under(policy: &Policy, script: &str) -> Output {
    policy
        .command("/bin/sh")
        .unwrap()
        .args(["-c", script])
        .current_dir(policy.root())
        .stdin(Stdio::null())
        .output()
        .expect("a confined command")
}

/// Stdout, and the stderr to look for denials in.
fn run(script: &str) -> (String, String) {
    let out = run_under(&sandboxed(), script);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A path under `$HOME`: the one place outside the root that is neither the temp directory nor a
/// build cache, and the place the policy exists to protect. `None` when `$HOME` is unset, which
/// leaves these tests with nothing to aim at.
fn outside_root(name: &str) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    home.is_dir()
        .then(|| home.join(format!(".sanduk-sandbox-{name}-{}", std::process::id())))
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

#[test]
fn preflight_installs_the_sandbox() {
    sandboxed()
        .preflight()
        .expect("the sandbox should install on a supported platform");
}

/// The policy bounds writes, not reads. A toolchain, its headers and its dependency sources all
/// sit outside the root.
#[test]
fn reads_outside_the_root_are_allowed() {
    let (out, err) = run("ls /usr >/dev/null && echo READABLE");
    assert!(out.contains("READABLE"), "{err}");
}

#[test]
fn the_temp_directory_and_dev_null_stay_writable() {
    let (out, err) =
        run("f=$(mktemp) && echo x >\"$f\" && echo y >/dev/null && rm \"$f\" && echo WRITABLE");
    assert!(out.contains("WRITABLE"), "{err}");
}

/// Landlock denies every rename across directories below ABI 2, so `mv` is how a floor that
/// slipped to V1 would show itself.
#[test]
fn a_rename_across_directories_is_allowed() {
    let (out, err) = run(
        "d=$(mktemp -d) && mkdir \"$d/a\" \"$d/b\" && touch \"$d/a/x\" \
         && mv \"$d/a/x\" \"$d/b/x\" && rm -rf \"$d\" && echo RENAMED",
    );
    assert!(out.contains("RENAMED"), "{err}");
}

#[test]
fn writes_outside_the_root_are_denied() {
    let Some(path) = outside_root("write") else {
        return;
    };
    let (out, err) = run(&format!("touch {} && echo WROTE", quoted(&path)));
    let created = path.exists();
    let _ = std::fs::remove_file(&path);
    assert!(!created, "a write reached {}", path.display());
    assert!(!out.contains("WROTE"), "{out}");
    assert_eq!(denials(&err), [Denial::Write], "{err}");
}

/// Control: the same write lands unconfined, so the test above is not passing on a path that
/// could never be written.
#[test]
fn the_same_write_lands_unconfined() {
    let Some(path) = outside_root("control") else {
        return;
    };
    let status = std::process::Command::new("touch")
        .arg(&path)
        .status()
        .unwrap();
    let created = path.exists();
    let _ = std::fs::remove_file(&path);
    assert!(status.success() && created, "{}", path.display());
}

/// `writable` widens the policy by the named directory and nothing else: the sibling beside it
/// is still denied.
#[test]
fn only_the_named_directory_is_added() {
    let Some(base) = outside_root("writable") else {
        return;
    };
    let (granted, denied) = (base.join("granted"), base.join("denied"));
    std::fs::create_dir_all(&granted).unwrap();
    std::fs::create_dir_all(&denied).unwrap();

    let policy = sandboxed().writable(&granted).unwrap();
    let out = run_under(
        &policy,
        &format!("touch {}/a; touch {}/b", quoted(&granted), quoted(&denied)),
    );

    let (added, sibling) = (granted.join("a").exists(), denied.join("b").exists());
    let _ = std::fs::remove_dir_all(&base);
    assert!(added, "the writable directory was denied: {out:?}");
    assert!(
        !sibling,
        "a write reached a sibling of the writable directory"
    );
}

/// Landlock handles `Truncate` only from ABI 3. Below it the read grant on `/` still permits
/// `: > file` anywhere, which destroys the file without ever writing to it.
#[test]
fn truncating_a_file_outside_the_root_is_denied() {
    let Some(path) = outside_root("truncate") else {
        return;
    };
    std::fs::write(&path, "kept").unwrap();
    let (_, err) = run(&format!(": > {}", quoted(&path)));
    let after = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    assert_eq!(after, "kept", "a truncate reached the file: {err}");
}

/// Set and empty, whatever the caller inherited: unset would let cargo fall back to a wrapper
/// named in config.
#[test]
fn rustc_wrappers_are_cleared() {
    let (out, _) = run("echo \"[${RUSTC_WRAPPER-unset}][${RUSTC_WORKSPACE_WRAPPER-unset}]\"");
    assert_eq!(out.trim(), "[][]");
}

/// `bin/` is on `PATH`, so a file there would run unconfined in the next shell; the registry
/// beside it is where a dependency fetch writes.
#[test]
fn only_the_cargo_caches_are_writable_under_cargo_home() {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    let Some(home) = home.filter(|h| h.join("bin").is_dir() && h.join("registry").is_dir()) else {
        return;
    };
    let name = format!(".sanduk-sandbox-{}", std::process::id());
    let (bin, registry) = (
        home.join("bin").join(&name),
        home.join("registry").join(&name),
    );
    let (out, err) = run(&format!(
        "touch {} && rm {} && echo REGISTRY; touch {}",
        quoted(&registry),
        quoted(&registry),
        quoted(&bin)
    ));
    let wrote_bin = bin.exists();
    let _ = std::fs::remove_file(&bin);
    let _ = std::fs::remove_file(&registry);
    assert!(out.contains("REGISTRY"), "the registry was denied: {err}");
    assert!(!wrote_bin, "a write reached $CARGO_HOME/bin");
}

/// The Go counterpart: `pkg/mod` takes a fetch, `bin/` beside it does not.
#[test]
fn only_the_go_caches_are_writable_under_gopath() {
    let gopath = std::env::var_os("GOPATH")
        .and_then(|p| std::env::split_paths(&p).next())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("go")));
    let Some(gopath) = gopath.filter(|g| g.join("bin").is_dir() && g.join("pkg/mod").is_dir())
    else {
        return;
    };
    let name = format!(".sanduk-sandbox-{}", std::process::id());
    let (bin, cache) = (
        gopath.join("bin").join(&name),
        gopath.join("pkg/mod").join(&name),
    );
    let (out, err) = run(&format!(
        "touch {} && rm {} && echo CACHE; touch {}",
        quoted(&cache),
        quoted(&cache),
        quoted(&bin)
    ));
    let wrote_bin = bin.exists();
    let _ = std::fs::remove_file(&bin);
    let _ = std::fs::remove_file(&cache);
    assert!(out.contains("CACHE"), "the module cache was denied: {err}");
    assert!(!wrote_bin, "a write reached $GOPATH/bin");
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use sanduk_sandbox::SANDBOX_EXEC;
    use std::time::Duration;

    /// Resolution through `PATH` would let a shim named `sandbox-exec` run the command
    /// unconfined, with `preflight` reading its exit 0 as a working sandbox.
    #[test]
    fn the_sandbox_is_spawned_by_absolute_path() {
        let command = sandboxed().command("true").unwrap();
        assert_eq!(command.get_program(), SANDBOX_EXEC);
        assert!(
            Path::new(SANDBOX_EXEC).is_file(),
            "{SANDBOX_EXEC} is missing"
        );
    }

    /// A toolchain's entry takes a write; `~/Library/Caches` itself, shared with every app, does
    /// not.
    #[test]
    fn only_toolchain_entries_of_library_caches_are_writable() {
        let Some(caches) = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Caches"))
            .filter(|c| c.is_dir())
        else {
            return;
        };
        let name = format!(".sanduk-sandbox-{}", std::process::id());
        let (shared, pip) = (caches.join(&name), caches.join("pip").join(&name));
        let (out, err) = run(&format!(
            "mkdir -p {} && touch {} && rm {} && echo PIP; touch {}",
            quoted(&caches.join("pip")),
            quoted(&pip),
            quoted(&pip),
            quoted(&shared)
        ));
        let wrote_shared = shared.exists();
        let _ = std::fs::remove_file(&shared);
        let _ = std::fs::remove_file(&pip);
        assert!(out.contains("PIP"), "the pip cache was denied: {err}");
        assert!(!wrote_shared, "a write reached ~/Library/Caches itself");
    }

    /// `swiftc` writes its clang module cache here, beside `$TMPDIR` rather than inside it.
    #[test]
    fn the_user_cache_directory_stays_writable() {
        let dir = sanduk_sandbox::caches::user_cache_dir().expect("confstr names the directory");
        let (out, err) = run(&format!(
            "f=$(mktemp {}/sanduk-XXXXXX) && rm \"$f\" && echo WRITABLE",
            quoted(&dir)
        ));
        assert!(out.contains("WRITABLE"), "{err}");
    }

    /// cfprefsd writes the plist, so the file rules never see it.
    #[test]
    fn a_preference_write_is_denied() {
        let domain = format!("sanduk.sandbox.test.{}", std::process::id());
        let (_, err) = run(&format!("defaults write {domain} k -string x"));
        let read = std::process::Command::new("defaults")
            .args(["read", &domain, "k"])
            .output()
            .unwrap();
        let _ = std::process::Command::new("defaults")
            .args(["delete", &domain])
            .output();
        assert!(!read.status.success(), "a preference write landed: {err}");
    }

    /// A process outside the sandbox cannot be signalled; the command's own children can.
    #[test]
    fn only_the_commands_own_processes_can_be_signalled() {
        let mut outside = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (out, err) = run(&format!(
            "kill {}; sleep 30 & kill $! && wait $!; echo OWN=$?",
            outside.id()
        ));
        let survived = outside.try_wait().unwrap().is_none();
        let _ = outside.kill();
        let _ = outside.wait();
        assert!(
            survived,
            "a signal reached a process outside the sandbox: {err}"
        );
        // 143 is 128 + SIGTERM: the child was signalled.
        assert!(out.contains("OWN=143"), "{out} {err}");
    }

    /// An app bundle the command builds would start through launchd, outside the sandbox.
    #[test]
    fn open_is_denied() {
        let Some(marker) = outside_root("opened") else {
            return;
        };
        let (_, err) = run(&format!(
            "d=$(mktemp -d) && mkdir -p \"$d/P.app/Contents/MacOS\" \
             && printf '#!/bin/sh\\ntouch {}\\n' > \"$d/P.app/Contents/MacOS/P\" \
             && chmod +x \"$d/P.app/Contents/MacOS/P\" \
             && printf '<plist><dict><key>CFBundleExecutable</key><string>P</string>\
<key>LSUIElement</key><true/></dict></plist>' > \"$d/P.app/Contents/Info.plist\" \
             && open \"$d/P.app\"; rm -rf \"$d\"",
            quoted(&marker)
        ));
        std::thread::sleep(Duration::from_secs(2));
        let launched = marker.exists();
        let _ = std::fs::remove_file(&marker);
        assert!(!launched, "open started an app outside the sandbox: {err}");
        assert!(denials(&err).contains(&Denial::Open), "{err}");
    }

    /// Seatbelt refuses a profile inside a sandbox.
    #[test]
    fn a_nested_sandbox_is_reported_as_nested() {
        let (_, err) = run(&format!(
            "{SANDBOX_EXEC} -p '(version 1) (allow default)' true"
        ));
        assert_eq!(denials(&err), [Denial::Nested], "{err}");
    }
}
