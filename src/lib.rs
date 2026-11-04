//! sanduk: run a coding agent in a disposable container, with the API key held on the host.
//!
//! ```no_run
//! use sanduk::run::{Mode, Options};
//! # fn main() -> sanduk::error::Result<()> {
//! let sanduk = sanduk::Sanduk::new(sanduk::util::Dirs::default());
//! let plan = sanduk.plan(Options {
//!     task: Some("Summarise this repository.".into()),
//!     workdir: "./repo".into(),
//!     mode: Mode::KeySafe,
//!     ..Options::default()
//! })?;
//! let mut live = plan.start()?;
//! let status = live.command().status()?;
//! let outcome = live.finish(status);
//! # let _ = outcome; Ok(()) }
//! ```
//!
//! The container engines are [`sanduk_container`]; the per-process sandbox is
//! [`sanduk_sandbox`](https://docs.rs/sanduk-sandbox). This crate holds what only sanduk uses: the
//! providers, the relay, the agents, recipes and kits, and the run. See [`run`] for its phases.

use std::sync::Arc;

pub mod agent;
pub mod catalog;
pub mod clone;
pub mod error;
pub mod http;
pub mod kits;
pub mod observe;
pub mod preflight;
pub mod providers;
pub mod recipes;
pub mod relay;
pub mod resources;
pub mod run;
pub mod sections;
pub mod util;

/// Every container, holder and image repository sanduk names starts with this.
pub const CONTAINER_PREFIX: &str = "sanduk-";

/// [`CONTAINER_PREFIX`], as an engine takes it.
pub fn prefix() -> sanduk_container::Prefix {
    sanduk_container::Prefix::new(CONTAINER_PREFIX).expect("CONTAINER_PREFIX is a valid prefix")
}

/// Runs agents with one set of directories and one observer. Holds no process-wide state: two in
/// one process may differ in both.
#[derive(Clone)]
pub struct Sanduk {
    dirs: util::Dirs,
    obs: Arc<dyn observe::Observer>,
    engine: Option<sanduk_container::Engine>,
}

impl Sanduk {
    /// Notes discarded until [`observer`](Sanduk::observer) is set.
    pub fn new(dirs: util::Dirs) -> Self {
        Sanduk {
            dirs,
            obs: Arc::new(()),
            engine: None,
        }
    }

    /// Where this instance's runs send their notes, and ask whether to stop.
    pub fn observer(mut self, obs: Arc<dyn observe::Observer>) -> Self {
        self.obs = obs;
        self
    }

    /// Use `engine` rather than the one a run's `runtime` names: for tests, with
    /// `sanduk_container::testing::Fake` (feature `test-util`).
    pub fn engine(mut self, engine: sanduk_container::Engine) -> Self {
        self.engine = Some(engine);
        self
    }

    pub fn dirs(&self) -> &util::Dirs {
        &self.dirs
    }

    /// Checks `opts` and acquires nothing. See [`run::Plan`].
    pub fn plan(&self, opts: run::Options) -> error::Result<run::Plan> {
        run::Plan::new(&self.dirs, self.obs.clone(), self.engine.clone(), opts)
    }
}
