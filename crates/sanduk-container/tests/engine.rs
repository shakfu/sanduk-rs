//! Engine tests: registry, argv construction, and how each engine's output is read.
//!
//! No engine is contacted. [`Fake`] answers every command and records its argv, which is the
//! whole of what an engine is: the shape of each command and how its output is read.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sanduk_container::{
    Captured, ContainerSpec, Engine, Exec, HOLDER_SECONDS, Kind, Mount, Network, default_kind,
    wait_for_gateway,
};

type Reply = Box<dyn Fn(&[String]) -> Captured + Send + Sync>;

struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    reply: Reply,
    on_path: bool,
}

impl Exec for Fake {
    fn run(&self, argv: &[String], _capture: bool) -> Captured {
        self.calls.lock().unwrap().push(argv.to_vec());
        (self.reply)(argv)
    }

    fn which(&self, _program: &str) -> bool {
        self.on_path
    }
}

fn answer(code: i32, stdout: &str) -> Captured {
    Captured {
        code: Some(code),
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

/// An engine whose every command gets the same answer, and the record of what it ran.
fn engine(kind: Kind, code: i32, stdout: &str) -> (Engine, Arc<Fake>) {
    let stdout = stdout.to_string();
    engine_with(kind, move |_| answer(code, &stdout))
}

fn engine_with(
    kind: Kind,
    reply: impl Fn(&[String]) -> Captured + Send + Sync + 'static,
) -> (Engine, Arc<Fake>) {
    let fake = Arc::new(Fake {
        calls: Mutex::new(Vec::new()),
        reply: Box::new(reply),
        on_path: true,
    });
    let engine = Engine::with_exec(kind, fake.clone()).poll_interval(Duration::ZERO);
    (engine, fake)
}

fn calls(fake: &Fake) -> Vec<Vec<String>> {
    fake.calls.lock().unwrap().clone()
}

fn argv(kind: Kind, spec: &ContainerSpec) -> Vec<String> {
    Engine::new(kind).run_argv(spec)
}

fn after<'a>(argv: &'a [String], flag: &str) -> &'a str {
    let i = argv.iter().position(|a| a == flag).expect(flag);
    &argv[i + 1]
}

fn has(argv: &[String], item: &str) -> bool {
    argv.iter().any(|a| a == item)
}

fn one_spec() -> ContainerSpec {
    ContainerSpec {
        command: vec!["-p".into(), "go".into()],
        mount: Some((PathBuf::from("/tmp/w"), "/work".into())),
        inherit_env: vec!["ANTHROPIC_API_KEY".into()],
        network: Some("sanduk-net".into()),
        ..ContainerSpec::new("sanduk-x", "sanduk:latest")
    }
}

// --- registry -----------------------------------------------------------------------------------

#[test]
fn the_default_runtime_is_the_first_installed_for_the_platform() {
    let cases: [(&str, &[&str], Kind); 6] = [
        ("macos", &["container", "docker"], Kind::Apple),
        ("macos", &["docker"], Kind::Docker),
        ("macos", &[], Kind::Apple),
        // A `container` on Linux is not Apple's engine.
        ("linux", &["container", "docker"], Kind::Docker),
        ("linux", &[], Kind::Docker),
        ("windows", &["docker"], Kind::Docker),
    ];
    for (os, on_path, expected) in cases {
        assert_eq!(
            default_kind(os, |cli| on_path.contains(&cli)),
            expected,
            "{os} {on_path:?}"
        );
    }
}

#[test]
fn an_explicit_runtime_is_not_cascaded() {
    assert_eq!(Engine::get(Some("apple")).unwrap().cli(), "container");
}

#[test]
fn unknown_runtime_names_the_known_ones() {
    let err = Engine::get(Some("nerdctl")).unwrap_err();
    assert!(err.0.contains("apple, docker"), "{err}");
}

// --- argv construction --------------------------------------------------------------------------

#[test]
fn the_network_holder_is_detached_and_runs_no_agent() {
    let spec = ContainerSpec {
        network: Some("sanduk-net".into()),
        detach: true,
        entrypoint: Some("sleep".into()),
        command: vec!["86400".into()],
        ..ContainerSpec::new("sanduk-hold-x", "sanduk:latest")
    };
    let argv = argv(Kind::Apple, &spec);
    assert!(has(&argv, "-d"));
    assert!(!has(&argv, "-v"));
    assert_eq!(
        argv[argv.len() - 4..],
        ["--entrypoint", "sleep", "sanduk:latest", "86400"]
    );
}

/// Bare `-e NAME`: the engine inherits the value, so it stays out of the host's process list.
#[test]
fn an_inherited_variable_is_named_without_its_value() {
    let argv = argv(Kind::Docker, &one_spec());
    assert_eq!(after(&argv, "-e"), "ANTHROPIC_API_KEY");
}

#[test]
fn the_workdir_is_mounted_and_entered() {
    let argv = argv(Kind::Docker, &one_spec());
    assert_eq!(after(&argv, "-v"), "/tmp/w:/work");
    assert_eq!(after(&argv, "-w"), "/work");
}

#[test]
fn an_oci_runtime_reaches_docker_before_the_image() {
    let spec = ContainerSpec {
        oci_runtime: Some("runsc".into()),
        ..ContainerSpec::new("n", "img")
    };
    let argv = argv(Kind::Docker, &spec);
    assert_eq!(after(&argv, "--runtime"), "runsc");
    let at = |s: &str| argv.iter().position(|a| a == s).unwrap();
    assert!(at("--runtime") < at("img"));
}

#[test]
fn without_an_oci_runtime_docker_keeps_its_default() {
    assert!(!has(
        &argv(Kind::Docker, &ContainerSpec::new("n", "img")),
        "--runtime"
    ));
}

/// Hardening is per engine by design; the rest of the argv is the spec's.
fn without_hardening(kind: Kind, argv: &[String]) -> Vec<String> {
    let block = kind.hardening();
    let start = argv.iter().position(|a| a == block[0]).unwrap();
    assert_eq!(argv[start..start + block.len()], *block);
    [&argv[..start], &argv[start + block.len()..]].concat()
}

/// `run_argv` is shared. Apple's engine adopted Docker's flags, so the day that stops being
/// true, this fails rather than a container run.
#[test]
fn the_two_engines_render_one_spec_identically() {
    let (a, d) = (
        argv(Kind::Apple, &one_spec()),
        argv(Kind::Docker, &one_spec()),
    );
    assert_eq!(
        without_hardening(Kind::Apple, &a)[1..],
        without_hardening(Kind::Docker, &d)[1..]
    );
    assert_eq!((a[0].as_str(), d[0].as_str()), ("container", "docker"));
}

/// The agent runs unprivileged and only reads, writes and forks; `--init` so a shell it leaves
/// behind is reaped rather than held by pid 1.
#[test]
fn every_container_drops_its_capabilities() {
    for kind in Kind::ALL {
        let argv = argv(kind, &one_spec());
        assert_eq!(after(&argv, "--cap-drop"), "ALL");
        assert!(has(&argv, "--init"));
    }
}

/// A shared kernel is where these matter, and Apple's CLI has neither flag: passing them there
/// would fail the run rather than harden it.
#[test]
fn only_docker_bounds_pids_and_new_privileges() {
    let docker = argv(Kind::Docker, &one_spec());
    assert_eq!(after(&docker, "--security-opt"), "no-new-privileges");
    assert_eq!(after(&docker, "--pids-limit"), "1024");
    let apple = argv(Kind::Apple, &one_spec());
    assert!(!has(&apple, "--pids-limit") && !has(&apple, "--security-opt"));
}

#[test]
fn only_docker_explains_a_gateway_that_will_not_bind() {
    assert!(Kind::Docker.gateway_hint().contains("VM"));
    assert_eq!(Kind::Apple.gateway_hint(), "");
}

/// `-v host:dest:ro` is Docker's alone; `--mount ...,readonly` is shared.
#[test]
fn a_read_only_mount_is_spelled_the_way_both_engines_read_it() {
    let spec = ContainerSpec {
        mounts: vec![Mount {
            host: "/tmp/repo".into(),
            dest: "/repo".into(),
            ro: true,
        }],
        ..one_spec()
    };
    for kind in Kind::ALL {
        assert_eq!(
            after(&argv(kind, &spec), "--mount"),
            "type=bind,source=/tmp/repo,target=/repo,readonly"
        );
    }
}

#[test]
fn a_writable_extra_mount_leaves_readonly_off() {
    let spec = ContainerSpec {
        mounts: vec![Mount {
            host: "/tmp/repo".into(),
            dest: "/repo".into(),
            ro: false,
        }],
        ..one_spec()
    };
    assert_eq!(
        after(&argv(Kind::Docker, &spec), "--mount"),
        "type=bind,source=/tmp/repo,target=/repo"
    );
}

#[test]
fn the_user_is_rendered_before_the_image() {
    let spec = ContainerSpec {
        user: Some("1001:1001".into()),
        ..ContainerSpec::new("n", "sanduk:latest")
    };
    let argv = argv(Kind::Docker, &spec);
    assert_eq!(after(&argv, "--user"), "1001:1001");
    let at = |s: &str| argv.iter().position(|a| a == s).unwrap();
    assert!(at("--user") < at("sanduk:latest"));
}

#[test]
fn stdin_is_open_only_when_it_is_asked_for() {
    assert!(!has(&argv(Kind::Docker, &one_spec()), "-i"));
    let spec = ContainerSpec {
        stdin: true,
        ..one_spec()
    };
    assert!(has(&argv(Kind::Docker, &spec), "-i"));
}

// --- images -------------------------------------------------------------------------------------

#[test]
fn apple_image_exists_reads_the_listing() {
    let listing = "NAME      TAG      DIGEST\n\
                   sanduk  latest   7429d9f6127f\n\
                   alpine    3.20     d9e853e87e55\n";
    let (e, _) = engine(Kind::Apple, 0, listing);
    assert!(e.image_exists("sanduk:latest"));
    assert!(e.image_exists("sanduk")); // the tag defaults to latest
    assert!(!e.image_exists("sanduk:test"));
    assert!(!e.image_exists("missing:latest"));
}

/// Measured 2026-09-25 on `container` 1.2: a pulled `docker.io/library/alpine:3.20` is listed as
/// `alpine 3.20`. A suffix match missed it, and matched a lookalike instead.
#[test]
fn apple_image_exists_matches_whole_names() {
    let listing = "NAME                         TAG     DIGEST\n\
                   alpine                       3.20    d9e853e87e55\n\
                   xsanduk                      latest  1\n\
                   localhost:5000/tool          latest  2\n";
    let (e, _) = engine(Kind::Apple, 0, listing);
    assert!(e.image_exists("docker.io/library/alpine:3.20"));
    assert!(e.image_exists("alpine:3.20"));
    assert!(!e.image_exists("sanduk:latest"));
    assert!(e.image_exists("localhost:5000/tool"));
}

#[test]
fn docker_image_exists_is_an_exit_status() {
    let (e, fake) = engine(Kind::Docker, 0, "");
    assert!(e.image_exists("sanduk-hax:latest"));
    assert_eq!(
        calls(&fake),
        [["docker", "image", "inspect", "sanduk-hax:latest"]]
    );
    let (e, _) = engine(Kind::Docker, 1, "");
    assert!(!e.image_exists("nope:latest"));
}

#[test]
fn image_tags_are_read_per_engine() {
    let listing = "NAME                    TAG     DIGEST\n\
                   sanduk-hax              abc     1\n\
                   docker.io/sanduk-hax    def     2\n\
                   sanduk-haxx             ghi     3\n";
    let (apple, _) = engine(Kind::Apple, 0, listing);
    assert_eq!(
        apple.image_tags("sanduk-hax"),
        ["sanduk-hax:abc", "docker.io/sanduk-hax:def"]
    );
    let (docker, _) = engine(Kind::Docker, 0, "sanduk-hax:abc\nsanduk-hax:<none>\n");
    assert_eq!(docker.image_tags("sanduk-hax"), ["sanduk-hax:abc"]);
}

/// null is an image with no labels; empty is a failed inspect.
#[test]
fn docker_reads_the_agents_uid_from_its_label() {
    for (stdout, uid) in [
        ("{\"sanduk.agent-uid\": \"1001\"}\n", Some(1001)),
        ("null\n", None),
        ("", None),
    ] {
        let (e, fake) = engine(Kind::Docker, 0, stdout);
        assert_eq!(e.image_uid("sanduk:latest"), uid, "{stdout:?}");
        assert_eq!(calls(&fake)[0].last().unwrap(), "sanduk:latest");
    }
}

/// One split, three resources: `container delete` against `docker rm`.
#[test]
fn the_delete_verb_covers_images_and_networks() {
    for (kind, verb) in [(Kind::Apple, "delete"), (Kind::Docker, "rm")] {
        let (e, fake) = engine(kind, 0, "");
        assert!(e.delete_image("sanduk:latest"));
        assert!(e.delete_network("sanduk-net"));
        let calls = calls(&fake);
        assert_eq!(calls[0][1..], ["image", verb, "sanduk:latest"]);
        assert_eq!(calls[1][1..], ["network", verb, "sanduk-net"]);
    }
}

#[test]
fn a_missing_containerfile_is_named() {
    let (e, fake) = engine(Kind::Docker, 0, "");
    let err = e
        .build_image("sanduk:latest", &PathBuf::from("/no/such/Containerfile"))
        .unwrap_err();
    assert!(err.0.contains("no Containerfile at"), "{err}");
    assert!(calls(&fake).is_empty());
}

#[test]
fn a_build_runs_in_the_containerfiles_directory() {
    let dir = std::env::temp_dir().join(format!("sanduk-build-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Containerfile"), "FROM scratch\n").unwrap();
    let (e, fake) = engine(Kind::Apple, 0, "");
    e.build_image("sanduk:latest", &dir.join("Containerfile"))
        .unwrap();
    let (failed, _) = engine(Kind::Apple, 3, "");
    let err = failed
        .build_image("sanduk:latest", &dir.join("Containerfile"))
        .unwrap_err();
    let resolved = std::fs::canonicalize(&dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let argv = &calls(&fake)[0];
    assert_eq!(argv[..4], ["container", "build", "-t", "sanduk:latest"]);
    assert_eq!(argv.last().unwrap(), &resolved.display().to_string());
    assert!(err.0.contains("exit 3"), "{err}");
}

// --- networks -----------------------------------------------------------------------------------

const DOCKER_NETWORK: &str = r#"
[{"Name": "sanduk-net", "Internal": true,
  "IPAM": {"Config": [{"Subnet": "172.20.0.0/16", "Gateway": "172.20.0.1"}]}}]
"#;

const DOCKER_ROUTABLE: &str = r#"[{"Name": "sanduk-net", "Internal": false,
  "IPAM": {"Config": [{"Subnet": "172.20.0.0/16", "Gateway": "172.20.0.1"}]}}]"#;

const APPLE_NETWORK: &str =
    r#"[{"status": {"ipv4Gateway": "192.168.64.1", "ipv4Subnet": "192.168.64.0/24"}}]"#;

fn docker_net() -> Network {
    Network {
        gateway: "172.20.0.1".into(),
        subnet: "172.20.0.0/16".into(),
    }
}

/// Apple reports it under status.ipv4Gateway; Docker under IPAM.Config.
#[test]
fn each_engine_reads_the_gateway_from_its_own_block() {
    let (docker, _) = engine(Kind::Docker, 0, DOCKER_NETWORK);
    assert_eq!(docker.network_info("sanduk-net"), Some(docker_net()));
    let (apple, _) = engine(Kind::Apple, 0, APPLE_NETWORK);
    assert_eq!(
        apple.network_info("sanduk-net").unwrap().gateway,
        "192.168.64.1"
    );
}

/// A network with no address must read as absent, not as a partial answer: `ensure_network`
/// turns `None` into an error naming the network.
#[test]
fn network_info_is_none_when_the_gateway_is_absent() {
    for stdout in ["", "not json", "[]", r#"[{"IPAM": {}}]"#] {
        let (e, _) = engine(Kind::Docker, 0, stdout);
        assert_eq!(e.network_info("sanduk-net"), None, "{stdout:?}");
    }
    let (missing, _) = engine(Kind::Docker, 1, DOCKER_NETWORK);
    assert_eq!(missing.network_info("sanduk-net"), None);
}

/// Every call fails, as a second `network create` does, until the first run's network appears.
fn racing(kind: Kind, appears_after: usize) -> Engine {
    let inspected = Mutex::new(0);
    let reply = move |argv: &[String]| {
        if argv[1..3] == ["network", "inspect"] {
            let mut n = inspected.lock().unwrap();
            *n += 1;
            if *n > appears_after {
                return answer(0, DOCKER_NETWORK);
            }
        }
        Captured {
            code: Some(1),
            stdout: String::new(),
            stderr: "has a pending operation".into(),
        }
    };
    engine_with(kind, reply).0
}

/// Two runs found no network and both created it; the second create failed.
#[test]
fn a_network_another_run_is_creating_is_waited_for() {
    let (network, created) = racing(Kind::Docker, 2).ensure_network("n", true).unwrap();
    assert_eq!(network, docker_net());
    assert!(!created);
}

#[test]
fn a_network_that_never_appears_is_still_an_error() {
    let err = racing(Kind::Docker, usize::MAX)
        .ensure_network("n", true)
        .unwrap_err();
    assert!(err.0.contains("pending operation"), "{err}");
}

#[test]
fn a_new_network_is_created_internal_and_reported_as_created() {
    let created = Mutex::new(false);
    let (e, fake) = engine_with(Kind::Docker, move |argv| {
        let mut done = created.lock().unwrap();
        if argv[1..3] == ["network", "create"] {
            *done = true;
            return answer(0, "");
        }
        if *done {
            answer(0, DOCKER_NETWORK)
        } else {
            answer(1, "")
        }
    });
    let (network, created) = e.ensure_network("sanduk-net", true).unwrap();
    assert!(created);
    assert_eq!(network, docker_net());
    assert!(calls(&fake).contains(&vec![
        "docker".to_string(),
        "network".into(),
        "create".into(),
        "--internal".into(),
        "sanduk-net".into(),
    ]));
}

/// A `key-safe` bridge under the name a sealed run asked for left that run with a route off the
/// host.
#[test]
fn a_sealed_run_refuses_a_routable_network_it_would_reuse() {
    let (e, _) = engine(Kind::Docker, 0, DOCKER_ROUTABLE);
    let err = e.ensure_network("sanduk-net", true).unwrap_err();
    assert!(err.0.contains("route off the host"), "{err}");
}

#[test]
fn an_internal_network_is_reused() {
    let (e, _) = engine(Kind::Docker, 0, DOCKER_NETWORK);
    assert_eq!(
        e.ensure_network("sanduk-net", true).unwrap(),
        (docker_net(), false)
    );
}

/// key-safe asks for egress, so a routable network is what it wants.
#[test]
fn a_routable_run_reuses_a_routable_network() {
    let (e, _) = engine(Kind::Docker, 0, DOCKER_ROUTABLE);
    assert_eq!(
        e.ensure_network("sanduk-open", false).unwrap().0,
        docker_net()
    );
}

/// Apple's engine does not report the mode, so there is nothing to refuse on.
#[test]
fn an_engine_that_does_not_report_the_mode_reuses_the_network() {
    let (e, _) = engine(Kind::Apple, 0, APPLE_NETWORK);
    assert_eq!(e.network_internal("sanduk-net"), None);
    assert_eq!(
        e.ensure_network("sanduk-net", true).unwrap().0.gateway,
        "192.168.64.1"
    );
}

/// Docker creates the bridge with the network; vmnet only while a container is attached.
#[test]
fn only_apple_needs_a_network_holder() {
    let (docker, fake) = engine(Kind::Docker, 0, "");
    assert_eq!(
        docker.hold_network_up("sanduk-net", "img", 60).unwrap(),
        None
    );
    assert!(calls(&fake).is_empty());
}

/// It was a flat day, which is a ceiling nothing announced: a longer run lost its bridge
/// mid-flight and failed as if the network had broken.
#[test]
fn the_holder_sleeps_as_long_as_it_is_told() {
    let (e, fake) = engine(Kind::Apple, 0, "");
    let first = e
        .hold_network_up("sanduk-net", "img", 3600)
        .unwrap()
        .unwrap();
    e.hold_network_up("sanduk-net", "img", HOLDER_SECONDS)
        .unwrap();
    let calls = calls(&fake);
    assert!(first.starts_with("sanduk-hold-"), "{first}");
    assert_eq!(after(&calls[0], "--name"), first);
    assert_eq!(after(&calls[0], "--entrypoint"), "sleep");
    assert_eq!(after(&calls[0], "--network"), "sanduk-net");
    assert_eq!(calls[0].last().unwrap(), "3600");
    assert_eq!(calls[1].last().unwrap(), &HOLDER_SECONDS.to_string());
}

#[test]
fn a_holder_that_will_not_start_is_an_error() {
    let (e, _) = engine(Kind::Apple, 1, "");
    assert!(e.hold_network_up("sanduk-net", "img", 60).is_err());
}

#[test]
fn a_gateway_on_this_host_is_bindable_and_one_elsewhere_is_not() {
    assert!(wait_for_gateway("127.0.0.1", Duration::ZERO));
    // TEST-NET-1 (RFC 5737): assigned to no host.
    assert!(!wait_for_gateway("192.0.2.1", Duration::ZERO));
}

// --- containers ---------------------------------------------------------------------------------

const APPLE_LIST: &str = "\
ID                IMAGE           OS     ARCH   STATE    IP
sanduk-e859b868   sanduk-hax:latest  linux  arm64  running  192.168.128.5/24
sanduk-hold-0af2  sanduk-hax:latest  linux  arm64  running  192.168.128.2/24
buildkit          builder:0.13.0     linux  arm64  running  192.168.64.2/24
";

const DOCKER_LIST: &str =
    "sanduk-e859b868\tsanduk-hax:latest\trunning\nbuildkit\tbuilder:0.13.0\trunning\n";

#[test]
fn apple_lists_containers_by_column() {
    let (e, _) = engine(Kind::Apple, 0, APPLE_LIST);
    let names: Vec<_> = e
        .list_containers("sanduk-")
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["sanduk-e859b868", "sanduk-hold-0af2"]);
}

#[test]
fn docker_lists_containers_by_format() {
    let (e, fake) = engine(Kind::Docker, 0, DOCKER_LIST);
    let found = e.list_containers("sanduk-").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        (&*found[0].name, &*found[0].image, &*found[0].state),
        ("sanduk-e859b868", "sanduk-hax:latest", "running")
    );
    assert!(has(&calls(&fake)[0], "{{.Names}}\t{{.Image}}\t{{.State}}"));
}

#[test]
fn an_empty_prefix_lists_everything() {
    let (e, _) = engine(Kind::Docker, 0, DOCKER_LIST);
    assert_eq!(e.list_containers("").unwrap().len(), 2);
}

/// An engine that cannot answer is not an engine holding no containers. A sweep that read a
/// stopped daemon's `[]` as "none" would drop the records of containers still holding a key.
#[test]
fn a_failed_listing_is_an_error_rather_than_empty() {
    for kind in Kind::ALL {
        let (e, _) = engine(kind, 1, DOCKER_LIST);
        assert!(e.list_containers("sanduk-").is_err(), "{kind:?}");
    }
}

/// A caller keeping a record of the container must know the delete failed.
#[test]
fn destroy_stops_then_deletes_and_reports_a_failed_delete() {
    let (e, fake) = engine(Kind::Docker, 0, "");
    e.destroy("sanduk-x").unwrap();
    assert_eq!(
        calls(&fake),
        [["docker", "stop", "sanduk-x"], ["docker", "rm", "sanduk-x"]]
    );
    let (failed, _) = engine(Kind::Apple, 1, "");
    assert!(
        failed
            .destroy("sanduk-x")
            .unwrap_err()
            .0
            .contains("sanduk-x")
    );
}

#[test]
fn a_shell_mounts_nothing_and_joins_no_network() {
    assert_eq!(
        Engine::new(Kind::Docker).shell_argv("img"),
        ["docker", "run", "--rm", "-it", "--entrypoint", "sh", "img"]
    );
}

// --- the engine's own service -------------------------------------------------------------------

#[test]
fn an_unreachable_docker_daemon_is_named() {
    let (e, _) = engine(Kind::Docker, 1, "");
    assert!(e.require().unwrap_err().0.contains("daemon"));
}

/// Measured 2026-09-25: a stopped service prints "apiserver is not running and not registered
/// with launchd" and exits 1. Either signal alone refuses it.
#[test]
fn a_stopped_apple_service_says_how_to_start_it() {
    for (code, stdout) in [
        (1, ""),
        (0, "apiserver is not running"),
        (1, "apiserver is running"),
    ] {
        let (e, _) = engine(Kind::Apple, code, stdout);
        let err = e.require().unwrap_err();
        assert!(
            err.0.contains("container system start"),
            "{code} {stdout:?}: {err}"
        );
    }
    let (running, _) = engine(Kind::Apple, 0, "apiserver is running");
    running.require().unwrap();
}

/// Its AppArmor profile blocks every exec under no-new-privileges.
#[test]
fn a_snap_docker_daemon_cannot_run_an_agent() {
    let (e, _) = engine(Kind::Docker, 0, "Ubuntu Core 24\n");
    assert!(e.require_run().unwrap_err().0.contains("snap package"));
}

/// ps, clean and destroy work there; destroy removes an older run's network.
#[test]
fn a_snap_docker_daemon_still_answers_the_other_verbs() {
    let (e, _) = engine(Kind::Docker, 0, "Ubuntu Core 24\n");
    e.require().unwrap();
}

#[test]
fn a_native_docker_daemon_can_run_an_agent() {
    let (e, fake) = engine(Kind::Docker, 0, "Ubuntu 24.04.5 LTS\n");
    e.require_run().unwrap();
    assert!(calls(&fake).contains(&vec![
        "docker".to_string(),
        "info".into(),
        "--format".into(),
        "{{.OperatingSystem}}".into(),
    ]));
}

/// Docker's daemon belongs to launchd, systemd, or Desktop. Pretending to start it would fail
/// somewhere less obvious.
#[test]
fn an_engine_without_a_service_command_says_so() {
    let (e, fake) = engine(Kind::Docker, 0, "");
    for result in [e.service_start(), e.service_stop()] {
        assert!(result.unwrap_err().0.contains("managed outside sanduk"));
    }
    assert!(calls(&fake).is_empty());
}

#[test]
fn apple_owns_its_service_commands() {
    let (e, fake) = engine(Kind::Apple, 0, "");
    e.service_start().unwrap();
    assert_eq!(calls(&fake)[0], ["container", "system", "start"]);
}

/// `status` exists to report a broken engine, so it must not need a working one.
#[test]
fn service_status_answers_rather_than_failing() {
    let fake = Arc::new(Fake {
        calls: Mutex::new(Vec::new()),
        reply: Box::new(|_| answer(0, "")),
        on_path: false,
    });
    let e = Engine::with_exec(Kind::Docker, fake);
    assert!(e.service_status().contains("not found on PATH"));
}
