/// Why `docker info` failed, with the command that fixes it on this host.
///
/// `os` is `std::env::consts::OS`; `systemd` is whether `systemctl` is installed.
pub fn docker_daemon_error(stderr: &str, os: &str, systemd: bool) -> String {
    let endpoint = endpoint(stderr);
    let place = match endpoint {
        Some(e) => format!("the docker daemon at {e}"),
        None => "the docker daemon".into(),
    };
    if stderr.to_lowercase().contains("permission denied") {
        return format!(
            "permission denied on {place}. Add yourself to the docker group (sudo usermod -aG \
             docker $USER) and log in again. Membership is root-equivalent on this host."
        );
    }
    if let Some(e) = endpoint
        && !e.starts_with("unix://")
    {
        // Starting a local daemon would not help; the CLI points elsewhere.
        return format!(
            "{place} is not reachable. Check that host, or point DOCKER_HOST or `docker context \
             use` at a local daemon."
        );
    }
    let fix = if os == "macos" {
        "open -a Docker, or colima start"
    } else if endpoint.is_some_and(|e| e.contains("/run/user/")) {
        "systemctl --user start docker"
    } else if systemd {
        "sudo systemctl start docker"
    } else {
        "sudo service docker start"
    };
    format!("{place} is not reachable. Start it with `{fix}`, then re-run.")
}

/// The first `unix://`, `tcp://`, `ssh://` or `npipe://` address that starts a word, up to
/// whitespace or `;`, less trailing punctuation.
fn endpoint(stderr: &str) -> Option<&str> {
    const SCHEMES: [&str; 4] = ["unix://", "tcp://", "ssh://", "npipe://"];
    stderr.char_indices().find_map(|(i, _)| {
        let rest = &stderr[i..];
        let starts_word = stderr[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let scheme = SCHEMES.iter().find(|s| rest.starts_with(**s))?;
        if !starts_word {
            return None;
        }
        let end = rest[scheme.len()..]
            .find(|c: char| c.is_whitespace() || c == ';')
            .map_or(rest.len(), |n| n + scheme.len());
        let found = rest[..end].trim_end_matches([':', ',', '.']);
        (found.len() > scheme.len()).then_some(found)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// stderr as docker 29.8.0 prints it, captured on Linux.
    const NO_SOCKET: &str = "failed to connect to the docker API at \
        unix:///var/run/docker.sock; check if the path is correct and if the daemon is running: \
        dial unix /var/run/docker.sock: connect: no such file or directory";

    #[test]
    fn the_docker_daemon_error_names_the_fix() {
        let rootless = NO_SOCKET.replace("/var/run/docker.sock", "/run/user/1000/docker.sock");
        let cases = [
            (NO_SOCKET, "linux", true, "`sudo systemctl start docker`"),
            (NO_SOCKET, "linux", false, "`sudo service docker start`"),
            (&rootless, "linux", true, "`systemctl --user start docker`"),
            (
                NO_SOCKET,
                "macos",
                false,
                "`open -a Docker, or colima start`",
            ),
            (
                "permission denied while trying to connect to the docker API at \
                 unix:///var/run/docker.sock",
                "linux",
                true,
                "sudo usermod -aG docker $USER",
            ),
            (
                "Cannot connect to the Docker daemon at tcp://10.0.0.5:2376. Is the docker \
                 daemon running?",
                "linux",
                true,
                "DOCKER_HOST",
            ),
        ];
        for (stderr, os, systemd, expected) in cases {
            let message = docker_daemon_error(stderr, os, systemd);
            assert!(message.contains(expected), "{message}");
            let endpoint = endpoint(stderr).expect("an endpoint");
            assert!(message.contains(endpoint), "{message}");
        }
    }

    #[test]
    fn the_endpoint_loses_its_trailing_punctuation() {
        assert_eq!(
            endpoint("at tcp://10.0.0.5:2376. Is it"),
            Some("tcp://10.0.0.5:2376")
        );
        assert_eq!(
            endpoint("at unix:///var/run/docker.sock; check"),
            Some("unix:///var/run/docker.sock")
        );
        // Not at the start of a word.
        assert_eq!(endpoint("xunix:///a"), None);
    }

    #[test]
    fn a_daemon_error_with_no_endpoint_still_names_a_fix() {
        let message = docker_daemon_error("", "linux", true);
        assert!(
            message.starts_with("the docker daemon is not reachable"),
            "{message}"
        );
        assert!(
            message.contains("`sudo systemctl start docker`"),
            "{message}"
        );
    }
}
