use std::{fmt, io};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// Bad command line; the message is shown with the usage text.
    Usage(String),
    /// A failed syscall, with what we were trying to do.
    Sys { ctx: String, source: io::Error },
}

impl Error {
    /// Capture `errno` for the syscall that just failed.
    pub fn sys(ctx: impl Into<String>) -> Self {
        Error::Sys { ctx: ctx.into(), source: io::Error::last_os_error() }
    }

    pub fn io(ctx: impl Into<String>, source: io::Error) -> Self {
        Error::Sys { ctx: ctx.into(), source }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(m) => write!(f, "{m}"),
            Error::Sys { ctx, source } => write!(f, "{ctx}: {source}"),
        }
    }
}

impl std::error::Error for Error {}
