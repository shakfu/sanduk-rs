//! The one error type the CLI turns into a line and an exit status.

use std::fmt;

/// A failure the caller can act on. `code` becomes the exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    pub code: i32,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
        }
    }

    pub fn with_code(message: impl Into<String>, code: i32) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
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
