//! Running the agent: stream its output through a reader, under a timeout and the signals that
//! should end it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Agent, Outcome};
use crate::error::{Error, Result};

static CAUGHT: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signum: libc::c_int) {
    CAUGHT.store(signum, Ordering::SeqCst);
}

/// SIGINT, SIGTERM and SIGHUP recorded rather than acted on while held, so a run tears down what
/// it started instead of dying with its container alive. SIGKILL cannot be caught: the run
/// records in `runs` are for that. Restores the previous handlers when dropped.
pub struct Signals {
    previous: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl Signals {
    pub fn catch() -> Self {
        CAUGHT.store(0, Ordering::SeqCst);
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        let previous = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
            .into_iter()
            // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
            .map(|s| (s, unsafe { libc::signal(s, handler) }))
            .collect();
        Signals { previous }
    }

    /// The signal that arrived while held, if any.
    pub fn caught() -> Option<i32> {
        Some(CAUGHT.load(Ordering::SeqCst)).filter(|s| *s != 0)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for (signum, handler) in &self.previous {
            // SAFETY: restoring the handler that was there before.
            unsafe { libc::signal(*signum, *handler) };
        }
    }
}

/// How the run ended, when it ended on its own.
pub struct Ran {
    pub outcome: Option<Outcome>,
    pub code: i32,
}

/// Streams the agent's output, returning its outcome and exit status.
///
/// The watchdog enforces the timeout: an agent that hangs without printing would never trip a
/// deadline checked inside the read loop. It also kills the agent when a signal arrives. Killing
/// the client does not stop the container; the caller deletes it.
///
/// `attach_stdin` hands this process's stdin to the agent. `passthrough` copies every line, before
/// it is parsed, to stdout: a reader that cannot make sense of a line must not decide whether it
/// is delivered.
pub fn launch(
    agent: &Agent,
    argv: &[String],
    timeout: Duration,
    quiet: bool,
    env: &[(String, String)],
    attach_stdin: bool,
    passthrough: bool,
) -> Result<Ran> {
    let mut reader = agent.reader();
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| Error::new("empty command"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(if attach_stdin {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped());
    // Its own process group, so a kill reaches whatever the engine's CLI started as well: one left
    // holding the output pipe would keep the read loop waiting past the deadline. Not with
    // --stdin: a background group that reads the terminal is stopped by SIGTTIN.
    let grouped = !attach_stdin;
    if grouped {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| Error::new(format!("{program}: {e}")))?;
    let pid = child.id() as libc::pid_t;

    let timed_out = Arc::new(AtomicBool::new(false));
    let (done, finished) = mpsc::channel::<()>();
    let expired = timed_out.clone();
    let watchdog = std::thread::spawn(move || {
        let deadline = Instant::now() + timeout;
        loop {
            match finished.recv_timeout(Duration::from_millis(100)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            let late = Instant::now() >= deadline;
            if late || Signals::caught().is_some() {
                expired.store(late, Ordering::SeqCst);
                // SAFETY: kill takes no pointers. The pid is still ours: the read loop has not
                // reported the child finished, and it is reaped only after that.
                // As a group leader, its pid is also its group's id.
                unsafe {
                    if grouped {
                        libc::killpg(pid, libc::SIGKILL);
                    } else {
                        libc::kill(pid, libc::SIGKILL);
                    }
                };
                return;
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut trace = |line: String| {
        if !quiet {
            println!("{line}");
        }
    };
    let pipe = BufReader::new(child.stdout.take().expect("piped"));
    for raw in pipe.split(b'\n') {
        let Ok(raw) = raw else { break };
        let line = String::from_utf8_lossy(&raw);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if passthrough {
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
        }
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(record)) if line.trim_start().starts_with('{') => {
                reader.event(&record, &mut trace)
            }
            _ => reader.line(line, &mut trace),
        }
    }
    let _ = done.send(());
    let status = child.wait()?;
    let _ = watchdog.join();

    if let Some(signum) = Signals::caught() {
        return Err(Error::with_code("interrupted", 128 + signum));
    }
    if timed_out.load(Ordering::SeqCst) {
        return Err(Error::with_code(
            format!("agent exceeded --timeout {}s", timeout.as_secs()),
            124,
        ));
    }
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    });
    Ok(Ran {
        outcome: reader.finish(),
        code,
    })
}
