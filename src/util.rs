//! Reporting, state directories, durations, randomness, and files the agent may have planted.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const PROG: &str = "sanduk";

/// One progress line on stderr, so stdout stays the agent's.
pub fn note(msg: &str) {
    eprintln!("{PROG}: {msg}");
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn xdg(var: &str, fallback: &[&str]) -> PathBuf {
    match std::env::var_os(var).filter(|v| !v.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => fallback.iter().fold(home(), |p, part| p.join(part)),
    }
}

thread_local! {
    /// Set by tests, which run in parallel threads: an environment variable is process-wide.
    static CONFIG_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static STATE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Points this thread's config and state directories elsewhere. For tests.
#[doc(hidden)]
pub fn override_dirs(config: Option<PathBuf>, state: Option<PathBuf>) {
    CONFIG_OVERRIDE.with(|c| *c.borrow_mut() = config);
    STATE_OVERRIDE.with(|s| *s.borrow_mut() = state);
}

/// `XDG_CONFIG_HOME/sanduk`, or `~/.config/sanduk`: user agents, recipes and kits.
pub fn config_dir() -> PathBuf {
    CONFIG_OVERRIDE
        .with(|c| c.borrow().clone())
        .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", &[".config"]).join("sanduk"))
}

/// `XDG_STATE_HOME/sanduk`, or `~/.local/state/sanduk`: what outlives a run. Bookkeeping, not
/// the user's work.
pub fn state_dir() -> PathBuf {
    STATE_OVERRIDE
        .with(|s| s.borrow().clone())
        .unwrap_or_else(|| xdg("XDG_STATE_HOME", &[".local", "state"]).join("sanduk"))
}

/// `XDG_CACHE_HOME/sanduk`, or `~/.cache/sanduk`: what can be made again.
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", &[".cache"]).join("sanduk")
}

/// A duration: a bare number is seconds, a suffix of s, m, h or d multiplies it. Every duration
/// sanduk takes reads the same way.
pub fn seconds(value: &str) -> Result<u64> {
    let text = value.trim();
    let unit = |c| match c {
        's' => Some(1),
        'm' => Some(60),
        'h' => Some(3600),
        'd' => Some(86400),
        _ => None,
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let total = if digits(text) {
        text.parse::<u64>().ok()
    } else {
        let (head, last) = text.split_at(text.len().saturating_sub(1));
        match last.chars().next().and_then(unit) {
            Some(n) if digits(head) => head.parse::<u64>().ok().and_then(|h| h.checked_mul(n)),
            _ => None,
        }
    };
    match total {
        Some(0) => Err(Error::new(format!("{value:?} is not a positive duration"))),
        Some(total) => Ok(total),
        None => Err(Error::new(format!(
            "{value:?} is not a duration: seconds as a number, or a suffix of s, m, h or d -- \
             900, 45s, 15m, 2h, 1d"
        ))),
    }
}

/// `n` bytes from the kernel's CSPRNG.
pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0; n];
    File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn random_hex(n: usize) -> Result<String> {
    Ok(random_bytes(n.div_ceil(2))?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..n]
        .to_string())
}

/// A URL-safe token of `n` random bytes, as Python's `secrets.token_urlsafe`.
pub fn token(n: usize) -> Result<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = random_bytes(n)?;
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let v = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(v >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    Ok(out)
}

/// `path` opened for reading, or `None` if it is not an ordinary file.
///
/// For anything the agent writes: it owns the mount, so a symlink it leaves there names a path
/// this process resolves against the host root. `O_NOFOLLOW` rather than a symlink test, which
/// the agent can swap between the test and the open. `O_NONBLOCK`, because a fifo at that name
/// blocks the open until something writes to it.
pub fn open_unfollowed(path: &Path) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let file = match file {
        Ok(file) => file,
        Err(_) => {
            if let Ok(target) = std::fs::read_link(path) {
                note(&format!(
                    "refusing {name}: a symlink to {}",
                    target.display()
                ));
            }
            return None;
        }
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        note(&format!("refusing {name}: not a regular file"));
        return None;
    }
    Some(file)
}

/// The text of `path`, or `None` if it is not an ordinary file.
pub fn read_unfollowed(path: &Path) -> Option<String> {
    let mut text = Vec::new();
    open_unfollowed(path)?.read_to_end(&mut text).ok()?;
    Some(String::from_utf8_lossy(&text).into_owned())
}

/// Copies an opened file to `dest`, following no symlink there: the destination can itself be
/// inside a mount.
pub fn copy_unfollowed(mut from: &File, dest: &Path) -> Result<()> {
    use std::io::Seek;
    use std::os::unix::fs::OpenOptionsExt;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)
        .map_err(|e| {
            Error::new(format!(
                "cannot write the report to {}: {e}",
                dest.display()
            ))
        })?;
    from.seek(io::SeekFrom::Start(0))?;
    io::copy(&mut from, &mut out)?;
    out.flush()?;
    Ok(())
}

/// POSIX shell quoting, as Python's `shlex.quote`: bare when every character is safe, else in
/// single quotes.
pub fn shell_quote(value: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !value.is_empty() && value.chars().all(safe) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A list of strings as Python's `json.dumps` writes it: `", "` between items, non-ASCII
/// escaped. Image tags hash this text, so it has to match byte for byte.
pub fn py_json_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| py_json_string(s)).collect();
    format!("[{}]", quoted.join(", "))
}

pub fn py_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_one_way() {
        for (given, expected) in [
            ("900", 900),
            ("45s", 45),
            ("15m", 900),
            ("2h", 7200),
            ("1d", 86400),
        ] {
            assert_eq!(seconds(given), Ok(expected), "{given}");
        }
        for bad in ["", "0", "-5", "5x", "m", "1.5h"] {
            assert!(seconds(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn shell_quoting_matches_shlex() {
        assert_eq!(shell_quote("plain-1.0/x"), "plain-1.0/x");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn json_lists_match_python() {
        assert_eq!(py_json_list(&[]), "[]");
        assert_eq!(py_json_list(&["a".into(), "b c".into()]), r#"["a", "b c"]"#);
        assert_eq!(
            py_json_string("\u{e9}\"\\\u{7f}"),
            "\"\\u00e9\\\"\\\\\\u007f\""
        );
    }

    #[test]
    fn tokens_are_url_safe_and_distinct() {
        let (a, b) = (token(24).unwrap(), token(24).unwrap());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_eq!(random_hex(8).unwrap().len(), 8);
    }
}
