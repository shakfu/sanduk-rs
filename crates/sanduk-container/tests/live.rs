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

use sanduk_container::{CONTAINER_PREFIX, ContainerSpec, Engine, Kind, wait_for_gateway};

fn engine() -> Engine {
    let engine = Engine::get(std::env::var("SANDUK_RUNTIME").ok().as_deref()).unwrap();
    engine.require_run().unwrap();
    engine
}

fn image() -> String {
    std::env::var("SANDUK_TEST_IMAGE").unwrap_or_else(|_| "docker.io/library/alpine:3.20".into())
}

fn unique(what: &str) -> String {
    format!("{CONTAINER_PREFIX}test-{what}-{}", std::process::id())
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
    let listed = engine.list_containers(CONTAINER_PREFIX).unwrap();
    if let Some(holder) = &holder {
        engine.destroy(holder).unwrap();
    }
    let after = engine.list_containers(CONTAINER_PREFIX).unwrap();
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
