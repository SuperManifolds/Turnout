//! Typed error for Tauri command results.
//!
//! Commands previously returned `Result<_, String>`, building error strings ad
//! hoc with `format!` and `map_err(|e| e.to_string())`. [`CommandError`] gives
//! the command boundary a typed error whose variants document the failure kind
//! and whose `From` impls let core/`std` errors propagate with `?`.
//!
//! It serializes as a plain string (its [`Display`]), matching the wire shape
//! the frontend already consumes — `web/src/tauri.rs`'s `invoke` wrapper
//! stringifies the rejection — so no frontend change is needed.

use std::fmt;

use serde::{Serialize, Serializer};

use turnout_core::CoreError;

/// The result type returned by fallible Tauri commands.
pub type CommandResult<T> = std::result::Result<T, CommandError>;

/// A failure surfaced to the frontend from a Tauri command.
#[derive(Debug)]
pub enum CommandError {
    /// A filesystem read/write (or store persistence) failure.
    Io(String),
    /// An outbound network / HTTP request failed.
    Network(String),
    /// Input data (blueprint, overlay, upstream response) could not be parsed.
    Parse(String),
    /// A local tile/render server could not be started.
    Server(String),
    /// A required resource (mods folder, clip, layer) was not found.
    NotFound(String),
    /// The request was rejected as invalid.
    Invalid(String),
    /// Any other failure.
    Other(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(m)
            | Self::Network(m)
            | Self::Parse(m)
            | Self::Server(m)
            | Self::NotFound(m)
            | Self::Invalid(m)
            | Self::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CommandError {}

// Commands return this over the IPC boundary. The frontend `invoke` wrapper
// reads the rejection as a string, so serialize as the message rather than a
// tagged object to keep the wire shape unchanged.
impl Serialize for CommandError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl From<std::io::Error> for CommandError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<CoreError> for CommandError {
    fn from(e: CoreError) -> Self {
        let message = e.to_string();
        match e {
            CoreError::Io(_) => Self::Io(message),
            CoreError::Import(_) => Self::Invalid(message),
            CoreError::UnexpectedEof { .. }
            | CoreError::VarintOverflow { .. }
            | CoreError::InvalidUtf8 { .. }
            | CoreError::BadContainer(_)
            | CoreError::Compression(_)
            | CoreError::TrailingBytes { .. }
            | CoreError::Parse { .. } => Self::Parse(message),
            CoreError::Context { .. } => Self::Other(message),
        }
    }
}
