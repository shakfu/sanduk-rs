//! The one error type: a message, the exit status the CLI ends with, and a kind a caller can act on.

use std::fmt;

/// What a failed run means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The request cannot run as given: a dirty tree under `clone_into`, an unknown agent, a bad
    /// flag. Nothing was acquired; retrying unchanged fails the same way.
    Refused,
    /// The machine was not ready: the engine is down, an image build failed, no key. Retryable.
    Setup,
    /// The clone's return failed. The work is in the kept clone.
    Return,
    /// The observer cancelled the run.
    Interrupted,
    /// Not yet classified.
    Other,
}

/// The facts of an error a caller may offer its own remedy for. A variant is added only where
/// one has a remedy; `message` stays a correct default for a caller that ignores it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Detail {
    /// The image runs its agent as `agent_uid`, which cannot write `workdir`, owned by `owner`.
    /// A rebuild builds as the calling user's uid, so it fixes this only when that is `owner`.
    AgentUid {
        image: String,
        agent_uid: u32,
        workdir: std::path::PathBuf,
        owner: u32,
        /// The engine, by name: `docker` or `apple`.
        runtime: &'static str,
    },
}

/// A failure the caller can act on. `code` becomes the exit status.
///
/// No message names a CLI flag. One about a single option names it in `option`, as its
/// [`run::Options`](crate::run::Options) field (`mounts`, `image.kits`), and `message` is a
/// predicate on it: `Display` joins the two with a space. A caller with its own names for the
/// options, as the CLI has flags, joins its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    pub code: i32,
    pub kind: ErrorKind,
    pub option: Option<&'static str>,
    pub detail: Option<Detail>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
            kind: ErrorKind::Other,
            option: None,
            detail: None,
        }
    }

    /// An error about `option`, a [`run::Options`](crate::run::Options) field. `message` reads
    /// after the option's name.
    pub fn about(option: &'static str, message: impl Into<String>) -> Self {
        Self {
            option: Some(option),
            ..Self::new(message)
        }
    }

    pub fn with_code(message: impl Into<String>, code: i32) -> Self {
        Self {
            code,
            ..Self::new(message)
        }
    }

    pub fn of_kind(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            ..Self::new(message)
        }
    }

    /// `kind`, unless one was set already.
    pub fn or_kind(mut self, kind: ErrorKind) -> Self {
        if self.kind == ErrorKind::Other {
            self.kind = kind;
        }
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.option {
            Some(option) => write!(f, "{option} {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(e.to_string())
    }
}

impl From<crate::providers::Error> for Error {
    fn from(e: crate::providers::Error) -> Self {
        Error::new(e.0)
    }
}

impl From<sanduk_container::Error> for Error {
    fn from(e: sanduk_container::Error) -> Self {
        Error::new(e.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `Err` with a message, for `return fail(...)`.
pub fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::new(message))
}
