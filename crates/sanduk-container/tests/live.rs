//! Against a real engine: the parsers in `engine.rs` are tested on recorded output, and an
//! engine release can change its columns. Ignored by default, since it boots containers.
//!
//! ```text
//! cargo test -p sanduk-container --test live -- --ignored --test-threads 1
//! ```
//!
//! `SANDUK_RUNTIME` picks the engine (`apple`, `docker`); otherwise this platform's default.
//! `SANDUK_TEST_IMAGE` picks the image, which needs `sh` and `sleep`.

use std::time::Duration;

use sanduk_container::runs::{self, Reaper};
use sanduk_container::{ContainerSpec, Engine, Kind, Prefix, wait_for_gateway};

const PREFIX: &str = "sanduk-";

fn engine() -> Engine {
    let engine = Engine::get(
        std::env::var("SANDUK_RUNTIME").ok().as_deref(),
        Prefix::new(PREFIX).unwrap(),
    )
    .unwrap();
    engine.require_run().unwrap();
    engine
}

fn image() -> String {
    std::env::var("SANDUK_TEST_IMAGE").unwrap_or_else(|_| "docker.io/library/alpine:3.20".into())
}

fn unique(what: &str) -> String {
    format!("{PREFIX}test-{what}-{}", std::process::id())
}

/// Runs `spec` to completion, output captured.
fn run(engine: &Engine, spec: &ContainerSpec) -> std::process::Output {
    let argv = engine.run_argv(spec);
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .unwrap()
}

#[test]
#[ignore]
fn a_container_runs_its_command_with_the_workdir_mounted() {
    let engine = engine();
    let work = std::env::temp_dir().join(unique("work"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("in.txt"), "from host").unwrap();
    let spec = ContainerSpec {
        mount: Some((std::fs::canonicalize(&work).unwrap(), "/work".into())),
        entrypoint: Some("sh".into()),
        command: vec!["-c".into(), "cat in.txt && echo from box > out.txt".into()],
        // Where the mount keeps host ownership, root without capabilities cannot write it.
        // SAFETY: getuid takes no arguments and cannot fail.
        user: engine
            .kind()
            .keeps_mount_owner()
            .then(|| unsafe { libc::getuid() }.to_string()),
        ..ContainerSpec::new(unique("run"), image())
    };
    let out = run(&engine, &spec);
    let written = std::fs::read_to_string(work.join("out.txt")).unwrap_or_default();
    engine.destroy(&spec.name).unwrap();
    let _ = std::fs::remove_dir_all(&work);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from host");
    assert_eq!(written, "from box\n");
}

/// The relayed-mode setup, as `run` does it: an internal network, a holder where the engine
/// needs one, the gateway bindable on this host, and teardown that leaves nothing listed.
#[test]
#[ignore]
fn a_network_comes_up_is_listed_and_goes_away() {
    let engine = engine();
    let name = unique("net");
    let (network, created) = engine.ensure_network(&name, true).unwrap();
    assert!(created);
    assert_eq!(
        engine.ensure_network(&name, true).unwrap(),
        (network.clone(), false)
    );
    if engine.kind() == Kind::Docker {
        assert_eq!(engine.network_internal(&name), Some(true));
    }

    let holder = engine.hold_network_up(&name, &image(), 120).unwrap();
    let bindable = wait_for_gateway(&network.gateway, Duration::from_secs(30));
    let listed = engine.list_containers(PREFIX).unwrap();
    if let Some(holder) = &holder {
        engine.destroy(holder).unwrap();
    }
    let after = engine.list_containers(PREFIX).unwrap();
    let deleted = engine.delete_network(&name);

    assert_eq!(holder.is_some(), engine.kind().needs_network_holder());
    if let Some(holder) = &holder {
        let found = listed
            .iter()
            .find(|c| &c.name == holder)
            .expect("the holder is listed");
        assert_eq!(found.state, "running");
        assert!(
            found.image.contains("alpine") || !image().contains("alpine"),
            "{found:?}"
        );
        assert!(
            !after.iter().any(|c| &c.name == holder),
            "the holder outlived destroy"
        );
    }
    // The relay binds it. Apple's vmnet puts it on this host; Docker Desktop, Colima and Lima
    // keep the bridge in a VM, so for Docker the answer depends on where the daemon runs.
    if engine.kind() == Kind::Apple {
        assert!(bindable, "{} was not bindable", network.gateway);
    } else if !bindable {
        eprintln!(
            "{} was not bindable here.{}",
            network.gateway,
            engine.kind().gateway_hint()
        );
    }
    assert!(deleted, "the network was not deleted");
    assert_eq!(engine.network_info(&name), None);
}

/// `(label, host, port)`. `dns` with no port is a name lookup.
type Target = (&'static str, String, u16);

/// Which of `targets` a container on `network` connects to, by label.
fn reached(engine: &Engine, network: &str, targets: &[Target]) -> Vec<(&'static str, bool)> {
    let script = targets
        .iter()
        .map(|(label, host, port)| {
            let test = if *port == 0 {
                format!("nslookup {host}")
            } else {
                format!("nc -z -w 2 {host} {port}")
            };
            format!(
                "if timeout 4 {test} >/dev/null 2>&1; then echo {label}=1; else echo {label}=0; fi"
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let spec = ContainerSpec {
        network: Some(network.into()),
        entrypoint: Some("sh".into()),
        command: vec!["-c".into(), script],
        ..ContainerSpec::new(unique("probe"), image())
    };
    let out = run(engine, &spec);
    engine.destroy(&spec.name).unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    targets
        .iter()
        .map(|(label, ..)| {
            let line = stdout.lines().find(|l| l.starts_with(&format!("{label}=")));
            (
                *label,
                line.expect("every target answers") == format!("{label}=1"),
            )
        })
        .collect()
}

/// This host's address on its default route. Connecting a UDP socket sends nothing.
fn lan_address() -> String {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    s.connect("1.1.1.1:80").unwrap();
    s.local_addr().unwrap().ip().to_string()
}

/// What `--mode sealed` claims: no route off the host. Probes from an internal network, with a
/// routable one as the control, so a blocked result is not an offline host. See
/// docs/dev/tsk-tsk.md.
#[test]
#[ignore]
fn a_sealed_container_reaches_no_address_off_the_host() {
    let engine = engine();
    let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut done = Vec::new();
    let mut probe = |internal: bool, targets: &dyn Fn(&str) -> Vec<Target>| {
        let name = unique(if internal { "sealed" } else { "open" });
        let (network, _) = engine.ensure_network(&name, internal).unwrap();
        let holder = engine.hold_network_up(&name, &image(), 120).unwrap();
        let on_host = wait_for_gateway(&network.gateway, Duration::from_secs(30));
        let result = reached(&engine, &name, &targets(&network.gateway));
        if let Some(holder) = &holder {
            engine.destroy(holder).unwrap();
        }
        assert!(engine.delete_network(&name));
        done.push(on_host);
        result
    };

    let control = probe(false, &|_| vec![("internet", "1.1.1.1".into(), 443)]);
    assert_eq!(
        control,
        [("internet", true)],
        "the control found no route out"
    );

    let lan = lan_address();
    let sealed = probe(true, &|gateway| {
        vec![
            ("internet", "1.1.1.1".into(), 443),
            ("metadata", "169.254.169.254".into(), 80),
            ("dns", "example.com".into(), 0),
            ("lan", lan.clone(), port),
            ("gateway", gateway.to_string(), port),
        ]
    });
    drop(listener);
    for (label, got) in &sealed[..4] {
        assert!(!got, "a sealed container reached {label}");
    }
    // The known gap: the gateway is a host address, so a listener on every address answers.
    // When a fix lands, this flips. With the bridge in a VM (Docker Desktop) it is not this host.
    if done[1] {
        assert_eq!(
            sealed[4],
            ("gateway", true),
            "the gap closed: update docs/dev/tsk-tsk.md"
        );
    }
}

/// The reaper is `sh` and the engine's real CLI: when its run ends without a teardown, the
/// running container goes, and so does the record. `runs.rs` tests the script on a stub CLI.
#[test]
#[ignore]
fn a_reaper_deletes_a_running_container_when_its_run_ends() {
    let engine = engine();
    let spec = ContainerSpec {
        entrypoint: Some("sleep".into()),
        command: vec!["300".into()],
        detach: true,
        ..ContainerSpec::new(unique("reap"), image())
    };
    let out = run(&engine, &spec);
    assert!(out.status.success(), "{out:?}");
    let dir = std::env::temp_dir().join(unique("runs"));
    let record = runs::claim(&dir, engine.kind().name(), &spec.name).unwrap();
    let reaper = Reaper::start(&record, &engine, &|m| panic!("{m}")).unwrap();
    let listed = |name: &str| {
        engine
            .list_containers(PREFIX)
            .unwrap()
            .iter()
            .any(|c| c.name == name)
    };
    let before = listed(&spec.name);
    drop(reaper);
    let after = listed(&spec.name);
    if after {
        let _ = engine.destroy(&spec.name);
    }
    let left = std::fs::read_dir(&dir).map_or(0, |d| d.count());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(before, "the reaper acted before its run ended");
    assert!(!after, "the container outlived its reaper");
    assert_eq!(left, 0, "the record outlived its containers");
}

#[test]
#[ignore]
fn an_image_that_was_pulled_exists_and_a_made_up_one_does_not() {
    let engine = engine();
    let pulled = ContainerSpec {
        entrypoint: Some("true".into()),
        ..ContainerSpec::new(unique("pull"), image())
    };
    let out = run(&engine, &pulled);
    engine.destroy(&pulled.name).unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(engine.image_exists(&image()));
    assert!(!engine.image_exists("sanduk-no-such-image:never"));
}

#[test]
#[ignore]
fn destroying_a_container_that_is_not_there_is_an_error() {
    assert!(engine().destroy(&unique("absent")).is_err());
}
