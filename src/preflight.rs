//! Host checks that run before any container is created. Both catch a failure that is silent and
//! expensive: a bad key costs ~174s of in-container retry backoff, and a firewalled relay hangs the
//! agent's first API call until --timeout with no error at all.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::http;
use crate::providers::{Provider, Scheme};

pub const FIREWALL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

/// `(path, blocked)` from `socketfilterfw --listapps`.
pub fn firewall_entries(listing: &str) -> Vec<(String, bool)> {
    let mut entries = Vec::new();
    let mut path: Option<String> = None;
    for line in listing.lines() {
        let stripped = line.trim();
        let (head, rest) = stripped.split_once(':').unwrap_or((stripped, ""));
        if !head.trim().is_empty() && head.trim().bytes().all(|b| b.is_ascii_digit()) {
            path = Some(rest.trim().to_string());
        } else if stripped.starts_with('(')
            && let Some(p) = path.take()
        {
            entries.push((p, stripped.to_lowercase().contains("block")));
        }
    }
    entries
}

/// Ports in LISTEN on every address, from the text of `/proc/net/tcp` or `/proc/net/tcp6`.
pub fn wildcard_listeners(table: &str) -> Vec<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (addr, port) = fields.get(1)?.split_once(':')?;
            let listening = fields.get(3) == Some(&"0A");
            let wildcard = addr.bytes().all(|b| b == b'0');
            (listening && wildcard)
                .then(|| u16::from_str_radix(port, 16).ok())
                .flatten()
        })
        .collect()
}

/// A warning that a sealed container reaches host services bound to every address, at `gateway`.
/// Measured on Linux Docker (docs/dev/tsk-tsk.md); elsewhere `/proc` is absent and this is `None`.
pub fn host_listener_warning(gateway: &str) -> Option<String> {
    let mut ports: Vec<u16> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|t| wildcard_listeners(&t))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    if ports.is_empty() {
        return None;
    }
    let list: Vec<String> = ports.iter().map(u16::to_string).collect();
    Some(format!(
        "WARNING host TCP ports {} listen on every address.\n  \
         The sealed container reaches them at {gateway}, past the relay. \
         Bind them to 127.0.0.1 to keep them out of its reach.",
        list.join(", ")
    ))
}

/// A warning when the macOS application firewall blocks this binary. A blocked binary drops
/// container-to-host connections with no error, so the agent's first call to the relay hangs.
pub fn firewall_warning() -> Option<String> {
    if !std::path::Path::new(FIREWALL).exists() {
        return None;
    }
    let run = |arg: &str| {
        std::process::Command::new(FIREWALL)
            .arg(arg)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    if !run("--getglobalstate").is_some_and(|s| s.to_lowercase().contains("enabled")) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let real = std::fs::canonicalize(&exe).unwrap_or(exe.clone());
    let names = [exe.display().to_string(), real.display().to_string()];
    let listing = run("--listapps").unwrap_or_default();
    // An unlisted signed binary is allowed; an unsigned one is prompted for, which nobody answers.
    let (blocked, _) = firewall_entries(&listing)
        .into_iter()
        .find(|(p, b)| *b && names.contains(p))?;
    Some(format!(
        "WARNING the macOS firewall is on and\n  {blocked}\n  is set to block incoming connections.\n  \
             The agent's calls to the relay will hang until the timeout. Allow it once:\n    \
             sudo {FIREWALL} --unblockapp {blocked}"
    ))
}

/// One cheap request, so a bad key fails in 0.2s instead of minutes of retries. `path_prefix` is
/// the path a `--base-url` carries, if any. A provider with
/// no auth has nothing to validate. Any status but 401 and 403 proves the endpoint answered, and
/// the agent is left to try.
pub fn validate_key(
    key: &str,
    scheme: Scheme,
    host: &str,
    path_prefix: &str,
    provider: &Provider,
) -> Result<()> {
    if !provider.has_auth {
        return Ok(());
    }
    let mut headers = vec![(provider.auth_header, provider.auth_value(key))];
    headers.extend(
        provider
            .validate_headers
            .iter()
            .map(|(k, v)| (*k, v.to_string())),
    );
    let base = format!("{}://{host}{path_prefix}", scheme.as_str());
    let path = format!("{path_prefix}{}", provider.validate_path);
    match http::get_status(scheme, host, &path, &headers, Duration::from_secs(15)) {
        Ok(status @ (401 | 403)) => Err(Error::new(format!(
            "{} rejected by {base} (HTTP {status}).",
            provider.key_env
        ))),
        Ok(_) => Ok(()),
        Err(e) => Err(Error::new(format!("cannot reach {base}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_listeners_are_listening_sockets_on_every_address() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue\n\
                   0: 00000000:0016 00000000:0000 0A 00000000:00000000\n\
                   1: 0100007F:0CEA 00000000:0000 0A 00000000:00000000\n\
                   2: 00000000:1F90 0100007F:9C40 01 00000000:00000000\n\
                   3: 010012AC:A1B2 00000000:0000 0A 00000000:00000000\n";
        // 22 on 0.0.0.0. Not: 127.0.0.1:3306, an established socket, 172.18.0.1 (the relay's kind).
        assert_eq!(wildcard_listeners(tcp), [22]);
        let tcp6 = "  sl  local_address remote_address st\n\
                    0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A\n\
                    1: 00000000000000000000000001000000:0277 00000000000000000000000000000000:0000 0A\n";
        assert_eq!(wildcard_listeners(tcp6), [8080]);
        assert!(wildcard_listeners("").is_empty());
    }

    #[test]
    fn firewall_entries_pair_paths_with_state() {
        let listing = "1 : /opt/homebrew/.../Python.app \n             (Block incoming connections)\n\
                       2 : /usr/bin/python3 \n             (Allow incoming connections)\n";
        let entries = firewall_entries(listing);
        assert_eq!(
            entries,
            [
                ("/opt/homebrew/.../Python.app".to_string(), true),
                ("/usr/bin/python3".to_string(), false)
            ]
        );
    }
}
