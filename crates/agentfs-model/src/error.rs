use serde::{Deserialize, Serialize};
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    NameConflict,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
    PermissionDenied,
    ReadOnly,
    Unsupported,
    Busy,
    CapacityExceeded,
    Integrity,
    Io,
    Unavailable,
    RemoteRequired,
    RemoteUnknown,
    Conflict,
    StaleBinding,
    StaleAuthority,
    TargetMoved,
    CandidateMoved,
    DirtyWorktree,
    BranchBusy,
    CoordinatorRequired,
    MountBusy,
    RequestIdConflict,
    TurnResultConflict,
    RecoveryRequired,
    NoCommonBase,
    AmbiguousBase,
    BaseUnavailable,
    UnresolvedConflict,
    ValidationFailed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub fn integrity(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Integrity, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        let code = match value.kind() {
            std::io::ErrorKind::NotFound => ErrorCode::NotFound,
            std::io::ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
            std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            std::io::ErrorKind::StorageFull => ErrorCode::CapacityExceeded,
            _ => ErrorCode::Io,
        };
        Self::new(code, value.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::integrity(value.to_string())
    }
}
