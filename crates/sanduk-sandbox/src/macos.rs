use std::ffi::OsStr;
use std::process::Command;

use crate::Policy;

/// Absolute, not `sandbox-exec` on `PATH`: a shim earlier in the search path would exec its
/// argument unconfined, and `preflight` would take its exit 0 as a working sandbox.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// `sandbox-exec -p <profile> program`.
pub fn command(policy: &Policy, program: &OsStr) -> Command {
    let mut process = Command::new(SANDBOX_EXEC);
    process.arg("-p").arg(profile(policy)).arg(program);
    process
}

/// The policy in SBPL. Allow-default with writes denied, rather than deny-default: a deny-default
/// profile has to name every path a toolchain reads, and a missing one fails the command outright.
fn profile(policy: &Policy) -> String {
    let mut profile =
        String::from("(version 1) (allow default) (deny file-write*) (allow file-write*");
    for path in policy.write_set() {
        // A path that does not exist yet, such as cargo's journal, takes `subpath` so it can be
        // created as either a file or a directory.
        let form = if path.is_dir() || !path.exists() {
            "subpath"
        } else {
            "literal"
        };
        // The backslash is replaced first, or it would escape the quote that follows it.
        let quoted = path
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        profile.push_str(&format!(" ({form} \"{quoted}\")"));
    }
    profile.push(')');
    // Three routes the file rules cannot see, each measured to escape them: `defaults write`
    // hands the plist to cfprefsd, `kill` reaches the user's other processes, and `open` has
    // launchd start an app, one the command just built included, outside the sandbox. macOS only:
    // Landlock scopes signals from ABI 6, above the ABI 3 floor, and Linux has neither daemon. A
    // command still signals its own descendants, which share its sandbox.
    profile.push_str(
        " (deny user-preference-write) (deny signal) (allow signal (target same-sandbox)) \
         (deny lsopen)",
    );
    profile
}
