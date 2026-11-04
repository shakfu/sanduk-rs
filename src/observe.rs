//! What a run reports while it works, and how a caller stops it. The library prints nothing.

/// One thing a run has to say.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Note {
    /// Progress or a warning, one line.
    Info(String),
    /// One line from the relay: a call it forwarded or refused.
    Relay(String),
    /// One line of output from a container sanduk runs itself: the check, or the clone's return.
    Output(String),
    /// One line of the engine's image build.
    Build(String),
}

/// Receives a run's notes. Called from the relay's thread as well as the caller's, so `Sync`.
pub trait Observer: Send + Sync {
    fn note(&self, note: Note);

    /// Polled while a step runs: true ends it early, as a signal does in the CLI.
    fn cancelled(&self) -> bool {
        false
    }
}

/// Discards every note and never cancels.
impl Observer for () {
    fn note(&self, _: Note) {}
}

/// `obs.note(Note::Info(..))`, shorter.
pub fn info(obs: &dyn Observer, msg: impl Into<String>) {
    obs.note(Note::Info(msg.into()));
}
