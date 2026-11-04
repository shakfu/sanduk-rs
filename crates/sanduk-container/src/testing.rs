//! A stand-in Docker engine, for testing code that drives one without a daemon. Behind the
//! `test-util` feature.
//!
//! [`Fake`] answers each command the way Docker does, from state it keeps in memory: images,
//! networks, containers. A container with its own entrypoint runs on the host, in the directory
//! mounted at its workdir: the relay probe's `curl` is answered with a plain HTTP request, and
//! `sh -c` runs as itself. The agent's container runs what [`Fake::on_agent`] returns. A build
//! records its tag and prints one line.
//!
//! The run record's reaper calls `false` in place of `docker`, so it never reaches a daemon on
//! the host. To the reaper, every container is then already gone.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::{AGENT_UID_LABEL, Captured, Engine, Exec, Kind, Prefix};

type Agent = Box<dyn Fn(&Path, &[String]) -> Command + Send + Sync>;

/// One container as the fake holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub image: String,
    /// `running` or `exited`.
    pub state: &'static str,
}

#[derive(Default)]
struct State {
    calls: Vec<Vec<String>>,
    /// Tag, and the agent uid it was built for.
    images: BTreeMap<String, Option<u32>>,
    /// Name, and whether it is internal.
    networks: BTreeMap<String, bool>,
    containers: BTreeMap<String, Held>,
}

/// A Docker engine with no daemon behind it. See the module docs.
#[derive(Default)]
pub struct Fake {
    state: Mutex<State>,
    agent: Mutex<Option<Agent>>,
}

/// What `docker run`'s flags say, as far as the fake needs.
struct Run {
    name: String,
    detach: bool,
    work: Option<PathBuf>,
    entrypoint: Option<String>,
    image: String,
    args: Vec<String>,
}

fn parse_run(argv: &[String]) -> Run {
    let mut run = Run {
        name: String::new(),
        detach: false,
        work: None,
        entrypoint: None,
        image: String::new(),
        args: Vec::new(),
    };
    let mut rest = argv.iter().skip(2);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-d" => run.detach = true,
            "--init" | "-i" | "--rm" | "-it" => {}
            "--name" | "--cpus" | "--memory" | "--cap-drop" | "--security-opt" | "--pids-limit"
            | "--user" | "-w" | "-e" | "--network" | "--entrypoint" | "--runtime" | "--mount"
            | "-v" => {
                let value = rest.next().cloned().unwrap_or_default();
                match arg.as_str() {
                    "--name" => run.name = value,
                    "--entrypoint" => run.entrypoint = Some(value),
                    "-v" => run.work = value.split(':').next().map(PathBuf::from),
                    _ => {}
                }
            }
            image => {
                run.image = image.to_string();
                run.args = rest.cloned().collect();
                break;
            }
        }
    }
    run
}

fn answer(code: i32, stdout: impl Into<String>) -> Captured {
    Captured {
        code: Some(code),
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

fn refuse(stderr: impl Into<String>) -> Captured {
    Captured {
        code: Some(1),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

/// The status line's code for a GET of `url`, as `curl -w %{http_code}` prints it; `000` when
/// nothing answers.
fn http_status(url: &str) -> String {
    let fetched = (|| -> Option<String> {
        let rest = url.strip_prefix("http://")?;
        let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
        let mut stream = std::net::TcpStream::connect(host).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        write!(
            stream,
            "GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .ok()?;
        let mut head = [0; 12];
        stream.read_exact(&mut head).ok()?;
        Some(String::from_utf8_lossy(&head[9..12]).into_owned())
    })();
    fetched.unwrap_or_else(|| "000".into())
}

impl Fake {
    pub fn new() -> Arc<Fake> {
        Arc::new(Fake::default())
    }

    /// A Docker engine whose every command this fake answers.
    pub fn engine(self: &Arc<Self>, prefix: Prefix) -> Engine {
        Engine::with_exec(Kind::Docker, self.clone(), prefix).poll_interval(Duration::ZERO)
    }

    /// What the agent's container runs: given the host directory mounted as its workdir and the
    /// agent's argv. Default: `true`.
    pub fn on_agent(&self, agent: impl Fn(&Path, &[String]) -> Command + Send + Sync + 'static) {
        *self.agent.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(agent));
    }

    /// An image already built, as if for `uid`.
    pub fn with_image(&self, tag: &str, uid: Option<u32>) {
        self.state().images.insert(tag.into(), uid);
    }

    /// Every command so far, `docker` first.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.state().calls.clone()
    }

    /// The containers it holds, running or not.
    pub fn containers(&self) -> BTreeMap<String, Held> {
        self.state().containers.clone()
    }

    pub fn networks(&self) -> Vec<String> {
        self.state().networks.keys().cloned().collect()
    }

    pub fn images(&self) -> Vec<String> {
        self.state().images.keys().cloned().collect()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, name: &str, image: &str, state: &'static str) {
        self.state().containers.insert(
            name.into(),
            Held {
                image: image.into(),
                state,
            },
        );
    }

    fn image(&self, argv: &[&str]) -> Captured {
        let mut st = self.state();
        match argv {
            ["inspect", "--format", _, tag] => match st.images.get(*tag) {
                Some(Some(uid)) => answer(
                    0,
                    format!(
                        "{}\n",
                        serde_json::json!({ AGENT_UID_LABEL: uid.to_string() })
                    ),
                ),
                Some(None) => answer(0, "null\n"),
                None => refuse(format!("No such image: {tag}")),
            },
            ["inspect", tag] if st.images.contains_key(*tag) => answer(0, "[{}]\n"),
            ["inspect", tag] => refuse(format!("No such image: {tag}")),
            ["ls", "--format", _, repository] => {
                let prefix = format!("{repository}:");
                answer(
                    0,
                    st.images
                        .keys()
                        .filter(|t| t.starts_with(&prefix))
                        .map(|t| format!("{t}\n"))
                        .collect::<String>(),
                )
            }
            ["rm", tag] if st.images.remove(*tag).is_some() => answer(0, ""),
            _ => refuse("unknown image command"),
        }
    }

    fn network(&self, argv: &[&str]) -> Captured {
        let mut st = self.state();
        match argv {
            ["inspect", name] => match st.networks.get(*name) {
                Some(internal) => answer(
                    0,
                    format!(
                        "[{{\"Name\": \"{name}\", \"Internal\": {internal}, \"IPAM\": {{\"Config\": \
                         [{{\"Subnet\": \"127.0.0.0/8\", \"Gateway\": \"127.0.0.1\"}}]}}}}]\n"
                    ),
                ),
                None => refuse(format!("network {name} not found")),
            },
            ["create", "--internal", name] => {
                st.networks.insert(name.to_string(), true);
                answer(0, "")
            }
            ["create", name] => {
                st.networks.insert(name.to_string(), false);
                answer(0, "")
            }
            ["rm", name] if st.networks.remove(*name).is_some() => answer(0, ""),
            _ => refuse("unknown network command"),
        }
    }

    fn build(&self, argv: &[&str]) -> Captured {
        let mut tag = None;
        let mut uid = None;
        for pair in argv.windows(2) {
            match pair {
                ["-t", t] => tag = Some(t.to_string()),
                ["--build-arg", a] => {
                    if let Some(u) = a.strip_prefix("AGENT_UID=") {
                        uid = u.parse().ok();
                    }
                }
                _ => {}
            }
        }
        match tag {
            Some(tag) => {
                self.state().images.insert(tag, uid);
                answer(0, "")
            }
            None => refuse("build: no -t"),
        }
    }

    fn run_container(&self, argv: &[String]) -> Captured {
        let run = parse_run(argv);
        self.set(&run.name, &run.image, "running");
        if run.detach {
            return answer(0, format!("{}\n", run.name));
        }
        if run.entrypoint.as_deref() == Some("curl") {
            self.set(&run.name, &run.image, "exited");
            return answer(0, http_status(run.args.last().map_or("", String::as_str)));
        }
        let out = self.command(argv).output();
        self.set(&run.name, &run.image, "exited");
        match out {
            Ok(out) => Captured {
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => refuse(e.to_string()),
        }
    }
}

impl Exec for Fake {
    fn run(&self, argv: &[String], _capture: bool) -> Captured {
        self.state().calls.push(argv.to_vec());
        let args: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
        match args.as_slice() {
            ["info", "--format", f] if f.contains("ServerVersion") => answer(0, "29.0.0\n"),
            ["info", "--format", _] => answer(0, "Fake Linux\n"),
            ["image", rest @ ..] => self.image(rest),
            ["network", rest @ ..] => self.network(rest),
            ["build", rest @ ..] => self.build(rest),
            ["ps", ..] => answer(
                0,
                self.state()
                    .containers
                    .iter()
                    .map(|(n, c)| format!("{n}\t{}\t{}\n", c.image, c.state))
                    .collect::<String>(),
            ),
            ["stop", name] => {
                if let Some(c) = self.state().containers.get_mut(*name) {
                    c.state = "exited";
                }
                answer(0, "")
            }
            ["rm", name] => match self.state().containers.remove(*name) {
                Some(_) => answer(0, ""),
                None => refuse(format!("No such container: {name}")),
            },
            ["exec", _, "curl", .., url] => answer(0, http_status(url)),
            ["run", ..] => self.run_container(argv),
            _ => refuse(format!("the fake has no answer for {}", argv.join(" "))),
        }
    }

    fn which(&self, _program: &str) -> bool {
        true
    }

    fn reaper_cli(&self) -> Option<&str> {
        Some("false")
    }

    /// `docker run` as a host process: the entrypoint in the mounted directory, or the agent.
    /// The container is held as exited from the start: nothing tells the fake when it ends.
    fn command(&self, argv: &[String]) -> Command {
        if argv.get(1).map(String::as_str) == Some("build") {
            let captured = self.run(argv, true);
            let mut command = Command::new("sh");
            command.args(["-c", "echo \"$1\"; exit \"$2\"", "build"]);
            command.args([
                format!("built {}", argv.join(" ")),
                captured.code.unwrap_or(1).to_string(),
            ]);
            return command;
        }
        if argv.get(1).map(String::as_str) != Some("run") {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            return command;
        }
        self.state().calls.push(argv.to_vec());
        let run = parse_run(argv);
        self.set(&run.name, &run.image, "exited");
        let work = run.work.clone().unwrap_or_else(|| PathBuf::from("."));
        let mut command = match &run.entrypoint {
            Some(entrypoint) => {
                let mut command = Command::new(entrypoint);
                command.args(&run.args);
                command
            }
            None => match &*self.agent.lock().unwrap_or_else(PoisonError::into_inner) {
                Some(agent) => agent(&work, &run.args),
                None => Command::new("true"),
            },
        };
        command.current_dir(work);
        command
    }
}
