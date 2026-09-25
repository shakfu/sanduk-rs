use std::ffi::OsStr;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
};

use crate::Policy;

/// Builds the ruleset in the parent and restricts the child in `pre_exec`. Only two system calls
/// run between `fork` and `exec`: allocating or taking a lock there can deadlock against a mutex
/// another thread held at the fork.
pub fn command(policy: &Policy, program: &OsStr) -> io::Result<Command> {
    // V3 is the floor. V1 denies every rename across directories, which would break `mv` inside
    // the root, and without V3's `Truncate` a read-only grant still permits truncating any file
    // on the system. `IoctlDev` arrives in V5 and is not handled, so ioctls on device files the
    // command can open stay unrestricted.
    let abi = ABI::V3;
    let write = AccessFs::from_all(abi);
    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(write)
        .map_err(|e| failed("configuring the Linux filesystem sandbox", e))?
        .create()
        .map_err(|e| failed("creating the Linux filesystem sandbox", e))?
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|e| failed("opening /", e))?,
            AccessFs::from_read(abi),
        ))
        .map_err(|e| failed("allowing reads", e))?
        .add_rule(PathBeneath::new(
            PathFd::new(policy.root()).map_err(|e| failed("opening the sandbox root", e))?,
            write,
        ))
        .map_err(|e| failed("allowing the sandbox root", e))?
        // Drops a path that does not open, and masks the directory-only rights that would be
        // rejected on a file, which `/dev/null` is.
        .add_rules(path_beneath_rules(policy.write_set().skip(1), write))
        .map_err(|e| failed("allowing the writable paths outside the root", e))?;

    let mut ruleset = Some(ruleset);
    let mut process = Command::new(program);
    // SAFETY: the closure only consumes the prebuilt ruleset and performs syscalls in the child.
    unsafe {
        process.pre_exec(move || {
            let status = ruleset
                .take()
                .ok_or_else(|| io::Error::other("sandbox pre-exec ran twice"))?
                .restrict_self()
                .map_err(io::Error::other)?;
            if status.ruleset != RulesetStatus::FullyEnforced {
                return Err(io::Error::other(
                    "Linux filesystem sandbox was not fully enforced",
                ));
            }
            Ok(())
        });
    }
    Ok(process)
}

fn failed(what: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}
