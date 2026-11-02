use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("decode: {path}:{line}: {message}")]
    Decode {
        path: PathBuf,
        line: usize,
        message: String,
    },

    #[error("unknown agent `{0}` (registered: {1})")]
    UnknownAgent(String, String),

    #[error("session `{0}` not found under agent `{1}`'s store")]
    SessionNotFound(String, String),

    #[error("session selector `{0}` for agent `{1}` is ambiguous: {2}")]
    AmbiguousSession(String, String, String),

    #[error("agent `{0}` has no readable session store (unsupported store kinds: {1})")]
    NoSessionStore(String, String),

    #[error("import target already exists: {0}")]
    TargetExists(PathBuf),

    #[error("snapshot `{0}` not found")]
    SnapshotNotFound(String),

    #[error("invalid selector `{0}`: {1}")]
    Selector(String, String),

    #[error("invalid document: {0}")]
    Document(String),

    #[error("{0}")]
    Usage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// `path` + `std::io::Error` → [`Error::Io`], for `map_err`.
    pub fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
        let path = path.into();
        move |source| Error::Io { path, source }
    }
}
