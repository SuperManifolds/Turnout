//! Typed error for the core library.
//!
//! Replaces the previous `anyhow` usage so callers (the CLI, the Tauri backend,
//! tests) can match on failure kinds. [`CoreError`] still implements
//! [`std::error::Error`], so a binary using `anyhow` absorbs it through `?`, and
//! `.to_string()` keeps producing the same human-facing messages.

use std::fmt;

/// The result type returned across the core public API.
pub type Result<T> = std::result::Result<T, CoreError>;

/// A failure decoding, encoding, or importing Nimby Rails data.
#[derive(Debug)]
pub enum CoreError {
    /// Ran out of bytes while decoding a wire value.
    UnexpectedEof { reading: &'static str, pos: usize },
    /// A LEB128 varint exceeded 64 bits.
    VarintOverflow { pos: usize },
    /// A length-prefixed string was not valid UTF-8.
    InvalidUtf8 { pos: usize },
    /// The NRC1 container header was missing or malformed.
    BadContainer(String),
    /// zstd compression or decompression failed.
    Compression(&'static str),
    /// Bytes remained after a full payload decode.
    TrailingBytes { remaining: usize, offset: usize, payload_len: usize },
    /// An input file could not be read.
    Io(std::io::Error),
    /// An external overlay format (`GeoJSON`, KML, shapefile) failed to parse.
    Parse { format: &'static str, detail: String },
    /// The import pipeline could not proceed.
    Import(String),
    /// Adds where a nested failure happened (decode position, record index),
    /// preserving the context chain `anyhow` used to carry.
    Context { context: String, source: Box<CoreError> },
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { reading, pos } => {
                write!(f, "unexpected EOF reading {reading} at offset {pos}")
            }
            Self::VarintOverflow { pos } => write!(f, "varint overflow at offset {pos}"),
            Self::InvalidUtf8 { pos } => write!(f, "invalid UTF-8 string at offset {pos}"),
            Self::BadContainer(msg) | Self::Import(msg) => write!(f, "{msg}"),
            Self::Compression(op) => write!(f, "zstd {op} failed"),
            Self::TrailingBytes { remaining, offset, payload_len } => write!(
                f,
                "{remaining} trailing bytes at offset {offset} (payload size {payload_len})"
            ),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Parse { format, detail } => write!(f, "{format} parse error: {detail}"),
            Self::Context { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Context { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Attach context to a core error, mirroring `anyhow`'s `.context()` /
/// `.with_context()`. Applies to any [`Result`] whose error converts into
/// [`CoreError`], so `?`-style call sites can annotate where they failed.
pub trait Context<T> {
    /// Wrap the error with a static or precomputed context message.
    fn context<C: fmt::Display>(self, context: C) -> Result<T>;
    /// Wrap the error with a lazily-computed context message.
    fn with_context<C: fmt::Display>(self, f: impl FnOnce() -> C) -> Result<T>;
}

impl<T, E: Into<CoreError>> Context<T> for std::result::Result<T, E> {
    fn context<C: fmt::Display>(self, context: C) -> Result<T> {
        self.map_err(|e| CoreError::Context {
            context: context.to_string(),
            source: Box::new(e.into()),
        })
    }

    fn with_context<C: fmt::Display>(self, f: impl FnOnce() -> C) -> Result<T> {
        self.map_err(|e| CoreError::Context {
            context: f().to_string(),
            source: Box::new(e.into()),
        })
    }
}
